//! Command-line configuration and the state directories derived from it.
//!
//! Everything is a flag on the MCP registration; there is no config file of our own
//! (docs/design.md, "Configuration"). Parsing is strict: an unknown, repeated or malformed flag
//! is an error and the process exits 2, because a typo that silently fell back to a default would
//! run Codex with settings the user did not ask for.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The agent model, pinned by full id: aliases move (AGENTS.md).
pub const DEFAULT_MODEL: &str = "gpt-6-astra";
/// The agent only relays the prompt to the image tool, so it needs little reasoning [decided].
pub const DEFAULT_EFFORT: &str = "low";
/// Whole-call budget for generate/refine. A turn takes 37-41 s [verified: smoke logs], so this
/// leaves room for a slow backend and a cold Codex start.
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Below this a normal turn would time out, so a smaller value is a mistake, not a preference.
pub const MIN_TIMEOUT_SECS: u64 = 30;
pub const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
pub const DEFAULT_MAX_CONCURRENT: u32 = 4;
pub const DEFAULT_SESSION_TTL_DAYS: u32 = 7;
/// Upper bound for the day counts, so later arithmetic on them (days to seconds, added to a Unix
/// time) can never overflow. Ten years is past any useful expiry.
pub const MAX_DAYS: u32 = 3650;

/// What the process was asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Serve MCP on stdio (the default).
    Serve,
    /// Print the status report from a terminal, then exit.
    Doctor,
    /// Sweep expired sessions, then exit. `older_than_days` overrides the TTL.
    Cleanup {
        older_than_days: Option<u32>,
    },
    Help,
    Version,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub mode: Mode,
    /// `--codex-bin`: absolute, checked at parse time. Whether it exists is checked only when the
    /// Codex child is first needed, so a missing file surfaces as an in-band CLI_NOT_FOUND rather
    /// than a server that never starts.
    pub codex_bin: Option<PathBuf>,
    /// `--codex-home`: CODEX_HOME for the child. Absolute.
    pub codex_home: Option<PathBuf>,
    pub model: String,
    pub effort: String,
    /// `--output-dir`, exactly as given; a relative path is resolved per call.
    pub output_dir: Option<PathBuf>,
    pub timeout: Duration,
    pub max_concurrent: u32,
    /// Sessions idle this many days expire. 0 disables expiry.
    pub session_ttl_days: u32,
    /// This process's working directory, normalized. The per-project state key is derived from it.
    pub cwd: PathBuf,
    /// `%CODEX_IMAGEGEN_HOME%`, else `%USERPROFILE%\.codex-imagegen`.
    pub state_base: PathBuf,
    /// The per-project state directory: `--state-dir`, else `<base>\<leaf>-<hash>`.
    pub state_dir: PathBuf,
    /// The Codex child's working directory: `<base>\work`, empty and outside any repository, so no
    /// project AGENTS.md, config or trust entry comes into play (docs/design.md, "Spawn").
    pub work_dir: PathBuf,
}

/// The parts of the process environment configuration depends on, gathered in one place so tests
/// can supply their own without mutating process-global state.
#[derive(Clone, Debug)]
pub struct Env {
    pub cwd: PathBuf,
    pub imagegen_home: Option<OsString>,
    pub user_profile: Option<OsString>,
}

impl Env {
    pub fn from_process() -> Result<Self, String> {
        Ok(Self {
            cwd: std::env::current_dir()
                .map_err(|e| format!("cannot determine the working directory: {e}"))?,
            imagegen_home: std::env::var_os("CODEX_IMAGEGEN_HOME"),
            user_profile: std::env::var_os("USERPROFILE"),
        })
    }
}

