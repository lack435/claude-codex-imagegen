//! The session store and its locks (docs/design.md, "Sessions").
//!
//! - **The store.** One per project, `<state_dir>\sessions.json`: every session's record, keyed by
//!   the lowercased name, because names compare case-insensitively as NTFS file names do. The
//!   record keeps the name as it was first spelled, for file names.
//! - **Atomic writes.** New content goes to a temp file in the same folder, is flushed to disk, and
//!   is renamed over the live file. A reader sees the old store or the new one, never part of one,
//!   and a crash leaves one of them. The live file is never deleted first.
//! - **The store lock.** Every read-modify-write holds a `LockFileEx` lock on `sessions.lock`
//!   beside the store, so two processes recording different sessions never lose each other's
//!   update. The OS releases it when a process dies. Plain reads take no lock: the rename already
//!   makes them safe.
//! - **Leases.** A `LockFileEx` lock on `locks\<name>-<hash>.lock`, held for a whole generate or
//!   refine call and taken before the record is read. It keeps a same-name generate in another
//!   process from spending quota on a name this one is creating, and keeps each record's read,
//!   turn and write in order. A lease held elsewhere is not waited for: the call gets
//!   SESSION_BUSY, as a busy session in this process does [decided].
//!
//! Two reads. [`Store::read`] fails closed (STORE_CORRUPT) when the file exists but cannot be
//! parsed, because generate and refine would otherwise write over sessions they cannot see.
//! [`Store::read_tolerant`] is for `status`: it shows what still parses and says what is wrong.
//!
//! Lock files are never deleted, by this module or anyone else while they are held: they are
//! opened without delete sharing, so no one can delete a held one and have the next process lock a
//! fresh file beside it.

use std::cell::Cell;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::raw::c_void;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::fnv1a64;
use crate::errors::{self, Failure};
use crate::output;

/// The store's file name in the per-project state directory.
pub const STORE_FILE: &str = "sessions.json";

/// The store lock's file name, beside the store.
const LOCK_FILE: &str = "sessions.lock";

/// The folder of lease files, beside the store.
const LEASE_DIR: &str = "locks";

/// The store format this build reads and writes. A file with another version is refused rather
/// than rewritten, since rewriting it would drop whatever a newer build keeps in it.
pub const STORE_VERSION: u32 = 1;

/// How long a read-modify-write waits for the store lock. Each holder keeps it for one small read
/// and write, so a lock held this long belongs to a stuck process.
const STORE_LOCK_WAIT: Duration = Duration::from_secs(10);

/// How often a wait for the store lock tries again.
const LOCK_POLL: Duration = Duration::from_millis(10);

const FILE_SHARE_READ: u32 = 0x1;
const FILE_SHARE_WRITE: u32 = 0x2;
const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x1;
const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x2;
/// What `LockFileEx` with `LOCKFILE_FAIL_IMMEDIATELY` fails with while another handle holds it.
const ERROR_LOCK_VIOLATION: i32 = 33;

/// OVERLAPPED. For a synchronous handle only the offset is read: the byte range starts there.
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut c_void,
}

impl Overlapped {
    fn at_start() -> Self {
        Self {
            internal: 0,
            internal_high: 0,
            offset: 0,
            offset_high: 0,
            event: std::ptr::null_mut(),
        }
    }
}

extern "system" {
    fn LockFileEx(
        file: *mut c_void,
        flags: u32,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut Overlapped,
    ) -> i32;
    fn UnlockFileEx(
        file: *mut c_void,
        reserved: u32,
        bytes_low: u32,
        bytes_high: u32,
        overlapped: *mut Overlapped,
    ) -> i32;
}

/// Unique temp names within this process; the pid makes them unique across processes.
static SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// One session (docs/design.md, "Record fields"). Times are Unix seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The name as the session was created. Lookups ignore case; file names use this spelling.
    pub name: String,
    pub thread_id: String,
    /// The Codex home the thread lives in: the child's `codexHome` from its handshake.
    pub codex_home: PathBuf,
    pub model: String,
    pub created: i64,
    pub updated: i64,
    /// Turns that completed at least one image.
    pub turns: u32,
    /// Codex's copy of the latest image (`savedPath`), if Codex reported one.
    pub last_saved_path: Option<PathBuf>,
    /// Our copy of the latest image, if the copy was published.
    pub last_output_path: Option<PathBuf>,
    /// The latest image's size. Both copies are byte-identical, so it checks either.
    pub last_output_bytes: Option<u64>,
    pub output_dir: PathBuf,
    /// The version the next image is published as: the last one actually used, plus one.
    pub next_version: u32,
    /// Every file this server published for the session. Cleanup deletes only these, and only
    /// while the path, size and content fingerprint still match.
    pub outputs: Vec<Output>,
}

