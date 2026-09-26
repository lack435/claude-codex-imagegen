//! The preview Claude looks at: the full-resolution PNG decoded, downscaled and re-encoded small
//! enough to travel as an MCP image block (docs/design.md, "Output files").
//!
//! 1. Decode with `png`, normalised to 8 bits per sample.
//! 2. Downscale with an exact area average to a long edge of at most 1024 px. Never upscale.
//! 3. Encode as JPEG, quality 85, 4:2:0, standard Huffman tables, with `jpeg-encoder`.
//! 4. Base64.
//!
//! An image with transparency gets a PNG preview instead, or, if that PNG is too big, a JPEG with
//! the transparency flattened onto white; the note says which.
//!
//! The file on disk is never touched here: it is the byte-for-byte original, C2PA chunk included.
//! Everything in this module is pure computation on bytes, so it runs on the call's own thread and
//! is tested without any Codex.

use std::io::Cursor;

use serde_json::{json, Value};

/// The preview's longest edge. A 1024 px preview costs at most 1,369 tokens and is not resized on
/// any model tier (docs/design.md, "Image cost").
pub const MAX_EDGE: u32 = 1024;

/// The JPEG quality every preview starts at [decided]. At 1024 px it measured 89-317 KB on real
/// Codex images, about 37 dB PSNR [verified: benchmark].
pub const JPEG_QUALITY: u8 = 85;

/// Claude Code re-encodes an image block larger than this many raw bytes as lossy JPEG
/// (docs/design.md, "Image blocks"). Our preview stays at or below it, so what Claude sees is what
/// this module made.
pub const MAX_PREVIEW_BYTES: usize = 512_000;

/// A PNG preview (an image with transparency) larger than this is flattened onto white and sent as
/// JPEG instead [decided: "500 KB" in the design].
pub const MAX_ALPHA_PNG_BYTES: usize = 500_000;

/// Qualities tried, in order, until the JPEG fits [`MAX_PREVIEW_BYTES`]. Real images fit at the
/// first; the rest exist for pathological content (noise-like detail), where a softer preview beats
/// none.
const JPEG_QUALITY_LADDER: [u8; 5] = [JPEG_QUALITY, 75, 65, 50, 35];

/// The largest decoded image accepted. Codex images are about 1.6 MP (under 7 MB decoded, RGBA);
/// the cap only stops a malformed header from asking for gigabytes.
const MAX_DECODED_BYTES: usize = 128 * 1024 * 1024;

/// A preview ready to send.
#[derive(Clone, Debug)]
pub struct Preview {
    /// `image/jpeg`, or `image/png` for an image with transparency.
    pub mime_type: &'static str,
    /// The encoded preview, base64.
    pub data: String,
    /// The encoded preview's size before base64.
    pub encoded_bytes: usize,
    pub width: u32,
    pub height: u32,
    /// The original image's dimensions.
    pub source_width: u32,
    pub source_height: u32,
    /// The JPEG quality used, `None` for a PNG preview.
    pub quality: Option<u8>,
    /// The image had transparency, and the preview flattened it onto white.
    pub flattened: bool,
}

impl Preview {
    /// The MCP content block.
    pub fn image_block(&self) -> Value {
        json!({"type": "image", "mimeType": self.mime_type, "data": self.data})
    }

    /// The line the result text carries about the preview, in the design's words.
    pub fn note(&self) -> String {
        let edge = self.width.max(self.height);
        if self.mime_type == "image/png" {
            format!(
                "the preview is a {edge}px PNG, because the image has transparency; the file is \
                 the full-resolution original"
            )
        } else if self.flattened {
            format!(
                "the preview is a {edge}px JPEG with the transparency flattened onto white; the \
                 file is the full-resolution original, transparency included"
            )
        } else {
            format!("the preview is a {edge}px JPEG; the file is the full-resolution original")
        }
    }
}

