//! What codex-imagegen knows about the Codex CLI: where to find it, the exact line it is started
//! with, the handshake, the free preflight calls, the usage snapshot, and the parameters of the
//! threads and turns it asks for.
//!
//! Every call made here is free: `initialize`, `account/read`, `modelProvider/capabilities/read`,
//! `model/list`, `config/read` and `account/rateLimits/read` spend no quota [verified]. This module
//! builds `thread/start` and `turn/start` parameters but sends neither; running a turn, the only
//! thing that spends quota, is `turn.rs`.
//!
//! The spawn switches and the preflight checks are a security boundary (AGENTS.md): they are
//! what keeps the child's posture, and its billing, where the design puts them. Change them only
//! with the design.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::appserver::{AppServer, RpcError, SpawnSpec};
use crate::cancel::RequestCancel;
use crate::config::Config;
use crate::errors::{self, Failure};

/// Deadline for each handshake and preflight call (docs/design.md, "Deadlines").
pub const CALL_DEADLINE: Duration = Duration::from_secs(30);

/// The Codex versions this build was tested against. Outside it `status` warns and generation
/// still proceeds, because a hard refusal would break on every Codex auto-update [decided].
pub const TESTED_RANGE: &str = "0.156.x";
const TESTED_MAJOR_MINOR: (u64, u64) = (0, 156);

/// Notifications the child is asked not to send: streaming deltas and state this server never
/// reads. `mcpServer/startupStatus/updated` is deliberately absent: it is the canary that an MCP
/// server started on one of our threads (docs/design.md, "Canary").
pub const OPT_OUT_NOTIFICATIONS: &[&str] = &[
    "item/agentMessage/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "item/plan/delta",
    "turn/plan/updated",
    "turn/diff/updated",
    "thread/tokenUsage/updated",
    "remoteControl/status/changed",
    "skills/changed",
];

/// Features the child is started with switched off (`--disable <name>`).
///
/// `multi_agent` alone does not remove the sub-agent tools: the model catalogue gives
/// `gpt-6-astra` MultiAgentV2, which applies unless `agents.enabled = false` (a `-c` switch in
/// [`spawn_args`]), and an enabled `multi_agent_v2` feature outranks that setting
/// (core/src/config/mod.rs `multi_agent_version_override` at rust-v0.156.0). So `multi_agent_v2`
/// is switched off too: a user config with `[features.multi_agent_v2] enabled = true` left it in
/// effect under `agents.enabled=false` [verified: config/read on a test home].
pub const DISABLED_FEATURES: &[&str] = &[
    "apps",
    "plugins",
    "hooks",
    "memories",
    "multi_agent",
    "multi_agent_v2",
    "goals",
    "shell_tool",
    "tool_suggest",
    "skill_search",
    "browser_use",
    "computer_use",
    "in_app_browser",
];

/// Legacy `[features]` keys that Codex still honours as another name for a feature the child
/// switches off (features/src/legacy.rs at rust-v0.156.0). They can beat the matching
/// `--disable`: `memory_tool = true` in a user config turned `memories` back on
/// [verified: config/read, 0.156.0], and `connectors` is applied after `apps`
/// [verified: source]. So any of these set to true fails preflight, whichever order Codex applies
/// it in.
const LEGACY_FEATURE_ALIASES: &[(&str, &str)] = &[
    ("connectors", "apps"),
    ("memory_tool", "memories"),
    ("collab", "multi_agent"),
    ("codex_hooks", "hooks"),
];

/// Removed from the child's environment, so billing cannot silently move off the ChatGPT plan
/// onto an API key [decided].
pub const STRIPPED_ENV: &[&str] = &["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"];

/// Pages of `model/list` followed before giving up. The catalogue is a handful of models; the
/// bound only stops a server that keeps handing out cursors.
const MAX_MODEL_PAGES: usize = 50;

// ---------------------------------------------------------------------------
// Locating the CLI
// ---------------------------------------------------------------------------

/// Locate the Codex CLI. An explicit `--codex-bin` must exist; otherwise PATH is searched with
/// the Windows executable extensions, then the native installer's location.
///
/// The PATH search is done here rather than left to `Command::new("codex")`, because Windows
/// program resolution looks in the calling executable's own directory before PATH, and whatever
/// sits next to this exe is not necessarily the Codex the user installed.
pub fn resolve_bin(explicit: Option<&Path>) -> Result<PathBuf, Failure> {
    if let Some(explicit) = explicit {
        let shown = explicit.display().to_string();
        if explicit.is_file() {
            if let Ok(abs) = std::path::absolute(explicit) {
                return Ok(abs);
            }
        }
        return Err(errors::cli_not_found(
            Some(&shown),
            &[format!("{shown} (from --codex-bin)")],
        ));
    }

    let mut tried = Vec::new();
    let exts = path_exts();
    match std::env::var_os("PATH") {
        Some(path) => {
            for dir in std::env::split_paths(&path) {
                if dir.as_os_str().is_empty() {
                    continue;
                }
                for ext in &exts {
                    let candidate = dir.join(format!("codex{ext}"));
                    if candidate.is_file() {
                        if let Ok(abs) = std::path::absolute(&candidate) {
                            return Ok(abs);
                        }
                    }
                }
            }
            tried.push(format!(
                "every PATH entry, for codex with the extensions {}",
                exts.join(" ")
            ));
        }
        None => tried.push("PATH (not set)".to_string()),
    }

    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        // Where `winget install OpenAI.Codex` puts it [verified: codex-cli 0.156.0 on this
        // machine]. Reached only when PATH has no codex, such as a host holding a PATH snapshot
        // from before the install.
        let candidate = PathBuf::from(local)
            .join("Programs")
            .join("OpenAI")
            .join("Codex")
            .join("bin")
            .join("codex.exe");
        if candidate.is_file() {
            return Ok(candidate);
        }
        tried.push(candidate.display().to_string());
    }
    Err(errors::cli_not_found(None, &tried))
}

/// PATHEXT, lowercased, with `.exe` first: a real executable is preferred over an npm `.cmd`
/// shim, which runs through cmd.exe and its extra layer of argument quoting. No bare-name entry:
/// an extensionless `codex` on PATH is npm's POSIX shell script, which Windows cannot start.
fn path_exts() -> Vec<String> {
    let mut exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_default()
        .split(';')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.starts_with('.') && s.len() > 1)
        .collect();
    if exts.is_empty() {
        exts = vec![".exe".into(), ".cmd".into(), ".bat".into()];
    }
    exts.sort_by_key(|e| match e.as_str() {
        ".exe" => 0,
        ".com" => 1,
        ".cmd" => 2,
        ".bat" => 3,
        _ => 4,
    });
    exts.dedup();
    exts
}

// ---------------------------------------------------------------------------
// The spawn line
// ---------------------------------------------------------------------------

/// The child's arguments, exactly as docs/design.md "Spawn" lists them. Every switch was
/// accepted and shows in `config/read` [verified]; preflight checks again on every spawn.
pub fn spawn_args() -> Vec<String> {
    let mut args: Vec<String> = ["app-server", "--listen", "stdio://"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // Guards against a user config that turns the image tool off.
    args.extend(["--enable".to_string(), "image_generation".to_string()]);
    for feature in DISABLED_FEATURES {
        args.extend(["--disable".to_string(), feature.to_string()]);
    }
    for setting in [
        "notify=[]",
        "skills.bundled.enabled=false",
        "skills.include_instructions=false",
        // The sub-agent ("collaboration") tools: see DISABLED_FEATURES.
        "agents.enabled=false",
        // `request_user_input`, which is otherwise offered (core/src/config/mod.rs
        // `resolve_experimental_request_user_input_enabled` at rust-v0.156.0).
        "tools.experimental_request_user_input.enabled=false",
        r#"web_search="disabled""#,
        r#"approvals_reviewer="user""#,
        r#"windows.sandbox="unelevated""#,
        "thread_unload_delay_secs=5",
    ] {
        args.extend(["-c".to_string(), setting.to_string()]);
    }
    args
}

/// How to start the child: the spawn line, the environment minus the API-key variables,
/// `CODEX_HOME` only with `--codex-home`, and the empty work directory as its cwd.
pub fn spawn_spec(bin: &Path, cfg: &Config) -> SpawnSpec {
    let mut env_set: Vec<(&'static str, OsString)> = Vec::new();
    if let Some(home) = &cfg.codex_home {
        env_set.push(("CODEX_HOME", home.clone().into_os_string()));
    }
    SpawnSpec {
        program: bin.to_path_buf(),
        args: spawn_args(),
        env_remove: STRIPPED_ENV.to_vec(),
        env_set,
        cwd: cfg.work_dir.clone(),
    }
}

/// Create the work directory and start the child. A binary the OS refuses to start is
/// SPAWN_FAILED; a work directory that cannot be created is codex-imagegen's own problem.
pub fn spawn(bin: &Path, cfg: &Config) -> Result<AppServer, Failure> {
    if let Err(e) = std::fs::create_dir_all(&cfg.work_dir) {
        return Err(errors::internal_error(format!(
            "codex-imagegen could not create its Codex working directory {}.",
            cfg.work_dir.display()
        ))
        .with_detail(e.to_string()));
    }
    if let Some(home) = &cfg.codex_home {
        match std::fs::metadata(home) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(errors::codex_home_unusable(home, "is not a folder")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(errors::codex_home_unusable(home, "does not exist"))
            }
            Err(e) => {
                return Err(
                    errors::codex_home_unusable(home, "cannot be read").with_detail(e.to_string())
                )
            }
        }
    }
    AppServer::spawn(&spawn_spec(bin, cfg))
        .map_err(|e| errors::spawn_failed(&bin.display().to_string(), e.to_string()))
}

// ---------------------------------------------------------------------------
// Calls
// ---------------------------------------------------------------------------

/// A whole call's budget: `--timeout-seconds` for generate and refine.
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub deadline: Instant,
    pub secs: u64,
}