/// A published file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub version: u32,
    pub path: PathBuf,
    pub bytes: u64,
    /// The 64-bit FNV-1a of the bytes published, as 16 lowercase hex digits: accidental-change
    /// detection, not a MAC [decided]. Cleanup deletes the file only while it still matches.
    /// Absent from records written before it existed; cleanup then cannot prove the file is the
    /// one published, and keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fnv1a64: Option<String>,
}

impl Output {
    /// How a fingerprint is recorded: 16 lowercase hex digits. A string rather than a JSON number,
    /// which many readers take as a double and would round.
    pub fn fingerprint_text(fingerprint: u64) -> String {
        format!("{fingerprint:016x}")
    }

    /// The recorded fingerprint, `None` when there is none or it is not 16 hex digits.
    pub fn fingerprint(&self) -> Option<u64> {
        let text = self.fnv1a64.as_deref()?;
        if text.len() != 16 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u64::from_str_radix(text, 16).ok()
    }
}

impl Record {
    /// The latest image's path for display: our copy, else Codex's.
    pub fn latest_path(&self) -> Option<&Path> {
        self.last_output_path
            .as_deref()
            .or(self.last_saved_path.as_deref())
    }

    /// Record a completed image as the session's latest.
    fn apply(&mut self, image: &ImageOutcome<'_>, now: i64) {
        self.updated = now;
        // Both from this image, even when absent: an older version is never the latest.
        self.last_saved_path = image.saved_path.map(Path::to_path_buf);
        self.last_output_path = image.output_path.map(Path::to_path_buf);
        self.last_output_bytes = image.bytes;
        if let (Some(version), Some(path), Some(bytes)) =
            (image.version, image.output_path, image.bytes)
        {
            self.outputs.push(Output {
                version,
                path: path.to_path_buf(),
                bytes,
                fnv1a64: image.fnv1a64.map(Output::fingerprint_text),
            });
            self.next_version = self.next_version.max(version.saturating_add(1));
        }
    }
}

/// The outcome of the last automatic expiry or manual sweep that covered this store, for
/// `status` (docs/design.md, "Cleanup").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastCleanup {
    /// Unix seconds.
    pub at: i64,
    pub removed: u32,
    pub freed_bytes: u64,
    pub skipped: Vec<Skipped>,
}

/// A session a cleanup left for next time, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub name: String,
    pub why: String,
}

/// The whole store file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreFile {
    pub version: u32,
    /// Keyed by [`key`].
    #[serde(default)]
    pub sessions: BTreeMap<String, Record>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cleanup: Option<LastCleanup>,
}

impl StoreFile {
    pub fn empty() -> Self {
        Self {
            version: STORE_VERSION,
            sessions: BTreeMap::new(),
            last_cleanup: None,
        }
    }

    /// The session named `name`, whatever its case.
    pub fn get(&self, name: &str) -> Option<&Record> {
        self.sessions.get(&key(name))
    }
}

/// The store key for a session name: lowercased, since names compare case-insensitively.
///
/// FROZEN persistence key, like the state directory's: it names records and lease files on disk.
pub fn key(name: &str) -> String {
    name.to_lowercase()
}

/// Now, in Unix seconds.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum StoreError {
    /// The store exists but is not one this build can read.
    Corrupt { path: PathBuf, detail: String },
    /// The store, its lock or a lease could not be used.
    Io {
        path: PathBuf,
        what: &'static str,
        error: io::Error,
    },
    /// The store is readable, but the change does not apply to what it holds.
    Conflict(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Corrupt { path, detail } => write!(
                f,
                "the session store {} cannot be read ({detail})",
                path.display()
            ),
            Self::Io { path, what, error } => {
                write!(f, "{what} {} failed ({error})", path.display())
            }
            Self::Conflict(why) => f.write_str(why),
        }
    }
}

impl StoreError {
    /// The failure a generate or refine call reports when it cannot use the store.
    pub fn failure(&self) -> Failure {
        match self {
            Self::Corrupt { path, detail } => errors::store_corrupt(path, detail.clone()),
            Self::Io { path, what, error } => {
                errors::store_unusable(path, format!("{what} {} failed: {error}", path.display()))
            }
            Self::Conflict(why) => errors::internal_error(format!("The session store: {why}.")),
        }
    }

