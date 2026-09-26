//! One image turn, from its thread's start to the result Claude gets (docs/design.md, "Request
//! flow", "Message handling", "Output files", "Progress and cancellation" and "Errors").
//!
//! It runs on two threads:
//!
//! - [`route_notification`] runs on the app-server reader thread. It parses the few notifications
//!   a turn cares about into [`Event`]s and routes them by thread id, through the registry, into the
//!   channel of the call that owns the thread. It never blocks and does nothing else: a follow-up
//!   the registry asks for (an interrupt that just came due, the unsubscribe of a lingering turn)
//!   is only queued.
//! - [`run`] runs on the call's own thread: `config/read`, then `thread/start` for generate or
//!   `thread/resume` for refine (retried while the thread is closing here or another process
//!   holds it), `turn/start`, then each event as it arrives. A completed image is copied into the
//!   output folder, previewed and recorded in its session at once, not at the end of the turn, so
//!   a turn that is interrupted, times out or fails afterwards still keeps it, and `status` and a
//!   later refine see it. When the turn ends, however it ends, the thread is unsubscribed and the
//!   result is built from whatever images completed.
//!
//! The one rule that deserves care (AGENTS.md): an image that completed is never reported as a
//! failure. Whatever happens after it -- an interrupt, a timeout, a failed copy, a failed preview, a
//! dead child -- becomes a `warning:` line on a success.
//!
//! Nothing here classifies a failure from model text. The agent's closing message is only ever
//! quoted, marked untrusted.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::de::IgnoredAny;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{json, Value};

use crate::appserver::{AppServer, DetachedSender, RpcError};
use crate::cancel::RequestCancel;
use crate::codex::{self, Budget, Rpc};
use crate::config::Config;
use crate::errors::{self, Failure};
use crate::mcp::Progress;
use crate::output;
use crate::preview::{self, Preview};
use crate::registry::{ChildRef, FollowUp, Interrupt, Registry, TurnEvent, TurnSlot};
use crate::session::{ImageOutcome, SessionWriter};

/// How long a call waits for `turn/completed` after interrupting its turn, before it returns and
/// leaves the session lingering (docs/design.md, "After `turn/interrupt`").
pub const INTERRUPT_WAIT: Duration = Duration::from_secs(15);

/// How often the call's thread, waiting for the next event, checks for a cancellation, the end of
/// its budget and the death of the child. A cancellation's interrupt goes out at once from the
/// cancel hook; this only bounds how late the call itself notices.
const EVENT_POLL: Duration = Duration::from_millis(100);

/// How long a call that finds the child dead waits for the reader to reach the end of the child's
/// output. The process can be seen to exit before its last lines are read, an image's
/// `item/completed` among them, and the reader marks the end only once it has routed every line.
/// Above the up to 2 s the reader then takes to collect the exit code and the stderr tail; bounded,
/// because a grandchild holding the pipe open could delay the end indefinitely.
const EXIT_SETTLE: Duration = Duration::from_millis(2500);

/// The longest revised prompt the result text repeats. Prompts come from Claude and are normally
/// far shorter; the bound keeps a runaway one from filling the result.
const MAX_PROMPT_CHARS: usize = 4000;

/// How much of the agent's closing line the success text quotes.
const MAX_NOTE_CHARS: usize = 300;

/// How long a refine keeps retrying `thread/resume` while another process holds the thread's
/// writer lock: Codex's unload delay (`thread_unload_delay_secs=5` on the spawn line) plus 10 s
/// (docs/design.md, "Refine"), long enough for another codex-imagegen that has just finished a
/// turn on it to let it go.
pub const WRITER_WAIT: Duration = Duration::from_secs(15);

/// The pause before retrying a `thread/resume` that found the thread closing in this child, and
/// the first pause while another process holds it.
const RESUME_RETRY: Duration = Duration::from_millis(250);

/// The longest pause between retries while another process holds the writer lock.
const WRITER_BACKOFF_MAX: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A notification about one of our threads, as the call's thread needs it.
#[derive(Debug)]
pub enum Event {
    TurnStarted {
        turn_id: String,
    },
    /// `item/started` for an image call: the phase becomes "generating image".
    ImageStarted {
        turn_id: String,
    },
    /// `item/completed` for an image with status `completed`.
    ImageCompleted {
        turn_id: String,
        image: CompletedImage,
    },
    /// `item/completed` for an image with status `failed`: no file, no version.
    ImageFailed {
        turn_id: String,
        failure: ItemFailure,
    },
    /// `item/completed` for an agent message; the last one is Codex's closing line.
    AgentMessage {
        turn_id: String,
        text: String,
    },
    TurnCompleted {
        turn_id: String,
        status: String,
        error: Option<TurnError>,
    },
    ThreadClosed,
    /// `mcpServer/startupStatus/updated` on this thread: the canary (docs/design.md, "Canary").
    McpServerStarting {
        name: String,
        status: String,
    },
    /// A notification about this thread that should have been readable and was not: a protocol
    /// anomaly, reported naming the Codex version if nothing else explains a missing image.
    Unreadable {
        what: String,
    },
}

/// A completed image, as the notification described it.
#[derive(Debug)]
pub struct CompletedImage {
    pub revised_prompt: Option<String>,
    /// Codex's own copy of the PNG.
    pub saved_path: Option<PathBuf>,
    /// The PNG as base64, carried only when Codex sent no `savedPath` (docs/design.md, "Image
    /// items"); decoded on the call's thread, never on the reader.
    pub base64: Option<String>,
}

/// Why an image call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemFailure {
    /// `failure: {type: "usageLimitExceeded", limitId, resetsAt}`: the image quota.
    UsageLimit {
        limit_id: String,
        resets_at: Option<i64>,
    },
    /// `failure: null`: a refusal, a backend error or no image data, which Codex does not tell
    /// apart for us.
    Unspecified,
    /// A `failure` type this build does not know.
    Other(String),
    /// Codex reported the image completed but it could not be recovered: no `savedPath`, and no
    /// usable image data either.
    Unrecoverable(String),
}

/// `TurnError`, reduced to what the error mapping reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnError {
    /// The `codexErrorInfo` variant's name, such as `unauthorized` or `httpConnectionFailed`.
    pub info: Option<String>,
    pub message: String,
}

impl TurnEvent for Event {
    fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnStarted { turn_id }
            | Self::ImageStarted { turn_id }
            | Self::ImageCompleted { turn_id, .. }
            | Self::ImageFailed { turn_id, .. }
            | Self::AgentMessage { turn_id, .. }
            | Self::TurnCompleted { turn_id, .. } => Some(turn_id),
            Self::ThreadClosed | Self::McpServerStarting { .. } | Self::Unreadable { .. } => None,
        }
    }

    fn ends_turn(&self) -> bool {
        matches!(self, Self::TurnCompleted { .. })
    }

    fn keep_unrouted(&self) -> bool {
        matches!(self, Self::McpServerStarting { .. })
    }
}

// ---------------------------------------------------------------------------
// Parsing, on the reader thread
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadOnly {
    thread_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemParams<'a> {
    thread_id: String,
    turn_id: String,
    #[serde(borrow)]
    item: &'a RawValue,
}

#[derive(Deserialize)]
struct ItemKind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageItem {
    status: String,
    #[serde(default)]
    revised_prompt: Option<String>,
    /// The PNG as base64, up to about 3.8 MB [verified]: skipped, never copied. Read only in the
    /// rare case below, through [`ImageResult`].
    #[serde(default, rename = "result")]
    _result: IgnoredAny,
    #[serde(default)]
    failure: Option<Value>,
    #[serde(default)]
    saved_path: Option<String>,
}

/// The re-parse for a completed image with no `savedPath`: the base64 borrowed from the line, then
/// copied once to cross to the call's thread.
#[derive(Deserialize)]
struct ImageResult<'a> {
    #[serde(borrow)]
    result: Cow<'a, str>,
}

#[derive(Deserialize)]
struct AgentMessageItem {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnParams {
    thread_id: String,
    turn: TurnWire,
}

#[derive(Deserialize)]
struct TurnWire {
    id: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error: Option<TurnErrorWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TurnErrorWire {
    #[serde(default)]
    message: String,
    #[serde(default)]
    codex_error_info: Option<Value>,
    #[serde(default)]
    additional_details: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpStartupParams {
    #[serde(default)]
    thread_id: Option<String>,
    name: String,
    status: String,
}

/// The thread a notification is about and what it says, for the notifications a turn reads;
/// `None` for everything else. Two stages for items: the kind first, then a typed struct per kind,
/// with the image's base64 skipped (docs/design.md, "Parsing").
pub fn parse_notification(method: &str, params: &RawValue) -> Option<(String, Event)> {
    let text = params.get();
    let unreadable = |e: serde_json::Error| {
        // Attributed to its thread when at least that much can be read; otherwise only logged.
        let what = format!("a {method} notification it cannot read ({e})");
        match serde_json::from_str::<ThreadOnly>(text) {
            Ok(thread) => Some((thread.thread_id, Event::Unreadable { what })),
            Err(_) => {
                eprintln!("codex-imagegen: ignored {what}");
                None
            }
        }
    };
    match method {
        "turn/started" => match serde_json::from_str::<TurnParams>(text) {
            Ok(p) => Some((p.thread_id, Event::TurnStarted { turn_id: p.turn.id })),
            Err(e) => unreadable(e),
        },
        "turn/completed" => match serde_json::from_str::<TurnParams>(text) {
            Ok(p) => Some((
                p.thread_id,
                Event::TurnCompleted {
                    turn_id: p.turn.id,
                    status: p.turn.status.unwrap_or_else(|| "unknown".to_string()),
                    error: p.turn.error.map(|e| TurnError {
                        info: e.codex_error_info.as_ref().and_then(error_info_name),
                        message: match e.additional_details {
                            Some(more) if !more.is_empty() => format!("{} ({more})", e.message),
                            _ => e.message,
                        },
                    }),
                },
            )),
            Err(e) => unreadable(e),
        },
        "item/started" | "item/completed" => {
            let p = match serde_json::from_str::<ItemParams<'_>>(text) {
                Ok(p) => p,
                Err(e) => return unreadable(e),
            };
            let kind = match serde_json::from_str::<ItemKind<'_>>(p.item.get()) {
                Ok(k) => k.kind,
                Err(e) => return unreadable(e),
            };
            let event = match (method, kind.as_ref()) {
                ("item/started", "imageGeneration") => Event::ImageStarted { turn_id: p.turn_id },
                ("item/completed", "imageGeneration") => match image_event(p.turn_id, p.item) {
                    Ok(event) => event,
                    Err(e) => return unreadable(e),
                },
                ("item/completed", "agentMessage") => {
                    match serde_json::from_str::<AgentMessageItem>(p.item.get()) {
                        Ok(m) => Event::AgentMessage {
                            turn_id: p.turn_id,
                            text: m.text,
                        },
                        Err(e) => return unreadable(e),
                    }
                }
                // Every other item (the user message, reasoning, the exec call) says nothing a
                // turn needs, and unknown ones are skipped (docs/design.md, "Codex version
                // pinning").
                _ => return None,
            };
            Some((p.thread_id, event))
        }
        "thread/closed" => serde_json::from_str::<ThreadOnly>(text)
            .ok()
            .map(|p| (p.thread_id, Event::ThreadClosed)),
        "mcpServer/startupStatus/updated" => match serde_json::from_str::<McpStartupParams>(text) {
            // A status with no thread is about the server as a whole, not one of our threads.
            Ok(p) => p.thread_id.map(|thread_id| {
                (
                    thread_id,
                    Event::McpServerStarting {
                        name: p.name,
                        status: p.status,
                    },
                )
            }),
            Err(e) => unreadable(e),
        },
        _ => None,
    }
}

/// An image item's `item/completed`. The status is read before anything else it carries.
fn image_event(turn_id: String, item: &RawValue) -> serde_json::Result<Event> {
    let image: ImageItem = serde_json::from_str(item.get())?;
    Ok(match image.status.as_str() {
        "completed" => {
            // Only a completed item with no savedPath is read again for its base64.
            let base64 = match &image.saved_path {
                Some(_) => None,
                None => serde_json::from_str::<ImageResult<'_>>(item.get())
                    .ok()
                    .map(|r| r.result.into_owned())
                    .filter(|b| !b.is_empty()),
            };
            Event::ImageCompleted {
                turn_id,
                image: CompletedImage {
                    revised_prompt: image.revised_prompt,
                    saved_path: image.saved_path.map(PathBuf::from),
                    base64,
                },
            }
        }
        "failed" => Event::ImageFailed {
            turn_id,
            failure: item_failure(image.failure.as_ref()),
        },
        // Not an outcome: an item completes as completed or failed [verified: source].
        other => Event::Unreadable {
            what: format!("an image item that completed with status '{other}'"),
        },
    })
}

fn item_failure(failure: Option<&Value>) -> ItemFailure {
    let Some(failure) = failure.filter(|f| !f.is_null()) else {
        return ItemFailure::Unspecified;
    };
    match failure.get("type").and_then(Value::as_str) {
        Some("usageLimitExceeded") => ItemFailure::UsageLimit {
            limit_id: failure
                .get("limitId")
                .and_then(Value::as_str)
                .unwrap_or("image_gen")
                .to_string(),
            resets_at: failure.get("resetsAt").and_then(Value::as_i64),
        },
        Some(other) => ItemFailure::Other(crate::jsonrpc::clamp(other, 100)),
        None => ItemFailure::Other("unknown".to_string()),
    }
}

/// `codexErrorInfo` is a bare variant name, or an object keyed by one when the variant carries
/// data (`{"httpConnectionFailed": {"httpStatusCode": 502}}`).
fn error_info_name(info: &Value) -> Option<String> {
    match info {
        Value::String(name) => Some(name.clone()),
        Value::Object(map) => map.keys().next().cloned(),
        _ => None,
    }
}

/// The part of the app-server notification handler that serves turns: parse, route, and queue
/// whatever the registry says must happen now. Runs on the reader thread, so nothing here waits.
pub fn route_notification(
    registry: &Registry<Event>,
    sender: &DetachedSender,
    method: &str,
    params: &RawValue,
) {
    let Some((thread_id, event)) = parse_notification(method, params) else {
        return;
    };
    send_follow_up(sender, registry.route(&thread_id, event));
}

/// Queue whatever the registry says must be sent now.
fn send_follow_up(sender: &DetachedSender, follow_up: Option<FollowUp>) {
    match follow_up {
        Some(FollowUp::Interrupt(interrupt)) => send_interrupt(sender, &interrupt),
        Some(FollowUp::Unsubscribe { thread_id }) => send_unsubscribe(sender, &thread_id),
        None => {}
    }
}

/// Queue `turn/interrupt`, without waiting for its reply.
pub fn send_interrupt(sender: &DetachedSender, interrupt: &Interrupt) {
    eprintln!(
        "codex-imagegen: interrupting turn {} on thread {}",
        interrupt.turn_id, interrupt.thread_id
    );
    let params = json!({"threadId": interrupt.thread_id, "turnId": interrupt.turn_id});
    if let Err(e) = sender.send("turn/interrupt", params) {
        eprintln!("codex-imagegen: could not send turn/interrupt: {e}");
    }
}

/// Queue `thread/unsubscribe`, without waiting for its reply. After it, and Codex's unload delay,
/// the thread's writer lock is released, so another process can resume it (docs/design.md,
/// "Threads are locked across processes").
fn send_unsubscribe(sender: &DetachedSender, thread_id: &str) {
    if let Err(e) = sender.send("thread/unsubscribe", json!({"threadId": thread_id})) {
        eprintln!("codex-imagegen: could not send thread/unsubscribe: {e}");
    }
}

// ---------------------------------------------------------------------------
// The call, on its own thread
// ---------------------------------------------------------------------------

/// What a generate or refine call runs with.
pub struct Call<'a> {
    pub cfg: &'a Config,
    pub server: &'a AppServer,
    /// Which child `server` is, and whether it is alive (see [`TurnSlot::attach`]).
    pub child: ChildRef,
    /// For protocol-anomaly messages.
    pub codex_version: Option<&'a str>,
    pub cancel: &'a Arc<RequestCancel>,
    pub progress: &'a Progress,
    pub budget: Budget,
    pub started: Instant,
    pub interrupt_wait: Duration,
    /// How long `thread/resume` is retried while another process holds the thread
    /// ([`WRITER_WAIT`]).
    pub writer_wait: Duration,
    /// The usage part of the result's timing line, read from the cache when the result is built,
    /// so it includes the updates that arrived during the turn.
    pub usage: &'a dyn Fn() -> String,
}

