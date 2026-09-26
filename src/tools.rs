//! The tool layer: the three tools, their argument validation, and the Codex child they share.
//!
//! The child is started lazily, by the first tool call that needs it, and never during
//! `initialize` (docs/design.md, "Spawn"). It is kept while it lives and passes preflight, and
//! replaced by a fresh one -- spawn, handshake, preflight -- when it has died, failed preflight,
//! or changed account. A failed preflight closes the child at once, so a retry after
//! `codex login` works without restarting anything.
//!
//! In this build `generate` and `refine` validate their arguments and bring the child up, so
//! setup problems surface exactly as they will later, and then stop: image generation itself
//! arrives in milestone M2.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};
use std::time::{Duration, Instant};

use serde_json::value::RawValue;
use serde_json::{json, Value};

use crate::appserver::{AppServer, NotificationHandler, RpcError};
use crate::cancel::RequestCancel;
use crate::codex::{self, Budget, Facts, Handshake, Rpc, Usage};
use crate::config::Config;
use crate::errors::{self, Failure};
use crate::mcp::{self, CallContext, Progress, ToolHost};

pub const GENERATE: &str = "codex_imagegen_generate";
pub const REFINE: &str = "codex_imagegen_refine";
pub const STATUS: &str = "codex_imagegen_status";

/// How long shutdown gives the child after closing its stdin (docs/design.md, "Lifecycle").
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How long a child that failed preflight, or is being replaced, gets to exit. It never ran a
/// turn, and an idle app-server exits within about 0.07 s of its stdin closing [verified].
const RETIRE_GRACE: Duration = Duration::from_secs(2);

/// Bound on each live read `status` makes of its own (docs/design.md, "codex_imagegen_status").
const STATUS_READ_DEADLINE: Duration = Duration::from_secs(10);

/// How often a call waiting for another call's Codex start checks whether it has been cancelled
/// or run out of time. The same interval a request uses while waiting for its reply.
const START_LOCK_POLL: Duration = Duration::from_millis(50);

/// How the tool layer gets a Codex child. A trait so tests can hand it a scripted fake; the real
/// one resolves the CLI and spawns it.
///
/// Two steps rather than one so a missing CLI is always reported as CLI_NOT_FOUND, even for a
/// call that arrives as the server is shutting down: locating the binary is local and harmless,
/// starting it is not.
pub trait Launcher: Send + Sync {
    fn resolve(&self, cfg: &Config) -> Result<PathBuf, Failure>;
    fn spawn(&self, cfg: &Config, bin: &Path) -> Result<AppServer, Failure>;
}

/// The real launcher.
struct CodexLauncher;

impl Launcher for CodexLauncher {
    fn resolve(&self, cfg: &Config) -> Result<PathBuf, Failure> {
        codex::resolve_bin(cfg.codex_bin.as_deref())
    }

    fn spawn(&self, cfg: &Config, bin: &Path) -> Result<AppServer, Failure> {
        codex::spawn(bin, cfg)
    }
}

/// The MCP tool host.
pub struct App {
    cfg: Config,
    launcher: Box<dyn Launcher>,
    /// What relative paths in tool arguments resolve against: `CLAUDE_PROJECT_DIR` when Claude
    /// Code sets it, else this server's working directory, the same rule as output directories
    /// (docs/design.md, "Output files"). Codex runs in its own empty work directory, so a path
    /// has to be absolute before it reaches it.
    path_base: PathBuf,
    shutting_down: AtomicBool,
    /// Held while a child is brought up, so two calls never start two children.
    start_lock: Mutex<()>,
    /// The child that exists now, starting or ready. Shutdown takes it from here, so a child is
    /// placed here as soon as it is spawned, before its handshake.
    child: Mutex<Option<Arc<CodexChild>>>,
    /// The latest usage, from `status` reads and `account/rateLimits/updated` notifications.
    usage: Arc<Mutex<UsageCache>>,
}

struct CodexChild {
    server: AppServer,
    bin: PathBuf,
    /// Set once the handshake and preflight have passed.
    ready: OnceLock<Ready>,
    /// Set when `account/updated` reports an auth mode other than ChatGPT: the preflight that
    /// admitted this child no longer holds, so the next call starts a fresh one.
    stale: Arc<AtomicBool>,
}

struct Ready {
    handshake: Handshake,
    facts: Facts,
}

/// Why a child could not be brought up, with whatever was learned on the way for `status`.
struct StartFailure {
    failure: Failure,
    bin: Option<PathBuf>,
    handshake: Option<Handshake>,
    facts: Facts,
}

impl StartFailure {
    fn new(failure: Failure) -> Self {
        Self {
            failure,
            bin: None,
            handshake: None,
            facts: Facts::default(),
        }
    }
}

#[derive(Default)]
struct UsageCache {
    usage: Option<Usage>,
    updated: Option<Instant>,
}