    fn io(path: &Path, what: &'static str) -> impl FnOnce(io::Error) -> Self {
        let path = path.to_path_buf();
        move |error| Self::Io { path, what, error }
    }
}

// ---------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------

/// An exclusive `LockFileEx` lock on the first byte of a lock file, released when dropped (and by
/// the OS if the process dies). Locks exclude each other across processes and across handles in
/// one process alike.
struct FileLock {
    file: File,
}

impl FileLock {
    /// The lock, or `None` when another handle holds it. Never waits for the holder.
    fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        // No delete sharing, so a held lock file cannot be deleted from under its holder. A scanner
        // briefly opening the file exclusively is ridden out rather than read as "held".
        let file = output::retry_transient(|| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .open(path)
        })?;
        let mut overlapped = Overlapped::at_start();
        // SAFETY: the handle is valid while `file` lives; `overlapped` is a zeroed OVERLAPPED
        // (offset 0, no event) that outlives the call, which on a synchronous handle with
        // LOCKFILE_FAIL_IMMEDIATELY returns at once.
        let locked = unsafe {
            LockFileEx(
                file.as_raw_handle() as *mut c_void,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if locked != 0 {
            return Ok(Some(Self { file }));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION) {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let mut overlapped = Overlapped::at_start();
        // SAFETY: the handle is still open (the file closes after this), and the range is the one
        // locked. Closing would release it too, but only once the OS gets to it; unlocking first
        // frees it for the next process at once.
        unsafe {
            UnlockFileEx(
                self.file.as_raw_handle() as *mut c_void,
                0,
                1,
                0,
                &mut overlapped,
            );
        }
    }
}

/// A session's lease, held for a whole generate or refine call. Dropping it releases it.
pub struct Lease {
    _lock: FileLock,
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// One project's session store.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
    lock_wait: Duration,
}

/// A store read for display: the records that could be read, and what is wrong if that is not
/// all of them.
pub struct Tolerant {
    pub file: StoreFile,
    pub problem: Option<String>,
}

impl Store {
    /// The store in `state_dir`. Nothing is created until something is written or locked.
    pub fn new(state_dir: &Path) -> Self {
        Self {
            dir: state_dir.to_path_buf(),
            lock_wait: STORE_LOCK_WAIT,
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join(STORE_FILE)
    }

    /// The lease file for `name`: `locks\<name>-<fnv64 of the key>.lock`. The readable part keeps
    /// only letters, digits, `-` and `_`, so no name can reach outside the folder or form a
    /// reserved device name; the hash of the full key keeps names apart that this folds together.
    fn lease_path(&self, name: &str) -> PathBuf {
        let key = key(name);
        let readable: String = key
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        self.dir
            .join(LEASE_DIR)
            .join(format!("{readable}-{:016x}.lock", fnv1a64(&key)))
    }

    /// Take `name`'s lease if no one holds it. `Ok(None)` when another process, or another call
    /// in this one, does.
    pub fn try_lease(&self, name: &str) -> Result<Option<Lease>, StoreError> {
        let path = self.lease_path(name);
        let dir = self.dir.join(LEASE_DIR);
        fs::create_dir_all(&dir).map_err(StoreError::io(&dir, "creating the lease folder"))?;
        let lock = FileLock::try_acquire(&path)
            .map_err(StoreError::io(&path, "locking the session lease"))?;
        Ok(lock.map(|lock| Lease { _lock: lock }))
    }

    /// The store, failing closed: a file that exists but cannot be parsed is STORE_CORRUPT. A
    /// missing file is an empty store.
    pub fn read(&self) -> Result<StoreFile, StoreError> {
        match self.read_bytes()? {
            None => Ok(StoreFile::empty()),
            Some(bytes) => self.parse(&bytes),
        }
    }

    /// The store for display. Records that parse are kept even when others, or the file as a
    /// whole, do not; `problem` says what is wrong.
    pub fn read_tolerant(&self) -> Tolerant {
        let bytes = match self.read_bytes() {
            Ok(None) => return Tolerant::whole(StoreFile::empty()),
            Ok(Some(bytes)) => bytes,
            Err(e) => {
                return Tolerant {
                    file: StoreFile::empty(),
                    problem: Some(e.to_string()),
                }
            }
        };
        let error = match self.parse(&bytes) {
            Ok(file) => return Tolerant::whole(file),
            Err(e) => e,
        };
        let mut file = StoreFile::empty();
        if let Ok(value) = serde_json::from_slice::<Value>(without_bom(&bytes)) {
            if let Some(sessions) = value.get("sessions").and_then(Value::as_object) {
                for (key, record) in sessions {
                    if let Ok(record) = Record::deserialize(record) {
                        file.sessions.insert(key.clone(), record);
                    }
                }
            }
            file.last_cleanup = value
                .get("last_cleanup")
                .and_then(|c| LastCleanup::deserialize(c).ok());
        }
        Tolerant {
            file,
            problem: Some(error.to_string()),
        }
    }

    /// Read, change and write the store under the store lock, so no other process's update is
    /// lost. Nothing is written when the read or `change` fails, and a store that cannot be
    /// parsed is never overwritten.
    pub fn update<R>(
        &self,
        change: impl FnOnce(&mut StoreFile) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let _lock = self.lock_store()?;
        let mut file = self.read()?;
        let result = change(&mut file)?;
        self.write(&file)?;
        Ok(result)
    }

    /// The raw store, `None` when there is none yet.
    fn read_bytes(&self) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.path();
        match output::retry_transient(|| fs::read(&path)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::io(&path, "reading")(e)),
        }
    }

    fn parse(&self, bytes: &[u8]) -> Result<StoreFile, StoreError> {
        let corrupt = |detail: String| StoreError::Corrupt {
            path: self.path(),
            detail,
        };
        let file: StoreFile =
            serde_json::from_slice(without_bom(bytes)).map_err(|e| corrupt(e.to_string()))?;
        if file.version != STORE_VERSION {
            return Err(corrupt(format!(
                "it is store version {}, and this build reads version {STORE_VERSION}; a newer \
                 codex-imagegen may have written it",
                file.version
            )));
        }
        Ok(file)
    }

    /// The store lock, waiting up to `lock_wait` for another holder.
    fn lock_store(&self) -> Result<FileLock, StoreError> {
        let path = self.dir.join(LOCK_FILE);
        fs::create_dir_all(&self.dir)
            .map_err(StoreError::io(&self.dir, "creating the state folder"))?;
        let give_up = Instant::now() + self.lock_wait;
        loop {
            match FileLock::try_acquire(&path) {
                Ok(Some(lock)) => return Ok(lock),
                Ok(None) if Instant::now() < give_up => std::thread::sleep(LOCK_POLL),
                Ok(None) => {
                    return Err(StoreError::Io {
                        path,
                        what: "locking",
                        error: io::Error::other(format!(
                            "another process has held the store lock for over {} s",
                            self.lock_wait.as_secs()
                        )),
                    })
                }
                Err(e) => return Err(StoreError::io(&path, "locking")(e)),
            }
        }
    }

    /// Replace the store atomically: a flushed temp file in the same folder, renamed over it.
    fn write(&self, file: &StoreFile) -> Result<(), StoreError> {
        let path = self.path();
        let json = serde_json::to_vec_pretty(file)
            .map_err(|e| StoreError::io(&path, "encoding")(io::Error::other(e)))?;
        let temp = self.dir.join(format!(
            ".sessions.{}-{}.tmp",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        output::write_new(&temp, &json).map_err(StoreError::io(&temp, "writing"))?;
        // std's rename replaces an existing file on Windows. Retried while a scanner or a reader
        // without delete sharing holds the live file.
        if let Err(e) = output::retry_transient(|| fs::rename(&temp, &path)) {
            // Ours: write_new created it.
            let _ = fs::remove_file(&temp);
            return Err(StoreError::io(&path, "replacing")(e));
        }
        Ok(())
    }
}

impl Tolerant {
    fn whole(file: StoreFile) -> Self {
        Self {
            file,
            problem: None,
        }
    }
}

/// A UTF-8 byte-order mark, which an editor may add to a file saved by hand, is not JSON.
fn without_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)
}

