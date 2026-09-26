//! The failure contract.
//!
//! When an image cannot be generated, the calling agent must not paper over it by producing an
//! image some other way -- an SVG, ASCII art, a chart drawn by code, another tool -- or by talking
//! as if an image exists. Every failure therefore carries a machine-readable code, a plain
//! statement of what broke, and the exact remediation to relay to the user
//! (docs/design.md, "Errors").
//!
//! Two render forms:
//!
//! - **Stop and escalate**, for setup and upstream problems the agent cannot fix. The text says
//!   no image was produced, forbids a substitute, and asks the agent to relay the remediation.
//! - **Agent-correctable**, a short "REQUEST REJECTED" form for mistakes the agent fixes itself
//!   by calling again differently (bad arguments, a busy session).
//!
//! `handler_thread_unavailable` is the one exception and returns finished text rather than a
//! `Failure`: it is about a call that never ran at all, so neither form's sentences fit.
//!
//! Every failure is returned only when no image completed. A call where an image did complete is
//! always a success with warnings, so "no image was produced" is always true where it is said.

use std::path::Path;

/// Codes that stop the agent and send the user a remediation, exactly as the design's table.
/// Rendering keys off [`AGENT_CORRECTABLE_CODES`] alone, so only the tests read this list.
#[cfg(test)]
pub const STOP_AND_ESCALATE_CODES: &[&str] = &[
    "CLI_NOT_FOUND",
    "SPAWN_FAILED",
    "APP_SERVER_FAILED",
    "NOT_AUTHENTICATED",
    "AUTH_EXPIRED",
    "IMAGEGEN_UNAVAILABLE",
    "MODEL_UNAVAILABLE",
    "RATE_LIMITED",
    "UPSTREAM_ERROR",
    "CONTENT_REFUSED",
    "IMAGE_FAILED",
    "NO_IMAGE",
    "TIMEOUT",
    "STORE_CORRUPT",
    "INTERNAL_ERROR",
];

/// Codes the agent corrects by calling again differently, exactly as the design's table.
pub const AGENT_CORRECTABLE_CODES: &[&str] = &[
    "BAD_REQUEST",
    "SESSION_EXISTS",
    "SESSION_NOT_FOUND",
    "SESSION_NOT_RESUMABLE",
    "SESSION_BUSY",
    "SESSION_OPEN_ELSEWHERE",
    "TOO_MANY_RUNNING",
    "CANCELLED",
    "SERVER_SHUTTING_DOWN",
];

/// Longest detail kept, in characters. Detail is mostly the Codex child's stderr tail, useful
/// for diagnosis but not worth the agent's whole context.
const MAX_DETAIL_CHARS: usize = 4000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub code: &'static str,
    pub summary: String,
    pub remediation: String,
    pub detail: Option<String>,
}

impl Failure {
    pub fn new(
        code: &'static str,
        summary: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            code,
            summary: summary.into(),
            remediation: remediation.into(),
            detail: None,
        }
    }

    /// Attach diagnostic detail, trimmed and truncated. Empty detail is ignored, so a caller can
    /// pass a stderr tail without checking it first.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        let trimmed = detail.trim();
        if !trimmed.is_empty() {
            self.detail = Some(truncate(trimmed, MAX_DETAIL_CHARS));
        }
        self
    }

    /// True when the failure is the agent's own to fix by calling again differently, rather than
    /// a reason to stop and involve the user.
    pub fn is_agent_correctable(&self) -> bool {
        AGENT_CORRECTABLE_CODES.contains(&self.code)
    }

    /// The text handed back to the calling agent, in the form its code calls for.
    pub fn render_for_agent(&self) -> String {
        if self.is_agent_correctable() {
            self.render_short()
        } else {
            self.render_stop()
        }
    }

    /// The stop-and-escalate form. Deliberately blunt: an agent that skims this must still come
    /// away knowing there is no image, that it must not make one some other way, and that the
    /// user has to hear about it.
    fn render_stop(&self) -> String {
        let mut out = format!(
            "IMAGE GENERATION FAILED\ncode: {}\n\n{}\n\n",
            self.code, self.summary
        );
        out.push_str("=== ACTION REQUIRED ===\n");
        out.push_str(
            "No image was produced. Stop the image task now. Do not substitute an image made \
             another way (SVG, ASCII art, HTML or CSS, code that draws one, or another image \
             tool), and do not say or imply that an image exists.\n\n\
             Report this to the user:\n\n",
        );
        out.push_str(&self.remediation);
        out.push_str("\n\nThen wait for the user before retrying.\n");
        out.push_str("=======================\n");
        if let Some(detail) = &self.detail {
            out.push_str("\n--- detail ---\n");
            out.push_str(detail);
            out.push('\n');
        }
        out
    }

    /// The agent-correctable form: what was wrong and how to call again, with no ceremony.
    fn render_short(&self) -> String {
        let mut out = format!(
            "REQUEST REJECTED\ncode: {}\n\n{}\n\n{}\n",
            self.code, self.summary, self.remediation
        );
        if let Some(detail) = &self.detail {
            out.push('\n');
            out.push_str(detail);
            out.push('\n');
        }
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut chars = s.chars();
    let kept: String = chars.by_ref().take(max).collect();
    if chars.next().is_none() {
        return kept;
    }
    format!("{kept}\n... [truncated]")
}