impl Config {
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        Self::parse(args, &Env::from_process()?)
    }

    pub fn parse(args: &[String], env: &Env) -> Result<Self, String> {
        let mut seen: Vec<&str> = Vec::new();
        let mut codex_bin = None;
        let mut codex_home = None;
        let mut model = None;
        let mut effort = None;
        let mut output_dir = None;
        let mut state_dir = None;
        let mut timeout_secs = DEFAULT_TIMEOUT_SECS;
        let mut max_concurrent = DEFAULT_MAX_CONCURRENT;
        let mut session_ttl_days = DEFAULT_SESSION_TTL_DAYS;
        let mut older_than_days = None;
        let (mut doctor, mut cleanup, mut help, mut version) = (false, false, false, false);

        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            // Accept both `--flag value` and `--flag=value`.
            let (key, inline) = match arg.split_once('=') {
                Some((k, v)) if k.starts_with("--") => (k, Some(v)),
                _ => (arg, None),
            };
            let canonical = match key {
                "-h" => "--help",
                "-V" => "--version",
                other => other,
            };
            if !FLAGS.contains(&canonical) {
                return Err(format!("unknown argument '{arg}' (try --help)"));
            }
            if seen.contains(&canonical) {
                return Err(format!("{canonical} was given more than once"));
            }
            seen.push(canonical);

            if SWITCHES.contains(&canonical) {
                if inline.is_some() {
                    return Err(format!("{canonical} takes no value"));
                }
                match canonical {
                    "--doctor" => doctor = true,
                    "--cleanup" => cleanup = true,
                    "--help" => help = true,
                    _ => version = true,
                }
                i += 1;
                continue;
            }

            let value = match inline {
                Some(v) => v,
                None => {
                    i += 1;
                    match args.get(i) {
                        // A value that is itself a flag means the real value was left out:
                        // `--codex-bin --doctor` must not run with a binary named "--doctor".
                        Some(v) if !v.starts_with("--") => v.as_str(),
                        _ => return Err(format!("{canonical} requires a value")),
                    }
                }
            };
            if value.is_empty() {
                return Err(format!("{canonical} requires a value"));
            }

            match canonical {
                "--codex-bin" => codex_bin = Some(absolute_path(canonical, value)?),
                "--codex-home" => codex_home = Some(absolute_dir(canonical, value)?),
                "--model" => model = Some(model_id(value)?),
                "--effort" => effort = Some(effort_level(value)?),
                "--output-dir" => output_dir = Some(PathBuf::from(value)),
                "--state-dir" => state_dir = Some(absolute_dir(canonical, value)?),
                "--timeout-seconds" => {
                    timeout_secs = number(canonical, value, MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)?
                }
                "--max-concurrent" => {
                    max_concurrent = number(canonical, value, 1, u32::MAX as u64)? as u32
                }
                "--session-ttl-days" => {
                    session_ttl_days = number(canonical, value, 0, MAX_DAYS as u64)? as u32
                }
                "--older-than-days" => {
                    older_than_days = Some(number(canonical, value, 0, MAX_DAYS as u64)? as u32)
                }
                other => unreachable!("{other} is in FLAGS but has no handler"),
            }
            i += 1;
        }

        if older_than_days.is_some() && !cleanup {
            return Err("--older-than-days is only valid with --cleanup".into());
        }
        let mode = if help {
            Mode::Help
        } else if version {
            Mode::Version
        } else if doctor && cleanup {
            return Err("--doctor and --cleanup cannot be combined".into());
        } else if doctor {
            Mode::Doctor
        } else if cleanup {
            Mode::Cleanup { older_than_days }
        } else {
            Mode::Serve
        };

        let cwd = normalize_dir(env.cwd.clone());
        let state_base = state_base(env)?;
        let state_dir = state_dir.unwrap_or_else(|| default_state_dir(&state_base, &cwd));
        let work_dir = state_base.join("work");

        Ok(Self {
            mode,
            codex_bin,
            codex_home,
            model: model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            effort: effort.unwrap_or_else(|| DEFAULT_EFFORT.to_string()),
            output_dir,
            timeout: Duration::from_secs(timeout_secs),
            max_concurrent,
            session_ttl_days,
            cwd,
            state_base,
            state_dir,
            work_dir,
        })
    }
}

/// Every flag, in its canonical spelling.
const FLAGS: &[&str] = &[
    "--codex-bin",
    "--codex-home",
    "--model",
    "--effort",
    "--output-dir",
    "--state-dir",
    "--timeout-seconds",
    "--max-concurrent",
    "--session-ttl-days",
    "--older-than-days",
    "--doctor",
    "--cleanup",
    "--help",
    "--version",
];

/// The flags that take no value.
const SWITCHES: &[&str] = &["--doctor", "--cleanup", "--help", "--version"];

fn absolute_dir(flag: &str, value: &str) -> Result<PathBuf, String> {
    absolute(flag, value, r"D:\codex-imagegen")
}