/// Build the preview of a PNG. `Err` says why not, for the result's warning line; the caller then
/// sends no image block (docs/design.md, "Success result").
pub fn build(png: &[u8]) -> Result<Preview, String> {
    let image = decode(png)?;
    let (source_width, source_height) = (image.width, image.height);
    let (width, height) = fit_long_edge(source_width, source_height, MAX_EDGE);
    let done = |mime_type, encoded: Vec<u8>, quality, flattened| Preview {
        mime_type,
        data: base64_encode(&encoded),
        encoded_bytes: encoded.len(),
        width,
        height,
        source_width,
        source_height,
        quality,
        flattened,
    };

    if image.has_transparency() {
        let small = downscale(&image, width, height);
        let encoded = encode_png(&small)?;
        if encoded.len() <= MAX_ALPHA_PNG_BYTES {
            return Ok(done("image/png", encoded, None, false));
        }
        // Flattening the downscaled image equals downscaling the flattened one, up to rounding:
        // the downscale averages premultiplied colour, and compositing over white is linear in it.
        let (encoded, quality) = encode_jpeg_to_fit(&flatten_onto_white(&small))?;
        return Ok(done("image/jpeg", encoded, Some(quality), true));
    }
    let small = downscale(&image.without_alpha(), width, height);
    let (encoded, quality) = encode_jpeg_to_fit(&small)?;
    Ok(done("image/jpeg", encoded, Some(quality), false))
}

/// A PNG's width and height from its header, without decoding it. For the result's image line
/// when the preview itself could not be built.
pub fn png_dimensions(png: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if png.len() < 24 || !png.starts_with(SIGNATURE) || &png[12..16] != b"IHDR" {
        return None;
    }
    let be = |at: usize| u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]);
    Some((be(16), be(20)))
}

// ---------------------------------------------------------------------------
// Pixels
// ---------------------------------------------------------------------------

/// Interleaved 8-bit samples: 1 (grey), 2 (grey, alpha), 3 (RGB) or 4 (RGBA) channels.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pixels {
    width: u32,
    height: u32,
    channels: usize,
    data: Vec<u8>,
}

impl Pixels {
    fn has_alpha_channel(&self) -> bool {
        self.channels == 2 || self.channels == 4
    }

    /// Whether any pixel is less than fully opaque. An alpha channel alone is not transparency: an
    /// opaque RGBA image gets the ordinary JPEG preview, not a PNG several times larger.
    fn has_transparency(&self) -> bool {
        self.has_alpha_channel()
            && self
                .data
                .chunks_exact(self.channels)
                .any(|px| px[self.channels - 1] < 255)
    }

    /// The same image without its alpha channel. Only for images with no transparency.
    fn without_alpha(self) -> Self {
        if !self.has_alpha_channel() {
            return self;
        }
        let channels = self.channels - 1;
        let data = self
            .data
            .chunks_exact(self.channels)
            .flat_map(|px| px[..channels].iter().copied())
            .collect();
        Self {
            channels,
            data,
            ..self
        }
    }
}

fn decode(png: &[u8]) -> Result<Pixels, String> {
    let mut decoder = png::Decoder::new(Cursor::new(png));
    // Palette and low-bit greys expanded, tRNS turned into an alpha channel, 16-bit cut to 8.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("the image is not a readable PNG ({e})"))?;
    let size = match reader.output_buffer_size() {
        Some(size) if size <= MAX_DECODED_BYTES => size,
        _ => {
            let info = reader.info();
            return Err(format!(
                "the image is too large to preview ({}x{})",
                info.width, info.height
            ));
        }
    };
    let mut data = vec![0; size];
    let frame = reader
        .next_frame(&mut data)
        .map_err(|e| format!("the PNG could not be decoded ({e})"))?;
    data.truncate(frame.buffer_size());
    let channels = frame.color_type.samples();
    if frame.bit_depth != png::BitDepth::Eight || !(1..=4).contains(&channels) {
        return Err(format!(
            "the PNG decoded to an unexpected format ({:?}, {:?})",
            frame.color_type, frame.bit_depth
        ));
    }
    if frame.width == 0 || frame.height == 0 {
        return Err("the PNG has no pixels".to_string());
    }
    Ok(Pixels {
        width: frame.width,
        height: frame.height,
        channels,
        data,
    })
}

/// The size that fits `max` on the long edge, keeping the aspect ratio. Never larger than the
/// image, and never zero.
fn fit_long_edge(width: u32, height: u32, max: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= max {
        return (width, height);
    }
    let scale = f64::from(max) / f64::from(long);
    let fit = |edge: u32| ((f64::from(edge) * scale).round() as u32).clamp(1, max);
    (fit(width), fit(height))
}

