//! Newline-delimited JSON-RPC 2.0 framing, shared by both ends this server speaks.
//!
//! codex-imagegen is a JSON-RPC *server* to Claude Code (MCP over our stdio) and a JSON-RPC
//! *client* to `codex app-server` (over the child's stdio). Both are one JSON message per line,
//! so the pieces that are easy to get subtly wrong live here once: the locked writer, the capped
//! line reader, the borrowed envelope that classifies a message, request keys and log clamping.
//!
//! Hand-rolled rather than pulled from a crate: the surface is small, and keeping the dependency
//! list to serde is what lets this ship as one small executable.

use std::borrow::Cow;
use std::io::{self, BufRead, Read, Write};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

/// Where outgoing messages go. A trait object rather than a concrete stream so a test can read
/// back exactly what a code path wrote; in production it is our stdout (MCP side) or the child's
/// stdin (app-server side).
///
/// The mutex is what keeps two threads' messages from interleaving mid-line: every write of a
/// whole line happens under it.
pub type Writer = Arc<Mutex<dyn Write + Send>>;

/// Longest line accepted from the MCP client. MCP messages from Claude Code are small (tool
/// arguments are a prompt and a few paths); the cap only exists so a runaway client cannot make
/// us buffer without bound.
pub const MCP_MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Longest line accepted from `codex app-server`. An `item/completed` for an image carries the
/// whole PNG as base64 on one line, measured at up to about 3.8 MB [verified: smoke logs], so the
/// cap is far above that and exists only to bound a misbehaving child.
pub const APP_SERVER_MAX_LINE_BYTES: usize = 64 * 1024 * 1024;

/// Serialise `message`, append the newline and write it as one line, flushing under the lock.
/// Returns the I/O error rather than logging it, for a caller that must act on it (the app-server
/// client turns a failed write into a failed request).
///
/// Serialised before the lock is taken, so a large message never holds other writers up for
/// longer than the write itself.
pub fn try_send(writer: &Writer, message: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    line.push(b'\n');
    let mut out = writer.lock().unwrap_or_else(|e| e.into_inner());
    out.write_all(&line)?;
    out.flush()
}

/// [`try_send`] for a caller with nothing better to do with a failure than report it. stdout is
/// protocol traffic only, so the report goes to stderr.
pub fn send(writer: &Writer, message: &Value) {
    if let Err(e) = try_send(writer, message) {
        eprintln!("codex-imagegen: could not write a message: {e}");
    }
}

/// A stable map key for a JSON-RPC id, which may be a string or a number.
///
/// Serialising rather than stringifying keeps `1` and `"1"` distinct, as the protocol requires:
/// they are different requests and a peer may legitimately have both open.
pub fn request_key(id: &Value) -> String {
    id.to_string()
}

/// [`request_key`] for an id still in its raw form. Parsed first, so the key does not depend on
/// how the peer spaced or escaped the id: `"a"` and `"a"` are the same id.
pub fn request_key_raw(id: &RawValue) -> Option<String> {
    serde_json::from_str::<Value>(id.get())
        .ok()
        .map(|v| request_key(&v))
}

/// Drop a leading UTF-8 byte-order mark. Some clients prefix their first write with one, which is
/// not valid JSON; stripping it costs nothing and turns a hard failure into a non-event.
pub fn strip_bom(line: &str) -> &str {
    line.strip_prefix('\u{feff}').unwrap_or(line)
}

/// Bound a peer-supplied string before it reaches our diagnostics. Not a security boundary: it
/// keeps a peer from putting an unbounded line, or one that renders as other than what it says,
/// into a log a human reads.
///
/// `is_control` alone covers only Cc, which leaves the zero-width and bidi-override characters
/// that are the usual way to make a log line lie. std has no `is_format` and a unicode-tables
/// dependency is not worth it here, so those blocks are named outright.
pub fn clamp(text: &str, max: usize) -> String {
    let mut kept = text.chars().filter(|c| {
        !c.is_control()
            && !matches!(c,
                '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}')
    });
    let mut out: String = kept.by_ref().take(max).collect();
    // Marked, so a clipped value is never mistaken for a complete one.
    if kept.next().is_some() {
        out.push('…');
    }
    out
}

