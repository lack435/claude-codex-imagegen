//! Where images go, and how they get there without ever replacing anything (docs/design.md,
//! "Output files").
//!
//! - The output directory, resolved in the design's order.
//! - The pre-check, before anything is spent: the directory is created, and a probe file is
//!   created in it and deleted.
//! - Publishing a completed image as `<session>-v<N>.png`: written to a temp file in the
//!   destination directory, then renamed with `MoveFileExW` *without* `MOVEFILE_REPLACE_EXISTING`.
//!   When the name is taken, N is bumped and the rename retried, so an existing file is never
//!   overwritten, even when several projects or processes share a folder.
//! - Automatic session names, from local time.
//!
//! Writing into the caller's output directory is one of the places rigor belongs (AGENTS.md). This
//! module creates only its probe file, its temp file and the published file; it deletes only the
//! probe and temp files it created itself; and it never replaces or deletes anything else.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::codex::SystemTime;
use crate::errors::{self, Failure};

/// The default folder under the project, when Claude Code sets `CLAUDE_PROJECT_DIR` [decided].
pub const PROJECT_DIR_NAME: &str = "generated-images";

/// How many names past the first a publish tries before giving up. Only reachable when a folder
/// already holds thousands of versions of one session name; the bound keeps a bug from looping.
const MAX_VERSION_BUMPS: u32 = 10_000;

/// Waits before retrying a rename refused with a sharing or access error. A file this process has
/// just written is often opened by an antivirus scanner for a moment, and the rename fails until it
/// lets go. About 0.8 s in all, then the error stands.
const RENAME_RETRY_WAITS_MS: [u64; 5] = [25, 50, 100, 200, 400];

const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SHARING_VIOLATION: i32 = 32;
const ERROR_LOCK_VIOLATION: i32 = 33;
const ERROR_FILE_EXISTS: i32 = 80;
const ERROR_ALREADY_EXISTS: i32 = 183;

extern "system" {
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    fn GetLocalTime(time: *mut SystemTime);
}

/// Unique temp and probe names within this process; the pid makes them unique across processes.
static SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// The directory
// ---------------------------------------------------------------------------

/// Which rule chose the output directory. Said in the pre-check's error, so the agent knows
/// whether passing `output_dir` would help.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirSource {
    /// The call's `output_dir`.
    Argument,
    /// For refine, the output folder its session was created with.
    Recorded,
    /// The server's `--output-dir`.
    Flag,
    /// `%CLAUDE_PROJECT_DIR%\generated-images`.
    Project,
    /// `<per-project state dir>\images`, when `CLAUDE_PROJECT_DIR` is not set.
    State,
}

impl DirSource {
    fn describe(self) -> &'static str {
        match self {
            Self::Argument => "from the output_dir argument",
            Self::Recorded => "the session's own output folder, recorded when it was created",
            Self::Flag => "from the server's --output-dir flag",
            Self::Project => "the default: the project's generated-images folder",
            Self::State => {
                "the default: codex-imagegen's per-project state folder, because \
                 CLAUDE_PROJECT_DIR is not set"
            }
        }
    }
}

/// An absolute output directory and the rule that chose it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDir {
    pub path: PathBuf,
    pub source: DirSource,
}

/// The output directory: the first of the call's `output_dir`, the server's `--output-dir`,
/// `<project_dir>\generated-images`, and `<state_dir>\images` that applies. (Refine uses its
/// session's recorded directory, [`DirSource::Recorded`], when the call names none.)
///
/// `project_dir` is `CLAUDE_PROJECT_DIR` when Claude Code sets it to an absolute path. A relative
/// argument or flag resolves against it, and otherwise against `cwd`, the server's working
/// directory: so one user-scope registration gives every project its own folder. The result is
/// absolute and lexically normalised, because it is shown to the agent and written into.
pub fn resolve_dir(
    argument: Option<&str>,
    flag: Option<&Path>,
    project_dir: Option<&Path>,
    cwd: &Path,
    state_dir: &Path,
) -> OutputDir {
    let base = project_dir.unwrap_or(cwd);
    let (joined, source) = match (argument, flag, project_dir) {
        (Some(dir), _, _) => (base.join(dir), DirSource::Argument),
        (None, Some(dir), _) => (base.join(dir), DirSource::Flag),
        (None, None, Some(project)) => (project.join(PROJECT_DIR_NAME), DirSource::Project),
        (None, None, None) => (state_dir.join("images"), DirSource::State),
    };
    let path = std::path::absolute(&joined).unwrap_or(joined);
    OutputDir { path, source }
}

