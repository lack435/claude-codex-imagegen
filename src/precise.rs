//! Precise mode: one image call to the OpenAI Images API, billed to the user's API key
//! (docs/design.md, "Precise mode").
//!
//! `tools.rs` validates the call, claims its session and picks the edit target, exactly as for the
//! Codex tools; this module checks the precise options, holds the daily cap, builds the request,
//! sends it, maps the answer, and publishes, previews and records each image the way `turn.rs`
//! does for Codex's. Nothing here involves Codex: no child, no thread, no Codex home.

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::Fnv1a64;
use crate::credential::Secret;
use crate::errors::{self, Failure};
use crate::https::{self, Transport, TransportError};
use crate::output;
use crate::preview::{self, Preview};
use crate::session::{self, ImageOutcome, SessionWriter};

/// Images per call [decided]. Each is its own version, with its own preview.
pub const MAX_IMAGES: u32 = 4;

/// The longest data URL the edits endpoint takes [assumed: P3, the API reference].
const MAX_DATA_URL_CHARS: usize = 20_971_520;

/// Size presets every GPT image model takes [assumed: P2].
const SIZE_PRESETS: &[&str] = &["auto", "1024x1024", "1536x1024", "1024x1536"];
const QUALITIES: &[&str] = &["auto", "low", "medium", "high", "xhigh", "max"];
const BACKGROUNDS: &[&str] = &["auto", "opaque", "transparent"];

/// The precise options a call gives, each `None` when left to the API's default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
    pub size: Option<String>,
    pub quality: Option<String>,
    pub background: Option<String>,
    pub n: u32,
}

/// The precise options from a call's arguments. Shapes only: which model takes which value is the
/// API's to say, and a refusal costs nothing (it comes back as BAD_REQUEST with its reason).
pub fn options(args: &Map<String, Value>) -> Result<Options, Failure> {
    let present = |key: &str| args.get(key).filter(|v| !v.is_null());
    let choice = |key: &str, allowed: &[&str]| -> Result<Option<String>, Failure> {
        match present(key) {
            None => Ok(None),
            Some(Value::String(v)) if allowed.contains(&v.as_str()) => Ok(Some(v.clone())),
            Some(_) => Err(errors::bad_request(format!(
                "'{key}' must be one of {}.",
                allowed.join(", ")
            ))),
        }
    };
    let size = match present("size") {
        None => None,
        Some(Value::String(size)) => {
            check_size(size).map_err(errors::bad_request)?;
            Some(size.clone())
        }
        Some(_) => {
            return Err(errors::bad_request(
                "'size' must be a string such as 1536x1024.",
            ))
        }
    };
    let n = match present("n") {
        None => 1,
        Some(value) => match value.as_u64() {
            Some(n) if (1..=u64::from(MAX_IMAGES)).contains(&n) => n as u32,
            _ => {
                return Err(errors::bad_request(format!(
                    "'n' must be a whole number from 1 to {MAX_IMAGES}."
                )))
            }
        },
    };
    Ok(Options {
        size,
        quality: choice("quality", QUALITIES)?,
        background: choice("background", BACKGROUNDS)?,
        n,
    })
}

