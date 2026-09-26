//! The tool layer: the three tools, their argument validation, and the Codex child they share.
//!
//! The child is started lazily, by the first tool call that needs it, and never during
//! `initialize` (docs/design.md, "Spawn"). It is kept while it lives and passes preflight, and
//! replaced by a fresh one -- spawn, handshake, preflight -- when it has died, failed preflight,
//! or changed account. A failed preflight closes the child at once, so a retry after
//! `codex login` works without restarting anything.
//!
//! `generate` validates its arguments, pre-checks the output folder, claims its session (the
//! registry, then the cross-process lease, then the store's check that the name is new), brings
//! the child up and hands the turn to `turn.rs`, in the order of docs/design.md's "Request flow",
//! so nothing is spent until every local check has passed. `refine` claims the existing session
//! the same way, pre-checks its output folder, picks the edit target, brings the child up and
//! checks the session's Codex home, then hands `turn.rs` the thread to resume.
//!
//! The first tool call that brings a child up in a server process also starts automatic session
//! expiry on it, on a thread of its own (`cleanup.rs`).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError, Weak};
use std::time::{Duration, Instant};

use serde_json::value::RawValue;
use serde_json::{json, Value};

use crate::appserver::{AppServer, DetachedSender, NotificationHandler, RpcError};
use crate::cancel::RequestCancel;
use crate::cleanup;
use crate::codex::{self, Budget, Facts, Handshake, Rpc, Usage};
use crate::config::Config;
use crate::errors::{self, Failure};
use crate::mcp::{self, CallContext, Progress, ToolHost};
use crate::output;
use crate::registry::{ChildRef, Liveness, Registry, TurnSlot};
use crate::session::{self, Lease, NewSession, Record, SessionWriter, Store};
use crate::turn::{self, Event};

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

/// How many automatic session names a call tries before giving up. Each try draws a new random
/// suffix, so a second collision in one process is already all but impossible.
const AUTO_NAME_TRIES: usize = 8;

/// How many sessions `status` lists, most recently updated first.
const STATUS_SESSIONS: usize = 20;

/// How long automatic expiry may run (docs/design.md, "Automatic expiry").
const EXPIRY_BUDGET: Duration = Duration::from_secs(60);

/// Each of automatic expiry's `thread/delete` calls gets at most this long.
const DELETE_DEADLINE: Duration = Duration::from_secs(10);

/// Each child's id (see [`ChildRef`]), unique for the life of the process.
static NEXT_CHILD_ID: AtomicU64 = AtomicU64::new(1);

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
pub struct CodexLauncher;

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
    /// `CLAUDE_PROJECT_DIR`, when Claude Code sets it to an absolute path. The default output
    /// folder is `generated-images` under it (docs/design.md, "Output files").
    project_dir: Option<PathBuf>,
    /// What relative paths in tool arguments resolve against: `project_dir` when set, else this
    /// server's working directory, the same rule as output directories. Codex runs in its own
    /// empty work directory, so a path has to be absolute before it reaches it.
    path_base: PathBuf,
    /// Shared with automatic expiry, which stops at shutdown.
    shutting_down: Arc<AtomicBool>,
    /// Set in a server process until automatic expiry has been started: once per process, by the
    /// first tool call that brings a child up. Never set for `--doctor`.
    expiry_due: AtomicBool,
    /// Held while a child is brought up, so two calls never start two children.
    start_lock: Mutex<()>,
    /// The child that exists now, starting or ready. Shutdown takes it from here, so a child is
    /// placed here as soon as it is spawned, before its handshake.
    child: Mutex<Option<Arc<CodexChild>>>,
    /// Children taken out of use while calls were still running turns on them, held weakly: the
    /// last of those calls closes one by letting go of it. Shutdown closes those still alive, so
    /// their turns are stopped too. Locked before `child` wherever both are taken.
    retired: Mutex<Vec<Weak<CodexChild>>>,
    /// The latest usage, from `status` reads and `account/rateLimits/updated` notifications.
    usage: Arc<Mutex<UsageCache>>,
    /// The image turns running in this process, and the routing of their notifications.
    registry: Arc<Registry<Event>>,
    /// This project's session store, in the per-project state directory.
    store: Store,
    /// How long a call waits for `turn/completed` after interrupting its turn.
    interrupt_wait: Duration,
    /// How long refine retries `thread/resume` while another process holds the thread.
    writer_wait: Duration,
}

/// A generate call's claim on its new session: the registry slot, which names it, and the lease.
struct Claim {
    slot: TurnSlot<Event>,
    lease: Lease,
}

/// A refine call's claim on an existing session: the slot, the lease, and the record as read with
/// the lease held.
struct Existing {
    slot: TurnSlot<Event>,
    lease: Lease,
    record: Record,
}