/// How to sign Codex in, for the home codex-imagegen runs it with.
fn login_instructions(codex_home: Option<&Path>) -> String {
    match codex_home {
        None => "  codex login\n\n\
                 Sign in with the ChatGPT account whose plan should pay for the images."
            .to_string(),
        // Commands to paste as they stand: PowerShell has no `VAR=value command` form, and the
        // variable is removed again so it does not linger in that window.
        Some(home) => format!(
            "codex-imagegen runs Codex with a dedicated home, {home} (its --codex-home flag), so \
             sign that home in. In PowerShell:\n\n\
             \x20 $env:CODEX_HOME = '{quoted}'; codex login --device-auth; Remove-Item \
             Env:CODEX_HOME\n\n\
             (in Git Bash: CODEX_HOME='{bash}' codex login --device-auth)\n\n\
             Sign in with the ChatGPT account whose plan should pay for the images.",
            home = home.display(),
            // A quote inside single quotes: doubled for PowerShell, closed and escaped for Bash.
            quoted = home.display().to_string().replace('\'', "''"),
            bash = home.display().to_string().replace('\'', r"'\''")
        ),
    }
}

// ---------------------------------------------------------------------------
// Constructors, one per failure the calling agent can meet.
// ---------------------------------------------------------------------------

/// No Codex CLI at the configured path, on PATH, or in the default install location.
///
/// `explicit` is the `--codex-bin` value when one was given: a wrong explicit path is a different
/// fix from a missing install, so the summary says which it was.
pub fn cli_not_found(explicit: Option<&str>, tried: &[String]) -> Failure {
    let summary = match explicit {
        Some(bin) => format!(
            "The Codex CLI configured with --codex-bin was not found at '{bin}', so no image can \
             be generated."
        ),
        None => "The Codex CLI could not be found, so no image can be generated.".to_string(),
    };
    let tried = if tried.is_empty() {
        "(nowhere)".to_string()
    } else {
        tried.join("\n")
    };
    Failure::new(
        "CLI_NOT_FOUND",
        summary,
        "codex-imagegen generates images through the Codex CLI, and could not find it.\n\n\
         To fix it, either:\n\
         \x20 1. Install the Codex CLI:  npm install -g @openai/codex\n\
         \x20    and sign it in once:    codex login\n\
         \x20 2. Or point codex-imagegen at an existing install by adding\n\
         \x20    --codex-bin \"C:\\full\\path\\to\\codex.exe\" after codex-imagegen.exe in its MCP\n\
         \x20    registration.\n\n\
         Then restart the MCP server (restart the Claude Code session, or reconnect the server \
         with /mcp) and retry.\n\n\
         The Codex desktop app alone is not enough: this needs the command-line CLI.",
    )
    .with_detail(format!("Looked for:\n{tried}"))
}

/// The Codex binary exists but the OS would not start it.
pub fn spawn_failed(bin: &str, detail: impl Into<String>) -> Failure {
    Failure::new(
        "SPAWN_FAILED",
        format!("The Codex CLI at '{bin}' could not be started."),
        format!(
            "codex-imagegen found the Codex CLI at:\n\n\
             \x20 {bin}\n\n\
             but the operating system refused to start it. The file may be damaged, blocked by \
             policy or antivirus, or not an executable. Running it directly in a terminal \
             (`\"{bin}\" --version`) shows the underlying error."
        ),
    )
    .with_detail(detail)
}

/// The Codex home to start the child with is missing or not a folder. Codex exits at once on such
/// a `CODEX_HOME` [verified: codex-cli 0.157.1], which would otherwise reach the agent as an
/// app-server failure with a retry remediation.
pub fn codex_home_unusable(home: &Path, why: &str) -> Failure {
    Failure::new(
        "SPAWN_FAILED",
        format!("Codex was not started: its home {} {why}.", home.display()),
        format!(
            "codex-imagegen runs Codex with CODEX_HOME set to its --codex-home folder, and Codex \
             refuses a home that is missing or not a folder. Either create the folder and sign it \
             in once:\n\n\
             \x20 New-Item -ItemType Directory -Force '{quoted}'\n\n{login}\n\n\
             and retry, or correct the --codex-home value in the codex-imagegen MCP registration \
             (a backslash just before the closing quote swallows the quote) and restart the MCP \
             server (restart the Claude Code session, or reconnect the server with /mcp).",
            quoted = home.display().to_string().replace('\'', "''"),
            login = login_instructions(Some(home))
        ),
    )
}

/// The Codex app-server failed, timed out, or exited while answering `method`.
pub fn app_server_failed(method: &str, detail: impl Into<String>) -> Failure {
    Failure::new(
        "APP_SERVER_FAILED",
        format!("The Codex app-server failed during '{method}'."),
        "Retry once. If it fails again, run codex-imagegen.exe --doctor in a terminal to check \
         the Codex install and login, and check OpenAI's service status: during an outage Codex \
         fails for reasons codex-imagegen cannot fix.",
    )
    .with_detail(detail)
}

/// Codex has no signed-in account (`account/read` returned a null account).
pub fn not_authenticated(codex_home: Option<&Path>) -> Failure {
    Failure::new(
        "NOT_AUTHENTICATED",
        "The Codex CLI is installed but not signed in, so it cannot generate images.",
        format!(
            "codex-imagegen needs Codex signed in with a ChatGPT account. To fix it, run this in a \
             terminal:\n\n{}\n\n\
             Once it reports success, retry. codex-imagegen picks up the new login on its next \
             call, with no restart.",
            login_instructions(codex_home)
        ),
    )
}

/// Codex is signed in, but not with a ChatGPT account (for example with an API key).
///
/// Refused rather than used: images must be billed to the user's ChatGPT plan, and an API-key
/// login would silently move them onto per-use API billing (docs/design.md, "Security posture").
pub fn not_chatgpt_account(account_type: &str, codex_home: Option<&Path>) -> Failure {
    Failure::new(
        "NOT_AUTHENTICATED",
        format!(
            "Codex is signed in with a '{account_type}' account. API-key auth is not supported: \
             codex-imagegen only generates images billed to a ChatGPT plan."
        ),
        format!(
            "Run codex login with a ChatGPT account (not an API key). In a terminal:\n\n{}\n\n\
             Once it reports success, retry. codex-imagegen picks up the new login on its next \
             call, with no restart.",
            login_instructions(codex_home)
        ),
    )
}