/// One read from a [`LineReader`].
#[derive(Debug, PartialEq, Eq)]
pub enum ReadLine<'a> {
    /// A complete line, without its line terminator.
    Line(&'a str),
    /// A line longer than the cap. It has been consumed up to and including its newline, so the
    /// stream stays in step, but its content is gone.
    TooLong,
    /// A line that is not UTF-8, and so cannot be JSON. Consumed like any other line.
    NotUtf8,
    /// End of stream.
    Eof,
}

/// Capacity kept between lines. A single image line grows the buffer to a few MB, which is worth
/// keeping for the next one; one that grew past this is released rather than held for the life of
/// the process.
const RETAINED_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Reads newline-delimited lines with a hard cap on line length, into one reused buffer.
///
/// `BufRead::lines` has no cap and allocates a new `String` per line, and it fails the whole
/// stream on a line that is not UTF-8. This reads bytes through `take(cap)`, so an oversized line
/// costs at most `cap` bytes of memory before the rest of it is skipped, and it reports a bad line
/// instead of ending the stream.
pub struct LineReader<R> {
    reader: R,
    cap: usize,
    buf: Vec<u8>,
}

impl<R: BufRead> LineReader<R> {
    /// `cap` counts the terminating newline.
    pub fn new(reader: R, cap: usize) -> Self {
        Self {
            reader,
            cap: cap.max(1),
            buf: Vec::new(),
        }
    }

    pub fn next_line(&mut self) -> io::Result<ReadLine<'_>> {
        if self.buf.capacity() > RETAINED_BUFFER_BYTES {
            self.buf = Vec::new();
        }
        self.buf.clear();
        let n = self
            .reader
            .by_ref()
            .take(self.cap as u64)
            .read_until(b'\n', &mut self.buf)?;
        if n == 0 {
            return Ok(ReadLine::Eof);
        }
        if self.buf.last() != Some(&b'\n') && n >= self.cap {
            // The cap was reached mid-line. Skip to the end of it without buffering, so the next
            // read starts on a line boundary.
            self.reader.skip_until(b'\n')?;
            self.buf.clear();
            return Ok(ReadLine::TooLong);
        }
        let mut end = self.buf.len();
        if end > 0 && self.buf[end - 1] == b'\n' {
            end -= 1;
        }
        if end > 0 && self.buf[end - 1] == b'\r' {
            end -= 1;
        }
        match std::str::from_utf8(&self.buf[..end]) {
            Ok(line) => Ok(ReadLine::Line(line)),
            Err(_) => Ok(ReadLine::NotUtf8),
        }
    }
}

/// The top level of any JSON-RPC message, borrowed from the line it was parsed from.
///
/// Every member is left raw, so classifying a message costs a scan of the line and no copy of its
/// payload: an `item/completed` carrying a 3.8 MB base64 image is routed without that image ever
/// being copied into a `serde_json::Value` (see docs/design.md, "Message handling"). A member
/// that is absent or JSON `null` reads as `None`.
///
/// Unknown members (such as `jsonrpc`) are ignored, which is also what makes this tolerant of a
/// peer that omits `"jsonrpc":"2.0"` from its replies, as the app-server does [verified].
#[derive(Debug)]
pub struct Envelope<'a> {
    pub id: Option<&'a RawValue>,
    /// Borrowed from the line unless it contains an escape.
    pub method: Option<Cow<'a, str>>,
    pub params: Option<&'a RawValue>,
    pub result: Option<&'a RawValue>,
    pub error: Option<&'a RawValue>,
}

/// What serde parses. `method` stays raw here too: serde's `borrow` does not reach through an
/// `Option<Cow<str>>`, so deriving it on `Envelope` would copy every method name. It is unwrapped
/// by hand in `Envelope::parse` instead.
#[derive(Deserialize)]
struct RawEnvelope<'a> {
    #[serde(borrow, default)]
    id: Option<&'a RawValue>,
    #[serde(borrow, default)]
    method: Option<&'a RawValue>,
    #[serde(borrow, default)]
    params: Option<&'a RawValue>,
    #[serde(borrow, default)]
    result: Option<&'a RawValue>,
    #[serde(borrow, default)]
    error: Option<&'a RawValue>,
}