/// Where the turn's thread comes from.
#[derive(Clone, Copy)]
pub enum Thread<'a> {
    /// generate: a new thread.
    Start,
    /// refine: the session's thread, resumed, and the image the edit applies to, already checked
    /// to exist (docs/design.md, "Refine").
    Resume {
        thread_id: &'a str,
        edit_target: &'a Path,
    },
}

/// A validated generate or refine request, with its output folder already pre-checked.
pub struct Request<'a> {
    /// The session's name as it names files: as it was first spelled.
    pub session: &'a str,
    /// The text the image tool is to use verbatim: generate's prompt, refine's feedback.
    pub prompt: &'a str,
    pub reference_images: &'a [PathBuf],
    pub output_dir: &'a Path,
    /// The version the first image is published as: the session record's `next_version`, 1 for a
    /// new session. A taken name bumps it (docs/design.md, "File names").
    pub first_version: u32,
    /// Records each image in the session store as it completes.
    pub record: &'a SessionWriter,
    pub thread: Thread<'a>,
}

/// A finished call: the success result, or the failure still unrendered (the tool layer reports a
/// failure caused by shutdown as the shutdown it was), and whether the turn failed with
/// `unauthorized`, so the tool layer retires the child once nothing runs on it (docs/design.md,
/// "When a turn fails with `AUTH_EXPIRED`").
pub struct Finished {
    pub result: Result<Value, Failure>,
    pub auth_expired: bool,
}

impl Finished {
    fn failed(failure: Failure) -> Self {
        Self {
            result: Err(failure),
            auth_expired: false,
        }
    }
}

/// Why the call interrupted its own turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Cancelled,
    TimedOut,
    Breach,
}

/// How the wait for the turn ended.
#[derive(Debug)]
enum Ended {
    Completed,
    ThreadClosed,
    ChildDied(String),
    /// The call interrupted the turn, and `turn/completed` did not come within the interrupt wait.
    GaveUp,
}

/// An image that completed, after its copy and preview.
struct Delivered {
    /// The version published, or `None` when the copy failed.
    version: Option<u32>,
    /// Where the full-resolution file is: our copy, else Codex's.
    path: Option<PathBuf>,
    bytes: Option<usize>,
    dims: Option<(u32, u32)>,
    preview: Option<Preview>,
    revised_prompt: Option<String>,
}

/// Everything the turn produced.
#[derive(Default)]
struct Turn {
    /// The Codex thread the turn runs on, recorded with each image.
    thread_id: String,
    images: Vec<Delivered>,
    failures: Vec<ItemFailure>,
    /// The last line of the last agent message.
    note: Option<String>,
    status: Option<String>,
    error: Option<TurnError>,
    image_started: bool,
    breach: Option<String>,
    unreadable: Option<String>,
    warnings: Vec<String>,
    next_version: u32,
}

/// Run one generate or refine turn and build its result. The slot is the call's claim on its
/// session: it is freed when the turn is over, or left lingering when the call gives up on a turn
/// Codex has not yet confirmed stopped.
pub fn run(call: &Call<'_>, slot: TurnSlot<Event>, request: &Request<'_>) -> Finished {
    let phase = |p: &str| {
        call.progress.set_phase(p);
        slot.set_phase(p);
    };
    phase("waiting for Codex");
    let rpc = Rpc {
        server: call.server,
        cancel: Some(call.cancel),
        per_call: codex::CALL_DEADLINE,
        budget: Some(call.budget),
    };
    let sender = call.server.detached_sender();

    let (thread_id, loaded_by) = match request.thread {
        Thread::Start => match start_thread(call, &rpc) {
            Ok(id) => (id, "thread/start"),
            Err(failure) => return Finished::failed(failure),
        },
        Thread::Resume {
            thread_id,
            edit_target,
        } => {
            phase("resuming session");
            if let Err(failure) =
                resume_thread(call, &rpc, &sender, request, thread_id, edit_target, &phase)
            {
                return Finished::failed(failure);
            }
            phase("waiting for Codex");
            (thread_id.to_string(), "thread/resume")
        }
    };
    let events = slot.attach(&thread_id, call.child.clone());

    // Installed once there is a thread, so a cancellation from here on interrupts the turn straight
    // from the MCP reader thread: the hook only asks the registry (one short lock) and queues a
    // line. Before its turn id is known, the registry remembers the request.
    let hooked = {
        let handle = slot.handle();
        let sender = sender.clone();
        call.cancel.set_hook(Box::new(move || {
            if let Some(interrupt) = handle.request_interrupt() {
                send_interrupt(&sender, &interrupt);
            }
        }))
    };
    if !hooked {
        send_unsubscribe(&sender, &thread_id);
        return Finished::failed(errors::cancelled());
    }

    let mut turn = Turn {
        thread_id: thread_id.clone(),
        next_version: request.first_version,
        ..Turn::default()
    };
    // The canary usually arrives right behind thread/start's reply [verified: smoke log]. Caught
    // here, before any turn, the agent model never sees the server's tools at all; one that comes
    // later still interrupts the turn.
    for event in events.try_iter() {
        if apply_event(&mut turn, event, request, &phase).is_some() {
            call.cancel.clear_hook();
            return Finished::failed(errors::app_server_failed(
                loaded_by,
                "Codex ended the thread before its turn started.",
            ));
        }
    }
    if let Some(name) = &turn.breach {
        call.cancel.clear_hook();
        send_unsubscribe(&sender, &thread_id);
        return Finished::failed(errors::isolation_breach(name));
    }

    let text = match request.thread {
        Thread::Start => codex::generate_input_text(request.prompt, request.reference_images),
        Thread::Resume { edit_target, .. } => {
            codex::refine_input_text(request.prompt, edit_target, request.reference_images)
        }
    };
    match rpc.call_raw(
        "turn/start",
        codex::turn_start_params(call.cfg, &thread_id, &text),
    ) {
        Ok(reply) => {
            if let Some(turn_id) = reply
                .get("turn")
                .and_then(|t| t.get("id"))
                .and_then(Value::as_str)
            {
                if let Some(interrupt) = slot.set_turn(turn_id) {
                    send_interrupt(&sender, &interrupt);
                }
            }
        }
        // Answered with an error: no turn started.
        Err(error @ RpcError::Remote { .. }) => {
            send_unsubscribe(&sender, &thread_id);
            return Finished::failed(rpc.failure("turn/start", error));
        }
        Err(error @ RpcError::ChildExited { .. }) => {
            return Finished::failed(rpc.failure("turn/start", error));
        }
        // Cancelled or out of time with turn/start unanswered: the turn may be starting anyway.
        // The session lingers, and the registry interrupts the turn as soon as its id turns up,
        // from the turn/started that follows the (dropped) reply.
        Err(error) => {
            let failure = rpc.failure("turn/start", error);
            call.cancel.clear_hook();
            send_follow_up(&sender, slot.linger());
            return Finished::failed(failure);
        }
    }

    let (ended, stop) = wait_for_turn(call, &slot, &events, &sender, request, &mut turn);
    // The cancel hook sends its interrupt itself, so Codex can answer it before this thread has
    // looked at the flag: a cancellation counts however the turn ended.
    let stop = stop.or_else(|| call.cancel.is_cancelled().then_some(Stop::Cancelled));
    call.cancel.clear_hook();
    match &ended {
        Ended::Completed => {
            send_unsubscribe(&sender, &thread_id);
            slot.finish();
        }
        // Codex has closed the thread, or the child is gone: there is nothing to unsubscribe from.
        Ended::ThreadClosed | Ended::ChildDied(_) => slot.finish(),
        // The unsubscribe waits for the turn/completed that frees the session, unless that came
        // in just as this call gave up: then it is due now.
        Ended::GaveUp => send_follow_up(&sender, slot.linger()),
    }
    let auth_expired = turn
        .error
        .as_ref()
        .is_some_and(|e| e.info.as_deref() == Some("unauthorized"));
    let result = if turn.images.is_empty() {
        Err(no_image_failure(call, request, &turn, &ended, stop))
    } else {
        Ok(success(call, request, turn, &ended, stop))
    };
    Finished {
        result,
        auth_expired,
    }
}

/// `config/read`, the MCP-off map built fresh from it, then `thread/start`. If the start fails
/// with a config error mentioning `mcp_servers` (a server removed between the read and the
/// start), the map is rebuilt from a new read and the start retried once (docs/design.md, "Thread
/// and turn parameters").
fn start_thread(call: &Call<'_>, rpc: &Rpc<'_>) -> Result<String, Failure> {
    for attempt in 0..2 {
        let config = codex::read_config(rpc, &call.cfg.work_dir)?;
        // Preflight checked the switches when this child started; the configuration can change
        // under a running child, and a thread started now reads it now.
        if let Some(setting) = codex::overridden_switch(&config) {
            return Err(errors::imagegen_unavailable_setting(&setting));
        }
        let params = codex::thread_start_params(call.cfg, &codex::mcp_off_map(&config.config));
        match rpc.call_raw("thread/start", params) {
            Ok(reply) => {
                return match reply
                    .get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                {
                    Some(id) if !id.is_empty() => Ok(id.to_string()),
                    _ => Err(anomaly(call, "a thread/start reply without a thread id")),
                };
            }
            Err(RpcError::Remote { message, .. })
                if attempt == 0 && message.contains("mcp_servers") =>
            {
                eprintln!(
                    "codex-imagegen: thread/start refused the MCP-off map ({}); reading the \
                     configuration again",
                    crate::jsonrpc::clamp(&message, 300)
                );
            }
            Err(e) => return Err(rpc.failure("thread/start", e)),
        }
    }
    unreachable!("the second attempt always returns")
}

/// `config/read`, the MCP-off map built fresh from it, then `thread/resume` of the session's
/// thread, before every refine turn. A thread still loaded here with no subscriber (inside its
/// unload delay) is shut down and resumed cold with the fresh map and instructions, which Codex
/// cannot apply to a loaded thread (docs/design.md, "Refine"). Its errors, by Codex's message:
///
/// - "is closing": this child is unloading the thread; retried every 250 ms.
/// - "already has an active writer": another process has it loaded, perhaps another
///   codex-imagegen inside its unload delay; retried with backoff for [`Call::writer_wait`], then
///   SESSION_OPEN_ELSEWHERE.
/// - "no rollout found": the thread's history is gone; SESSION_NOT_RESUMABLE.
/// - a config error naming `mcp_servers`: the map is rebuilt and the resume retried once, as for
///   `thread/start`.
/// - anything else: APP_SERVER_FAILED with the detail.
///
/// The resume itself gets whatever remains of the call's budget, and every pause between tries
/// ends at a cancellation or the end of the budget. A resume that may have subscribed this child
/// without the turn going ahead (abandoned on a cancel or timeout, an unreadable reply, an error
/// not listed above) is followed by `thread/unsubscribe`, so the thread's writer lock does not
/// outlive the call.
fn resume_thread(
    call: &Call<'_>,
    rpc: &Rpc<'_>,
    sender: &DetachedSender,
    request: &Request<'_>,
    thread_id: &str,
    edit_target: &Path,
    phase: &dyn Fn(&str),
) -> Result<(), Failure> {
    const METHOD: &str = "thread/resume";
    let give_up = Instant::now() + call.writer_wait;
    let mut backoff = RESUME_RETRY;
    let mut announced = false;
    let mut map_retried = false;
    loop {
        let config = codex::read_config(rpc, &call.cfg.work_dir)?;
        if let Some(setting) = codex::overridden_switch(&config) {
            return Err(errors::imagegen_unavailable_setting(&setting));
        }
        let params =
            codex::thread_resume_params(call.cfg, thread_id, &codex::mcp_off_map(&config.config));
        let resume = Rpc {
            server: rpc.server,
            cancel: rpc.cancel,
            per_call: call
                .budget
                .deadline
                .saturating_duration_since(Instant::now()),
            budget: rpc.budget,
        };
        let (code, message) = match resume.call_raw(METHOD, params) {
            Ok(reply) => {
                return match reply
                    .get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(Value::as_str)
                {
                    Some(id) if id == thread_id => Ok(()),
                    // Resumed, so subscribed, whatever the reply says.
                    _ => {
                        send_unsubscribe(sender, thread_id);
                        Err(anomaly(
                            call,
                            "a thread/resume reply that does not name the thread resumed",
                        ))
                    }
                };
            }
            Err(RpcError::Remote { code, message }) => (code, message),
            // Sent, but no longer waited for: Codex goes on with it, and may still load the thread
            // and subscribe this child. It runs the requests about one thread in the order they
            // arrive [verified: source, `request_serialization.rs`], so this unsubscribe follows
            // that resume, and the thread unloads after its delay whether or not the resume took.
            Err(e @ (RpcError::Cancelled | RpcError::Timeout { .. })) => {
                send_unsubscribe(sender, thread_id);
                return Err(resume.failure(METHOD, e));
            }
            // Never sent, or the child is gone: nothing to unsubscribe from.
            Err(e) => return Err(resume.failure(METHOD, e)),
        };
        if message.contains("no rollout found") {
            return Err(errors::session_not_resumable(
                request.session,
                "Codex no longer has this session's thread: its history was deleted (no rollout \
                 found). Nothing was spent.",
                Some(edit_target),
            )
            .with_detail(message));
        }
        let writer_held = message.contains("already has an active writer");
        let pause = if writer_held {
            if !announced {
                eprintln!(
                    "codex-imagegen: session {}: another process has its Codex thread loaded; \
                     waiting up to {} s for it to be released",
                    request.session,
                    call.writer_wait.as_secs()
                );
                phase("waiting for the session to be released");
                announced = true;
            }
            let pause = backoff;
            backoff = (backoff * 2).min(WRITER_BACKOFF_MAX);
            pause
        } else if message.contains("is closing") {
            RESUME_RETRY
        } else if message.contains("mcp_servers") && !map_retried {
            eprintln!(
                "codex-imagegen: thread/resume refused the MCP-off map ({}); reading the \
                 configuration again",
                crate::jsonrpc::clamp(&message, 300)
            );
            map_retried = true;
            continue;
        } else {
            // A few of Codex's resume errors come after it has subscribed this child [verified:
            // source, `thread_resume_inner`]. On a thread this child is not subscribed to, the
            // unsubscribe is harmless.
            send_unsubscribe(sender, thread_id);
            return Err(resume.failure(METHOD, RpcError::Remote { code, message }));
        };
        let now = Instant::now();
        if now >= give_up {
            return Err(if writer_held {
                errors::session_open_elsewhere(request.session, call.writer_wait.as_secs())
                    .with_detail(message)
            } else {
                errors::app_server_failed(
                    METHOD,
                    format!(
                        "Codex kept reporting the thread as closing for {} s: {message}",
                        call.writer_wait.as_secs()
                    ),
                )
            });
        }
        wait(call, pause.min(give_up - now))?;
    }
}

/// Sleep for `pause`, ending early with the failure to report when the call is cancelled or its
/// budget runs out.
fn wait(call: &Call<'_>, pause: Duration) -> Result<(), Failure> {
    let until = Instant::now() + pause;
    loop {
        if call.cancel.is_cancelled() {
            return Err(errors::cancelled());
        }
        let now = Instant::now();
        if now >= call.budget.deadline {
            return Err(errors::timeout(call.budget.secs));
        }
        if now >= until {
            return Ok(());
        }
        let next = until.min(call.budget.deadline);
        std::thread::sleep(next.saturating_duration_since(now).min(EVENT_POLL));
    }
}