/// The signed-in account's plan does not get image generation (`planType: "free"`).
pub fn imagegen_unavailable_on_plan(plan: &str, codex_home: Option<&Path>) -> Failure {
    Failure::new(
        "IMAGEGEN_UNAVAILABLE",
        format!(
            "The signed-in ChatGPT account is on the '{plan}' plan, and codex-imagegen does not \
             generate images on that plan."
        ),
        format!(
            "Sign Codex in with an account on a paid ChatGPT plan. In a terminal:\n\n{}\n\n\
             Then retry. codex-imagegen picks up the new login on its next call, with no restart.",
            login_instructions(codex_home)
        ),
    )
}

/// The model provider reports no image capability (`modelProvider/capabilities/read` returned
/// `imageGeneration: false`). A property of the provider Codex is configured with, not of the
/// login.
pub fn imagegen_unavailable_capability() -> Failure {
    Failure::new(
        "IMAGEGEN_UNAVAILABLE",
        "Codex reports that its model provider does not offer image generation \
         (imageGeneration: false).",
        "This depends on which model provider Codex is configured to use, not on the login. If \
         your Codex config.toml selects a custom model_provider, codex-imagegen cannot generate \
         images through it: remove that setting, or run codex-imagegen with --codex-home pointing \
         at a dedicated Codex home that uses the default provider. Then retry.",
    )
}

/// The effective Codex configuration does not show one of the switches codex-imagegen starts
/// Codex with: managed configuration overrides it, or a legacy alias such as
/// `features.connectors = true` re-enables a feature the child switches off. The switch cannot be
/// forced back from here, so nothing is started. `setting` names what Codex reported, for example
/// `features.image_generation = false`.
pub fn imagegen_unavailable_setting(setting: &str) -> Failure {
    Failure::new(
        "IMAGEGEN_UNAVAILABLE",
        format!(
            "Codex's effective configuration shows `{setting}`, overriding a setting \
             codex-imagegen starts Codex with, so codex-imagegen will not run it."
        ),
        "Remove that setting from your Codex config.toml (%USERPROFILE%\\.codex\\config.toml, or \
         the file in CODEX_HOME), or run codex-imagegen with --codex-home pointing at a dedicated \
         Codex home. If it comes from a managed Codex configuration (managed_config.toml, or \
         requirements set by an administrator), whoever manages that has to change it. Then \
         retry.",
    )
}

/// Codex could not resolve its own configuration (`config/read` answered with an error), so
/// codex-imagegen cannot confirm that the child runs with the settings it was started with.
/// codex-cli 0.156.0 answers this way for a legacy top-level `profile = "..."` key, while its
/// app-server logs "Invalid configuration; using defaults" and keeps serving [verified]: a child
/// whose settings cannot be confirmed must not run.
pub fn codex_config_unreadable(detail: impl Into<String>) -> Failure {
    Failure::new(
        "IMAGEGEN_UNAVAILABLE",
        "Codex reports that it cannot read its configuration, so codex-imagegen cannot confirm \
         the settings it runs Codex with and will not run it.",
        "Fix the problem Codex reports below in your Codex config.toml (%USERPROFILE%\\.codex\\\
         config.toml, or the file in CODEX_HOME), or run codex-imagegen with --codex-home \
         pointing at a dedicated Codex home. A legacy top-level `profile = \"...\"` line is a \
         common cause: current Codex versions no longer accept it. Then retry.",
    )
    .with_detail(detail)
}

/// The pinned agent model is not in this Codex's `model/list`.
pub fn model_unavailable(model: &str, detail: impl Into<String>) -> Failure {
    Failure::new(
        "MODEL_UNAVAILABLE",
        format!("The agent model '{model}' is not offered by this Codex (it is missing from model/list)."),
        format!(
            "Update the Codex CLI, or start codex-imagegen with --model set to a model id that \
             Codex lists (the full id, not an alias), then restart the MCP server. The configured \
             model is '{model}'."
        ),
    )
    .with_detail(detail)
}

/// Bad tool arguments. The agent's mistake, not a setup problem, so it gets a plain correction
/// instead of the stop-and-escalate contract.
pub fn bad_request(summary: impl Into<String>) -> Failure {
    Failure::new(
        "BAD_REQUEST",
        summary,
        "No image was produced and nothing was spent. Correct the tool arguments and call it \
         again.",
    )
}

/// Something inside codex-imagegen went wrong.
pub fn internal_error(summary: impl Into<String>) -> Failure {
    Failure::new(
        "INTERNAL_ERROR",
        summary,
        "codex-imagegen hit an internal error. Retrying is reasonable; if it recurs, it is a bug \
         in codex-imagegen, and the server's stderr log has the details.",
    )
}

/// The whole call ran out of its `--timeout-seconds` budget before an image completed.
pub fn timeout(secs: u64) -> Failure {
    Failure::new(
        "TIMEOUT",
        format!("No image completed within the {secs}-second limit (--timeout-seconds)."),
        "Retry once: an image usually takes under a minute. If it times out again, check OpenAI's \
         service status, or raise --timeout-seconds in the codex-imagegen MCP registration.",
    )
}

/// Stdin has closed, so the call was refused rather than started with nowhere to answer.
pub fn server_shutting_down() -> Failure {
    Failure::new(
        "SERVER_SHUTTING_DOWN",
        "codex-imagegen is shutting down (its stdin closed), so it did not start this call.",
        "No image was produced and nothing was spent. Reconnect the codex-imagegen MCP server \
         (for example with /mcp in Claude Code) and call the tool again.",
    )
}