impl App {
    pub fn new(cfg: Config) -> Self {
        let path_base = std::env::var_os("CLAUDE_PROJECT_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| cfg.cwd.clone());
        Self::with_launcher(cfg, Box::new(CodexLauncher), path_base)
    }

    fn with_launcher(cfg: Config, launcher: Box<dyn Launcher>, path_base: PathBuf) -> Self {
        Self {
            cfg,
            launcher,
            path_base,
            shutting_down: AtomicBool::new(false),
            start_lock: Mutex::new(()),
            child: Mutex::new(None),
            usage: Arc::default(),
        }
    }

    /// `--doctor`: the status report from a terminal, then close the child. The flag says
    /// whether Codex is ready to generate images.
    pub fn doctor(&self) -> (String, bool) {
        let report = self.status_report(None, None);
        self.begin_shutdown();
        report
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// The ready child, bringing one up if there is none.
    fn ensure_child(
        &self,
        cancel: Option<&RequestCancel>,
        progress: Option<&Progress>,
        budget: Option<Budget>,
    ) -> Result<Arc<CodexChild>, Box<StartFailure>> {
        let _starting = self.acquire_start_lock(cancel, progress, budget)?;
        let existing = lock(&self.child).clone();
        if let Some(child) = existing {
            if child.ready.get().is_some()
                && !child.stale.load(Ordering::SeqCst)
                && child.server.is_alive()
            {
                return Ok(child);
            }
            // Dead, stale, or left over from a start that did not finish: replace it.
            self.retire(&child);
        }

        let bin = self
            .launcher
            .resolve(&self.cfg)
            .map_err(|f| Box::new(StartFailure::new(f)))?;
        let failed = |failure: Failure, handshake: Option<Handshake>, facts: Facts| {
            Box::new(StartFailure {
                failure,
                bin: Some(bin.clone()),
                handshake,
                facts,
            })
        };
        if self.is_shutting_down() {
            return Err(failed(
                errors::server_shutting_down(),
                None,
                Facts::default(),
            ));
        }
        // Checked again here, after anything above that took time: a call that is cancelled or
        // out of budget must not start a child it can no longer use.
        if let Err(failure) = abandoned(cancel, budget) {
            return Err(failed(failure, None, Facts::default()));
        }
        if let Some(progress) = progress {
            progress.set_phase("starting Codex");
        }
        let server = self
            .launcher
            .spawn(&self.cfg, &bin)
            .map_err(|f| failed(f, None, Facts::default()))?;
        let stale = Arc::new(AtomicBool::new(false));
        server.set_notification_handler(notification_handler(
            Arc::clone(&self.usage),
            Arc::clone(&stale),
        ));
        let child = Arc::new(CodexChild {
            server,
            bin: bin.clone(),
            ready: OnceLock::new(),
            stale,
        });
        *lock(&self.child) = Some(Arc::clone(&child));
        // Checked after publishing: shutdown sets the flag before it takes the slot, so either it
        // saw this child or this sees the flag.
        if self.is_shutting_down() {
            self.retire(&child);
            return Err(failed(
                errors::server_shutting_down(),
                None,
                Facts::default(),
            ));
        }

        let rpc = Rpc {
            server: &child.server,
            cancel,
            per_call: codex::CALL_DEADLINE,
            budget,
        };
        let mut facts = Facts::default();
        let handshake = match codex::initialize(&rpc) {
            Ok(handshake) => handshake,
            Err(failure) => {
                self.retire(&child);
                return Err(failed(self.during_shutdown(failure), None, facts));
            }
        };
        if let Err(failure) = codex::preflight(&rpc, &handshake, &self.cfg, &mut facts) {
            // Closed at once: it never ran a turn, so nothing is lost, and the next call's fresh
            // child re-reads auth.json (docs/design.md, "When preflight fails").
            self.retire(&child);
            return Err(failed(
                self.during_shutdown(failure),
                Some(handshake),
                facts,
            ));
        }
        if let Some(version) = &handshake.codex_version {
            if !handshake.in_tested_range() {
                eprintln!(
                    "codex-imagegen: warning: Codex {version} is outside the tested range {}; \
                     continuing",
                    codex::TESTED_RANGE
                );
            }
        }
        let _ = child.ready.set(Ready { handshake, facts });
        Ok(child)
    }

    /// Take `start_lock`, giving up as soon as the call is cancelled or its budget runs out.
    ///
    /// Another call may hold the lock for a whole spawn, handshake and preflight: tens of seconds
    /// when Codex is slow. A plain `lock` would make a cancelled call sit that out and then
    /// start a child of its own, so the wait polls, at the same interval requests use.
    fn acquire_start_lock(
        &self,
        cancel: Option<&RequestCancel>,
        progress: Option<&Progress>,
        budget: Option<Budget>,
    ) -> Result<MutexGuard<'_, ()>, Box<StartFailure>> {
        let mut announced = false;
        loop {
            abandoned(cancel, budget).map_err(|f| Box::new(StartFailure::new(f)))?;
            match self.start_lock.try_lock() {
                Ok(guard) => return Ok(guard),
                Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
                Err(TryLockError::WouldBlock) => {
                    if !announced {
                        if let Some(progress) = progress {
                            progress.set_phase("waiting for Codex to start");
                        }
                        announced = true;
                    }
                    std::thread::sleep(START_LOCK_POLL);
                }
            }
        }
    }

    /// A failure caused by shutdown closing the child is reported as the shutdown it was.
    fn during_shutdown(&self, failure: Failure) -> Failure {
        if self.is_shutting_down() && failure.code != "CANCELLED" {
            errors::server_shutting_down()
        } else {
            failure
        }
    }

    /// Close `child`, removing it from the slot if it is still the current one.
    fn retire(&self, child: &Arc<CodexChild>) {
        {
            let mut slot = lock(&self.child);
            if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, child)) {
                *slot = None;
            }
        }
        child.server.shutdown(RETIRE_GRACE);
    }

    /// generate and refine: validate, bring up Codex, then stop short of generating (M2).
    fn image_call(&self, spec: &ToolSpec, args: &Value, ctx: &CallContext) -> Value {
        let request = match validate(spec, args, &self.path_base) {
            Ok(request) => request,
            Err(failure) => return mcp::failure_result(&failure),
        };
        let budget = Budget::starting_now(self.cfg.timeout);
        let progress = ctx.progress();
        match self.ensure_child(Some(ctx.cancel()), Some(&progress), Some(budget)) {
            Err(start) => mcp::failure_result(&start.failure),
            Ok(_) => {
                eprintln!(
                    "codex-imagegen: {} passed validation ({} reference image(s)) and Codex is \
                     ready, but this build cannot generate images yet",
                    spec.name,
                    request.reference_images.len()
                );
                mcp::failure_result(&errors::not_implemented_yet())
            }
        }
    }

    fn status(&self, args: &Value, ctx: &CallContext) -> Value {
        if let Err(failure) = check_keys(STATUS, args, &[]) {
            return mcp::failure_result(&failure);
        }
        let progress = ctx.progress();
        let (report, _) = self.status_report(Some(ctx.cancel()), Some(&progress));
        // Not an error even when Codex cannot start: the report is the answer, and the failure
        // it contains says what to do.
        mcp::text_result(report, false)
    }

    /// The readable status report, and whether Codex is ready to generate.
    fn status_report(
        &self,
        cancel: Option<&RequestCancel>,
        progress: Option<&Progress>,
    ) -> (String, bool) {
        let started = self.ensure_child(cancel, progress, None);
        // Read before the report is built, because a child lost during the read is not running,
        // whatever its preflight found.
        let (usage, lost) = match &started {
            Ok(child) => self.live_usage(child, cancel),
            Err(_) => (self.cached_usage("Codex is not running"), None),
        };
        let (child, bin, handshake, facts, failure) = match &started {
            Ok(child) => {
                let ready = child.ready.get().expect("a returned child is ready");
                (
                    lost.is_none().then_some(child),
                    Some(&child.bin),
                    Some(&ready.handshake),
                    &ready.facts,
                    lost.as_ref(),
                )
            }
            Err(start) => (
                None,
                start.bin.as_ref(),
                start.handshake.as_ref(),
                &start.facts,
                Some(&start.failure),
            ),
        };

        let mut out = String::new();
        let mut line = |text: String| {
            out.push_str(&text);
            out.push('\n');
        };
        line("codex-imagegen status".to_string());
        line(format!("server: {}", crate::version_line()));
        line(format!(
            "Codex CLI: {}",
            bin.map_or_else(|| "not found".to_string(), |b| b.display().to_string())
        ));
        line(format!("Codex version: {}", version_text(handshake)));
        line(match child {
            Some(child) => match child.server.pid() {
                Some(pid) => format!("app-server: running (pid {pid})"),
                None => "app-server: running".to_string(),
            },
            None if lost.is_some() => "app-server: not running (it stopped after passing \
                                       preflight; the next call starts a fresh one)"
                .to_string(),
            None => "app-server: not running".to_string(),
        });
        if let Some(handshake) = handshake {
            let mode = if self.cfg.codex_home.is_some() {
                "dedicated, from --codex-home"
            } else {
                "ambient: the user's own Codex home"
            };
            line(format!(
                "Codex home: {} ({mode})",
                handshake.codex_home.display()
            ));
        }
        line(format!("account: {}", account_text(facts, failure)));
        line(format!(
            "image generation: {}",
            match failure {
                None => "available (sign-in, plan, provider capability and settings checked)"
                    .to_string(),
                Some(failure) => format!("unavailable ({})", failure.code),
            }
        ));
        line(format!(
            "agent model: {}, effort {} -- {}",
            self.cfg.model,
            self.cfg.effort,
            model_text(facts, failure)
        ));
        if let Some(map) = &facts.mcp_off_map {
            line(format!("user MCP servers: {}", mcp_servers_text(map)));
        }
        for usage_line in usage {
            line(format!("usage: {usage_line}"));
        }
        line("running turns: none".to_string());
        line("sessions: not implemented yet (milestone M3)".to_string());
        line(format!(
            "settings: --timeout-seconds {}, --max-concurrent {}, --session-ttl-days {}",
            self.cfg.timeout.as_secs(),
            self.cfg.max_concurrent,
            self.cfg.session_ttl_days
        ));
        line(format!(
            "output directory: {}",
            match &self.cfg.output_dir {
                Some(dir) => format!("{} (from --output-dir)", dir.display()),
                None => "the default (the project's generated-images folder)".to_string(),
            }
        ));
        line(format!(
            "state directory: {} (state base {})",
            self.cfg.state_dir.display(),
            self.cfg.state_base.display()
        ));
        if let Some(failure) = failure {
            out.push_str(if lost.is_some() {
                "\nCodex is not ready to generate images: its app-server stopped during this \
                 check.\n\n"
            } else {
                "\nCodex is not ready to generate images. This is what a generate call would \
                 return:\n\n"
            });
            out.push_str(&failure.render_for_agent());
        }
        (out, failure.is_none())
    }

    /// A fresh usage read from the running child, else the last value with its age.
    ///
    /// Also says whether the child was lost: it exited during the read, or is gone by the time
    /// the read returns. A lost child is discarded, so the next call starts a fresh one, and the
    /// failure makes the report not ready. A read that fails with the child still alive (a
    /// timeout, an error reply) only makes the usage stale.
    fn live_usage(
        &self,
        child: &Arc<CodexChild>,
        cancel: Option<&RequestCancel>,
    ) -> (Vec<String>, Option<Failure>) {
        const METHOD: &str = "account/rateLimits/read";
        let rpc = Rpc {
            server: &child.server,
            cancel,
            per_call: STATUS_READ_DEADLINE,
            budget: None,
        };
        let read = rpc.call_raw(METHOD, json!({}));
        let lost = match &read {
            Err(e @ RpcError::ChildExited { .. }) => Some(rpc.failure(METHOD, e.clone())),
            _ if !child.server.is_alive() => Some(errors::app_server_failed(
                METHOD,
                "The Codex app-server exited after answering.",
            )),
            _ => None,
        };
        if lost.is_some() {
            self.retire(child);
        }
        let lines = match read {
            Ok(reply) => {
                // Merged per bucket, not swapped in whole. A read is a full snapshot of each
                // bucket it reports, so those are replaced; one it does not report, such as an
                // image_gen bucket learned from an update (reads have returned only the codex
                // bucket [verified]), is kept.
                let fresh = Usage::from_read(&reply);
                let mut cache = lock(&self.usage);
                let merged = cache.usage.get_or_insert_with(Usage::default);
                merged.buckets.extend(fresh.buckets);
                let lines = merged.lines();
                cache.updated = Some(Instant::now());
                lines
            }
            Err(_) if lost.is_some() => self.cached_usage("the Codex app-server exited"),
            Err(e) => self.cached_usage(&format!("the live read failed: {e}")),
        };
        (lines, lost)
    }

    /// The last usage seen, each line with its age and why it is not fresh.
    fn cached_usage(&self, why_stale: &str) -> Vec<String> {
        let cache = lock(&self.usage);
        match (&cache.usage, cache.updated) {
            (Some(usage), Some(at)) => {
                let age = at.elapsed().as_secs();
                usage
                    .lines()
                    .into_iter()
                    .map(|l| format!("{l} (as of {age} s ago; {why_stale})"))
                    .collect()
            }
            _ => vec![format!("unavailable ({why_stale})")],
        }
    }
}

