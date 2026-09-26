//! Session expiry and the `--cleanup` sweep (docs/design.md, "Cleanup").
//!
//! Each image leaves three things behind: our copy in the output folder, Codex's copy under
//! `<CODEX_HOME>\generated_images\<threadId>\`, and the thread's rollout, which only
//! `thread/delete` removes. Nothing can shrink a live thread, so a session is removed whole: its
//! thread, Codex's image folder for it, the files recorded in its `outputs`, and last its record,
//! so a cleanup cut short is simply retried next time.
//!
//! Deleting is one of the places rigor belongs (AGENTS.md). The safety rules:
//!
//! - Only a thread named in a session record is deleted, through `thread/delete` on a child of the
//!   Codex home the record names. A record from another home, or whose thread id is not a
//!   well-formed UUID, is skipped whole: nothing of it is touched.
//! - In Codex's image folder only `*.png` files are removed, then the folder once it is empty.
//!   Neither `generated_images` nor the thread's folder may be a link (a reparse point), and the
//!   folder's canonical path must be the canonical Codex home's `generated_images\<threadId>`;
//!   otherwise nothing in it is touched. A folder that holds something cleanup kept on purpose
//!   stays, and the session still goes; one that should now be empty but cannot be removed is a
//!   problem, so the record is kept and its removal retried.
//! - An output is deleted only while its exact path is a plain file under the name it was
//!   published with, with the recorded size and content fingerprint, and while the handle that
//!   deletes it shows it at the final path recorded when it was published. A file that was
//!   edited, replaced (even by one of the same size), renamed, or is now reached through a link
//!   somewhere else is left alone, and so is one whose record has no fingerprint or no resolved
//!   path. No folder in an output directory is ever removed, and nothing is matched by wildcard
//!   there.
//! - Every file is deleted through the handle its checks were made on ([`crate::delete`]), which
//!   admits no other writer or deleter and never follows a link at the file's name.
//! - A read-only file or folder is kept, as the user's protection: logged, not retried, and not a
//!   reason to keep the session.
//! - The session's lease is held throughout, taken without waiting: a session in use is skipped,
//!   and the record is read again under the lease, so one a call has just refreshed is left be.
//!
//! Two entry points: [`expire`], run once per server process on its own child for this project's
//! store, and [`sweep`], `codex-imagegen.exe --cleanup`, across every project store under the
//! state base with one child per Codex home.

use std::fs::{self, Metadata};
use std::io::{self, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::appserver::{AppServer, RpcError};
use crate::codex::{self, Handshake, Rpc};
use crate::config::Config;
use crate::delete;
use crate::errors::Failure;
use crate::output;
use crate::session::{self, LastCleanup, Record, Skipped, Store, STORE_FILE};
use crate::tools::Launcher;

/// Deletes one thread through a Codex child: `thread/delete {threadId}`.
pub type DeleteThread<'a> = &'a dyn Fn(&str) -> Result<(), RpcError>;

/// Whether a session is in use by a call in this process (automatic expiry), which its lease does
/// not show once that call has returned and left its turn lingering.
pub type BusyHere<'a> = &'a dyn Fn(&str) -> bool;

/// How long the sweep's children get to exit once their input ends.
const CHILD_GRACE: Duration = Duration::from_secs(5);

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

const SECS_PER_DAY: i64 = 86_400;

/// What became of one session.
#[derive(Debug, PartialEq, Eq)]
enum Fate {
    Removed {
        freed: u64,
    },
    Skipped(String),
    /// Not due any more when read under its lease: a call refreshed it, or it is gone.
    NotDue,
}

/// One store's cleanup: what its `last_cleanup` records.
#[derive(Debug, Default)]
struct Tally {
    removed: Vec<String>,
    freed: u64,
    skipped: Vec<Skipped>,
}

impl Tally {
    fn add(&mut self, name: &str, fate: Fate) {
        match fate {
            Fate::Removed { freed } => {
                self.removed.push(name.to_string());
                self.freed += freed;
            }
            Fate::Skipped(why) => self.skipped.push(Skipped {
                name: name.to_string(),
                why,
            }),
            Fate::NotDue => {}
        }
    }

    fn last_cleanup(&self, at: i64) -> LastCleanup {
        LastCleanup {
            at,
            removed: self.removed.len() as u32,
            freed_bytes: self.freed,
            skipped: self.skipped.clone(),
        }
    }

    /// `removed 2 sessions (fox, owl), freed 5.1 MB; skipped 1 (busy: in use ...)`.
    fn summary(&self) -> String {
        let mut text = format!(
            "removed {}{}, freed {}",
            sessions(self.removed.len()),
            if self.removed.is_empty() {
                String::new()
            } else {
                format!(" ({})", self.removed.join(", "))
            },
            megabytes(self.freed)
        );
        if !self.skipped.is_empty() {
            let reasons: Vec<String> = self
                .skipped
                .iter()
                .map(|s| format!("{}: {}", s.name, s.why))
                .collect();
            text.push_str(&format!(
                "; skipped {} ({})",
                self.skipped.len(),
                reasons.join("; ")
            ));
        }
        text
    }
}

fn sessions(n: usize) -> String {
    match n {
        1 => "1 session".to_string(),
        n => format!("{n} sessions"),
    }
}

/// `2.6 MB`, or `12 KB` below a megabyte.
fn megabytes(bytes: u64) -> String {
    const MB: u64 = 1024 * 1024;
    if bytes >= MB || bytes == 0 {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// Whether `record` is due: idle more than `days` days, or any age when `days` is 0 (the sweep's
/// "every session not in use"). `now` is Unix seconds.
fn is_due(record: &Record, now: i64, days: u32) -> bool {
    days == 0 || now.saturating_sub(record.updated) > i64::from(days) * SECS_PER_DAY
}

/// A well-formed UUID: 8-4-4-4-12 hex digits. Codex names threads so, and the thread id becomes a
/// folder name under the Codex home, so anything else is never used to delete there.
pub fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Whether two paths name the same folder as Windows compares them: case-insensitively, with or
/// without a trailing separator, and with or without the `\\?\` prefix Codex's canonicalised
/// `CODEX_HOME` carries.
pub fn same_path(a: &Path, b: &Path) -> bool {
    fn plain(path: &Path) -> String {
        let text = path.to_string_lossy().replace('/', "\\");
        let text = match text.strip_prefix(r"\\?\UNC\") {
            Some(rest) => format!(r"\\{rest}"),
            None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
        };
        text.trim_end_matches('\\').to_lowercase()
    }
    plain(a) == plain(b)
}

/// Whether two final paths, both read with `GetFinalPathNameByHandleW`, are the same file's path:
/// exactly, apart from the `\\?\` prefix. Both carry the on-disk case, so a real match is
/// byte-identical, and a case-insensitive compare would let a junction redirect a delete to a
/// sibling whose name differs only by case (a case-sensitive folder) or by a Unicode case fold NTFS
/// does not apply, such as U+212A KELVIN SIGN for `k` [verified: scratch test, 2026-09-26]. The cost:
/// a folder renamed by case alone keeps its file, which fails closed.
fn same_final_path(a: &Path, b: &Path) -> bool {
    fn plain(path: &Path) -> String {
        let text = path.to_string_lossy().into_owned();
        match text.strip_prefix(r"\\?\UNC\") {
            Some(rest) => format!(r"\\{rest}"),
            None => text.strip_prefix(r"\\?\").unwrap_or(&text).to_string(),
        }
    }
    plain(a) == plain(b)
}

fn is_plain(meta: &Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
}

/// Remove one session, following the safety rules above. `codex_home` is the home of the child
/// `delete` goes to. `now` and `days` say which sessions are due.
fn remove_session(
    store: &Store,
    name: &str,
    codex_home: &Path,
    now: i64,
    days: u32,
    delete: DeleteThread<'_>,
    busy_here: BusyHere<'_>,
) -> Fate {
    if busy_here(name) {
        return Fate::Skipped("in use by a call in this server".to_string());
    }
    let _lease = match store.try_lease(name) {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            return Fate::Skipped(
                "in use (another codex-imagegen process holds its lease)".to_string(),
            )
        }
        Err(e) => return Fate::Skipped(e.to_string()),
    };
    // Read again under the lease: a call that finished since the listing may have refreshed it.
    let record = match store.read() {
        Ok(file) => match file.get(name) {
            Some(record) if is_due(record, now, days) => record.clone(),
            _ => return Fate::NotDue,
        },
        Err(e) => return Fate::Skipped(e.to_string()),
    };
    if !is_uuid(&record.thread_id) {
        return Fate::Skipped(
            "its Codex thread id is not a well-formed UUID, so nothing of it was deleted"
                .to_string(),
        );
    }
    if !same_path(&record.codex_home, codex_home) {
        return Fate::Skipped(format!(
            "its Codex thread lives in another Codex home ({}), so nothing of it was deleted; \
             codex-imagegen --cleanup covers every home",
            record.codex_home.display()
        ));
    }
    if let Err(why) = thread_gone(delete(&record.thread_id)) {
        return Fate::Skipped(why);
    }
    let mut freed = 0;
    let mut problems = Vec::new();
    remove_codex_images(codex_home, &record.thread_id, &mut freed, &mut problems);
    remove_outputs(&record, &mut freed, &mut problems);
    if !problems.is_empty() {
        return Fate::Skipped(format!(
            "{}; its record is kept, so the rest is retried next time",
            problems.join("; ")
        ));
    }
    // Last, so a cleanup cut short before this finds the session again next time.
    let key = session::key(name);
    let dropped = store.update(|file| {
        if file
            .sessions
            .get(&key)
            .is_some_and(|r| r.thread_id == record.thread_id)
        {
            file.sessions.remove(&key);
        }
        Ok(())
    });
    match dropped {
        Ok(()) => Fate::Removed { freed },
        Err(e) => Fate::Skipped(format!(
            "its thread and files were removed, but its record could not be dropped ({e}); \
             retried next time"
        )),
    }
}

/// Whether `thread/delete`'s outcome leaves the thread gone. "no rollout found" (and "thread not
/// found") mean Codex had nothing left to delete, which counts as gone [verified: live, V8, for
/// "no rollout found" after a delete]. "active writer" or anything else leaves it for next time.
fn thread_gone(result: Result<(), RpcError>) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(RpcError::Remote { message, .. })
            if message.contains("no rollout found") || message.contains("thread not found") =>
        {
            eprintln!(
                "codex-imagegen: cleanup: the thread was already gone ({})",
                crate::jsonrpc::clamp(&message, 200)
            );
            Ok(())
        }
        Err(RpcError::Remote { message, .. })
            if message.contains("already has an active writer") =>
        {
            Err(format!(
                "its Codex thread is open in another process ({})",
                crate::jsonrpc::clamp(&message, 200)
            ))
        }
        Err(e) => Err(format!(
            "thread/delete failed ({})",
            crate::jsonrpc::clamp(&e.to_string(), 300)
        )),
    }
}

/// What became of one thing cleanup meant to delete.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Deleted, freeing this many bytes.
    Deleted(u64),
    /// Nothing was there.
    Gone,
    /// Left alone on purpose: it cannot be shown to be ours. Logged; the session still goes.
    Kept(String),
    /// Left alone because it changed under cleanup in a way nothing should: counted as a problem,
    /// so the record is kept and the next cleanup looks again.
    Refused(String),
}

