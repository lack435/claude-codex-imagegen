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
        Some(home) => format!(
            "codex-imagegen runs Codex with a dedicated home, {home} (its --codex-home flag), so \
             sign that home in: in a terminal, set the CODEX_HOME environment variable to \
             {home} and run\n\n\
             \x20 codex login --device-auth\n\n\
             Sign in with the ChatGPT account whose plan should pay for the images.",
            home = home.display()
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
pub fn imagegen_unavailable_on_plan(plan: &str) -> Failure {
    Failure::new(
        "IMAGEGEN_UNAVAILABLE",
        format!(
            "The signed-in ChatGPT account is on the '{plan}' plan, and codex-imagegen does not \
             generate images on that plan."
        ),
        "Sign Codex in with an account on a paid ChatGPT plan (run codex login in a terminal), \
         then retry. codex-imagegen picks up the new login on its next call, with no restart.",
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

/// Generation is declared but not built yet in this milestone.
pub fn not_implemented_yet() -> Failure {
    Failure::new(
        "INTERNAL_ERROR",
        "Image generation is not implemented in this build of codex-imagegen yet (milestone M2).",
        "This build of codex-imagegen can check its setup (codex_imagegen_status) but cannot \
         generate images yet. Tell the user that image generation needs a newer codex-imagegen \
         build.",
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
// Raised by the registry, which generate is wired to in the change that follows this one.
#[cfg_attr(not(test), allow(dead_code))]
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

/// `--max-concurrent` image calls are already running in this process.
#[cfg_attr(not(test), allow(dead_code))]
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
            imagegen_unavailable_on_plan("free"),
            imagegen_unavailable_capability(),
            imagegen_unavailable_setting("features.image_generation = false"),
            codex_config_unreadable("legacy `profile = \"x\"` config is no longer supported"),
            model_unavailable("gpt-6-astra", ""),
            bad_request("'prompt' must not be empty."),
            internal_error("boom"),
            not_implemented_yet(),
            timeout(300),
            server_shutting_down(),
            session_busy("fox", false),
            session_busy("fox", true),
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
        assert!(failure.remediation.contains(r"D:\codex-home"));
        assert!(failure.remediation.contains("CODEX_HOME"));
        let ambient = not_authenticated(None);
        assert!(!ambient.remediation.contains("CODEX_HOME"));
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
    fn a_thread_failure_names_the_tool_and_the_os_error() {
        let text = handler_thread_unavailable("codex_imagegen_generate", "os error 8");
        assert!(text.starts_with("TOOL CALL NOT HANDLED\ncode: INTERNAL_ERROR\n"));
        assert!(text.contains("codex_imagegen_generate"));
        assert!(text.contains("os error 8"));
        assert!(text.contains("no image was produced"));
        assert!(handler_thread_unavailable("", "e").contains("'codex-imagegen' call"));
    }
}