/// A preset, or a custom `WIDTHxHEIGHT`: both sides multiples of 16, at most 3840x2160 either way
/// round, an aspect ratio within 3:1, and 655,360 to 8,294,400 pixels [assumed: P2].
pub fn check_size(size: &str) -> Result<(), String> {
    if SIZE_PRESETS.contains(&size) {
        return Ok(());
    }
    let bad = || {
        format!(
            "'size' must be auto, 1024x1024, 1536x1024, 1024x1536, or a custom WIDTHxHEIGHT \
             (both multiples of 16, at most 3840x2160, aspect ratio within 3:1); got '{}'.",
            crate::jsonrpc::clamp(size, 40)
        )
    };
    let (w, h) = size.split_once('x').ok_or_else(bad)?;
    let parse = |s: &str| -> Option<u32> {
        (!s.is_empty() && s.len() <= 5 && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let (w, h) = (parse(w).ok_or_else(bad)?, parse(h).ok_or_else(bad)?);
    let (long, short) = (w.max(h), w.min(h));
    let pixels = u64::from(w) * u64::from(h);
    let ok = w % 16 == 0
        && h % 16 == 0
        && short > 0
        && long <= 3840
        && short <= 2160
        && long <= short * 3
        && (655_360..=8_294_400).contains(&pixels);
    if ok {
        Ok(())
    } else {
        Err(bad())
    }
}

/// A request ready to send.
pub struct Body {
    pub path: &'static str,
    pub json: Vec<u8>,
}

/// The request for a call: a generation, or an edit when there are input images (the edit target
/// first, then the references). The output is always PNG [decided].
pub fn request_body(
    model: &str,
    prompt: &str,
    options: &Options,
    moderation_low: bool,
    inputs: &[Vec<u8>],
) -> Result<Body, Failure> {
    let mut body = json!({
        "model": model,
        "prompt": prompt,
        "n": options.n,
        "output_format": "png",
    });
    let fields = body.as_object_mut().expect("an object");
    for (key, value) in [
        ("size", &options.size),
        ("quality", &options.quality),
        ("background", &options.background),
    ] {
        if let Some(value) = value {
            fields.insert(key.to_string(), json!(value));
        }
    }
    if moderation_low {
        fields.insert("moderation".to_string(), json!("low"));
    }
    let path = if inputs.is_empty() {
        "/v1/images/generations"
    } else {
        let mut images = Vec::with_capacity(inputs.len());
        for bytes in inputs {
            images.push(json!({"image_url": data_url(bytes).map_err(errors::bad_request)?}));
        }
        fields.insert("images".to_string(), Value::Array(images));
        "/v1/images/edits"
    };
    let json = serde_json::to_vec(&body)
        .map_err(|e| errors::internal_error(format!("encoding the request failed: {e}")))?;
    Ok(Body { path, json })
}

/// An image file as a `data:` URL, typed by its signature.
fn data_url(bytes: &[u8]) -> Result<String, String> {
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err("An input image is not a PNG, JPEG or WebP file.".into());
    };
    let url = format!("data:{mime};base64,{}", preview::base64_encode(bytes));
    if url.len() > MAX_DATA_URL_CHARS {
        return Err(format!(
            "An input image is too large for the Images API: {} MB as a data URL, over the {} MB \
             it accepts.",
            url.len() / (1024 * 1024),
            MAX_DATA_URL_CHARS / (1024 * 1024)
        ));
    }
    Ok(url)
}

// ---------------------------------------------------------------------------
// The answer
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ImagesResponse {
    #[serde(default)]
    data: Vec<ImageData>,
    size: Option<String>,
    quality: Option<String>,
    background: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct ImageData {
    b64_json: Option<String>,
    revised_prompt: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<ErrorDetail>,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    code: Option<String>,
    moderation_details: Option<ModerationDetails>,
}

#[derive(Deserialize)]
struct ModerationDetails {
    moderation_stage: Option<String>,
}

/// One image the API returned.
pub struct ApiImage {
    pub png: Vec<u8>,
    pub revised_prompt: Option<String>,
}

/// A successful answer.
pub struct Answer {
    pub images: Vec<ApiImage>,
    /// Images in `data` that could not be decoded.
    pub unreadable: usize,
    /// What the API reports it made: size, quality and background.
    pub made: Vec<String>,
    pub usage: Option<(Option<u64>, Option<u64>)>,
}

/// The failure an HTTP error maps to (docs/design.md, "Precise mode", "Errors"). The API's message
/// is quoted as untrusted detail; the key is never in it, and the request is never echoed.
pub fn map_error(status: u16, body: &[u8]) -> Failure {
    let detail = serde_json::from_slice::<ErrorBody>(body)
        .ok()
        .and_then(|b| b.error);
    let message = detail.as_ref().and_then(|d| d.message.as_deref());
    let code = detail.as_ref().and_then(|d| d.code.as_deref());
    let kind = detail.as_ref().and_then(|d| d.kind.as_deref());
    match status {
        401 => errors::api_key_rejected(status),
        403 => errors::api_access(status, message),
        429 if code == Some("insufficient_quota") || kind == Some("insufficient_quota") => {
            errors::api_quota(status, message)
        }
        429 => errors::api_rate_limited(status, message),
        400 if code == Some("moderation_blocked") => {
            let stage = detail
                .as_ref()
                .and_then(|d| d.moderation_details.as_ref())
                .and_then(|m| m.moderation_stage.as_deref());
            errors::api_content_refused(status, stage, message)
        }
        400 | 404 | 413 | 415 | 422 => errors::api_refused_request(status, message),
        _ => errors::api_upstream(format!(
            "HTTP {status}{}",
            match message {
                Some(m) => format!(
                    "; the API's message (untrusted, quoted): {}",
                    errors::quote_untrusted(m, 500)
                ),
                None => String::new(),
            }
        )),
    }
}

/// A 200's images, decoded.
pub fn parse_answer(body: &[u8]) -> Result<Answer, Failure> {
    let response: ImagesResponse = serde_json::from_slice(body).map_err(|e| {
        errors::api_no_image(format!("the response is not the JSON expected ({e})"))
    })?;
    let mut images = Vec::new();
    let mut unreadable = 0;
    for item in response.data {
        match item
            .b64_json
            .as_deref()
            .and_then(|b| preview::base64_decode(b.as_bytes()))
        {
            Some(png) => images.push(ApiImage {
                png,
                revised_prompt: item.revised_prompt,
            }),
            None => unreadable += 1,
        }
    }
    if images.is_empty() {
        return Err(errors::api_no_image(format!(
            "{unreadable} image entr{} without decodable base64",
            if unreadable == 1 { "y" } else { "ies" }
        )));
    }
    let made = [
        ("size", response.size),
        ("quality", response.quality),
        ("background", response.background),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|v| format!("{k} {v}")))
    .collect();
    Ok(Answer {
        images,
        unreadable,
        made,
        usage: response.usage.map(|u| (u.input_tokens, u.output_tokens)),
    })
}

// ---------------------------------------------------------------------------
// The daily cap
// ---------------------------------------------------------------------------

const USAGE_FILE: &str = "precise-usage.json";
const USAGE_LOCK: &str = "precise-usage.lock";

/// Images requested on one UTC day, across every project of this state base.
#[derive(Default, Serialize, Deserialize)]
struct DayCount {
    /// Days since the Unix epoch, UTC.
    day: i64,
    images: u32,
}

/// Today, as days since the Unix epoch (UTC).
pub fn today() -> i64 {
    session::now_unix().div_euclid(86_400)
}

/// The stored count, strictly: a missing file is no images yet, and anything else that cannot be
/// read or parsed is an error, so the cap never fails open.
fn read_count(state_base: &Path) -> Result<DayCount, String> {
    let path = state_base.join(USAGE_FILE);
    match output::retry_transient(|| std::fs::read(&path)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} cannot be parsed ({e})", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DayCount::default()),
        Err(e) => Err(format!("{} cannot be read ({e})", path.display())),
    }
}