// ---------------------------------------------------------------------------
// Recording a call's images
// ---------------------------------------------------------------------------

/// What a new session's record starts with, besides its name and thread.
pub struct NewSession {
    /// The child's `codexHome`, from its handshake.
    pub codex_home: PathBuf,
    pub model: String,
    pub output_dir: PathBuf,
}

/// One completed image, as the record needs it.
pub struct ImageOutcome<'a> {
    /// The version published, `None` when the copy failed.
    pub version: Option<u32>,
    /// Our copy, when it was published.
    pub output_path: Option<&'a Path>,
    /// Codex's copy, when Codex reported one.
    pub saved_path: Option<&'a Path>,
    /// The image's size, when its bytes could be read.
    pub bytes: Option<u64>,
    /// The 64-bit FNV-1a of those bytes, which are exactly what was published.
    pub fnv1a64: Option<u64>,
}

/// Records one call's images in its session, each as it completes, in stream order
/// (docs/design.md, "When the record is written"). One writer per call, used on the call's own
/// thread while it holds the session's lease.
///
/// For a new session (generate) the record is created by the first image and updated by the rest;
/// a turn that completes no image never touches the store, so the name stays free. For an existing
/// session (refine) the record must be there, on the same thread.
pub struct SessionWriter {
    store: Store,
    name: String,
    new_session: Option<NewSession>,
    /// This call's turn has been counted in `turns`. Counted by the first image recorded, not
    /// the first that completed, so a turn whose first write failed is still counted once.
    counted: Cell<bool>,
}