/// For each destination pixel along one axis: the first source pixel it covers, and the weight of
/// each source pixel from there, as the exact fraction of the destination pixel it covers.
fn coverage(src: usize, dst: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = src as f64 / dst as f64;
    (0..dst)
        .map(|i| {
            let from = i as f64 * scale;
            let to = from + scale;
            let first = from.floor() as usize;
            let end = (to.ceil() as usize).min(src);
            let weights = (first..end)
                .map(|s| ((((s + 1) as f64).min(to) - (s as f64).max(from)) / scale) as f32)
                .collect();
            (first, weights)
        })
        .collect()
}

/// Downscale by exact area averaging: each destination pixel is the mean of the source area it
/// covers, partial pixels weighted by how much of them it covers. Separable, horizontal pass then
/// vertical. Averages in gamma-encoded sRGB, as every common resizer does.
///
/// With an alpha channel, colour is averaged premultiplied by alpha, so the (arbitrary, often
/// black) colour of fully transparent pixels cannot bleed into the edges of opaque ones.
fn downscale(image: &Pixels, width: u32, height: u32) -> Pixels {
    let (sw, sh, ch) = (image.width as usize, image.height as usize, image.channels);
    let (dw, dh) = (width as usize, height as usize);
    if (sw, sh) == (dw, dh) {
        return image.clone();
    }
    let alpha = image.has_alpha_channel();
    let colours = if alpha { ch - 1 } else { ch };
    let across = coverage(sw, dw);
    let down = coverage(sh, dh);
    let stride = dw * ch;

    let mut rows = vec![0f32; sh * stride];
    for y in 0..sh {
        let src = &image.data[y * sw * ch..(y + 1) * sw * ch];
        let out = &mut rows[y * stride..(y + 1) * stride];
        for (x, (first, weights)) in across.iter().enumerate() {
            let mut acc = [0f32; 4];
            for (k, &w) in weights.iter().enumerate() {
                let px = &src[(first + k) * ch..(first + k + 1) * ch];
                let coverage = if alpha {
                    w * f32::from(px[colours]) / 255.0
                } else {
                    w
                };
                for c in 0..colours {
                    acc[c] += f32::from(px[c]) * coverage;
                }
                if alpha {
                    acc[colours] += f32::from(px[colours]) * w;
                }
            }
            out[x * ch..(x + 1) * ch].copy_from_slice(&acc[..ch]);
        }
    }

    let mut data = vec![0u8; dh * stride];
    let mut acc = vec![0f32; stride];
    for (y, (first, weights)) in down.iter().enumerate() {
        acc.fill(0.0);
        for (k, &w) in weights.iter().enumerate() {
            let row = &rows[(first + k) * stride..(first + k + 1) * stride];
            for (a, &r) in acc.iter_mut().zip(row) {
                *a += r * w;
            }
        }
        let out = &mut data[y * stride..(y + 1) * stride];
        for (px, sums) in out.chunks_exact_mut(ch).zip(acc.chunks_exact(ch)) {
            if alpha {
                let a = sums[colours];
                for c in 0..colours {
                    // Un-premultiply. A fully transparent result has no colour to recover.
                    px[c] = if a > 0.0 {
                        to_u8(sums[c] * 255.0 / a)
                    } else {
                        0
                    };
                }
                px[colours] = to_u8(a);
            } else {
                for c in 0..ch {
                    px[c] = to_u8(sums[c]);
                }
            }
        }
    }
    Pixels {
        width,
        height,
        channels: ch,
        data,
    }
}

fn to_u8(value: f32) -> u8 {
    (value + 0.5).clamp(0.0, 255.0) as u8
}