/// The day a count belongs to and the images in it, as of `today`. A stored day later than today
/// (the clock went back) keeps counting into that day rather than starting again.
fn current(count: &DayCount, today: i64) -> (i64, u32) {
    if count.day >= today {
        (count.day, count.images)
    } else {
        (today, 0)
    }
}

/// Images requested so far today, for `status`; 0 when the count cannot be read.
pub fn used_on(state_base: &Path, day: i64) -> u32 {
    read_count(state_base).map_or(0, |count| current(&count, day).1)
}

/// Count `asked` images against `day`'s cap, under a lock across processes, before anything is
/// sent. A request counts whether or not it then succeeds [decided]. With `cap` 0 it is counted
/// and never refused. A count that cannot be read or kept fails closed.
pub fn reserve(state_base: &Path, cap: u32, asked: u32, day: i64) -> Result<u32, Failure> {
    let _lock = session::lock_file(&state_base.join(USAGE_LOCK)).map_err(|e| e.failure())?;
    let stored = read_count(state_base).map_err(|why| {
        errors::internal_error(format!(
            "precise mode's daily image count: {why}, so nothing was sent. Moving the file aside \
             starts the count again"
        ))
    })?;
    let (day, used) = current(&stored, day);
    if cap > 0 && used.saturating_add(asked) > cap {
        return Err(errors::precise_cap_reached(cap, used, asked));
    }
    let count = DayCount {
        day,
        images: used.saturating_add(asked),
    };
    let bytes = serde_json::to_vec(&count).expect("a plain struct encodes");
    session::replace_file(&state_base.join(USAGE_FILE), &bytes).map_err(|e| {
        errors::internal_error(format!(
            "precise mode's daily image count could not be saved in {} ({e}), so nothing was sent",
            state_base.display()
        ))
    })?;
    Ok(count.images)
}

// ---------------------------------------------------------------------------
// One call
// ---------------------------------------------------------------------------

/// Everything a precise call needs once its session is claimed.
pub struct Call<'a> {
    pub transport: &'a dyn Transport,
    pub key: &'a Secret,
    pub model: &'a str,
    pub moderation_low: bool,
    pub max_per_day: u32,
    pub state_base: &'a Path,
    pub session: &'a str,
    pub prompt: &'a str,
    pub options: &'a Options,
    /// The edit target, if any, then the references: files read just before the request.
    pub inputs: &'a [PathBuf],
    pub output_dir: &'a Path,
    pub first_version: u32,
    pub record: &'a SessionWriter,
    pub deadline: Instant,
    pub started: Instant,
    pub ttl_days: u32,
    pub cancelled: &'a dyn Fn() -> bool,
}

/// Send the call and publish what comes back. A failure means no image was published.
pub fn run(call: &Call<'_>) -> Result<Value, Failure> {
    let mut inputs = Vec::with_capacity(call.inputs.len());
    for path in call.inputs {
        inputs.push(std::fs::read(path).map_err(|e| {
            errors::bad_request(format!(
                "The input image {} could not be read ({e}).",
                path.display()
            ))
        })?);
    }
    let body = request_body(
        call.model,
        call.prompt,
        call.options,
        call.moderation_low,
        &inputs,
    )?;
    drop(inputs);
    // Last before sending: every local check has passed, and a cancel still stops it here.
    if (call.cancelled)() {
        return Err(errors::cancelled());
    }
    let remaining = call.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(errors::timeout(
            call.deadline.duration_since(call.started).as_secs(),
        ));
    }
    let used = reserve(call.state_base, call.max_per_day, call.options.n, today())?;
    eprintln!(
        "codex-imagegen: precise {}: sending {} image request to the OpenAI API ({} today)",
        call.session,
        if body.path.ends_with("edits") {
            "an edit"
        } else {
            "a generation"
        },
        used
    );
    let response = call
        .transport
        .send(
            call.key,
            &https::Request {
                method: "POST",
                path: body.path,
                body: Some(&body.json),
                timeout: remaining,
            },
        )
        .map_err(|e| match e {
            TransportError::TimedOut => {
                errors::api_timeout(call.deadline.duration_since(call.started).as_secs())
            }
            TransportError::Failed(why) => {
                errors::api_upstream(format!("no response from {}: {why}", https::API_HOST))
            }
        })?;
    eprintln!(
        "codex-imagegen: precise {}: HTTP {}, {} bytes",
        call.session,
        response.status,
        response.body.len()
    );
    if response.status != 200 {
        return Err(map_error(response.status, &response.body));
    }
    let answer = parse_answer(&response.body)?;
    Ok(publish(call, answer))
}