struct CodexChild {
    /// From [`NEXT_CHILD_ID`].
    id: u64,
    server: AppServer,
    bin: PathBuf,
    /// Set once the handshake and preflight have passed.
    ready: OnceLock<Ready>,
    /// Set when `account/updated` reports an auth mode other than ChatGPT, or a turn fails with
    /// `unauthorized`: the login that admitted this child no longer holds, so the next call starts
    /// a fresh one, and this one is closed once no call is using it.
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
    /// The tool host for `--doctor`: no automatic expiry.
    pub fn new(cfg: Config) -> Self {
        let project_dir = std::env::var_os("CLAUDE_PROJECT_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        Self::with_launcher(cfg, Box::new(CodexLauncher), project_dir)
    }

    /// The tool host for the MCP server: as [`new`](Self::new), with automatic session expiry
    /// started by the first tool call that brings Codex up (docs/design.md, "Automatic expiry").
    pub fn serving(cfg: Config) -> Self {
        let app = Self::new(cfg);
        app.expiry_due.store(true, Ordering::SeqCst);
        app
    }

    fn with_launcher(
        cfg: Config,
        launcher: Box<dyn Launcher>,
        project_dir: Option<PathBuf>,
    ) -> Self {
        let path_base = project_dir.clone().unwrap_or_else(|| cfg.cwd.clone());
        let registry = Arc::new(Registry::new(cfg.max_concurrent));
        let store = Store::new(&cfg.state_dir);
        Self {
            cfg,
            launcher,
            project_dir,
            path_base,
            shutting_down: Arc::default(),
            expiry_due: AtomicBool::new(false),
            start_lock: Mutex::new(()),
            child: Mutex::new(None),
            retired: Mutex::new(Vec::new()),
            usage: Arc::default(),
            registry,
            store,
            interrupt_wait: turn::INTERRUPT_WAIT,
            writer_wait: turn::WRITER_WAIT,
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
            // Dead, stale, or left over from a start that did not finish: replace it. A stale one
            // may still be running other calls' turns, which keep it until they finish.
            self.retire_when_idle(child);
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
            Arc::clone(&self.registry),
            server.detached_sender(),
        ));
        let child = Arc::new(CodexChild {
            id: NEXT_CHILD_ID.fetch_add(1, Ordering::Relaxed),
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
        self.start_expiry(&child);
        Ok(child)
    }

    /// Start automatic session expiry on `child`, once per server process (docs/design.md,
    /// "Automatic expiry"). It runs on a thread of its own, bounded to [`EXPIRY_BUDGET`], so it
    /// never delays or fails the call that brought the child up. It holds the child only weakly,
    /// and only for each `thread/delete`, so a child retired meanwhile is not kept alive by it.
    fn start_expiry(&self, child: &Arc<CodexChild>) {
        let days = self.cfg.session_ttl_days;
        if !self.expiry_due.swap(false, Ordering::SeqCst) || days == 0 {
            return;
        }
        let codex_home = child
            .ready
            .get()
            .expect("a started child is ready")
            .handshake
            .codex_home
            .clone();
        let store = self.store.clone();
        let registry = Arc::clone(&self.registry);
        let shutting_down = Arc::clone(&self.shutting_down);
        let weak = Arc::downgrade(child);
        let spawned = std::thread::Builder::new()
            .name("session-expiry".to_string())
            .spawn(move || {
                let deadline = Instant::now() + EXPIRY_BUDGET;
                let delete = |thread_id: &str| -> Result<(), RpcError> {
                    if shutting_down.load(Ordering::SeqCst) {
                        return Err(RpcError::Io("codex-imagegen is shutting down".to_string()));
                    }
                    let Some(child) = weak.upgrade() else {
                        return Err(RpcError::ChildExited {
                            detail: "the Codex app-server was replaced".to_string(),
                        });
                    };
                    let now = Instant::now();
                    let wait = DELETE_DEADLINE.min(deadline.saturating_duration_since(now));
                    child
                        .server
                        .request(
                            "thread/delete",
                            json!({"threadId": thread_id}),
                            now + wait,
                            None,
                        )
                        .map(|_| ())
                };
                // A call that returned while its interrupted turn lingers no longer holds the
                // lease, but the registry still has the session busy.
                let busy = |name: &str| {
                    registry
                        .running()
                        .iter()
                        .any(|t| t.session.eq_ignore_ascii_case(name))
                };
                cleanup::expire(&store, &codex_home, days, &delete, &busy, deadline);
            });
        if let Err(e) = spawned {
            eprintln!("codex-imagegen: session expiry did not start: {e}");
        }
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
        self.take_out_of_use(child);
        child.server.shutdown(RETIRE_GRACE);
    }

    /// Take `child` out of use, and close it once no call holds it any more: at once when this is
    /// the last reference, and otherwise when the last call running a turn on it lets go, since
    /// dropping it drops its job, which kills its tree (docs/design.md, "When a turn fails with
    /// `AUTH_EXPIRED`"). Out of the slot, nothing new can pick it up, so a count of one cannot grow.
    ///
    /// Until then it is listed as retired, for shutdown. Both steps happen under the list's lock,
    /// which shutdown also takes, so shutdown finds the child in the slot or in the list.
    fn retire_when_idle(&self, child: Arc<CodexChild>) {
        let mut retired = lock(&self.retired);
        self.take_out_of_use(&child);
        if Arc::strong_count(&child) == 1 {
            drop(retired);
            child.server.shutdown(RETIRE_GRACE);
            return;
        }
        let weak = Arc::downgrade(&child);
        retired.retain(|c| c.strong_count() > 0 && !c.ptr_eq(&weak));
        retired.push(weak);
    }

    fn take_out_of_use(&self, child: &Arc<CodexChild>) {
        let mut slot = lock(&self.child);
        if slot.as_ref().is_some_and(|c| Arc::ptr_eq(c, child)) {
            *slot = None;
        }
    }

    /// generate, in the order of docs/design.md's "Request flow": validate and pre-check (nothing
    /// spent on any failure), claim the session, bring Codex up, then run the turn.
    fn generate(&self, args: &Value, ctx: &CallContext) -> Value {
        let started = Instant::now();
        let request = match validate(&GENERATE_SPEC, args, &self.path_base) {
            Ok(request) => request,
            Err(failure) => return mcp::failure_result(&failure),
        };
        let dir = output::resolve_dir(
            request.output_dir.as_deref(),
            self.cfg.output_dir.as_deref(),
            self.project_dir.as_deref(),
            &self.cfg.cwd,
            &self.cfg.state_dir,
        );
        if let Err(failure) = output::precheck(&dir) {
            return mcp::failure_result(&failure);
        }
        let budget = Budget::starting_now(self.cfg.timeout);
        let mut auto_name = output::auto_session_name;
        let Claim { slot, lease } =
            match self.claim_new_session(request.session.as_deref(), &mut auto_name) {
                Ok(claim) => claim,
                Err(failure) => return self.claim_failure(failure),
            };
        let progress = ctx.progress();
        let child = match self.ensure_child(Some(ctx.cancel()), Some(&progress), Some(budget)) {
            Ok(child) => child,
            Err(start) => return mcp::failure_result(&start.failure),
        };
        let name = slot.session().to_string();
        let ready = child.ready.get().expect("a returned child is ready");
        // The record is created by the session's first image, with the home the thread lives in.
        let record = SessionWriter::new(
            self.store.clone(),
            &name,
            Some(NewSession {
                codex_home: ready.handshake.codex_home.clone(),
                model: self.cfg.model.clone(),
                output_dir: dir.path.clone(),
            }),
        );
        let finished = self.run_turn(
            &child,
            ctx,
            &progress,
            budget,
            started,
            slot,
            &turn::Request {
                session: &name,
                prompt: &request.text,
                reference_images: &request.reference_images,
                output_dir: &dir.path,
                first_version: 1,
                record: &record,
                thread: turn::Thread::Start,
            },
        );
        self.finish(child, finished, lease)
    }

    /// A claim that failed. As in ensure_child, a missing CLI is reported as CLI_NOT_FOUND even
    /// for a call that arrives as the server shuts down, because locating it is local and
    /// harmless, and it is the failure the user can act on (the CI contract check depends on it).
    fn claim_failure(&self, failure: Failure) -> Value {
        if failure.code == "SERVER_SHUTTING_DOWN" {
            if let Err(missing) = self.launcher.resolve(&self.cfg) {
                return mcp::failure_result(&missing);
            }
        }
        mcp::failure_result(&failure)
    }

    /// Hand the turn to `turn.rs` on `child`.
    #[allow(clippy::too_many_arguments)]
    fn run_turn(
        &self,
        child: &Arc<CodexChild>,
        ctx: &CallContext,
        progress: &Progress,
        budget: Budget,
        started: Instant,
        slot: TurnSlot<Event>,
        request: &turn::Request<'_>,
    ) -> turn::Finished {
        let ready = child.ready.get().expect("a returned child is ready");
        let weak = Arc::downgrade(child);
        let alive: Liveness = Arc::new(move || weak.upgrade().is_some_and(|c| c.server.is_alive()));
        let usage = || self.usage_summary();
        turn::run(
            &turn::Call {
                cfg: &self.cfg,
                server: &child.server,
                child: ChildRef {
                    id: child.id,
                    alive,
                },
                codex_version: ready.handshake.codex_version.as_deref(),
                cancel: ctx.cancel(),
                progress,
                budget,
                started,
                interrupt_wait: self.interrupt_wait,
                writer_wait: self.writer_wait,
                usage: &usage,
            },
            slot,
            request,
        )
    }

    /// The end of a generate or refine call: release the lease, retire the child if its login no
    /// longer holds, and render the result.
    fn finish(&self, child: Arc<CodexChild>, finished: turn::Finished, lease: Lease) -> Value {
        // Held for the whole call, then released (docs/design.md, "Request flow").
        drop(lease);
        if finished.auth_expired {
            // The login this child was admitted with no longer holds. Calls still running on it
            // keep it until they finish; the next call starts a fresh child, which re-reads
            // auth.json (docs/design.md, "When a turn fails with `AUTH_EXPIRED`").
            eprintln!(
                "codex-imagegen: Codex reported the login as unauthorized; the next call starts a \
                 fresh Codex"
            );
            child.stale.store(true, Ordering::SeqCst);
            self.retire_when_idle(child);
        }
        match finished.result {
            Ok(result) => result,
            Err(failure) => mcp::failure_result(&self.during_shutdown(failure)),
        }
    }

    /// Claim a new session for generate: the name given, or a fresh automatic one from
    /// `auto_name`, drawn again while the one drawn is taken. Nothing is spent here.
    ///
    /// In order: the registry (busy in this process, the concurrency cap, shutdown), the lease
    /// (busy in another process), then the store, read with the lease held so no other process
    /// can create the name meanwhile. A name that exists, whatever its case, is SESSION_EXISTS; a
    /// store that exists but cannot be read is STORE_CORRUPT, because the record written later
    /// would replace sessions this call cannot see.
    fn claim_new_session(
        &self,
        session: Option<&str>,
        auto_name: &mut dyn FnMut() -> String,
    ) -> Result<Claim, Failure> {
        if let Some(name) = session {
            return self.try_claim(name)?;
        }
        let mut last = None;
        for _ in 0..AUTO_NAME_TRIES {
            match self.try_claim(&auto_name())? {
                Ok(claim) => return Ok(claim),
                Err(taken) => last = Some(taken),
            }
        }
        Err(last.unwrap_or_else(|| errors::internal_error("no session name could be claimed")))
    }

    /// One attempt at claiming `name`. The outer error ends the call; the inner one says the name
    /// is taken (busy here, busy elsewhere, or recorded), which an automatic name retries.
    fn try_claim(&self, name: &str) -> Result<Result<Claim, Failure>, Failure> {
        let slot = match self.registry.try_start(name) {
            Ok(slot) => slot,
            Err(busy) if busy.code == "SESSION_BUSY" => return Ok(Err(busy)),
            Err(failure) => return Err(failure),
        };
        let lease = match self.store.try_lease(name) {
            Ok(Some(lease)) => lease,
            Ok(None) => return Ok(Err(errors::session_busy_elsewhere(name))),
            Err(e) => return Err(e.failure()),
        };
        let store = self.store.read().map_err(|e| e.failure())?;
        if let Some(existing) = store.get(name) {
            return Ok(Err(errors::session_exists(name, &existing.name)));
        }
        Ok(Ok(Claim { slot, lease }))
    }

    /// refine, in the order of docs/design.md's "Request flow" and "Refine": validate, claim the
    /// session (the registry, the lease, then its record, read with the lease held), pre-check
    /// the output folder, pick the edit target, bring Codex up and check that the session's thread
    /// lives in its home -- nothing is spent on any failure so far -- then resume the thread and
    /// run the turn.
    fn refine(&self, args: &Value, ctx: &CallContext) -> Value {
        let started = Instant::now();
        let request = match validate(&REFINE_SPEC, args, &self.path_base) {
            Ok(request) => request,
            Err(failure) => return mcp::failure_result(&failure),
        };
        let name = request
            .session
            .as_deref()
            .expect("validate requires refine's session");
        let budget = Budget::starting_now(self.cfg.timeout);
        let Existing {
            slot,
            lease,
            record,
        } = match self.claim_session(name) {
            Ok(existing) => existing,
            Err(failure) => return self.claim_failure(failure),
        };
        // The call's own folder, else the one the session was created with. A folder given here
        // applies to this call only [decided].
        let dir = match request.output_dir.as_deref() {
            Some(argument) => output::resolve_dir(
                Some(argument),
                self.cfg.output_dir.as_deref(),
                self.project_dir.as_deref(),
                &self.cfg.cwd,
                &self.cfg.state_dir,
            ),
            None => output::OutputDir {
                path: record.output_dir.clone(),
                source: output::DirSource::Recorded,
            },
        };
        if let Err(failure) = output::precheck(&dir) {
            return mcp::failure_result(&failure);
        }
        let Some(edit_target) = edit_target(&record) else {
            return mcp::failure_result(&errors::session_not_resumable(
                &record.name,
                "Neither copy of the session's latest image still exists as it was saved (Codex's \
                 own copy and the published file are both gone, or were changed), so there is \
                 nothing to edit. Nothing was spent.",
                surviving_copy(&record).as_deref(),
            ));
        };
        let progress = ctx.progress();
        let child = match self.ensure_child(Some(ctx.cancel()), Some(&progress), Some(budget)) {
            Ok(child) => child,
            Err(start) => return mcp::failure_result(&start.failure),
        };
        let home = &child
            .ready
            .get()
            .expect("a returned child is ready")
            .handshake
            .codex_home;
        if !cleanup::same_resolved_path(&record.codex_home, home) {
            return mcp::failure_result(&errors::session_not_resumable(
                &record.name,
                format!(
                    "The session's Codex thread lives in the Codex home {}, and codex-imagegen \
                     now runs Codex with the home {} (its --codex-home setting changed). Nothing \
                     was spent.",
                    record.codex_home.display(),
                    home.display()
                ),
                Some(&edit_target),
            ));
        }
        let writer = SessionWriter::new(self.store.clone(), &record.name, None);
        let finished = self.run_turn(
            &child,
            ctx,
            &progress,
            budget,
            started,
            slot,
            &turn::Request {
                session: &record.name,
                prompt: &request.text,
                reference_images: &request.reference_images,
                output_dir: &dir.path,
                first_version: record.next_version,
                record: &writer,
                thread: turn::Thread::Resume {
                    thread_id: &record.thread_id,
                    edit_target: &edit_target,
                },
            },
        );
        self.finish(child, finished, lease)
    }

    /// Claim an existing session for refine, in generate's order: the registry (busy here, the
    /// cap, shutdown), the lease (busy in another process), then the record, read with the lease
    /// held. A store that exists but cannot be read is STORE_CORRUPT; a name it does not hold,
    /// whatever its case, is SESSION_NOT_FOUND. Nothing is spent here.
    fn claim_session(&self, name: &str) -> Result<Existing, Failure> {
        let slot = self.registry.try_start(name)?;
        let lease = match self.store.try_lease(name) {
            Ok(Some(lease)) => lease,
            Ok(None) => return Err(errors::session_busy_elsewhere(name)),
            Err(e) => return Err(e.failure()),
        };
        let store = self.store.read().map_err(|e| e.failure())?;
        let record = store
            .get(name)
            .cloned()
            .ok_or_else(|| errors::session_not_found(name))?;
        Ok(Existing {
            slot,
            lease,
            record,
        })
    }

    /// The usage part of a result's timing line: the Codex agent bucket, plus the image quota
    /// when Codex has reported one (docs/design.md, "Usage display").
    fn usage_summary(&self) -> String {
        let cache = lock(&self.usage);
        match &cache.usage {
            Some(usage) => {
                let lines = usage.lines();
                if usage.buckets.contains_key("image_gen") {
                    lines.join("; ")
                } else {
                    lines[0].clone()
                }
            }
            None => "Codex agent usage: not reported".to_string(),
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
        line(format!(
            "running turns: {}",
            running_text(&self.registry.running())
        ));
        // Local and free; a store that cannot be read is reported here, not failed on.
        for session_line in sessions_text(&self.store.read_tolerant(), session::now_unix()) {
            line(session_line);
        }
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
             The user often cannot see tool results, so show them each image: display or send \
             the file if you have a tool for that, otherwise give them its path.\n\n\
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
                        "output_dir": {
                            "type": "string",
                            "description": "Folder for the full-resolution PNG. Default: the \
                                            project's generated-images folder. A relative path \
                                            resolves against the project directory.",
                        },
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
                        "output_dir": {
                            "type": "string",
                            "description": "Folder for the new version's full-resolution PNG. \
                                            Default: the folder the session was created with. A \
                                            folder given here applies to this call only. A \
                                            relative path resolves against the project \
                                            directory.",
                        },
                    },
                    "required": ["session", "feedback"],
                    "additionalProperties": false,
                },
            }),
            json!({
                "name": STATUS,
                "description": "Check codex-imagegen's setup without generating anything or \
                                using quota: the Codex CLI and its version, sign-in and plan, \
                                image-generation availability, the agent model, usage windows, \
                                running work and this project's sessions. Use it when a \
                                generation fails, or before the first one.",
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
            GENERATE => self.generate(args, ctx),
            REFINE => self.refine(args, ctx),
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
        // Every running turn is interrupted: the ones whose id is known now, the others as soon as
        // it turns up. Queued before the child's input ends, so they are written first
        // (docs/design.md, "Lifecycle").
        let interrupts = self.registry.begin_shutdown();
        // Every live child: the current one, and each retired one that a call still holds, whose
        // turn would otherwise run on to its deadline and hold up the exit.
        let children: Vec<Arc<CodexChild>> = {
            let mut retired = lock(&self.retired);
            let mut children: Vec<_> = retired.drain(..).filter_map(|c| c.upgrade()).collect();
            children.extend(lock(&self.child).take());
            children
        };
        for (child_id, interrupt) in &interrupts {
            // Sent by the child running the turn. One that is gone has no turn left to stop.
            if let Some(child) = children.iter().find(|c| c.id == *child_id) {
                turn::send_interrupt(&child.server.detached_sender(), interrupt);
            }
        }
        // Every child's input ends now, and they share one grace.
        for child in &children {
            child.server.end_input();
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        for child in &children {
            child
                .server
                .shutdown(deadline.saturating_duration_since(Instant::now()));
        }
    }
}

/// The child's one notification handler. Usage updates are merged into the cache, an account that
/// stops being a ChatGPT login marks the child stale, and everything about a turn is routed to the
/// call running it (`turn.rs`). Runs on the reader thread, so nothing here waits.
fn notification_handler(
    usage: Arc<Mutex<UsageCache>>,
    stale: Arc<AtomicBool>,
    registry: Arc<Registry<Event>>,
    sender: DetachedSender,
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
        _ => turn::route_notification(&registry, &sender, method, params),
    })
}