impl ToolHost for App {
    fn instructions(&self) -> String {
        format!(
            "codex-imagegen makes images with the user's local Codex CLI, billed to their \
             ChatGPT plan.\n\n\
             - {GENERATE}: start a new session and make one image from a prompt.\n\
             - {REFINE}: edit the latest image of a session; each call returns a new version.\n\
             - {STATUS}: check the setup (Codex, login, plan, model, usage) without generating \
             anything or using quota.\n\n\
             Each image comes back as a 1024px preview plus the absolute path of the \
             full-resolution PNG. A generation takes about a minute. {}\n\n\
             If a call fails, no image was produced: relay the remediation in the result to the \
             user and stop the image task. Never substitute an image made another way (SVG, \
             ASCII art, HTML or CSS, code that draws one, or another image tool), and never say \
             an image exists when the result says none was produced.",
            scratch_note(self.cfg.session_ttl_days)
        )
    }

    fn tool_definitions(&self) -> Vec<Value> {
        let scratch = scratch_note(self.cfg.session_ttl_days);
        let failure_rule = "If the call fails, no image exists: relay the remediation to the user \
                            and stop. Never substitute an image made another way (SVG, ASCII \
                            art, code, another tool).";
        let reference_images = |max: usize, extra: &str| {
            json!({
                "type": "array",
                "items": {"type": "string"},
                "maxItems": max,
                "description": format!(
                    "Up to {max} image files (PNG, JPEG or WebP) for Codex to use as \
                     references: absolute paths, or relative to the project directory.{extra}"
                ),
            })
        };
        let output_dir = json!({
            "type": "string",
            "description": "Folder for the full-resolution PNG. Default: the project's \
                            generated-images folder. A relative path resolves against the \
                            project directory.",
        });
        vec![
            json!({
                "name": GENERATE,
                "description": format!(
                    "Generate one image from a text prompt with the user's local Codex CLI, \
                     starting a new session that {REFINE} can continue. Returns a 1024px \
                     preview image plus the absolute path of the full-resolution PNG; use the \
                     path for anything beyond looking at it. Takes about a minute. {scratch} \
                     {failure_rule}"
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "prompt": {
                            "type": "string",
                            "description": "What to draw. Passed to the image tool verbatim.",
                        },
                        "session": {
                            "type": "string",
                            "pattern": "^[A-Za-z0-9._-]{1,64}$",
                            "description": "Name for the new session (letters, digits, '.', \
                                            '_', '-'; up to 64). Omit to have one picked.",
                        },
                        "reference_images": reference_images(5, ""),
                        "output_dir": output_dir.clone(),
                    },
                    "required": ["prompt"],
                    "additionalProperties": false,
                },
            }),
            json!({
                "name": REFINE,
                "description": format!(
                    "Edit the latest image of an existing session with the user's local Codex \
                     CLI and return the new version: a 1024px preview image plus the absolute \
                     path of the full-resolution PNG. Takes about a minute. {scratch} \
                     {failure_rule}"
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session": {
                            "type": "string",
                            "pattern": "^[A-Za-z0-9._-]{1,64}$",
                            "description": "The session to continue, as named by generate.",
                        },
                        "feedback": {
                            "type": "string",
                            "description": "The change to make. Passed to the image tool \
                                            verbatim as the edit request.",
                        },
                        "reference_images": reference_images(
                            4,
                            " The session's latest image is always the edit target."
                        ),
                        "output_dir": output_dir,
                    },
                    "required": ["session", "feedback"],
                    "additionalProperties": false,
                },
            }),
            json!({
                "name": STATUS,
                "description": "Check codex-imagegen's setup without generating anything or \
                                using quota: the Codex CLI and its version, sign-in and plan, \
                                image-generation availability, the agent model, usage windows \
                                and running work. Use it when a generation fails, or before \
                                the first one.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            }),
        ]
    }

    fn call_tool(&self, name: &str, args: &Value, ctx: &CallContext) -> Value {
        match name {
            GENERATE => self.image_call(&GENERATE_SPEC, args, ctx),
            REFINE => self.image_call(&REFINE_SPEC, args, ctx),
            STATUS => self.status(args, ctx),
            other => mcp::failure_result(&errors::bad_request(format!(
                "Unknown tool '{}'. codex-imagegen offers {GENERATE}, {REFINE} and {STATUS}.",
                crate::jsonrpc::clamp(other, 100)
            ))),
        }
    }

    fn begin_shutdown(&self) {
        // The flag first, then the slot: a start in progress either sees the flag or has already
        // published its child for this to take.
        self.shutting_down.store(true, Ordering::SeqCst);
        let child = lock(&self.child).take();
        if let Some(child) = child {
            child.server.shutdown(SHUTDOWN_GRACE);
        }
    }
}