/// `--codex-bin` must be absolute for a sharper reason than the directories: Claude Code starts
/// the server in each project's directory, so a relative binary would resolve inside whichever
/// repository is open, and a repository holding a file by that name would get it run with the
/// user's Codex login.
fn absolute_path(flag: &str, value: &str) -> Result<PathBuf, String> {
    absolute(flag, value, r"C:\tools\codex.exe")
}

/// A relative path would resolve against whichever directory each Claude Code window launched us
/// from, so two windows would silently use different ones. On Windows `is_absolute` also refuses
/// the drive-relative `C:x` and the root-relative `\x`, which depend on the current drive or its
/// current directory.
fn absolute(flag: &str, value: &str, example: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!(
            "{flag} requires an absolute path (for example {example}), got '{value}'"
        ));
    }
    Ok(path)
}

fn number(flag: &str, value: &str, min: u64, max: u64) -> Result<u64, String> {
    let n: u64 = value
        .parse()
        .map_err(|_| format!("{flag} must be a whole number, got '{value}'"))?;
    if n < min || n > max {
        return Err(format!("{flag} must be between {min} and {max}, got {n}"));
    }
    Ok(n)
}

/// A model id is passed to Codex as-is. Checked only for shape, since which ids exist is Codex's
/// to say (preflight checks the pinned one against `model/list`).
fn model_id(value: &str) -> Result<String, String> {
    let ok = value.chars().count() <= 128
        && value.chars().all(|c| !c.is_whitespace() && !c.is_control());
    if !ok {
        return Err(format!(
            "--model must be a model id such as {DEFAULT_MODEL} (no spaces), got '{value}'"
        ));
    }
    Ok(value.to_string())
}

/// Codex types reasoning effort as a free string, so only the shape is checked here.
fn effort_level(value: &str) -> Result<String, String> {
    let ok = value.len() <= 32
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(format!(
            "--effort must be a reasoning effort such as low, medium or high, got '{value}'"
        ));
    }
    Ok(value.to_string())
}

/// `%CODEX_IMAGEGEN_HOME%`, else `%USERPROFILE%\.codex-imagegen` [decided].
///
/// Deliberately not under `%LOCALAPPDATA%`, which packaged (MSIX) hosts redirect, splitting the
/// state between two views of the disk. Must be absolute: a relative base would resolve against
/// each process's launch directory, and two windows would key their state differently.
fn state_base(env: &Env) -> Result<PathBuf, String> {
    if let Some(home) = env.imagegen_home.as_ref().filter(|h| !h.is_empty()) {
        let path = PathBuf::from(home);
        if !path.is_absolute() {
            return Err(format!(
                "CODEX_IMAGEGEN_HOME must be an absolute path, got '{}'",
                path.display()
            ));
        }
        return Ok(path);
    }
    match env.user_profile.as_ref().map(PathBuf::from) {
        Some(profile) if profile.is_absolute() => Ok(profile.join(".codex-imagegen")),
        _ => Err(
            "cannot place codex-imagegen's state: USERPROFILE is not an absolute path. \
                  Set CODEX_IMAGEGEN_HOME to an absolute directory."
                .into(),
        ),
    }
}

/// Resolve a directory to an absolute path without the `\\?\` verbatim prefix that `canonicalize`
/// adds on Windows. That prefix is correct but ends up in paths shown to the user and passed to
/// Codex, where some tools mishandle it. A directory that cannot be canonicalized is kept as
/// given.
pub fn normalize_dir(dir: PathBuf) -> PathBuf {
    let resolved = dir.canonicalize().unwrap_or(dir);
    let text = resolved.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    resolved
}

/// The per-project state directory, `<base>\<leaf>-<fnv1a64 of the lowercased cwd>`, so two
/// checkouts never share sessions while the name stays readable.
///
/// FROZEN persistence key. The lowercase fold and the hash are baked into a durable directory
/// name, so changing either relocates every project's state and orphans its sessions.
fn default_state_dir(base: &Path, cwd: &Path) -> PathBuf {
    let leaf = cwd
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    let leaf: String = leaf
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    base.join(format!(
        "{leaf}-{:016x}",
        fnv1a64(&cwd.to_string_lossy().to_lowercase())
    ))
}

/// 64-bit FNV-1a. Stable across Rust versions and platforms, unlike `DefaultHasher`, which is
/// what a key persisted on disk needs.
pub fn fnv1a64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

pub const USAGE: &str = r#"codex-imagegen - MCP server that lets Claude Code generate images through a local Codex CLI

USAGE:
  codex-imagegen [OPTIONS]