/// Add up one deletion's outcome.
fn settle(path: &Path, outcome: io::Result<Verdict>, freed: &mut u64, problems: &mut Vec<String>) {
    match outcome {
        Ok(Verdict::Deleted(bytes)) => *freed += bytes,
        Ok(Verdict::Gone) => {}
        Ok(Verdict::Kept(why)) => {
            eprintln!("codex-imagegen: cleanup: kept {}: {why}", path.display());
        }
        Ok(Verdict::Refused(why)) => problems.push(format!("left {}: {why}", path.display())),
        Err(e) => problems.push(format!("could not delete {} ({e})", path.display())),
    }
}

/// Whether Codex's image folder for a thread may be cleaned out.
#[derive(Debug, PartialEq, Eq)]
enum FolderCheck {
    /// There is no folder.
    Absent,
    /// It is where it should be; this is its canonical path.
    Ours(PathBuf),
    /// It, or `generated_images`, is a link or resolves elsewhere: nothing in it is touched, the
    /// reason is logged, and the rest of the session's cleanup goes on.
    LeftAlone(String),
    /// It could not be checked: a problem, so the record is kept and it is retried.
    Failed(String),
}

/// Check that `<codex_home>\generated_images\<thread_id>` is Codex's image folder for the thread
/// and not somewhere else reached through a link: neither `generated_images` nor the folder is a
/// reparse point, and the folder's canonical path is the canonical Codex home's
/// `generated_images\<thread_id>`. A Codex home that is itself reached through a link is fine: it
/// is the home the child reported, and both canonical paths resolve through it alike.
///
/// A process that can already write the user's Codex home could swap a link in after this check;
/// defending against that is out of scope, since such a process can delete those files itself.
/// Each deletion is checked again through its own handle ([`delete_codex_png`]), which catches a
/// link that appears after this.
fn check_codex_folder(codex_home: &Path, thread_id: &str) -> FolderCheck {
    let images = codex_home.join("generated_images");
    let dir = images.join(thread_id);
    for path in [&images, &dir] {
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() && is_plain(&meta) => {}
            Ok(_) => {
                return FolderCheck::LeftAlone(format!(
                    "{} is a link (a reparse point) or not a folder",
                    path.display()
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return FolderCheck::Absent,
            Err(e) => {
                return FolderCheck::Failed(format!("could not read {} ({e})", path.display()))
            }
        }
    }
    let home = match fs::canonicalize(codex_home) {
        Ok(home) => home,
        Err(e) => {
            return FolderCheck::Failed(format!(
                "could not resolve the Codex home {} ({e})",
                codex_home.display()
            ))
        }
    };
    let expected = home.join("generated_images").join(thread_id);
    match fs::canonicalize(&dir) {
        Ok(actual) if same_path(&actual, &expected) => FolderCheck::Ours(expected),
        Ok(actual) => FolderCheck::LeftAlone(format!(
            "it resolves to {}, not {}",
            actual.display(),
            expected.display()
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => FolderCheck::Absent,
        Err(e) => FolderCheck::Failed(format!("could not resolve {} ({e})", dir.display())),
    }
}

/// Delete Codex's copies of the thread's images: the `*.png` files in
/// `<codex_home>\generated_images\<thread_id>\`, then the folder once it is empty, after
/// [`check_codex_folder`] has shown the folder to be the one it should be. `thread/delete` leaves
/// it behind [verified: live, V8].
///
/// The folder stays, logged, when cleanup kept something in it on purpose: anything not a PNG, a
/// PNG that is a link or not a plain file, or a read-only one. Then it cannot be emptied, and that
/// is no reason to keep the session. Otherwise it should be empty, and a failure to remove it is a
/// problem, so the record is kept and the next cleanup tries again: another process may hold it
/// open without delete sharing, or, where POSIX deletes are unsupported, a PNG a viewer holds may
/// still be delete-pending. Were it only logged, the record would go and nothing would ever retry
/// the folder.
fn remove_codex_images(
    codex_home: &Path,
    thread_id: &str,
    freed: &mut u64,
    problems: &mut Vec<String>,
) {
    let dir = codex_home.join("generated_images").join(thread_id);
    let folder = match check_codex_folder(codex_home, thread_id) {
        FolderCheck::Absent => return,
        FolderCheck::Ours(folder) => folder,
        FolderCheck::LeftAlone(why) => {
            eprintln!(
                "codex-imagegen: cleanup: left Codex's image folder {} alone, and nothing in it \
                 was deleted: {why}",
                dir.display()
            );
            return;
        }
        FolderCheck::Failed(why) => {
            problems.push(why);
            return;
        }
    };
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => {
            problems.push(format!("could not list {} ({e})", dir.display()));
            return;
        }
    };
    let before = problems.len();
    // Whether cleanup left anything in the folder on purpose.
    let mut kept = false;
    for entry in entries {
        let path = match entry {
            Ok(entry) => entry.path(),
            Err(e) => {
                problems.push(format!("could not list {} ({e})", dir.display()));
                continue;
            }
        };
        let is_png = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"));
        if !is_png {
            kept = true;
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_file() && is_plain(&meta) => {}
            Ok(_) => {
                kept = true;
                continue;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                problems.push(format!("could not read {} ({e})", path.display()));
                continue;
            }
        }
        // Retried as a whole, so each attempt opens and checks the file afresh.
        let outcome = output::retry_transient(|| delete_codex_png(&path, &folder));
        kept |= matches!(outcome, Ok(Verdict::Kept(_)));
        settle(&path, outcome, freed, problems);
    }
    if problems.len() > before {
        // The record is kept, and the folder is looked at again next time.
        return;
    }
    if kept {
        eprintln!(
            "codex-imagegen: cleanup: left Codex's image folder {}: it holds what cleanup kept",
            dir.display()
        );
        return;
    }
    let outcome = output::retry_transient(|| remove_codex_folder(&dir, &folder));
    settle(&dir, outcome, freed, problems);
}

/// Delete one of Codex's PNGs through a handle that never follows a link at its name, once that
/// handle shows a plain file directly in `folder`, the canonical path of the thread's image
/// folder, that is not read-only. Checking the location through the handle means a link swapped
/// into the path after the folder was checked cannot redirect the deletion.
fn delete_codex_png(path: &Path, folder: &Path) -> io::Result<Verdict> {
    let Some(file) = delete::open(path)? else {
        return Ok(Verdict::Gone);
    };
    if !file.is_plain_file() {
        return Ok(Verdict::Kept("it is not a plain file".to_string()));
    }
    let at = file.final_path()?;
    if !at.parent().is_some_and(|parent| same_path(parent, folder)) {
        return Ok(Verdict::Refused(format!(
            "it resolves to {}, outside {}",
            at.display(),
            folder.display()
        )));
    }
    if file.is_read_only() {
        return Ok(read_only());
    }
    let size = file.size();
    file.delete()?;
    Ok(Verdict::Deleted(size))
}

/// Remove the thread's image folder, empty by now, through a handle, once that handle shows the
/// plain folder at `folder`, not read-only.
fn remove_codex_folder(dir: &Path, folder: &Path) -> io::Result<Verdict> {
    let Some(opened) = delete::open(dir)? else {
        return Ok(Verdict::Gone);
    };
    if !opened.is_plain_folder() {
        return Ok(Verdict::Kept("it is no longer a plain folder".to_string()));
    }
    let at = opened.final_path()?;
    if !same_path(&at, folder) {
        return Ok(Verdict::Refused(format!("it resolves to {}", at.display())));
    }
    if opened.is_read_only() {
        return Ok(read_only());
    }
    opened.delete()?;
    Ok(Verdict::Deleted(0))
}

/// Delete the files this server published for the session, each only while its exact path is a
/// plain file under its published name, with the recorded size and content fingerprint, that the
/// deleting handle shows where it was published.
fn remove_outputs(record: &Record, freed: &mut u64, problems: &mut Vec<String>) {
    for published in &record.outputs {
        let path = &published.path;
        let expected = output::file_name(&record.name, published.version);
        let named = path.is_absolute()
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case(&expected));
        if !named {
            eprintln!(
                "codex-imagegen: cleanup: kept {}: it is not named {expected}, as published",
                path.display()
            );
            continue;
        }
        // A first look by path, to say plainly why a file is kept. What decides is checked again
        // through the handle that deletes.
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => {
                problems.push(format!("could not read {} ({e})", path.display()));
                continue;
            }
            Ok(meta) if meta.is_file() && is_plain(&meta) && meta.len() == published.bytes => {}
            Ok(_) => {
                eprintln!(
                    "codex-imagegen: cleanup: kept {}: it changed after it was published",
                    path.display()
                );
                continue;
            }
        }
        let Some(fingerprint) = published.fingerprint() else {
            eprintln!(
                "codex-imagegen: cleanup: kept {}: its record holds no content fingerprint (an \
                 earlier build wrote it), so it cannot be shown to be the file published",
                path.display()
            );
            continue;
        };
        let Some(resolved) = published.resolved_path.as_deref() else {
            eprintln!(
                "codex-imagegen: cleanup: kept {}: its record holds no resolved path (an earlier \
                 build wrote it, or the path could not be read when it was published), so it \
                 cannot be shown to be where the file was published",
                path.display()
            );
            continue;
        };
        // Retried as a whole, so each attempt opens and checks the file afresh: one replaced
        // between attempts is judged as it is then.
        let outcome =
            output::retry_transient(|| delete_output(path, published.bytes, fingerprint, resolved));
        settle(path, outcome, freed, problems);
    }
}

/// Delete a published output through one handle, only while that handle shows a plain file at
/// `resolved`, the final path recorded when it was published, with the recorded size and content
/// fingerprint, that is not read-only. Location, size, fingerprint and attributes are read through
/// the handle, which admits no other writer or deleter, and the deletion is marked on it, so the
/// file checked is the file deleted.
///
/// The handle never follows a link at the file's own name, but folders on the way are resolved: a
/// published folder since replaced by a junction to an archive holding an identical copy reaches
/// that copy, with the same size and fingerprint. Its final path gives it away, and it is kept.
fn delete_output(
    path: &Path,
    bytes: u64,
    fingerprint: u64,
    resolved: &Path,
) -> io::Result<Verdict> {
    let Some(file) = delete::open(path)? else {
        return Ok(Verdict::Gone);
    };
    if !file.is_plain_file() || file.size() != bytes {
        return Ok(Verdict::Kept(
            "it changed after it was published".to_string(),
        ));
    }
    let at = file.final_path()?;
    if !same_final_path(&at, resolved) {
        return Ok(Verdict::Kept(format!(
            "it is no longer where it was published: it resolves to {}, not {}",
            at.display(),
            resolved.display()
        )));
    }
    let (content, read) = file.fingerprint()?;
    if read != bytes || content != fingerprint {
        return Ok(Verdict::Kept(
            "its content changed after it was published".to_string(),
        ));
    }
    if file.is_read_only() {
        return Ok(read_only());
    }
    file.delete()?;
    Ok(Verdict::Deleted(bytes))
}

/// A read-only file or folder is kept: the attribute is the user's protection, and cleanup honours
/// it rather than overriding it. Kept, not a failure, so the deletion is not retried and the rest of
/// the session still goes; were it attempted, it would fail with ERROR_ACCESS_DENIED, which
/// [`output::retry_transient`] takes for a busy file.
fn read_only() -> Verdict {
    Verdict::Kept("it is read-only".to_string())
}

/// Remove `names` from `store` one by one while time remains, then record the outcome as the
/// store's `last_cleanup`.
#[allow(clippy::too_many_arguments)]
fn clean_store(
    store: &Store,
    names: &[String],
    codex_home: &Path,
    now: i64,
    days: u32,
    delete: DeleteThread<'_>,
    busy_here: BusyHere<'_>,
    deadline: Option<Instant>,
    mut report: impl FnMut(&str, &Fate),
) -> Tally {
    let mut tally = Tally::default();
    for name in names {
        let fate = if deadline.is_some_and(|d| Instant::now() >= d) {
            Fate::Skipped("cleanup ran out of time; retried next time".to_string())
        } else {
            remove_session(store, name, codex_home, now, days, delete, busy_here)
        };
        report(name, &fate);
        tally.add(name, fate);
    }
    tally
}

/// Record `tally` as the store's last cleanup, for `status`. Leaves the sessions alone, and never
/// writes a store it cannot parse.
fn record_outcome(store: &Store, tally: &Tally, at: i64) -> Result<(), String> {
    let outcome = tally.last_cleanup(at);
    store
        .update(|file| {
            file.last_cleanup = Some(outcome);
            Ok(())
        })
        .map_err(|e| e.to_string())
}

/// The names of the sessions in `file` due at `now`, oldest first.
fn due_names(file: &session::StoreFile, now: i64, days: u32) -> Vec<(String, PathBuf)> {
    let mut due: Vec<&Record> = file
        .sessions
        .values()
        .filter(|r| is_due(r, now, days))
        .collect();
    due.sort_by_key(|r| r.updated);
    due.iter()
        .map(|r| (r.name.clone(), r.codex_home.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Automatic expiry
// ---------------------------------------------------------------------------

/// Automatic expiry (docs/design.md, "Automatic expiry"): the sessions in this project's store
/// idle more than `days` days, through the server's own child, whose home is `codex_home`, until
/// `deadline`. Run once per server process on a thread of its own, so it never fails or delays the
/// call that started it; the outcome goes to stderr and to the store for `status`.
pub fn expire(
    store: &Store,
    codex_home: &Path,
    days: u32,
    delete: DeleteThread<'_>,
    busy_here: BusyHere<'_>,
    deadline: Instant,
) {
    let file = match store.read() {
        Ok(file) => file,
        Err(e) => {
            eprintln!("codex-imagegen: session expiry did not run: {e}");
            return;
        }
    };
    let now = session::now_unix();
    let names: Vec<String> = due_names(&file, now, days)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let tally = clean_store(
        store,
        &names,
        codex_home,
        now,
        days,
        delete,
        busy_here,
        Some(deadline),
        |_, _| {},
    );
    let rule = match days {
        1 => "idle over 1 day".to_string(),
        n => format!("idle over {n} days"),
    };
    if names.is_empty() {
        eprintln!("codex-imagegen: session expiry ({rule}): nothing to remove");
    } else {
        eprintln!(
            "codex-imagegen: session expiry ({rule}): {}",
            tally.summary()
        );
    }
    if let Err(e) = record_outcome(store, &tally, now) {
        eprintln!("codex-imagegen: session expiry could not record its outcome: {e}");
    }
}

// ---------------------------------------------------------------------------
// The manual sweep
// ---------------------------------------------------------------------------

/// `codex-imagegen.exe --cleanup [--older-than-days N]` (docs/design.md, "Manual sweep"): every
/// project store under the state base (and `--state-dir`'s, when it lies elsewhere). Sessions are
/// grouped by the Codex home their thread lives in, with one child per home: the default child,
/// the one a server here would start, for its own home, and one with `CODEX_HOME` set for each
/// other. N defaults to `--session-ttl-days`; 0 means every session not in use.
///
/// Prints what it removed and skipped to `out`. Returns the exit code: 0 when the state base could
/// be searched, every store read and every home reached (sessions skipped because they are in use
/// are normal), 1 otherwise.
pub fn sweep(
    cfg: &Config,
    launcher: &dyn Launcher,
    older_than_days: Option<u32>,
    out: &mut dyn Write,
) -> i32 {
    let days = match older_than_days {
        Some(days) => days,
        None if cfg.session_ttl_days == 0 => {
            let _ = writeln!(
                out,
                "codex-imagegen cleanup: expiry is off (--session-ttl-days 0), so nothing was \
                 removed. Pass --older-than-days N to sweep sessions idle over N days (0: every \
                 session not in use)."
            );
            return 0;
        }
        None => cfg.session_ttl_days,
    };
    let (stores, unsearched) = project_stores(cfg);
    let rule = match days {
        0 => "every session not in use".to_string(),
        1 => "sessions idle over 1 day".to_string(),
        n => format!("sessions idle over {n} days"),
    };
    let _ = writeln!(
        out,
        "codex-imagegen cleanup: {rule}, in {} project store(s) under {}",
        stores.len(),
        cfg.state_base.display()
    );
    // A project that could not be searched is a failure, never an empty project: otherwise its
    // sessions would drop out of the sweep unseen and the sweep would report success.
    for why in &unsearched {
        let _ = writeln!(out, "could not search for project stores: {why}");
    }

    let now = session::now_unix();
    let mut failed = !unsearched.is_empty();
    let mut tallies: Vec<Option<Tally>> = Vec::new();
    // (home, [(store index, session name)]), homes compared as Windows compares paths.
    let mut groups: Vec<(PathBuf, Vec<(usize, String)>)> = Vec::new();
    for (index, store) in stores.iter().enumerate() {
        match store.read() {
            Ok(file) => {
                tallies.push(Some(Tally::default()));
                for (name, home) in due_names(&file, now, days) {
                    match groups.iter_mut().find(|(h, _)| same_path(h, &home)) {
                        Some((_, members)) => members.push((index, name)),
                        None => groups.push((home, vec![(index, name)])),
                    }
                }
            }
            Err(e) => {
                failed = true;
                tallies.push(None);
                let _ = writeln!(out, "skipped a store: {e}");
            }
        }
    }

    if groups.is_empty() {
        let _ = writeln!(
            out,
            "nothing to remove{}",
            if failed {
                " in the stores that could be read"
            } else {
                ""
            }
        );
    } else {
        failed |= sweep_groups(
            cfg,
            launcher,
            &stores,
            &groups,
            now,
            days,
            &mut tallies,
            out,
        );
    }

    let mut total = Tally::default();
    for (store, tally) in stores.iter().zip(&tallies) {
        let Some(tally) = tally else { continue };
        if let Err(e) = record_outcome(store, tally, now) {
            let _ = writeln!(
                out,
                "could not record the outcome in {}: {e}",
                store.path().display()
            );
        }
        total.removed.extend(tally.removed.iter().cloned());
        total.freed += tally.freed;
        total.skipped.extend(tally.skipped.iter().cloned());
    }
    let _ = writeln!(
        out,
        "done: removed {}, freed {}{}, skipped {}{}",
        sessions(total.removed.len()),
        megabytes(total.freed),
        if total.removed.is_empty() {
            ""
        } else {
            " (not counting the Codex rollouts thread/delete removed)"
        },
        total.skipped.len(),
        match unsearched.len() {
            0 => String::new(),
            n => format!("; {n} place(s) could not be searched for project stores"),
        }
    );
    i32::from(failed)
}

/// Run each home's sessions through a child of that home. Returns whether a home could not be
/// reached.
#[allow(clippy::too_many_arguments)]
fn sweep_groups(
    cfg: &Config,
    launcher: &dyn Launcher,
    stores: &[Store],
    groups: &[(PathBuf, Vec<(usize, String)>)],
    now: i64,
    days: u32,
    tallies: &mut [Option<Tally>],
    out: &mut dyn Write,
) -> bool {
    let mut failed = false;
    let skip_group = |members: &[(usize, String)],
                      why: &str,
                      tallies: &mut [Option<Tally>],
                      out: &mut dyn Write| {
        for (index, name) in members {
            let _ = writeln!(out, "skipped {}: {name}: {why}", label(&stores[*index]));
            if let Some(tally) = tallies[*index].as_mut() {
                tally.add(name, Fate::Skipped(why.to_string()));
            }
        }
    };
    let bin = match launcher.resolve(cfg) {
        Ok(bin) => bin,
        Err(failure) => {
            for (_, members) in groups {
                skip_group(
                    members,
                    &format!("the Codex CLI was not found ({})", failure.code),
                    tallies,
                    out,
                );
            }
            let _ = writeln!(out, "{}", failure.summary);
            return true;
        }
    };
    // The child a server here would start: its home is the ambient one, or --codex-home.
    let default = start_child(launcher, cfg, &bin);
    if let Err(failure) = &default {
        let _ = writeln!(
            out,
            "Codex could not be started: {} ({})",
            failure.summary, failure.code
        );
    }
    for (home, members) in groups {
        let own;
        let child = match &default {
            Ok((server, handshake)) if same_path(&handshake.codex_home, home) => {
                Ok((server, handshake))
            }
            _ => {
                let mut for_home = cfg.clone();
                for_home.codex_home = Some(home.clone());
                own = start_child(launcher, &for_home, &bin);
                match &own {
                    Ok((server, handshake)) if same_path(&handshake.codex_home, home) => {
                        Ok((server, handshake))
                    }
                    Ok((_, handshake)) => Err(format!(
                        "Codex started with CODEX_HOME={} reported the home {}",
                        home.display(),
                        handshake.codex_home.display()
                    )),
                    Err(failure) => Err(format!(
                        "Codex could not be started for the home {} ({}: {})",
                        home.display(),
                        failure.code,
                        crate::jsonrpc::clamp(&failure.summary, 200)
                    )),
                }
            }
        };
        let (server, handshake) = match child {
            Ok(child) => child,
            Err(why) => {
                failed = true;
                skip_group(members, &why, tallies, out);
                continue;
            }
        };
        let delete = |thread_id: &str| -> Result<(), RpcError> {
            server
                .request(
                    "thread/delete",
                    json!({"threadId": thread_id}),
                    Instant::now() + codex::CALL_DEADLINE,
                    None,
                )
                .map(|_| ())
        };
        for (index, name) in members {
            let store = &stores[*index];
            let fate = remove_session(
                store,
                name,
                &handshake.codex_home,
                now,
                days,
                &delete,
                &|_| false,
            );
            match &fate {
                Fate::Removed { freed } => {
                    let _ = writeln!(
                        out,
                        "removed {}: {name} (freed {})",
                        label(store),
                        megabytes(*freed)
                    );
                }
                Fate::Skipped(why) => {
                    let _ = writeln!(out, "skipped {}: {name}: {why}", label(store));
                }
                Fate::NotDue => {
                    let _ = writeln!(out, "left {}: {name}: it was used meanwhile", label(store));
                }
            }
            if let Some(tally) = tallies[*index].as_mut() {
                tally.add(name, fate);
            }
        }
    }
    if let Ok((server, _)) = &default {
        server.shutdown(CHILD_GRACE);
    }
    failed
}

/// A Codex child for thread/delete: spawned and initialized. No preflight: deleting a thread is
/// local to the Codex home, needs no sign-in or model, and runs no turn, so a signed-out home can
/// still be cleaned [decided]. The spawn line is the server's own, switches and all. The child is
/// shut down when its `AppServer` drops, or by the caller.
fn start_child(
    launcher: &dyn Launcher,
    cfg: &Config,
    bin: &Path,
) -> Result<(AppServer, Handshake), Failure> {
    let server = launcher.spawn(cfg, bin)?;
    let handshake = codex::initialize(&Rpc {
        server: &server,
        cancel: None,
        per_call: codex::CALL_DEADLINE,
        budget: None,
    });
    match handshake {
        Ok(handshake) => Ok((server, handshake)),
        Err(failure) => {
            server.shutdown(CHILD_GRACE);
            Err(failure)
        }
    }
}

/// Every project store: each folder directly under the state base that holds a session store,
/// plus `--state-dir`'s when it lies elsewhere. Also what could not be searched: only genuine
/// absence (NotFound) counts as no store, so a project folder that cannot be read is reported
/// rather than silently missing from the sweep.
fn project_stores(cfg: &Config) -> (Vec<Store>, Vec<String>) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut unsearched = Vec::new();
    let base = &cfg.state_base;
    match fs::read_dir(base) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        unsearched.push(format!("could not list {} ({e})", base.display()));
                        continue;
                    }
                };
                // A file directly under the state base is not a project folder.
                match entry.file_type() {
                    Ok(kind) if kind.is_file() => continue,
                    Ok(_) => {}
                    Err(e) => {
                        unsearched.push(format!("could not read {} ({e})", entry.path().display()));
                        continue;
                    }
                }
                let dir = entry.path();
                match holds_store(&dir) {
                    Ok(true) => dirs.push(dir),
                    Ok(false) => {}
                    Err(why) => unsearched.push(why),
                }
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => unsearched.push(format!("could not list {} ({e})", base.display())),
    }
    if !dirs.iter().any(|dir| same_path(dir, &cfg.state_dir)) {
        match holds_store(&cfg.state_dir) {
            Ok(true) => dirs.push(cfg.state_dir.clone()),
            Ok(false) => {}
            Err(why) => unsearched.push(why),
        }
    }
    dirs.sort();
    (dirs.iter().map(|dir| Store::new(dir)).collect(), unsearched)
}

