//! HTTPS to the OpenAI API through WinHTTP, over raw FFI: no new crate (docs/design.md, "Precise
//! mode", "Network").
//!
//! The rules that keep the API key where it belongs are structural:
//! - the host is the constant [`API_HOST`], port 443, TLS only, and there is no way to name
//!   another;
//! - redirects are never followed, so the `Authorization` header cannot travel on to another host;
//! - TLS is 1.2 or newer, and certificate validation is left on: no security flag that would
//!   ignore a certificate problem is ever set (a test checks the source for their names);
//! - the key is added only as the `Authorization` header and appears in no error text.

use std::ffi::c_void;
use std::io;
use std::time::Duration;

use crate::credential::Secret;

/// The only host precise mode talks to [decided].
pub const API_HOST: &str = "api.openai.com";

/// A response bigger than this is refused rather than read on: four images of a few MB each, as
/// base64, are well under it.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

const USER_AGENT: &str = concat!("codex-imagegen/", env!("CARGO_PKG_VERSION"));

/// An HTTP request to [`API_HOST`].
pub struct Request<'a> {
    pub method: &'static str,
    /// The path, starting with `/v1/`.
    pub path: &'a str,
    /// A JSON body, sent with `Content-Type: application/json`.
    pub body: Option<&'a [u8]>,
    /// The exchange's limit: each WinHTTP step (resolve, connect, send, receive) gets it, and the
    /// response is read only while it lasts, so a slow trickle of data cannot run past it by much.
    pub timeout: Duration,
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Why a request got no HTTP response.
#[derive(Debug, PartialEq, Eq)]
pub enum TransportError {
    TimedOut,
    Failed(String),
}