/// A protocol anomaly, named with the Codex version (docs/design.md, "Codex version pinning").
fn anomaly(call: &Call<'_>, what: &str) -> Failure {
    errors::app_server_failed(
        "the image turn",
        format!(
            "codex-cli {} sent {what}. This build was tested with codex-cli {}.",
            call.codex_version.unwrap_or("(unknown version)"),
            codex::TESTED_RANGE
        ),
    )
}

/// Handle the turn's events until it ends, the call gives up on it, or the child dies. Returns how
/// it ended, and why the call interrupted it, if it did.
fn wait_for_turn(
    call: &Call<'_>,
    slot: &TurnSlot<Event>,
    events: &Receiver<Event>,
    sender: &DetachedSender,
    request: &Request<'_>,
    turn: &mut Turn,
) -> (Ended, Option<Stop>) {
    let phase = |p: &str| {
        call.progress.set_phase(p);
        slot.set_phase(p);
    };
    let mut stop: Option<Stop> = None;
    let mut give_up_at: Option<Instant> = None;
    loop {
        let now = Instant::now();
        if stop.is_none() {
            let reason = if call.cancel.is_cancelled() {
                Some(Stop::Cancelled)
            } else if turn.breach.is_some() {
                Some(Stop::Breach)
            } else if now >= call.budget.deadline {
                Some(Stop::TimedOut)
            } else {
                None
            };
            if let Some(reason) = reason {
                // Whoever asks first sends it: this, the cancel hook, or the registry once the
                // turn id is known.
                if let Some(interrupt) = slot.handle().request_interrupt() {
                    send_interrupt(sender, &interrupt);
                }
                stop = Some(reason);
                give_up_at = Some(now + call.interrupt_wait);
            }
        }
        let until = give_up_at.unwrap_or(call.budget.deadline);
        if give_up_at.is_some_and(|at| now >= at) {
            return (Ended::GaveUp, stop);
        }
        if !call.server.is_alive() {
            return (child_gone(call, turn, events, request, &phase), stop);
        }
        let slice = until.saturating_duration_since(now).min(EVENT_POLL);
        let event = match events.recv_timeout(slice) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => continue,
            // The registry drops the sender only with the slot, which this call still holds. A
            // channel reports this only once every event sent before has been received, so there
            // is nothing left to drain.
            Err(RecvTimeoutError::Disconnected) => {
                return (
                    Ended::ChildDied("the turn's event channel closed".to_string()),
                    stop,
                )
            }
        };
        if let Some(ended) = apply_event(turn, event, request, &phase) {
            return (ended, stop);
        }
    }
}

/// The child is gone. Its exit can be seen before the reader has read its last lines, and the
/// reader may have routed more while this thread was busy with an earlier image: an image that
/// completed in the child's last moments must still count (the success rule). So wait, bounded,
/// for the reader to reach the end of the output, which it marks only after routing every line,
/// then handle everything it routed. One of those events can still end the turn.
fn child_gone(
    call: &Call<'_>,
    turn: &mut Turn,
    events: &Receiver<Event>,
    request: &Request<'_>,
    phase: &dyn Fn(&str),
) -> Ended {
    let settle_until = Instant::now() + EXIT_SETTLE;
    while call.server.exit_detail().is_none() && Instant::now() < settle_until {
        std::thread::sleep(Duration::from_millis(20));
    }
    events
        .try_iter()
        .find_map(|event| apply_event(turn, event, request, phase))
        .unwrap_or_else(|| {
            Ended::ChildDied(
                call.server
                    .exit_detail()
                    .unwrap_or_else(|| "the process exited".to_string()),
            )
        })
}

/// What one event changes. `Some` when it ends the wait for the turn.
fn apply_event(
    turn: &mut Turn,
    event: Event,
    request: &Request<'_>,
    phase: &dyn Fn(&str),
) -> Option<Ended> {
    match event {
        Event::TurnStarted { .. } => {}
        Event::ImageStarted { .. } => {
            turn.image_started = true;
            phase("generating image");
        }
        Event::ImageCompleted { image, .. } => {
            phase("saving image");
            deliver(turn, image, request);
            phase("waiting for Codex");
        }
        Event::ImageFailed { failure, .. } => turn.failures.push(failure),
        Event::AgentMessage { text, .. } => {
            if let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) {
                turn.note = Some(line.trim().to_string());
            }
        }
        Event::TurnCompleted { status, error, .. } => {
            turn.status = Some(status);
            turn.error = error;
            return Some(Ended::Completed);
        }
        Event::ThreadClosed => return Some(Ended::ThreadClosed),
        Event::McpServerStarting { name, status } => {
            // A disabled server emits no startup status at all [verified: source, codex-mcp
            // connection_manager.rs starts only servers whose enabled() holds], so any report on
            // our thread, whether or not the server is in the disabled map, means a server is
            // starting. "disabled" is let through only in case a later Codex reports that state.
            if status != "disabled" && turn.breach.is_none() {
                eprintln!(
                    "codex-imagegen: isolation breach: MCP server '{}' reported '{}' on an image \
                     thread; stopping the turn",
                    crate::jsonrpc::clamp(&name, 100),
                    crate::jsonrpc::clamp(&status, 40)
                );
                turn.breach = Some(name);
            }
        }
        Event::Unreadable { what } => {
            eprintln!("codex-imagegen: Codex sent {what}");
            turn.unreadable.get_or_insert(what);
        }
    }
    None
}

/// Copy a completed image into the output folder and build its preview, straight away. Both are
/// best-effort: the image completed, so whatever fails here becomes a warning, never a failure
/// (docs/design.md, "Copy").
fn deliver(turn: &mut Turn, image: CompletedImage, request: &Request<'_>) {
    eprintln!(
        "{}",
        image_source_line(request.session, image.saved_path.as_deref())
    );
    let bytes: Result<Vec<u8>, String> = match (&image.saved_path, &image.base64) {
        // One read, shared by the copy and the preview.
        (Some(path), _) => std::fs::read(path)
            .map_err(|e| format!("could not read Codex's file {} ({e})", path.display())),
        (None, Some(base64)) => preview::base64_decode(base64.as_bytes()).ok_or_else(|| {
            "Codex sent no file path, and its image data is not valid base64".into()
        }),
        (None, None) => Err("Codex sent neither a file path nor the image data".to_string()),
    };
    let bytes = match (bytes, &image.saved_path) {
        (Ok(bytes), _) => bytes,
        // Codex has the file; it is just not readable from here. The line points at it.
        (Err(why), Some(path)) => {
            turn.warnings.push(format!(
                "{why}, so it was not copied into {} and has no preview; the image line points at \
                 Codex's own copy",
                request.output_dir.display()
            ));
            record_image(
                turn,
                request,
                &ImageOutcome {
                    version: None,
                    output_path: None,
                    saved_path: Some(path),
                    bytes: None,
                },
            );
            turn.images.push(Delivered {
                version: None,
                path: Some(path.clone()),
                bytes: None,
                dims: None,
                preview: None,
                revised_prompt: image.revised_prompt,
            });
            return;
        }
        // No file anywhere and nothing to decode: there is no image to hand back.
        (Err(why), None) => {
            eprintln!("codex-imagegen: a completed image could not be recovered: {why}");
            turn.failures.push(ItemFailure::Unrecoverable(why));
            return;
        }
    };

    let (version, path) = match output::publish(
        request.output_dir,
        request.session,
        turn.next_version,
        &bytes,
    ) {
        Ok(published) => {
            turn.next_version = published.version + 1;
            (Some(published.version), Some(published.path))
        }
        Err(e) => {
            let fallback = image.saved_path.clone();
            turn.warnings.push(match &fallback {
                Some(saved) => format!(
                    "could not copy the image into {} ({e}); the image line points at Codex's \
                         own copy, {}",
                    request.output_dir.display(),
                    saved.display()
                ),
                None => format!(
                    "could not save the image into {} ({e}), and Codex kept no copy of its \
                         own: the preview is the only copy",
                    request.output_dir.display()
                ),
            });
            (None, fallback)
        }
    };
    let preview = match preview::build(&bytes) {
        Ok(preview) => {
            eprintln!(
                "codex-imagegen: preview {}x{} {}{}, {} bytes",
                preview.width,
                preview.height,
                preview.mime_type,
                preview
                    .quality
                    .map_or_else(String::new, |q| format!(" quality {q}")),
                preview.encoded_bytes
            );
            Some(preview)
        }
        Err(why) => {
            turn.warnings.push(format!(
                "no preview could be built ({why}); open the file to see the image"
            ));
            None
        }
    };
    let dims = preview
        .as_ref()
        .map(|p| (p.source_width, p.source_height))
        .or_else(|| preview::png_dimensions(&bytes));
    record_image(
        turn,
        request,
        &ImageOutcome {
            version,
            // `path` falls back to Codex's copy when the copy failed; only ours is an output.
            output_path: version.and(path.as_deref()),
            saved_path: image.saved_path.as_deref(),
            bytes: Some(bytes.len() as u64),
        },
    );
    turn.images.push(Delivered {
        version,
        path,
        bytes: Some(bytes.len()),
        dims,
        preview,
        revised_prompt: image.revised_prompt,
    });
}

/// Record a delivered image in the session store, at once. A write that fails is a warning: the
/// image completed and was delivered, and only a later refine is affected (docs/design.md,
/// "Success result").
fn record_image(turn: &mut Turn, request: &Request<'_>, image: &ImageOutcome<'_>) {
    if let Err(e) = request.record.image_completed(&turn.thread_id, image) {
        eprintln!(
            "codex-imagegen: session {}: could not update the session record: {e}",
            request.session
        );
        turn.warnings.push(format!(
            "the session record could not be updated ({e}), so {} may not find this session",
            crate::tools::REFINE
        ));
    }
}