/// One published image, for the result.
struct Delivered {
    version: Option<u32>,
    path: Option<PathBuf>,
    bytes: usize,
    dims: Option<(u32, u32)>,
    preview: Option<Preview>,
    revised_prompt: Option<String>,
}

/// Publish, preview and record each image, as `turn.rs` does, and build the success result. Every
/// image here completed, so this is a success whatever goes wrong (docs/design.md, "Success
/// result").
fn publish(call: &Call<'_>, answer: Answer) -> Value {
    let mut warnings = Vec::new();
    let mut delivered = Vec::new();
    let mut next = call.first_version;
    for image in answer.images {
        let published = output::publish(call.output_dir, call.session, next, &image.png);
        let (version, path, resolved) = match published {
            Ok(p) => {
                next = p.version + 1;
                (Some(p.version), Some(p.path), p.resolved)
            }
            Err(e) => {
                warnings.push(format!(
                    "could not save an image into {} ({e}); the preview is the only copy",
                    call.output_dir.display()
                ));
                (None, None, None)
            }
        };
        let preview = match preview::build(&image.png) {
            Ok(p) => Some(p),
            Err(why) => {
                warnings.push(format!(
                    "no preview could be built ({why}); open the file to see the image"
                ));
                None
            }
        };
        let dims = preview
            .as_ref()
            .map(|p| (p.source_width, p.source_height))
            .or_else(|| preview::png_dimensions(&image.png));
        if let Some(version) = version {
            if let Err(e) = call.record.image_completed(
                "",
                &ImageOutcome {
                    version: Some(version),
                    output_path: path.as_deref(),
                    saved_path: None,
                    bytes: Some(image.png.len() as u64),
                    fnv1a64: Some(Fnv1a64::of(&image.png)),
                    resolved_path: resolved.as_deref(),
                },
            ) {
                warnings.push(format!(
                    "the session record could not be updated ({e}), so a later precise call may \
                     not find this session"
                ));
            }
        }
        delivered.push(Delivered {
            version,
            path,
            bytes: image.png.len(),
            dims,
            preview,
            revised_prompt: image.revised_prompt,
        });
    }
    if answer.unreadable > 0 {
        warnings.push(format!(
            "{} more image(s) in the API's answer could not be decoded",
            answer.unreadable
        ));
    }

    let mut content: Vec<Value> = delivered
        .iter()
        .filter_map(|d| d.preview.as_ref().map(Preview::image_block))
        .collect();
    let mut lines = Vec::new();
    let versions: Vec<String> = delivered
        .iter()
        .filter_map(|d| d.version.map(|v| v.to_string()))
        .collect();
    lines.push(match versions.len() {
        0 => format!(
            "session: {}   version: none published (see the warnings)",
            call.session
        ),
        1 => format!("session: {}   version: {}", call.session, versions[0]),
        _ => format!(
            "session: {}   versions: {}",
            call.session,
            versions.join(", ")
        ),
    });
    let count = delivered.len();
    for (i, image) in delivered.iter().enumerate() {
        let label = if count > 1 {
            format!("image {} of {count}", i + 1)
        } else {
            "image".to_string()
        };
        let path = image.path.as_ref().map_or_else(
            || "not saved (see the warnings)".to_string(),
            |p| p.display().to_string(),
        );
        let size = crate::turn::format_size(image.bytes);
        lines.push(match image.dims {
            Some((w, h)) => format!("{label}: {path}  ({w}x{h}, {size} PNG)"),
            None => format!("{label}: {path}  ({size} PNG)"),
        });
        if let Some(preview) = &image.preview {
            lines.push(preview.note());
        }
        if let Some(revised) = &image.revised_prompt {
            lines.push(format!(
                "api prompt: {}",
                crate::turn::quote_prompt(revised)
            ));
        }
    }
    lines.push(format!(
        "precise: model {}{}",
        call.model,
        if answer.made.is_empty() {
            String::new()
        } else {
            format!(", {}", answer.made.join(", "))
        }
    ));
    let tokens = match answer.usage {
        Some((input, output)) => format!(
            " ({} input, {} output tokens)",
            input.map_or_else(|| "?".to_string(), |n| n.to_string()),
            output.map_or_else(|| "?".to_string(), |n| n.to_string())
        ),
        None => String::new(),
    };
    lines.push(format!(
        "billed to the user's OpenAI API key, not the ChatGPT plan{tokens}; took {:.1} s",
        call.started.elapsed().as_secs_f64()
    ));
    lines.push(crate::turn::scratch_line(call.ttl_days));
    lines.push(crate::turn::SHOW_USER_LINE.to_string());
    for warning in warnings {
        lines.push(format!("warning: {warning}"));
    }
    content.push(json!({"type": "text", "text": lines.join("\n")}));
    json!({"content": content, "isError": false})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn options_are_checked_for_shape_and_default_to_the_apis_choice() {
        assert_eq!(
            options(&args(json!({}))).unwrap(),
            Options {
                n: 1,
                ..Options::default()
            }
        );
        let full = options(&args(json!({
            "size": "1536x1024", "quality": "xhigh", "background": "transparent", "n": 4
        })))
        .unwrap();
        assert_eq!(full.size.as_deref(), Some("1536x1024"));
        assert_eq!(full.quality.as_deref(), Some("xhigh"));
        assert_eq!(full.background.as_deref(), Some("transparent"));
        assert_eq!(full.n, 4);
        for bad in [
            json!({"quality": "ultra"}),
            json!({"background": "clear"}),
            json!({"n": 0}),
            json!({"n": 5}),
            json!({"n": 1.5}),
            json!({"size": 1024}),
            json!({"size": "1000x1000"}),
        ] {
            assert_eq!(
                options(&args(bad.clone())).unwrap_err().code,
                "BAD_REQUEST",
                "{bad}"
            );
        }
    }

    #[test]
    fn custom_sizes_follow_the_documented_rules() {
        for good in [
            "auto",
            "1024x1024",
            "1536x1024",
            "3840x2160",
            "2160x3840",
            "2048x1152",
        ] {
            assert!(check_size(good).is_ok(), "{good}");
        }
        for bad in [
            "1000x1000", // not multiples of 16
            "4096x2160", // too wide
            "3840x2176", // short side over 2160
            "2400x768",  // wider than 3:1
            "512x512",   // under the pixel floor
            "x1024",
            "1024x",
            "1024*1024",
            "-16x1024",
            "99999x16",
        ] {
            assert!(check_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_generation_and_an_edit_have_the_documented_bodies() {
        let opts = Options {
            size: Some("1536x1024".into()),
            quality: Some("high".into()),
            background: None,
            n: 2,
        };
        let gen = request_body("gpt-image-2", "a fox", &opts, false, &[]).unwrap();
        assert_eq!(gen.path, "/v1/images/generations");
        assert_eq!(
            serde_json::from_slice::<Value>(&gen.json).unwrap(),
            json!({"model": "gpt-image-2", "prompt": "a fox", "n": 2, "output_format": "png",
                   "size": "1536x1024", "quality": "high"})
        );

        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        let jpeg = b"\xff\xd8\xff\xe0rest".to_vec();
        let edit = request_body(
            "m",
            "warmer",
            &Options {
                n: 1,
                ..Options::default()
            },
            true,
            &[png.clone(), jpeg.clone()],
        )
        .unwrap();
        assert_eq!(edit.path, "/v1/images/edits");
        let value: Value = serde_json::from_slice(&edit.json).unwrap();
        assert_eq!(value["moderation"], "low");
        assert_eq!(
            value["images"],
            json!([
                {"image_url": format!("data:image/png;base64,{}", preview::base64_encode(&png))},
                {"image_url": format!("data:image/jpeg;base64,{}", preview::base64_encode(&jpeg))},
            ])
        );
        assert!(value.get("size").is_none());

        let not_image = request_body(
            "m",
            "p",
            &Options {
                n: 1,
                ..Options::default()
            },
            false,
            &[b"GIF89a".to_vec()],
        );
        assert_eq!(not_image.err().unwrap().code, "BAD_REQUEST");
    }

    #[test]
    fn http_errors_map_to_the_designs_codes_with_the_message_quoted() {
        let body = |code: &str, kind: &str| {
            serde_json::to_vec(
                &json!({"error": {"message": "nope\u{202e} here", "type": kind,
                                                  "code": code}}),
            )
            .unwrap()
        };
        assert_eq!(
            map_error(401, &body("invalid_api_key", "x")).code,
            "API_KEY_REJECTED"
        );
        assert_eq!(map_error(403, &body("", "")).code, "API_ACCESS");
        assert_eq!(
            map_error(429, &body("insufficient_quota", "")).code,
            "API_QUOTA"
        );
        assert_eq!(
            map_error(429, &body("", "insufficient_quota")).code,
            "API_QUOTA"
        );
        assert_eq!(
            map_error(429, &body("rate_limit_exceeded", "")).code,
            "RATE_LIMITED"
        );
        assert_eq!(
            map_error(400, &body("invalid_value", "")).code,
            "BAD_REQUEST"
        );
        assert_eq!(map_error(500, b"not json").code, "UPSTREAM_ERROR");
        let refused = map_error(
            400,
            &serde_json::to_vec(
                &json!({"error": {"message": "blocked", "code": "moderation_blocked",
                "type": "image_generation_user_error",
                "moderation_details": {"moderation_stage": "output"}}}),
            )
            .unwrap(),
        );
        assert_eq!(refused.code, "CONTENT_REFUSED");
        assert!(refused.summary.contains("refused the generated image"));
        // The message is quoted as untrusted, with the hiding character removed.
        let detail = map_error(400, &body("x", "")).detail.unwrap();
        assert!(
            detail.contains("(untrusted, quoted): \"nope here\""),
            "{detail}"
        );
    }

    #[test]
    fn an_answer_without_a_readable_image_is_image_failed() {
        let png = preview::testing::photo_like_png(64, 48);
        let good = serde_json::to_vec(&json!({
            "data": [{"b64_json": preview::base64_encode(&png), "revised_prompt": "a fox"},
                     {"b64_json": "!!!"}],
            "size": "1024x1024", "quality": "high", "background": "opaque",
            "usage": {"input_tokens": 12, "output_tokens": 345}
        }))
        .unwrap();
        let answer = parse_answer(&good).unwrap();
        assert_eq!(answer.images.len(), 1);
        assert_eq!(answer.images[0].png, png);
        assert_eq!(answer.unreadable, 1);
        assert_eq!(
            answer.made,
            vec!["size 1024x1024", "quality high", "background opaque"]
        );
        assert_eq!(answer.usage, Some((Some(12), Some(345))));
        assert_eq!(
            parse_answer(br#"{"data": []}"#).err().unwrap().code,
            "IMAGE_FAILED"
        );
        assert_eq!(parse_answer(b"<html>").err().unwrap().code, "IMAGE_FAILED");
    }

    // -----------------------------------------------------------------------
    // The tool, end to end, against a scripted transport and key
    // -----------------------------------------------------------------------

    use crate::codex::testing::FakeCodex;
    use crate::credential::KeySource;
    use crate::https::{Request, Response};
    use crate::mcp::{CallContext, ToolHost};
    use crate::tools::testing::{fixture_with, result_text, Fixture};
    use crate::tools::{PRECISE, REFINE};
    use std::sync::{Arc, Mutex};

    const KEY: &str = "sk-test-precise-key-7777";

    /// A request the fake saw: method, path, body and the key it was handed.
    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        path: String,
        body: Value,
        key: String,
    }

    struct FakeApi {
        seen: Arc<Mutex<Vec<Seen>>>,
        answers: Mutex<Vec<Result<Response, TransportError>>>,
    }

    impl Transport for FakeApi {
        fn send(&self, key: &Secret, request: &Request<'_>) -> Result<Response, TransportError> {
            self.seen.lock().unwrap().push(Seen {
                method: request.method.to_string(),
                path: request.path.to_string(),
                body: request
                    .body
                    .map_or(Value::Null, |b| serde_json::from_slice(b).unwrap()),
                key: key.expose().to_string(),
            });
            let mut answers = self.answers.lock().unwrap();
            assert!(
                !answers.is_empty(),
                "an unscripted request: {}",
                request.path
            );
            answers.remove(0)
        }
    }

    struct FakeKey(Option<&'static str>);

    impl KeySource for FakeKey {
        fn read(&self) -> Result<Option<Secret>, String> {
            Ok(self.0.map(|k| Secret::new(k).unwrap()))
        }
    }

    /// An answer of `count` images, each different from every other answer's.
    fn images_answer(count: usize) -> Result<Response, TransportError> {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let data: Vec<Value> = (0..count)
            .map(|_| {
                let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let png = preview::testing::photo_like_png(96 + 16 * (n % 8), 64 + 16 * (n / 8));
                json!({"b64_json": preview::base64_encode(&png)})
            })
            .collect();
        Ok(Response {
            status: 200,
            body: serde_json::to_vec(&json!({
                "data": data, "size": "1536x1024", "quality": "high", "background": "opaque",
                "usage": {"input_tokens": 20, "output_tokens": 4000}
            }))
            .unwrap(),
        })
    }

    fn error_answer(status: u16, code: &str) -> Result<Response, TransportError> {
        Ok(Response {
            status,
            body: serde_json::to_vec(&json!({"error": {"message": "refused", "code": code}}))
                .unwrap(),
        })
    }

    /// A fixture under `--precise` with the given flags, key and scripted answers.
    fn precise_fixture(
        flags: &[&str],
        key: Option<&'static str>,
        answers: Vec<Result<Response, TransportError>>,
    ) -> (Fixture, Arc<Mutex<Vec<Seen>>>) {
        let mut all = vec!["--precise"];
        all.extend_from_slice(flags);
        let mut f = fixture_with(FakeCodex::default(), &all, |_, _| {});
        let seen = Arc::new(Mutex::new(Vec::new()));
        f.set_precise(
            Box::new(FakeApi {
                seen: Arc::clone(&seen),
                answers: Mutex::new(answers),
            }),
            Box::new(FakeKey(key)),
        );
        (f, seen)
    }

    fn call(f: &Fixture, tool: &str, args: Value) -> Value {
        f.app.call_tool(tool, &args, &CallContext::detached())
    }

    fn code(result: &Value) -> String {
        let text = result_text(result).1;
        text.lines()
            .find_map(|l| l.strip_prefix("code: "))
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn without_the_flag_the_tool_does_not_exist() {
        let f = fixture_with(FakeCodex::default(), &[], |_, _| {});
        let names: Vec<String> = f
            .app
            .tool_definitions()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        assert!(!names.iter().any(|n| n == PRECISE), "{names:?}");
        assert!(!f.app.instructions().contains(PRECISE));
        let result = call(&f, PRECISE, json!({"prompt": "a fox"}));
        assert_eq!(code(&result), "BAD_REQUEST");
        assert!(result_text(&result).1.contains("Unknown tool"));
    }

    #[test]
    fn a_generation_publishes_each_image_records_a_precise_session_and_never_shows_the_key() {
        let (f, seen) = precise_fixture(&[], Some(KEY), vec![images_answer(2)]);
        let tools = f.app.tool_definitions();
        let tool = tools.iter().find(|t| t["name"] == PRECISE).unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(description.starts_with("Billed per image to the user's OpenAI API key"));
        assert!(description.chars().count() < 2048);

        let result = call(
            &f,
            PRECISE,
            json!({"prompt": "a fox", "session": "Fox", "size": "1536x1024",
                   "quality": "high", "n": 2}),
        );
        assert_eq!(result["isError"], false, "{result}");
        let text = result_text(&result).1;
        assert!(
            text.starts_with("session: Fox   versions: 1, 2\n"),
            "{text}"
        );
        assert!(text.contains("precise: model gpt-image-2.5-sunburst-2026-09-08, size 1536x1024"));
        assert!(text.contains(
            "billed to the user's OpenAI API key, not the ChatGPT plan (20 input, 4000 output \
             tokens)"
        ));
        assert!(!text.contains(KEY) && !result.to_string().contains(KEY));
        assert_eq!(
            result["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "image")
                .count(),
            2
        );

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].path, "/v1/images/generations");
        assert_eq!(seen[0].key, KEY);
        assert_eq!(
            seen[0].body,
            json!({"model": "gpt-image-2.5-sunburst-2026-09-08", "prompt": "a fox", "n": 2,
                   "output_format": "png", "size": "1536x1024", "quality": "high"})
        );
        // No Codex was involved.
        assert_eq!(f.spawns.load(std::sync::atomic::Ordering::SeqCst), 0);

        let record = f.record("fox").unwrap();
        assert_eq!(record.engine, session::Engine::Api);
        assert_eq!(record.thread_id, "");
        assert_eq!(record.model, "gpt-image-2.5-sunburst-2026-09-08");
        assert_eq!(record.turns, 1);
        assert_eq!(record.next_version, 3);
        let out = f.dir.join("generated-images");
        assert_eq!(
            record
                .outputs
                .iter()
                .map(|o| o.path.clone())
                .collect::<Vec<_>>(),
            vec![out.join("Fox-v1.png"), out.join("Fox-v2.png")]
        );
        assert!(out.join("Fox-v2.png").is_file());

        let status = result_text(&call(&f, crate::tools::STATUS, json!({}))).1;
        assert!(
            status.contains("  Fox (precise): 1 turn, latest "),
            "{status}"
        );
        assert!(
            status.contains(
                "precise mode: on, billed to the OpenAI API key; model \
                             gpt-image-2.5-sunburst-2026-09-08; API key stored (…7777); 2 of 20 \
                             images today (UTC)"
            ),
            "{status}"
        );
        assert!(!status.contains(KEY));
    }

    #[test]
    fn a_precise_session_is_edited_from_its_latest_image_or_from_version() {
        let (f, seen) = precise_fixture(
            &[],
            Some(KEY),
            vec![images_answer(1), images_answer(1), images_answer(1)],
        );
        assert_eq!(
            call(&f, PRECISE, json!({"prompt": "a fox", "session": "fox"}))["isError"],
            false
        );
        let out = f.dir.join("generated-images");
        let v1 = std::fs::read(out.join("fox-v1.png")).unwrap();

        let edit = call(
            &f,
            PRECISE,
            json!({"prompt": "warmer", "session": "fox",
                                            "background": "transparent"}),
        );
        assert_eq!(edit["isError"], false, "{edit}");
        assert!(result_text(&edit)
            .1
            .starts_with("session: fox   version: 2\n"));
        let rewind = call(
            &f,
            PRECISE,
            json!({"prompt": "cooler", "session": "fox",
                                              "from_version": 1}),
        );
        assert!(result_text(&rewind)
            .1
            .starts_with("session: fox   version: 3\n"));

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[1].path, "/v1/images/edits");
        assert_eq!(seen[1].body["background"], "transparent");
        let v2 = std::fs::read(out.join("fox-v2.png")).unwrap();
        assert_eq!(
            seen[1].body["images"],
            json!([{"image_url": format!("data:image/png;base64,{}",
                                        preview::base64_encode(&v1))}])
        );
        // from_version 1 edits v1, not the latest (v2).
        assert_eq!(
            seen[2].body["images"][0]["image_url"],
            format!("data:image/png;base64,{}", preview::base64_encode(&v1))
        );
        assert_ne!(v1, v2);
        assert_eq!(f.record("fox").unwrap().turns, 3);
    }

    #[test]
    fn nothing_is_sent_without_a_key_or_past_the_cap_or_for_a_bad_argument() {
        let (f, seen) = precise_fixture(&[], None, vec![]);
        let missing = call(&f, PRECISE, json!({"prompt": "a fox"}));
        assert_eq!(code(&missing), "API_KEY_MISSING");
        assert!(result_text(&missing).1.contains("--set-api-key"));
        assert!(seen.lock().unwrap().is_empty());

        let (f, seen) = precise_fixture(
            &["--precise-max-images-per-day", "3"],
            Some(KEY),
            vec![images_answer(2)],
        );
        assert_eq!(
            call(&f, PRECISE, json!({"prompt": "a", "n": 2}))["isError"],
            false
        );
        let capped = call(&f, PRECISE, json!({"prompt": "b", "n": 2}));
        assert_eq!(code(&capped), "PRECISE_CAP_REACHED");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "the capped call sent nothing"
        );

        let (f, seen) = precise_fixture(&[], Some(KEY), vec![]);
        for bad in [
            json!({"prompt": "a", "size": "1000x1000"}),
            json!({"prompt": "a", "quality": "ultra"}),
            json!({"prompt": "a", "n": 9}),
            json!({"prompt": "a", "from_version": 1}),
            json!({"prompt": "a", "moderation": "low"}),
        ] {
            assert_eq!(
                code(&call(&f, PRECISE, bad.clone())),
                "BAD_REQUEST",
                "{bad}"
            );
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn an_api_refusal_leaves_no_record_and_its_code_is_the_designs() {
        let (f, _) = precise_fixture(
            &[],
            Some(KEY),
            vec![
                error_answer(401, "invalid_api_key"),
                error_answer(429, "insufficient_quota"),
                Err(TransportError::Failed("no route".into())),
            ],
        );
        let rejected = call(&f, PRECISE, json!({"prompt": "a", "session": "s1"}));
        assert_eq!(code(&rejected), "API_KEY_REJECTED");
        assert!(!result_text(&rejected).1.contains(KEY));
        assert!(f.record("s1").is_none(), "a failed call records nothing");
        assert_eq!(
            code(&call(&f, PRECISE, json!({"prompt": "a"}))),
            "API_QUOTA"
        );
        assert_eq!(
            code(&call(&f, PRECISE, json!({"prompt": "a"}))),
            "UPSTREAM_ERROR"
        );
    }

    #[test]
    fn precise_and_codex_sessions_each_belong_to_their_own_tool() {
        let (f, _) = precise_fixture(&[], Some(KEY), vec![images_answer(1)]);
        assert_eq!(
            call(&f, PRECISE, json!({"prompt": "a", "session": "api1"}))["isError"],
            false
        );
        let refine = call(&f, REFINE, json!({"session": "api1", "feedback": "x"}));
        assert_eq!(code(&refine), "BAD_REQUEST");
        assert!(result_text(&refine)
            .1
            .contains("was made with codex_imagegen_precise"));

        // A Codex session's name is refused by precise mode.
        f.store()
            .update(|file| {
                let mut record = file.sessions.get("api1").unwrap().clone();
                record.name = "codex1".into();
                record.engine = session::Engine::Codex;
                record.thread_id = "019a0000-0000-7000-8000-00000000cafe".into();
                file.sessions.insert("codex1".into(), record);
                Ok(())
            })
            .unwrap();
        let precise = call(&f, PRECISE, json!({"prompt": "a", "session": "codex1"}));
        assert_eq!(code(&precise), "BAD_REQUEST");
        assert!(result_text(&precise).1.contains("was made with Codex"));
    }

    #[test]
    fn the_daily_cap_counts_across_calls_and_resets_each_day() {
        let dir = crate::testutil::temp_dir("precise-cap");
        assert_eq!(reserve(&dir, 5, 2, 100).unwrap(), 2);
        assert_eq!(reserve(&dir, 5, 3, 100).unwrap(), 5);
        let refused = reserve(&dir, 5, 1, 100).unwrap_err();
        assert_eq!(refused.code, "PRECISE_CAP_REACHED");
        assert!(refused.summary.contains("5 were already requested"));
        assert_eq!(used_on(&dir, 100), 5);
        // A new UTC day starts again.
        assert_eq!(used_on(&dir, 101), 0);
        assert_eq!(reserve(&dir, 5, 4, 101).unwrap(), 4);
        // 0 is no cap: counted, never refused.
        assert_eq!(reserve(&dir, 0, 4, 101).unwrap(), 8);
        // A clock that goes back keeps counting into the later day.
        assert_eq!(reserve(&dir, 10, 1, 100).unwrap(), 9);
        // A count that cannot be parsed fails closed, and is left as it is.
        std::fs::write(dir.join(USAGE_FILE), b"{trunc").unwrap();
        let broken = reserve(&dir, 5, 1, 101).unwrap_err();
        assert_eq!(broken.code, "INTERNAL_ERROR");
        assert!(
            broken.summary.contains("nothing was sent"),
            "{}",
            broken.summary
        );
        assert_eq!(std::fs::read(dir.join(USAGE_FILE)).unwrap(), b"{trunc");
        assert_eq!(used_on(&dir, 101), 0);
    }
}