The server speaks MCP over stdio. Register it with Claude Code:
  claude mcp add --scope user codex-imagegen -- C:\tools\codex-imagegen.exe [OPTIONS]

OPTIONS:
  --codex-bin <abs path>      The Codex CLI. Default: PATH, then
                              %LOCALAPPDATA%\Programs\OpenAI\Codex\bin\codex.exe
  --codex-home <abs dir>      Run Codex with CODEX_HOME set to this directory, a dedicated
                              home that keeps ~/.codex (config, AGENTS.md, MCP servers,
                              plugins, skills) away from it. Sign it in once with
                              CODEX_HOME=<dir> codex login --device-auth
  --model <id>                Agent model, full id. Default: gpt-6-astra
  --effort <level>            Agent reasoning effort. Default: low
  --output-dir <dir>          Default output directory for images. A relative path resolves
                              against CLAUDE_PROJECT_DIR when set, else the working directory.
                              Default: %CLAUDE_PROJECT_DIR%\generated-images, else the
                              per-project state directory's images folder
  --state-dir <abs dir>       Override the per-project state directory. Default:
                              <state base>\<project>-<hash>, where the state base is
                              %CODEX_IMAGEGEN_HOME%, else %USERPROFILE%\.codex-imagegen
  --timeout-seconds <n>       Whole-call limit for generate/refine, 30..86400. Default: 300
  --max-concurrent <n>        Concurrent image turns across sessions, at least 1. Default: 4
  --session-ttl-days <n>      Expire sessions idle this many days, and their files. 0 disables.
                              Default: 7