/// The stderr line saying where a completed image's bytes come from. The base64 fallback is
/// otherwise invisible in the result, so `smoke.ps1` reads this line to check that Codex reports
/// `savedPath` (V1); keep the two in step. Only the path is logged, never the data.
fn image_source_line(session: &str, saved_path: Option<&Path>) -> String {
    match saved_path {
        Some(path) => format!(
            "codex-imagegen: session {session}: image item has savedPath {}",
            path.display()
        ),
        None => format!(
            "codex-imagegen: session {session}: image item has no savedPath; falling back to its \
             base64 data"
        ),
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// At least one image completed: a success, with a warning for anything unusual (docs/design.md,
/// "Success result").
fn success(
    call: &Call<'_>,
    request: &Request<'_>,
    mut turn: Turn,
    ended: &Ended,
    stop: Option<Stop>,
) -> Value {
    let mut warnings = end_warnings(call, request, &turn, ended, stop);
    warnings.append(&mut turn.warnings);

    let count = turn.images.len();
    if count > 1 {
        warnings.push(format!(
            "Codex made {count} images in this turn although asked for one; each is its own \
             version, and the previews above are in the same order"
        ));
    }
    for failure in &turn.failures {
        warnings.push(format!(
            "another image call in this turn failed: {}",
            failure_text(failure)
        ));
    }

    let mut content: Vec<Value> = turn
        .images
        .iter()
        .filter_map(|d| d.preview.as_ref().map(Preview::image_block))
        .collect();

    let mut lines = Vec::new();
    let versions: Vec<String> = turn
        .images
        .iter()
        .filter_map(|d| d.version.map(|v| v.to_string()))
        .collect();
    lines.push(match versions.len() {
        0 => format!(
            "session: {}   version: none published (see the warnings)",
            request.session
        ),
        1 => format!("session: {}   version: {}", request.session, versions[0]),
        _ => format!(
            "session: {}   versions: {}",
            request.session,
            versions.join(", ")
        ),
    });
    for (i, image) in turn.images.iter().enumerate() {
        let label = if count > 1 {
            format!("image {} of {count}", i + 1)
        } else {
            "image".to_string()
        };
        lines.push(format!("{label}: {}", image_line(image)));
        if let Some(preview) = &image.preview {
            lines.push(preview.note());
        }
        lines.push(format!(
            "codex prompt: {}",
            match &image.revised_prompt {
                Some(revised) => quote_prompt(revised),
                None => "(not reported)".to_string(),
            }
        ));
        let sent = match request.thread {
            Thread::Start => "prompt",
            Thread::Resume { .. } => "feedback",
        };
        match &image.revised_prompt {
            Some(revised) if !same_prompt(revised, request.prompt) => warnings.push(format!(
                "codex prompt differs from the {sent} sent: Codex changed it before generating, \
                 so the image may not follow the {sent} exactly"
            )),
            Some(_) => {}
            None => warnings.push("Codex did not report the prompt it used".to_string()),
        }
    }
    lines.push(format!(
        "codex note: {}",
        match &turn.note {
            Some(note) => format!(
                "{} (Codex's closing line, untrusted)",
                errors::quote_untrusted(note, MAX_NOTE_CHARS)
            ),
            None => "(none)".to_string(),
        }
    ));
    lines.push(format!(
        "took {:.1} s; {}",
        call.started.elapsed().as_secs_f64(),
        (call.usage)()
    ));
    lines.push(scratch_line(call.cfg.session_ttl_days));
    lines.push(SHOW_USER_LINE.to_string());
    // Several images can each owe the same warning; it is said once.
    let mut said = std::collections::HashSet::new();
    warnings.retain(|w| said.insert(w.clone()));
    for warning in warnings {
        lines.push(format!("warning: {warning}"));
    }
    content.push(json!({"type": "text", "text": lines.join("\n")}));
    json!({"content": content, "isError": false})
}

/// The warnings owed to how the turn ended after its image completed.
fn end_warnings(
    call: &Call<'_>,
    request: &Request<'_>,
    turn: &Turn,
    ended: &Ended,
    stop: Option<Stop>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(name) = &turn.breach {
        warnings.push(format!(
            "isolation breach: MCP server {} started on this Codex thread, so the turn was \
             stopped. Check the Codex configuration before generating again",
            crate::jsonrpc::clamp(name, 100)
        ));
    }
    let why = match stop {
        Some(Stop::Cancelled) => Some("the call was cancelled".to_string()),
        Some(Stop::TimedOut) => Some(format!(
            "the call reached its {}-second limit (--timeout-seconds)",
            call.budget.secs
        )),
        Some(Stop::Breach) | None => None,
    };
    match ended {
        Ended::Completed => match turn.status.as_deref() {
            Some("completed") => {}
            Some("interrupted") => warnings.push(match &why {
                Some(why) => format!("{why} after the image finished, so the turn was interrupted"),
                None if stop == Some(Stop::Breach) => {
                    "the turn was interrupted after the image finished".to_string()
                }
                None => "Codex interrupted the turn after the image finished".to_string(),
            }),
            Some("failed") => {
                let failure = turn_failure(call, request, turn);
                warnings.push(format!(
                    "the turn failed after the image finished ({}: {})",
                    failure.code, failure.summary
                ));
            }
            other => warnings.push(format!(
                "the turn ended as '{}' after the image finished",
                other.unwrap_or("unknown")
            )),
        },
        Ended::GaveUp => warnings.push(format!(
            "{} after the image finished; the turn was interrupted, Codex had not confirmed it \
             within {} s, and the session stays busy until it does",
            why.unwrap_or_else(|| "the turn was stopped".to_string()),
            call.interrupt_wait.as_secs()
        )),
        Ended::ChildDied(_) => {
            warnings.push("the Codex app-server exited after the image finished".to_string())
        }
        Ended::ThreadClosed => {
            warnings.push("Codex closed the thread before the turn completed".to_string())
        }
    }
    if let Some(what) = &turn.unreadable {
        warnings.push(format!(
            "codex-cli {} sent {what}",
            call.codex_version.unwrap_or("(unknown version)")
        ));
    }
    warnings
}

/// No image completed: the failure that best explains it (docs/design.md, "Errors"). What the call
/// knows for certain comes first (a breach, a cancellation, a dead child), then Codex's typed turn
/// error, then the image items, and only then the vaguer outcomes.
fn no_image_failure(
    call: &Call<'_>,
    request: &Request<'_>,
    turn: &Turn,
    ended: &Ended,
    stop: Option<Stop>,
) -> Failure {
    if let Some(name) = &turn.breach {
        return errors::isolation_breach(name);
    }
    if stop == Some(Stop::Cancelled) {
        return errors::cancelled();
    }
    if let Ended::ChildDied(detail) = ended {
        return errors::child_died_mid_turn(format!(
            "The Codex app-server stopped during the turn: {detail}"
        ));
    }
    if turn.status.as_deref() == Some("failed") {
        return turn_failure(call, request, turn);
    }
    let note = turn.note.as_deref();
    if let Some((limit_id, resets_at)) = turn.failures.iter().find_map(|f| match f {
        ItemFailure::UsageLimit {
            limit_id,
            resets_at,
        } => Some((limit_id, resets_at)),
        _ => None,
    }) {
        let resets = resets_at.and_then(codex::local_time);
        return errors::image_quota_exhausted(limit_id, resets.as_deref());
    }
    if let Some(failure) = turn.failures.first() {
        let failure_detail = failure_text(failure);
        let failed = errors::image_failed(note);
        return match failed.detail.clone() {
            Some(detail) => failed.with_detail(format!("{failure_detail}\n{detail}")),
            None => failed.with_detail(failure_detail),
        };
    }
    if stop == Some(Stop::TimedOut) {
        return errors::timeout(call.budget.secs);
    }
    if let Some(what) = &turn.unreadable {
        return anomaly(call, what);
    }
    match ended {
        Ended::ThreadClosed => errors::app_server_failed(
            "the image turn",
            "Codex closed the thread before the turn completed.",
        ),
        _ => match turn.status.as_deref() {
            Some("completed") if turn.image_started => errors::image_failed(note),
            // A refine always hands Codex an image to read: its edit target.
            Some("completed") => errors::no_image(
                note,
                !request.reference_images.is_empty()
                    || matches!(request.thread, Thread::Resume { .. }),
            ),
            Some(status) => errors::turn_ended_without_image(status, note),
            None => errors::turn_ended_without_image("unknown", note),
        },
    }
}

/// The failure a failed turn's `TurnError` maps to. A refine whose session has run out of room
/// names its edit target as the new session's reference (docs/design.md, "Turn-level failures").
fn turn_failure(call: &Call<'_>, request: &Request<'_>, turn: &Turn) -> Failure {
    let (info, message) = match &turn.error {
        Some(error) => (error.info.as_deref(), error.message.as_str()),
        None => (None, "Codex gave no error."),
    };
    let failure = errors::turn_failed(info, message, call.cfg.codex_home.as_deref());
    match request.thread {
        Thread::Resume { edit_target, .. } if failure.code == "SESSION_NOT_RESUMABLE" => {
            let mut resumable =
                errors::session_not_resumable(request.session, failure.summary, Some(edit_target));
            resumable.detail = failure.detail;
            resumable
        }
        _ => failure,
    }
}

fn failure_text(failure: &ItemFailure) -> String {
    match failure {
        ItemFailure::UsageLimit {
            limit_id,
            resets_at,
        } => match resets_at.and_then(codex::local_time) {
            Some(at) => format!("the image quota ('{limit_id}') is used up until {at}"),
            None => format!("the image quota ('{limit_id}') is used up; reset time unknown"),
        },
        ItemFailure::Unspecified => {
            "the image tool reported a failure (a refusal, a backend error or no image data)"
                .to_string()
        }
        ItemFailure::Other(kind) => format!("the image tool reported a '{kind}' failure"),
        ItemFailure::Unrecoverable(why) => {
            format!("Codex reported an image as completed, but it could not be recovered: {why}")
        }
    }
}

/// `<path>  (1312x1199, 2.6 MB PNG)`.
fn image_line(image: &Delivered) -> String {
    let path = match &image.path {
        Some(path) => path.display().to_string(),
        None => "not saved (see the warnings)".to_string(),
    };
    let size = image.bytes.map(format_size);
    match (image.dims, size) {
        (Some((w, h)), Some(size)) => format!("{path}  ({w}x{h}, {size} PNG)"),
        (None, Some(size)) => format!("{path}  ({size} PNG)"),
        (Some((w, h)), None) => format!("{path}  ({w}x{h} PNG)"),
        (None, None) => path,
    }
}

fn format_size(bytes: usize) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// The prompt Codex used, as one JSON-quoted line: escapes keep a newline in it from reading as the
/// next line of the result, and characters that hide text are removed.
fn quote_prompt(prompt: &str) -> String {
    let mut shown: String = prompt
        .chars()
        .filter(|c| !crate::jsonrpc::hides_text(*c))
        .take(MAX_PROMPT_CHARS)
        .collect();
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        shown.push('…');
    }
    serde_json::to_string(&shown).unwrap_or_else(|_| "\"\"".to_string())
}

/// Whether Codex used the prompt as sent. Whitespace at either end is not counted: the tags put the
/// prompt on lines of its own, so where it starts and ends is the agent's reading of the layout,
/// not a change to the prompt [decided].
fn same_prompt(revised: &str, sent: &str) -> bool {
    revised.trim() == sent.trim()
}

/// Every success tells the agent to show the image. Many clients fold tool results away (the Claude
/// desktop app shows the preview only inside the collapsed tool row [verified: owner, 2026-09-25]),
/// so the preview is often seen by the agent alone [decided].
const SHOW_USER_LINE: &str = "the user may not see this tool result: show them the image, by \
     displaying or sending the file if you have a tool for that, otherwise by giving them its path";

/// The scratch-space line every success carries (docs/design.md, "Output files").
fn scratch_line(ttl_days: u32) -> String {
    match ttl_days {
        0 => "output files are scratch space; move keepers into the project".to_string(),
        1 => "output files are scratch and expire after 1 day idle; move keepers into the project"
            .to_string(),
        n => format!(
            "output files are scratch and expire after {n} days idle; move keepers into the project"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(value: Value) -> Box<RawValue> {
        RawValue::from_string(value.to_string()).unwrap()
    }

    fn parse(message: Value) -> Option<(String, Event)> {
        parse_notification(
            message["method"].as_str().unwrap(),
            &raw(message["params"].clone()),
        )
    }

    use crate::codex::testing::{
        agent_message, image_completed, image_failed, image_started, notification, turn_completed,
        turn_error, turn_started, THREAD_ID, TURN_ID,
    };

    #[test]
    fn the_notifications_a_turn_reads_are_parsed_and_the_rest_ignored() {
        let (thread, event) = parse(turn_started()).unwrap();
        assert_eq!(thread, THREAD_ID);
        assert!(matches!(event, Event::TurnStarted { turn_id } if turn_id == TURN_ID));

        let (_, event) = parse(image_started("exec-1")).unwrap();
        assert!(matches!(event, Event::ImageStarted { .. }));
        assert_eq!(event.turn_id(), Some(TURN_ID));

        let (_, event) = parse(agent_message("Made it.\nCreated your watercolor fox.")).unwrap();
        assert!(
            matches!(event, Event::AgentMessage { text, .. } if text.ends_with("watercolor fox."))
        );

        let (_, event) = parse(turn_completed("completed", Value::Null)).unwrap();
        assert!(event.ends_turn());
        assert!(
            matches!(event, Event::TurnCompleted { status, error: None, .. }
                         if status == "completed")
        );

        // Items a turn does not read, and methods it does not care about.
        let user = notification(
            "item/completed",
            json!({"turnId": TURN_ID, "item": {"type": "userMessage", "id": "u", "content": []}}),
        );
        assert!(parse(user).is_none());
        let started_msg = notification(
            "item/started",
            json!({"turnId": TURN_ID, "item": {"type": "agentMessage", "id": "m", "text": ""}}),
        );
        assert!(parse(started_msg).is_none());
        assert!(parse(notification("thread/status/changed", json!({}))).is_none());
        assert!(matches!(
            parse(notification("thread/closed", json!({}))),
            Some((_, Event::ThreadClosed))
        ));
    }

    #[test]
    fn a_completed_image_is_read_without_its_base64_unless_it_has_no_saved_path() {
        let big = "A".repeat(100_000);
        let saved = Path::new(r"C:\codex\generated_images\t\exec-1.png");
        let (_, event) = parse(image_completed("exec-1", "a fox", &big, Some(saved))).unwrap();
        let Event::ImageCompleted { image, .. } = event else {
            panic!("{event:?}")
        };
        assert_eq!(image.saved_path.as_deref(), Some(saved));
        assert_eq!(image.revised_prompt.as_deref(), Some("a fox"));
        assert!(image.base64.is_none(), "the base64 was copied");

        let (_, event) = parse(image_completed("exec-1", "a fox", "iVBORw0K", None)).unwrap();
        let Event::ImageCompleted { image, .. } = event else {
            panic!("{event:?}")
        };
        assert_eq!(image.saved_path, None);
        assert_eq!(image.base64.as_deref(), Some("iVBORw0K"));
    }

    #[test]
    fn failed_images_carry_their_failure() {
        let usage = image_failed(
            "exec-1",
            json!({"type": "usageLimitExceeded", "limitId": "image_gen", "resetsAt": 1790710629}),
        );
        let (_, event) = parse(usage).unwrap();
        assert!(
            matches!(event, Event::ImageFailed { failure: ItemFailure::UsageLimit {
            limit_id, resets_at: Some(1790710629) }, .. } if limit_id == "image_gen")
        );
        let (_, event) = parse(image_failed("exec-1", Value::Null)).unwrap();
        assert!(matches!(
            event,
            Event::ImageFailed {
                failure: ItemFailure::Unspecified,
                ..
            }
        ));
        let (_, event) = parse(image_failed("exec-1", json!({"type": "brandNew"}))).unwrap();
        assert!(
            matches!(event, Event::ImageFailed { failure: ItemFailure::Other(k), .. }
                         if k == "brandNew")
        );
    }

    #[test]
    fn a_turn_error_keeps_the_codex_error_info_name_whatever_its_shape() {
        for (info, name) in [
            (json!("unauthorized"), Some("unauthorized")),
            (
                json!({"httpConnectionFailed": {"httpStatusCode": 502}}),
                Some("httpConnectionFailed"),
            ),
            (Value::Null, None),
        ] {
            let (_, event) = parse(turn_completed("failed", turn_error(info))).unwrap();
            let Event::TurnCompleted {
                error: Some(error), ..
            } = event
            else {
                panic!("{event:?}")
            };
            assert_eq!(error.info.as_deref(), name);
            assert_eq!(error.message, "the turn failed upstream");
        }
    }

    #[test]
    fn the_canary_is_kept_until_attach_and_a_threadless_status_is_not_ours() {
        let (thread, event) = parse(notification(
            "mcpServer/startupStatus/updated",
            json!({"name": "rogue", "status": "starting", "error": null}),
        ))
        .unwrap();
        assert_eq!(thread, THREAD_ID);
        assert!(event.keep_unrouted());
        assert!(matches!(event, Event::McpServerStarting { name, .. } if name == "rogue"));
        assert!(parse(json!({"method": "mcpServer/startupStatus/updated",
            "params": {"threadId": null, "name": "x", "status": "ready"}}))
        .is_none());
    }

    #[test]
    fn an_unreadable_notification_about_our_thread_is_reported_not_dropped() {
        let broken = notification(
            "item/completed",
            json!({"turnId": TURN_ID, "item": {"type": "imageGeneration", "id": 7}}),
        );
        let (thread, event) = parse(broken).unwrap();
        assert_eq!(thread, THREAD_ID);
        assert!(matches!(event, Event::Unreadable { what } if what.contains("item/completed")));
        // Without even a thread id there is nobody to tell.
        assert!(parse_notification("turn/completed", &raw(json!({"x": 1}))).is_none());
    }

    #[test]
    fn the_image_source_line_keeps_the_wording_smoke_ps1_reads() {
        // smoke.ps1 matches these lines to check that Codex reported savedPath (V1).
        assert_eq!(
            image_source_line(
                "smoke-1",
                Some(Path::new(r"C:\codex\generated_images\t\exec-1.png"))
            ),
            r"codex-imagegen: session smoke-1: image item has savedPath C:\codex\generated_images\t\exec-1.png"
        );
        assert!(image_source_line("smoke-1", None)
            .starts_with("codex-imagegen: session smoke-1: image item has no savedPath;"));
    }

    #[test]
    fn sizes_prompts_and_the_scratch_line_are_formatted_for_the_result() {
        assert_eq!(format_size(2_726_297), "2.6 MB");
        assert_eq!(format_size(300_000), "293 KB");
        assert_eq!(quote_prompt("a \"red\"\nfox\\"), r#""a \"red\"\nfox\\""#);
        assert_eq!(quote_prompt("a\u{202e}b"), r#""ab""#);
        assert!(quote_prompt(&"x".repeat(5000)).ends_with("…\""));
        assert!(same_prompt("a fox", "  a fox\n"));
        assert!(!same_prompt("a fox, detailed", "a fox"));
        assert_eq!(
            scratch_line(7),
            "output files are scratch and expire after 7 days idle; move keepers into the project"
        );
        assert!(scratch_line(1).contains("after 1 day idle"));
        assert!(!scratch_line(0).contains("expire"));
        assert!(SHOW_USER_LINE.starts_with("the user may not see this tool result"));
    }
}

/// Whole generate calls through the tool layer, against a scripted fake Codex. No real Codex, no
/// quota: the fake replays the notification shapes codex-cli 0.156.0 sent in the smoke run.
#[cfg(test)]
mod generate_tests {
    use super::*;
    use crate::codex::testing::{
        agent_message, image_completed, image_failed, image_started, notification, turn_completed,
        turn_error, turn_started, FakeCodex, Step, TurnScript, THREAD_ID, TURN_ID,
    };
    use crate::mcp::{CallContext, ToolHost};
    use crate::preview::testing::photo_like_png;
    use crate::session::{Output, Store};
    use crate::testutil::{temp_dir, TempDir};
    use crate::tools::testing::{fixture, fixture_with, result_text, Fixture};
    use crate::tools::{GENERATE, STATUS};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    pub(super) const WAIT: Duration = Duration::from_secs(10);

    /// Codex's own copy of an image, where the image tool leaves it.
    pub(super) struct Saved {
        _dir: TempDir,
        pub(super) path: PathBuf,
        pub(super) bytes: Vec<u8>,
    }

    pub(super) fn saved_file(bytes: Vec<u8>) -> Saved {
        let dir = temp_dir("turn");
        let path = dir.join("exec-1.png");
        std::fs::write(&path, &bytes).unwrap();
        Saved {
            _dir: dir,
            path,
            bytes,
        }
    }

    pub(super) fn saved_png() -> Saved {
        saved_file(photo_like_png(96, 64))
    }

    fn usage_update() -> Value {
        json!({"method": "account/rateLimits/updated", "params": {"rateLimits": {
            "limitId": "codex", "primary": {"usedPercent": 44, "windowDurationMins": 10080,
            "resetsAt": null}, "secondary": null}}})
    }

    /// A turn that makes one image, in the order codex-cli 0.156.0 sends it [verified: smoke log].
    pub(super) fn image_turn(saved: &Path, revised: &str) -> Vec<Step> {
        vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            Step::Send(image_completed("exec-1", revised, "", Some(saved))),
            Step::Send(usage_update()),
            Step::Send(agent_message("Created your fox.")),
            Step::Send(turn_completed("completed", Value::Null)),
        ]
    }

    pub(super) fn codex_with(turn: TurnScript) -> FakeCodex {
        FakeCodex {
            turn,
            ..FakeCodex::default()
        }
    }

    pub(super) fn codex(steps: Vec<Step>) -> FakeCodex {
        codex_with(TurnScript {
            steps,
            ..TurnScript::default()
        })
    }

    fn generate(f: &Fixture, args: Value) -> Value {
        f.app.call_tool(GENERATE, &args, &CallContext::detached())
    }

    pub(super) fn text_of(result: &Value) -> String {
        result_text(result).1
    }

    /// A failure's code, from its rendered text.
    pub(super) fn code_of(result: &Value) -> String {
        let (is_error, text) = result_text(result);
        assert!(is_error, "not a failure: {text}");
        text.lines()
            .find_map(|l| l.strip_prefix("code: "))
            .unwrap_or_else(|| panic!("no code in {text}"))
            .to_string()
    }

    /// Wait until `method` has been sent at least `count` times; its params, in order.
    pub(super) fn wait_sent(seen: &Mutex<Vec<Value>>, method: &str, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            let sent = FakeCodex::sent(seen, method);
            if sent.len() >= count {
                return sent;
            }
            assert!(
                Instant::now() < deadline,
                "{method} was sent {} time(s), not {count}",
                sent.len()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn wait_until(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + WAIT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(super) fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_generate_turn_publishes_its_image_and_returns_a_preview_and_the_designs_text() {
        let saved = saved_png();
        let prompt = "A \"red\" fox\\ on a rock,\nwatercolour \u{2014} \u{fc}ber caf\u{e9}";
        let fake = codex(image_turn(&saved.path, prompt));
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let reference = f.dir.join("ref.png");
        std::fs::write(&reference, &saved.bytes).unwrap();

        let result = generate(
            &f,
            json!({"prompt": prompt, "session": "fox", "reference_images": ["ref.png"]}),
        );

        let starts = FakeCodex::sent(&seen, "thread/start");
        assert_eq!(
            starts,
            vec![json!({
                "model": "gpt-6-astra",
                "cwd": f.dir.join("state").join("work"),
                "sandbox": "read-only",
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "developerInstructions": codex::DEVELOPER_INSTRUCTIONS,
                "config": {"mcp_servers": {"cua_repl": {"enabled": false},
                                           "node_repl": {"enabled": false}}},
                "ephemeral": false,
            })]
        );
        let turn_text = format!(
            "<image_prompt>\n{prompt}\n</image_prompt>\n<reference_images>\n{}\n</reference_images>",
            reference.display()
        );
        assert_eq!(
            FakeCodex::sent(&seen, "turn/start"),
            vec![json!({
                "threadId": THREAD_ID,
                "input": [{"type": "text", "text": turn_text, "text_elements": []}],
                "model": "gpt-6-astra",
                "effort": "low",
            })]
        );

        assert_eq!(result["isError"], false, "{result}");
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 2, "{result}");
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["mimeType"], "image/jpeg");
        let jpeg = preview::base64_decode(content[0]["data"].as_str().unwrap().as_bytes()).unwrap();
        assert!(jpeg.starts_with(b"\xff\xd8\xff"));

        let published = f.dir.join("generated-images").join("fox-v1.png");
        assert_eq!(
            std::fs::read(&published).unwrap(),
            saved.bytes,
            "not a byte copy"
        );
        assert_eq!(listing(published.parent().unwrap()), vec!["fox-v1.png"]);

        let text = content[1]["text"].as_str().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 8, "{text}");
        assert_eq!(lines[0], "session: fox   version: 1");
        assert_eq!(
            lines[1],
            format!(
                "image: {}  (96x64, {} KB PNG)",
                published.display(),
                saved.bytes.len().div_ceil(1024)
            )
        );
        assert_eq!(
            lines[2],
            "the preview is a 96px JPEG; the file is the full-resolution original"
        );
        assert_eq!(
            lines[3],
            format!("codex prompt: {}", serde_json::to_string(prompt).unwrap())
        );
        assert_eq!(
            lines[4],
            r#"codex note: "Created your fox." (Codex's closing line, untrusted)"#
        );
        assert!(
            lines[5].starts_with("took ")
                && lines[5].ends_with(" s; Codex agent usage: weekly 44%"),
            "{}",
            lines[5]
        );
        assert_eq!(
            lines[6],
            "output files are scratch and expire after 7 days idle; move keepers into the project"
        );
        assert_eq!(lines[7], SHOW_USER_LINE);
        assert!(!text.contains("warning:"), "{text}");

        assert_eq!(
            wait_sent(&seen, "thread/unsubscribe", 1),
            vec![json!({"threadId": THREAD_ID})]
        );
        assert!(FakeCodex::sent(&seen, "turn/interrupt").is_empty());
        assert!(f.running().is_empty(), "the session was not freed");
    }

    #[test]
    fn images_go_to_the_argument_else_the_flag_else_the_projects_generated_images() {
        let saved = saved_png();
        let f = fixture(codex(image_turn(&saved.path, "p")));
        let ok = |result: Value| assert_eq!(result["isError"], false, "{result}");

        ok(generate(&f, json!({"prompt": "p", "session": "a"})));
        assert!(f.dir.join("generated-images").join("a-v1.png").is_file());
        // A relative argument resolves against the project.
        ok(generate(
            &f,
            json!({"prompt": "p", "session": "b", "output_dir": "art/out"}),
        ));
        assert!(f.dir.join("art").join("out").join("b-v1.png").is_file());
        // An automatic name names the file.
        let text = text_of(&generate(&f, json!({"prompt": "p"})));
        let session = text
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("session: "))
            .and_then(|l| l.split_whitespace().next())
            .unwrap();
        assert!(session.starts_with("img-"), "{text}");
        assert!(f
            .dir
            .join("generated-images")
            .join(format!("{session}-v1.png"))
            .is_file());

        let g = fixture_with(
            codex(image_turn(&saved.path, "p")),
            &["--output-dir", "flagged"],
            |_, _| {},
        );
        ok(generate(&g, json!({"prompt": "p", "session": "c"})));
        assert!(g.dir.join("flagged").join("c-v1.png").is_file());
        ok(generate(
            &g,
            json!({"prompt": "p", "session": "d", "output_dir": "arg"}),
        ));
        assert!(g.dir.join("arg").join("d-v1.png").is_file());
        assert!(!g.dir.join("generated-images").exists());
    }

    #[test]
    fn an_output_folder_that_cannot_be_created_is_a_bad_request_before_codex_starts() {
        let f = fixture(codex(vec![]));
        let file = f.dir.join("a-file");
        std::fs::write(&file, b"not a folder").unwrap();
        let (is_error, text) = result_text(&generate(
            &f,
            json!({"prompt": "p", "output_dir": file.join("below")}),
        ));
        assert!(is_error);
        assert!(
            text.starts_with("REQUEST REJECTED\ncode: BAD_REQUEST"),
            "{text}"
        );
        assert!(text.contains(&file.display().to_string()), "{text}");
        assert!(text.contains("nothing was spent"), "{text}");
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "Codex was started");
    }

    #[test]
    fn a_taken_file_name_bumps_the_version_and_the_existing_file_is_untouched() {
        let saved = saved_png();
        let f = fixture(codex(image_turn(&saved.path, "p")));
        let dir = f.dir.join("generated-images");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fox-v1.png"), b"someone else's").unwrap();
        let text = text_of(&generate(&f, json!({"prompt": "p", "session": "fox"})));
        assert!(text.starts_with("session: fox   version: 2\n"), "{text}");
        assert_eq!(
            std::fs::read(dir.join("fox-v1.png")).unwrap(),
            b"someone else's"
        );
        assert_eq!(std::fs::read(dir.join("fox-v2.png")).unwrap(), saved.bytes);
        assert_eq!(listing(&dir), vec!["fox-v1.png", "fox-v2.png"]);
        // The record carries on from the version actually used, and lists only our file.
        let record = f.record("fox").unwrap();
        assert_eq!(record.next_version, 3);
        assert_eq!(
            record.outputs,
            vec![Output {
                version: 2,
                path: dir.join("fox-v2.png"),
                bytes: saved.bytes.len() as u64
            }]
        );
    }

    #[test]
    fn the_session_record_is_written_as_the_first_image_completes_not_at_turn_end() {
        let saved = saved_png();
        // The fake checks the store between the image and the end of the turn. The store's path
        // is known only once the fixture exists, so it is handed over through this.
        let store: Arc<OnceLock<Store>> = Arc::default();
        let recorded_mid_turn = Arc::new(AtomicBool::new(false));
        let (probe, flag) = (Arc::clone(&store), Arc::clone(&recorded_mid_turn));
        let fake = codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            Step::Send(image_completed("exec-1", "p", "", Some(&saved.path))),
            Step::Run(Arc::new(move || {
                let store = probe.get().expect("the store was handed over");
                let deadline = Instant::now() + WAIT;
                while Instant::now() < deadline {
                    if store.read().is_ok_and(|f| f.get("fox").is_some()) {
                        flag.store(true, Ordering::SeqCst);
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            })),
            Step::Send(agent_message("Created your fox.")),
            Step::Send(turn_completed("completed", Value::Null)),
        ]);
        let f = fixture(fake);
        store.set(f.store().clone()).unwrap();
        let result = generate(&f, json!({"prompt": "p", "session": "Fox"}));
        assert_eq!(result["isError"], false, "{result}");
        assert!(
            recorded_mid_turn.load(Ordering::SeqCst),
            "the record was not there before the turn ended"
        );

        let record = f.record("FOX").unwrap();
        let published = f.dir.join("generated-images").join("Fox-v1.png");
        assert_eq!(record.name, "Fox");
        assert_eq!(record.thread_id, THREAD_ID);
        // The home the child reported in its handshake, and the output folder this call used.
        assert_eq!(record.codex_home, PathBuf::from(r"C:\Users\someone\.codex"));
        assert_eq!(record.model, "gpt-6-astra");
        assert_eq!(record.output_dir, f.dir.join("generated-images"));
        assert_eq!(record.turns, 1);
        assert_eq!(
            record.last_output_path.as_deref(),
            Some(published.as_path())
        );
        assert_eq!(
            record.last_saved_path.as_deref(),
            Some(saved.path.as_path())
        );
        assert_eq!(record.last_output_bytes, Some(saved.bytes.len() as u64));
        assert_eq!(record.next_version, 2);
        assert_eq!(record.outputs.len(), 1);
        assert!(record.created > 0 && record.created == record.updated);

        // status lists it.
        let status = text_of(
            &f.app
                .call_tool(STATUS, &json!({}), &CallContext::detached()),
        );
        assert!(status.contains("sessions in this project: 1\n"), "{status}");
        assert!(
            status.contains(&format!(
                "\n  Fox: 1 turn, latest {}, updated ",
                published.display()
            )),
            "{status}"
        );
        assert!(status.contains("(under a minute ago)"), "{status}");
    }

    #[test]
    fn a_turn_that_completes_no_image_writes_no_record_and_leaves_the_name_free() {
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            Step::Send(image_failed("exec-1", Value::Null)),
            Step::Send(turn_completed("completed", Value::Null)),
        ]));
        assert_eq!(
            code_of(&generate(&f, json!({"prompt": "p", "session": "fox"}))),
            "IMAGE_FAILED"
        );
        assert!(f.record("fox").is_none());
        assert!(!f.store().path().exists(), "the store was written");
        // The name is still free: the next try is not SESSION_EXISTS.
        assert_eq!(
            code_of(&generate(&f, json!({"prompt": "p", "session": "fox"}))),
            "IMAGE_FAILED"
        );
    }

    #[test]
    fn an_existing_session_name_is_refused_whatever_its_case_before_codex_starts() {
        let f = fixture(codex(vec![]));
        let writer = SessionWriter::new(
            f.store().clone(),
            "Fox",
            Some(crate::session::NewSession {
                codex_home: PathBuf::from(r"C:\Users\someone\.codex"),
                model: "gpt-6-astra".to_string(),
                output_dir: f.dir.join("generated-images"),
            }),
        );
        writer
            .image_completed(
                THREAD_ID,
                &ImageOutcome {
                    version: None,
                    output_path: None,
                    saved_path: None,
                    bytes: None,
                },
            )
            .unwrap();
        let result = generate(&f, json!({"prompt": "p", "session": "FOX"}));
        assert_eq!(code_of(&result), "SESSION_EXISTS");
        let text = text_of(&result);
        assert!(text.starts_with("REQUEST REJECTED\n"), "{text}");
        assert!(text.contains("'FOX' already exists"), "{text}");
        assert!(text.contains("(as 'Fox')"), "{text}");
        assert!(
            text.contains("codex_imagegen_refine with session 'Fox'"),
            "{text}"
        );
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "Codex was started");
        assert!(f.running().is_empty(), "the session was left claimed");
    }

    #[test]
    fn a_session_leased_by_another_process_is_busy_before_codex_starts() {
        let saved = saved_png();
        let f = fixture(codex(image_turn(&saved.path, "p")));
        // Another handle on the same lease file, as another codex-imagegen process has.
        let elsewhere = Store::new(&f.state_dir()).try_lease("fox").unwrap();
        assert!(elsewhere.is_some());
        let result = generate(&f, json!({"prompt": "p", "session": "FOX"}));
        assert_eq!(code_of(&result), "SESSION_BUSY");
        assert!(
            text_of(&result).contains("another codex-imagegen process"),
            "{}",
            text_of(&result)
        );
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "Codex was started");
        assert!(f.running().is_empty(), "the session was left claimed");
        // Released there, it goes ahead here; and this call's lease is released when it returns.
        drop(elsewhere);
        let result = generate(&f, json!({"prompt": "p", "session": "FOX"}));
        assert_eq!(result["isError"], false, "{result}");
        assert!(f.store().try_lease("fox").unwrap().is_some());
    }

    #[test]
    fn a_session_store_that_cannot_be_read_stops_generate_before_codex_starts_but_not_status() {
        let f = fixture(codex(vec![]));
        let path = f.store().path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        for args in [
            json!({"prompt": "p", "session": "fox"}),
            json!({"prompt": "p"}),
        ] {
            let result = generate(&f, args);
            assert_eq!(code_of(&result), "STORE_CORRUPT");
            let text = text_of(&result);
            assert!(text.contains("=== ACTION REQUIRED ==="), "{text}");
            assert!(text.contains(&path.display().to_string()), "{text}");
        }
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "Codex was started");
        assert!(f.running().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");

        let (is_error, status) = result_text(&f.app.call_tool(
            STATUS,
            &json!({}),
            &CallContext::detached(),
        ));
        assert!(!is_error, "{status}");
        assert!(
            status.contains("sessions in this project: unknown -- the session store"),
            "{status}"
        );
        assert!(status.contains("(STORE_CORRUPT)"), "{status}");
        assert!(status.contains("image generation: available"), "{status}");
    }

    #[test]
    fn a_session_record_that_cannot_be_written_is_a_warning_on_the_success() {
        let saved = saved_png();
        let store: Arc<OnceLock<Store>> = Arc::default();
        let probe = Arc::clone(&store);
        let fake = codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            // After the claim read the store: a folder now stands where the store goes.
            Step::Run(Arc::new(move || {
                std::fs::create_dir_all(probe.get().unwrap().path()).unwrap();
            })),
            Step::Send(image_completed("exec-1", "p", "", Some(&saved.path))),
            Step::Send(turn_completed("completed", Value::Null)),
        ]);
        let f = fixture(fake);
        store.set(f.store().clone()).unwrap();
        let result = generate(&f, json!({"prompt": "p", "session": "fox"}));
        assert_eq!(result["isError"], false, "{result}");
        let text = text_of(&result);
        assert!(text.starts_with("session: fox   version: 1\n"), "{text}");
        assert!(
            text.contains("\nwarning: the session record could not be updated ("),
            "{text}"
        );
        assert!(
            text.contains("so codex_imagegen_refine may not find this session"),
            "{text}"
        );
        assert!(f.dir.join("generated-images").join("fox-v1.png").is_file());
    }

    #[test]
    fn a_revised_prompt_that_differs_from_the_one_sent_is_a_warning() {
        let saved = saved_png();
        let f = fixture(codex(image_turn(&saved.path, "a fox, highly detailed, 8k")));
        let text = text_of(&generate(&f, json!({"prompt": "a fox", "session": "s"})));
        assert!(
            text.contains(r#"codex prompt: "a fox, highly detailed, 8k""#),
            "{text}"
        );
        assert!(
            text.contains("\nwarning: codex prompt differs from the prompt sent"),
            "{text}"
        );
    }

    #[test]
    fn a_failed_image_in_a_completed_turn_is_image_failed_with_the_note_as_untrusted_detail() {
        let fake = codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            Step::Send(image_failed("exec-1", Value::Null)),
            Step::Send(agent_message("The image service refused:\u{202e} policy")),
            Step::Send(turn_completed("completed", Value::Null)),
        ]);
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let result = generate(&f, json!({"prompt": "p", "session": "s"}));
        assert_eq!(code_of(&result), "IMAGE_FAILED");
        let text = text_of(&result);
        assert!(text.contains("=== ACTION REQUIRED ==="), "{text}");
        assert!(text.contains("untrusted model text"), "{text}");
        assert!(
            text.contains(r#""The image service refused: policy""#),
            "{text}"
        );
        assert!(listing(&f.dir.join("generated-images")).is_empty());
        wait_sent(&seen, "thread/unsubscribe", 1);
    }

    #[test]
    fn an_exhausted_image_quota_is_rate_limited_with_its_reset_time_or_unknown() {
        for (resets_at, expected) in [
            (json!(1790710629), "It resets at 2026-09-"),
            (Value::Null, "reset time unknown"),
        ] {
            let f = fixture(codex(vec![
                Step::Send(turn_started()),
                Step::Send(image_started("exec-1")),
                Step::Send(image_failed(
                    "exec-1",
                    json!({"type": "usageLimitExceeded", "limitId": "image_gen",
                           "resetsAt": resets_at}),
                )),
                Step::Send(turn_completed("completed", Value::Null)),
            ]));
            let result = generate(&f, json!({"prompt": "p"}));
            assert_eq!(code_of(&result), "RATE_LIMITED");
            assert!(text_of(&result).contains(expected), "{}", text_of(&result));
        }
    }

    #[test]
    fn a_completed_turn_with_no_image_item_is_no_image_with_the_untrusted_note() {
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(agent_message(
                "Thinking.\nI could not read the reference image.",
            )),
            Step::Send(turn_completed("completed", Value::Null)),
        ]));
        let result = generate(&f, json!({"prompt": "p"}));
        assert_eq!(code_of(&result), "NO_IMAGE");
        let text = text_of(&result);
        assert!(
            text.contains(r#""I could not read the reference image.""#),
            "{text}"
        );
        assert!(text.contains("untrusted model text"), "{text}");
    }

    #[test]
    fn a_failed_turn_is_classified_by_its_codex_error_info_alone() {
        for (info, code) in [
            (json!("usageLimitExceeded"), "RATE_LIMITED"),
            (json!("cyberPolicy"), "CONTENT_REFUSED"),
            (json!("contextWindowExceeded"), "SESSION_NOT_RESUMABLE"),
            (
                json!({"httpConnectionFailed": {"httpStatusCode": 502}}),
                "UPSTREAM_ERROR",
            ),
            (json!("sandboxError"), "IMAGE_FAILED"),
            (Value::Null, "IMAGE_FAILED"),
        ] {
            let f = fixture(codex(vec![
                Step::Send(turn_started()),
                // Model text that would mislead a classifier reading it.
                Step::Send(agent_message("usage limit exceeded; unauthorized")),
                Step::Send(turn_completed("failed", turn_error(info.clone()))),
            ]));
            let result = generate(&f, json!({"prompt": "p"}));
            assert_eq!(code_of(&result), code, "{info}");
        }
    }

    #[test]
    fn an_image_that_completed_before_the_turn_failed_or_was_interrupted_is_a_success() {
        let saved = saved_png();
        for (status, error, warning) in [
            (
                "failed",
                turn_error(json!("serverOverloaded")),
                "warning: the turn failed after the image finished (RATE_LIMITED:",
            ),
            (
                "interrupted",
                Value::Null,
                "warning: Codex interrupted the turn after the image finished",
            ),
        ] {
            let f = fixture(codex(vec![
                Step::Send(turn_started()),
                Step::Send(image_started("exec-1")),
                Step::Send(image_completed("exec-1", "p", "", Some(&saved.path))),
                Step::Send(turn_completed(status, error)),
            ]));
            let result = generate(&f, json!({"prompt": "p", "session": "s"}));
            assert_eq!(result["isError"], false, "{result}");
            let text = text_of(&result);
            assert!(text.contains(warning), "{text}");
            assert!(f.dir.join("generated-images").join("s-v1.png").is_file());
        }
    }

    #[test]
    fn a_copy_that_fails_after_the_precheck_is_a_success_pointing_at_codexs_copy() {
        let saved = saved_png();
        let out = temp_dir("turn-out");
        let target = out.join("images");
        let doomed = target.clone();
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            // The folder passed the pre-check, then disappears before the image arrives.
            Step::Run(Arc::new(move || {
                let _ = std::fs::remove_dir_all(&doomed);
            })),
            Step::Send(image_completed("exec-1", "p", "", Some(&saved.path))),
            Step::Send(turn_completed("completed", Value::Null)),
        ]));
        let result = generate(
            &f,
            json!({"prompt": "p", "session": "s", "output_dir": target}),
        );
        assert_eq!(result["isError"], false, "{result}");
        let text = text_of(&result);
        assert!(
            text.starts_with("session: s   version: none published"),
            "{text}"
        );
        assert!(
            text.contains(&format!("image: {}  (96x64", saved.path.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "warning: could not copy the image into {}",
                target.display()
            )),
            "{text}"
        );
        // The preview was built from the bytes all the same.
        assert_eq!(result["content"][0]["type"], "image");
        // Recorded all the same, with Codex's copy as the latest image and no output of ours.
        let record = f.record("s").unwrap();
        assert_eq!(record.last_output_path, None);
        assert_eq!(
            record.last_saved_path.as_deref(),
            Some(saved.path.as_path())
        );
        assert_eq!(record.last_output_bytes, Some(saved.bytes.len() as u64));
        assert!(record.outputs.is_empty());
        assert_eq!(record.next_version, 1);
    }

    #[test]
    fn a_completed_image_with_no_saved_path_is_recovered_from_its_base64() {
        let bytes = photo_like_png(80, 60);
        let base64 = preview::base64_encode(&bytes);
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            Step::Send(image_completed("exec-1", "p", &base64, None)),
            Step::Send(turn_completed("completed", Value::Null)),
        ]));
        let result = generate(&f, json!({"prompt": "p", "session": "b64"}));
        assert_eq!(result["isError"], false, "{result}");
        let published = f.dir.join("generated-images").join("b64-v1.png");
        assert_eq!(std::fs::read(published).unwrap(), bytes);
        assert!(
            !text_of(&result).contains("warning:"),
            "{}",
            text_of(&result)
        );
    }

    #[test]
    fn a_preview_that_cannot_be_built_is_a_warning_and_the_file_is_still_copied() {
        let saved = saved_file(b"\x89PNG but not really an image".to_vec());
        let f = fixture(codex(image_turn(&saved.path, "p")));
        let result = generate(&f, json!({"prompt": "p", "session": "s"}));
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(
            result["content"].as_array().unwrap().len(),
            1,
            "no image block"
        );
        let text = text_of(&result);
        assert!(
            text.contains("warning: no preview could be built"),
            "{text}"
        );
        assert!(!text.contains("the preview is a"), "{text}");
        let published = f.dir.join("generated-images").join("s-v1.png");
        assert_eq!(std::fs::read(published).unwrap(), saved.bytes);
    }

    #[test]
    fn several_images_in_one_turn_each_get_a_version_and_a_failed_one_is_a_warning() {
        let saved = saved_png();
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_completed("exec-1", "p", "", Some(&saved.path))),
            Step::Send(image_failed("exec-2", Value::Null)),
            Step::Send(image_completed("exec-3", "p", "", Some(&saved.path))),
            Step::Send(turn_completed("completed", Value::Null)),
        ]));
        let result = generate(&f, json!({"prompt": "p", "session": "s"}));
        assert_eq!(result["isError"], false, "{result}");
        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 3, "two previews and the text");
        let text = text_of(&result);
        assert!(text.starts_with("session: s   versions: 1, 2\n"), "{text}");
        assert!(text.contains("\nimage 1 of 2: "), "{text}");
        assert!(text.contains("\nimage 2 of 2: "), "{text}");
        assert!(
            text.contains("warning: Codex made 2 images in this turn"),
            "{text}"
        );
        assert!(
            text.contains("warning: another image call in this turn failed"),
            "{text}"
        );
        // One turn, two outputs, in stream order.
        let record = f.record("s").unwrap();
        assert_eq!(record.turns, 1);
        let versions: Vec<u32> = record.outputs.iter().map(|o| o.version).collect();
        assert_eq!(versions, vec![1, 2]);
        assert_eq!(record.next_version, 3);
        assert_eq!(
            record.last_output_path,
            Some(f.dir.join("generated-images").join("s-v2.png"))
        );
    }

    #[test]
    fn a_cancel_interrupts_the_turn_from_the_hook_at_once_and_the_call_returns_cancelled() {
        let fake = codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
        ]);
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let cancel = Arc::new(RequestCancel::new());
        let ctx = CallContext::with_cancel(Arc::clone(&cancel));
        let result = std::thread::scope(|s| {
            let call = s.spawn(|| {
                f.app
                    .call_tool(GENERATE, &json!({"prompt": "p", "session": "fox"}), &ctx)
            });
            wait_until("the image call to start", || {
                f.running()
                    .first()
                    .is_some_and(|t| t.phase == "generating image")
            });
            // What the MCP reader thread does on notifications/cancelled: it must not wait on
            // Codex, since every other message queues behind it.
            let started = Instant::now();
            assert!(cancel.cancel(), "no cancel hook was installed");
            assert!(started.elapsed() < Duration::from_millis(200));
            assert_eq!(
                wait_sent(&seen, "turn/interrupt", 1),
                vec![json!({"threadId": THREAD_ID, "turnId": TURN_ID})]
            );
            call.join().unwrap()
        });
        assert_eq!(code_of(&result), "CANCELLED");
        wait_sent(&seen, "thread/unsubscribe", 1);
        assert_eq!(
            FakeCodex::sent(&seen, "turn/interrupt").len(),
            1,
            "sent twice"
        );
        assert!(f.running().is_empty());
    }

    #[test]
    fn a_turn_that_outlives_the_budget_is_interrupted_and_times_out() {
        let fake = codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
        ]);
        let seen = Arc::clone(&fake.seen);
        let f = fixture_with(fake, &[], |cfg, _| cfg.timeout = Duration::from_secs(1));
        let result = generate(&f, json!({"prompt": "p"}));
        assert_eq!(code_of(&result), "TIMEOUT");
        assert_eq!(
            FakeCodex::sent(&seen, "turn/interrupt"),
            vec![json!({"threadId": THREAD_ID, "turnId": TURN_ID})]
        );
        wait_sent(&seen, "thread/unsubscribe", 1);
        assert!(f.running().is_empty());
    }

    #[test]
    fn a_turn_start_reply_that_comes_after_the_call_gave_up_gets_its_turn_interrupted() {
        let fake = codex_with(TurnScript {
            answer_turn_start: false,
            steps: vec![
                Step::Sleep(Duration::from_millis(1500)),
                Step::AnswerTurnStart,
                Step::Send(turn_started()),
                Step::Send(image_started("exec-1")),
            ],
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let f = fixture_with(fake, &[], |cfg, _| cfg.timeout = Duration::from_millis(700));
        let result = generate(&f, json!({"prompt": "p", "session": "late"}));
        assert_eq!(code_of(&result), "TIMEOUT");
        // Returned, but the session stays busy: the turn may be starting.
        let running = f.running();
        assert_eq!(running.len(), 1);
        assert!(running[0].interrupted);
        // The dropped reply is followed by turn/started, which names the turn.
        assert_eq!(
            wait_sent(&seen, "turn/interrupt", 1),
            vec![json!({"threadId": THREAD_ID, "turnId": TURN_ID})]
        );
        wait_sent(&seen, "thread/unsubscribe", 1);
        wait_until("the session to be freed", || f.running().is_empty());
    }

    #[test]
    fn an_unanswered_interrupt_keeps_the_session_busy_until_the_turn_completes() {
        let fake = codex_with(TurnScript {
            steps: vec![
                Step::Send(turn_started()),
                Step::Send(image_started("exec-1")),
                Step::Sleep(Duration::from_millis(2000)),
                Step::Send(turn_completed("interrupted", Value::Null)),
            ],
            on_interrupt: vec![],
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let f = fixture_with(fake, &[], |cfg, wait| {
            cfg.timeout = Duration::from_millis(500);
            *wait = Duration::from_millis(300);
        });
        let started = Instant::now();
        let result = generate(&f, json!({"prompt": "p", "session": "fox"}));
        assert_eq!(code_of(&result), "TIMEOUT");
        assert!(
            started.elapsed() < Duration::from_millis(1800),
            "it waited too long"
        );
        wait_sent(&seen, "turn/interrupt", 1);

        let again = generate(&f, json!({"prompt": "p", "session": "FOX"}));
        assert_eq!(code_of(&again), "SESSION_BUSY");
        assert!(
            text_of(&again).contains("interrupted"),
            "{}",
            text_of(&again)
        );
        // Unsubscribing now would also stop the turn/completed that frees the session.
        assert!(FakeCodex::sent(&seen, "thread/unsubscribe").is_empty());

        wait_sent(&seen, "thread/unsubscribe", 1);
        wait_until("the session to be freed", || f.running().is_empty());
    }

    #[test]
    fn a_turn_start_refused_by_codex_unsubscribes_and_fails() {
        let fake = codex_with(TurnScript {
            turn_start_error: Some(json!({"code": -32600, "message": "bad input"})),
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let result = generate(&f, json!({"prompt": "p"}));
        assert_eq!(code_of(&result), "APP_SERVER_FAILED");
        assert!(
            text_of(&result).contains("turn/start"),
            "{}",
            text_of(&result)
        );
        wait_sent(&seen, "thread/unsubscribe", 1);
        assert!(f.running().is_empty());
    }

    #[test]
    fn a_thread_start_that_trips_on_the_mcp_map_is_retried_once_with_a_fresh_one() {
        let saved = saved_png();
        let fake = codex_with(TurnScript {
            thread_start_errors: vec![json!({"code": -32603,
                "message": "invalid config: mcp_servers.gone: server not found"})],
            steps: image_turn(&saved.path, "p"),
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let result = generate(&f, json!({"prompt": "p"}));
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(FakeCodex::sent(&seen, "thread/start").len(), 2);
        // Preflight's read, then one per attempt.
        assert_eq!(FakeCodex::sent(&seen, "config/read").len(), 3);
    }

    fn canary(name: &str) -> Value {
        notification(
            "mcpServer/startupStatus/updated",
            json!({"name": name, "status": "starting", "error": null, "failureReason": null}),
        )
    }

    #[test]
    fn an_mcp_server_starting_mid_turn_interrupts_it_and_fails_as_an_isolation_breach() {
        // node_repl is in the disabled map: a disabled server emits nothing, so a report for it
        // means the map did not hold.
        let fake = codex(vec![
            Step::Send(turn_started()),
            Step::Send(canary("node_repl")),
        ]);
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let result = generate(&f, json!({"prompt": "p"}));
        assert_eq!(code_of(&result), "APP_SERVER_FAILED");
        assert!(
            text_of(&result).contains("isolation breach: MCP server node_repl started"),
            "{}",
            text_of(&result)
        );
        assert_eq!(
            wait_sent(&seen, "turn/interrupt", 1),
            vec![json!({"threadId": THREAD_ID, "turnId": TURN_ID})]
        );
        wait_sent(&seen, "thread/unsubscribe", 1);
    }

    #[test]
    fn an_mcp_server_reported_right_after_thread_start_is_caught_whatever_the_timing() {
        // Codex reports startups right behind the thread/start reply [verified: smoke log], which
        // can be before the call has attached its thread; the registry keeps it until then.
        for _ in 0..5 {
            let fake = codex_with(TurnScript {
                on_thread_start: vec![canary("rogue")],
                steps: vec![Step::Send(turn_started())],
                ..TurnScript::default()
            });
            let seen = Arc::clone(&fake.seen);
            let f = fixture(fake);
            let result = generate(&f, json!({"prompt": "p"}));
            assert_eq!(code_of(&result), "APP_SERVER_FAILED");
            assert!(
                text_of(&result).contains("isolation breach: MCP server rogue started"),
                "{}",
                text_of(&result)
            );
            // Caught before the turn, or the turn was interrupted.
            if !FakeCodex::sent(&seen, "turn/start").is_empty() {
                wait_sent(&seen, "turn/interrupt", 1);
            }
            wait_sent(&seen, "thread/unsubscribe", 1);
        }
    }

    #[test]
    fn a_server_request_during_a_turn_is_declined_and_the_turn_carries_on() {
        let saved = saved_png();
        let mut steps = image_turn(&saved.path, "p");
        steps.insert(
            2,
            Step::Send(
                json!({"id": "srv-1", "method": "item/commandExecution/requestApproval",
                              "params": {"threadId": THREAD_ID, "turnId": TURN_ID}}),
            ),
        );
        let fake = codex(steps);
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let result = generate(&f, json!({"prompt": "p"}));
        assert_eq!(result["isError"], false, "{result}");
        let answer = seen
            .lock()
            .unwrap()
            .iter()
            .find(|m| m["id"] == "srv-1")
            .cloned()
            .expect("the request was answered");
        assert_eq!(answer["error"]["code"], -32601);
        assert!(answer.get("result").is_none());
    }

    #[test]
    fn a_child_that_dies_mid_turn_fails_unless_an_image_had_completed() {
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            Step::Exit,
        ]));
        let result = generate(&f, json!({"prompt": "p"}));
        assert_eq!(code_of(&result), "APP_SERVER_FAILED");
        assert!(
            text_of(&result).contains("stopped during the turn"),
            "{}",
            text_of(&result)
        );
        assert!(f.running().is_empty());
        // The next call starts a fresh child.
        generate(&f, json!({"prompt": "p"}));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 2);

        let saved = saved_png();
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_completed("exec-1", "p", "", Some(&saved.path))),
            Step::Exit,
        ]));
        let result = generate(&f, json!({"prompt": "p", "session": "s"}));
        assert_eq!(result["isError"], false, "{result}");
        assert!(
            text_of(&result).contains("warning: the Codex app-server exited after the image"),
            "{}",
            text_of(&result)
        );
        assert!(f.dir.join("generated-images").join("s-v1.png").is_file());
    }

    #[test]
    fn an_image_still_unread_when_the_child_is_seen_to_exit_is_kept() {
        // The process is seen to exit while its last line, the image, is still in the pipe: the
        // call must wait for the reader to reach the end of the output, not give up at once.
        let saved = saved_png();
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
            Step::ProcessExits,
            Step::Sleep(EVENT_POLL * 3),
            Step::Send(image_completed("exec-1", "p", "", Some(&saved.path))),
            Step::Exit,
        ]));
        let result = generate(&f, json!({"prompt": "p", "session": "s"}));
        assert_eq!(result["isError"], false, "{result}");
        assert!(
            text_of(&result).contains("warning: the Codex app-server exited after the image"),
            "{}",
            text_of(&result)
        );
        assert!(f.dir.join("generated-images").join("s-v1.png").is_file());
    }

    #[test]
    fn images_routed_while_the_call_was_busy_are_kept_when_the_child_then_exits() {
        // A large first image keeps the call's thread busy with its copy and preview while the
        // reader routes the second one and reaches the end of the output.
        let big = saved_file(photo_like_png(2400, 2400));
        let small = saved_png();
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_completed("exec-1", "p", "", Some(&big.path))),
            Step::Send(image_completed("exec-2", "p", "", Some(&small.path))),
            Step::Exit,
        ]));
        let result = generate(&f, json!({"prompt": "p", "session": "s"}));
        assert_eq!(result["isError"], false, "{result}");
        let text = text_of(&result);
        assert_eq!(result["content"].as_array().unwrap().len(), 3, "{text}");
        assert!(text.starts_with("session: s   versions: 1, 2\n"), "{text}");
        let dir = f.dir.join("generated-images");
        assert_eq!(std::fs::read(dir.join("s-v1.png")).unwrap(), big.bytes);
        assert_eq!(std::fs::read(dir.join("s-v2.png")).unwrap(), small.bytes);
    }

    #[test]
    fn an_unauthorized_turn_is_auth_expired_and_the_child_is_replaced() {
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(turn_completed("failed", turn_error(json!("unauthorized")))),
        ]));
        let result = generate(&f, json!({"prompt": "p"}));
        assert_eq!(code_of(&result), "AUTH_EXPIRED");
        assert!(
            text_of(&result).contains("codex login"),
            "{}",
            text_of(&result)
        );
        assert!(!f.has_child(), "the child was kept");
        generate(&f, json!({"prompt": "p"}));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 2, "no fresh child");
    }

    #[test]
    fn a_busy_session_and_the_concurrency_cap_are_refused_and_status_shows_the_turn() {
        let saved = saved_png();
        let f = fixture_with(
            codex(vec![
                Step::Send(turn_started()),
                Step::Send(image_started("exec-1")),
                Step::Sleep(Duration::from_millis(1000)),
                Step::Send(image_completed("exec-1", "p", "", Some(&saved.path))),
                Step::Send(turn_completed("completed", Value::Null)),
            ]),
            &["--max-concurrent", "1"],
            |_, _| {},
        );
        std::thread::scope(|s| {
            let first = s.spawn(|| generate(&f, json!({"prompt": "p", "session": "fox"})));
            wait_until("the image call to start", || {
                f.running()
                    .first()
                    .is_some_and(|t| t.phase == "generating image")
            });
            assert_eq!(
                code_of(&generate(&f, json!({"prompt": "p", "session": "FOX"}))),
                "SESSION_BUSY"
            );
            assert_eq!(
                code_of(&generate(&f, json!({"prompt": "p", "session": "other"}))),
                "TOO_MANY_RUNNING"
            );
            let status = text_of(
                &f.app
                    .call_tool(STATUS, &json!({}), &CallContext::detached()),
            );
            assert!(
                status.contains("running turns: fox (generating image, "),
                "{status}"
            );
            assert_eq!(first.join().unwrap()["isError"], false);
        });
        assert_eq!(f.spawns.load(Ordering::SeqCst), 1);
        let status = text_of(
            &f.app
                .call_tool(STATUS, &json!({}), &CallContext::detached()),
        );
        assert!(status.contains("running turns: none"), "{status}");
    }

    #[test]
    fn shutdown_interrupts_a_running_turn() {
        let fake = codex(vec![
            Step::Send(turn_started()),
            Step::Send(image_started("exec-1")),
        ]);
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let result = std::thread::scope(|s| {
            let call = s.spawn(|| generate(&f, json!({"prompt": "p", "session": "fox"})));
            wait_until("the image call to start", || {
                f.running()
                    .first()
                    .is_some_and(|t| t.phase == "generating image")
            });
            f.app.begin_shutdown();
            call.join().unwrap()
        });
        assert_eq!(code_of(&result), "SERVER_SHUTTING_DOWN");
        assert_eq!(
            wait_sent(&seen, "turn/interrupt", 1),
            vec![json!({"threadId": THREAD_ID, "turnId": TURN_ID})]
        );
    }
}