/// How a precise call reaches the API. A trait so the unit tests run against a scripted fake and
/// never touch the network.
pub trait Transport: Send + Sync {
    fn send(&self, key: &Secret, request: &Request<'_>) -> Result<Response, TransportError>;
}

/// The real transport: WinHTTP.
pub struct WinHttp;

impl Transport for WinHttp {
    fn send(&self, key: &Secret, request: &Request<'_>) -> Result<Response, TransportError> {
        send(key, request)
    }
}

type HInternet = *mut c_void;

const WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY: u32 = 4;
const WINHTTP_FLAG_SECURE: u32 = 0x0080_0000;
const INTERNET_DEFAULT_HTTPS_PORT: u16 = 443;
const WINHTTP_OPTION_SECURE_PROTOCOLS: u32 = 84;
const WINHTTP_OPTION_REDIRECT_POLICY: u32 = 88;
const WINHTTP_OPTION_REDIRECT_POLICY_NEVER: u32 = 0;
const WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2: u32 = 0x0000_0800;
const WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3: u32 = 0x0000_2000;
const WINHTTP_QUERY_STATUS_CODE: u32 = 19;
const WINHTTP_QUERY_FLAG_NUMBER: u32 = 0x2000_0000;
const ERROR_WINHTTP_TIMEOUT: i32 = 12002;

#[link(name = "winhttp")]
extern "system" {
    fn WinHttpOpen(
        agent: *const u16,
        access_type: u32,
        proxy: *const u16,
        bypass: *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpConnect(
        session: HInternet,
        server: *const u16,
        port: u16,
        reserved: u32,
    ) -> HInternet;
    fn WinHttpOpenRequest(
        connect: HInternet,
        verb: *const u16,
        object: *const u16,
        version: *const u16,
        referrer: *const u16,
        accept_types: *const *const u16,
        flags: u32,
    ) -> HInternet;
    fn WinHttpSetOption(handle: HInternet, option: u32, buffer: *const c_void, length: u32) -> i32;
    fn WinHttpSetTimeouts(
        handle: HInternet,
        resolve: i32,
        connect: i32,
        send: i32,
        receive: i32,
    ) -> i32;
    fn WinHttpSendRequest(
        request: HInternet,
        headers: *const u16,
        headers_length: u32,
        optional: *const c_void,
        optional_length: u32,
        total_length: u32,
        context: usize,
    ) -> i32;
    fn WinHttpReceiveResponse(request: HInternet, reserved: *mut c_void) -> i32;
    fn WinHttpQueryHeaders(
        request: HInternet,
        info_level: u32,
        name: *const u16,
        buffer: *mut c_void,
        length: *mut u32,
        index: *mut u32,
    ) -> i32;
    fn WinHttpQueryDataAvailable(request: HInternet, available: *mut u32) -> i32;
    fn WinHttpReadData(
        request: HInternet,
        buffer: *mut c_void,
        to_read: u32,
        read: *mut u32,
    ) -> i32;
    fn WinHttpCloseHandle(handle: HInternet) -> i32;
}

/// A WinHTTP handle, closed when dropped.
struct Handle(HInternet);

impl Handle {
    fn new(raw: HInternet, what: &str) -> Result<Self, TransportError> {
        if raw.is_null() {
            Err(last_error(what))
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: a handle WinHTTP returned and nothing else closes.
        unsafe { WinHttpCloseHandle(self.0) };
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error(what: &str) -> TransportError {
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_WINHTTP_TIMEOUT) {
        return TransportError::TimedOut;
    }
    TransportError::Failed(format!("{what} failed ({error})"))
}

fn check(ok: i32, what: &str) -> Result<(), TransportError> {
    if ok == 0 {
        Err(last_error(what))
    } else {
        Ok(())
    }
}

fn set_u32(handle: &Handle, option: u32, value: u32) -> i32 {
    // SAFETY: `value` is a DWORD that outlives the call, and its length is given.
    unsafe {
        WinHttpSetOption(
            handle.0,
            option,
            (&value as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    }
}

/// The request headers, as UTF-16 for WinHTTP. The key goes here and nowhere else; it is encoded
/// straight into this buffer, sized up front so it never reallocates and leaves a partial copy
/// behind, and the caller overwrites it after the send. WinHTTP keeps its own copy, which this
/// code cannot reach.
fn headers(key: &Secret, json: bool) -> Vec<u16> {
    let mut text: Vec<u16> = Vec::with_capacity(64 + key.expose().len());
    text.extend("Authorization: Bearer ".encode_utf16());
    text.extend(key.expose().encode_utf16());
    text.extend("\r\n".encode_utf16());
    if json {
        text.extend("Content-Type: application/json\r\n".encode_utf16());
    }
    text
}

fn send(key: &Secret, request: &Request<'_>) -> Result<Response, TransportError> {
    let deadline = std::time::Instant::now() + request.timeout;
    let agent = wide(USER_AGENT);
    // SAFETY: NUL-terminated strings that outlive the call; null proxy names as the access type
    // requires.
    let session = Handle::new(
        unsafe {
            WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            )
        },
        "WinHttpOpen",
    )?;
    // TLS 1.2 or 1.3; a Windows that does not know 1.3 refuses the pair, and gets 1.2 alone.
    if set_u32(
        &session,
        WINHTTP_OPTION_SECURE_PROTOCOLS,
        WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2 | WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_3,
    ) == 0
    {
        check(
            set_u32(
                &session,
                WINHTTP_OPTION_SECURE_PROTOCOLS,
                WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_2,
            ),
            "requiring TLS 1.2",
        )?;
    }
    let millis = request.timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
    // SAFETY: a valid session handle.
    check(
        unsafe { WinHttpSetTimeouts(session.0, millis, millis, millis, millis) },
        "setting timeouts",
    )?;
    let host = wide(API_HOST);
    // SAFETY: a valid session handle and a NUL-terminated host.
    let connection = Handle::new(
        unsafe { WinHttpConnect(session.0, host.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0) },
        "WinHttpConnect",
    )?;
    let verb = wide(request.method);
    let path = wide(request.path);
    // SAFETY: a valid connection handle; NUL-terminated strings; null version, referrer and
    // accept types take the defaults.
    let http = Handle::new(
        unsafe {
            WinHttpOpenRequest(
                connection.0,
                verb.as_ptr(),
                path.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            )
        },
        "WinHttpOpenRequest",
    )?;
    check(
        set_u32(
            &http,
            WINHTTP_OPTION_REDIRECT_POLICY,
            WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        ),
        "refusing redirects",
    )?;

    let body = request.body.unwrap_or(&[]);
    let mut header_text = headers(key, request.body.is_some());
    let body_length = u32::try_from(body.len())
        .map_err(|_| TransportError::Failed("the request body is too large".into()))?;
    // SAFETY: a valid request handle; the headers and body outlive the call, with their lengths
    // given (headers in characters, the body in bytes).
    let sent = unsafe {
        WinHttpSendRequest(
            http.0,
            header_text.as_ptr(),
            header_text.len() as u32,
            if body.is_empty() {
                std::ptr::null()
            } else {
                body.as_ptr().cast()
            },
            body_length,
            body_length,
            0,
        )
    };
    // The header copy held the key: overwrite it whatever happened.
    for unit in header_text.iter_mut() {
        // SAFETY: a valid pointer into our own buffer.
        unsafe { std::ptr::write_volatile(unit, 0) };
    }
    check(sent, "sending the request")?;
    // SAFETY: a valid request handle, after a successful send.
    check(
        unsafe { WinHttpReceiveResponse(http.0, std::ptr::null_mut()) },
        "receiving the response",
    )?;

    let mut status = 0u32;
    let mut length = std::mem::size_of::<u32>() as u32;
    // SAFETY: `status` is a DWORD buffer of the given length.
    check(
        unsafe {
            WinHttpQueryHeaders(
                http.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                (&mut status as *mut u32).cast(),
                &mut length,
                std::ptr::null_mut(),
            )
        },
        "reading the status code",
    )?;

    let mut body = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(TransportError::TimedOut);
        }
        let mut available = 0u32;
        // SAFETY: a valid request handle and out pointer.
        check(
            unsafe { WinHttpQueryDataAvailable(http.0, &mut available) },
            "reading the response",
        )?;
        if available == 0 {
            break;
        }
        if body.len() + available as usize > MAX_RESPONSE_BYTES {
            return Err(TransportError::Failed(format!(
                "the response is larger than {} MB",
                MAX_RESPONSE_BYTES / (1024 * 1024)
            )));
        }
        let start = body.len();
        body.resize(start + available as usize, 0);
        let mut read = 0u32;
        // SAFETY: the buffer has `available` bytes free from `start`.
        check(
            unsafe {
                WinHttpReadData(
                    http.0,
                    body[start..].as_mut_ptr().cast(),
                    available,
                    &mut read,
                )
            },
            "reading the response",
        )?;
        body.truncate(start + read as usize);
        if read == 0 {
            break;
        }
    }
    Ok(Response {
        status: status as u16,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_goes_only_into_the_authorization_header() {
        let key = Secret::new("sk-test-header-9999").unwrap();
        assert_eq!(
            String::from_utf16(&headers(&key, true)).unwrap(),
            "Authorization: Bearer sk-test-header-9999\r\nContent-Type: application/json\r\n"
        );
        assert_eq!(
            String::from_utf16(&headers(&key, false)).unwrap(),
            "Authorization: Bearer sk-test-header-9999\r\n"
        );
    }

    /// P1 and P4, free: a real HTTPS request to the API with a key that cannot be valid must come
    /// back as HTTP 401, which shows WinHTTP, TLS and the proxy setting work, and maps to
    /// API_KEY_REJECTED. Needs the network, so it is ignored by default:
    /// `cargo test -- --ignored a_made_up_key`.
    #[test]
    #[ignore = "network: run by hand, free"]
    fn a_made_up_key_is_refused_by_the_real_api_with_401() {
        let key = Secret::new("sk-codex-imagegen-not-a-real-key-0000").unwrap();
        let response = send(
            &key,
            &Request {
                method: "GET",
                path: "/v1/models",
                body: None,
                timeout: Duration::from_secs(30),
            },
        )
        .expect("an HTTP response from api.openai.com");
        assert_eq!(response.status, 401);
        let failure = crate::precise::map_error(response.status, &response.body);
        assert_eq!(failure.code, "API_KEY_REJECTED");
        // The API's message echoes part of the key; none of it may reach the result.
        let text = failure.render_for_agent();
        assert!(
            !text.contains("sk-codex") && !text.contains("0000"),
            "{text}"
        );
        eprintln!("{text}");
    }

    /// Certificate validation must never be switched off. The names are assembled here so this
    /// test does not match itself.
    #[test]
    fn no_certificate_check_is_ever_switched_off() {
        let source = include_str!("https.rs");
        for forbidden in [
            ["SECURITY_FLAG_", "IGNORE"].concat(),
            ["WINHTTP_OPTION_", "SECURITY_FLAGS"].concat(),
            ["WINHTTP_OPTION_", "SERVER_CERT_CONTEXT"].concat(),
        ] {
            assert!(
                !source.contains(&forbidden),
                "{forbidden} appears in https.rs"
            );
        }
        assert!(source.contains(&format!("\"{}\"", "api.openai.com")));
    }
}