impl Budget {
    pub fn starting_now(limit: Duration) -> Self {
        Self {
            deadline: Instant::now() + limit,
            secs: limit.as_secs(),
        }
    }
}

/// How each call to the child is bounded: its own deadline, the whole call's budget if there is
/// one, and the caller's cancellation.
pub struct Rpc<'a> {
    pub server: &'a AppServer,
    pub cancel: Option<&'a RequestCancel>,
    pub per_call: Duration,
    pub budget: Option<Budget>,
}

impl Rpc<'_> {
    fn deadline(&self) -> Instant {
        let own = Instant::now() + self.per_call;
        self.budget.map_or(own, |b| own.min(b.deadline))
    }

    pub fn call_raw(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        self.server
            .request(method, params, self.deadline(), self.cancel)
    }

    pub fn call(&self, method: &str, params: Value) -> Result<Value, Failure> {
        self.call_raw(method, params)
            .map_err(|e| self.failure(method, e))
    }

    /// The failure a call's error amounts to. A missed deadline is TIMEOUT once the whole call's
    /// budget is spent, and APP_SERVER_FAILED naming the method otherwise.
    pub fn failure(&self, method: &str, error: RpcError) -> Failure {
        match error {
            RpcError::Timeout { .. } => match self.budget {
                Some(budget) if Instant::now() >= budget.deadline => errors::timeout(budget.secs),
                _ => errors::app_server_failed(
                    method,
                    format!(
                        "Codex did not answer {method} within {} s.",
                        self.per_call.as_secs()
                    ),
                ),
            },
            RpcError::Cancelled => errors::cancelled(),
            RpcError::ChildExited { detail } => errors::app_server_failed(
                method,
                format!("The Codex app-server stopped before answering: {detail}"),
            ),
            RpcError::Remote { code, message } => errors::app_server_failed(
                method,
                format!("Codex answered with error {code}: {message}"),
            ),
            RpcError::Io(detail) => errors::app_server_failed(method, detail),
        }
    }
}

/// Read a reply into its typed form. A shape we cannot read is a protocol anomaly, reported as
/// APP_SERVER_FAILED naming the Codex version (docs/design.md, "Codex version pinning").
fn parse<T: DeserializeOwned>(
    method: &str,
    value: Value,
    version: Option<&str>,
) -> Result<T, Failure> {
    serde_json::from_value(value).map_err(|e| {
        errors::app_server_failed(
            method,
            format!(
                "codex-cli {} sent a {method} reply codex-imagegen cannot read ({e}). This \
                 build was tested with codex-cli {TESTED_RANGE}.",
                version.unwrap_or("(unknown version)")
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// What `initialize` reports about the child.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handshake {
    pub codex_home: PathBuf,
    pub user_agent: String,
    /// Parsed from `user_agent`; `None` if it does not have the expected shape.
    pub codex_version: Option<String>,
}

impl Handshake {
    pub fn in_tested_range(&self) -> bool {
        self.codex_version.as_deref().is_some_and(in_tested_range)
    }
}

/// The `initialize` params, exactly as docs/design.md "Handshake and preflight" gives them.
pub fn initialize_params() -> Value {
    json!({
        "clientInfo": {"name": "codex-imagegen", "title": null, "version": crate::VERSION},
        "capabilities": {
            "experimentalApi": false,
            "requestAttestation": false,
            "optOutNotificationMethods": OPT_OUT_NOTIFICATIONS,
        },
    })
}

/// `initialize`, then the `initialized` notification.
pub fn initialize(rpc: &Rpc<'_>) -> Result<Handshake, Failure> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Reply {
        user_agent: String,
        codex_home: PathBuf,
    }
    let reply: Reply = parse(
        "initialize",
        rpc.call("initialize", initialize_params())?,
        None,
    )?;
    rpc.server
        .notify("initialized", None)
        .map_err(|e| rpc.failure("initialized", e))?;
    Ok(Handshake {
        codex_version: parse_version(&reply.user_agent),
        user_agent: reply.user_agent,
        codex_home: reply.codex_home,
    })
}

/// The Codex version from a user agent such as
/// `codex-imagegen/0.156.0 (Windows 10.0.26200; x86_64) xterm-256color (codex-imagegen; 0.1.0)`
/// [verified]: the token after the first `/`, up to the first space.
pub fn parse_version(user_agent: &str) -> Option<String> {
    let (_, rest) = user_agent.split_once('/')?;
    let version = rest.split(' ').next()?;
    let looks_right = !version.is_empty()
        && version.len() <= 40
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    looks_right.then(|| version.to_string())
}

/// Whether `version` is in [`TESTED_RANGE`]: same major and minor, any patch or suffix.
pub fn in_tested_range(version: &str) -> bool {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|p| p.parse::<u64>().ok());
    let minor = parts.next().and_then(|p| p.parse::<u64>().ok());
    (major, minor) == (Some(TESTED_MAJOR_MINOR.0), Some(TESTED_MAJOR_MINOR.1))
}

// ---------------------------------------------------------------------------
// Preflight
// ---------------------------------------------------------------------------

/// What preflight learned, kept whether or not it passed, so `status` can show how far it got.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub account: Option<AccountFacts>,
    pub image_capability: Option<bool>,
    pub model: Option<ModelFacts>,
    /// The per-thread MCP-off map built from preflight's `config/read`, for display. Each thread
    /// rebuilds its own from a fresh read (docs/design.md, "Thread and turn parameters").
    pub mcp_off_map: Option<Value>,
}

/// The signed-in account. The email is deliberately not kept: nothing here needs it, and a status
/// report that printed it would leak it into a transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountFacts {
    pub kind: String,
    pub plan: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelFacts {
    /// A hidden model is often being retired, so `status` points it out.
    pub hidden: bool,
}

/// Check that this child can generate images before any turn is sent (docs/design.md,
/// "Handshake and preflight", steps 1-4; step 5, the version, comes from the handshake).
/// Whatever was learned is left in `facts` either way.
pub fn preflight(
    rpc: &Rpc<'_>,
    handshake: &Handshake,
    cfg: &Config,
    facts: &mut Facts,
) -> Result<(), Failure> {
    let version = handshake.codex_version.as_deref();

    // 1. The account: signed in, with ChatGPT, on a plan that gets images.
    #[derive(Deserialize)]
    struct AccountReply {
        account: Option<Value>,
    }
    let reply: AccountReply = parse(
        "account/read",
        rpc.call("account/read", json!({}))?,
        version,
    )?;
    let Some(account) = reply.account else {
        return Err(errors::not_authenticated(cfg.codex_home.as_deref()));
    };
    let kind = account
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let plan = account
        .get("planType")
        .and_then(Value::as_str)
        .map(str::to_string);
    facts.account = Some(AccountFacts {
        kind: kind.clone(),
        plan: plan.clone(),
    });
    if kind != "chatgpt" {
        return Err(errors::not_chatgpt_account(
            &kind,
            cfg.codex_home.as_deref(),
        ));
    }
    if plan.as_deref() == Some("free") {
        return Err(errors::imagegen_unavailable_on_plan(
            "free",
            cfg.codex_home.as_deref(),
        ));
    }

    // 2. The provider's image capability. Not a login check: it reads true even signed out.
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Capabilities {
        image_generation: bool,
    }
    let caps: Capabilities = parse(
        "modelProvider/capabilities/read",
        rpc.call("modelProvider/capabilities/read", json!({}))?,
        version,
    )?;
    facts.image_capability = Some(caps.image_generation);
    if !caps.image_generation {
        return Err(errors::imagegen_unavailable_capability());
    }

    // 3. The pinned model, hidden ones included, across every page.
    facts.model = Some(find_model(rpc, &cfg.model, version)?);

    // 4. The effective configuration: readable, and showing every switch the child was started
    //    with still in effect.
    let config = read_config(rpc, &cfg.work_dir)?;
    facts.mcp_off_map = Some(mcp_off_map(&config.config));
    if let Some(setting) = overridden_switch(&config) {
        return Err(errors::imagegen_unavailable_setting(&setting));
    }
    Ok(())
}

fn find_model(rpc: &Rpc<'_>, model: &str, version: Option<&str>) -> Result<ModelFacts, Failure> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Page {
        data: Vec<Entry>,
        #[serde(default)]
        next_cursor: Option<String>,
    }
    #[derive(Deserialize)]
    struct Entry {
        id: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        hidden: bool,
    }

    let mut offered: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_MODEL_PAGES {
        let mut params = json!({"includeHidden": true});
        if let Some(cursor) = &cursor {
            params["cursor"] = json!(cursor);
        }
        let page: Page = parse("model/list", rpc.call("model/list", params)?, version)?;
        for entry in &page.data {
            if entry.id == model || entry.model.as_deref() == Some(model) {
                return Ok(ModelFacts {
                    hidden: entry.hidden,
                });
            }
            offered.push(entry.id.clone());
        }
        match page.next_cursor {
            // A cursor that repeats would loop forever; treat it as the end.
            Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
            _ => break,
        }
    }
    let listed = if offered.is_empty() {
        "Codex listed no models.".to_string()
    } else {
        format!("Codex lists: {}", offered.join(", "))
    };
    Err(errors::model_unavailable(model, listed))
}

/// What `config/read` reports: the effective config object, and `origins`, which names for each
/// dotted key path the config layer its value was taken from.
#[derive(Clone, Debug)]
pub struct EffectiveConfig {
    pub config: Value,
    /// `{"<dotted.path>": {"name": {"type": "sessionFlags" | "user" | ...}, "version"}}`, or an
    /// empty object when the reply has none.
    pub origins: Value,
}