/// Whole refine calls through the tool layer, against the same scripted fake Codex. No real Codex,
/// no quota: the fake answers `thread/resume` as codex-cli 0.156.0 did after a restart [verified:
/// smoke log], and replays a turn.
#[cfg(test)]
mod refine_tests {
    use super::generate_tests::{
        code_of, codex, codex_with, image_turn, listing, saved_png, text_of, wait_sent, wait_until,
        Saved,
    };
    use super::*;
    use crate::codex::testing::{
        agent_message, image_started, notification, turn_completed, turn_error, turn_started,
        FakeCodex, Step, TurnScript, CODEX_HOME, THREAD_ID, TURN_ID,
    };
    use crate::mcp::{CallContext, ToolHost};
    use crate::session::{NewSession, Output, Store};
    use crate::tools::testing::{fixture, fixture_with, result_text, Fixture};
    use crate::tools::{GENERATE, REFINE, STATUS};
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    const FEEDBACK: &str = "Make the \"sky\" warmer,\nkeep C:\\keeper \u{2014} caf\u{e9} as is.";

    fn refine(f: &Fixture, args: Value) -> Value {
        f.app.call_tool(REFINE, &args, &CallContext::detached())
    }

    fn status(f: &Fixture) -> String {
        result_text(
            &f.app
                .call_tool(STATUS, &json!({}), &CallContext::detached()),
        )
        .1
    }