OTHER:
  --doctor                    Check the Codex CLI, login, plan, image capability and model from
                              a terminal (free: no image is generated), then exit.
  --cleanup                   Remove expired sessions across all projects (their Codex
                              threads, Codex's image copies and the published files), print
                              what was removed and skipped, then exit.
  --older-than-days <n>       With --cleanup: override the TTL (0 = every session not in use).
  --help, -h                  Show this help.
  --version, -V               Show the version.

Both --flag value and --flag=value are accepted.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env {
            cwd: PathBuf::from(r"C:\definitely-not-here\Projects\My App"),
            imagegen_home: None,
            user_profile: Some(OsString::from(r"C:\Users\someone")),
        }
    }

    fn parse(items: &[&str]) -> Result<Config, String> {
        let args: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        Config::parse(&args, &env())
    }

    fn err(items: &[&str]) -> String {
        parse(items).expect_err("expected a parse error")
    }

    #[test]
    fn defaults_follow_the_design() {
        let cfg = parse(&[]).unwrap();
        assert_eq!(cfg.mode, Mode::Serve);
        assert_eq!(cfg.codex_bin, None);
        assert_eq!(cfg.codex_home, None);
        assert_eq!(cfg.model, "gpt-6-astra");
        assert_eq!(cfg.effort, "low");
        assert_eq!(cfg.output_dir, None);
        assert_eq!(cfg.timeout, Duration::from_secs(300));
        assert_eq!(cfg.max_concurrent, 4);
        assert_eq!(cfg.session_ttl_days, 7);
    }

    #[test]
    fn every_value_flag_is_accepted_in_both_syntaxes() {
        let spaced = parse(&[
            "--codex-bin",
            r"C:\tools\codex.exe",
            "--codex-home",
            r"D:\codex-home",
            "--model",
            "gpt-6-luna",
            "--effort",
            "medium",
            "--output-dir",
            "images",
            "--state-dir",
            r"D:\state",
            "--timeout-seconds",
            "600",
            "--max-concurrent",
            "2",
            "--session-ttl-days",
            "0",
        ])
        .unwrap();
        let joined = parse(&[
            r"--codex-bin=C:\tools\codex.exe",
            r"--codex-home=D:\codex-home",
            "--model=gpt-6-luna",
            "--effort=medium",
            "--output-dir=images",
            r"--state-dir=D:\state",
            "--timeout-seconds=600",
            "--max-concurrent=2",
            "--session-ttl-days=0",
        ])
        .unwrap();
        for cfg in [spaced, joined] {
            assert_eq!(cfg.codex_bin, Some(PathBuf::from(r"C:\tools\codex.exe")));
            assert_eq!(cfg.codex_home, Some(PathBuf::from(r"D:\codex-home")));
            assert_eq!(cfg.model, "gpt-6-luna");
            assert_eq!(cfg.effort, "medium");
            assert_eq!(cfg.output_dir, Some(PathBuf::from("images")));
            assert_eq!(cfg.state_dir, PathBuf::from(r"D:\state"));
            assert_eq!(cfg.timeout, Duration::from_secs(600));
            assert_eq!(cfg.max_concurrent, 2);
            assert_eq!(cfg.session_ttl_days, 0);
        }
    }

    #[test]
    fn a_value_may_itself_contain_an_equals_sign() {
        let cfg = parse(&[r"--output-dir=C:\a=b"]).unwrap();
        assert_eq!(cfg.output_dir, Some(PathBuf::from(r"C:\a=b")));
    }

    #[test]
    fn modes_are_selected_by_their_switches() {
        assert_eq!(parse(&["--doctor"]).unwrap().mode, Mode::Doctor);
        assert_eq!(
            parse(&["--cleanup"]).unwrap().mode,
            Mode::Cleanup {
                older_than_days: None
            }
        );
        assert_eq!(
            parse(&["--cleanup", "--older-than-days", "0"])
                .unwrap()
                .mode,
            Mode::Cleanup {
                older_than_days: Some(0)
            }
        );
        assert_eq!(
            parse(&["--older-than-days=3", "--cleanup"]).unwrap().mode,
            Mode::Cleanup {
                older_than_days: Some(3)
            }
        );
        assert_eq!(parse(&["--help"]).unwrap().mode, Mode::Help);
        assert_eq!(parse(&["-h"]).unwrap().mode, Mode::Help);
        assert_eq!(parse(&["--version"]).unwrap().mode, Mode::Version);
        assert_eq!(parse(&["-V"]).unwrap().mode, Mode::Version);
        assert_eq!(parse(&["--doctor", "--help"]).unwrap().mode, Mode::Help);
    }

    #[test]
    fn unknown_repeated_and_malformed_flags_are_errors() {
        assert!(err(&["--nope"]).contains("unknown argument '--nope'"));
        assert!(err(&["positional"]).contains("unknown argument"));
        assert!(err(&["--model=a", "--model", "b"]).contains("more than once"));
        assert!(err(&["-h", "--help"]).contains("more than once"));
        assert!(err(&["--model"]).contains("requires a value"));
        assert!(err(&["--model="]).contains("requires a value"));
        assert!(err(&["--codex-bin", "--doctor"]).contains("requires a value"));
        assert!(err(&["--doctor=yes"]).contains("takes no value"));
        assert!(err(&["--model", "gpt 6"]).contains("--model"));
        assert!(err(&["--effort", "very high"]).contains("--effort"));
    }

    #[test]
    fn numbers_are_range_checked() {
        assert!(err(&["--timeout-seconds", "29"]).contains("between 30 and 86400"));
        assert!(err(&["--timeout-seconds", "86401"]).contains("between 30 and 86400"));
        assert!(err(&["--timeout-seconds", "5m"]).contains("whole number"));
        assert!(err(&["--timeout-seconds", "-1"]).contains("whole number"));
        assert_eq!(
            parse(&["--timeout-seconds", "30"]).unwrap().timeout,
            Duration::from_secs(30)
        );
        assert!(err(&["--max-concurrent", "0"]).contains("between 1"));
        assert!(err(&["--session-ttl-days", "3651"]).contains("between 0 and 3650"));
        assert!(err(&["--cleanup", "--older-than-days", "x"]).contains("whole number"));
    }

    #[test]
    fn the_codex_bin_must_be_absolute() {
        for bad in [
            &["--codex-bin", r"tools\codex.exe"][..],
            &["--codex-bin=codex.exe"][..],
            &["--codex-bin", "C:codex.exe"][..],
            &["--codex-bin", r"\tools\codex.exe"][..],
        ] {
            let e = err(bad);
            assert!(e.contains("--codex-bin requires an absolute path"), "{e}");
        }
        // Absolute is enough: whether it exists is checked when Codex is first needed.
        assert_eq!(
            parse(&["--codex-bin", r"C:\nope\codex.exe"])
                .unwrap()
                .codex_bin,
            Some(PathBuf::from(r"C:\nope\codex.exe"))
        );
    }

    #[test]
    fn directories_that_must_be_absolute_are_checked() {
        assert!(err(&["--codex-home", "codex-home"]).contains("absolute"));
        assert!(err(&["--state-dir", r"\state"]).contains("absolute"));
        // --output-dir may be relative: it resolves per call.
        assert!(parse(&["--output-dir", "out"]).is_ok());
    }

    #[test]
    fn mode_combinations_that_make_no_sense_are_errors() {
        assert!(err(&["--older-than-days", "3"]).contains("only valid with --cleanup"));
        assert!(err(&["--doctor", "--cleanup"]).contains("cannot be combined"));
    }

    #[test]
    fn the_state_base_prefers_codex_imagegen_home() {
        let mut env = env();
        env.imagegen_home = Some(OsString::from(r"E:\imagegen-state"));
        let cfg = Config::parse(&[], &env).unwrap();
        assert_eq!(cfg.state_base, PathBuf::from(r"E:\imagegen-state"));
        assert_eq!(cfg.work_dir, PathBuf::from(r"E:\imagegen-state\work"));
        assert!(cfg.state_dir.starts_with(r"E:\imagegen-state"));

        // An empty value counts as unset.
        env.imagegen_home = Some(OsString::new());
        let cfg = Config::parse(&[], &env).unwrap();
        assert_eq!(
            cfg.state_base,
            PathBuf::from(r"C:\Users\someone\.codex-imagegen")
        );
    }

    #[test]
    fn the_state_base_must_be_absolute() {
        let mut env = env();
        env.imagegen_home = Some(OsString::from("relative"));
        let e = Config::parse(&[], &env).unwrap_err();
        assert!(
            e.contains("CODEX_IMAGEGEN_HOME must be an absolute path"),
            "{e}"
        );

        let mut env = self::env();
        env.user_profile = None;
        let e = Config::parse(&[], &env).unwrap_err();
        assert!(e.contains("CODEX_IMAGEGEN_HOME"), "{e}");
    }

    #[test]
    fn the_per_project_state_dir_is_leaf_plus_hash_of_the_lowercased_cwd() {
        let cfg = parse(&[]).unwrap();
        let cwd = r"C:\definitely-not-here\Projects\My App";
        let expected = format!(
            r"C:\Users\someone\.codex-imagegen\My_App-{:016x}",
            fnv1a64(&cwd.to_lowercase())
        );
        assert_eq!(cfg.state_dir, PathBuf::from(expected));
        assert_eq!(
            cfg.work_dir,
            PathBuf::from(r"C:\Users\someone\.codex-imagegen\work")
        );

        // NTFS paths are case-insensitive, so the same folder spelled differently is one project;
        // a different folder is a different one.
        let mut upper = env();
        upper.cwd = PathBuf::from(r"C:\DEFINITELY-NOT-HERE\PROJECTS\MY APP");
        let upper = Config::parse(&[], &upper).unwrap();
        let hash = |c: &Config| {
            let name = c
                .state_dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_string();
            name.rsplit('-').next().unwrap().to_string()
        };
        assert_eq!(hash(&upper), hash(&cfg));
        let mut other = env();
        other.cwd = PathBuf::from(r"C:\definitely-not-here\Projects\Other App");
        let other = Config::parse(&[], &other).unwrap();
        assert_ne!(other.state_dir, cfg.state_dir);
    }

    #[test]
    fn a_drive_root_cwd_gets_a_placeholder_leaf() {
        let mut env = env();
        env.cwd = PathBuf::from(r"Q:\");
        let cfg = Config::parse(&[], &env).unwrap();
        let name = cfg
            .state_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(name.starts_with("project-"), "{name}");
        assert_eq!(name.len(), "project-".len() + 16);
    }

    #[test]
    fn fnv1a64_matches_the_reference_vectors() {
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64("foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn normalize_dir_strips_the_verbatim_prefix() {
        let dir = crate::testutil::temp_dir("config");
        let normalized = normalize_dir(dir.to_path_buf());
        let text = normalized.to_string_lossy();
        assert!(!text.starts_with(r"\\?\"), "{text}");
        assert!(normalized.is_absolute());
        // A directory that does not exist is kept as given.
        let missing = PathBuf::from(r"C:\definitely-not-here\x");
        assert_eq!(normalize_dir(missing.clone()), missing);
    }

    #[test]
    fn usage_lists_every_flag() {
        for flag in FLAGS {
            assert!(USAGE.contains(flag), "USAGE does not mention {flag}");
        }
        assert!(!USAGE.contains("Not implemented yet"));
    }
}