/// Create the directory and prove a file can be created in it, before anything is spent. Any
/// failure is BAD_REQUEST naming the path (docs/design.md, "Pre-check").
///
/// It cannot promise the publish will work -- the folder can change in the minute a generation
/// takes -- which is why a failed copy is a warning on a success, not a lost image.
pub fn precheck(dir: &OutputDir) -> Result<(), Failure> {
    let shown = dir.path.display();
    let source = dir.source.describe();
    let rejected = |what: String| {
        errors::bad_request(format!(
            "The output directory '{shown}' ({source}) {what}. Pass an output_dir that \
             codex-imagegen can create files in."
        ))
    };
    fs::create_dir_all(&dir.path).map_err(|e| rejected(format!("cannot be created ({e})")))?;
    let probe = dir.path.join(format!(
        ".codex-imagegen-probe.{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    // create_new: the probe must be a file this call created, since it is deleted next.
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|e| rejected(format!("does not accept new files ({e})")))?;
    fs::remove_file(&probe).map_err(|e| {
        rejected(format!(
            "accepted a new file but would not let codex-imagegen delete it again ({e}); the \
             file {} may be left behind",
            probe.display()
        ))
    })
}

// ---------------------------------------------------------------------------
// Publishing
// ---------------------------------------------------------------------------

/// A published image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    pub path: PathBuf,
    /// The N actually used, which is `first_version` unless that name was taken.
    pub version: u32,
}

/// `<session>-v<N>.png`.
pub fn file_name(session: &str, version: u32) -> String {
    format!("{session}-v{version}.png")
}

/// Publish `image`, the exact bytes of Codex's PNG, as `<dir>\<session>-v<N>.png` for the first
/// free N from `first_version` on. Never overwrites: a taken name bumps N.
///
/// The bytes go to `.<session>-v<N>.<pid>-<seq>.tmp` in the same directory first and are flushed
/// to disk, then that file is renamed into place, so the published name never shows a partial
/// file. On any failure the temp file is removed and nothing else is touched.
pub fn publish(
    dir: &Path,
    session: &str,
    first_version: u32,
    image: &[u8],
) -> io::Result<Published> {
    // The name becomes part of a path in the caller's folder. Callers pass validated session names
    // (`[A-Za-z0-9._-]{1,64}`), so this never fires; it keeps a slip elsewhere from writing outside
    // `dir`.
    let safe = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-');
    if session.is_empty() || !session.bytes().all(safe) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("'{session}' is not a valid session name"),
        ));
    }
    let temp = dir.join(format!(
        ".{session}-v{first_version}.{}-{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    write_new(&temp, image)?;
    let published = rename_to_first_free(&temp, dir, session, first_version);
    if published.is_err() {
        // Ours: write_new created it with create_new.
        let _ = fs::remove_file(&temp);
    }
    published
}

/// Create `path`, which must not exist, holding `bytes`, flushed to disk. If anything fails after
/// the file was created, it is removed again. Also writes the session store's temp files.
pub fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if written.is_err() {
        let _ = fs::remove_file(path);
    }
    written
}