/// What the child's notifications change here: usage updates are merged into the cache, and an
/// account that stops being a ChatGPT login marks the child stale.
fn notification_handler(
    usage: Arc<Mutex<UsageCache>>,
    stale: Arc<AtomicBool>,
) -> Arc<NotificationHandler> {
    Arc::new(move |method: &str, params: &RawValue| match method {
        "account/rateLimits/updated" => {
            let Ok(params) = serde_json::from_str::<Value>(params.get()) else {
                return;
            };
            let mut cache = lock(&usage);
            cache
                .usage
                .get_or_insert_with(Usage::default)
                .merge(&params["rateLimits"]);
            cache.updated = Some(Instant::now());
        }
        "account/updated" => {
            let mode = serde_json::from_str::<Value>(params.get())
                .ok()
                .and_then(|p| {
                    p.get("authMode")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            if mode.as_deref() != Some("chatgpt") {
                eprintln!(
                    "codex-imagegen: Codex reports auth mode {}; the next call starts a fresh \
                     Codex and checks it again",
                    mode.as_deref().unwrap_or("none")
                );
                stale.store(true, Ordering::SeqCst);
            }
        }
        _ => {}
    })
}

/// `Err` with the failure to report when the call has been cancelled or has used up its budget.
fn abandoned(cancel: Option<&RequestCancel>, budget: Option<Budget>) -> Result<(), Failure> {
    if cancel.is_some_and(RequestCancel::is_cancelled) {
        return Err(errors::cancelled());
    }
    match budget {
        Some(b) if Instant::now() >= b.deadline => Err(errors::timeout(b.secs)),
        _ => Ok(()),
    }
}

fn version_text(handshake: Option<&Handshake>) -> String {
    let Some(handshake) = handshake else {
        return "unknown (Codex did not start)".to_string();
    };
    match &handshake.codex_version {
        Some(version) if handshake.in_tested_range() => {
            format!("{version} (tested range {})", codex::TESTED_RANGE)
        }
        Some(version) => format!(
            "{version} -- outside the tested range {}; generation still proceeds, but this \
             version is untested",
            codex::TESTED_RANGE
        ),
        None => format!(
            "unknown (unexpected user agent '{}')",
            crate::jsonrpc::clamp(&handshake.user_agent, 120)
        ),
    }
}

fn account_text(facts: &Facts, failure: Option<&Failure>) -> String {
    match &facts.account {
        Some(account) => match &account.plan {
            Some(plan) => format!("{}, plan {plan}", account.kind),
            None => account.kind.clone(),
        },
        None if failure.is_some_and(|f| f.code == "NOT_AUTHENTICATED") => {
            "not signed in".to_string()
        }
        None => "not checked".to_string(),
    }
}

fn model_text(facts: &Facts, failure: Option<&Failure>) -> String {
    match &facts.model {
        Some(model) if model.hidden => "listed, but hidden: a hidden model is often being \
                                        retired, so consider --model with a listed one"
            .to_string(),
        Some(_) => "listed".to_string(),
        None if failure.is_some_and(|f| f.code == "MODEL_UNAVAILABLE") => {
            "not listed by this Codex".to_string()
        }
        None => "not checked".to_string(),
    }
}

fn mcp_servers_text(map: &Value) -> String {
    let names: Vec<&str> = map["mcp_servers"]
        .as_object()
        .map(|servers| servers.keys().map(String::as_str).collect())
        .unwrap_or_default();
    if names.is_empty() {
        "none configured".to_string()
    } else {
        format!(
            "{} (each turned off for every image thread)",
            names.join(", ")
        )
    }
}

fn scratch_note(ttl_days: u32) -> String {
    match ttl_days {
        0 => "Output files are scratch space: move or copy keepers into the project.".to_string(),
        1 => "Output files are scratch and expire after 1 day idle: move or copy keepers into \
              the project."
            .to_string(),
        n => format!(
            "Output files are scratch and expire after {n} days idle: move or copy keepers into \
             the project."
        ),
    }
}

// ---------------------------------------------------------------------------
// Argument validation
// ---------------------------------------------------------------------------

/// The shape of one image tool's arguments.
struct ToolSpec {
    name: &'static str,
    /// The text passed to the image tool: `prompt` or `feedback`.
    text_key: &'static str,
    session_required: bool,
    max_references: usize,
    keys: &'static [&'static str],
}

const GENERATE_SPEC: ToolSpec = ToolSpec {
    name: GENERATE,
    text_key: "prompt",
    session_required: false,
    max_references: 5,
    keys: &["prompt", "session", "reference_images", "output_dir"],
};

const REFINE_SPEC: ToolSpec = ToolSpec {
    name: REFINE,
    text_key: "feedback",
    session_required: true,
    max_references: 4,
    keys: &["session", "feedback", "reference_images", "output_dir"],
};

/// A validated generate or refine call.
// Most fields are read only by generation (M2) and sessions (M3), which do not exist yet.
#[allow(dead_code)]
#[derive(Debug)]
struct ImageRequest {
    text: String,
    session: Option<String>,
    /// Absolute, and each checked to start with an image signature.
    reference_images: Vec<PathBuf>,
    output_dir: Option<String>,
}

/// Check a call's arguments strictly, before anything is started or spent. The schema says the
/// same things, but MCP clients do not enforce schemas, so this is what actually holds.
fn validate(spec: &ToolSpec, args: &Value, base: &Path) -> Result<ImageRequest, Failure> {
    let args = check_keys(spec.name, args, spec.keys)?;
    let present = |key: &str| args.get(key).filter(|v| !v.is_null());

    let text = match present(spec.text_key) {
        None => {
            return Err(errors::bad_request(format!(
                "'{}' is required.",
                spec.text_key
            )))
        }
        Some(Value::String(text)) if text.trim().is_empty() => {
            return Err(errors::bad_request(format!(
                "'{}' must not be empty.",
                spec.text_key
            )))
        }
        Some(Value::String(text)) => text.clone(),
        Some(_) => {
            return Err(errors::bad_request(format!(
                "'{}' must be a string.",
                spec.text_key
            )))
        }
    };

    let session = match present("session") {
        None if spec.session_required => {
            return Err(errors::bad_request(
                "'session' is required: name the session to continue, as generate named it.",
            ))
        }
        None => None,
        Some(Value::String(name)) if is_valid_session_name(name) => Some(name.clone()),
        Some(Value::String(name)) => {
            return Err(errors::bad_request(format!(
                "'session' must be 1-64 characters, each a letter, a digit, '.', '_' or '-'; got \
                 '{}'.",
                crate::jsonrpc::clamp(name, 80)
            )))
        }
        Some(_) => return Err(errors::bad_request("'session' must be a string.")),
    };

    let reference_images = match present("reference_images") {
        None => Vec::new(),
        Some(Value::Array(items)) => {
            if items.len() > spec.max_references {
                return Err(errors::bad_request(format!(
                    "'reference_images' accepts at most {} paths for {}; got {}.",
                    spec.max_references,
                    spec.name,
                    items.len()
                )));
            }
            let mut paths = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item.as_str() {
                    Some(raw) if !raw.trim().is_empty() => paths.push(check_reference(raw, base)?),
                    Some(_) => {
                        return Err(errors::bad_request(format!(
                            "'reference_images[{i}]' is empty."
                        )))
                    }
                    None => {
                        return Err(errors::bad_request(format!(
                            "'reference_images[{i}]' must be a string path."
                        )))
                    }
                }
            }
            paths
        }
        Some(_) => {
            return Err(errors::bad_request(
                "'reference_images' must be an array of file paths.",
            ))
        }
    };

    let output_dir = match present("output_dir") {
        None => None,
        Some(Value::String(dir)) if dir.trim().is_empty() => {
            return Err(errors::bad_request(
                "'output_dir' must not be empty; omit it for the default.",
            ))
        }
        Some(Value::String(dir)) => Some(dir.clone()),
        Some(_) => return Err(errors::bad_request("'output_dir' must be a string.")),
    };

    Ok(ImageRequest {
        text,
        session,
        reference_images,
        output_dir,
    })
}