/// `config/read` as a thread in the work directory would see it. An error reply means Codex
/// cannot resolve its own configuration -- for example a legacy top-level `profile` key, which
/// codex-cli 0.156.0 rejects here while its app-server logs "using defaults" and keeps serving
/// [verified] -- so nothing about the switches can be confirmed, and preflight fails closed.
pub fn read_config(rpc: &Rpc<'_>, work_dir: &Path) -> Result<EffectiveConfig, Failure> {
    let method = "config/read";
    match rpc.call_raw(method, json!({"cwd": work_dir})) {
        Ok(reply) => match reply.get("config") {
            Some(config) if config.is_object() => Ok(EffectiveConfig {
                config: config.clone(),
                origins: reply
                    .get("origins")
                    .filter(|origins| origins.is_object())
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            }),
            _ => Err(errors::app_server_failed(
                method,
                "the reply has no config object",
            )),
        },
        Err(RpcError::Remote { message, .. }) => Err(errors::codex_config_unreadable(message)),
        Err(e) => Err(rpc.failure(method, e)),
    }
}

/// The per-thread `config` that turns off every MCP server in `config` (a `config/read` config
/// object): `{mcp_servers: {<name>: {enabled: false}}}` for each name present. `-c` and thread
/// config can only add or override keys, never remove them, so each server is disabled by name
/// [verified].
pub fn mcp_off_map(config: &Value) -> Value {
    let servers: Map<String, Value> = config
        .get("mcp_servers")
        .and_then(Value::as_object)
        .map(|servers| {
            servers
                .keys()
                .map(|name| (name.clone(), json!({"enabled": false})))
                .collect()
        })
        .unwrap_or_default();
    json!({"mcp_servers": servers})
}

/// Switches whose value `config/read` leaves out of its config object: its `tools` carries only
/// `web_search` (app-server-protocol `ToolsV2` at rust-v0.156.0), and
/// `tools.experimental_request_user_input.enabled` was missing from it under the switch
/// [verified: config/read, 0.156.0]. `origins` still names the layer each one was taken from, so
/// it must be the child's own switches (`sessionFlags`), which hold the value it was started with.
/// This checks the layer, not the value: that the value in our layer is `false` is guaranteed by
/// the spawn line itself, which `the_spawn_line_is_exactly_the_designs` pins.
const SWITCHES_CHECKED_BY_ORIGIN: &[&str] = &["tools.experimental_request_user_input.enabled"];

/// The first spawn switch that the effective configuration does not show in effect, named as
/// the setting that beat it, or `None` when every one holds.
///
/// Our switches outrank every config layer except legacy managed config (`managed_config.toml`)
/// [verified: source, config/src/config_layer_source.rs], so this finds that layer or managed
/// requirements overriding one, or a legacy alias that Codex applies after the canonical key.
/// Compared exactly: a value that is merely absent is not the value the child was started with,
/// so it fails too, rather than being assumed to be a harmless default.
pub fn overridden_switch(effective: &EffectiveConfig) -> Option<String> {
    let config = &effective.config;
    let at = |path: &[&str]| {
        path.iter()
            .try_fold(config, |value, key| value.get(*key))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let describe = |name: &str, value: &Value| match value {
        Value::Null => format!("{name} (not in effect: Codex reports no value)"),
        other => format!("{name} = {other}"),
    };

    let image = at(&["features", "image_generation"]);
    if image != Value::Bool(true) {
        return Some(describe("features.image_generation", &image));
    }
    for feature in DISABLED_FEATURES {
        let value = at(&["features", feature]);
        // A feature that also takes settings reads as a table once a config sets any of them:
        // `--disable multi_agent_v2` over a user's `[features.multi_agent_v2]` table showed
        // `{"enabled": false, ...}` [verified: config/read on a test home].
        let off = value == Value::Bool(false) || value.get("enabled") == Some(&Value::Bool(false));
        if !off {
            return Some(describe(&format!("features.{feature}"), &value));
        }
    }
    for (alias, _) in LEGACY_FEATURE_ALIASES {
        let value = at(&["features", alias]);
        if value == Value::Bool(true) {
            return Some(describe(&format!("features.{alias}"), &value));
        }
    }
    let expected: [(&[&str], Value); 7] = [
        (&["web_search"], json!("disabled")),
        (&["notify"], json!([])),
        (&["skills", "bundled", "enabled"], json!(false)),
        (&["skills", "include_instructions"], json!(false)),
        (&["agents", "enabled"], json!(false)),
        (&["approvals_reviewer"], json!("user")),
        (&["windows", "sandbox"], json!("unelevated")),
    ];
    for (path, want) in expected {
        let value = at(path);
        if value != want {
            return Some(describe(&path.join("."), &value));
        }
    }
    for path in SWITCHES_CHECKED_BY_ORIGIN {
        let layer = effective
            .origins
            .get(*path)
            .and_then(|origin| origin.get("name"));
        match layer {
            Some(layer) if layer.get("type") == Some(&json!("sessionFlags")) => {}
            Some(layer) => {
                return Some(format!(
                    "{path} (taken from the config layer {layer}, not from the switch \
                     codex-imagegen starts Codex with)"
                ))
            }
            None => {
                return Some(format!(
                    "{path} (not in effect: Codex reports no origin for it)"
                ))
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Threads and turns
// ---------------------------------------------------------------------------

/// The developer instructions every image thread gets, verbatim from docs/design.md
/// ("developerInstructions"); a test holds the two to the same text.
pub const DEVELOPER_INSTRUCTIONS: &str = "You are a headless image-generation backend driven by a \
program, not a person. For each user message, call the image generation tool exactly once. Calling \
it through `exec` is expected; inside `exec`, call only the image generation tool. Use the text \
inside `<image_prompt>` or `<edit_request>` as the tool's `prompt`, character for character: do not \
rewrite, expand, translate or summarise it. Set `referenced_image_paths` to the `<edit_target>` \
path, if present, followed by every `<reference_images>` path in order. Omit it when there are \
none, and never use `num_last_images_to_include`. If the tool returns an error, do not call it \
again; reply with the error text. Do not call shell or file tools, do not create, copy or move \
files, and do not ask questions. After the tool returns, reply with one short line describing the \
result.";

/// `thread/start`, exactly as docs/design.md "Thread and turn parameters" lists it. `config` is the
/// MCP-off map built from a `config/read` made just before (see [`mcp_off_map`]).
///
/// The sandbox and approval policy are part of the child's posture, a security boundary
/// (AGENTS.md): read-only, never ask.
pub fn thread_start_params(cfg: &Config, mcp_off: &Value) -> Value {
    json!({
        "model": cfg.model,
        "cwd": cfg.work_dir,
        "sandbox": "read-only",
        "approvalPolicy": "never",
        "approvalsReviewer": "user",
        "developerInstructions": DEVELOPER_INSTRUCTIONS,
        "config": mcp_off,
        "ephemeral": false,
    })
}

/// `thread/resume`: the thread parameters again, with the MCP-off map built from a fresh
/// `config/read`, plus `threadId` and `excludeTurns: true` (docs/design.md, "Thread and turn
/// parameters"). `ephemeral` is left out: `ThreadResumeParams` has no such field, and the thread
/// was created persistent.
pub fn thread_resume_params(cfg: &Config, thread_id: &str, mcp_off: &Value) -> Value {
    let mut params = thread_start_params(cfg, mcp_off);
    let object = params
        .as_object_mut()
        .expect("thread parameters are an object");
    object.remove("ephemeral");
    object.insert("threadId".to_string(), json!(thread_id));
    object.insert("excludeTurns".to_string(), json!(true));
    params
}

/// The text a generate turn sends: the prompt inside `<image_prompt>`, so it stays apart from
/// anything else, and the reference images, absolute, one per line (docs/design.md, "Input text
/// sent to Codex").
pub fn generate_input_text(prompt: &str, reference_images: &[PathBuf]) -> String {
    let mut text = format!("<image_prompt>\n{prompt}\n</image_prompt>");
    push_references(&mut text, reference_images);
    text
}

/// The text a refine turn sends: the feedback inside `<edit_request>`, the image to edit named in
/// `<edit_target>` (Codex never picks it from memory [decided]), then the reference images.
pub fn refine_input_text(
    feedback: &str,
    edit_target: &Path,
    reference_images: &[PathBuf],
) -> String {
    let mut text = format!(
        "<edit_request>\n{feedback}\n</edit_request>\n<edit_target>{}</edit_target>",
        edit_target.display()
    );
    push_references(&mut text, reference_images);
    text
}

fn push_references(text: &mut String, reference_images: &[PathBuf]) {
    if !reference_images.is_empty() {
        text.push_str("\n<reference_images>");
        for path in reference_images {
            text.push('\n');
            text.push_str(&path.display().to_string());
        }
        text.push_str("\n</reference_images>");
    }
}

/// `turn/start`: one text input, and the agent model and effort. The agent only relays the prompt,
/// so the effort is low by default [decided].
pub fn turn_start_params(cfg: &Config, thread_id: &str, text: &str) -> Value {
    json!({
        "threadId": thread_id,
        "input": [{"type": "text", "text": text, "text_elements": []}],
        "model": cfg.model,
        "effort": cfg.effort,
    })
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// One rate-limit window.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub used_percent: f64,
    pub duration_mins: Option<u64>,
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bucket {
    pub primary: Option<Window>,
    pub secondary: Option<Window>,
}

/// Usage per metered bucket, keyed by `limitId` (docs/design.md, "Usage display").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub buckets: BTreeMap<String, Bucket>,
}

impl Usage {
    /// From an `account/rateLimits/read` reply: the multi-bucket view, plus the single-bucket
    /// view for a server that sends only that.
    pub fn from_read(reply: &Value) -> Self {
        let mut usage = Self::default();
        if let Some(snapshot) = reply.get("rateLimits") {
            usage.merge(snapshot);
        }
        if let Some(by_id) = reply.get("rateLimitsByLimitId").and_then(Value::as_object) {
            for snapshot in by_id.values() {
                usage.merge(snapshot);
            }
        }
        usage
    }

    /// Merge one rate-limit snapshot, such as an `account/rateLimits/updated` notification's.
    /// Updates are sparse: only the fields present and non-null change anything.
    pub fn merge(&mut self, snapshot: &Value) {
        if !snapshot.is_object() {
            return;
        }
        // The single-bucket view predates bucket ids and is the Codex agent bucket.
        let id = snapshot
            .get("limitId")
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .to_string();
        let bucket = self.buckets.entry(id).or_default();
        merge_window(&mut bucket.primary, snapshot.get("primary"));
        merge_window(&mut bucket.secondary, snapshot.get("secondary"));
    }

    /// The display lines: the Codex agent bucket, then the image quota. Other buckets are not
    /// shown; nothing here knows what they meter.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![match self.buckets.get("codex") {
            Some(bucket) => format!("Codex agent usage: {}", bucket_text(bucket)),
            None => "Codex agent usage: not reported".to_string(),
        }];
        lines.push(match self.buckets.get("image_gen") {
            Some(bucket) => format!("image quota: {}", bucket_text(bucket)),
            None => "image quota: not reported".to_string(),
        });
        lines
    }
}

fn merge_window(slot: &mut Option<Window>, update: Option<&Value>) {
    let Some(update) = update.filter(|u| u.is_object()) else {
        return;
    };
    let used = update.get("usedPercent").and_then(Value::as_f64);
    let duration = update.get("windowDurationMins").and_then(Value::as_u64);
    let resets = update.get("resetsAt").and_then(Value::as_i64);
    match slot {
        Some(window) => {
            if let Some(used) = used {
                window.used_percent = used;
            }
            if duration.is_some() {
                window.duration_mins = duration;
            }
            if resets.is_some() {
                window.resets_at = resets;
            }
        }
        None => {
            if let Some(used) = used {
                *slot = Some(Window {
                    used_percent: used,
                    duration_mins: duration,
                    resets_at: resets,
                });
            }
        }
    }
}

fn bucket_text(bucket: &Bucket) -> String {
    let windows: Vec<String> = [&bucket.primary, &bucket.secondary]
        .into_iter()
        .flatten()
        .map(window_text)
        .collect();
    if windows.is_empty() {
        "no windows reported".to_string()
    } else {
        windows.join("; ")
    }
}

/// `weekly 44% (resets 2026-10-01 14:17)`.
pub fn window_text(window: &Window) -> String {
    let label = match window.duration_mins {
        Some(10080) => "weekly".to_string(),
        Some(300) => "5-hour".to_string(),
        Some(n) => format!("{n}-min"),
        None => "window".to_string(),
    };
    let used = if window.used_percent.fract() == 0.0 {
        format!("{:.0}%", window.used_percent)
    } else {
        format!("{:.1}%", window.used_percent)
    };
    match window.resets_at.and_then(local_time) {
        Some(at) => format!("{label} {used} (resets {at})"),
        None => format!("{label} {used}"),
    }
}

// ---------------------------------------------------------------------------
// Local time, for reset times
// ---------------------------------------------------------------------------

#[repr(C)]
struct FileTime {
    low: u32,
    high: u32,
}

/// SYSTEMTIME. Also filled by `GetLocalTime` for automatic session names (output.rs).
#[repr(C)]
#[derive(Default)]
pub(crate) struct SystemTime {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub milliseconds: u16,
}

extern "system" {
    fn FileTimeToSystemTime(file_time: *const FileTime, system_time: *mut SystemTime) -> i32;
    fn SystemTimeToTzSpecificLocalTime(
        time_zone: *const std::ffi::c_void,
        universal: *const SystemTime,
        local: *mut SystemTime,
    ) -> i32;
}

/// Seconds between 1601-01-01 (the FILETIME epoch) and 1970-01-01.
const UNIX_TO_FILETIME_SECS: i64 = 11_644_473_600;

fn utc_system_time(unix_secs: i64) -> Option<SystemTime> {
    let ticks = unix_secs
        .checked_add(UNIX_TO_FILETIME_SECS)?
        .checked_mul(10_000_000)?;
    let ticks = u64::try_from(ticks).ok()?;
    let file_time = FileTime {
        low: ticks as u32,
        high: (ticks >> 32) as u32,
    };
    let mut utc = SystemTime::default();
    // SAFETY: both pointers are to live, correctly laid out FILETIME / SYSTEMTIME values.
    (unsafe { FileTimeToSystemTime(&file_time, &mut utc) } != 0).then_some(utc)
}

fn format_system_time(t: &SystemTime) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute
    )
}

