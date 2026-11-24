//! The OpenAI API key for precise mode, kept in Windows Credential Manager (docs/design.md,
//! "Precise mode").
//!
//! The key is a generic credential, persisted on this computer only and never roaming. It never
//! enters an environment variable, a flag, a file of ours or a log line: [`Secret`] zeroes its
//! bytes when dropped and prints as `Secret(…abcd)`, so a stray `{:?}` cannot leak it. Credential
//! Manager keeps it from other Windows users; any program running as the same user can read it,
//! as it could read Codex's own `auth.json` [decided: out of scope].

use std::ffi::c_void;
use std::fmt;
use std::io::{self, BufRead, Write};

/// The credential's name in Credential Manager.
pub const TARGET: &str = "codex-imagegen/openai-api-key";

/// `CRED_MAX_CREDENTIAL_BLOB_SIZE` [assumed: P1, Microsoft Learn].
pub const MAX_KEY_BYTES: usize = 5 * 512;

const CRED_TYPE_GENERIC: u32 = 1;
/// Persists on this computer, never roams with a roaming profile [assumed: P1].
const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;
const ERROR_NOT_FOUND: i32 = 1168;
const STD_INPUT_HANDLE: u32 = -10i32 as u32;
const ENABLE_ECHO_INPUT: u32 = 0x0004;

#[repr(C)]
struct FileTime {
    low: u32,
    high: u32,
}

/// `CREDENTIALW`.
#[repr(C)]
struct CredentialW {
    flags: u32,
    kind: u32,
    target_name: *mut u16,
    comment: *mut u16,
    last_written: FileTime,
    credential_blob_size: u32,
    credential_blob: *mut u8,
    persist: u32,
    attribute_count: u32,
    attributes: *mut c_void,
    target_alias: *mut u16,
    user_name: *mut u16,
}

#[link(name = "advapi32")]
extern "system" {
    fn CredReadW(
        target: *const u16,
        kind: u32,
        flags: u32,
        credential: *mut *mut CredentialW,
    ) -> i32;
    fn CredWriteW(credential: *const CredentialW, flags: u32) -> i32;
    fn CredDeleteW(target: *const u16, kind: u32, flags: u32) -> i32;
    fn CredFree(buffer: *mut c_void);
}

#[link(name = "kernel32")]
extern "system" {
    fn GetStdHandle(which: u32) -> *mut c_void;
    fn GetConsoleMode(console: *mut c_void, mode: *mut u32) -> i32;
    fn SetConsoleMode(console: *mut c_void, mode: u32) -> i32;
}

/// An API key in memory. Zeroed when dropped; never printed.
pub struct Secret(Vec<u8>);

impl Secret {
    /// A key as typed or read: trimmed, then checked for shape only (printable ASCII, no spaces,
    /// within Credential Manager's limit). Whether OpenAI accepts it is checked by the caller.
    pub fn new(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("the key is empty".into());
        }
        if trimmed.len() > MAX_KEY_BYTES {
            return Err(format!(
                "the key is {} bytes, more than Credential Manager holds ({MAX_KEY_BYTES})",
                trimmed.len()
            ));
        }
        if !trimmed.bytes().all(|b| b.is_ascii_graphic()) {
            return Err("the key holds spaces or characters an API key never has".into());
        }
        Ok(Self(trimmed.as_bytes().to_vec()))
    }

    /// The key, for the one place it goes: the `Authorization` header.
    pub fn expose(&self) -> &str {
        // Only built from checked ASCII, by `new` or `read`.
        std::str::from_utf8(&self.0).unwrap_or("")
    }

    /// The last four characters, the most `status` and the key commands ever show.
    pub fn last4(&self) -> String {
        let text = self.expose();
        let start = text.len().saturating_sub(4);
        format!("…{}", &text[start..])
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        for byte in self.0.iter_mut() {
            // SAFETY: a valid, aligned pointer to a byte of our own buffer. Volatile so the
            // compiler cannot drop the write as dead before the free.
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({})", self.last4())
    }
}

/// Where a precise call gets its key. A trait so tests never touch the real Credential Manager.
pub trait KeySource: Send + Sync {
    /// The key, `Ok(None)` when none is stored.
    fn read(&self) -> Result<Option<Secret>, String>;
}

/// The real source: the generic credential [`TARGET`].
pub struct CredentialManager;