    /// A session as generate leaves it: v1 published in `out`, Codex's own copy at `saved`.
    fn seed(f: &Fixture, name: &str, saved: &Saved, out: &Path) -> PathBuf {
        std::fs::create_dir_all(out).unwrap();
        let v1 = out.join(format!("{name}-v1.png"));
        std::fs::write(&v1, &saved.bytes).unwrap();
        SessionWriter::new(
            f.store().clone(),
            name,
            Some(NewSession {
                codex_home: PathBuf::from(CODEX_HOME),
                model: "gpt-6-astra".to_string(),
                output_dir: out.to_path_buf(),
            }),
        )
        .image_completed(
            THREAD_ID,
            &ImageOutcome {
                version: Some(1),
                output_path: Some(&v1),
                saved_path: Some(&saved.path),
                bytes: Some(saved.bytes.len() as u64),
            },
        )
        .unwrap();
        v1
    }

    fn set_updated(f: &Fixture, name: &str, updated: i64) {
        f.store()
            .update(|file| {
                file.sessions.get_mut(name).unwrap().updated = updated;
                Ok(())
            })
            .unwrap();
    }

    fn edit_text(target: &Path) -> String {
        format!(
            "<edit_request>\n{FEEDBACK}\n</edit_request>\n<edit_target>{}</edit_target>",
            target.display()
        )
    }

