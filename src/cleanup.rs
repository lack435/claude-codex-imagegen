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
//! - In Codex's image folder only `*.png` files are removed, then the folder once it is empty, and
//!   only when it is a plain folder rather than a link to somewhere else.
//! - An output is deleted only while its exact path is a plain file with the recorded size, under
//!   the name it was published with. A file that was edited, replaced or renamed is left alone. No
//!   folder in an output directory is ever removed, and nothing is matched by wildcard there.
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

/// Delete Codex's copies of the thread's images: the `*.png` files in
/// `<codex_home>\generated_images\<thread_id>\`, then the folder once it is empty. `thread/delete`
/// leaves it behind [verified: live, V8].
fn remove_codex_images(
    codex_home: &Path,
    thread_id: &str,
    freed: &mut u64,
    problems: &mut Vec<String>,
) {
    let dir = codex_home.join("generated_images").join(thread_id);
    let meta = match fs::symlink_metadata(&dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return,
        Err(e) => {
            problems.push(format!("could not read {} ({e})", dir.display()));
            return;
        }
    };
    if !meta.is_dir() || !is_plain(&meta) {
        problems.push(format!(
            "{} is not a plain folder, so it was left alone",
            dir.display()
        ));
        return;
    }
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) => {
            problems.push(format!("could not list {} ({e})", dir.display()));
            return;
        }
    };
    let before = problems.len();
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
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if !is_png || !meta.is_file() || !is_plain(&meta) {
            continue;
        }
        match output::retry_transient(|| fs::remove_file(&path)) {
            Ok(()) => *freed += meta.len(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => problems.push(format!("could not delete {} ({e})", path.display())),
        }
    }
    if problems.len() == before {
        match fs::remove_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            // Something other than a PNG is in it: not ours to delete, and no reason to keep the
            // session.
            Err(e) => eprintln!("codex-imagegen: cleanup: left {} ({e})", dir.display()),
        }
    }
}

/// Delete the files this server published for the session, each only while its exact path is a
/// plain file with the recorded size under its published name.
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
        match fs::symlink_metadata(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => problems.push(format!("could not read {} ({e})", path.display())),
            Ok(meta) if meta.is_file() && is_plain(&meta) && meta.len() == published.bytes => {
                match output::retry_transient(|| fs::remove_file(path)) {
                    Ok(()) => *freed += published.bytes,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => problems.push(format!("could not delete {} ({e})", path.display())),
                }
            }
            Ok(_) => eprintln!(
                "codex-imagegen: cleanup: kept {}: it changed after it was published",
                path.display()
            ),
        }
    }
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
/// Prints what it removed and skipped to `out`. Returns the exit code: 0 when every store could be
/// read and every home reached (sessions skipped because they are in use are normal), 1 otherwise.
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
    let stores = project_stores(cfg);
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

    let now = session::now_unix();
    let mut failed = false;
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
        let _ = writeln!(out, "nothing to remove");
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
        "done: removed {}, freed {}{}, skipped {}",
        sessions(total.removed.len()),
        megabytes(total.freed),
        if total.removed.is_empty() {
            ""
        } else {
            " (not counting the Codex rollouts thread/delete removed)"
        },
        total.skipped.len()
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
/// plus `--state-dir`'s when it lies elsewhere.
fn project_stores(cfg: &Config) -> Vec<Store> {
    let mut dirs: Vec<PathBuf> = match fs::read_dir(&cfg.state_base) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|dir| dir.join(STORE_FILE).is_file())
            .collect(),
        Err(_) => Vec::new(),
    };
    if cfg.state_dir.join(STORE_FILE).is_file()
        && !dirs.iter().any(|dir| same_path(dir, &cfg.state_dir))
    {
        dirs.push(cfg.state_dir.clone());
    }
    dirs.sort();
    dirs.iter().map(|dir| Store::new(dir)).collect()
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
    use crate::session::{Output, StoreFile};
    use crate::testutil::{temp_dir, TempDir};
    use crate::tools::testing::cfg;
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
                    path,
                    bytes: bytes.len() as u64,
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
            skipped[0].why.contains("retried next time"),
            "{:?}",
            skipped
        );
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
                    path,
                    bytes: 5,
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