impl KeySource for CredentialManager {
    fn read(&self) -> Result<Option<Secret>, String> {
        read(TARGET)
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The generic credential `target`, `Ok(None)` when there is none.
pub fn read(target: &str) -> Result<Option<Secret>, String> {
    let name = wide(target);
    let mut credential: *mut CredentialW = std::ptr::null_mut();
    // SAFETY: `name` is NUL-terminated UTF-16 that outlives the call; `credential` receives a
    // buffer the API allocates, freed below with CredFree.
    let ok = unsafe { CredReadW(name.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
    if ok == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_NOT_FOUND) {
            return Ok(None);
        }
        return Err(format!("reading the credential {target} failed ({error})"));
    }
    // SAFETY: on success `credential` points at a CREDENTIALW whose blob holds
    // `credential_blob_size` bytes; both live until CredFree.
    let result = unsafe {
        let blob = std::slice::from_raw_parts(
            (*credential).credential_blob,
            (*credential).credential_blob_size as usize,
        );
        let secret = std::str::from_utf8(blob)
            .map_err(|_| format!("the credential {target} does not hold text"))
            .and_then(Secret::new);
        // Overwrite the API's copy before freeing it, as best effort.
        let blob_mut = std::slice::from_raw_parts_mut(
            (*credential).credential_blob,
            (*credential).credential_blob_size as usize,
        );
        for byte in blob_mut.iter_mut() {
            std::ptr::write_volatile(byte, 0);
        }
        CredFree(credential as *mut c_void);
        secret
    };
    result.map(Some)
}

/// Store `secret` as the generic credential `target`, replacing any there.
pub fn write(target: &str, secret: &Secret) -> Result<(), String> {
    let mut name = wide(target);
    let mut user = wide("openai");
    let mut blob = secret.0.clone();
    let credential = CredentialW {
        flags: 0,
        kind: CRED_TYPE_GENERIC,
        target_name: name.as_mut_ptr(),
        comment: std::ptr::null_mut(),
        last_written: FileTime { low: 0, high: 0 },
        credential_blob_size: blob.len() as u32,
        credential_blob: blob.as_mut_ptr(),
        persist: CRED_PERSIST_LOCAL_MACHINE,
        attribute_count: 0,
        attributes: std::ptr::null_mut(),
        target_alias: std::ptr::null_mut(),
        user_name: user.as_mut_ptr(),
    };
    // SAFETY: every pointer in `credential` points at a buffer of ours that outlives the call.
    let ok = unsafe { CredWriteW(&credential, 0) };
    for byte in blob.iter_mut() {
        // SAFETY: as in `Secret::drop`.
        unsafe { std::ptr::write_volatile(byte, 0) };
    }
    if ok == 0 {
        return Err(format!(
            "storing the credential {target} failed ({})",
            io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Delete the generic credential `target`. `Ok(false)` when there was none.
pub fn delete(target: &str) -> Result<bool, String> {
    let name = wide(target);
    // SAFETY: `name` is NUL-terminated UTF-16 that outlives the call.
    let ok = unsafe { CredDeleteW(name.as_ptr(), CRED_TYPE_GENERIC, 0) };
    if ok != 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_NOT_FOUND) {
        return Ok(false);
    }
    Err(format!("deleting the credential {target} failed ({error})"))
}

/// Prompt on the console and read one line with echo off. Refuses when stdin is not a console, so
/// a key never arrives through a pipe or a file that something may have logged.
pub fn read_hidden_line(prompt: &str) -> Result<String, String> {
    // SAFETY: GetStdHandle has no preconditions; a null or invalid handle makes GetConsoleMode
    // fail, which is handled.
    let console = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut mode = 0u32;
    // SAFETY: `mode` is a valid out pointer.
    if unsafe { GetConsoleMode(console, &mut mode) } == 0 {
        return Err(
            "run this in a terminal: the key is only read from the keyboard, with \
                    typing hidden, never from a pipe or a file"
                .into(),
        );
    }
    eprint!("{prompt}");
    let _ = io::stderr().flush();
    // SAFETY: the handle is the console input handle checked above.
    if unsafe { SetConsoleMode(console, mode & !ENABLE_ECHO_INPUT) } == 0 {
        return Err(format!(
            "could not hide typing in this console ({})",
            io::Error::last_os_error()
        ));
    }
    let mut line = String::new();
    let read = io::stdin().lock().read_line(&mut line);
    // SAFETY: as above; restores the mode the console had.
    unsafe { SetConsoleMode(console, mode) };
    eprintln!();
    read.map_err(|e| format!("could not read the key ({e})"))?;
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway credential name, so tests never touch the real one.
    fn test_target() -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("codex-imagegen-test/{}-{nanos:x}", std::process::id())
    }

    #[test]
    fn a_secret_is_trimmed_checked_and_never_printed() {
        let secret = Secret::new("  sk-proj-abcdEFGH1234\n").unwrap();
        assert_eq!(secret.expose(), "sk-proj-abcdEFGH1234");
        assert_eq!(secret.last4(), "…1234");
        let shown = format!("{secret:?}");
        assert_eq!(shown, "Secret(…1234)");
        assert!(!shown.contains("abcd"));
        assert!(Secret::new("").is_err());
        assert!(Secret::new("   ").is_err());
        assert!(Secret::new("sk has space").is_err());
        assert!(Secret::new("sk-\u{e9}").is_err());
        assert!(Secret::new(&"a".repeat(MAX_KEY_BYTES + 1)).is_err());
        assert!(Secret::new(&"a".repeat(MAX_KEY_BYTES)).is_ok());
        assert_eq!(Secret::new("ab").unwrap().last4(), "…ab");
    }

    #[test]
    fn a_credential_is_written_read_replaced_and_deleted() {
        let target = test_target();
        assert!(read(&target).unwrap().is_none());
        assert!(!delete(&target).unwrap());

        write(&target, &Secret::new("sk-test-first-0001").unwrap()).unwrap();
        assert_eq!(
            read(&target).unwrap().unwrap().expose(),
            "sk-test-first-0001"
        );
        write(&target, &Secret::new("sk-test-second-0002").unwrap()).unwrap();
        assert_eq!(
            read(&target).unwrap().unwrap().expose(),
            "sk-test-second-0002"
        );

        assert!(delete(&target).unwrap());
        assert!(read(&target).unwrap().is_none());
    }
}