/// Another call in this process is using the session. `interrupted` is the case where that call
/// has already returned, but its turn was interrupted and Codex has not yet confirmed that it
/// stopped: a new turn on the thread would merge into it (docs/design.md, "After `turn/interrupt`").
pub fn session_busy(session: &str, interrupted: bool) -> Failure {
    let why = if interrupted {
        "its previous turn was interrupted and Codex has not yet confirmed that it stopped"
    } else {
        "another call on it is still running"
    };
    Failure::new(
        "SESSION_BUSY",
        format!("The session '{session}' is busy: {why}."),
        "Nothing was spent. Wait for the other call to finish and call again, or use a different \
         session name.",
    )
}

/// Another codex-imagegen process holds the session's lease: a call on it is running there, in
/// another Claude Code window, or a cleanup is removing it. Not waited for, as a busy session in
/// this process is not (docs/design.md, "Sessions").
pub fn session_busy_elsewhere(session: &str) -> Failure {
    Failure::new(
        "SESSION_BUSY",
        format!(
            "The session '{session}' is busy in another codex-imagegen process (another Claude \
             Code window, or a cleanup): a call on it is still running there."
        ),
        "Nothing was spent. Wait for that call to finish and call again, or use a different \
         session name.",
    )
}

/// generate always starts a new session, and this name is taken (compared case-insensitively).
/// `recorded` is the name as the session was created, which may differ in case.
pub fn session_exists(session: &str, recorded: &str) -> Failure {
    let spelled = if recorded == session {
        String::new()
    } else {
        format!(" (as '{recorded}')")
    };
    Failure::new(
        "SESSION_EXISTS",
        format!(
            "A session named '{session}' already exists in this project{spelled}. \
             codex_imagegen_generate always starts a new session."
        ),
        format!(
            "Nothing was spent. To change that session's image, call codex_imagegen_refine with \
             session '{recorded}'. To start a new one, call codex_imagegen_generate with another \
             session name, or without one to have a name picked."
        ),
    )
}

/// refine names a session this project's store does not hold.
pub fn session_not_found(session: &str) -> Failure {
    Failure::new(
        "SESSION_NOT_FOUND",
        format!("No session named '{session}' exists in this project."),
        "Nothing was spent. codex_imagegen_status lists this project's sessions; each project has \
         its own, and a session expires once it has been idle for the server's \
         --session-ttl-days. To make a new image, call codex_imagegen_generate, passing an \
         existing image in reference_images if the new one should follow it.",
    )
}

/// The session exists but cannot be continued: its thread is gone or out of room, it lives in
/// another Codex home, or no copy of its latest image survives to edit. `surviving` is a copy of
/// the session's image that still exists, offered as the new session's reference.
pub fn session_not_resumable(
    session: &str,
    why: impl Into<String>,
    surviving: Option<&Path>,
) -> Failure {
    let remediation = match surviving {
        Some(path) => format!(
            "The session '{session}' cannot be continued. Start a new session with \
             codex_imagegen_generate, passing the session's latest image as a reference: \
             reference_images: [{}].",
            serde_json::to_string(&path.display().to_string()).unwrap_or_default()
        ),
        None => format!(
            "The session '{session}' cannot be continued, and no copy of its images that \
             codex-imagegen recorded still exists. Start a new session with \
             codex_imagegen_generate; if the user has a copy of the image, pass it in \
             reference_images."
        ),
    };
    Failure::new("SESSION_NOT_RESUMABLE", why, remediation)
}

/// Another process has the session's Codex thread loaded and did not release it within the wait
/// (docs/design.md, "Refine"): Codex's writer lock admits one process per thread.
pub fn session_open_elsewhere(session: &str, waited_secs: u64) -> Failure {
    Failure::new(
        "SESSION_OPEN_ELSEWHERE",
        format!(
            "The session '{session}' is open in another program: another process has its Codex \
             thread loaded, and did not release it within {waited_secs} s."
        ),
        "Nothing was spent. The session is open in another Claude Code window or in the Codex \
         app. Close it there (or let that window's call finish) and call again, or start a new \
         session with codex_imagegen_generate.",
    )
}

/// The session store exists but cannot be parsed. generate and refine refuse to run rather than
/// write over sessions they cannot see (docs/design.md, "Sessions").
pub fn store_corrupt(path: &Path, detail: impl Into<String>) -> Failure {
    Failure::new(
        "STORE_CORRUPT",
        format!(
            "codex-imagegen's session store {} cannot be read, so it will not start or continue \
             a session: writing to it would lose the sessions recorded there.",
            path.display()
        ),
        format!(
            "The file {path} is damaged or was written by a newer codex-imagegen. Move it aside \
             (for example, rename it to sessions.json.bad) and retry: codex-imagegen then starts \
             a fresh store for this project. The sessions recorded in the old file can no longer \
             be refined, and their images and Codex threads are no longer expired \
             automatically.",
            path = path.display()
        ),
    )
    .with_detail(detail)
}

/// The session store, its lock or a session lease could not be used: the state folder is missing
/// permissions, on a filesystem without byte-range locks, or held by a stuck process.
pub fn store_unusable(path: &Path, detail: impl Into<String>) -> Failure {
    Failure::new(
        "STORE_CORRUPT",
        format!(
            "codex-imagegen could not use its session store at {}, so it will not start or \
             continue a session.",
            path.display()
        ),
        "Check that codex-imagegen's state folder exists on a local disk and that this user can \
         create and change files in it (codex_imagegen_status shows the folder). To move it, set \
         the CODEX_IMAGEGEN_HOME environment variable or pass --state-dir, then restart the MCP \
         server. If another codex-imagegen process is stuck, closing it releases the store.",
    )
    .with_detail(detail)
}