/// A JSON string member as text: borrowed when it has no escapes, decoded otherwise. `Err` if the
/// member is not a string.
fn string_member(raw: &RawValue) -> serde_json::Result<Cow<'_, str>> {
    match serde_json::from_str::<&str>(raw.get()) {
        Ok(text) => Ok(Cow::Borrowed(text)),
        Err(_) => serde_json::from_str::<String>(raw.get()).map(Cow::Owned),
    }
}

/// What kind of JSON-RPC message an [`Envelope`] holds.
#[derive(Debug)]
pub enum Kind<'e> {
    /// Has an id and a method: the sender expects an answer.
    Request { id: &'e RawValue, method: &'e str },
    /// Has a method and no id: must never be answered.
    Notification { method: &'e str },
    /// Has an id and no method: answers a request we sent.
    Response { id: &'e RawValue },
    /// Neither: nothing can be done with it except log it.
    Invalid,
}

impl<'a> Envelope<'a> {
    /// Parse the envelope of one line. `Err` means the line is not a JSON object with the
    /// expected member types; [`parse_failure_code`] says which JSON-RPC error that is.
    pub fn parse(line: &'a str) -> serde_json::Result<Self> {
        let raw: RawEnvelope<'a> = serde_json::from_str(line)?;
        Ok(Self {
            id: raw.id,
            method: raw.method.map(string_member).transpose()?,
            params: raw.params,
            result: raw.result,
            error: raw.error,
        })
    }

    pub fn kind(&self) -> Kind<'_> {
        match (self.id, self.method.as_deref()) {
            (Some(id), Some(method)) => Kind::Request { id, method },
            (None, Some(method)) => Kind::Notification { method },
            (Some(id), None) => Kind::Response { id },
            (None, None) => Kind::Invalid,
        }
    }

    /// The id as an owned value, for echoing it back exactly as it was sent.
    pub fn id_value(&self) -> Option<Value> {
        self.id.and_then(|id| serde_json::from_str(id.get()).ok())
    }

    /// The params as an owned value (`Null` when absent). For small messages only: this copies.
    pub fn params_value(&self) -> Value {
        self.params
            .and_then(|p| serde_json::from_str(p.get()).ok())
            .unwrap_or(Value::Null)
    }
}