/// A Unix time as `YYYY-MM-DD HH:MM` in the machine's time zone, with its daylight-saving rule
/// for that date; UTC, marked, if the conversion fails.
pub fn local_time(unix_secs: i64) -> Option<String> {
    let utc = utc_system_time(unix_secs)?;
    let mut local = SystemTime::default();
    // SAFETY: a null time zone means the current one; both pointers are to live SYSTEMTIMEs.
    let ok = unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } != 0;
    Some(if ok {
        format_system_time(&local)
    } else {
        format!("{} UTC", format_system_time(&utc))
    })
}

/// A scripted fake Codex for tests: the real reply shapes, recorded from codex-cli 0.156.0 with
/// free calls only, answered from data a test can change.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::appserver::fake::{self, Flow};
    use std::sync::{Arc, Mutex};

    /// The thread and turn ids every fake thread and turn gets.
    pub const THREAD_ID: &str = "019a0000-0000-7000-8000-00000000cafe";
    pub const TURN_ID: &str = "019a0000-0000-7000-8000-00000000beef";

    /// One step of a scripted turn, run in order on a thread of its own once `turn/start` has
    /// arrived, so the fake keeps answering `turn/interrupt` and `thread/unsubscribe` meanwhile.
    #[derive(Clone)]
    pub enum Step {
        Send(Value),
        Sleep(Duration),
        /// Answer the `turn/start` request now (with [`TurnScript::answer_turn_start`] false).
        AnswerTurnStart,
        /// Run arbitrary test code, such as deleting the output folder mid-turn.
        Run(Arc<dyn Fn() + Send + Sync>),
        /// Close the output, as a child that dies does.
        Exit,
        /// The process exits as the client's liveness check sees it, while its output stays open
        /// until an `Exit` step: a real child's exit can be seen before its last lines are read.
        ProcessExits,
    }

    /// What the fake does with a turn.
    #[derive(Clone)]
    pub struct TurnScript {
        /// Sent right after the `thread/start` reply, as Codex sends `thread/started` and MCP
        /// startup statuses there.
        pub on_thread_start: Vec<Value>,
        /// The error `thread/start` answers with, for as many calls as there are entries.
        pub thread_start_errors: Vec<Value>,
        /// The error `thread/resume` answers with, for as many calls as there are entries.
        pub resume_errors: Vec<Value>,
        /// Sent right after a `thread/resume` reply.
        pub on_thread_resume: Vec<Value>,
        /// How long the fake takes to answer a successful `thread/resume`. It answers other
        /// requests meanwhile, and answers the resume even after the client stopped waiting for
        /// it, as Codex does.
        pub resume_delay: Duration,
        /// Whether `turn/start` is answered at once. If not, a step answers it, or nothing does.
        pub answer_turn_start: bool,
        /// The error `turn/start` answers with instead of a turn.
        pub turn_start_error: Option<Value>,
        pub steps: Vec<Step>,
        /// Sent after `turn/interrupt` is answered. Empty: the interrupt is acknowledged, and the
        /// turn never completes.
        pub on_interrupt: Vec<Value>,
    }

    impl Default for TurnScript {
        fn default() -> Self {
            Self {
                on_thread_start: vec![json!({"method": "thread/started",
                    "params": {"thread": {"id": THREAD_ID}}})],
                thread_start_errors: Vec::new(),
                resume_errors: Vec::new(),
                on_thread_resume: Vec::new(),
                resume_delay: Duration::ZERO,
                answer_turn_start: true,
                turn_start_error: None,
                steps: Vec::new(),
                on_interrupt: vec![turn_completed("interrupted", Value::Null)],
            }
        }
    }

    pub fn notification(method: &str, mut params: Value) -> Value {
        if params.get("threadId").is_none() {
            params["threadId"] = json!(THREAD_ID);
        }
        json!({"method": method, "params": params})
    }

    pub fn turn_started() -> Value {
        notification(
            "turn/started",
            json!({"turn": {"id": TURN_ID, "items": [], "status": "inProgress", "error": null}}),
        )
    }

    pub fn image_started(item_id: &str) -> Value {
        notification(
            "item/started",
            json!({"turnId": TURN_ID, "item": {"type": "imageGeneration", "id": item_id,
                   "status": "in_progress", "revisedPrompt": null, "result": "",
                   "transparentBackground": null, "failure": null}}),
        )
    }

    /// A completed image item, shaped as codex-cli 0.156.0 sends it [verified: smoke log].
    /// `saved_path` `None` leaves the field out, as the schema allows.
    pub fn image_completed(
        item_id: &str,
        revised_prompt: &str,
        result_base64: &str,
        saved_path: Option<&Path>,
    ) -> Value {
        let mut item = json!({"type": "imageGeneration", "id": item_id, "status": "completed",
                              "revisedPrompt": revised_prompt, "result": result_base64,
                              "transparentBackground": false, "failure": null});
        if let Some(path) = saved_path {
            item["savedPath"] = json!(path);
        }
        notification("item/completed", json!({"turnId": TURN_ID, "item": item}))
    }

    pub fn image_failed(item_id: &str, failure: Value) -> Value {
        notification(
            "item/completed",
            json!({"turnId": TURN_ID, "item": {"type": "imageGeneration", "id": item_id,
                   "status": "failed", "revisedPrompt": "x", "result": "",
                   "transparentBackground": null, "failure": failure}}),
        )
    }

    pub fn agent_message(text: &str) -> Value {
        notification(
            "item/completed",
            json!({"turnId": TURN_ID, "item": {"type": "agentMessage", "id": "msg_1",
                   "text": text, "phase": "final_answer", "memoryCitation": null}}),
        )
    }

    pub fn turn_completed(status: &str, error: Value) -> Value {
        notification(
            "turn/completed",
            json!({"turn": {"id": TURN_ID, "items": [], "status": status, "error": error}}),
        )
    }

    /// A `TurnError` with the given `codexErrorInfo`.
    pub fn turn_error(info: Value) -> Value {
        json!({"message": "the turn failed upstream", "codexErrorInfo": info,
               "additionalDetails": null})
    }

    #[derive(Clone)]
    pub struct FakeCodex {
        pub user_agent: String,
        pub account: Value,
        pub capabilities: Value,
        /// `model/list` pages, in order; each one's `nextCursor` is filled in.
        pub model_pages: Vec<Vec<Value>>,
        /// The `config/read` config object, or the error it answers with.
        pub config: Result<Value, Value>,
        /// The `origins` object of the `config/read` reply.
        pub origins: Value,
        pub rate_limits: Result<Value, Value>,
        /// A method the fake dies on: it closes its output instead of answering, as a child
        /// that exits while handling the request does.
        pub exits_on: Option<&'static str>,
        /// The `codexHome` the handshake reports.
        pub codex_home: String,
        /// The error `thread/delete` answers with, by thread id. Any other thread is deleted.
        pub delete_errors: std::collections::HashMap<String, Value>,
        pub turn: TurnScript,
        /// Every message the client sent, in order.
        pub seen: Arc<Mutex<Vec<Value>>>,
    }

    /// The `codexHome` a default fake reports.
    pub const CODEX_HOME: &str = r"C:\Users\someone\.codex";

    pub fn model(id: &str, hidden: bool) -> Value {
        json!({"id": id, "model": id, "upgrade": null, "displayName": id, "description": "",
               "hidden": hidden, "supportedReasoningEfforts": [], "defaultReasoningEffort": "low",
               "inputModalities": ["text", "image"], "isDefault": false})
    }

    /// The effective config a real ambient home reported under the design's spawn line
    /// (config/read on codex-cli 0.156.0), trimmed to the keys preflight reads.
    pub fn healthy_config() -> Value {
        json!({
            "features": {
                "network_proxy": null, "apps": false, "browser_use": false, "chronicle": false,
                "computer_use": false, "goals": false, "hooks": false, "image_generation": true,
                "in_app_browser": false, "js_repl": false, "memories": false,
                "multi_agent": false, "multi_agent_v2": false, "plugins": false,
                "shell_tool": false, "skill_search": false, "tool_suggest": false,
                "api_key_model_discovery": false, "auth_elicitation": true, "mentions_v2": true,
                "remote_control": false, "remote_plugin": true, "windows_sandbox_service": false
            },
            "web_search": "disabled",
            "notify": [],
            "skills": {"bundled": {"enabled": false}, "include_instructions": false},
            "agents": {"enabled": false, "max_concurrent_threads_per_session": null,
                       "max_depth": null, "default_subagent_model": null,
                       "default_subagent_reasoning_effort": null,
                       "job_max_runtime_seconds": null, "interrupt_message": null},
            "tools": {"web_search": null},
            "approvals_reviewer": "user",
            "windows": {"sandbox": "unelevated"},
            "thread_unload_delay_secs": 5,
            "profile": null,
            "profiles": {},
            "mcp_servers": {"node_repl": {"enabled": true}, "cua_repl": {"enabled": true}}
        })
    }

    /// `origins` from the same reply, trimmed to the entry preflight reads.
    pub fn healthy_origins() -> Value {
        json!({
            "tools.experimental_request_user_input.enabled": {
                "name": {"type": "sessionFlags"},
                "version": "sha256:82c64ff791dafc9ad598a4f0e798195e08ad6d72e71d5f4b4a1fed569481e88f"
            }
        })
    }

    impl Default for FakeCodex {
        fn default() -> Self {
            Self {
                user_agent: "codex-imagegen/0.156.0 (Windows 10.0.26200; x86_64) \
                             xterm-256color (codex-imagegen; 0.1.0)"
                    .to_string(),
                account: json!({"account": {"type": "chatgpt", "email": "someone@example.com",
                                            "planType": "pro"},
                                "requiresOpenaiAuth": true, "workspaceRouting": null}),
                capabilities: json!({"namespaceTools": true, "imageGeneration": true,
                                     "webSearch": true}),
                model_pages: vec![vec![
                    model("gpt-6-astra", false),
                    model("gpt-5.6-sol", false),
                ]],
                config: Ok(healthy_config()),
                origins: healthy_origins(),
                rate_limits: Ok(json!({
                    "ordinaryUsageAllowed": true,
                    "rateLimits": {"limitId": "codex", "primary": {"usedPercent": 44,
                        "windowDurationMins": 10080, "resetsAt": 1790710629}, "secondary": null},
                    "rateLimitsByLimitId": {"codex": {"limitId": "codex", "primary":
                        {"usedPercent": 44, "windowDurationMins": 10080, "resetsAt": 1790710629},
                        "secondary": null}}
                })),
                exits_on: None,
                codex_home: CODEX_HOME.to_string(),
                delete_errors: std::collections::HashMap::new(),
                turn: TurnScript::default(),
                seen: Arc::default(),
            }
        }
    }

    impl FakeCodex {
        pub fn answer(&self, message: &Value) -> Option<Value> {
            let id = message.get("id")?;
            let method = message.get("method")?.as_str()?;
            let ok = |result: Value| Some(json!({"id": id, "result": result}));
            let err = |error: &Value| Some(json!({"id": id, "error": error}));
            match method {
                "initialize" => ok(json!({"userAgent": self.user_agent,
                                          "codexHome": self.codex_home,
                                          "platformFamily": "windows", "platformOs": "windows"})),
                "thread/delete" => {
                    let thread = message["params"]["threadId"].as_str().unwrap_or("");
                    match self.delete_errors.get(thread) {
                        Some(error) => err(error),
                        None => ok(json!({})),
                    }
                }
                "account/read" => ok(self.account.clone()),
                "modelProvider/capabilities/read" => ok(self.capabilities.clone()),
                "model/list" => {
                    let index = match message["params"].get("cursor").and_then(Value::as_str) {
                        None => 0,
                        Some(cursor) => cursor.strip_prefix("page-")?.parse().ok()?,
                    };
                    let data = self.model_pages.get(index).cloned().unwrap_or_default();
                    let next =
                        (index + 1 < self.model_pages.len()).then(|| format!("page-{}", index + 1));
                    ok(json!({"data": data, "nextCursor": next}))
                }
                "config/read" => match &self.config {
                    Ok(config) => ok(json!({"config": config, "origins": self.origins,
                                            "layers": null})),
                    Err(error) => err(error),
                },
                "account/rateLimits/read" => match &self.rate_limits {
                    Ok(limits) => ok(limits.clone()),
                    Err(error) => err(error),
                },
                _ => err(&json!({"code": -32601, "message": "unknown method"})),
            }
        }

        pub fn connect(self) -> AppServer {
            let mut thread_start_errors = self.turn.thread_start_errors.clone().into_iter();
            let mut resume_errors = self.turn.resume_errors.clone().into_iter();
            fake::connect(move |message, out| {
                self.seen.lock().unwrap().push(message.clone());
                if self
                    .exits_on
                    .is_some_and(|method| message["method"] == method)
                {
                    return Flow::Exit;
                }
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                match message["method"].as_str().unwrap_or("") {
                    "thread/start" => {
                        if let Some(error) = thread_start_errors.next() {
                            out.send(json!({"id": id, "error": error}));
                            return Flow::Continue;
                        }
                        out.send(json!({"id": id, "result": {
                            "thread": {"id": THREAD_ID, "ephemeral": false, "turns": []},
                            "model": message["params"]["model"], "cwd": message["params"]["cwd"],
                            "approvalPolicy": "never", "approvalsReviewer": "user",
                            "sandbox": {"type": "readOnly"}, "reasoningEffort": "low"}}));
                        for note in &self.turn.on_thread_start {
                            out.send(note.clone());
                        }
                    }
                    "thread/resume" => {
                        if let Some(error) = resume_errors.next() {
                            out.send(json!({"id": id, "error": error}));
                            return Flow::Continue;
                        }
                        let reply = json!({"id": id, "result": {
                            "thread": {"id": message["params"]["threadId"], "ephemeral": false,
                                       "turns": []},
                            "model": message["params"]["model"], "cwd": message["params"]["cwd"],
                            "approvalPolicy": "never", "approvalsReviewer": "user",
                            "sandbox": {"type": "readOnly"}, "reasoningEffort": "low"}});
                        let notes = self.turn.on_thread_resume.clone();
                        let delay = self.turn.resume_delay;
                        let out = out.clone();
                        let answer = move || {
                            std::thread::sleep(delay);
                            out.send(reply);
                            for note in notes {
                                out.send(note);
                            }
                        };
                        if delay.is_zero() {
                            answer();
                        } else {
                            std::thread::spawn(answer);
                        }
                    }
                    "turn/start" => {
                        if let Some(error) = &self.turn.turn_start_error {
                            out.send(json!({"id": id, "error": error}));
                            return Flow::Continue;
                        }
                        let reply = json!({"id": id, "result": {"turn": {"id": TURN_ID,
                            "items": [], "status": "inProgress", "error": null}}});
                        if self.turn.answer_turn_start {
                            out.send(reply.clone());
                        }
                        let steps = self.turn.steps.clone();
                        let out = out.clone();
                        std::thread::spawn(move || {
                            for step in steps {
                                match step {
                                    Step::Send(message) => out.send(message),
                                    Step::Sleep(pause) => std::thread::sleep(pause),
                                    Step::AnswerTurnStart => out.send(reply.clone()),
                                    Step::Run(f) => f(),
                                    Step::Exit => {
                                        out.close();
                                        return;
                                    }
                                    Step::ProcessExits => out.exit_process(),
                                }
                            }
                        });
                    }
                    "turn/interrupt" => {
                        out.send(json!({"id": id, "result": {}}));
                        for note in &self.turn.on_interrupt {
                            out.send(note.clone());
                        }
                    }
                    "thread/unsubscribe" => {
                        out.send(json!({"id": id, "result": {"status": "unsubscribed"}}));
                    }
                    _ => {
                        if let Some(reply) = self.answer(message) {
                            out.send(reply);
                        }
                    }
                }
                Flow::Continue
            })
        }

        /// The requests the client sent with `method`, params only, in order.
        pub fn sent(seen: &Mutex<Vec<Value>>, method: &str) -> Vec<Value> {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|m| m["method"] == method)
                .map(|m| m["params"].clone())
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{healthy_config, healthy_origins, model, FakeCodex};
    use super::*;
    use crate::config::Env;
    use std::sync::Arc;

    fn cfg(extra: &[&str]) -> Config {
        let args: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        Config::parse(
            &args,
            &Env {
                cwd: PathBuf::from(r"C:\definitely-not-here\proj"),
                imagegen_home: Some(OsString::from(r"C:\definitely-not-here\state")),
                user_profile: None,
            },
        )
        .unwrap()
    }

    fn rpc(server: &AppServer) -> Rpc<'_> {
        Rpc {
            server,
            cancel: None,
            per_call: Duration::from_secs(10),
            budget: None,
        }
    }

    /// Run the handshake and preflight against `fake`.
    fn run(fake: FakeCodex, cfg: &Config) -> (Result<(), Failure>, Facts) {
        let server = fake.connect();
        let rpc = rpc(&server);
        let handshake = initialize(&rpc).expect("handshake");
        let mut facts = Facts::default();
        let result = preflight(&rpc, &handshake, cfg, &mut facts);
        (result, facts)
    }

    #[test]
    fn a_codex_home_that_is_missing_or_a_file_is_refused_before_codex_starts() {
        let dir = crate::testutil::temp_dir("codex-home");
        let file = dir.join("a-file");
        std::fs::write(&file, b"x").unwrap();
        let mut config = cfg(&[]);
        config.work_dir = dir.join("work");
        // Never started: the check comes first.
        let bin = dir.join("never-run.exe");
        for (home, why) in [
            (dir.join("missing"), "does not exist"),
            (file, "is not a folder"),
        ] {
            config.codex_home = Some(home.clone());
            let failure = spawn(&bin, &config).err().expect("refused");
            assert_eq!(failure.code, "SPAWN_FAILED");
            assert!(
                failure
                    .summary
                    .contains(&format!("{} {why}", home.display())),
                "{}",
                failure.summary
            );
            assert!(failure
                .remediation
                .contains("New-Item -ItemType Directory -Force"));
            assert!(failure.remediation.contains("$env:CODEX_HOME"));
        }
    }

    #[test]
    fn the_spawn_line_is_exactly_the_designs() {
        let expected = "app-server --listen stdio:// --enable image_generation \
            --disable apps --disable plugins --disable hooks --disable memories \
            --disable multi_agent --disable multi_agent_v2 --disable goals --disable shell_tool \
            --disable tool_suggest --disable skill_search --disable browser_use \
            --disable computer_use --disable in_app_browser -c notify=[] \
            -c skills.bundled.enabled=false -c skills.include_instructions=false \
            -c agents.enabled=false -c tools.experimental_request_user_input.enabled=false \
            -c web_search=\"disabled\" -c approvals_reviewer=\"user\" \
            -c windows.sandbox=\"unelevated\" -c thread_unload_delay_secs=5";
        assert_eq!(
            spawn_args(),
            expected.split_whitespace().collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_spawn_spec_strips_api_keys_and_sets_codex_home_only_when_asked() {
        let ambient = spawn_spec(Path::new(r"C:\codex.exe"), &cfg(&[]));
        assert_eq!(
            ambient.env_remove,
            vec!["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"]
        );
        assert!(ambient.env_set.is_empty());
        assert_eq!(
            ambient.cwd,
            PathBuf::from(r"C:\definitely-not-here\state\work")
        );

        let dedicated = spawn_spec(
            Path::new(r"C:\codex.exe"),
            &cfg(&["--codex-home", r"D:\codex-home"]),
        );
        assert_eq!(
            dedicated.env_set,
            vec![("CODEX_HOME", OsString::from(r"D:\codex-home"))]
        );
    }

    /// The environment edits a `Command` carries, as (name, Some(value)) for a set and
    /// (name, None) for a removal.
    fn env_edits(cmd: &std::process::Command) -> Vec<(String, Option<String>)> {
        cmd.get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.map(|v| v.to_string_lossy().to_string()),
                )
            })
            .collect()
    }

    #[test]
    fn the_child_command_loses_the_api_keys_and_runs_in_the_work_dir() {
        let bin = Path::new(r"C:\tools\codex.exe");
        for (flags, home) in [
            (
                &["--codex-home", r"D:\codex-home"][..],
                Some(r"D:\codex-home"),
            ),
            (&[][..], None),
        ] {
            let cfg = cfg(flags);
            let cmd = crate::appserver::command(&spawn_spec(bin, &cfg));
            assert_eq!(cmd.get_program(), bin.as_os_str());
            let args: Vec<String> = cmd
                .get_args()
                .map(|a| a.to_string_lossy().to_string())
                .collect();
            assert_eq!(args, spawn_args());
            assert_eq!(cmd.get_current_dir(), Some(cfg.work_dir.as_path()));

            let edits = env_edits(&cmd);
            for name in ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"] {
                assert!(
                    edits.contains(&(name.to_string(), None)),
                    "{name} is not removed: {edits:?}"
                );
            }
            let codex_home: Vec<&Option<String>> = edits
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("CODEX_HOME"))
                .map(|(_, v)| v)
                .collect();
            match home {
                Some(home) => assert_eq!(codex_home, vec![&Some(home.to_string())]),
                // Ambient mode leaves CODEX_HOME exactly as the user has it.
                None => assert!(codex_home.is_empty(), "{edits:?}"),
            }
        }
    }

    #[test]
    fn initialize_sends_exactly_the_designs_params_then_initialized() {
        let fake = FakeCodex::default();
        let seen = Arc::clone(&fake.seen);
        let server = fake.connect();
        let handshake = initialize(&rpc(&server)).unwrap();
        assert_eq!(handshake.codex_version.as_deref(), Some("0.156.0"));
        assert!(handshake.in_tested_range());
        assert_eq!(
            handshake.codex_home,
            PathBuf::from(r"C:\Users\someone\.codex")
        );

        // `initialized` is a notification, so nothing waits for the fake to have read it.
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.lock().unwrap().len() < 2 {
            assert!(Instant::now() < deadline, "initialized never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0]["method"], "initialize");
        assert_eq!(
            seen[0]["params"],
            json!({
                "clientInfo": {"name": "codex-imagegen", "title": null, "version": crate::VERSION},
                "capabilities": {
                    "experimentalApi": false,
                    "requestAttestation": false,
                    "optOutNotificationMethods": [
                        "item/agentMessage/delta", "item/reasoning/summaryTextDelta",
                        "item/reasoning/summaryPartAdded", "item/reasoning/textDelta",
                        "item/plan/delta", "turn/plan/updated", "turn/diff/updated",
                        "thread/tokenUsage/updated", "remoteControl/status/changed",
                        "skills/changed"
                    ]
                }
            })
        );
        // The canary must keep flowing.
        assert!(!OPT_OUT_NOTIFICATIONS.contains(&"mcpServer/startupStatus/updated"));
        assert_eq!(seen[1], json!({"method": "initialized"}));
    }

    #[test]
    fn a_healthy_setup_passes_and_records_what_it_found() {
        let fake = FakeCodex::default();
        let seen = Arc::clone(&fake.seen);
        let (result, facts) = run(fake, &cfg(&[]));
        result.expect("preflight passes");
        assert_eq!(
            facts.account,
            Some(AccountFacts {
                kind: "chatgpt".into(),
                plan: Some("pro".into())
            })
        );
        assert_eq!(facts.image_capability, Some(true));
        assert_eq!(facts.model, Some(ModelFacts { hidden: false }));
        assert_eq!(
            facts.mcp_off_map,
            Some(json!({"mcp_servers": {"cua_repl": {"enabled": false},
                                        "node_repl": {"enabled": false}}}))
        );
        // The calls, in the design's order, each free.
        let methods: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("id").and(m["method"].as_str().map(str::to_string)))
            .collect();
        assert_eq!(
            methods,
            vec![
                "initialize",
                "account/read",
                "modelProvider/capabilities/read",
                "model/list",
                "config/read"
            ]
        );
    }

    #[test]
    fn no_account_is_not_authenticated() {
        let fake = FakeCodex {
            account: json!({"account": null, "requiresOpenaiAuth": true,
                            "workspaceRouting": null}),
            ..FakeCodex::default()
        };
        let (result, facts) = run(fake, &cfg(&[]));
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "NOT_AUTHENTICATED");
        assert!(failure.remediation.contains("codex login"));
        assert_eq!(facts.account, None);
    }

    #[test]
    fn a_dedicated_home_is_named_when_it_is_not_signed_in() {
        let fake = FakeCodex {
            account: json!({"account": null, "requiresOpenaiAuth": true}),
            ..FakeCodex::default()
        };
        let (result, _) = run(fake, &cfg(&["--codex-home", r"D:\imagegen-home"]));
        assert!(result
            .unwrap_err()
            .remediation
            .contains(r"D:\imagegen-home"));
    }

    #[test]
    fn an_api_key_account_is_refused() {
        let fake = FakeCodex {
            account: json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": true}),
            ..FakeCodex::default()
        };
        let (result, facts) = run(fake, &cfg(&[]));
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "NOT_AUTHENTICATED");
        assert!(failure.summary.contains("API-key auth is not supported"));
        assert_eq!(facts.account.unwrap().kind, "apiKey");
    }

    #[test]
    fn the_free_plan_has_no_image_generation() {
        let fake = FakeCodex {
            account: json!({"account": {"type": "chatgpt", "email": null, "planType": "free"}}),
            ..FakeCodex::default()
        };
        let (result, _) = run(fake, &cfg(&[]));
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "IMAGEGEN_UNAVAILABLE");
        assert!(failure.summary.contains("'free'"));
    }

    #[test]
    fn a_provider_without_image_capability_is_unavailable() {
        let fake = FakeCodex {
            capabilities: json!({"namespaceTools": true, "imageGeneration": false,
                                 "webSearch": true}),
            ..FakeCodex::default()
        };
        let (result, facts) = run(fake, &cfg(&[]));
        assert_eq!(result.unwrap_err().code, "IMAGEGEN_UNAVAILABLE");
        assert_eq!(facts.image_capability, Some(false));
    }

    #[test]
    fn a_missing_model_is_unavailable_and_the_offered_ones_are_listed() {
        let fake = FakeCodex {
            model_pages: vec![vec![model("gpt-5.6-sol", false)]],
            ..FakeCodex::default()
        };
        let (result, _) = run(fake, &cfg(&[]));
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "MODEL_UNAVAILABLE");
        assert!(failure.summary.contains("gpt-6-astra"));
        assert!(failure.detail.unwrap().contains("gpt-5.6-sol"));
    }

    #[test]
    fn a_hidden_model_passes_and_is_noted() {
        let fake = FakeCodex {
            model_pages: vec![vec![model("gpt-6-astra", true)]],
            ..FakeCodex::default()
        };
        let (result, facts) = run(fake, &cfg(&[]));
        result.unwrap();
        assert_eq!(facts.model, Some(ModelFacts { hidden: true }));
    }

    #[test]
    fn the_model_is_found_on_a_later_page_with_hidden_models_included() {
        let fake = FakeCodex {
            model_pages: vec![
                vec![model("gpt-5.6-sol", false)],
                vec![model("gpt-5.6-luna", false)],
                vec![model("gpt-6-astra", false)],
            ],
            ..FakeCodex::default()
        };
        let seen = Arc::clone(&fake.seen);
        let (result, _) = run(fake, &cfg(&[]));
        result.unwrap();
        let lists: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["method"] == "model/list")
            .map(|m| m["params"].clone())
            .collect();
        assert_eq!(
            lists,
            vec![
                json!({"includeHidden": true}),
                json!({"includeHidden": true, "cursor": "page-1"}),
                json!({"includeHidden": true, "cursor": "page-2"}),
            ]
        );
    }

    #[test]
    fn a_configured_model_is_checked_rather_than_the_default() {
        let (result, _) = run(FakeCodex::default(), &cfg(&["--model", "gpt-5.6-sol"]));
        result.unwrap();
        let (result, _) = run(FakeCodex::default(), &cfg(&["--model", "gpt-9"]));
        assert_eq!(result.unwrap_err().code, "MODEL_UNAVAILABLE");
    }

    fn with_config(edit: impl FnOnce(&mut Value)) -> FakeCodex {
        let mut config = healthy_config();
        edit(&mut config);
        FakeCodex {
            config: Ok(config),
            ..FakeCodex::default()
        }
    }

    #[test]
    fn a_setting_that_re_disables_image_generation_is_named() {
        let fake = with_config(|c| c["features"]["image_generation"] = json!(false));
        let (result, _) = run(fake, &cfg(&[]));
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "IMAGEGEN_UNAVAILABLE");
        assert!(
            failure
                .summary
                .contains("features.image_generation = false"),
            "{}",
            failure.summary
        );
    }

    #[test]
    fn a_switched_off_feature_turned_back_on_is_named() {
        for feature in DISABLED_FEATURES {
            let fake = with_config(|c| c["features"][*feature] = json!(true));
            let (result, _) = run(fake, &cfg(&[]));
            let failure = result.unwrap_err();
            assert_eq!(failure.code, "IMAGEGEN_UNAVAILABLE");
            assert!(
                failure
                    .summary
                    .contains(&format!("features.{feature} = true")),
                "{}",
                failure.summary
            );
        }
    }

    #[test]
    fn a_legacy_alias_that_beats_a_disable_is_named() {
        // What codex-cli 0.156.0 reported for a user config with `connectors = true`: the
        // canonical key still reads false, and Codex applies the alias after it.
        let fake = with_config(|c| c["features"]["connectors"] = json!(true));
        let (result, _) = run(fake, &cfg(&[]));
        assert!(result
            .unwrap_err()
            .summary
            .contains("features.connectors = true"));
        // An alias set to false changes nothing.
        let fake = with_config(|c| c["features"]["collab"] = json!(false));
        run(fake, &cfg(&[])).0.unwrap();
    }

    #[test]
    fn a_switch_that_is_not_in_effect_fails_closed() {
        let fake = with_config(|c| {
            c["features"].as_object_mut().unwrap().remove("shell_tool");
        });
        let (result, _) = run(fake, &cfg(&[]));
        assert!(result
            .unwrap_err()
            .summary
            .contains("features.shell_tool (not in effect"));

        let fake = with_config(|c| c["web_search"] = json!("live"));
        let (result, _) = run(fake, &cfg(&[]));
        assert!(result
            .unwrap_err()
            .summary
            .contains(r#"web_search = "live""#));

        let fake = with_config(|c| c["windows"]["sandbox"] = json!("elevated"));
        assert!(run(fake, &cfg(&[])).0.is_err());
    }

    fn summary_of(fake: FakeCodex) -> String {
        let (result, _) = run(fake, &cfg(&[]));
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "IMAGEGEN_UNAVAILABLE");
        failure.summary
    }

    #[test]
    fn the_sub_agent_switches_are_checked() {
        // A config that turns sub-agents back on, or a layer that leaves the setting out.
        let fake = with_config(|c| c["agents"]["enabled"] = json!(true));
        assert!(summary_of(fake).contains("agents.enabled = true"));
        let fake = with_config(|c| {
            c.as_object_mut().unwrap().remove("agents");
        });
        assert!(summary_of(fake).contains("agents.enabled (not in effect"));

        // An enabled multi_agent_v2 outranks agents.enabled = false, so it fails as a bare
        // value (the loop in a_switched_off_feature_turned_back_on_is_named) or as a table,
        // which is what a user's `[features.multi_agent_v2]` becomes...
        let fake = with_config(|c| {
            c["features"]["multi_agent_v2"] =
                json!({"enabled": true, "max_concurrent_threads_per_session": 3})
        });
        assert!(summary_of(fake).contains("features.multi_agent_v2 = {"));
        // ...while the table codex-cli 0.156.0 reported for `--disable multi_agent_v2` over such
        // a user table is off.
        let fake = with_config(|c| {
            c["features"]["multi_agent_v2"] =
                json!({"enabled": false, "max_concurrent_threads_per_session": 3})
        });
        run(fake, &cfg(&[])).0.unwrap();
    }

    #[test]
    fn the_user_input_switch_is_checked_by_its_origin() {
        // config/read's config object never carries the setting, so its origin must be the
        // child's own switches.
        let managed = FakeCodex {
            origins: json!({"tools.experimental_request_user_input.enabled": {
                "name": {"type": "legacyManagedConfigTomlFromFile",
                         "file": r"C:\ProgramData\OpenAI\Codex\managed_config.toml"},
                "version": "sha256:0"}}),
            ..FakeCodex::default()
        };
        let summary = summary_of(managed);
        assert!(
            summary.contains("tools.experimental_request_user_input.enabled (taken from")
                && summary.contains("legacyManagedConfigTomlFromFile"),
            "{summary}"
        );

        let missing = FakeCodex {
            origins: json!({}),
            ..FakeCodex::default()
        };
        assert!(summary_of(missing)
            .contains("tools.experimental_request_user_input.enabled (not in effect"));

        // What codex-cli 0.156.0 reported under the spawn line passes.
        let healthy = FakeCodex {
            origins: healthy_origins(),
            ..FakeCodex::default()
        };
        run(healthy, &cfg(&[])).0.unwrap();
    }

    #[test]
    fn a_configuration_codex_cannot_read_fails_closed() {
        // What codex-cli 0.156.0 answered for a config.toml with a legacy `profile` key.
        let fake = FakeCodex {
            config: Err(json!({"code": -32603, "message":
                "failed to resolve feature override precedence: legacy `profile = \"x\"` config \
                 is no longer supported; use `--profile x` with `x.config.toml` instead"})),
            ..FakeCodex::default()
        };
        let (result, _) = run(fake, &cfg(&[]));
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "IMAGEGEN_UNAVAILABLE");
        assert!(failure.detail.unwrap().contains("legacy `profile"));
    }

    #[test]
    fn config_read_is_asked_from_the_work_directory() {
        let fake = FakeCodex::default();
        let seen = Arc::clone(&fake.seen);
        run(fake, &cfg(&[])).0.unwrap();
        let params = seen
            .lock()
            .unwrap()
            .iter()
            .find(|m| m["method"] == "config/read")
            .unwrap()["params"]
            .clone();
        assert_eq!(params, json!({"cwd": r"C:\definitely-not-here\state\work"}));
    }

    #[test]
    fn a_reply_shape_it_cannot_read_names_the_codex_version() {
        let fake = FakeCodex {
            capabilities: json!({"imageGeneration": "yes"}),
            ..FakeCodex::default()
        };
        let (result, _) = run(fake, &cfg(&[]));
        let failure = result.unwrap_err();
        assert_eq!(failure.code, "APP_SERVER_FAILED");
        assert!(failure.detail.unwrap().contains("codex-cli 0.156.0"));
    }

    #[test]
    fn the_mcp_off_map_names_every_server_and_nothing_else() {
        assert_eq!(
            mcp_off_map(&json!({"mcp_servers": {"a": {"command": "x"}, "b": {}}})),
            json!({"mcp_servers": {"a": {"enabled": false}, "b": {"enabled": false}}})
        );
        assert_eq!(mcp_off_map(&json!({})), json!({"mcp_servers": {}}));
        assert_eq!(
            mcp_off_map(&json!({"mcp_servers": null})),
            json!({"mcp_servers": {}})
        );
    }

    #[test]
    fn the_developer_instructions_are_the_designs_word_for_word() {
        let design = include_str!("../docs/design.md");
        let section = &design[design
            .find("### developerInstructions")
            .expect("the design has a developerInstructions section")..];
        let quoted: Vec<&str> = section
            .lines()
            .skip_while(|l| !l.starts_with('>'))
            .take_while(|l| l.starts_with('>'))
            .map(|l| l.trim_start_matches('>').trim())
            .collect();
        assert_eq!(quoted.join(" "), DEVELOPER_INSTRUCTIONS);
    }

    #[test]
    fn thread_and_turn_parameters_are_exactly_the_designs() {
        let cfg = cfg(&[]);
        let map = json!({"mcp_servers": {"x": {"enabled": false}}});
        assert_eq!(
            thread_start_params(&cfg, &map),
            json!({
                "model": "gpt-6-astra",
                "cwd": r"C:\definitely-not-here\state\work",
                "sandbox": "read-only",
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "developerInstructions": DEVELOPER_INSTRUCTIONS,
                "config": {"mcp_servers": {"x": {"enabled": false}}},
                "ephemeral": false,
            })
        );
        assert_eq!(
            thread_resume_params(&cfg, "t-1", &map),
            json!({
                "threadId": "t-1",
                "excludeTurns": true,
                "model": "gpt-6-astra",
                "cwd": r"C:\definitely-not-here\state\work",
                "sandbox": "read-only",
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "developerInstructions": DEVELOPER_INSTRUCTIONS,
                "config": {"mcp_servers": {"x": {"enabled": false}}},
            })
        );
        assert_eq!(
            turn_start_params(&cfg, "t-1", "hello"),
            json!({"threadId": "t-1",
                   "input": [{"type": "text", "text": "hello", "text_elements": []}],
                   "model": "gpt-6-astra", "effort": "low"})
        );
        let medium = super::Config {
            effort: "medium".to_string(),
            ..cfg
        };
        assert_eq!(turn_start_params(&medium, "t", "x")["effort"], "medium");
    }

    #[test]
    fn the_input_text_tags_the_prompt_and_lists_references_only_when_given() {
        let prompt = "a \"fox\"\nwith a \\ and ü";
        assert_eq!(
            generate_input_text(prompt, &[]),
            format!("<image_prompt>\n{prompt}\n</image_prompt>")
        );
        let refs = [PathBuf::from(r"C:\a.png"), PathBuf::from(r"D:\b c\d.jpg")];
        assert_eq!(
            generate_input_text("p", &refs),
            "<image_prompt>\np\n</image_prompt>\n<reference_images>\nC:\\a.png\nD:\\b c\\d.jpg\n\
             </reference_images>"
        );
        let target = Path::new(r"C:\codex\generated_images\t\exec-1.png");
        assert_eq!(
            refine_input_text(prompt, target, &[]),
            format!(
                "<edit_request>\n{prompt}\n</edit_request>\n\
                 <edit_target>C:\\codex\\generated_images\\t\\exec-1.png</edit_target>"
            )
        );
        assert_eq!(
            refine_input_text("f", target, &refs[..1]),
            "<edit_request>\nf\n</edit_request>\n\
             <edit_target>C:\\codex\\generated_images\\t\\exec-1.png</edit_target>\n\
             <reference_images>\nC:\\a.png\n</reference_images>"
        );
    }

    #[test]
    fn versions_are_parsed_from_the_user_agent_and_checked_against_the_range() {
        let ua = "codex-imagegen/0.156.0 (Windows 10.0.26200; x86_64) xterm-256color \
                  (codex-imagegen; 0.1.0)";
        assert_eq!(parse_version(ua).as_deref(), Some("0.156.0"));
        assert_eq!(
            parse_version("codex-imagegen/0.157.2-alpha.1 (x)").as_deref(),
            Some("0.157.2-alpha.1")
        );
        assert_eq!(
            parse_version("codex-imagegen/0.156.3").as_deref(),
            Some("0.156.3")
        );
        assert_eq!(parse_version("no slash here"), None);
        assert_eq!(parse_version("codex-imagegen/ (x)"), None);
        assert_eq!(parse_version("codex-imagegen/(Windows)"), None);

        assert!(in_tested_range("0.156.0"));
        assert!(in_tested_range("0.156.12"));
        assert!(in_tested_range("0.156.1-alpha.2"));
        assert!(!in_tested_range("0.157.0"));
        assert!(!in_tested_range("0.15.6"));
        assert!(!in_tested_range("1.156.0"));
        assert!(!in_tested_range("garbage"));
    }

    #[test]
    fn a_call_that_misses_its_deadline_names_the_method() {
        let server = crate::appserver::fake::connect(|_, _| crate::appserver::fake::Flow::Continue);
        let rpc = Rpc {
            server: &server,
            cancel: None,
            per_call: Duration::from_millis(50),
            budget: None,
        };
        let failure = rpc.call("account/read", json!({})).unwrap_err();
        assert_eq!(failure.code, "APP_SERVER_FAILED");
        assert!(failure.summary.contains("account/read"));
    }

    #[test]
    fn a_call_that_outlives_the_whole_budget_is_a_timeout() {
        let server = crate::appserver::fake::connect(|_, _| crate::appserver::fake::Flow::Continue);
        let rpc = Rpc {
            server: &server,
            cancel: None,
            per_call: Duration::from_secs(30),
            budget: Some(Budget {
                deadline: Instant::now() + Duration::from_millis(50),
                secs: 300,
            }),
        };
        let failure = rpc.call("account/read", json!({})).unwrap_err();
        assert_eq!(failure.code, "TIMEOUT");
        assert!(failure.summary.contains("300-second"));
    }

    #[test]
    fn an_explicit_bin_that_does_not_exist_is_cli_not_found() {
        let failure = resolve_bin(Some(Path::new(r"C:\nope\codex.exe"))).unwrap_err();
        assert_eq!(failure.code, "CLI_NOT_FOUND");
        assert!(failure.summary.contains(r"C:\nope\codex.exe"));
        assert!(failure.summary.contains("--codex-bin"));
        let text = failure.render_for_agent();
        assert!(text.contains("ACTION REQUIRED"));
    }

    #[test]
    fn an_explicit_bin_that_exists_is_used_as_an_absolute_path() {
        let dir = crate::testutil::temp_dir("codex-bin");
        let bin = dir.join("codex.exe");
        std::fs::write(&bin, b"MZ").unwrap();
        assert_eq!(resolve_bin(Some(&bin)).unwrap(), bin);
        // A directory is not a binary.
        assert!(resolve_bin(Some(&dir)).is_err());
    }

    #[test]
    fn path_extensions_put_exe_first_and_have_no_bare_entry() {
        let exts = path_exts();
        assert_eq!(exts.first().map(String::as_str), Some(".exe"));
        assert!(exts.iter().all(|e| e.starts_with('.') && e.len() > 1));
    }

    #[test]
    fn usage_is_labelled_per_the_design() {
        let usage = Usage::from_read(&json!({
            "rateLimits": {"limitId": "codex", "primary": {"usedPercent": 44,
                "windowDurationMins": 10080, "resetsAt": null}, "secondary": {"usedPercent": 12.5,
                "windowDurationMins": 300, "resetsAt": null}},
            "rateLimitsByLimitId": {
                "codex": {"limitId": "codex", "primary": {"usedPercent": 44,
                    "windowDurationMins": 10080, "resetsAt": null}, "secondary": {"usedPercent": 12.5,
                    "windowDurationMins": 300, "resetsAt": null}},
                "image_gen": {"limitId": "image_gen", "primary": {"usedPercent": 30,
                    "windowDurationMins": 1440, "resetsAt": null}, "secondary": null},
                "other": {"limitId": "other", "primary": {"usedPercent": 1,
                    "windowDurationMins": 60, "resetsAt": null}}
            }
        }));
        assert_eq!(
            usage.lines(),
            vec![
                "Codex agent usage: weekly 44%; 5-hour 12.5%".to_string(),
                "image quota: 1440-min 30%".to_string(),
            ]
        );
        let bare = Usage::from_read(&json!({"rateLimits": {"limitId": "codex",
            "primary": {"usedPercent": 3, "windowDurationMins": null, "resetsAt": null}}}));
        assert_eq!(
            bare.lines(),
            vec![
                "Codex agent usage: window 3%".to_string(),
                "image quota: not reported".to_string()
            ]
        );
        assert_eq!(
            Usage::default().lines()[0],
            "Codex agent usage: not reported"
        );
    }

    #[test]
    fn a_sparse_update_merges_only_its_non_null_fields() {
        let mut usage = Usage::from_read(&json!({"rateLimits": {"limitId": "codex",
            "primary": {"usedPercent": 44, "windowDurationMins": 10080, "resetsAt": 1790710629},
            "secondary": {"usedPercent": 5, "windowDurationMins": 300, "resetsAt": 1790000000}}}));
        usage.merge(&json!({"limitId": "codex",
            "primary": {"usedPercent": 45, "windowDurationMins": null, "resetsAt": null},
            "secondary": null}));
        let codex = &usage.buckets["codex"];
        let primary = codex.primary.as_ref().unwrap();
        assert_eq!(primary.used_percent, 45.0);
        assert_eq!(primary.duration_mins, Some(10080));
        assert_eq!(primary.resets_at, Some(1790710629));
        assert_eq!(codex.secondary.as_ref().unwrap().used_percent, 5.0);

        // A bucket seen first in an update is added.
        usage.merge(&json!({"limitId": "image_gen",
            "primary": {"usedPercent": 100, "windowDurationMins": 1440, "resetsAt": null}}));
        assert!(usage.lines()[1].starts_with("image quota: 1440-min 100%"));
    }

    #[test]
    fn reset_times_are_formatted_as_a_date_and_minute() {
        let utc = utc_system_time(1_790_710_629).unwrap();
        assert_eq!(format_system_time(&utc), "2026-09-29 19:37");
        let local = local_time(1_790_710_629).unwrap();
        assert_eq!(local.len(), "2026-09-29 19:37".len(), "{local}");
        assert!(local.starts_with("2026-09-"), "{local}");
        assert!(window_text(&Window {
            used_percent: 44.0,
            duration_mins: Some(10080),
            resets_at: Some(1_790_710_629)
        })
        .starts_with("weekly 44% (resets 2026-09-"));
        assert_eq!(local_time(i64::MAX), None);
    }
}