/// The running turns, for `status`.
fn running_text(running: &[crate::registry::RunningTurn]) -> String {
    if running.is_empty() {
        return "none".to_string();
    }
    running
        .iter()
        .map(|t| {
            let state = if t.interrupted {
                "interrupted; waiting for Codex to confirm it stopped".to_string()
            } else {
                t.phase.clone()
            };
            format!("{} ({state}, {} s)", t.session, t.elapsed.as_secs())
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The sessions part of `status`: this project's sessions, most recently updated first, and the
/// last cleanup. `now` is Unix seconds.
fn sessions_text(store: &session::Tolerant, now: i64) -> Vec<String> {
    let mut lines = Vec::new();
    let mut records: Vec<&session::Record> = store.file.sessions.values().collect();
    records.sort_by(|a, b| b.updated.cmp(&a.updated).then(a.name.cmp(&b.name)));
    lines.push(match (&store.problem, records.len()) {
        (Some(problem), 0) => format!(
            "sessions in this project: unknown -- {problem}; generate and refine refuse to run \
             until it is fixed (STORE_CORRUPT)"
        ),
        (Some(problem), n) => format!(
            "sessions in this project: {n} still readable -- {problem}; generate and refine \
             refuse to run until it is fixed (STORE_CORRUPT)"
        ),
        (None, 0) => "sessions in this project: none".to_string(),
        (None, n) => format!("sessions in this project: {n}"),
    });
    for record in records.iter().take(STATUS_SESSIONS) {
        let turns = match record.turns {
            1 => "1 turn".to_string(),
            n => format!("{n} turns"),
        };
        let latest = record.latest_path().map_or_else(
            || "no saved image".to_string(),
            |p| format!("latest {}", p.display()),
        );
        lines.push(format!(
            "  {}: {turns}, {latest}, updated {} ({} ago)",
            record.name,
            codex::local_time(record.updated).unwrap_or_else(|| "at an unknown time".to_string()),
            age_text(now - record.updated)
        ));
    }
    if records.len() > STATUS_SESSIONS {
        lines.push(format!(
            "  ... and {} older",
            records.len() - STATUS_SESSIONS
        ));
    }
    lines.push(format!(
        "last cleanup: {}",
        match &store.file.last_cleanup {
            None => "not run yet".to_string(),
            Some(cleanup) => cleanup_text(cleanup, now),
        }
    ));
    lines
}

/// `<when> -- removed N sessions, freed X MB, skipped M (name: why; ...)`.
fn cleanup_text(cleanup: &session::LastCleanup, now: i64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    let removed = match cleanup.removed {
        1 => "removed 1 session".to_string(),
        n => format!("removed {n} sessions"),
    };
    let mut text = format!(
        "{} ({} ago) -- {removed}, freed {:.1} MB",
        codex::local_time(cleanup.at).unwrap_or_else(|| "at an unknown time".to_string()),
        age_text(now - cleanup.at),
        cleanup.freed_bytes as f64 / MB
    );
    if !cleanup.skipped.is_empty() {
        let reasons: Vec<String> = cleanup
            .skipped
            .iter()
            .take(5)
            .map(|s| format!("{}: {}", s.name, crate::jsonrpc::clamp(&s.why, 120)))
            .collect();
        let more = cleanup.skipped.len().saturating_sub(reasons.len());
        text.push_str(&format!(
            ", skipped {} ({}{})",
            cleanup.skipped.len(),
            reasons.join("; "),
            if more > 0 {
                format!("; {more} more")
            } else {
                String::new()
            }
        ));
    }
    text
}

/// A rough age: `under a minute`, `12 min`, `5 h`, `3 days`.
fn age_text(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => "under a minute".to_string(),
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 2 * 86_400 => format!("{} h", s / 3600),
        s => format!("{} days", s / 86_400),
    }
}

/// The image a refine edits: the first of Codex's copy and ours of the session's latest image that
/// still exists with the recorded size (docs/design.md, "Refine"). The two are byte-identical, so
/// the one size checks either. When the size was never learned (Codex's file could not be read as
/// the image completed), that the file exists is all there is to check.
fn edit_target(record: &Record) -> Option<PathBuf> {
    [&record.last_saved_path, &record.last_output_path]
        .into_iter()
        .flatten()
        .find(|path| has_size(path, record.last_output_bytes))
        .cloned()
}

/// A copy of the session's images that still exists, for a remediation that starts a new session
/// from it: the edit target, else the newest published file still at its recorded size.
fn surviving_copy(record: &Record) -> Option<PathBuf> {
    edit_target(record).or_else(|| {
        record
            .outputs
            .iter()
            .rev()
            .find(|o| has_size(&o.path, Some(o.bytes)))
            .map(|o| o.path.clone())
    })
}

/// Whether `path` is a file of `bytes` bytes, or of any size when `bytes` is unknown.
fn has_size(path: &Path, bytes: Option<u64>) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && bytes.is_none_or(|b| meta.len() == b))
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

/// The tool layer over scripted fake children, for the tests here and the turn tests.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::codex::testing::FakeCodex;
    use crate::config::Env;
    use crate::registry::RunningTurn;
    use crate::testutil::{temp_dir, TempDir};
    use std::ffi::OsString;
    use std::sync::atomic::AtomicUsize;

    /// Hands out scripted fake children and counts them.
    pub struct FakeLauncher {
        pub codex: FakeCodex,
        pub spawns: Arc<AtomicUsize>,
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

    pub fn cfg(dir: &Path, extra: &[&str]) -> Config {
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

    pub struct Fixture {
        pub app: App,
        pub spawns: Arc<AtomicUsize>,
        /// Also the project directory, so images go to `generated-images` under it by default.
        pub dir: TempDir,
    }

    impl Fixture {
        pub fn running(&self) -> Vec<RunningTurn> {
            self.app.registry.running()
        }

        pub fn has_child(&self) -> bool {
            lock(&self.app.child).is_some()
        }

        pub fn store(&self) -> &Store {
            &self.app.store
        }

        pub fn state_dir(&self) -> PathBuf {
            self.app.cfg.state_dir.clone()
        }

        /// The session record for `name`, read strictly.
        pub fn record(&self, name: &str) -> Option<session::Record> {
            self.app.store.read().unwrap().get(name).cloned()
        }

        /// How long refine retries while another process holds the thread.
        pub fn set_writer_wait(&mut self, wait: Duration) {
            self.app.writer_wait = wait;
        }

        /// Have the first call that brings Codex up start automatic expiry, as a server does.
        pub fn arm_expiry(&self) {
            self.app.expiry_due.store(true, Ordering::SeqCst);
        }

        /// Stop the current child, as a Codex that dies or a server restart does, and wait until
        /// it is gone: the next call starts a fresh one.
        pub fn kill_child(&self) {
            let child = lock(&self.app.child).clone().expect("a child to stop");
            child.server.shutdown(Duration::from_millis(100));
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.server.is_alive() {
                assert!(Instant::now() < deadline, "the child outlived its shutdown");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    pub fn fixture(codex: FakeCodex) -> Fixture {
        fixture_with(codex, &[], |_, _| {})
    }

    /// A fixture with extra flags, and a chance to set what flags cannot: a budget under the
    /// 30-second minimum, and the interrupt wait.
    pub fn fixture_with(
        codex: FakeCodex,
        flags: &[&str],
        tweak: impl FnOnce(&mut Config, &mut Duration),
    ) -> Fixture {
        let dir = temp_dir("tools");
        let mut cfg = cfg(&dir, flags);
        let mut interrupt_wait = turn::INTERRUPT_WAIT;
        tweak(&mut cfg, &mut interrupt_wait);
        let spawns = Arc::new(AtomicUsize::new(0));
        let launcher = FakeLauncher {
            codex,
            spawns: Arc::clone(&spawns),
        };
        let mut app = App::with_launcher(cfg, Box::new(launcher), Some(dir.to_path_buf()));
        app.interrupt_wait = interrupt_wait;
        Fixture { app, spawns, dir }
    }

    /// Whether the result is an error, and its text: the last content block, which is the only
    /// one for everything but a success with images.
    pub fn call(app: &App, tool: &str, args: Value) -> (bool, String) {
        let result = app.call_tool(tool, &args, &CallContext::detached());
        result_text(&result)
    }

    pub fn result_text(result: &Value) -> (bool, String) {
        let content = result["content"].as_array().expect("a content list");
        (
            result["isError"].as_bool().unwrap(),
            content.last().unwrap()["text"]
                .as_str()
                .expect("a text block last")
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::codex::testing::FakeCodex;
    use crate::testutil::temp_dir;

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
        // Each tool's own default folder: refine's is the session's, not the project's.
        assert_eq!(
            generate["properties"]["output_dir"]["description"],
            "Folder for the full-resolution PNG. Default: the project's generated-images folder. \
             A relative path resolves against the project directory."
        );
        assert_eq!(
            refine["properties"]["output_dir"]["description"],
            "Folder for the new version's full-resolution PNG. Default: the folder the session \
             was created with. A folder given here applies to this call only. A relative path \
             resolves against the project directory."
        );
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
    fn a_refine_of_a_session_that_does_not_exist_is_refused_before_codex_starts() {
        let f = fixture(FakeCodex::default());
        let (is_error, text) = call(&f.app, REFINE, json!({"session": "s", "feedback": "bluer"}));
        assert!(is_error);
        assert!(
            text.starts_with("REQUEST REJECTED\ncode: SESSION_NOT_FOUND"),
            "{text}"
        );
        assert!(text.contains("No session named 's'"), "{text}");
        assert!(!text.contains("ACTION REQUIRED"), "{text}");
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "Codex was started");
        assert!(f.running().is_empty(), "the session was left claimed");
        // The lease was released too.
        assert!(f.store().try_lease("s").unwrap().is_some());
    }

    #[test]
    fn a_missing_codex_bin_is_cli_not_found_with_the_stop_block() {
        let dir = temp_dir("tools");
        // The real launcher, with the project directory given rather than read from
        // CLAUDE_PROJECT_DIR, so the output pre-check stays inside the test's folder.
        let real = || {
            App::with_launcher(
                cfg(&dir, &["--codex-bin", r"C:\nope\codex.exe"]),
                Box::new(CodexLauncher),
                Some(dir.to_path_buf()),
            )
        };
        let app = real();
        let (is_error, text) = call(&app, GENERATE, json!({"prompt": "x"}));
        assert!(is_error);
        assert!(
            text.starts_with("IMAGE GENERATION FAILED\ncode: CLI_NOT_FOUND"),
            "{text}"
        );
        assert!(text.contains("=== ACTION REQUIRED ==="));
        assert!(text.contains(r"C:\nope\codex.exe"));

        // Also once shutdown has begun, as when a client pipes its requests and closes stdin at
        // once (the CI contract check does exactly that).
        let closing = real();
        closing.begin_shutdown();
        let (is_error, text) = call(&closing, GENERATE, json!({"prompt": "x"}));
        assert!(is_error);
        assert!(
            text.starts_with("IMAGE GENERATION FAILED\ncode: CLI_NOT_FOUND"),
            "{text}"
        );

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
            "sessions in this project: none",
            "last cleanup: not run yet",
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
                id: NEXT_CHILD_ID.fetch_add(1, Ordering::Relaxed),
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

    /// A notification handler as a child gets one, with a registry and a sender of its own. The
    /// server is returned because the sender only holds it weakly.
    fn test_handler(
        usage: &Arc<Mutex<UsageCache>>,
        stale: &Arc<AtomicBool>,
    ) -> (Arc<NotificationHandler>, AppServer) {
        let server = crate::appserver::fake::connect(|_, _| crate::appserver::fake::Flow::Continue);
        let handler = notification_handler(
            Arc::clone(usage),
            Arc::clone(stale),
            Arc::new(Registry::new(4)),
            server.detached_sender(),
        );
        (handler, server)
    }

    #[test]
    fn a_usage_read_keeps_an_image_quota_learned_from_an_update() {
        let f = fixture(FakeCodex::default());
        call(&f.app, STATUS, json!({}));
        let (handler, _server) = test_handler(&f.app.usage, &Arc::default());
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
    fn shutdown_interrupts_and_closes_a_retired_child_that_still_runs_a_turn() {
        use crate::codex::testing::{image_started, turn_started, Step, TurnScript};
        use crate::codex::testing::{THREAD_ID, TURN_ID};
        // The call's turn runs on the first child. Mid-turn Codex reports a login that is no
        // longer ChatGPT, so the child goes stale, and it is retired while the call still holds
        // it. A fresh child then takes the slot, as the next call would start one.
        let fake = FakeCodex {
            turn: TurnScript {
                steps: vec![
                    Step::Send(turn_started()),
                    Step::Send(image_started("exec-1")),
                    Step::Send(json!({"method": "account/updated",
                                      "params": {"authMode": "apikey", "planType": null}})),
                ],
                // Acknowledged, and the turn never completes on its own.
                on_interrupt: vec![],
                ..TurnScript::default()
            },
            ..FakeCodex::default()
        };
        let first_seen = Arc::clone(&fake.seen);
        // A budget the call would sit out if nothing stopped it, short enough to end the test.
        let f = fixture_with(fake, &[], |cfg, wait| {
            cfg.timeout = Duration::from_secs(5);
            *wait = Duration::from_millis(200);
        });
        let replacement_fake = FakeCodex {
            turn: TurnScript {
                on_interrupt: vec![],
                ..TurnScript::default()
            },
            ..FakeCodex::default()
        };
        let replacement_seen = Arc::clone(&replacement_fake.seen);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            s.spawn(|| {
                let _ = tx.send(call(
                    &f.app,
                    GENERATE,
                    json!({"prompt": "p", "session": "fox"}),
                ));
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            let first = loop {
                let child = lock(&f.app.child).clone();
                let generating = f
                    .running()
                    .first()
                    .is_some_and(|t| t.phase == "generating image");
                match child {
                    Some(child) if generating && child.stale.load(Ordering::SeqCst) => break child,
                    _ => {}
                }
                assert!(
                    Instant::now() < deadline,
                    "the turn never ran on a stale child"
                );
                std::thread::sleep(Duration::from_millis(5));
            };
            let first_weak = Arc::downgrade(&first);
            f.app.retire_when_idle(first);
            assert!(!f.has_child());
            assert!(
                first_weak.upgrade().is_some_and(|c| c.server.is_alive()),
                "the call's child was closed under it"
            );
            let replacement = Arc::new(CodexChild {
                id: NEXT_CHILD_ID.fetch_add(1, Ordering::Relaxed),
                server: replacement_fake.connect(),
                bin: PathBuf::from(r"C:\fake\codex.exe"),
                ready: OnceLock::new(),
                stale: Arc::default(),
            });
            *lock(&f.app.child) = Some(replacement);

            // Fake children have no process or job, so this only shows begin_shutdown does not
            // block on the retired child; the shared-grace arithmetic runs only for real children.
            // The checks that matter follow: the call returns promptly, and the interrupt went to
            // the retired child, not the replacement.
            let started = Instant::now();
            f.app.begin_shutdown();
            assert!(
                started.elapsed() < SHUTDOWN_GRACE,
                "begin_shutdown blocked for {:?}",
                started.elapsed()
            );
            let (is_error, text) = rx
                .recv_timeout(Duration::from_secs(3))
                .expect("the call on the retired child was left running");
            assert!(is_error, "{text}");
            assert!(text.contains("code: SERVER_SHUTTING_DOWN"), "{text}");
            assert_eq!(
                FakeCodex::sent(&first_seen, "turn/interrupt"),
                vec![json!({"threadId": THREAD_ID, "turnId": TURN_ID})],
                "the interrupt did not reach the child running the turn"
            );
            assert!(
                FakeCodex::sent(&replacement_seen, "turn/interrupt").is_empty(),
                "the interrupt went to a child that does not run the turn"
            );
            assert!(first_weak.upgrade().is_none_or(|c| !c.server.is_alive()));
        });
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
        let (handler, _server) = test_handler(&usage, &stale);
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
    fn an_automatic_name_is_drawn_again_while_the_one_drawn_is_taken() {
        let f = fixture(FakeCodex::default());
        // Taken three ways: recorded in the store, busy in this process, leased by another.
        f.store()
            .update(|file| {
                let record: session::Record = serde_json::from_value(json!({
                    "name": "img-recorded", "thread_id": "t", "codex_home": r"C:\h",
                    "model": "m", "created": 1, "updated": 1, "turns": 1,
                    "last_saved_path": null, "last_output_path": null,
                    "last_output_bytes": null, "output_dir": r"C:\o", "next_version": 2,
                    "outputs": []}))
                .unwrap();
                file.sessions.insert(session::key("img-recorded"), record);
                Ok(())
            })
            .unwrap();
        let _busy = f.app.registry.try_start("img-busy").unwrap();
        let _leased = Store::new(&f.state_dir())
            .try_lease("img-leased")
            .unwrap()
            .unwrap();

        let mut drawn = Vec::new();
        let mut names = ["IMG-RECORDED", "img-busy", "img-leased", "img-free"]
            .into_iter()
            .map(str::to_string);
        let mut next = || {
            let name = names.next().unwrap_or_else(|| "img-free".to_string());
            drawn.push(name.clone());
            name
        };
        let claim = f.app.claim_new_session(None, &mut next).ok().unwrap();
        assert_eq!(claim.slot.session(), "img-free");
        drop(claim);
        assert_eq!(drawn.len(), 4);

        // Every draw taken: the last reason is reported, and nothing is left claimed.
        let mut always_taken = || "img-recorded".to_string();
        let failure = f
            .app
            .claim_new_session(None, &mut always_taken)
            .err()
            .unwrap();
        assert_eq!(failure.code, "SESSION_EXISTS");
        assert_eq!(f.running().len(), 1, "only the test's own busy slot");
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn status_lists_sessions_newest_first_and_the_last_cleanup() {
        let now = 1_790_000_000;
        let record = |name: &str, turns: u32, updated: i64, latest: Option<&str>| {
            serde_json::from_value::<session::Record>(json!({
                "name": name, "thread_id": "t", "codex_home": r"C:\h", "model": "m",
                "created": updated, "updated": updated, "turns": turns,
                "last_saved_path": null, "last_output_path": latest,
                "last_output_bytes": 1, "output_dir": r"C:\o", "next_version": 2,
                "outputs": []}))
            .unwrap()
        };
        let mut file = session::StoreFile::empty();
        for r in [
            record("old", 1, now - 3 * 86_400, None),
            record("Fox", 3, now - 720, Some(r"C:\o\Fox-v3.png")),
        ] {
            file.sessions.insert(session::key(&r.name), r);
        }
        let lines = sessions_text(
            &session::Tolerant {
                file: file.clone(),
                problem: None,
            },
            now,
        );
        assert_eq!(lines[0], "sessions in this project: 2");
        assert!(
            lines[1].starts_with(r"  Fox: 3 turns, latest C:\o\Fox-v3.png, updated "),
            "{lines:?}"
        );
        assert!(lines[1].ends_with(" (12 min ago)"), "{lines:?}");
        assert!(
            lines[2].starts_with("  old: 1 turn, no saved image, updated "),
            "{lines:?}"
        );
        assert!(lines[2].ends_with(" (3 days ago)"), "{lines:?}");
        assert_eq!(lines[3], "last cleanup: not run yet");

        file.last_cleanup = Some(session::LastCleanup {
            at: now - 7200,
            removed: 2,
            freed_bytes: 12 * 1024 * 1024,
            skipped: vec![session::Skipped {
                name: "busy".to_string(),
                why: "in use by another process".to_string(),
            }],
        });
        for i in 0..STATUS_SESSIONS {
            let r = record(&format!("s{i}"), 1, now - 10, None);
            file.sessions.insert(session::key(&r.name), r);
        }
        let lines = sessions_text(
            &session::Tolerant {
                file,
                problem: None,
            },
            now,
        );
        assert_eq!(
            lines[0],
            format!("sessions in this project: {}", STATUS_SESSIONS + 2)
        );
        assert_eq!(lines[STATUS_SESSIONS + 1], "  ... and 2 older");
        let cleanup = lines.last().unwrap();
        assert!(cleanup.starts_with("last cleanup: "), "{cleanup}");
        assert!(
            cleanup.ends_with(
                "(2 h ago) -- removed 2 sessions, freed 12.0 MB, skipped 1 (busy: in use by \
                 another process)"
            ),
            "{cleanup}"
        );
        assert_eq!(age_text(-5), "under a minute");
        assert_eq!(age_text(3599), "59 min");
        assert_eq!(age_text(47 * 3600), "47 h");
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