impl SessionWriter {
    /// `new_session` is `Some` for generate, `None` for refine.
    pub fn new(store: Store, name: &str, new_session: Option<NewSession>) -> Self {
        Self {
            store,
            name: name.to_string(),
            new_session,
            counted: Cell::new(false),
        }
    }

    /// Record an image that completed on `thread_id`. Never overwrites another thread's record.
    pub fn image_completed(
        &self,
        thread_id: &str,
        image: &ImageOutcome<'_>,
    ) -> Result<(), StoreError> {
        let now = now_unix();
        let count = !self.counted.get();
        self.store.update(|file| {
            let record = match file.sessions.entry(key(&self.name)) {
                Entry::Occupied(entry) => {
                    let record = entry.into_mut();
                    if record.thread_id != thread_id {
                        return Err(StoreError::Conflict(format!(
                            "the name '{}' is recorded for another Codex thread ({})",
                            record.name, record.thread_id
                        )));
                    }
                    if count {
                        record.turns = record.turns.saturating_add(1);
                    }
                    record
                }
                Entry::Vacant(entry) => match &self.new_session {
                    Some(new) => entry.insert(Record {
                        name: self.name.clone(),
                        thread_id: thread_id.to_string(),
                        codex_home: new.codex_home.clone(),
                        model: new.model.clone(),
                        created: now,
                        updated: now,
                        turns: 1,
                        last_saved_path: None,
                        last_output_path: None,
                        last_output_bytes: None,
                        output_dir: new.output_dir.clone(),
                        next_version: 1,
                        outputs: Vec::new(),
                    }),
                    None => {
                        return Err(StoreError::Conflict(format!(
                            "the session '{}' is no longer in the store",
                            self.name
                        )))
                    }
                },
            };
            record.apply(image, now);
            Ok(())
        })?;
        self.counted.set(true);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    const THREAD: &str = "019a0000-0000-7000-8000-00000000cafe";

    fn new_session(dir: &Path) -> NewSession {
        NewSession {
            codex_home: PathBuf::from(r"C:\Users\someone\.codex"),
            model: "gpt-6-astra".to_string(),
            output_dir: dir.join("out"),
        }
    }

    fn published<'a>(version: u32, path: &'a Path, bytes: u64) -> ImageOutcome<'a> {
        ImageOutcome {
            version: Some(version),
            output_path: Some(path),
            saved_path: Some(Path::new(r"C:\codex\generated_images\t\exec-1.png")),
            bytes: Some(bytes),
            fnv1a64: Some(0xfeed_0000_0000_0000 | bytes),
        }
    }

    fn record(name: &str) -> Record {
        Record {
            name: name.to_string(),
            thread_id: THREAD.to_string(),
            codex_home: PathBuf::from(r"C:\Users\someone\.codex"),
            model: "gpt-6-astra".to_string(),
            created: 1,
            updated: 1,
            turns: 1,
            last_saved_path: None,
            last_output_path: None,
            last_output_bytes: None,
            output_dir: PathBuf::from(r"C:\out"),
            next_version: 2,
            outputs: Vec::new(),
        }
    }

    fn insert(store: &Store, name: &str) {
        store
            .update(|file| {
                file.sessions.insert(key(name), record(name));
                Ok(())
            })
            .unwrap();
    }

    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_missing_store_reads_as_empty_and_the_first_write_creates_it() {
        let dir = temp_dir("session");
        let store = Store::new(&dir.join("state"));
        assert_eq!(store.read().unwrap(), StoreFile::empty());
        assert!(store.read_tolerant().problem.is_none());
        insert(&store, "Fox");
        let file = store.read().unwrap();
        assert_eq!(file.get("FOX").unwrap().name, "Fox");
        assert_eq!(file.sessions.keys().collect::<Vec<_>>(), vec!["fox"]);
        // Only the store and its lock: no temp file is left behind.
        assert_eq!(
            listing(&dir.join("state")),
            vec!["sessions.json", "sessions.lock"]
        );
    }

    #[test]
    fn the_record_is_written_with_the_designs_field_names() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        insert(&store, "fox");
        let value: Value = serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(value["version"], 1);
        let mut fields: Vec<&str> = value["sessions"]["fox"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort();
        let mut expected = vec![
            "name",
            "thread_id",
            "codex_home",
            "model",
            "created",
            "updated",
            "turns",
            "last_saved_path",
            "last_output_path",
            "last_output_bytes",
            "output_dir",
            "next_version",
            "outputs",
        ];
        expected.sort();
        assert_eq!(fields, expected);
        assert!(value.get("last_cleanup").is_none());
    }

    #[test]
    fn an_output_recorded_before_fingerprints_still_reads_and_is_written_back_without_one() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        let mut old = serde_json::to_value(record("fox")).unwrap();
        old["outputs"] = serde_json::json!([
            {"version": 1, "path": r"C:\out\fox-v1.png", "bytes": 5},
            {"version": 2, "path": r"C:\out\fox-v2.png", "bytes": 6, "fnv1a64": "not hex!"},
            {"version": 3, "path": r"C:\out\fox-v3.png", "bytes": 7, "fnv1a64": "00000000000000ff"},
        ]);
        let text = serde_json::json!({"version": 1, "sessions": {"fox": old}}).to_string();
        fs::write(store.path(), text).unwrap();
        let file = store.read().expect("an older record is still readable");
        let outputs = &file.get("fox").unwrap().outputs;
        assert_eq!(outputs[0].fnv1a64, None);
        // Neither a missing nor a malformed fingerprint proves anything.
        assert_eq!(outputs[0].fingerprint(), None);
        assert_eq!(outputs[1].fingerprint(), None);
        assert_eq!(outputs[2].fingerprint(), Some(0xff));
        store.update(|_| Ok(())).unwrap();
        let value: Value = serde_json::from_slice(&fs::read(store.path()).unwrap()).unwrap();
        let written = &value["sessions"]["fox"]["outputs"];
        assert!(written[0].get("fnv1a64").is_none(), "{written}");
        assert_eq!(written[2]["fnv1a64"], "00000000000000ff");
        assert_eq!(Output::fingerprint_text(0xab), "00000000000000ab");
    }

    #[test]
    fn concurrent_writers_on_separate_handles_lose_no_update_and_readers_never_see_a_partial_file()
    {
        let dir = temp_dir("session");
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            let reader = s.spawn(|| {
                let store = Store::new(&dir);
                let mut reads = 0;
                while !done.load(Ordering::SeqCst) {
                    if let Err(e) = store.read() {
                        panic!("a read saw {e}");
                    }
                    reads += 1;
                }
                reads
            });
            let writers: Vec<_> = (0..6)
                .map(|w| {
                    let dir = &dir;
                    s.spawn(move || {
                        // Each its own Store, so each update opens its own lock handle.
                        let store = Store::new(dir);
                        for i in 0..5 {
                            insert(&store, &format!("w{w}-{i}"));
                        }
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            done.store(true, Ordering::SeqCst);
            assert!(reader.join().unwrap() > 0);
        });
        let file = Store::new(&dir).read().unwrap();
        assert_eq!(file.sessions.len(), 30);
        assert_eq!(listing(&dir), vec!["sessions.json", "sessions.lock"]);
    }

    #[test]
    fn a_store_that_cannot_be_parsed_fails_closed_and_is_never_overwritten() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        fs::write(store.path(), b"{\"version\": 1, \"sessions\": {").unwrap();
        let err = store.read().unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { .. }), "{err}");
        assert_eq!(err.failure().code, "STORE_CORRUPT");
        assert!(err.failure().remediation.contains("Move it aside"));
        let err = store.update(|_| Ok(())).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt { .. }), "{err}");
        assert_eq!(
            fs::read(store.path()).unwrap(),
            b"{\"version\": 1, \"sessions\": {"
        );
        let tolerant = store.read_tolerant();
        assert!(tolerant.file.sessions.is_empty());
        assert!(
            tolerant
                .problem
                .as_deref()
                .unwrap()
                .contains("cannot be read"),
            "{:?}",
            tolerant.problem
        );
        // Empty is not a store either.
        fs::write(store.path(), b"").unwrap();
        assert!(matches!(store.read(), Err(StoreError::Corrupt { .. })));
    }

    #[test]
    fn the_tolerant_read_keeps_the_records_that_parse() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        let good = serde_json::to_value(record("fox")).unwrap();
        let text = serde_json::json!({"version": 1, "sessions": {
            "fox": good, "bad": {"name": "bad", "turns": "many"}}})
        .to_string();
        fs::write(store.path(), text).unwrap();
        assert!(matches!(store.read(), Err(StoreError::Corrupt { .. })));
        let tolerant = store.read_tolerant();
        assert!(tolerant.problem.is_some());
        assert_eq!(
            tolerant.file.sessions.keys().collect::<Vec<_>>(),
            vec!["fox"]
        );
    }

    #[test]
    fn a_store_of_another_version_is_refused_and_a_byte_order_mark_is_not() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        fs::write(store.path(), br#"{"version": 2, "sessions": {}}"#).unwrap();
        let err = store.read().unwrap_err();
        assert!(err.to_string().contains("newer codex-imagegen"), "{err}");
        fs::write(
            store.path(),
            b"\xef\xbb\xbf{\"version\": 1, \"sessions\": {}}",
        )
        .unwrap();
        assert_eq!(store.read().unwrap(), StoreFile::empty());
    }

    #[test]
    fn leases_exclude_each_other_across_handles_whatever_the_case() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        let held = store.try_lease("Fox").unwrap().expect("a free lease");
        assert!(store.try_lease("fox").unwrap().is_none());
        // Another Store opens another handle, as another process does.
        assert!(Store::new(&dir).try_lease("FOX").unwrap().is_none());
        let other = store.try_lease("fox2").unwrap();
        assert!(other.is_some(), "a different name is not held");
        drop(held);
        assert!(Store::new(&dir).try_lease("fOx").unwrap().is_some());
        // Lease files stay behind, and are never deleted.
        assert_eq!(listing(&dir.join("locks")).len(), 2);
    }

    #[test]
    fn lease_files_are_named_safely() {
        let store = Store::new(Path::new(r"C:\state"));
        let name = |n: &str| {
            store
                .lease_path(n)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string()
        };
        assert_eq!(
            name("Fox.v2"),
            format!("fox_v2-{:016x}.lock", fnv1a64("fox.v2"))
        );
        assert_eq!(name("FOX.V2"), name("fox.v2"));
        assert_ne!(name("fox.v2"), name("fox_v2"), "the hash keeps them apart");
        assert!(name("con.x").starts_with("con_x-"), "{}", name("con.x"));
        assert!(name(r"..\escape").starts_with("___escape-"));
        assert_eq!(
            store.lease_path("x").parent().unwrap(),
            Path::new(r"C:\state\locks")
        );
    }

    #[test]
    fn a_store_lock_held_too_long_is_an_error_and_its_release_frees_the_store() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        let held = store.lock_store().unwrap();
        let impatient = Store {
            lock_wait: Duration::from_millis(100),
            ..Store::new(&dir)
        };
        let err = impatient.update(|_| Ok(())).unwrap_err();
        assert!(err.to_string().contains("held the store lock"), "{err}");
        assert_eq!(err.failure().code, "STORE_CORRUPT");
        drop(held);
        impatient.update(|_| Ok(())).unwrap();
    }

    #[test]
    fn a_new_session_is_created_by_its_first_image_and_updated_by_the_next() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        let writer = SessionWriter::new(store.clone(), "Fox", Some(new_session(&dir)));
        let v1 = dir.join("out").join("Fox-v1.png");
        writer
            .image_completed(THREAD, &published(1, &v1, 100))
            .unwrap();
        let first = store.read().unwrap().get("fox").cloned().unwrap();
        assert_eq!(first.name, "Fox");
        assert_eq!(first.thread_id, THREAD);
        assert_eq!(first.codex_home, PathBuf::from(r"C:\Users\someone\.codex"));
        assert_eq!(first.model, "gpt-6-astra");
        assert_eq!(first.output_dir, dir.join("out"));
        assert_eq!(first.turns, 1);
        assert_eq!(first.created, first.updated);
        assert_eq!(first.last_output_path.as_deref(), Some(v1.as_path()));
        assert_eq!(first.last_output_bytes, Some(100));
        assert_eq!(first.next_version, 2);
        assert_eq!(
            first.outputs,
            vec![Output {
                version: 1,
                path: v1.clone(),
                bytes: 100,
                fnv1a64: Some("feed000000000064".to_string()),
            }]
        );
        assert_eq!(first.outputs[0].fingerprint(), Some(0xfeed_0000_0000_0064));

        // A second image in the same turn: the same turn, a new latest image.
        let v3 = dir.join("out").join("Fox-v3.png");
        writer
            .image_completed(THREAD, &published(3, &v3, 300))
            .unwrap();
        let second = store.read().unwrap().get("fox").cloned().unwrap();
        assert_eq!(second.turns, 1);
        assert_eq!(second.last_output_path.as_deref(), Some(v3.as_path()));
        assert_eq!(second.next_version, 4);
        assert_eq!(second.outputs.len(), 2);

        // A later call on the existing session counts a turn, once however many images it makes.
        let later = SessionWriter::new(store.clone(), "FOX", None);
        let v4 = dir.join("out").join("Fox-v4.png");
        later
            .image_completed(THREAD, &published(4, &v4, 4))
            .unwrap();
        later
            .image_completed(THREAD, &published(5, &v4, 5))
            .unwrap();
        let third = store.read().unwrap().get("fox").cloned().unwrap();
        assert_eq!(third.turns, 2);
        assert_eq!(third.next_version, 6);
        assert_eq!(third.name, "Fox", "the first spelling is kept");
    }

    #[test]
    fn a_copy_that_failed_is_recorded_as_the_latest_image_without_an_output() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        let writer = SessionWriter::new(store.clone(), "fox", Some(new_session(&dir)));
        let v1 = dir.join("out").join("fox-v1.png");
        writer
            .image_completed(THREAD, &published(1, &v1, 10))
            .unwrap();
        let saved = Path::new(r"C:\codex\generated_images\t\exec-2.png");
        writer
            .image_completed(
                THREAD,
                &ImageOutcome {
                    version: None,
                    output_path: None,
                    saved_path: Some(saved),
                    bytes: Some(20),
                    fnv1a64: Some(20),
                },
            )
            .unwrap();
        let record = store.read().unwrap().get("fox").cloned().unwrap();
        // v1 is not the latest image any more, so it must not be offered as the edit target.
        assert_eq!(record.last_output_path, None);
        assert_eq!(record.last_saved_path.as_deref(), Some(saved));
        assert_eq!(record.last_output_bytes, Some(20));
        assert_eq!(record.latest_path(), Some(saved));
        assert_eq!(record.next_version, 2);
        assert_eq!(record.outputs.len(), 1);
    }

    #[test]
    fn a_writer_never_takes_over_another_threads_record_or_revives_a_removed_one() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        insert(&store, "fox");
        let before = store.read().unwrap();
        let v1 = dir.join("fox-v1.png");
        let intruder = SessionWriter::new(store.clone(), "FOX", Some(new_session(&dir)));
        let err = intruder
            .image_completed("another-thread", &published(1, &v1, 1))
            .unwrap_err();
        assert!(err.to_string().contains("another Codex thread"), "{err}");
        assert_eq!(store.read().unwrap(), before, "the store changed");

        let refine = SessionWriter::new(store.clone(), "gone", None);
        let err = refine
            .image_completed(THREAD, &published(1, &v1, 1))
            .unwrap_err();
        assert!(err.to_string().contains("no longer in the store"), "{err}");
        assert!(store.read().unwrap().get("gone").is_none());
    }

    #[test]
    fn a_failed_first_write_leaves_the_turn_to_be_counted_by_the_next_image() {
        let dir = temp_dir("session");
        let store = Store::new(&dir);
        insert(&store, "fox");
        let later = SessionWriter::new(store.clone(), "fox", None);
        let v2 = dir.join("fox-v2.png");
        // The store cannot be written for the first image: a folder stands where it goes.
        let saved = fs::read(store.path()).unwrap();
        fs::remove_file(store.path()).unwrap();
        fs::create_dir(store.path()).unwrap();
        assert!(later
            .image_completed(THREAD, &published(2, &v2, 2))
            .is_err());
        fs::remove_dir(store.path()).unwrap();
        fs::write(store.path(), saved).unwrap();
        later
            .image_completed(THREAD, &published(3, &v2, 3))
            .unwrap();
        assert_eq!(store.read().unwrap().get("fox").unwrap().turns, 2);
    }
}