/// `--max-concurrent` image calls are already running in this process.
pub fn too_many_running(max: usize) -> Failure {
    Failure::new(
        "TOO_MANY_RUNNING",
        format!(
            "codex-imagegen is already running {max} image call(s), its limit (--max-concurrent \
             {max})."
        ),
        "Nothing was spent. Wait for one of the running calls to finish, then call again.",
    )
}

// ---------------------------------------------------------------------------
// A turn that produced no image (docs/design.md, "Errors"). Every one of these is returned only
// when no image completed: once one has, the call is a success with warnings.
// ---------------------------------------------------------------------------

/// How much of Codex's closing message a failure quotes. It is untrusted model text, kept only to
/// help a person diagnose; never used to classify anything.
const MAX_QUOTED_NOTE_CHARS: usize = 500;

/// The agent's closing message, for a failure's detail: truncated, stripped of characters that
/// hide text, quoted, and labelled as untrusted.
pub fn untrusted_note_detail(note: Option<&str>) -> String {
    match note {
        Some(note) => format!(
            "Codex's closing message (untrusted model text, quoted; not a diagnosis): {}",
            quote_untrusted(note, MAX_QUOTED_NOTE_CHARS)
        ),
        None => "Codex sent no closing message.".to_string(),
    }
}

/// Untrusted text as one JSON-quoted line: bounded, with control, zero-width and bidi characters
/// removed, so it cannot pass for anything but a quotation.
pub fn quote_untrusted(text: &str, max_chars: usize) -> String {
    // Line breaks and tabs become spaces first: clamp drops control characters, which would
    // otherwise run the words on either side together.
    let flat: String = text
        .trim()
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    serde_json::to_string(&crate::jsonrpc::clamp(&flat, max_chars))
        .unwrap_or_else(|_| "\"\"".to_string())
}

/// The image tool hit the `image_gen` usage limit: an item-level `failure` of type
/// `usageLimitExceeded`. `resets` is the reset time already formatted, `None` when Codex gave none.
pub fn image_quota_exhausted(limit_id: &str, resets: Option<&str>) -> Failure {
    let when = match resets {
        Some(at) => format!("It resets at {at}."),
        None => "Codex did not say when it resets (reset time unknown).".to_string(),
    };
    Failure::new(
        "RATE_LIMITED",
        format!("The ChatGPT plan's image quota is used up (Codex limit '{limit_id}'). {when}"),
        format!(
            "Image generation is metered in its own quota on the user's ChatGPT plan, and it is \
             exhausted. {when} Retry after that; codex_imagegen_status shows the usage codex-imagegen \
             knows about. A plan with a larger image allowance also lifts it."
        ),
    )
}

/// A failed image item with no `failure` set: a refusal, a backend error or no image data. Codex
/// sends the reason only to its agent model, so it cannot be told apart here [verified: source].
pub fn image_failed(note: Option<&str>) -> Failure {
    Failure::new(
        "IMAGE_FAILED",
        "Codex's image tool reported that the image failed. The image backend refused the prompt, \
         hit a server error, or returned no image; Codex does not say which.",
        "Retry once. If it fails again, the prompt may have been refused by image moderation: tell \
         the user, who may want to rephrase it.",
    )
    .with_detail(untrusted_note_detail(note))
}

/// The turn completed without any image item: the agent never called the image tool, or the tool
/// failed before it started, as it does for a reference image it cannot read.
pub fn no_image(note: Option<&str>, had_references: bool) -> Failure {
    let references = if had_references {
        " Check that every reference image still exists and is readable: Codex reads them itself, \
         after codex-imagegen checked them."
    } else {
        ""
    };
    Failure::new(
        "NO_IMAGE",
        "Codex finished the turn without making an image: its image tool never ran.",
        format!(
            "Retry once.{references} If it happens again, run codex-imagegen.exe --doctor in a \
             terminal and report the detail below."
        ),
    )
    .with_detail(untrusted_note_detail(note))
}

/// The turn ended in a state codex-imagegen did not ask for and cannot classify, such as
/// `interrupted` with no interrupt sent.
pub fn turn_ended_without_image(status: &str, note: Option<&str>) -> Failure {
    Failure::new(
        "IMAGE_FAILED",
        format!("The Codex turn ended as '{status}' before an image completed."),
        "Retry once. If it happens again, run codex-imagegen.exe --doctor in a terminal and check \
         OpenAI's service status.",
    )
    .with_detail(untrusted_note_detail(note))
}