fn rename_to_first_free(
    temp: &Path,
    dir: &Path,
    session: &str,
    first_version: u32,
) -> io::Result<Published> {
    let last = first_version.saturating_add(MAX_VERSION_BUMPS);
    let mut version = first_version;
    let mut waits = RENAME_RETRY_WAITS_MS.iter();
    loop {
        let target = dir.join(file_name(session, version));
        match rename_no_replace(temp, &target) {
            Ok(()) => {
                return Ok(Published {
                    path: target,
                    version,
                })
            }
            Err(e) if name_taken(&e) => {
                if version >= last {
                    return Err(io::Error::other(format!(
                        "{} and the {MAX_VERSION_BUMPS} names after it are all taken",
                        file_name(session, first_version)
                    )));
                }
                version += 1;
            }
            Err(e) if transient(&e) => match waits.next() {
                Some(ms) => std::thread::sleep(Duration::from_millis(*ms)),
                None => return Err(e),
            },
            Err(e) => return Err(e),
        }
    }
}

fn name_taken(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(ERROR_FILE_EXISTS | ERROR_ALREADY_EXISTS)
    )
}

fn transient(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
    )
}

/// Run `op`, retrying for about 0.8 s while it fails with a sharing or access error: another
/// process (typically an antivirus scanner) holding a file this process has just written. The
/// session store's reads, renames and lock files go through this.
pub fn retry_transient<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut waits = RENAME_RETRY_WAITS_MS.iter();
    loop {
        match op() {
            Err(e) if transient(&e) => match waits.next() {
                Some(ms) => std::thread::sleep(Duration::from_millis(*ms)),
                None => return Err(e),
            },
            done => return done,
        }
    }
}

/// Rename `from` to `to`, failing with ERROR_ALREADY_EXISTS or ERROR_FILE_EXISTS if `to` exists.
///
/// `MoveFileExW` with no flags: without `MOVEFILE_REPLACE_EXISTING` an existing target is an error,
/// never replaced, and that check and the rename are one filesystem operation, so nothing can
/// appear under the name in between. `std::fs::rename` cannot do this: on Windows it replaces.
/// Without `MOVEFILE_COPY_ALLOWED` it is a pure rename, which is all a temp file in the same
/// directory needs.
fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let from = wide(from);
    let to = wide(to);
    // SAFETY: both are NUL-terminated UTF-16 strings that outlive the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A path as a NUL-terminated UTF-16 string for a Win32 call.
///
/// A long absolute path gets the `\\?\` prefix, which lifts the 260-character MAX_PATH limit, as
/// the standard library does for its own calls: otherwise a deep folder would pass the pre-check,
/// which goes through the standard library, and then fail here. The prefix turns off Win32 path
/// normalisation, which is safe for these paths: `resolve_dir` already made the directory absolute
/// and normalised, and a file name is appended with a backslash.
fn wide(path: &Path) -> Vec<u16> {
    const VERBATIM_FROM: usize = 248;
    let raw: Vec<u16> = path.as_os_str().encode_wide().collect();
    let starts = |prefix: &str| raw.starts_with(&prefix.encode_utf16().collect::<Vec<u16>>());
    let drive_absolute = raw.len() >= 3 && raw[1] == u16::from(b':') && raw[2] == u16::from(b'\\');
    let mut out: Vec<u16> = if raw.len() < VERBATIM_FROM || starts(r"\\?\") || starts(r"\\.\") {
        raw
    } else if starts(r"\\") {
        r"\\?\UNC\"
            .encode_utf16()
            .chain(raw[2..].iter().copied())
            .collect()
    } else if drive_absolute {
        r"\\?\".encode_utf16().chain(raw).collect()
    } else {
        raw
    };
    out.push(0);
    out
}

// ---------------------------------------------------------------------------
// Session names
// ---------------------------------------------------------------------------