/// The arguments as an object with no keys outside `allowed`. A misspelt key is rejected rather
/// than ignored, because an ignored `reference_image` would silently generate without it.
fn check_keys<'a>(
    tool: &str,
    args: &'a Value,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, Failure> {
    let Some(object) = args.as_object() else {
        return Err(errors::bad_request(format!(
            "The arguments to {tool} must be a JSON object."
        )));
    };
    if let Some(unknown) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
        let accepted = if allowed.is_empty() {
            "It takes no arguments.".to_string()
        } else {
            format!("Accepted: {}.", allowed.join(", "))
        };
        return Err(errors::bad_request(format!(
            "Unknown argument '{}' for {tool}. {accepted}",
            crate::jsonrpc::clamp(unknown, 80)
        )));
    }
    Ok(object)
}

/// `[A-Za-z0-9._-]{1,64}` (docs/design.md, "Session names").
fn is_valid_session_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Resolve a reference image to an absolute path and check that it opens and starts with a PNG,
/// JPEG or WebP signature. Codex would otherwise fail to read it before its image tool runs, and
/// the turn would end with no image and no clear reason (docs/design.md, "How failures
/// surface").
fn check_reference(raw: &str, base: &Path) -> Result<PathBuf, Failure> {
    let joined = base.join(raw);
    let path = std::path::absolute(&joined).unwrap_or(joined);
    let shown = path.display().to_string();
    if path.is_dir() {
        return Err(errors::bad_request(format!(
            "Reference image '{shown}' is a folder, not an image file."
        )));
    }
    let mut head = [0u8; 12];
    let read = File::open(&path).and_then(|mut file| {
        let mut filled = 0;
        while filled < head.len() {
            match file.read(&mut head[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        Ok(filled)
    });
    let filled = read.map_err(|e| {
        errors::bad_request(format!(
            "Reference image '{shown}' cannot be opened ({e}). Pass the absolute path of an \
             existing PNG, JPEG or WebP file."
        ))
    })?;
    if !is_image_signature(&head[..filled]) {
        return Err(errors::bad_request(format!(
            "Reference image '{shown}' is not a PNG, JPEG or WebP file: its first bytes match none \
             of those formats."
        )));
    }
    Ok(path)
}

fn is_image_signature(head: &[u8]) -> bool {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JPEG: &[u8] = b"\xff\xd8\xff";
    head.starts_with(PNG)
        || head.starts_with(JPEG)
        || (head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::testing::FakeCodex;
    use crate::config::Env;
    use crate::testutil::{temp_dir, TempDir};
    use std::ffi::OsString;
    use std::sync::atomic::AtomicUsize;

    /// Hands out scripted fake children and counts them.
    struct FakeLauncher {
        codex: FakeCodex,
        spawns: Arc<AtomicUsize>,
    }

    impl Launcher for FakeLauncher {
        fn resolve(&self, _cfg: &Config) -> Result<PathBuf, Failure> {
            Ok(PathBuf::from(r"C:\fake\codex.exe"))
        }

        fn spawn(&self, _cfg: &Config, _bin: &Path) -> Result<AppServer, Failure> {
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(self.codex.clone().connect())
        }
    }

    fn cfg(dir: &Path, extra: &[&str]) -> Config {
        let args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        Config::parse(
            &args,
            &Env {
                cwd: dir.to_path_buf(),
                imagegen_home: Some(OsString::from(dir.join("state"))),
                user_profile: None,
            },
        )
        .unwrap()
    }

    struct Fixture {
        app: App,
        spawns: Arc<AtomicUsize>,
        dir: TempDir,
    }

    fn fixture(codex: FakeCodex) -> Fixture {
        let dir = temp_dir("tools");
        let spawns = Arc::new(AtomicUsize::new(0));
        let launcher = FakeLauncher {
            codex,
            spawns: Arc::clone(&spawns),
        };
        let app = App::with_launcher(cfg(&dir, &[]), Box::new(launcher), dir.to_path_buf());
        Fixture { app, spawns, dir }
    }

    fn call(app: &App, tool: &str, args: Value) -> (bool, String) {
        let result = app.call_tool(tool, &args, &CallContext::detached());
        (
            result["isError"].as_bool().unwrap(),
            result["content"][0]["text"].as_str().unwrap().to_string(),
        )
    }

    fn rejected(app: &App, tool: &str, args: Value) -> String {
        let (is_error, text) = call(app, tool, args);
        assert!(is_error, "{text}");
        assert!(
            text.starts_with("REQUEST REJECTED\ncode: BAD_REQUEST"),
            "{text}"
        );
        text
    }

    fn write(dir: &Path, name: &str, bytes: &[u8]) -> String {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.display().to_string()
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
    const JPEG: &[u8] = b"\xff\xd8\xff\xe0\0\x10JFIF";
    const WEBP: &[u8] = b"RIFF\x24\0\0\0WEBPVP8 ";

    #[test]
    fn the_three_tools_are_declared_with_the_designs_schemas() {
        let f = fixture(FakeCodex::default());
        let tools = f.app.tool_definitions();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec![GENERATE, REFINE, STATUS]);

        let generate = &tools[0]["inputSchema"];
        assert_eq!(generate["required"], json!(["prompt"]));
        assert_eq!(generate["additionalProperties"], false);
        assert_eq!(generate["properties"]["reference_images"]["maxItems"], 5);
        assert_eq!(
            generate["properties"]["session"]["pattern"],
            "^[A-Za-z0-9._-]{1,64}$"
        );
        let refine = &tools[1]["inputSchema"];
        assert_eq!(refine["required"], json!(["session", "feedback"]));
        assert_eq!(refine["properties"]["reference_images"]["maxItems"], 4);
        assert_eq!(tools[2]["inputSchema"]["properties"], json!({}));

        for tool in &tools {
            let description = tool["description"].as_str().unwrap();
            assert!(description.chars().count() < 2048, "{description}");
            // No output schema: structured content would hide our text from the model.
            assert!(tool.get("outputSchema").is_none());
            assert!(tool.get("execution").is_none());
        }
        for tool in &tools[..2] {
            let description = tool["description"].as_str().unwrap();
            assert!(description.contains("1024px preview"), "{description}");
            assert!(description.contains("absolute path"), "{description}");
            assert!(
                description.contains("expire after 7 days idle"),
                "{description}"
            );
            assert!(description.contains("Never substitute"), "{description}");
        }
    }

    #[test]
    fn the_instructions_are_short_and_carry_the_rules() {
        let f = fixture(FakeCodex::default());
        let text = f.app.instructions();
        assert!(text.chars().count() < 2048, "{}", text.len());
        assert!(text.contains("1024px preview"));
        assert!(text.contains("absolute path of the full-resolution PNG"));
        assert!(text.contains("expire after 7 days idle"));
        assert!(text.contains("Never substitute"));
        assert_eq!(
            scratch_note(0),
            "Output files are scratch space: move or copy keepers into the project."
        );
    }

    #[test]
    fn unknown_keys_and_wrong_shapes_are_rejected_before_anything_starts() {
        let f = fixture(FakeCodex::default());
        let app = &f.app;
        assert!(
            rejected(app, GENERATE, json!({"prompt": "x", "reference_image": []}))
                .contains("Unknown argument 'reference_image'")
        );
        assert!(rejected(app, GENERATE, json!("a string")).contains("JSON object"));
        assert!(rejected(app, GENERATE, json!({})).contains("'prompt' is required"));
        assert!(rejected(app, GENERATE, json!({"prompt": ""})).contains("must not be empty"));
        assert!(rejected(app, GENERATE, json!({"prompt": " \n\t"})).contains("must not be empty"));
        assert!(rejected(app, GENERATE, json!({"prompt": 5})).contains("must be a string"));
        assert!(
            rejected(app, GENERATE, json!({"prompt": "x", "output_dir": ""}))
                .contains("'output_dir' must not be empty")
        );
        assert!(
            rejected(app, GENERATE, json!({"prompt": "x", "output_dir": 3}))
                .contains("'output_dir' must be a string")
        );
        // A refine argument is not a generate argument.
        assert!(
            rejected(app, GENERATE, json!({"prompt": "x", "feedback": "y"}))
                .contains("Unknown argument 'feedback'")
        );
        assert!(rejected(app, STATUS, json!({"verbose": true})).contains("takes no arguments"));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "nothing may start");
    }

    #[test]
    fn session_names_follow_the_design() {
        let f = fixture(FakeCodex::default());
        let app = &f.app;
        for bad in [
            "",
            "has space",
            "slash/name",
            "üñí",
            &"a".repeat(65),
            "semi;colon",
        ] {
            assert!(
                rejected(app, GENERATE, json!({"prompt": "x", "session": bad}))
                    .contains("'session' must be 1-64 characters"),
                "{bad:?}"
            );
        }
        assert!(
            rejected(app, GENERATE, json!({"prompt": "x", "session": 7}))
                .contains("'session' must be a string")
        );
        assert!(rejected(app, REFINE, json!({"feedback": "x"})).contains("'session' is required"));
        assert!(rejected(app, REFINE, json!({"session": "fox"})).contains("'feedback' is required"));
        for good in ["fox", "fox-watercolor_v2.final", &"a".repeat(64), "A.b-C_9"] {
            assert!(is_valid_session_name(good), "{good}");
        }
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn reference_images_are_counted_opened_and_sniffed() {
        let f = fixture(FakeCodex::default());
        let app = &f.app;
        let png = write(&f.dir, "a.png", PNG);
        let jpeg = write(&f.dir, "b.jpg", JPEG);
        let webp = write(&f.dir, "c.webp", WEBP);
        let text = write(&f.dir, "notes.png", b"hello, not an image");
        let short = write(&f.dir, "tiny.webp", b"RIFF");

        let six: Vec<&str> = std::iter::repeat_n(png.as_str(), 6).collect();
        assert!(rejected(
            app,
            GENERATE,
            json!({"prompt": "x", "reference_images": six})
        )
        .contains("at most 5 paths for codex_imagegen_generate; got 6"));
        let five: Vec<&str> = std::iter::repeat_n(png.as_str(), 5).collect();
        assert!(rejected(
            app,
            REFINE,
            json!({"session": "s", "feedback": "x", "reference_images": five})
        )
        .contains("at most 4 paths for codex_imagegen_refine; got 5"));

        let missing = f.dir.join("missing.png").display().to_string();
        assert!(rejected(
            app,
            GENERATE,
            json!({"prompt": "x", "reference_images": [missing]})
        )
        .contains("cannot be opened"));
        assert!(rejected(
            app,
            GENERATE,
            json!({"prompt": "x", "reference_images": [text]})
        )
        .contains("not a PNG, JPEG or WebP"));
        assert!(rejected(
            app,
            GENERATE,
            json!({"prompt": "x", "reference_images": [short]})
        )
        .contains("not a PNG, JPEG or WebP"));
        let folder = f.dir.display().to_string();
        assert!(rejected(
            app,
            GENERATE,
            json!({"prompt": "x", "reference_images": [folder]})
        )
        .contains("is a folder"));
        assert!(rejected(
            app,
            GENERATE,
            json!({"prompt": "x", "reference_images": [3]})
        )
        .contains("'reference_images[0]' must be a string"));
        assert!(rejected(
            app,
            GENERATE,
            json!({"prompt": "x", "reference_images": [png, " "]})
        )
        .contains("'reference_images[1]' is empty"));
        assert!(rejected(
            app,
            GENERATE,
            json!({"prompt": "x", "reference_images": "a.png"})
        )
        .contains("must be an array"));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0);

        // Every accepted format, by absolute and by relative path.
        let request = validate(
            &GENERATE_SPEC,
            &json!({"prompt": "x", "reference_images": [png, jpeg, webp, "a.png"]}),
            &f.dir,
        )
        .unwrap();
        assert_eq!(request.reference_images.len(), 4);
        assert!(request.reference_images.iter().all(|p| p.is_absolute()));
        assert_eq!(request.reference_images[3], f.dir.join("a.png"));
    }

    #[test]
    fn nulls_count_as_absent_for_optional_arguments() {
        let request = validate(
            &GENERATE_SPEC,
            &json!({"prompt": "a fox", "session": null, "reference_images": null,
                    "output_dir": null}),
            Path::new(r"C:\"),
        )
        .unwrap();
        assert_eq!(request.text, "a fox");
        assert_eq!(request.session, None);
        assert!(request.reference_images.is_empty());
        assert_eq!(request.output_dir, None);
    }

    #[test]
    fn a_valid_generate_brings_codex_up_then_says_generation_is_not_built_yet() {
        let f = fixture(FakeCodex::default());
        let (is_error, text) = call(&f.app, GENERATE, json!({"prompt": "a red fox"}));
        assert!(is_error);
        assert!(
            text.starts_with("IMAGE GENERATION FAILED\ncode: INTERNAL_ERROR"),
            "{text}"
        );
        assert!(text.contains("milestone M2"), "{text}");
        assert!(text.contains("ACTION REQUIRED"));
        // The child is kept for the next call.
        let (_, again) = call(&f.app, REFINE, json!({"session": "s", "feedback": "bluer"}));
        assert!(again.contains("milestone M2"));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_missing_codex_bin_is_cli_not_found_with_the_stop_block() {
        let dir = temp_dir("tools");
        let app = App::new(cfg(&dir, &["--codex-bin", r"C:\nope\codex.exe"]));
        let (is_error, text) = call(&app, GENERATE, json!({"prompt": "x"}));
        assert!(is_error);
        assert!(
            text.starts_with("IMAGE GENERATION FAILED\ncode: CLI_NOT_FOUND"),
            "{text}"
        );
        assert!(text.contains("=== ACTION REQUIRED ==="));
        assert!(text.contains(r"C:\nope\codex.exe"));

        // status reports the same failure without being an error itself.
        let (is_error, report) = call(&app, STATUS, json!({}));
        assert!(!is_error);
        assert!(report.contains("Codex CLI: not found"), "{report}");
        assert!(report.contains("app-server: not running"), "{report}");
        assert!(report.contains("CLI_NOT_FOUND"), "{report}");
    }

    #[test]
    fn a_call_waiting_on_another_start_stops_at_its_cancel_or_deadline_and_spawns_nothing() {
        let f = fixture(FakeCodex::default());
        let code = |r: Result<Arc<CodexChild>, Box<StartFailure>>| r.err().map(|e| e.failure.code);
        std::thread::scope(|s| {
            // Another call is mid-start and holds the lock for as long as this test needs.
            // Dropped on unwind too, so a failed assertion cannot leave the waiters stuck.
            let holder = lock(&f.app.start_lock);
            let (tx, rx) = std::sync::mpsc::channel();

            // Cancelled before it even gets to wait.
            let early = RequestCancel::new();
            early.cancel();
            {
                let tx = tx.clone();
                let app = &f.app;
                s.spawn(move || {
                    let started = Instant::now();
                    let r = code(app.ensure_child(Some(&early), None, None));
                    tx.send(("early", r, started.elapsed())).unwrap();
                });
            }
            // Cancelled while it waits.
            let late = Arc::new(RequestCancel::new());
            {
                let tx = tx.clone();
                let app = &f.app;
                let late = Arc::clone(&late);
                s.spawn(move || {
                    let r = code(app.ensure_child(Some(&late), None, None));
                    tx.send(("late", r, Duration::ZERO)).unwrap();
                });
            }
            // Runs out of budget while it waits.
            {
                let tx = tx.clone();
                let app = &f.app;
                s.spawn(move || {
                    let started = Instant::now();
                    let budget = Budget {
                        deadline: Instant::now() + Duration::from_millis(150),
                        secs: 300,
                    };
                    let r = code(app.ensure_child(None, None, Some(budget)));
                    tx.send(("budget", r, started.elapsed())).unwrap();
                });
            }

            let next = || {
                rx.recv_timeout(Duration::from_secs(3))
                    .expect("a waiting call sat out the other call's start")
            };
            let mut results = std::collections::HashMap::new();
            for _ in 0..2 {
                let (name, r, took) = next();
                results.insert(name, (r, took));
            }
            assert_eq!(results["early"].0, Some("CANCELLED"));
            assert!(results["early"].1 < Duration::from_secs(1));
            assert_eq!(results["budget"].0, Some("TIMEOUT"));
            assert!(results["budget"].1 < Duration::from_secs(2));

            let cancelled_at = Instant::now();
            late.cancel();
            let (name, r, _) = next();
            let late_took = cancelled_at.elapsed();
            assert_eq!((name, r), ("late", Some("CANCELLED")));
            assert!(late_took < Duration::from_secs(1), "{late_took:?}");
            assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "a Codex was started");
            drop(holder);
        });
        // The lock itself is unharmed: the next call starts Codex as usual.
        let (_, report) = call(&f.app, STATUS, json!({}));
        assert!(report.contains("image generation: available"), "{report}");
        assert_eq!(f.spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_failed_preflight_closes_the_child_and_the_next_call_starts_a_fresh_one() {
        let codex = FakeCodex {
            account: json!({"account": null, "requiresOpenaiAuth": true}),
            ..FakeCodex::default()
        };
        let f = fixture(codex);
        let (is_error, text) = call(&f.app, GENERATE, json!({"prompt": "x"}));
        assert!(is_error);
        assert!(
            text.starts_with("IMAGE GENERATION FAILED\ncode: NOT_AUTHENTICATED"),
            "{text}"
        );
        assert!(lock(&f.app.child).is_none(), "the failed child was kept");
        call(&f.app, GENERATE, json!({"prompt": "x"}));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn status_reports_a_healthy_setup_without_the_email() {
        let f = fixture(FakeCodex::default());
        let (is_error, report) = call(&f.app, STATUS, json!({}));
        assert!(!is_error);
        for expected in [
            "server: codex-imagegen ",
            r"Codex CLI: C:\fake\codex.exe",
            "Codex version: 0.156.0 (tested range 0.156.x)",
            "app-server: running",
            r"Codex home: C:\Users\someone\.codex (ambient",
            "account: chatgpt, plan pro",
            "image generation: available",
            "agent model: gpt-6-astra, effort low -- listed",
            "user MCP servers: cua_repl, node_repl",
            "usage: Codex agent usage: weekly 44% (resets 2026-09-",
            "usage: image quota: not reported",
            "running turns: none",
            "sessions: not implemented yet",
        ] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in:\n{report}"
            );
        }
        assert!(!report.contains("someone@example.com"), "{report}");
        assert!(!report.contains("ACTION REQUIRED"), "{report}");
        let (report, ready) = f.app.status_report(None, None);
        assert!(ready, "{report}");
    }

    #[test]
    fn status_includes_the_failure_when_codex_is_not_ready() {
        let codex = FakeCodex {
            account: json!({"account": {"type": "chatgpt", "planType": "free"}}),
            rate_limits: Err(json!({"code": -32600, "message": "auth required"})),
            ..FakeCodex::default()
        };
        let f = fixture(codex);
        let (is_error, report) = call(&f.app, STATUS, json!({}));
        assert!(!is_error, "status is not itself an error");
        assert!(report.contains("account: chatgpt, plan free"), "{report}");
        assert!(report.contains("image generation: unavailable (IMAGEGEN_UNAVAILABLE)"));
        assert!(report.contains("app-server: not running"), "{report}");
        assert!(
            report.contains("usage: unavailable (Codex is not running)"),
            "{report}"
        );
        assert!(report.contains("IMAGE GENERATION FAILED\ncode: IMAGEGEN_UNAVAILABLE"));
        assert!(report.contains("ACTION REQUIRED"));
    }

    #[test]
    fn status_warns_about_an_untested_codex_version_and_shows_a_hidden_model() {
        let codex = FakeCodex {
            user_agent: "codex-imagegen/0.157.1 (Windows 10.0.26200; x86_64)".to_string(),
            model_pages: vec![vec![crate::codex::testing::model("gpt-6-astra", true)]],
            ..FakeCodex::default()
        };
        let f = fixture(codex);
        let (_, report) = call(&f.app, STATUS, json!({}));
        assert!(
            report.contains("Codex version: 0.157.1 -- outside the tested range 0.156.x"),
            "{report}"
        );
        assert!(report.contains("listed, but hidden"), "{report}");
        assert!(report.contains("image generation: available"), "{report}");
    }

    #[test]
    fn a_failed_usage_read_shows_the_last_value_with_its_age() {
        let f = fixture(FakeCodex::default());
        call(&f.app, STATUS, json!({}));
        // The next read fails: swap in a child whose rate-limit read errors.
        let failing = FakeCodex {
            rate_limits: Err(json!({"code": -32603, "message": "backend unavailable"})),
            ..FakeCodex::default()
        };
        {
            let child = lock(&f.app.child).clone().unwrap();
            let replacement = Arc::new(CodexChild {
                server: failing.connect(),
                bin: child.bin.clone(),
                ready: OnceLock::new(),
                stale: Arc::new(AtomicBool::new(false)),
            });
            let _ = replacement.ready.set(Ready {
                handshake: child.ready.get().unwrap().handshake.clone(),
                facts: child.ready.get().unwrap().facts.clone(),
            });
            *lock(&f.app.child) = Some(replacement);
        }
        let (_, report) = call(&f.app, STATUS, json!({}));
        assert!(
            report.contains("weekly 44%") && report.contains("s ago; the live read failed"),
            "{report}"
        );
        assert!(report.contains("backend unavailable"), "{report}");
    }

    #[test]
    fn a_child_that_dies_during_the_usage_read_is_reported_not_ready_and_discarded() {
        let f = fixture(FakeCodex {
            exits_on: Some("account/rateLimits/read"),
            ..FakeCodex::default()
        });
        let (is_error, report) = call(&f.app, STATUS, json!({}));
        assert!(!is_error, "status is not itself an error");
        for expected in [
            "app-server: not running",
            "image generation: unavailable (APP_SERVER_FAILED)",
            "usage: unavailable (the Codex app-server exited)",
            "IMAGE GENERATION FAILED\ncode: APP_SERVER_FAILED",
            "account/rateLimits/read",
            "stopped before answering",
            "ACTION REQUIRED",
        ] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in:\n{report}"
            );
        }
        assert!(!report.contains("app-server: running"), "{report}");
        assert!(!report.contains("image generation: available"), "{report}");
        // What preflight learned before the child died is still shown.
        assert!(report.contains("account: chatgpt, plan pro"), "{report}");
        assert!(lock(&f.app.child).is_none(), "the dead child was kept");
        // The next call starts a fresh Codex rather than reusing the dead one.
        call(&f.app, STATUS, json!({}));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn doctor_is_not_ready_when_the_child_dies_during_the_usage_read() {
        let f = fixture(FakeCodex {
            exits_on: Some("account/rateLimits/read"),
            ..FakeCodex::default()
        });
        // main.rs exits 1 on this flag.
        let (report, ready) = f.app.doctor();
        assert!(!ready, "{report}");
        assert!(report.contains("app-server: not running"), "{report}");
        assert!(report.contains("APP_SERVER_FAILED"), "{report}");
    }

    #[test]
    fn a_usage_read_that_fails_with_the_child_alive_keeps_codex_ready() {
        let f = fixture(FakeCodex {
            rate_limits: Err(json!({"code": -32603, "message": "backend unavailable"})),
            ..FakeCodex::default()
        });
        let (report, ready) = f.app.status_report(None, None);
        assert!(ready, "{report}");
        for expected in [
            "app-server: running",
            "image generation: available",
            "usage: unavailable (the live read failed: error -32603: backend unavailable)",
        ] {
            assert!(
                report.contains(expected),
                "missing {expected:?} in:\n{report}"
            );
        }
        assert!(!report.contains("ACTION REQUIRED"), "{report}");
        assert!(lock(&f.app.child).is_some(), "a live child was discarded");
        let (report, ready) = f.app.doctor();
        assert!(ready, "{report}");
        assert_eq!(f.spawns.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_usage_read_keeps_an_image_quota_learned_from_an_update() {
        let f = fixture(FakeCodex::default());
        call(&f.app, STATUS, json!({}));
        let handler = notification_handler(Arc::clone(&f.app.usage), Arc::default());
        handler(
            "account/rateLimits/updated",
            &RawValue::from_string(
                r#"{"rateLimits":{"limitId":"image_gen","primary":{"usedPercent":20,
                   "windowDurationMins":1440,"resetsAt":null}}}"#
                    .to_string(),
            )
            .unwrap(),
        );
        // The next read reports only the codex bucket, as the real one does.
        let (_, report) = call(&f.app, STATUS, json!({}));
        assert!(
            report.contains("usage: image quota: 1440-min 20%"),
            "{report}"
        );
        assert!(
            report.contains("usage: Codex agent usage: weekly 44%"),
            "{report}"
        );
    }

    #[test]
    fn shutdown_closes_the_child_and_refuses_later_calls() {
        let f = fixture(FakeCodex::default());
        call(&f.app, STATUS, json!({}));
        let child = lock(&f.app.child).clone().unwrap();
        let started = Instant::now();
        f.app.begin_shutdown();
        assert!(started.elapsed() < Duration::from_secs(6));
        assert!(lock(&f.app.child).is_none());
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.server.is_alive() {
            assert!(Instant::now() < deadline, "the child outlived shutdown");
            std::thread::sleep(Duration::from_millis(10));
        }
        let (is_error, text) = call(&f.app, GENERATE, json!({"prompt": "x"}));
        assert!(is_error);
        assert!(
            text.starts_with("REQUEST REJECTED\ncode: SERVER_SHUTTING_DOWN"),
            "{text}"
        );
        assert_eq!(
            f.spawns.load(Ordering::SeqCst),
            1,
            "nothing new was started"
        );
    }

    #[test]
    fn a_dead_child_is_replaced_on_the_next_call() {
        let f = fixture(FakeCodex::default());
        call(&f.app, STATUS, json!({}));
        lock(&f.app.child)
            .clone()
            .unwrap()
            .server
            .shutdown(Duration::from_millis(100));
        let deadline = Instant::now() + Duration::from_secs(5);
        while lock(&f.app.child).as_ref().unwrap().server.is_alive() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let (_, report) = call(&f.app, STATUS, json!({}));
        assert!(report.contains("image generation: available"), "{report}");
        assert_eq!(f.spawns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn notifications_merge_usage_and_mark_a_non_chatgpt_login_stale() {
        let usage: Arc<Mutex<UsageCache>> = Arc::default();
        let stale = Arc::new(AtomicBool::new(false));
        let handler = notification_handler(Arc::clone(&usage), Arc::clone(&stale));
        let raw = |text: &str| RawValue::from_string(text.to_string()).unwrap();

        handler(
            "account/rateLimits/updated",
            &raw(
                r#"{"rateLimits":{"limitId":"image_gen","primary":{"usedPercent":20,
                   "windowDurationMins":1440,"resetsAt":null}}}"#,
            ),
        );
        let lines = lock(&usage).usage.as_ref().unwrap().lines();
        assert_eq!(lines[1], "image quota: 1440-min 20%");

        handler(
            "account/updated",
            &raw(r#"{"authMode":"chatgpt","planType":"pro"}"#),
        );
        assert!(!stale.load(Ordering::SeqCst));
        handler(
            "account/updated",
            &raw(r#"{"authMode":"apikey","planType":null}"#),
        );
        assert!(stale.load(Ordering::SeqCst));
    }

    #[test]
    fn a_stale_child_is_replaced() {
        let f = fixture(FakeCodex::default());
        call(&f.app, STATUS, json!({}));
        lock(&f.app.child)
            .as_ref()
            .unwrap()
            .stale
            .store(true, Ordering::SeqCst);
        call(&f.app, STATUS, json!({}));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn an_unknown_tool_is_a_bad_request() {
        let f = fixture(FakeCodex::default());
        let text = rejected(&f.app, "codex_imagegen_nope", json!({}));
        assert!(text.contains("Unknown tool 'codex_imagegen_nope'"));
    }

    #[test]
    fn image_signatures_are_recognised_exactly() {
        assert!(is_image_signature(PNG));
        assert!(is_image_signature(JPEG));
        assert!(is_image_signature(WEBP));
        assert!(!is_image_signature(b"\x89PNG"));
        assert!(!is_image_signature(b"RIFF\0\0\0\0WAVEfmt "));
        assert!(!is_image_signature(b"GIF89a"));
        assert!(!is_image_signature(b""));
    }
}