    fn resume_error(message: &str) -> Value {
        json!({"code": -32600, "message": message})
    }

    fn quoted(path: &Path) -> String {
        serde_json::to_string(&path.display().to_string()).unwrap()
    }

    #[test]
    fn a_refine_resumes_the_thread_names_the_edit_target_and_publishes_the_next_version() {
        let saved = saved_png();
        let fake = codex(image_turn(&saved.path, FEEDBACK));
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        // The session's own folder, not the default: refine uses the recorded one.
        let art = f.dir.join("art");
        let v1 = seed(&f, "Fox", &saved, &art);
        let before = f.record("fox").unwrap();

        let result = refine(&f, json!({"session": "FOX", "feedback": FEEDBACK}));
        assert_eq!(result["isError"], false, "{result}");

        assert!(FakeCodex::sent(&seen, "thread/start").is_empty());
        assert_eq!(
            FakeCodex::sent(&seen, "thread/resume"),
            vec![json!({
                "threadId": THREAD_ID,
                "excludeTurns": true,
                "model": "gpt-6-astra",
                "cwd": f.dir.join("state").join("work"),
                "sandbox": "read-only",
                "approvalPolicy": "never",
                "approvalsReviewer": "user",
                "developerInstructions": codex::DEVELOPER_INSTRUCTIONS,
                "config": {"mcp_servers": {"cua_repl": {"enabled": false},
                                           "node_repl": {"enabled": false}}},
            })]
        );
        // Preflight's read, then a fresh one for the resume's MCP-off map.
        assert_eq!(FakeCodex::sent(&seen, "config/read").len(), 2);
        // Codex's copy is the first choice of edit target.
        assert_eq!(
            FakeCodex::sent(&seen, "turn/start"),
            vec![json!({
                "threadId": THREAD_ID,
                "input": [{"type": "text", "text": edit_text(&saved.path), "text_elements": []}],
                "model": "gpt-6-astra",
                "effort": "low",
            })]
        );

        let text = text_of(&result);
        assert!(text.starts_with("session: Fox   version: 2\n"), "{text}");
        let v2 = art.join("Fox-v2.png");
        assert!(
            text.contains(&format!("\nimage: {}  (96x64", v2.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "\ncodex prompt: {}\n",
                serde_json::to_string(FEEDBACK).unwrap()
            )),
            "{text}"
        );
        assert!(!text.contains("warning:"), "{text}");
        assert_eq!(std::fs::read(&v2).unwrap(), saved.bytes);
        assert_eq!(listing(&art), vec!["Fox-v1.png", "Fox-v2.png"]);

        let record = f.record("fox").unwrap();
        assert_eq!(record.name, "Fox");
        assert_eq!(record.turns, 2);
        assert_eq!(record.next_version, 3);
        assert_eq!(record.last_output_path.as_deref(), Some(v2.as_path()));
        assert_eq!(
            record.output_dir, art,
            "a refine keeps the session's folder"
        );
        assert_eq!(record.created, before.created);
        assert!(record.updated >= before.updated);
        assert_eq!(
            record.outputs,
            vec![
                Output {
                    version: 1,
                    path: v1,
                    bytes: saved.bytes.len() as u64
                },
                Output {
                    version: 2,
                    path: v2,
                    bytes: saved.bytes.len() as u64
                },
            ]
        );
        assert_eq!(
            wait_sent(&seen, "thread/unsubscribe", 1),
            vec![json!({"threadId": THREAD_ID})]
        );
        assert!(f.running().is_empty());
        assert!(f.store().try_lease("fox").unwrap().is_some(), "the lease");
    }

    #[test]
    fn the_edit_target_falls_back_to_the_published_copy_and_the_versions_carry_on() {
        let seeded = saved_png();
        let turn_image = saved_png();
        let fake = codex(image_turn(&turn_image.path, FEEDBACK));
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let out = f.dir.join("generated-images");
        let v1 = seed(&f, "fox", &seeded, &out);
        // Codex's copy changed size (replaced), so ours is the target.
        std::fs::write(&seeded.path, b"replaced").unwrap();
        // Someone else's file holds the next name: the version is bumped past it.
        std::fs::write(out.join("fox-v2.png"), b"not ours").unwrap();

        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(result["isError"], false, "{result}");
        let sent = FakeCodex::sent(&seen, "turn/start");
        assert_eq!(sent[0]["input"][0]["text"], edit_text(&v1));
        assert!(
            text_of(&result).starts_with("session: fox   version: 3\n"),
            "{}",
            text_of(&result)
        );
        assert_eq!(std::fs::read(out.join("fox-v2.png")).unwrap(), b"not ours");
        let record = f.record("fox").unwrap();
        assert_eq!(record.next_version, 4);
        assert_eq!(record.outputs.len(), 2);
        assert_eq!(record.outputs[1].version, 3);
    }