/// Composite onto white and drop the alpha channel: JPEG has none.
fn flatten_onto_white(image: &Pixels) -> Pixels {
    if !image.has_alpha_channel() {
        return image.clone();
    }
    let ch = image.channels;
    let colours = ch - 1;
    let mut data = Vec::with_capacity(image.data.len() / ch * colours);
    for px in image.data.chunks_exact(ch) {
        let a = u32::from(px[colours]);
        for &c in &px[..colours] {
            data.push(((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8);
        }
    }
    Pixels {
        width: image.width,
        height: image.height,
        channels: colours,
        data,
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// The JPEG at the first quality of the ladder that fits [`MAX_PREVIEW_BYTES`], and that quality.
fn encode_jpeg_to_fit(image: &Pixels) -> Result<(Vec<u8>, u8), String> {
    encode_jpeg_to_fit_within(image, MAX_PREVIEW_BYTES)
}

fn encode_jpeg_to_fit_within(image: &Pixels, limit: usize) -> Result<(Vec<u8>, u8), String> {
    let mut smallest = 0;
    for quality in JPEG_QUALITY_LADDER {
        let encoded = encode_jpeg(image, quality)?;
        if encoded.len() <= limit {
            return Ok((encoded, quality));
        }
        smallest = encoded.len();
    }
    Err(format!(
        "even at JPEG quality {} the preview is {smallest} bytes, over the {limit}-byte limit",
        JPEG_QUALITY_LADDER[JPEG_QUALITY_LADDER.len() - 1]
    ))
}

/// Baseline JPEG, 4:2:0 for colour, standard Huffman tables.
///
/// The Huffman trap (docs/design.md): `set_optimized_huffman_tables(true)` makes jpeg-encoder write
/// one scan per component instead of one interleaved scan, and with 4:2:0 those files decoded as
/// garbage in zune-jpeg and were garbled through Claude Code's Read path [verified]. It is set to
/// false here explicitly, as is the subsampling, so neither rests on a library default; a test
/// checks for a single scan covering every component.
fn encode_jpeg(image: &Pixels, quality: u8) -> Result<Vec<u8>, String> {
    let colour = match image.channels {
        1 => jpeg_encoder::ColorType::Luma,
        3 => jpeg_encoder::ColorType::Rgb,
        n => return Err(format!("cannot encode {n} channels as JPEG")),
    };
    let (Ok(width), Ok(height)) = (u16::try_from(image.width), u16::try_from(image.height)) else {
        return Err(format!(
            "{}x{} is too large for a JPEG",
            image.width, image.height
        ));
    };
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, quality);
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
    encoder.set_optimized_huffman_tables(false);
    encoder
        .encode(&image.data, width, height, colour)
        .map_err(|e| format!("the JPEG encoder failed ({e})"))?;
    Ok(out)
}

fn encode_png(image: &Pixels) -> Result<Vec<u8>, String> {
    let colour = match image.channels {
        1 => png::ColorType::Grayscale,
        2 => png::ColorType::GrayscaleAlpha,
        3 => png::ColorType::Rgb,
        _ => png::ColorType::Rgba,
    };
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
    encoder.set_color(colour);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Balanced);
    let failed = |e: png::EncodingError| format!("the PNG encoder failed ({e})");
    let mut writer = encoder.write_header().map_err(failed)?;
    writer.write_image_data(&image.data).map_err(failed)?;
    writer.finish().map_err(failed)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// Base64 (RFC 4648, standard alphabet, padded)
// ---------------------------------------------------------------------------

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard padded base64. Hand-written rather than a crate: it is this short, and a test checks it
/// against the `base64` crate for every length up to 2 KB.
pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let symbol = |n: u32, shift: u32| char::from(BASE64[(n >> shift) as usize & 63]);
    let mut chunks = input.chunks_exact(3);
    for c in &mut chunks {
        let n = u32::from(c[0]) << 16 | u32::from(c[1]) << 8 | u32::from(c[2]);
        for shift in [18, 12, 6, 0] {
            out.push(symbol(n, shift));
        }
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let n = u32::from(rest[0]) << 16 | u32::from(rest.get(1).copied().unwrap_or(0)) << 8;
        out.push(symbol(n, 18));
        out.push(symbol(n, 12));
        out.push(if rest.len() == 2 { symbol(n, 6) } else { '=' });
        out.push('=');
    }
    out
}

/// Decode standard padded base64, as the image tool's `result` carries it. Strict: `None` for a
/// length that is not a multiple of four, a character outside the alphabet, or padding anywhere but
/// the end.
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        Some(u32::from(v))
    }
    if !input.len().is_multiple_of(4) {
        return None;
    }
    let quads = input.len() / 4;
    let mut out = Vec::with_capacity(quads * 3);
    for (i, quad) in input.chunks_exact(4).enumerate() {
        let pad = if i + 1 == quads {
            quad.iter().rev().take_while(|&&c| c == b'=').count()
        } else {
            0
        };
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = n << 6 | value(c)?;
        }
        n <<= 6 * pad as u32;
        out.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8][..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
pub(crate) mod testing {
    //! Synthetic images for tests, here and in the generate tests.

    /// A PNG of `width` x `height` with `channels` channels, each sample from `pixel(x, y, c)`.
    /// Stored uncompressed-fast so building large test images stays quick in debug builds.
    pub fn png_from(
        width: u32,
        height: u32,
        channels: usize,
        pixel: impl Fn(u32, u32, usize) -> u8,
    ) -> Vec<u8> {
        let mut data = Vec::with_capacity(width as usize * height as usize * channels);
        for y in 0..height {
            for x in 0..width {
                for c in 0..channels {
                    data.push(pixel(x, y, c));
                }
            }
        }
        let colour = match channels {
            1 => png::ColorType::Grayscale,
            2 => png::ColorType::GrayscaleAlpha,
            3 => png::ColorType::Rgb,
            _ => png::ColorType::Rgba,
        };
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(colour);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fastest);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&data).unwrap();
        writer.finish().unwrap();
        out
    }

    /// Something like a photograph: smooth gradients with fine texture on top, which is what
    /// JPEG size depends on. Deterministic.
    pub fn photo_like_png(width: u32, height: u32) -> Vec<u8> {
        png_from(width, height, 3, |x, y, c| {
            let gradient = (x * 255 / width.max(1) + y * 255 / height.max(1)) / 2;
            let texture = noise(x, y, c as u32) % 48;
            let wave = ((x as f32 / 23.0 + c as f32).sin() * 30.0 + 30.0) as u32;
            ((gradient + texture + wave) % 256) as u8
        })
    }

    /// A deterministic hash of a position, for texture and noise.
    pub fn noise(x: u32, y: u32, c: u32) -> u32 {
        let mut h =
            x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B) ^ c.wrapping_mul(0xC2B2_AE35);
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        h
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{noise, photo_like_png, png_from};
    use super::*;
    use base64::Engine as _;

    /// The JPEG's segments, as (marker, payload) pairs, up to the first scan's header. Enough to
    /// read the frame and scan headers; entropy-coded data is skipped by searching for the next
    /// marker.
    fn jpeg_markers(jpeg: &[u8]) -> Vec<(u8, Vec<u8>)> {
        assert_eq!(&jpeg[..2], b"\xff\xd8", "no SOI");
        let mut markers = Vec::new();
        let mut i = 2;
        while i + 4 <= jpeg.len() {
            assert_eq!(jpeg[i], 0xff, "expected a marker at {i}");
            let marker = jpeg[i + 1];
            if marker == 0xd9 {
                markers.push((marker, Vec::new()));
                break;
            }
            let len = usize::from(u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]));
            markers.push((marker, jpeg[i + 4..i + 2 + len].to_vec()));
            i += 2 + len;
            if marker == 0xda {
                // Skip entropy-coded data: a 0xff is data when followed by 0x00 or a restart
                // marker, and a real marker otherwise.
                while i + 1 < jpeg.len() {
                    if jpeg[i] == 0xff && jpeg[i + 1] != 0 && !(0xd0..=0xd7).contains(&jpeg[i + 1])
                    {
                        break;
                    }
                    i += 1;
                }
            }
        }
        markers
    }

    /// (width, height, [(component id, h, v)]) from the SOF0 header.
    fn frame_header(jpeg: &[u8]) -> (u32, u32, Vec<(u8, u8, u8)>) {
        let markers = jpeg_markers(jpeg);
        let (_, sof) = markers
            .iter()
            .find(|(m, _)| *m == 0xc0)
            .expect("a baseline SOF0 frame");
        let height = u32::from(u16::from_be_bytes([sof[1], sof[2]]));
        let width = u32::from(u16::from_be_bytes([sof[3], sof[4]]));
        let components = (0..usize::from(sof[5]))
            .map(|i| {
                let c = &sof[6 + i * 3..9 + i * 3];
                (c[0], c[1] >> 4, c[1] & 0x0f)
            })
            .collect();
        (width, height, components)
    }

    fn decoded(preview: &Preview) -> Vec<u8> {
        base64_decode(preview.data.as_bytes()).expect("the preview's base64 decodes")
    }

    #[test]
    fn a_real_sized_image_becomes_a_1024px_jpeg_under_the_limit() {
        // Codex's commonest non-square size [verified].
        let png = photo_like_png(1312, 1199);
        let preview = build(&png).unwrap();
        assert_eq!(preview.mime_type, "image/jpeg");
        assert_eq!((preview.width, preview.height), (1024, 936));
        assert_eq!((preview.source_width, preview.source_height), (1312, 1199));
        assert_eq!(preview.quality, Some(85));
        assert!(!preview.flattened);
        let jpeg = decoded(&preview);
        assert_eq!(jpeg.len(), preview.encoded_bytes);
        assert!(jpeg.len() <= MAX_PREVIEW_BYTES, "{} bytes", jpeg.len());
        let (w, h, _) = frame_header(&jpeg);
        assert_eq!((w, h), (1024, 936));
        assert_eq!(
            preview.note(),
            "the preview is a 1024px JPEG; the file is the full-resolution original"
        );
        let block = preview.image_block();
        assert_eq!(block["type"], "image");
        assert_eq!(block["mimeType"], "image/jpeg");
        assert_eq!(block["data"], preview.data.as_str());
    }

    #[test]
    fn the_jpeg_has_one_interleaved_scan_covering_every_component() {
        // The Huffman trap: one SOS per component decoded as garbage (docs/design.md).
        for (channels, expected_components) in [(3, 3), (1, 1)] {
            let png = png_from(300, 200, channels, |x, y, c| {
                (noise(x, y, c as u32) % 200) as u8
            });
            let preview = build(&png).unwrap();
            let jpeg = decoded(&preview);
            let markers = jpeg_markers(&jpeg);
            let scans: Vec<&Vec<u8>> = markers
                .iter()
                .filter(|(m, _)| *m == 0xda)
                .map(|(_, p)| p)
                .collect();
            assert_eq!(scans.len(), 1, "expected exactly one SOS");
            assert_eq!(usize::from(scans[0][0]), expected_components);
            let (_, _, components) = frame_header(&jpeg);
            assert_eq!(components.len(), expected_components);
            let scanned: Vec<u8> = (0..expected_components)
                .map(|i| scans[0][1 + i * 2])
                .collect();
            let framed: Vec<u8> = components.iter().map(|c| c.0).collect();
            assert_eq!(scanned, framed, "the scan must cover every frame component");
            // No other SOF type: baseline, not progressive.
            assert!(!markers.iter().any(|(m, _)| *m == 0xc2));
        }
    }

    #[test]
    fn colour_previews_are_4_2_0() {
        let preview = build(&photo_like_png(64, 64)).unwrap();
        let (_, _, components) = frame_header(&decoded(&preview));
        let sampling: Vec<(u8, u8)> = components.iter().map(|c| (c.1, c.2)).collect();
        assert_eq!(sampling, vec![(2, 2), (1, 1), (1, 1)]);
    }

    #[test]
    fn dimensions_fit_the_long_edge_and_never_upscale() {
        assert_eq!(fit_long_edge(1312, 1199, 1024), (1024, 936));
        assert_eq!(fit_long_edge(1254, 1254, 1024), (1024, 1024));
        assert_eq!(fit_long_edge(1199, 1312, 1024), (936, 1024));
        assert_eq!(fit_long_edge(1815, 867, 1024), (1024, 489));
        assert_eq!(fit_long_edge(1024, 700, 1024), (1024, 700));
        assert_eq!(fit_long_edge(800, 600, 1024), (800, 600));
        assert_eq!(fit_long_edge(1, 1, 1024), (1, 1));
        assert_eq!(fit_long_edge(5000, 1, 1024), (1024, 1));
        assert_eq!(fit_long_edge(1, 5000, 1024), (1, 1024));

        let small = build(&photo_like_png(640, 480)).unwrap();
        assert_eq!((small.width, small.height), (640, 480));
        let (w, h, _) = frame_header(&decoded(&small));
        assert_eq!((w, h), (640, 480));
        assert_eq!(
            small.note(),
            "the preview is a 640px JPEG; the file is the full-resolution original"
        );
    }

    #[test]
    fn the_downscale_is_an_exact_area_average() {
        // 3 -> 2: each output pixel covers one and a half inputs.
        let row = Pixels {
            width: 3,
            height: 1,
            channels: 1,
            data: vec![0, 90, 180],
        };
        assert_eq!(downscale(&row, 2, 1).data, vec![30, 150]);

        // 4x4 -> 2x2: plain 2x2 block means, per channel.
        let data: Vec<u8> = (0..16u8).flat_map(|i| [i * 10, 255 - i * 10, 7]).collect();
        let block = Pixels {
            width: 4,
            height: 4,
            channels: 3,
            data,
        };
        let small = downscale(&block, 2, 2);
        let mean = |idx: [u8; 4], f: &dyn Fn(u8) -> f32| {
            (idx.iter().map(|&i| f(i)).sum::<f32>() / 4.0 + 0.5) as u8
        };
        let red = |i: u8| f32::from(i * 10);
        let green = |i: u8| f32::from(255 - i * 10);
        assert_eq!(
            small.data,
            vec![
                mean([0, 1, 4, 5], &red),
                mean([0, 1, 4, 5], &green),
                7,
                mean([2, 3, 6, 7], &red),
                mean([2, 3, 6, 7], &green),
                7,
                mean([8, 9, 12, 13], &red),
                mean([8, 9, 12, 13], &green),
                7,
                mean([10, 11, 14, 15], &red),
                mean([10, 11, 14, 15], &green),
                7,
            ]
        );

        // A flat image stays exactly flat, whatever the ratio.
        let flat = Pixels {
            width: 1312,
            height: 7,
            channels: 1,
            data: vec![123; 1312 * 7],
        };
        assert!(downscale(&flat, 1024, 5).data.iter().all(|&v| v == 123));
    }

    #[test]
    fn transparent_pixels_do_not_darken_the_edges_they_are_averaged_with() {
        // Opaque white beside fully transparent black: premultiplied, the average is white at
        // half coverage, not a grey that would show as a dark fringe.
        let pair = Pixels {
            width: 2,
            height: 1,
            channels: 4,
            data: vec![255, 255, 255, 255, 0, 0, 0, 0],
        };
        assert_eq!(downscale(&pair, 1, 1).data, vec![255, 255, 255, 128]);
        let gone = Pixels {
            width: 2,
            height: 1,
            channels: 2,
            data: vec![9, 0, 200, 0],
        };
        assert_eq!(downscale(&gone, 1, 1).data, vec![0, 0]);
    }

    #[test]
    fn flattening_composites_onto_white() {
        let image = Pixels {
            width: 3,
            height: 1,
            channels: 4,
            data: vec![0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 128],
        };
        let flat = flatten_onto_white(&image);
        assert_eq!(flat.channels, 3);
        assert_eq!(flat.data, vec![255, 255, 255, 0, 0, 0, 127, 127, 127]);
        let grey = Pixels {
            width: 1,
            height: 1,
            channels: 2,
            data: vec![100, 255],
        };
        assert_eq!(flatten_onto_white(&grey).data, vec![100]);
    }

    #[test]
    fn an_image_with_transparency_gets_a_png_preview() {
        let png = png_from(1200, 600, 4, |x, _, c| match c {
            3 if x < 600 => 0,
            3 => 255,
            _ => 40,
        });
        let preview = build(&png).unwrap();
        assert_eq!(preview.mime_type, "image/png");
        assert_eq!(preview.quality, None);
        assert!(!preview.flattened);
        assert!(preview.encoded_bytes <= MAX_ALPHA_PNG_BYTES);
        assert_eq!((preview.width, preview.height), (1024, 512));
        assert!(preview
            .note()
            .contains("1024px PNG, because the image has transparency"));
        let back = decode(&decoded(&preview)).unwrap();
        assert_eq!((back.width, back.height, back.channels), (1024, 512, 4));
        // The transparent half stays transparent, the opaque half opaque.
        assert_eq!(back.data[3], 0);
        assert_eq!(back.data[back.data.len() - 1], 255);
    }

    #[test]
    fn a_large_transparent_preview_is_flattened_onto_white_as_jpeg() {
        // Noise does not compress, so the PNG preview is far over 500 KB.
        let png = png_from(1100, 1100, 4, |x, y, c| {
            if c == 3 {
                if (x / 50 + y / 50) % 2 == 0 {
                    0
                } else {
                    255
                }
            } else {
                (noise(x, y, c as u32) % 256) as u8
            }
        });
        let preview = build(&png).unwrap();
        assert_eq!(preview.mime_type, "image/jpeg");
        assert!(preview.flattened);
        assert!(preview.encoded_bytes <= MAX_PREVIEW_BYTES);
        assert_eq!((preview.width, preview.height), (1024, 1024));
        assert!(
            preview.note().contains("transparency flattened onto white"),
            "{}",
            preview.note()
        );
        let (_, _, components) = frame_header(&decoded(&preview));
        assert_eq!(components.len(), 3);
    }

    #[test]
    fn an_opaque_alpha_channel_is_not_transparency() {
        let png = png_from(
            200,
            100,
            4,
            |x, y, c| {
                if c == 3 {
                    255
                } else {
                    (x + y) as u8
                }
            },
        );
        let preview = build(&png).unwrap();
        assert_eq!(preview.mime_type, "image/jpeg");
        assert!(!preview.flattened);
        assert_eq!(frame_header(&decoded(&preview)).2.len(), 3);

        let grey_alpha = png_from(50, 50, 2, |x, _, c| if c == 1 { 255 } else { x as u8 });
        let preview = build(&grey_alpha).unwrap();
        assert_eq!(preview.mime_type, "image/jpeg");
        assert_eq!(frame_header(&decoded(&preview)).2.len(), 1);
    }

    #[test]
    fn noise_that_will_not_fit_at_85_steps_down_the_quality_ladder() {
        // Pure noise is JPEG's worst case: about 500 KB even at quality 50, at 1024 px.
        let png = png_from(1024, 1024, 3, |x, y, c| (noise(x, y, c as u32) % 256) as u8);
        let preview = build(&png).unwrap();
        assert!(preview.encoded_bytes <= MAX_PREVIEW_BYTES);
        let quality = preview.quality.unwrap();
        assert!(quality < JPEG_QUALITY, "{quality}");
        assert!(JPEG_QUALITY_LADDER.contains(&quality));
        // When not even the last rung fits, that is an error to report, not a panic.
        let err = encode_jpeg_to_fit_within(&decode(&png).unwrap(), 1000).unwrap_err();
        assert!(err.contains("over the 1000-byte limit"), "{err}");
    }

    #[test]
    fn palette_sixteen_bit_and_trns_images_are_normalised() {
        // A 16-bit RGB image decodes to 8-bit RGB.
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, 4, 2);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0xab; 4 * 2 * 6]).unwrap();
        }
        let image = decode(&out).unwrap();
        assert_eq!((image.channels, image.data.len()), (3, 4 * 2 * 3));
        assert!(image.data.iter().all(|&v| v == 0xab));

        // A palette image whose second entry is transparent decodes to RGBA with that pixel clear.
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, 2, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(vec![255, 0, 0, 0, 0, 255]);
            encoder.set_trns(vec![255, 0]);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0, 1]).unwrap();
        }
        let image = decode(&out).unwrap();
        assert_eq!(image.channels, 4);
        assert_eq!(image.data, vec![255, 0, 0, 255, 0, 0, 255, 0]);
        assert!(image.has_transparency());
        assert_eq!(build(&out).unwrap().mime_type, "image/png");
    }

    #[test]
    fn what_is_not_a_png_is_an_error_not_a_panic() {
        assert!(build(b"").unwrap_err().contains("not a readable PNG"));
        assert!(build(b"\xff\xd8\xff\xe0 a jpeg").is_err());
        let png = photo_like_png(20, 20);
        assert!(build(&png[..png.len() / 2]).is_err(), "a truncated PNG");
    }

    #[test]
    fn png_dimensions_come_from_the_header() {
        let png = photo_like_png(37, 12);
        assert_eq!(png_dimensions(&png), Some((37, 12)));
        assert_eq!(png_dimensions(b"\x89PNG\r\n\x1a\n"), None);
        assert_eq!(png_dimensions(b"not a png at all, not even close"), None);
    }

    #[test]
    fn base64_matches_the_base64_crate_for_every_length_up_to_2k() {
        let engine = base64::engine::general_purpose::STANDARD;
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut buf = Vec::new();
        for len in 0..2048usize {
            buf.clear();
            for _ in 0..len {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                buf.push(x as u8);
            }
            let ours = base64_encode(&buf);
            assert_eq!(ours, engine.encode(&buf), "encode, length {len}");
            assert_eq!(
                base64_decode(ours.as_bytes()).as_deref(),
                Some(buf.as_slice()),
                "decode, length {len}"
            );
        }
    }

    #[test]
    fn malformed_base64_is_refused() {
        for bad in [
            &b"abc"[..],
            b"ab=c",
            b"a===",
            b"====",
            b"ab==cd==",
            b"ab\ncd==",
            b"ab-_",
            b"abc\0",
        ] {
            assert_eq!(
                base64_decode(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        assert_eq!(base64_decode(b""), Some(Vec::new()));
        assert_eq!(base64_decode(b"TWE=").as_deref(), Some(&b"Ma"[..]));
        assert_eq!(base64_decode(b"TQ==").as_deref(), Some(&b"M"[..]));
    }
}