/// The JSON-RPC error code for a line whose [`Envelope`] would not parse: `-32700` (parse error)
/// when the line is not JSON at all, `-32600` (invalid request) when it is JSON but not a message
/// object. Only called on the failure path, so the second parse costs nothing in normal traffic.
pub fn parse_failure_code(line: &str) -> i64 {
    match serde_json::from_str::<serde::de::IgnoredAny>(line) {
        Ok(_) => -32600,
        Err(_) => -32700,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Recorder;
    use serde_json::json;

    fn lines_of(input: &[u8], cap: usize) -> Vec<String> {
        let mut reader = LineReader::new(input, cap);
        let mut out = Vec::new();
        loop {
            match reader.next_line().expect("read") {
                ReadLine::Line(l) => out.push(format!("L:{l}")),
                ReadLine::TooLong => out.push("TOO_LONG".into()),
                ReadLine::NotUtf8 => out.push("NOT_UTF8".into()),
                ReadLine::Eof => break,
            }
        }
        out
    }

    #[test]
    fn send_writes_one_terminated_line_per_message() {
        let recorder = Recorder::default();
        let writer = recorder.writer();
        send(&writer, &json!({"a": 1}));
        send(&writer, &json!({"b": "x\ny"}));
        // An embedded newline is escaped by the serialiser, so content cannot break the framing:
        // two messages, two lines.
        assert_eq!(
            recorder.messages(),
            vec![json!({"a": 1}), json!({"b": "x\ny"})]
        );
    }

    #[test]
    fn a_numeric_and_a_string_request_id_are_different_requests() {
        assert_ne!(request_key(&json!(1)), request_key(&json!("1")));
        let raw: Box<RawValue> = serde_json::from_str(r#""a""#).unwrap();
        assert_eq!(request_key_raw(&raw).unwrap(), request_key(&json!("a")));
    }

    #[test]
    fn a_leading_bom_is_stripped_and_nothing_else() {
        assert_eq!(strip_bom("\u{feff}{}"), "{}");
        assert_eq!(strip_bom("{}"), "{}");
        assert_eq!(strip_bom("x\u{feff}"), "x\u{feff}");
    }

    #[test]
    fn clamp_bounds_length_and_removes_characters_that_hide_text() {
        assert_eq!(clamp("abc", 10), "abc");
        assert_eq!(clamp("abcdef", 3), "abc…");
        assert_eq!(clamp("a\u{202e}b\u{200b}c\nd", 10), "abcd");
    }

    #[test]
    fn lines_are_split_on_lf_with_an_optional_cr() {
        assert_eq!(
            lines_of(b"one\r\ntwo\nthree", 100),
            vec!["L:one", "L:two", "L:three"]
        );
        assert_eq!(lines_of(b"", 100), Vec::<String>::new());
        assert_eq!(lines_of(b"\n", 100), vec!["L:"]);
    }

    #[test]
    fn an_oversized_line_is_skipped_and_the_stream_stays_in_step() {
        // cap 8 counts the newline: "1234567\n" fits, "12345678\n" does not.
        let input = b"1234567\n123456789abcdef\nok\n12345678\nend";
        assert_eq!(
            lines_of(input, 8),
            vec!["L:1234567", "TOO_LONG", "L:ok", "TOO_LONG", "L:end"]
        );
    }

    #[test]
    fn a_line_that_is_not_utf8_is_reported_not_fatal() {
        assert_eq!(
            lines_of(b"a\n\xff\xfe\nb\n", 100),
            vec!["L:a", "NOT_UTF8", "L:b"]
        );
    }

    #[test]
    fn the_envelope_classifies_every_message_kind() {
        let req = Envelope::parse(r#"{"jsonrpc":"2.0","id":7,"method":"m","params":{}}"#).unwrap();
        assert!(matches!(req.kind(), Kind::Request { method: "m", .. }));
        assert_eq!(req.id_value(), Some(json!(7)));

        let note = Envelope::parse(r#"{"method":"n","params":{"x":1}}"#).unwrap();
        assert!(matches!(note.kind(), Kind::Notification { method: "n" }));
        assert_eq!(note.params_value(), json!({"x": 1}));

        let reply = Envelope::parse(r#"{"id":"a","result":{"ok":true}}"#).unwrap();
        assert!(matches!(reply.kind(), Kind::Response { .. }));
        assert_eq!(reply.result.unwrap().get(), r#"{"ok":true}"#);

        // A null id is no id: such a message can never be answered.
        let null_id = Envelope::parse(r#"{"id":null,"method":"n"}"#).unwrap();
        assert!(matches!(null_id.kind(), Kind::Notification { method: "n" }));

        assert!(matches!(
            Envelope::parse("{}").unwrap().kind(),
            Kind::Invalid
        ));

        // A legal escape in the method name is decoded rather than rejected.
        let escaped = Envelope::parse(r#"{"method":"notifications\/cancelled"}"#).unwrap();
        assert!(matches!(
            escaped.kind(),
            Kind::Notification {
                method: "notifications/cancelled"
            }
        ));
    }

    #[test]
    fn the_envelope_borrows_the_payload_rather_than_copying_it() {
        let big = "A".repeat(1 << 20);
        let line = format!(r#"{{"method":"item/completed","params":{{"result":"{big}"}}}}"#);
        let envelope = Envelope::parse(&line).unwrap();
        let params = envelope.params.unwrap().get();
        // The raw params point into `line` itself.
        let line_range = line.as_ptr() as usize..line.as_ptr() as usize + line.len();
        assert!(line_range.contains(&(params.as_ptr() as usize)));
        assert!(matches!(envelope.method, Some(Cow::Borrowed(_))));
    }

    #[test]
    fn a_line_that_is_not_a_message_object_is_told_apart_from_bad_json() {
        assert!(Envelope::parse("[1,2]").is_err());
        assert_eq!(parse_failure_code("[1,2]"), -32600);
        assert_eq!(parse_failure_code(r#"{"method": 5}"#), -32600);
        assert_eq!(parse_failure_code("{not json"), -32700);
    }
}