/// What a failed turn's `codexErrorInfo` means (docs/design.md, "Turn-level failures"). `info` is
/// its variant name (`unauthorized`, `httpConnectionFailed`, ...), `None` when Codex sent none;
/// `message` is `TurnError.message`, kept as detail. Never decided from model text.
pub fn turn_failed(info: Option<&str>, message: &str, codex_home: Option<&Path>) -> Failure {
    let named = info.unwrap_or("none");
    let detail = format!("codexErrorInfo: {named}\nCodex's error message: {message}");
    match info {
        Some("usageLimitExceeded" | "rateLimitExceeded" | "serverOverloaded") => Failure::new(
            "RATE_LIMITED",
            format!("Codex stopped the turn on a usage or rate limit (codexErrorInfo {named})."),
            "Codex's agent model is rate-limited or over its usage allowance on the user's ChatGPT \
             plan, or OpenAI is overloaded. Wait a few minutes and retry; codex_imagegen_status \
             shows the usage windows.",
        ),
        Some("unauthorized") => Failure::new(
            "AUTH_EXPIRED",
            "Codex's ChatGPT sign-in is no longer accepted (codexErrorInfo unauthorized).",
            format!(
                "The Codex login expired or was revoked. Sign in again in a terminal:\n\n{}\n\n\
                 Then retry: codex-imagegen starts a fresh Codex for the next call, which picks up \
                 the new login.",
                login_instructions(codex_home)
            ),
        ),
        Some("cyberPolicy" | "misalignmentPolicyViolation") => Failure::new(
            "CONTENT_REFUSED",
            format!(
                "Codex's agent model refused the request under its usage policy (codexErrorInfo \
                 {named}). This is the agent model's policy check, not image moderation."
            ),
            "Tell the user that the request was refused. Do not retry the same prompt; the user may \
             want to rephrase it.",
        ),
        Some("contextWindowExceeded" | "sessionBudgetExceeded") => Failure::new(
            "SESSION_NOT_RESUMABLE",
            format!("The Codex session has run out of room (codexErrorInfo {named})."),
            "Nothing more can be done in this session. Start a new one with \
             codex_imagegen_generate, passing the last image as reference_images.",
        ),
        Some(name)
            if name == "internalServerError"
                || name == "httpConnectionFailed"
                || name.starts_with("responseStream") =>
        {
            Failure::new(
                "UPSTREAM_ERROR",
                format!("Codex could not get an answer from OpenAI (codexErrorInfo {named})."),
                "Retry once. If it fails again, check OpenAI's service status: during an outage \
                 Codex fails for reasons codex-imagegen cannot fix.",
            )
        }
        _ => Failure::new(
            "IMAGE_FAILED",
            format!("The Codex turn failed before an image completed (codexErrorInfo {named})."),
            "Retry once. If it fails again, run codex-imagegen.exe --doctor in a terminal and check \
             OpenAI's service status.",
        ),
    }
    .with_detail(detail)
}

/// An MCP server started on one of codex-imagegen's Codex threads, although every one is turned
/// off for them: the isolation the design depends on did not hold, so the turn was stopped
/// (docs/design.md, "Canary").
pub fn isolation_breach(server: &str) -> Failure {
    Failure::new(
        "APP_SERVER_FAILED",
        format!(
            "isolation breach: MCP server {} started on codex-imagegen's Codex thread, so the \
             turn was stopped.",
            crate::jsonrpc::clamp(server, 100)
        ),
        "codex-imagegen turns off every MCP server for the Codex threads it runs, and one started \
         anyway. Check the Codex configuration (the mcp_servers entries in config.toml, and any \
         managed configuration), or run codex-imagegen with --codex-home pointing at a dedicated \
         Codex home. Do not retry until the cause is understood.",
    )
}

/// The Codex app-server stopped in the middle of a turn, before an image completed.
pub fn child_died_mid_turn(detail: impl Into<String>) -> Failure {
    app_server_failed("the image turn", detail)
}

/// The request was cancelled before an image completed. A cancelled request normally gets no
/// response at all; this exists for the paths that still need a result value.
pub fn cancelled() -> Failure {
    Failure::new(
        "CANCELLED",
        "The request was cancelled before an image completed.",
        "No image was produced by this call. Call the tool again if the image is still wanted.",
    )
}