/// A fresh automatic session name, `img-<yyyyMMdd-HHmmss>-<4 hex>`, in local time
/// (docs/design.md, "codex_imagegen_generate"). The suffix mixes the clock's nanoseconds, the
/// process id and a per-process counter, so two processes naming a session in the same second
/// almost surely differ, and a retry after a collision always gets a different name.
pub fn auto_session_name() -> String {
    static NAMES: AtomicU64 = AtomicU64::new(0);
    let mut now = SystemTime::default();
    // SAFETY: GetLocalTime fills the SYSTEMTIME it is given and cannot fail.
    unsafe { GetLocalTime(&mut now) };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let salt = nanos
        ^ u64::from(std::process::id()).rotate_left(32)
        ^ NAMES
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
    session_name_at(&now, salt)
}

fn session_name_at(t: &SystemTime, salt: u64) -> String {
    format!(
        "img-{:04}{:02}{:02}-{:02}{:02}{:02}-{:04x}",
        t.year,
        t.month,
        t.day,
        t.hour,
        t.minute,
        t.second,
        (mix(salt) >> 48) as u16
    )
}

/// The splitmix64 finaliser: spreads every input bit over the output, so the suffix's 16 bits
/// depend on all of the salt.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;

    /// Every entry in `dir`, sorted.
    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn the_output_directory_follows_the_designs_order() {
        let project = Path::new(r"C:\work\proj");
        let cwd = Path::new(r"C:\server-cwd");
        let state = Path::new(r"C:\state\proj-0123");
        let flag = Path::new(r"D:\flag-images");

        let dir = resolve_dir(Some(r"E:\arg"), Some(flag), Some(project), cwd, state);
        assert_eq!(
            dir,
            OutputDir {
                path: PathBuf::from(r"E:\arg"),
                source: DirSource::Argument
            }
        );
        let dir = resolve_dir(None, Some(flag), Some(project), cwd, state);
        assert_eq!((dir.path.as_path(), dir.source), (flag, DirSource::Flag));
        let dir = resolve_dir(None, None, Some(project), cwd, state);
        assert_eq!(
            (dir.path, dir.source),
            (
                PathBuf::from(r"C:\work\proj\generated-images"),
                DirSource::Project
            )
        );
        let dir = resolve_dir(None, None, None, cwd, state);
        assert_eq!(
            (dir.path, dir.source),
            (
                PathBuf::from(r"C:\state\proj-0123\images"),
                DirSource::State
            )
        );
    }

    #[test]
    fn relative_directories_resolve_against_the_project_else_the_server_cwd() {
        let project = Path::new(r"C:\work\proj");
        let cwd = Path::new(r"C:\server-cwd");
        let state = Path::new(r"C:\state");
        let rel = |argument, flag: Option<&str>, project| {
            resolve_dir(argument, flag.map(Path::new), project, cwd, state).path
        };
        assert_eq!(
            rel(Some("out"), None, Some(project)),
            PathBuf::from(r"C:\work\proj\out")
        );
        assert_eq!(
            rel(Some("out"), None, None),
            PathBuf::from(r"C:\server-cwd\out")
        );
        assert_eq!(
            rel(None, Some(r"art\png"), Some(project)),
            PathBuf::from(r"C:\work\proj\art\png")
        );
        assert_eq!(
            rel(None, Some("images"), None),
            PathBuf::from(r"C:\server-cwd\images")
        );
        // Normalised: shown to the agent and written into, so no `..` or `.` survive.
        assert_eq!(
            rel(Some(r"..\sibling\.\out"), None, Some(project)),
            PathBuf::from(r"C:\work\sibling\out")
        );
        assert_eq!(
            rel(Some("a/b"), None, Some(project)),
            PathBuf::from(r"C:\work\proj\a\b")
        );
    }

    #[test]
    fn the_precheck_creates_the_directory_and_leaves_nothing_behind() {
        let root = temp_dir("output");
        let dir = OutputDir {
            path: root.join("new").join("nested"),
            source: DirSource::Argument,
        };
        precheck(&dir).unwrap();
        assert!(dir.path.is_dir());
        assert!(listing(&dir.path).is_empty(), "{:?}", listing(&dir.path));
        // An existing directory with files in it passes too, and keeps its files.
        fs::write(dir.path.join("keep.png"), b"x").unwrap();
        precheck(&dir).unwrap();
        assert_eq!(listing(&dir.path), vec!["keep.png"]);
    }

    #[test]
    fn a_directory_that_cannot_be_created_is_a_bad_request_naming_it() {
        let root = temp_dir("output");
        let file = root.join("a-file");
        fs::write(&file, b"not a folder").unwrap();
        for (path, source, source_text) in [
            (
                file.clone(),
                DirSource::Argument,
                "from the output_dir argument",
            ),
            (file.join("below"), DirSource::Flag, "--output-dir"),
        ] {
            let failure = precheck(&OutputDir {
                path: path.clone(),
                source,
            })
            .unwrap_err();
            assert_eq!(failure.code, "BAD_REQUEST");
            assert!(
                failure.summary.contains(&path.display().to_string()),
                "{}",
                failure.summary
            );
            assert!(failure.summary.contains(source_text), "{}", failure.summary);
            assert!(failure.summary.contains("cannot be created"));
            assert!(failure.remediation.contains("nothing was spent"));
        }
        assert_eq!(fs::read(&file).unwrap(), b"not a folder");
    }

    #[test]
    fn publishing_writes_v1_and_cleans_up_its_temp_file() {
        let dir = temp_dir("output");
        let published = publish(&dir, "fox", 1, b"png bytes").unwrap();
        assert_eq!(
            published,
            Published {
                path: dir.join("fox-v1.png"),
                version: 1
            }
        );
        assert_eq!(fs::read(&published.path).unwrap(), b"png bytes");
        assert_eq!(listing(&dir), vec!["fox-v1.png"]);
        // The next version, asked for explicitly.
        let next = publish(&dir, "fox", 2, b"second").unwrap();
        assert_eq!(next.version, 2);
        assert_eq!(listing(&dir), vec!["fox-v1.png", "fox-v2.png"]);
    }

    #[test]
    fn a_taken_name_bumps_the_version_and_is_never_overwritten() {
        let dir = temp_dir("output");
        fs::write(dir.join("fox-v1.png"), b"someone else's v1").unwrap();
        fs::write(dir.join("fox-v2.png"), b"someone else's v2").unwrap();
        // A folder in the way counts as taken too.
        fs::create_dir(dir.join("fox-v3.png")).unwrap();
        let published = publish(&dir, "fox", 1, b"ours").unwrap();
        assert_eq!(published.version, 4);
        assert_eq!(published.path, dir.join("fox-v4.png"));
        assert_eq!(
            fs::read(dir.join("fox-v1.png")).unwrap(),
            b"someone else's v1"
        );
        assert_eq!(
            fs::read(dir.join("fox-v2.png")).unwrap(),
            b"someone else's v2"
        );
        assert!(dir.join("fox-v3.png").is_dir());
        assert_eq!(fs::read(&published.path).unwrap(), b"ours");
        assert_eq!(
            listing(&dir),
            vec!["fox-v1.png", "fox-v2.png", "fox-v3.png", "fox-v4.png"]
        );
    }

    #[test]
    fn concurrent_publishers_each_get_their_own_name() {
        // Several processes sharing a folder, modelled as threads: every image survives, each
        // under a different name.
        let dir = temp_dir("output");
        let results: Vec<Published> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..8u8)
                .map(|i| {
                    let dir = &dir;
                    s.spawn(move || publish(dir, "shared", 1, &[i; 1000]).unwrap())
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut versions: Vec<u32> = results.iter().map(|p| p.version).collect();
        versions.sort();
        assert_eq!(versions, (1..=8).collect::<Vec<u32>>());
        for (i, published) in results.iter().enumerate() {
            assert_eq!(fs::read(&published.path).unwrap(), vec![i as u8; 1000]);
        }
        assert_eq!(listing(&dir).len(), 8, "{:?}", listing(&dir));
    }

    #[test]
    fn a_publish_that_fails_leaves_nothing_behind() {
        let root = temp_dir("output");
        let missing = root.join("gone");
        let err = publish(&missing, "fox", 1, b"x").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert!(listing(&root).is_empty());
        // A name that could reach outside the folder is refused before anything is written.
        for bad in ["", r"..\escape", "a/b", "c:d"] {
            let err = publish(&root, bad, 1, b"x").unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
        assert!(listing(&root).is_empty());
        assert!(!root.parent().unwrap().join("escape-v1.png").exists());
    }

    #[test]
    fn a_deep_directory_past_max_path_publishes_like_any_other() {
        let root = temp_dir("output");
        let mut deep = root.to_path_buf();
        while deep.as_os_str().len() < 300 {
            deep.push("a-rather-long-folder-name");
        }
        let dir = resolve_dir(Some(deep.to_str().unwrap()), None, None, &root, &root);
        precheck(&dir).unwrap();
        fs::write(dir.path.join("deep-v1.png"), b"taken").unwrap();
        let published = publish(&dir.path, "deep", 1, b"deep bytes").unwrap();
        assert_eq!(published.version, 2);
        assert_eq!(fs::read(&published.path).unwrap(), b"deep bytes");
        assert_eq!(listing(&dir.path), vec!["deep-v1.png", "deep-v2.png"]);
    }

    #[test]
    fn wide_paths_get_the_verbatim_prefix_only_when_long() {
        let text = |p: &str| String::from_utf16(&wide(Path::new(p))[..]).unwrap();
        assert_eq!(text(r"C:\short\x.png"), "C:\\short\\x.png\0");
        let long = format!(r"C:\{}\x.png", "d".repeat(260));
        assert_eq!(text(&long), format!("\\\\?\\{long}\0"));
        let unc = format!(r"\\server\share\{}", "d".repeat(260));
        assert_eq!(
            text(&unc),
            format!("\\\\?\\UNC\\server\\share\\{}\0", "d".repeat(260))
        );
        let verbatim = format!(r"\\?\C:\{}", "d".repeat(260));
        assert_eq!(text(&verbatim), format!("{verbatim}\0"));
    }

    #[test]
    fn automatic_session_names_are_valid_local_time_stamps() {
        let t = SystemTime {
            year: 2026,
            month: 9,
            day_of_week: 5,
            day: 5,
            hour: 7,
            minute: 3,
            second: 9,
            milliseconds: 0,
        };
        let name = session_name_at(&t, 42);
        assert!(name.starts_with("img-20260905-070309-"), "{name}");
        assert_eq!(name.len(), "img-20260905-070309-abcd".len());
        assert_ne!(session_name_at(&t, 43), name, "the suffix follows the salt");

        let a = auto_session_name();
        let b = auto_session_name();
        assert_ne!(a, b, "a retry must get a different name");
        for name in [&a, &b] {
            let parts: Vec<&str> = name.split('-').collect();
            assert_eq!(parts.len(), 4, "{name}");
            assert_eq!(parts[0], "img");
            assert!(parts[1].len() == 8 && parts[1].bytes().all(|c| c.is_ascii_digit()));
            assert!(parts[2].len() == 6 && parts[2].bytes().all(|c| c.is_ascii_digit()));
            assert!(parts[3].len() == 4 && parts[3].bytes().all(|c| c.is_ascii_hexdigit()));
            // Within the session-name rules, so it can be passed back to refine as is.
            assert!(name.len() <= 64);
            assert!(name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-')));
        }
        assert!(a[4..8].parse::<u32>().unwrap() >= 2026, "{a}");
    }

    #[test]
    fn file_names_follow_the_design() {
        assert_eq!(file_name("fox-watercolor", 2), "fox-watercolor-v2.png");
    }
}