    #[test]
    fn with_no_copy_of_the_latest_image_left_nothing_is_spent_and_a_survivor_is_named() {
        let saved = saved_png();
        let fake = codex(image_turn(&saved.path, FEEDBACK));
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let out = f.dir.join("generated-images");
        let v1 = seed(&f, "fox", &saved, &out);
        std::fs::remove_file(&saved.path).unwrap();
        std::fs::remove_file(&v1).unwrap();

        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "SESSION_NOT_RESUMABLE");
        let text = text_of(&result);
        assert!(text.starts_with("REQUEST REJECTED\n"), "{text}");
        assert!(text.contains("Nothing was spent"), "{text}");
        assert!(text.contains("no copy of its images"), "{text}");
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "Codex was started");
        assert!(FakeCodex::sent(&seen, "turn/start").is_empty());
        assert!(f.running().is_empty());

        // An older published version still there is offered for the new session.
        let older = out.join("fox-v0.png");
        std::fs::write(&older, &saved.bytes).unwrap();
        f.store()
            .update(|file| {
                file.sessions.get_mut("fox").unwrap().outputs.insert(
                    0,
                    Output {
                        version: 0,
                        path: older.clone(),
                        bytes: saved.bytes.len() as u64,
                    },
                );
                Ok(())
            })
            .unwrap();
        let text = text_of(&refine(&f, json!({"session": "fox", "feedback": "x"})));
        assert!(
            text.contains(&format!("reference_images: [{}]", quoted(&older))),
            "{text}"
        );
    }

    #[test]
    fn a_session_from_another_codex_home_is_not_resumable() {
        let saved = saved_png();
        let fake = codex(image_turn(&saved.path, FEEDBACK));
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        f.store()
            .update(|file| {
                file.sessions.get_mut("fox").unwrap().codex_home = PathBuf::from(r"D:\other-home");
                Ok(())
            })
            .unwrap();
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "SESSION_NOT_RESUMABLE");
        let text = text_of(&result);
        assert!(text.contains(r"D:\other-home"), "{text}");
        assert!(text.contains(CODEX_HOME), "{text}");
        assert!(FakeCodex::sent(&seen, "thread/resume").is_empty());
        assert!(FakeCodex::sent(&seen, "turn/start").is_empty());
        assert_eq!(f.record("fox").unwrap().turns, 1);
    }

    #[test]
    fn a_thread_that_is_closing_is_resumed_again_after_a_pause() {
        let saved = saved_png();
        let fake = codex_with(TurnScript {
            resume_errors: vec![resume_error(&format!(
                "thread {THREAD_ID} is closing; retry thread/resume after the thread is closed"
            ))],
            steps: image_turn(&saved.path, FEEDBACK),
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let started = Instant::now();
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(result["isError"], false, "{result}");
        assert!(started.elapsed() >= RESUME_RETRY);
        assert_eq!(FakeCodex::sent(&seen, "thread/resume").len(), 2);
        // A fresh map for each try.
        assert_eq!(FakeCodex::sent(&seen, "config/read").len(), 3);
    }

    #[test]
    fn a_thread_held_by_another_process_is_waited_for_then_open_elsewhere() {
        let saved = saved_png();
        let writer = resume_error(&format!("thread {THREAD_ID} already has an active writer"));
        // Released after two tries: the call goes ahead.
        let fake = codex_with(TurnScript {
            resume_errors: vec![writer.clone(), writer.clone()],
            steps: image_turn(&saved.path, FEEDBACK),
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let mut f = fixture(fake);
        f.set_writer_wait(Duration::from_secs(5));
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(FakeCodex::sent(&seen, "thread/resume").len(), 3);

        // Never released: SESSION_OPEN_ELSEWHERE once the wait is over, with nothing spent.
        let fake = codex_with(TurnScript {
            resume_errors: vec![writer; 50],
            steps: image_turn(&saved.path, FEEDBACK),
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let mut f = fixture(fake);
        f.set_writer_wait(Duration::from_millis(700));
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let started = Instant::now();
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        let took = started.elapsed();
        assert_eq!(code_of(&result), "SESSION_OPEN_ELSEWHERE");
        let text = text_of(&result);
        assert!(text.starts_with("REQUEST REJECTED\n"), "{text}");
        assert!(
            text.contains("another Claude Code window or in the Codex app"),
            "{text}"
        );
        assert!(took >= Duration::from_millis(700), "{took:?}");
        assert!(took < Duration::from_secs(3), "{took:?}");
        let tries = FakeCodex::sent(&seen, "thread/resume").len();
        assert!((2..10).contains(&tries), "{tries} tries: backoff");
        assert!(FakeCodex::sent(&seen, "turn/start").is_empty());
        assert_eq!(f.record("fox").unwrap().turns, 1);
        assert!(f.running().is_empty());
    }

    #[test]
    fn a_resume_abandoned_on_a_cancel_or_a_timeout_is_followed_by_an_unsubscribe() {
        let saved = saved_png();
        let slow_resume = |delay_ms: u64| {
            codex_with(TurnScript {
                resume_delay: Duration::from_millis(delay_ms),
                steps: image_turn(&saved.path, FEEDBACK),
                ..TurnScript::default()
            })
        };
        // Codex answers the resume after the call stopped waiting, and this child is then
        // subscribed: without the unsubscribe the thread would hold its writer lock for as long as
        // the child lives.
        let check = |seen: &Mutex<Vec<Value>>, f: &Fixture| {
            assert_eq!(
                wait_sent(seen, "thread/unsubscribe", 1),
                vec![json!({"threadId": THREAD_ID})]
            );
            let methods: Vec<String> = seen
                .lock()
                .unwrap()
                .iter()
                .filter_map(|m| m["method"].as_str().map(str::to_string))
                .collect();
            let at = |method: &str| methods.iter().position(|m| m == method).unwrap();
            assert!(
                at("thread/resume") < at("thread/unsubscribe"),
                "{methods:?}"
            );
            assert!(FakeCodex::sent(seen, "turn/start").is_empty());
            assert_eq!(f.record("fox").unwrap().turns, 1);
            assert!(f.running().is_empty());
            assert!(f.store().try_lease("fox").unwrap().is_some(), "the lease");
        };

        // Cancelled (Esc) while the resume is unanswered.
        let fake = slow_resume(600);
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let cancel = Arc::new(RequestCancel::new());
        let ctx = CallContext::with_cancel(Arc::clone(&cancel));
        let result = std::thread::scope(|s| {
            let call = s.spawn(|| {
                f.app.call_tool(
                    REFINE,
                    &json!({"session": "fox", "feedback": FEEDBACK}),
                    &ctx,
                )
            });
            wait_sent(&seen, "thread/resume", 1);
            cancel.cancel();
            call.join().unwrap()
        });
        assert_eq!(code_of(&result), "CANCELLED");
        check(&seen, &f);
        // The late reply changes nothing.
        std::thread::sleep(Duration::from_millis(800));
        assert!(FakeCodex::sent(&seen, "turn/start").is_empty());

        // Still unanswered when the call's budget runs out.
        let fake = slow_resume(2000);
        let seen = Arc::clone(&fake.seen);
        let f = fixture_with(fake, &[], |cfg, _| cfg.timeout = Duration::from_millis(800));
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "TIMEOUT");
        check(&seen, &f);
    }

    #[test]
    fn a_thread_codex_no_longer_has_is_not_resumable_and_names_the_edit_target() {
        let saved = saved_png();
        let fake = codex_with(TurnScript {
            resume_errors: vec![resume_error(&format!(
                "no rollout found for thread id {THREAD_ID}"
            ))],
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "SESSION_NOT_RESUMABLE");
        let text = text_of(&result);
        assert!(text.contains("no rollout found"), "{text}");
        assert!(
            text.contains(&format!("reference_images: [{}]", quoted(&saved.path))),
            "{text}"
        );
        assert_eq!(FakeCodex::sent(&seen, "thread/resume").len(), 1);
        assert!(FakeCodex::sent(&seen, "turn/start").is_empty());

        // Any other refusal is APP_SERVER_FAILED, with Codex's words. Some come after Codex has
        // subscribed this child, so it unsubscribes.
        let fake = codex_with(TurnScript {
            resume_errors: vec![resume_error("thread is archived")],
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "APP_SERVER_FAILED");
        assert!(text_of(&result).contains("thread is archived"));
        assert_eq!(
            wait_sent(&seen, "thread/unsubscribe", 1),
            vec![json!({"threadId": THREAD_ID})]
        );
    }

    #[test]
    fn a_session_out_of_room_is_not_resumable_with_the_new_session_remediation() {
        let saved = saved_png();
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(turn_completed(
                "failed",
                turn_error(json!("contextWindowExceeded")),
            )),
        ]));
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "SESSION_NOT_RESUMABLE");
        let text = text_of(&result);
        assert!(
            text.contains("codexErrorInfo contextWindowExceeded"),
            "{text}"
        );
        assert!(
            text.contains(&format!("reference_images: [{}]", quoted(&saved.path))),
            "{text}"
        );
        let record = f.record("fox").unwrap();
        assert_eq!((record.turns, record.next_version), (1, 2), "unchanged");
    }

    #[test]
    fn a_revised_prompt_that_is_not_the_feedback_is_a_warning() {
        let saved = saved_png();
        let f = fixture(codex(image_turn(&saved.path, "something else")));
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let text = text_of(&refine(&f, json!({"session": "fox", "feedback": FEEDBACK})));
        assert!(
            text.contains("\nwarning: codex prompt differs from the feedback sent"),
            "{text}"
        );
    }

    #[test]
    fn a_turn_without_an_image_on_refine_is_no_image_naming_the_references() {
        let saved = saved_png();
        let f = fixture(codex(vec![
            Step::Send(turn_started()),
            Step::Send(agent_message("I could not read the edit target.")),
            Step::Send(turn_completed("completed", Value::Null)),
        ]));
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "NO_IMAGE");
        assert!(
            text_of(&result).contains("Check that every reference image still exists"),
            "{}",
            text_of(&result)
        );
        assert_eq!(f.record("fox").unwrap().turns, 1, "no image, no turn");
    }

    #[test]
    fn an_mcp_server_starting_on_the_resumed_thread_is_an_isolation_breach() {
        let saved = saved_png();
        let fake = codex_with(TurnScript {
            on_thread_resume: vec![notification(
                "mcpServer/startupStatus/updated",
                json!({"name": "rogue", "status": "starting", "error": null}),
            )],
            steps: vec![
                Step::Send(turn_started()),
                Step::Send(image_started("exec-1")),
            ],
            ..TurnScript::default()
        });
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "APP_SERVER_FAILED");
        assert!(
            text_of(&result).contains("isolation breach: MCP server rogue started"),
            "{}",
            text_of(&result)
        );
        // Caught before the turn, or the turn was interrupted.
        if !FakeCodex::sent(&seen, "turn/start").is_empty() {
            assert_eq!(
                wait_sent(&seen, "turn/interrupt", 1),
                vec![json!({"threadId": THREAD_ID, "turnId": TURN_ID})]
            );
        }
        wait_sent(&seen, "thread/unsubscribe", 1);
    }

    #[test]
    fn refine_is_refused_before_codex_starts_on_a_lease_held_elsewhere_or_an_unreadable_store() {
        let saved = saved_png();
        let f = fixture(codex(image_turn(&saved.path, FEEDBACK)));
        seed(&f, "fox", &saved, &f.dir.join("generated-images"));
        let elsewhere = Store::new(&f.state_dir()).try_lease("FOX").unwrap();
        assert!(elsewhere.is_some());
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "SESSION_BUSY");
        drop(elsewhere);

        let path = f.store().path();
        std::fs::write(&path, b"{ not json").unwrap();
        let result = refine(&f, json!({"session": "fox", "feedback": FEEDBACK}));
        assert_eq!(code_of(&result), "STORE_CORRUPT");
        assert!(text_of(&result).contains("=== ACTION REQUIRED ==="));
        assert_eq!(f.spawns.load(Ordering::SeqCst), 0, "Codex was started");
        assert!(f.running().is_empty());
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
        // status still reports, tolerantly.
        assert!(status(&f).contains("(STORE_CORRUPT)"));
    }

    #[test]
    fn a_session_made_by_generate_is_refined_after_codex_restarts() {
        let saved = saved_png();
        let fake = codex(image_turn(&saved.path, "p"));
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let first = f.app.call_tool(
            GENERATE,
            &json!({"prompt": "p", "session": "fox"}),
            &CallContext::detached(),
        );
        assert_eq!(first["isError"], false, "{first}");
        assert!(status(&f).contains("fox: 1 turn"));
        // Codex goes away between the calls, as it does when the server restarts.
        f.kill_child();
        let result = refine(&f, json!({"session": "fox", "feedback": "p"}));
        assert_eq!(result["isError"], false, "{result}");
        assert_eq!(f.spawns.load(Ordering::SeqCst), 2, "a fresh Codex");
        assert_eq!(FakeCodex::sent(&seen, "thread/start").len(), 1);
        assert_eq!(FakeCodex::sent(&seen, "thread/resume").len(), 1);
        let text = text_of(&result);
        assert!(text.starts_with("session: fox   version: 2\n"), "{text}");
        assert!(status(&f).contains("fox: 2 turns, latest "));
    }

    #[test]
    fn an_output_dir_argument_applies_to_that_refine_only_and_references_follow_the_target() {
        let saved = saved_png();
        let fake = codex(image_turn(&saved.path, FEEDBACK));
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        let out = f.dir.join("generated-images");
        seed(&f, "fox", &saved, &out);
        let reference = f.dir.join("ref.png");
        std::fs::write(&reference, &saved.bytes).unwrap();
        let result = refine(
            &f,
            json!({"session": "fox", "feedback": FEEDBACK, "output_dir": "elsewhere",
                   "reference_images": ["ref.png"]}),
        );
        assert_eq!(result["isError"], false, "{result}");
        assert!(f.dir.join("elsewhere").join("fox-v2.png").is_file());
        assert_eq!(f.record("fox").unwrap().output_dir, out);
        let sent = FakeCodex::sent(&seen, "turn/start");
        assert_eq!(
            sent[0]["input"][0]["text"],
            format!(
                "{}\n<reference_images>\n{}\n</reference_images>",
                edit_text(&saved.path),
                reference.display()
            )
        );
    }

    #[test]
    fn automatic_expiry_runs_once_when_the_first_call_brings_codex_up() {
        let saved = saved_png();
        let fake = codex(vec![]);
        let seen = Arc::clone(&fake.seen);
        let f = fixture(fake);
        f.arm_expiry();
        let v1 = seed(&f, "old", &saved, &f.dir.join("generated-images"));
        seed(&f, "new", &saved, &f.dir.join("generated-images"));
        set_updated(&f, "old", 1_000);
        assert!(!status(&f).contains("ACTION REQUIRED"));
        assert_eq!(
            wait_sent(&seen, "thread/delete", 1),
            vec![json!({"threadId": THREAD_ID})]
        );
        wait_until("the expired session to go", || {
            f.record("old").is_none() && f.store().read().unwrap().last_cleanup.is_some()
        });
        assert!(!v1.exists());
        assert!(f.record("new").is_some());
        let report = status(&f);
        assert!(report.contains("removed 1 session, freed"), "{report}");

        // Once per process: another expired session waits for the next server.
        set_updated(&f, "new", 1_000);
        f.kill_child();
        status(&f);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(FakeCodex::sent(&seen, "thread/delete").len(), 1);
        assert!(f.record("new").is_some());
    }

    #[test]
    fn a_session_ttl_of_zero_turns_automatic_expiry_off() {
        let saved = saved_png();
        let fake = codex(vec![]);
        let seen = Arc::clone(&fake.seen);
        let f = fixture_with(fake, &["--session-ttl-days", "0"], |_, _| {});
        f.arm_expiry();
        seed(&f, "old", &saved, &f.dir.join("generated-images"));
        set_updated(&f, "old", 1_000);
        status(&f);
        std::thread::sleep(Duration::from_millis(300));
        assert!(FakeCodex::sent(&seen, "thread/delete").is_empty());
        assert!(f.record("old").is_some());
        assert!(f.store().read().unwrap().last_cleanup.is_none());
    }
}