/// The server could not start a thread to run a `tools/call`.
///
/// Written out rather than built from a `Failure`, because neither render form fits: the
/// stop-and-escalate form tells the agent to abandon the image task over what is usually a
/// transient resource limit, and the agent-correctable form blames a request that was fine. What
/// holds for every tool is only that this call did not run: nothing was sent to Codex and no
/// session changed.
pub fn handler_thread_unavailable(tool: &str, os_error: &str) -> String {
    let tool = if tool.is_empty() {
        "codex-imagegen"
    } else {
        tool
    };
    format!(
        "TOOL CALL NOT HANDLED\n\
         code: INTERNAL_ERROR\n\n\
         The codex-imagegen server could not start a thread to run the '{tool}' call \
         ({os_error}), so the call did not run.\n\n\
         The operating system refused to create the thread, which in practice means the machine \
         is out of memory or this process has hit its thread limit. It is not a problem with the \
         Codex setup or the arguments. Nothing was sent to Codex, no image was produced, and no \
         session changed.\n\n\
         Call the tool again. If it fails the same way a second time, stop and tell the user the \
         server cannot start threads on this machine. Do not produce an image some other way.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every constructor, so the code tables can be checked against what is actually raised.
    fn every_constructor() -> Vec<Failure> {
        vec![
            cli_not_found(None, &["PATH".to_string()]),
            cli_not_found(
                Some(r"C:\nope\codex.exe"),
                &[r"C:\nope\codex.exe".to_string()],
            ),
            spawn_failed(r"C:\x\codex.exe", "os error 193"),
            app_server_failed("initialize", "exited"),
            not_authenticated(None),
            not_authenticated(Some(Path::new(r"D:\codex-home"))),
            not_chatgpt_account("apiKey", None),
            imagegen_unavailable_on_plan("free", None),
            imagegen_unavailable_capability(),
            imagegen_unavailable_setting("features.image_generation = false"),
            codex_config_unreadable("legacy `profile = \"x\"` config is no longer supported"),
            model_unavailable("gpt-6-astra", ""),
            bad_request("'prompt' must not be empty."),
            internal_error("boom"),
            image_quota_exhausted("image_gen", Some("2026-09-26 14:00")),
            image_quota_exhausted("image_gen", None),
            image_failed(Some("the backend refused")),
            no_image(None, true),
            turn_ended_without_image("interrupted", None),
            turn_failed(Some("unauthorized"), "401", None),
            turn_failed(Some("usageLimitExceeded"), "limit", None),
            turn_failed(Some("cyberPolicy"), "no", None),
            turn_failed(Some("contextWindowExceeded"), "full", None),
            turn_failed(Some("responseStreamDisconnected"), "gone", None),
            turn_failed(None, "odd", None),
            isolation_breach("rogue"),
            child_died_mid_turn("exit code 1"),
            timeout(300),
            server_shutting_down(),
            session_busy("fox", false),
            session_busy("fox", true),
            session_busy_elsewhere("fox"),
            session_exists("FOX", "fox"),
            session_not_found("fox"),
            session_not_resumable("fox", "gone", Some(Path::new(r"C:\o\fox-v2.png"))),
            session_not_resumable("fox", "gone", None),
            session_open_elsewhere("fox", 15),
            store_corrupt(Path::new(r"C:\s\sessions.json"), "expected value at line 1"),
            store_unusable(Path::new(r"C:\s\sessions.lock"), "access is denied"),
            too_many_running(4),
            cancelled(),
        ]
    }

    #[test]
    fn the_code_tables_match_the_design_exactly() {
        // docs/design.md, "Errors". Written out here so a change to either table is a visible,
        // deliberate edit in two places.
        let stop = "CLI_NOT_FOUND SPAWN_FAILED APP_SERVER_FAILED NOT_AUTHENTICATED AUTH_EXPIRED \
                    IMAGEGEN_UNAVAILABLE MODEL_UNAVAILABLE RATE_LIMITED UPSTREAM_ERROR \
                    CONTENT_REFUSED IMAGE_FAILED NO_IMAGE TIMEOUT STORE_CORRUPT INTERNAL_ERROR";
        let short = "BAD_REQUEST SESSION_EXISTS SESSION_NOT_FOUND SESSION_NOT_RESUMABLE \
                     SESSION_BUSY SESSION_OPEN_ELSEWHERE TOO_MANY_RUNNING CANCELLED \
                     SERVER_SHUTTING_DOWN";
        assert_eq!(
            STOP_AND_ESCALATE_CODES,
            stop.split_whitespace().collect::<Vec<_>>()
        );
        assert_eq!(
            AGENT_CORRECTABLE_CODES,
            short.split_whitespace().collect::<Vec<_>>()
        );
        for code in STOP_AND_ESCALATE_CODES {
            assert!(!AGENT_CORRECTABLE_CODES.contains(code), "{code} is in both");
        }
    }

    #[test]
    fn every_constructor_raises_a_code_from_the_tables() {
        for failure in every_constructor() {
            assert!(
                STOP_AND_ESCALATE_CODES.contains(&failure.code)
                    || AGENT_CORRECTABLE_CODES.contains(&failure.code),
                "{} is in neither table",
                failure.code
            );
        }
    }

    #[test]
    fn the_stop_form_forbids_a_substitute_and_asks_for_the_user() {
        let text = cli_not_found(None, &["PATH".to_string()]).render_for_agent();
        assert!(
            text.starts_with("IMAGE GENERATION FAILED\ncode: CLI_NOT_FOUND\n"),
            "{text}"
        );
        assert!(text.contains("=== ACTION REQUIRED ==="));
        assert!(text.contains("No image was produced"));
        assert!(text.contains("Stop the image task"));
        assert!(text.contains("Do not substitute an image made another way"));
        assert!(text.contains("SVG") && text.contains("ASCII art"));
        assert!(text.contains("another image tool"));
        assert!(text.contains("do not say or imply that an image exists"));
        assert!(text.contains("Report this to the user"));
        assert!(text.contains("npm install -g @openai/codex"));
        assert!(text.contains("--- detail ---\nLooked for:\nPATH"));
    }

    #[test]
    fn the_short_form_is_a_plain_correction() {
        let text = bad_request("'prompt' must not be empty.").render_for_agent();
        assert!(
            text.starts_with("REQUEST REJECTED\ncode: BAD_REQUEST\n\n'prompt' must not be empty."),
            "{text}"
        );
        assert!(text.contains("Correct the tool arguments"));
        assert!(!text.contains("ACTION REQUIRED"));
        assert!(!text.contains("IMAGE GENERATION FAILED"));
    }

    #[test]
    fn each_constructor_renders_in_the_form_its_code_calls_for() {
        for failure in every_constructor() {
            let text = failure.render_for_agent();
            let header = if failure.is_agent_correctable() {
                "REQUEST REJECTED\ncode: "
            } else {
                "IMAGE GENERATION FAILED\ncode: "
            };
            assert!(
                text.starts_with(&format!("{header}{}\n", failure.code)),
                "{text}"
            );
            assert_eq!(
                text.contains("ACTION REQUIRED"),
                !failure.is_agent_correctable(),
                "{text}"
            );
            // Nothing here may suggest an image exists or offer a way to make one without Codex.
            let lower = text.to_lowercase();
            assert!(!lower.contains("image is ready"), "{text}");
            assert!(!lower.contains("image was saved"), "{text}");
        }
    }

    #[test]
    fn detail_is_trimmed_truncated_and_marked() {
        let long = "x".repeat(MAX_DETAIL_CHARS + 10);
        let failure = internal_error("boom").with_detail(format!("  {long}  "));
        let detail = failure.detail.unwrap();
        assert!(detail.ends_with("\n... [truncated]"), "{detail}");
        assert_eq!(
            detail.chars().filter(|c| *c == 'x').count(),
            MAX_DETAIL_CHARS
        );

        // Exactly at the limit is kept whole; multi-byte characters count once each.
        let exact = "é".repeat(MAX_DETAIL_CHARS);
        let kept = internal_error("boom").with_detail(exact.clone()).detail;
        assert_eq!(kept.as_deref(), Some(exact.as_str()));

        // Blank detail is dropped rather than rendered as an empty section.
        assert_eq!(internal_error("boom").with_detail("  \n ").detail, None);
    }

    #[test]
    fn an_api_key_login_is_refused_with_its_reason() {
        let failure = not_chatgpt_account("apiKey", None);
        assert_eq!(failure.code, "NOT_AUTHENTICATED");
        assert!(failure.summary.contains("API-key auth is not supported"));
        assert!(failure.remediation.contains("codex login"));
        assert!(failure.remediation.contains("ChatGPT account"));
    }

    #[test]
    fn a_dedicated_home_is_named_in_the_login_instructions() {
        let failure = not_authenticated(Some(Path::new(r"D:\codex-home")));
        assert!(
            failure.remediation.contains(
                r"$env:CODEX_HOME = 'D:\codex-home'; codex login --device-auth; Remove-Item Env:CODEX_HOME"
            ),
            "{}",
            failure.remediation
        );
        assert!(failure
            .remediation
            .contains(r"CODEX_HOME='D:\codex-home' codex login --device-auth"));
        let ambient = not_authenticated(None);
        assert!(!ambient.remediation.contains("CODEX_HOME"));
        // A quote in the path, inside single quotes: doubled for PowerShell, escaped for Bash.
        let quoted = not_authenticated(Some(Path::new(r"D:\o'neil")));
        assert!(quoted
            .remediation
            .contains(r"$env:CODEX_HOME = 'D:\o''neil';"));
        assert!(quoted
            .remediation
            .contains(r"CODEX_HOME='D:\o'\''neil' codex login"));
        // The plan refusal signs in the same home.
        let plan = imagegen_unavailable_on_plan("free", Some(Path::new(r"D:\codex-home")));
        assert!(plan
            .remediation
            .contains(r"$env:CODEX_HOME = 'D:\codex-home'"));
    }

    #[test]
    fn a_wrong_explicit_path_is_told_apart_from_a_missing_install() {
        let explicit = cli_not_found(Some(r"C:\nope\codex.exe"), &[]);
        assert!(explicit.summary.contains("--codex-bin"));
        assert!(explicit.summary.contains(r"C:\nope\codex.exe"));
        let missing = cli_not_found(None, &[]);
        assert!(!missing.summary.contains("--codex-bin"));
    }

    #[test]
    fn every_codex_error_info_maps_to_the_designs_code() {
        // docs/design.md, "Turn-level failures", row by row.
        for (info, code) in [
            ("usageLimitExceeded", "RATE_LIMITED"),
            ("rateLimitExceeded", "RATE_LIMITED"),
            ("serverOverloaded", "RATE_LIMITED"),
            ("unauthorized", "AUTH_EXPIRED"),
            ("cyberPolicy", "CONTENT_REFUSED"),
            ("misalignmentPolicyViolation", "CONTENT_REFUSED"),
            ("contextWindowExceeded", "SESSION_NOT_RESUMABLE"),
            ("sessionBudgetExceeded", "SESSION_NOT_RESUMABLE"),
            ("internalServerError", "UPSTREAM_ERROR"),
            ("httpConnectionFailed", "UPSTREAM_ERROR"),
            ("responseStreamConnectionFailed", "UPSTREAM_ERROR"),
            ("responseStreamDisconnected", "UPSTREAM_ERROR"),
            ("responseTooManyFailedAttempts", "IMAGE_FAILED"),
            ("badRequest", "IMAGE_FAILED"),
            ("sandboxError", "IMAGE_FAILED"),
            ("other", "IMAGE_FAILED"),
            ("somethingNew", "IMAGE_FAILED"),
        ] {
            let failure = turn_failed(Some(info), "upstream said no", None);
            assert_eq!(failure.code, code, "{info}");
            let detail = failure.detail.unwrap();
            assert!(
                detail.contains(&format!("codexErrorInfo: {info}")),
                "{detail}"
            );
            assert!(detail.contains("upstream said no"), "{detail}");
        }
        let none = turn_failed(None, "no info", None);
        assert_eq!(none.code, "IMAGE_FAILED");
        assert!(none.summary.contains("codexErrorInfo none"));
    }

    #[test]
    fn an_untrusted_note_is_quoted_bounded_and_labelled() {
        let detail = untrusted_note_detail(Some("line one\n\u{202e}\"quoted\" and more"));
        assert!(
            detail.starts_with("Codex's closing message (untrusted model text"),
            "{detail}"
        );
        assert!(
            detail.ends_with(r#""line one \"quoted\" and more""#),
            "{detail}"
        );
        let long = untrusted_note_detail(Some(&"x".repeat(2000)));
        assert!(long.chars().count() < 700, "{long}");
        assert_eq!(
            untrusted_note_detail(None),
            "Codex sent no closing message."
        );
    }

    #[test]
    fn an_exhausted_image_quota_says_when_it_resets_or_that_it_is_unknown() {
        let known = image_quota_exhausted("image_gen", Some("2026-09-26 14:00"));
        assert!(known.summary.contains("resets at 2026-09-26 14:00"));
        let unknown = image_quota_exhausted("image_gen", None);
        assert!(unknown.summary.contains("reset time unknown"));
        assert!(unknown.summary.contains("'image_gen'"));
    }

    #[test]
    fn a_thread_failure_names_the_tool_and_the_os_error() {
        let text = handler_thread_unavailable("codex_imagegen_generate", "os error 8");
        assert!(text.starts_with("TOOL CALL NOT HANDLED\ncode: INTERNAL_ERROR\n"));
        assert!(text.contains("codex_imagegen_generate"));
        assert!(text.contains("os error 8"));
        assert!(text.contains("no image was produced"));
        assert!(handler_thread_unavailable("", "e").contains("'codex-imagegen' call"));
    }
}