/// Whether `dir` holds a session store. `Ok(false)` only when there is none; anything that stops
/// that being told is an error.
fn holds_store(dir: &Path) -> Result<bool, String> {
    let path = dir.join(STORE_FILE);
    match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => Ok(true),
        Ok(_) => Err(format!("{} is not a file", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("could not read {} ({e})", path.display())),
    }
}

/// A store as the sweep's report names it: its project folder's name.
fn label(store: &Store) -> String {
    let path = store.path();
    path.parent().and_then(Path::file_name).map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::testing::{FakeCodex, CODEX_HOME};
    use crate::config::Fnv1a64;
    use crate::session::{Output, StoreFile};
    use crate::testutil::{make_junction, set_read_only, temp_dir, TempDir};
    use crate::tools::testing::cfg;
    use std::io::Read;
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const THREAD: &str = "019a0000-0000-7000-8000-00000000cafe";
    const OTHER_THREAD: &str = "019a0000-0000-7000-8000-00000000f00d";
    const OLD: i64 = 1_000_000;

    /// A session with images on disk: Codex's copies under its home, ours in an output folder.
    struct World {
        dir: TempDir,
        store: Store,
        home: PathBuf,
    }

    impl World {
        fn new() -> Self {
            let dir = temp_dir("cleanup");
            let home = dir.join("codex-home");
            fs::create_dir_all(&home).unwrap();
            let store = Store::new(&dir.join("state").join("proj-0000000000000000"));
            Self { dir, store, home }
        }

        fn out_dir(&self) -> PathBuf {
            let out = self.dir.join("out");
            fs::create_dir_all(&out).unwrap();
            out
        }

        fn images_dir(&self, thread: &str) -> PathBuf {
            self.home.join("generated_images").join(thread)
        }

        /// A session on `thread`, last updated at `updated`, with `versions` published and each
        /// also in Codex's image folder.
        fn session(&self, name: &str, thread: &str, updated: i64, versions: &[u32]) -> Record {
            let codex_dir = self.images_dir(thread);
            fs::create_dir_all(&codex_dir).unwrap();
            let mut outputs = Vec::new();
            for v in versions {
                let bytes = vec![*v as u8; 100 + *v as usize];
                fs::write(codex_dir.join(format!("exec-{v}.png")), &bytes).unwrap();
                let path = self.out_dir().join(output::file_name(name, *v));
                fs::write(&path, &bytes).unwrap();
                outputs.push(Output {
                    version: *v,
                    resolved_path: Some(fs::canonicalize(&path).unwrap()),
                    path,
                    bytes: bytes.len() as u64,
                    fnv1a64: Some(Output::fingerprint_text(Fnv1a64::of(&bytes))),
                });
            }
            let record = Record {
                name: name.to_string(),
                thread_id: thread.to_string(),
                codex_home: self.home.clone(),
                model: "gpt-6-astra".to_string(),
                created: updated,
                updated,
                turns: versions.len() as u32,
                last_saved_path: None,
                last_output_path: outputs.last().map(|o| o.path.clone()),
                last_output_bytes: outputs.last().map(|o| o.bytes),
                output_dir: self.out_dir(),
                next_version: versions.iter().max().map_or(1, |v| v + 1),
                outputs,
            };
            self.store
                .update(|file| {
                    file.sessions.insert(session::key(name), record.clone());
                    Ok(())
                })
                .unwrap();
            record
        }

        fn file(&self) -> StoreFile {
            self.store.read().unwrap()
        }
    }

    /// A delete that records each thread asked for and answers from `errors`.
    struct Deletes {
        asked: Mutex<Vec<String>>,
        errors: Vec<(String, RpcError)>,
    }

    impl Deletes {
        fn ok() -> Self {
            Self::failing(&[])
        }

        fn failing(errors: &[(&str, &str)]) -> Self {
            Self {
                asked: Mutex::default(),
                errors: errors
                    .iter()
                    .map(|(thread, message)| {
                        (
                            thread.to_string(),
                            RpcError::Remote {
                                code: -32600,
                                message: message.to_string(),
                            },
                        )
                    })
                    .collect(),
            }
        }

        fn call(&self, thread: &str) -> Result<(), RpcError> {
            self.asked.lock().unwrap().push(thread.to_string());
            match self.errors.iter().find(|(t, _)| t == thread) {
                Some((_, error)) => Err(error.clone()),
                None => Ok(()),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    fn expire_with(world: &World, days: u32, deletes: &Deletes, busy: BusyHere<'_>) {
        expire(
            &world.store,
            &world.home,
            days,
            &|t| deletes.call(t),
            busy,
            Instant::now() + Duration::from_secs(30),
        );
    }

    #[test]
    fn an_expired_session_is_removed_completely() {
        let world = World::new();
        let record = world.session("Fox", THREAD, OLD, &[1, 2]);
        // Recent: not due.
        world.session("owl", OTHER_THREAD, session::now_unix(), &[1]);
        let deletes = Deletes::ok();
        expire_with(&world, 7, &deletes, &|_| false);

        assert_eq!(deletes.asked(), vec![THREAD.to_string()]);
        assert!(!world.images_dir(THREAD).exists(), "Codex's image folder");
        for output in &record.outputs {
            assert!(!output.path.exists(), "{}", output.path.display());
        }
        let file = world.file();
        assert!(file.get("fox").is_none(), "the record was kept");
        assert!(file.get("owl").is_some(), "a recent session was removed");
        assert!(world.images_dir(OTHER_THREAD).is_dir());
        let outcome = file.last_cleanup.expect("the outcome is recorded");
        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.freed_bytes, 2 * (101 + 102));
        assert!(outcome.skipped.is_empty());
    }

    #[test]
    fn the_record_is_dropped_last_so_a_cleanup_cut_short_is_retried() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1]);
        // A file that cannot be deleted yet: open without delete sharing.
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1)
            .open(&record.outputs[0].path)
            .unwrap();
        let deletes = Deletes::ok();
        expire_with(&world, 7, &deletes, &|_| false);
        let file = world.file();
        assert!(
            file.get("fox").is_some(),
            "the record went before its files"
        );
        let skipped = &file.last_cleanup.unwrap().skipped;
        assert!(
            skipped[0].why.contains("retried next time")
                && skipped[0].why.contains("could not delete"),
            "{:?}",
            skipped
        );
        assert!(record.outputs[0].path.is_file(), "a held file was deleted");
        assert!(!world.images_dir(THREAD).exists());

        // Next time the thread is already gone, and the rest goes.
        drop(held);
        let gone = Deletes::failing(&[(THREAD, "no rollout found for thread id x")]);
        expire_with(&world, 7, &gone, &|_| false);
        assert!(world.file().get("fox").is_none());
        assert!(!record.outputs[0].path.exists());
    }

    #[test]
    fn a_session_whose_lease_is_held_or_busy_here_is_skipped_and_nothing_is_deleted() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1]);
        world.session("owl", OTHER_THREAD, OLD, &[1]);
        let elsewhere = Store::new(world.store.path().parent().unwrap())
            .try_lease("FOX")
            .unwrap()
            .unwrap();
        let deletes = Deletes::ok();
        expire_with(&world, 7, &deletes, &|name| {
            name.eq_ignore_ascii_case("OWL")
        });
        assert!(deletes.asked().is_empty());
        assert!(record.outputs[0].path.exists());
        assert!(world.images_dir(THREAD).is_dir());
        let file = world.file();
        assert_eq!(file.sessions.len(), 2);
        let skipped = file.last_cleanup.unwrap().skipped;
        let why: Vec<&str> = skipped.iter().map(|s| s.why.as_str()).collect();
        assert!(
            why.iter()
                .any(|w| w.contains("another codex-imagegen process")),
            "{why:?}"
        );
        assert!(why.iter().any(|w| w.contains("in this server")), "{why:?}");
        drop(elsewhere);
    }

    #[test]
    fn no_rollout_found_counts_as_gone_and_other_errors_skip_the_session() {
        let world = World::new();
        world.session("fox", THREAD, OLD, &[1]);
        world.session("owl", OTHER_THREAD, OLD, &[1]);
        let deletes = Deletes::failing(&[
            (THREAD, &format!("no rollout found for thread id {THREAD}")),
            (
                OTHER_THREAD,
                &format!("thread {OTHER_THREAD} already has an active writer"),
            ),
        ]);
        expire_with(&world, 7, &deletes, &|_| false);
        let file = world.file();
        assert!(
            file.get("fox").is_none(),
            "no rollout found is already gone"
        );
        assert!(!world.images_dir(THREAD).exists());
        let owl = file.get("owl").expect("an active writer skips the session");
        assert!(owl.outputs[0].path.exists());
        assert!(world.images_dir(OTHER_THREAD).is_dir());
        let skipped = file.last_cleanup.unwrap().skipped;
        assert_eq!(skipped.len(), 1);
        assert!(
            skipped[0].why.contains("open in another process"),
            "{skipped:?}"
        );
    }

    #[test]
    fn an_output_that_changed_or_was_renamed_is_kept_and_the_session_still_goes() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1, 2, 3]);
        // v1 edited (another size), v2 replaced by a folder, v3 recorded under another name.
        fs::write(&record.outputs[0].path, b"edited by hand").unwrap();
        fs::remove_file(&record.outputs[1].path).unwrap();
        fs::create_dir(&record.outputs[1].path).unwrap();
        let renamed = world.out_dir().join("keeper.png");
        fs::write(&renamed, vec![3u8; 103]).unwrap();
        world
            .store
            .update(|file| {
                file.sessions.get_mut("fox").unwrap().outputs[2].path = renamed.clone();
                Ok(())
            })
            .unwrap();
        // Something in Codex's folder that is not a PNG keeps the folder, not the session.
        fs::write(world.images_dir(THREAD).join("notes.txt"), b"x").unwrap();

        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        assert!(world.file().get("fox").is_none());
        assert_eq!(
            fs::read(&record.outputs[0].path).unwrap(),
            b"edited by hand"
        );
        assert!(record.outputs[1].path.is_dir());
        assert!(renamed.is_file());
        assert!(record.outputs[2].path.is_file(), "an unrecorded file");
        assert!(world.images_dir(THREAD).join("notes.txt").is_file());
        assert!(!world.images_dir(THREAD).join("exec-1.png").exists());
    }

    #[test]
    fn an_output_replaced_by_other_content_of_the_same_size_is_kept() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1, 2]);
        // v1 replaced by an edit of exactly the same length: name, type and size all still match.
        let edited = vec![0xEEu8; record.outputs[0].bytes as usize];
        fs::write(&record.outputs[0].path, &edited).unwrap();
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        assert_eq!(
            fs::read(&record.outputs[0].path).unwrap(),
            edited,
            "a same-size replacement was deleted"
        );
        assert!(
            !record.outputs[1].path.exists(),
            "the unchanged v2 was kept"
        );
        let file = world.file();
        assert!(
            file.get("fox").is_none(),
            "a file kept on purpose kept the session"
        );
        // Codex's two copies and our unchanged v2.
        assert_eq!(file.last_cleanup.unwrap().freed_bytes, 101 + 102 + 102);
    }

    #[test]
    fn an_output_whose_record_has_no_fingerprint_is_kept() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1]);
        // As an earlier build recorded it.
        world
            .store
            .update(|file| {
                file.sessions.get_mut("fox").unwrap().outputs[0].fnv1a64 = None;
                Ok(())
            })
            .unwrap();
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        assert!(
            record.outputs[0].path.is_file(),
            "a file that cannot be shown to be ours was deleted"
        );
        assert!(world.file().get("fox").is_none());
        assert!(!world.images_dir(THREAD).exists());
    }

    #[test]
    fn an_output_folder_swapped_for_a_junction_to_an_identical_copy_keeps_the_copy() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1, 2]);
        // The output folder is archived, and a junction to the archive stands where it was: each
        // recorded path now reaches a copy with the same name, size and content.
        let out = world.out_dir();
        let archive = world.dir.join("archive");
        fs::create_dir(&archive).unwrap();
        for output in &record.outputs {
            fs::copy(&output.path, archive.join(output.path.file_name().unwrap())).unwrap();
        }
        fs::remove_dir_all(&out).unwrap();
        make_junction(&out, &archive);
        assert_eq!(fs::read(&record.outputs[0].path).unwrap(), vec![1u8; 101]);

        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        for name in ["fox-v1.png", "fox-v2.png"] {
            assert!(
                archive.join(name).is_file(),
                "the archived copy {name} was deleted through the junction"
            );
        }
        assert_eq!(
            fs::read(archive.join("fox-v1.png")).unwrap(),
            vec![1u8; 101]
        );
        assert_eq!(
            fs::read(archive.join("fox-v2.png")).unwrap(),
            vec![2u8; 102]
        );
        // Kept on purpose, like a renamed file: nothing of ours is left at the recorded paths, and
        // the link will still be there next time, so the session goes rather than being retried
        // for ever.
        assert!(!world.images_dir(THREAD).exists(), "Codex's image folder");
        let file = world.file();
        assert!(file.get("fox").is_none(), "the kept copy kept the session");
        let outcome = file.last_cleanup.unwrap();
        assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
        assert_eq!(outcome.freed_bytes, 101 + 102, "Codex's two copies only");
        // Why, as cleanup logs it.
        let v1 = &record.outputs[0];
        let verdict = delete_output(
            &v1.path,
            v1.bytes,
            v1.fingerprint().unwrap(),
            v1.resolved_path.as_deref().unwrap(),
        )
        .unwrap();
        assert!(
            matches!(&verdict, Verdict::Kept(why)
                if why.starts_with("it is no longer where it was published")),
            "{verdict:?}"
        );
    }

    #[test]
    fn an_output_whose_record_has_no_resolved_path_is_kept() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1]);
        // As an earlier build recorded it: a fingerprint, but no resolved path.
        world
            .store
            .update(|file| {
                file.sessions.get_mut("fox").unwrap().outputs[0].resolved_path = None;
                Ok(())
            })
            .unwrap();
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        assert!(
            record.outputs[0].path.is_file(),
            "a file that cannot be shown to be where it was published was deleted"
        );
        let file = world.file();
        assert!(file.get("fox").is_none());
        assert!(file.last_cleanup.unwrap().skipped.is_empty());
        assert!(!world.images_dir(THREAD).exists());
    }

    #[test]
    fn a_matching_output_is_checked_and_deleted_through_one_handle() {
        let dir = temp_dir("cleanup");
        let path = dir.join("fox-v1.png");
        fs::write(&path, b"foobar").unwrap();
        let fingerprint = Fnv1a64::of(b"foobar");
        let resolved = fs::canonicalize(&path).unwrap();
        let kept = |verdict: Verdict| matches!(verdict, Verdict::Kept(_));
        assert!(kept(
            delete_output(&path, 7, fingerprint, &resolved).unwrap()
        ));
        assert!(kept(
            delete_output(&path, 6, fingerprint ^ 1, &resolved).unwrap()
        ));
        // Everything matches but where it was published.
        let elsewhere = fs::canonicalize(&dir)
            .unwrap()
            .join("other")
            .join("fox-v1.png");
        assert!(kept(
            delete_output(&path, 6, fingerprint, &elsewhere).unwrap()
        ));
        assert_eq!(fs::read(&path).unwrap(), b"foobar");
        // Compared exactly, apart from the `\\?\` prefix: another case is another path, so the
        // file is kept...
        let upper = resolved.to_string_lossy().to_uppercase();
        assert!(kept(
            delete_output(&path, 6, fingerprint, Path::new(&upper)).unwrap()
        ));
        assert_eq!(fs::read(&path).unwrap(), b"foobar");
        // ...and a Unicode case fold NTFS does not apply (KELVIN SIGN for `k`) is not a match
        // either, although Rust's to_lowercase would make it one.
        let folded = resolved.to_string_lossy().replacen('k', "\u{212a}", 1);
        if folded != resolved.to_string_lossy() {
            assert!(kept(
                delete_output(&path, 6, fingerprint, Path::new(&folded)).unwrap()
            ));
        }
        // ...while the same path without the prefix matches.
        let plain = resolved.to_string_lossy()[4..].to_string();
        assert_eq!(
            delete_output(&path, 6, fingerprint, Path::new(&plain)).unwrap(),
            Verdict::Deleted(6)
        );
        assert!(!path.exists());
        assert_eq!(
            delete_output(&path, 6, fingerprint, &resolved).unwrap(),
            Verdict::Gone
        );
        // A folder at the name is not a file of ours.
        fs::create_dir(&path).unwrap();
        assert!(kept(
            delete_output(&path, 0, Fnv1a64::of(b""), &resolved).unwrap()
        ));
        assert!(path.is_dir());
    }

    #[test]
    fn an_output_replaced_while_its_deletion_is_retried_is_checked_again_and_kept() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1]);
        let path = record.outputs[0].path.clone();
        let edited = vec![0xEEu8; record.outputs[0].bytes as usize];
        // A writer that shares neither writing nor deleting holds the file, so the first attempts
        // to delete it fail; while it holds it, it writes new content of the same size.
        let mut held = fs::OpenOptions::new()
            .write(true)
            .share_mode(0x1)
            .open(&path)
            .unwrap();
        std::thread::scope(|s| {
            let edited = edited.clone();
            s.spawn(move || {
                std::thread::sleep(Duration::from_millis(60));
                held.write_all(&edited).unwrap();
                held.sync_all().unwrap();
            });
            expire_with(&world, 7, &Deletes::ok(), &|_| false);
        });
        assert_eq!(
            fs::read(&path).unwrap(),
            edited,
            "the replacement was deleted"
        );
        assert!(world.file().get("fox").is_none());
    }

    /// Well under the ~0.8 s `output::retry_transient` spends on an error it takes for a busy file.
    const PROMPTLY: Duration = Duration::from_millis(600);

    #[test]
    fn a_read_only_output_is_kept_at_once_and_the_session_still_goes() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1, 2]);
        let protected = &record.outputs[0];
        set_read_only(&protected.path);
        let started = Instant::now();
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        let took = started.elapsed();
        assert!(
            took < PROMPTLY,
            "the read-only file was retried as if it were busy ({took:?})"
        );
        assert_eq!(fs::read(&protected.path).unwrap(), vec![1u8; 101]);
        assert!(!record.outputs[1].path.exists(), "the unprotected v2");
        assert!(!world.images_dir(THREAD).exists(), "Codex's image folder");
        let file = world.file();
        assert!(
            file.get("fox").is_none(),
            "a read-only file kept the session"
        );
        let outcome = file.last_cleanup.unwrap();
        assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
        // Codex's two copies and our v2.
        assert_eq!(outcome.freed_bytes, 101 + 102 + 102);
        // Kept on purpose, which cleanup logs, without the deletion being attempted.
        let fingerprint = protected.fingerprint().unwrap();
        let resolved = protected.resolved_path.as_deref().unwrap();
        assert_eq!(
            delete_output(&protected.path, protected.bytes, fingerprint, resolved).unwrap(),
            Verdict::Kept("it is read-only".to_string())
        );
        assert!(protected.path.is_file());
    }

    #[test]
    fn a_read_only_codex_png_is_kept_with_its_folder_and_the_session_still_goes() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1, 2]);
        let protected = world.images_dir(THREAD).join("exec-1.png");
        set_read_only(&protected);
        let started = Instant::now();
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        let took = started.elapsed();
        assert!(
            took < PROMPTLY,
            "the read-only file was retried as if it were busy ({took:?})"
        );
        assert_eq!(fs::read(&protected).unwrap(), vec![1u8; 101]);
        assert!(!world.images_dir(THREAD).join("exec-2.png").exists());
        for output in &record.outputs {
            assert!(!output.path.exists(), "{}", output.path.display());
        }
        let file = world.file();
        assert!(
            file.get("fox").is_none(),
            "a read-only file kept the session"
        );
        let outcome = file.last_cleanup.unwrap();
        assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
        // Codex's v2 and our two copies.
        assert_eq!(outcome.freed_bytes, 102 + 101 + 102);
        // Kept on purpose, which cleanup logs, and its folder with it.
        let folder = fs::canonicalize(world.images_dir(THREAD)).unwrap();
        assert_eq!(
            delete_codex_png(&protected, &folder).unwrap(),
            Verdict::Kept("it is read-only".to_string())
        );
        assert!(protected.is_file());
    }

    #[test]
    fn a_read_only_codex_folder_is_emptied_but_kept_at_once_and_the_session_still_goes() {
        let world = World::new();
        world.session("fox", THREAD, OLD, &[1]);
        let dir = world.images_dir(THREAD);
        set_read_only(&dir);
        let started = Instant::now();
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        let took = started.elapsed();
        assert!(
            took < PROMPTLY,
            "the read-only folder was retried as if it were busy ({took:?})"
        );
        // Its PNG is deleted: the attribute on a folder does not protect what is in it.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(world.file().get("fox").is_none());
        let folder = fs::canonicalize(&dir).unwrap();
        assert_eq!(
            remove_codex_folder(&dir, &folder).unwrap(),
            Verdict::Kept("it is read-only".to_string())
        );
        assert!(dir.is_dir());
    }

    #[test]
    fn a_codex_png_open_in_a_viewer_that_shares_delete_is_unlinked_and_its_folder_removed() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1]);
        let png = world.images_dir(THREAD).join("exec-1.png");
        // An image viewer: reading, and sharing reading, writing and deleting.
        let mut viewer = fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2 | 0x4)
            .open(&png)
            .unwrap();
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        assert!(
            !world.images_dir(THREAD).exists(),
            "Codex's image folder stayed behind a PNG open in a viewer"
        );
        assert!(!record.outputs[0].path.exists());
        let file = world.file();
        assert!(file.get("fox").is_none());
        let outcome = file.last_cleanup.unwrap();
        assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
        assert_eq!(outcome.freed_bytes, 101 + 101);
        // The viewer still reads what it opened.
        let mut content = Vec::new();
        viewer.read_to_end(&mut content).unwrap();
        assert_eq!(content, vec![1u8; 101]);
    }

    /// A handle on `dir` that shares reading and writing but not deleting, as another process
    /// might hold it: the folder cannot be removed while it is open.
    fn hold_folder(dir: &Path) -> fs::File {
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir)
            .unwrap()
    }

    #[test]
    fn a_codex_folder_that_cannot_be_removed_keeps_the_record_and_is_retried() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1]);
        let dir = world.images_dir(THREAD);
        let held = hold_folder(&dir);
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        // Its PNG and the session's output went; the folder could not.
        assert!(dir.is_dir(), "a held folder was removed");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(!record.outputs[0].path.exists());
        let file = world.file();
        assert!(
            file.get("fox").is_some(),
            "the record went, so nothing would ever retry the folder"
        );
        let outcome = file.last_cleanup.unwrap();
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.skipped.len(), 1, "{:?}", outcome.skipped);
        let why = &outcome.skipped[0].why;
        assert!(
            why.contains(&format!("could not delete {}", dir.display()))
                && why.contains("os error 32")
                && why.contains("retried next time"),
            "{why}"
        );

        // Next time the handle is gone, the thread and outputs already are, and the rest goes.
        drop(held);
        let gone = Deletes::failing(&[(THREAD, "no rollout found for thread id x")]);
        expire_with(&world, 7, &gone, &|_| false);
        assert_eq!(gone.asked(), vec![THREAD.to_string()]);
        assert!(!dir.exists(), "the folder was not retried");
        assert!(world.home.join("generated_images").is_dir());
        let file = world.file();
        assert!(file.get("fox").is_none());
        let outcome = file.last_cleanup.unwrap();
        assert_eq!(outcome.removed, 1);
        assert_eq!(outcome.freed_bytes, 0);
        assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
    }

    #[test]
    fn a_codex_folder_kept_on_purpose_is_not_retried_and_the_session_still_goes() {
        let world = World::new();
        let record = world.session("fox", THREAD, OLD, &[1]);
        let dir = world.images_dir(THREAD);
        // A read-only PNG, which cleanup keeps, so the folder cannot be emptied; held as well,
        // which must not turn it into a failure or a wait.
        let protected = dir.join("exec-1.png");
        set_read_only(&protected);
        let held = hold_folder(&dir);
        let started = Instant::now();
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        let took = started.elapsed();
        drop(held);
        assert!(
            took < PROMPTLY,
            "the removal of a folder cleanup kept something in was retried ({took:?})"
        );
        assert_eq!(fs::read(&protected).unwrap(), vec![1u8; 101]);
        assert!(!record.outputs[0].path.exists());
        let file = world.file();
        assert!(
            file.get("fox").is_none(),
            "a folder kept on purpose kept the session"
        );
        let outcome = file.last_cleanup.unwrap();
        assert!(outcome.skipped.is_empty(), "{:?}", outcome.skipped);
        assert_eq!(outcome.freed_bytes, 101, "our copy only");
    }

    #[test]
    fn a_generated_images_junction_is_not_followed_and_the_rest_of_the_session_goes() {
        let world = World::new();
        // Codex's image folder reached through a junction at generated_images: the thread's PNGs
        // really live elsewhere.
        let elsewhere = world.dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        make_junction(&world.home.join("generated_images"), &elsewhere);
        let record = world.session("fox", THREAD, OLD, &[1]);
        let behind = elsewhere.join(THREAD).join("exec-1.png");
        assert!(behind.is_file());
        let deletes = Deletes::ok();
        expire_with(&world, 7, &deletes, &|_| false);
        assert!(behind.is_file(), "a PNG behind the junction was deleted");
        assert!(world.home.join("generated_images").exists());
        assert_eq!(deletes.asked(), vec![THREAD.to_string()]);
        assert!(!record.outputs[0].path.exists(), "the session's own output");
        let file = world.file();
        assert!(file.get("fox").is_none(), "the rest of the session went");
        assert_eq!(file.last_cleanup.unwrap().freed_bytes, 101);
    }

    #[test]
    fn a_thread_folder_that_is_a_junction_is_left_alone() {
        let world = World::new();
        let elsewhere = world.dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        fs::create_dir_all(world.home.join("generated_images")).unwrap();
        make_junction(&world.images_dir(THREAD), &elsewhere);
        let record = world.session("fox", THREAD, OLD, &[1]);
        assert!(elsewhere.join("exec-1.png").is_file());
        expire_with(&world, 7, &Deletes::ok(), &|_| false);
        assert!(
            elsewhere.join("exec-1.png").is_file(),
            "a PNG behind the junction was deleted"
        );
        assert!(world.images_dir(THREAD).exists(), "the junction itself");
        assert!(!record.outputs[0].path.exists());
        assert!(world.file().get("fox").is_none());
    }

    #[test]
    fn a_codex_home_reached_through_a_junction_is_cleaned_like_any_other() {
        let world = World::new();
        world.session("fox", THREAD, OLD, &[1]);
        // The child reports the home by a link to it, and the record names it so.
        let link = world.dir.join("home-link");
        make_junction(&link, &world.home);
        world
            .store
            .update(|file| {
                file.sessions.get_mut("fox").unwrap().codex_home = link.clone();
                Ok(())
            })
            .unwrap();
        let deletes = Deletes::ok();
        expire(
            &world.store,
            &link,
            7,
            &|t| deletes.call(t),
            &|_| false,
            Instant::now() + Duration::from_secs(30),
        );
        assert!(!world.images_dir(THREAD).exists());
        assert!(world.home.join("generated_images").is_dir());
        assert!(world.file().get("fox").is_none());
    }

    #[test]
    fn a_codex_png_is_deleted_only_through_a_handle_that_shows_it_in_the_threads_folder() {
        let dir = temp_dir("cleanup");
        let folder = dir.join("generated_images").join(THREAD);
        fs::create_dir_all(&folder).unwrap();
        assert!(matches!(
            check_codex_folder(&dir, THREAD),
            FolderCheck::Ours(_)
        ));
        let canonical = fs::canonicalize(&folder).unwrap();
        let target = dir.join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.png"), b"keep").unwrap();

        // A link at the PNG's own name is opened as itself, never followed.
        make_junction(&folder.join("trap.png"), &target);
        let verdict = delete_codex_png(&folder.join("trap.png"), &canonical).unwrap();
        assert!(matches!(verdict, Verdict::Kept(_)), "{verdict:?}");
        assert!(folder.join("trap.png").exists());

        // A path that reaches a PNG through a link that appeared after the folder was checked:
        // the handle shows where the file really is, and it is refused.
        let swapped = dir.join("swapped");
        make_junction(&swapped, &target);
        let verdict = delete_codex_png(&swapped.join("keep.png"), &canonical).unwrap();
        assert!(matches!(verdict, Verdict::Refused(_)), "{verdict:?}");
        assert_eq!(fs::read(target.join("keep.png")).unwrap(), b"keep");

        // A symbolic link to a file, where this machine lets a test make one (Developer Mode or
        // the privilege); junctions above cover the rest.
        let real = dir.join("real.png");
        fs::write(&real, b"real").unwrap();
        match std::os::windows::fs::symlink_file(&real, folder.join("link.png")) {
            Ok(()) => {
                let verdict = delete_codex_png(&folder.join("link.png"), &canonical).unwrap();
                assert!(matches!(verdict, Verdict::Kept(_)), "{verdict:?}");
                assert_eq!(fs::read(&real).unwrap(), b"real");
            }
            Err(e) => eprintln!("the symbolic-link case did not run: {e}"),
        }

        // A plain PNG in the folder is deleted.
        fs::write(folder.join("exec-1.png"), b"png").unwrap();
        assert_eq!(
            delete_codex_png(&folder.join("exec-1.png"), &canonical).unwrap(),
            Verdict::Deleted(3)
        );
        assert!(!folder.join("exec-1.png").exists());
    }

    #[test]
    fn a_malformed_thread_id_or_a_foreign_codex_home_deletes_nothing() {
        let world = World::new();
        let bad = world.session("bad", THREAD, OLD, &[1]);
        world
            .store
            .update(|file| {
                file.sessions.get_mut("bad").unwrap().thread_id = r"..\..\escape".to_string();
                Ok(())
            })
            .unwrap();
        let foreign = world.session("foreign", OTHER_THREAD, OLD, &[1]);
        let elsewhere = world.dir.join("another-home");
        world
            .store
            .update(|file| {
                file.sessions.get_mut("foreign").unwrap().codex_home = elsewhere.clone();
                Ok(())
            })
            .unwrap();
        let deletes = Deletes::ok();
        expire_with(&world, 7, &deletes, &|_| false);
        assert!(deletes.asked().is_empty(), "{:?}", deletes.asked());
        assert!(bad.outputs[0].path.exists());
        assert!(foreign.outputs[0].path.exists());
        assert!(world.images_dir(THREAD).is_dir());
        assert!(world.images_dir(OTHER_THREAD).is_dir());
        let file = world.file();
        assert_eq!(file.sessions.len(), 2);
        let skipped = file.last_cleanup.unwrap().skipped;
        assert!(skipped
            .iter()
            .any(|s| s.why.contains("not a well-formed UUID")));
        assert!(skipped.iter().any(|s| s.why.contains("another Codex home")));
    }

    #[test]
    fn a_session_refreshed_before_its_lease_was_taken_is_left_alone() {
        let world = World::new();
        world.session("fox", THREAD, OLD, &[1]);
        let file = world.file();
        let names: Vec<String> = due_names(&file, session::now_unix(), 7)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(names, vec!["fox"]);
        // A refine elsewhere finishes between the listing and the lease.
        world
            .store
            .update(|file| {
                file.sessions.get_mut("fox").unwrap().updated = session::now_unix();
                Ok(())
            })
            .unwrap();
        let deletes = Deletes::ok();
        let tally = clean_store(
            &world.store,
            &names,
            &world.home,
            session::now_unix(),
            7,
            &|t| deletes.call(t),
            &|_| false,
            None,
            |_, _| {},
        );
        assert!(tally.removed.is_empty() && tally.skipped.is_empty());
        assert!(deletes.asked().is_empty());
        assert!(world.file().get("fox").is_some());
    }

    #[test]
    fn helpers_recognise_uuids_and_paths_as_windows_does() {
        assert!(is_uuid(THREAD));
        assert!(is_uuid("019A0000-0000-7000-8000-00000000CAFE"));
        for bad in [
            "",
            "t",
            r"..\x",
            "019a0000-0000-7000-8000-00000000cafe0",
            "019a0000_0000-7000-8000-00000000cafe",
        ] {
            assert!(!is_uuid(bad), "{bad}");
        }
        assert!(same_path(
            Path::new(r"\\?\D:\Codex-Home\"),
            Path::new(r"d:\codex-home")
        ));
        assert!(same_path(
            Path::new(r"\\?\UNC\srv\share\h"),
            Path::new(r"\\SRV\share\h")
        ));
        assert!(!same_path(
            Path::new(r"D:\codex-home"),
            Path::new(r"D:\codex-home2")
        ));
        let record = |updated| Record {
            updated,
            ..World::new().session("x", THREAD, 0, &[])
        };
        let now = 10 * SECS_PER_DAY;
        assert!(is_due(&record(now - 8 * SECS_PER_DAY), now, 7));
        assert!(!is_due(&record(now - 7 * SECS_PER_DAY), now, 7));
        assert!(is_due(&record(now), now, 0), "0 is every session");
    }

    // -- the sweep --

    /// Hands out fake children, each reporting the home it was started for, and remembers them.
    struct SweepLauncher {
        codex: FakeCodex,
        spawned: Mutex<Vec<Option<PathBuf>>>,
        seen: Mutex<Vec<Arc<Mutex<Vec<serde_json::Value>>>>>,
        spawns: AtomicUsize,
    }

    impl SweepLauncher {
        fn new(codex: FakeCodex) -> Self {
            Self {
                codex,
                spawned: Mutex::default(),
                seen: Mutex::default(),
                spawns: AtomicUsize::new(0),
            }
        }

        /// Every thread/delete any child was sent, with the home of the child.
        fn deletes(&self) -> Vec<(Option<PathBuf>, String)> {
            let spawned = self.spawned.lock().unwrap();
            let seen = self.seen.lock().unwrap();
            let mut all = Vec::new();
            for (home, seen) in spawned.iter().zip(seen.iter()) {
                for params in FakeCodex::sent(seen, "thread/delete") {
                    all.push((
                        home.clone(),
                        params["threadId"].as_str().unwrap().to_string(),
                    ));
                }
            }
            all
        }
    }

    impl Launcher for SweepLauncher {
        fn resolve(&self, _cfg: &Config) -> Result<PathBuf, Failure> {
            Ok(PathBuf::from(r"C:\fake\codex.exe"))
        }

        fn spawn(&self, cfg: &Config, _bin: &Path) -> Result<AppServer, Failure> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            let mut codex = FakeCodex {
                seen: Arc::default(),
                ..self.codex.clone()
            };
            if let Some(home) = &cfg.codex_home {
                // As Codex reports a CODEX_HOME it canonicalised.
                codex.codex_home = format!(r"\\?\{}", home.display());
            }
            self.spawned.lock().unwrap().push(cfg.codex_home.clone());
            self.seen.lock().unwrap().push(Arc::clone(&codex.seen));
            Ok(codex.connect())
        }
    }

    fn sweep_text(cfg: &Config, launcher: &SweepLauncher, days: Option<u32>) -> (i32, String) {
        let mut out = Vec::new();
        let code = sweep(cfg, launcher, days, &mut out);
        (code, String::from_utf8(out).unwrap())
    }

    #[test]
    fn the_sweep_covers_every_project_store_and_starts_one_child_per_codex_home() {
        let dir = temp_dir("cleanup-sweep");
        let cfg = cfg(&dir, &["--cleanup"]);
        let state = &cfg.state_base;
        // Two projects. The ambient home is the fake's default; a dedicated one is a real folder.
        let ambient = PathBuf::from(CODEX_HOME);
        let dedicated = dir.join("dedicated-home");
        fs::create_dir_all(dedicated.join("generated_images").join(OTHER_THREAD)).unwrap();
        fs::write(
            dedicated
                .join("generated_images")
                .join(OTHER_THREAD)
                .join("a.png"),
            b"png",
        )
        .unwrap();
        let project_a = Store::new(&state.join("a-0000000000000001"));
        let project_b = Store::new(&state.join("b-0000000000000002"));
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();
        let add = |store: &Store, name: &str, thread: &str, home: &Path, updated: i64| {
            let path = out.join(output::file_name(name, 1));
            fs::write(&path, b"12345").unwrap();
            let record = Record {
                name: name.to_string(),
                thread_id: thread.to_string(),
                codex_home: home.to_path_buf(),
                model: "m".to_string(),
                created: updated,
                updated,
                turns: 1,
                last_saved_path: None,
                last_output_path: Some(path.clone()),
                last_output_bytes: Some(5),
                output_dir: out.clone(),
                next_version: 2,
                outputs: vec![Output {
                    version: 1,
                    resolved_path: Some(fs::canonicalize(&path).unwrap()),
                    path,
                    bytes: 5,
                    fnv1a64: Some(Output::fingerprint_text(Fnv1a64::of(b"12345"))),
                }],
            };
            store
                .update(|file| {
                    file.sessions.insert(session::key(name), record);
                    Ok(())
                })
                .unwrap();
        };
        add(&project_a, "fox", THREAD, &ambient, OLD);
        add(&project_b, "owl", OTHER_THREAD, &dedicated, OLD);
        add(
            &project_b,
            "fresh",
            "019a0000-0000-7000-8000-0000000fe5e5",
            &ambient,
            session::now_unix(),
        );

        let launcher = SweepLauncher::new(FakeCodex::default());
        let (code, text) = sweep_text(&cfg, &launcher, None);
        assert_eq!(code, 0, "{text}");
        assert!(
            text.contains("sessions idle over 7 days, in 2 project store(s)"),
            "{text}"
        );
        assert!(text.contains("removed a-0000000000000001: fox"), "{text}");
        assert!(text.contains("removed b-0000000000000002: owl"), "{text}");
        assert!(text.contains("done: removed 2 sessions"), "{text}");
        // One default child, one for the dedicated home, each deleting its own thread.
        assert_eq!(launcher.spawns.load(Ordering::SeqCst), 2);
        let mut deletes = launcher.deletes();
        deletes.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(
            deletes,
            vec![
                (None, THREAD.to_string()),
                (Some(dedicated.clone()), OTHER_THREAD.to_string())
            ]
        );
        assert!(!dedicated
            .join("generated_images")
            .join(OTHER_THREAD)
            .exists());
        assert!(!out.join("fox-v1.png").exists() && !out.join("owl-v1.png").exists());
        assert!(out.join("fresh-v1.png").exists());
        let a = project_a.read().unwrap();
        let b = project_b.read().unwrap();
        assert!(a.sessions.is_empty());
        assert_eq!(b.sessions.keys().collect::<Vec<_>>(), vec!["fresh"]);
        assert_eq!(a.last_cleanup.as_ref().unwrap().removed, 1);
        assert_eq!(b.last_cleanup.as_ref().unwrap().removed, 1);

        // 0: every session not in use.
        let (code, text) = sweep_text(&cfg, &launcher, Some(0));
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("every session not in use"), "{text}");
        assert!(project_b.read().unwrap().sessions.is_empty(), "{text}");
    }

    #[test]
    fn the_sweep_reports_an_unreadable_store_and_a_home_it_cannot_reach() {
        let dir = temp_dir("cleanup-sweep");
        let cfg = cfg(&dir, &["--cleanup"]);
        let broken = cfg.state_base.join("broken-0000000000000003");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(STORE_FILE), b"{ not json").unwrap();
        let ok = Store::new(&cfg.state_base.join("ok-0000000000000004"));
        let world = World::new();
        let mut record = world.session("fox", THREAD, OLD, &[1]);
        // A home that reports another path when started: nothing is deleted there.
        record.codex_home = dir.join("moved-home");
        ok.update(|file| {
            file.sessions.insert("fox".to_string(), record.clone());
            Ok(())
        })
        .unwrap();
        let launcher = SweepLauncher::new(FakeCodex {
            ..FakeCodex::default()
        });
        struct Liar<'a>(&'a SweepLauncher);
        impl Launcher for Liar<'_> {
            fn resolve(&self, cfg: &Config) -> Result<PathBuf, Failure> {
                self.0.resolve(cfg)
            }
            fn spawn(&self, cfg: &Config, bin: &Path) -> Result<AppServer, Failure> {
                // Always the default home, whatever CODEX_HOME says.
                self.0.spawn(
                    &Config {
                        codex_home: None,
                        ..cfg.clone()
                    },
                    bin,
                )
            }
        }
        let mut out = Vec::new();
        let code = sweep(&cfg, &Liar(&launcher), Some(0), &mut out);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(code, 1, "{text}");
        assert!(
            text.contains("skipped a store: the session store"),
            "{text}"
        );
        assert!(
            text.contains("skipped ok-0000000000000004: fox: Codex started with CODEX_HOME="),
            "{text}"
        );
        assert!(launcher.deletes().is_empty());
        assert!(record.outputs[0].path.exists());
        assert_eq!(fs::read(broken.join(STORE_FILE)).unwrap(), b"{ not json");
        assert_eq!(ok.read().unwrap().last_cleanup.unwrap().skipped.len(), 1);
    }

    #[test]
    fn a_project_folder_that_cannot_be_searched_is_reported_and_fails_the_sweep() {
        let dir = temp_dir("cleanup-sweep");
        let cfg = cfg(&dir, &["--cleanup"]);
        fs::create_dir_all(&cfg.state_base).unwrap();
        // A project folder whose store cannot even be looked for: a junction to itself, which
        // Windows cannot resolve. It must not read as a project with no store.
        let looped = cfg.state_base.join("looped-0000000000000006");
        make_junction(&looped, &looped);
        // A readable project beside it, with nothing due, and a stray file, which is no project.
        Store::new(&cfg.state_base.join("ok-0000000000000007"))
            .update(|_| Ok(()))
            .unwrap();
        fs::write(cfg.state_base.join("notes.txt"), b"x").unwrap();
        let (stores, unsearched) = project_stores(&cfg);
        assert_eq!(stores.len(), 1);
        assert_eq!(unsearched.len(), 1, "{unsearched:?}");
        assert!(
            unsearched[0].contains("looped-0000000000000006"),
            "{unsearched:?}"
        );

        let launcher = SweepLauncher::new(FakeCodex::default());
        let (code, text) = sweep_text(&cfg, &launcher, Some(0));
        assert_eq!(code, 1, "{text}");
        assert!(
            text.contains("could not search for project stores: could not read"),
            "{text}"
        );
        assert!(
            text.contains("nothing to remove in the stores that could be read"),
            "{text}"
        );
        assert!(
            text.contains("; 1 place(s) could not be searched for project stores"),
            "{text}"
        );

        // A state base that cannot be listed at all (here, a file) is a failure too.
        let file = dir.join("a-file");
        fs::write(&file, b"x").unwrap();
        let unlistable = Config {
            state_base: file,
            ..cfg.clone()
        };
        let (code, text) = sweep_text(&unlistable, &launcher, Some(0));
        assert_eq!(code, 1, "{text}");
        assert!(text.contains("could not list"), "{text}");

        // One that does not exist is genuinely empty.
        let missing = Config {
            state_base: dir.join("missing"),
            ..cfg.clone()
        };
        let (code, text) = sweep_text(&missing, &launcher, Some(0));
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("\nnothing to remove\n"), "{text}");
        assert_eq!(launcher.spawns.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_sweep_does_nothing_when_expiry_is_off_and_no_age_is_given() {
        let dir = temp_dir("cleanup-sweep");
        let cfg = cfg(&dir, &["--cleanup", "--session-ttl-days", "0"]);
        let launcher = SweepLauncher::new(FakeCodex::default());
        let (code, text) = sweep_text(&cfg, &launcher, None);
        assert_eq!(code, 0);
        assert!(text.contains("expiry is off"), "{text}");
        assert_eq!(launcher.spawns.load(Ordering::SeqCst), 0);
        // Nothing due: no child at all, and the outcome is still recorded.
        let store = Store::new(&cfg.state_base.join("p-0000000000000005"));
        store.update(|_| Ok(())).unwrap();
        let (code, text) = sweep_text(&cfg, &launcher, Some(3));
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("nothing to remove"), "{text}");
        assert_eq!(launcher.spawns.load(Ordering::SeqCst), 0);
        assert_eq!(store.read().unwrap().last_cleanup.unwrap().removed, 0);
    }
}
