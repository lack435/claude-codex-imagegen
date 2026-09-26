//! MCP over stdio: newline-delimited JSON-RPC 2.0, as the server Claude Code talks to.
//!
//! Hand-rolled rather than pulled from a crate: the protocol surface a tools-only server needs is
//! a handful of methods, and keeping the dependency list short is what lets this ship as one small
//! executable (docs/design.md, "MCP server details").
//!
//! This module knows nothing about images. It owns the protocol loop, one thread per
//! `tools/call`, cancellation and progress; the tools themselves sit behind [`ToolHost`].
//!
//! stdout carries protocol traffic only. Anything diagnostic goes to stderr.

use std::collections::HashMap;
use std::io::BufRead;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::cancel::RequestCancel;
use crate::errors::{self, Failure};
use crate::jsonrpc::{self, Envelope, Kind, LineReader, ReadLine, Writer};

/// Protocol versions we can speak, newest first. For a tools-only server they are equivalent, so
/// a client's version is echoed whenever it is one of these. Anything else gets the newest, which
/// is what the MCP spec asks for ("SHOULD be the latest version the server supports") and what
/// keeps a newer client from being downgraded. Claude Code sends 2025-11-25.
pub const SUPPORTED_PROTOCOLS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

pub const SERVER_NAME: &str = "codex-imagegen";

/// How often a running call reports progress when nothing else happens. An image turn takes about
/// 40 s, so this shows the call is alive a handful of times without flooding the client; a phase
/// change is reported at once regardless.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

/// Claude Code shows progress messages cut to 200 characters (docs/design.md, "Timeouts and
/// progress"), so a message is built to fit rather than be cut mid-word by the client.
const MAX_PROGRESS_MESSAGE_CHARS: usize = 200;

/// The tool layer, as the protocol loop sees it.
///
/// A trait so the loop can be tested against a fake host, with no Codex anywhere near it.
pub trait ToolHost: Send + Sync {
    /// The `instructions` sent in the `initialize` result.
    fn instructions(&self) -> String;

    /// The `tools/list` entries.
    fn tool_definitions(&self) -> Vec<Value>;

    /// Run one tool call and return the MCP result object (`{content, isError}`). Called on the
    /// call's own thread. Failures are `isError` results, never panics or protocol errors: the
    /// calling model has to read the remediation, and JSON-RPC errors are not reliably shown to it.
    fn call_tool(&self, name: &str, args: &Value, ctx: &CallContext) -> Value;

    /// Stdin has closed. Refuse new work and release anything parked, so the calls still in
    /// flight can finish before the process exits. Called before those calls are joined.
    fn begin_shutdown(&self);
}

/// What a tool call gets besides its arguments: its cancellation state and its progress handle.
pub struct CallContext {
    cancel: Arc<RequestCancel>,
    progress: Progress,
}

impl CallContext {
    /// The request's cancellation state, for installing a hook that stops the work it started.
    pub fn cancel(&self) -> &Arc<RequestCancel> {
        &self.cancel
    }

    /// This call's progress: [`Progress::set_phase`] reports the phase it is in. A clone, so it
    /// can be moved to another thread.
    pub fn progress(&self) -> Progress {
        self.progress.clone()
    }

    /// A context for driving a tool outside the protocol loop: never cancelled, no progress.
    #[cfg(test)]
    pub fn detached() -> Self {
        Self::with_cancel(Arc::new(RequestCancel::new()))
    }

    /// A context whose cancellation a test controls.
    #[cfg(test)]
    pub fn with_cancel(cancel: Arc<RequestCancel>) -> Self {
        Self {
            cancel,
            progress: Progress::default(),
        }
    }
}

/// The phase-reporting side of a call's progress. Inert when the client asked for no progress.
#[derive(Clone, Default)]
pub struct Progress {
    shared: Option<Arc<ProgressShared>>,
}

impl Progress {
    /// Set the phase shown to the user, such as "generating image". A change is reported at once;
    /// setting the phase it already has changes nothing. Ignored once the call has finished.
    pub fn set_phase(&self, phase: &str) {
        let Some(shared) = &self.shared else {
            return;
        };
        let mut state = shared.lock();
        if state.done || state.phase.as_deref() == Some(phase) {
            return;
        }
        state.phase = Some(phase.to_string());
        state.changed = true;
        shared.wake.notify_all();
    }
}

/// Knobs a test needs to turn. The server always runs with the defaults.
#[derive(Clone, Debug)]
pub struct ServeOptions {
    pub progress_interval: Duration,
    pub max_line_bytes: usize,
}

impl Default for ServeOptions {
    fn default() -> Self {
        Self {
            progress_interval: PROGRESS_INTERVAL,
            max_line_bytes: jsonrpc::MCP_MAX_LINE_BYTES,
        }
    }
}

/// `tools/call` handlers still running, keyed by the client's request id, so that
/// `notifications/cancelled` can reach the right one. Every other method is answered on the reader
/// thread and has finished before the next line is even read.
type Pending = Arc<Mutex<HashMap<String, Arc<RequestCancel>>>>;

/// Serve MCP on this process's stdin and stdout until stdin closes.
pub fn serve(host: Arc<dyn ToolHost>) {
    let writer: Writer = Arc::new(Mutex::new(std::io::stdout()));
    eprintln!("{}: serving MCP on stdio", crate::version_line());
    serve_on(
        host,
        std::io::stdin().lock(),
        writer,
        &ServeOptions::default(),
    );
    eprintln!("codex-imagegen: stdin closed, shut down");
}

/// The protocol loop, over any reader and writer so tests can drive it end to end.
pub fn serve_on<R: BufRead>(
    host: Arc<dyn ToolHost>,
    reader: R,
    writer: Writer,
    options: &ServeOptions,
) {
    let mut lines = LineReader::new(reader, options.max_line_bytes);
    // Handler threads are joined at shutdown: exiting while one is mid-flight would drop a
    // response the client is still waiting for.
    let mut in_flight: Vec<JoinHandle<()>> = Vec::new();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    loop {
        let line = match lines.next_line() {
            Ok(ReadLine::Line(line)) => line,
            Ok(ReadLine::Eof) => break,
            Ok(ReadLine::TooLong) => {
                eprintln!(
                    "codex-imagegen: discarded a message longer than {} bytes",
                    options.max_line_bytes
                );
                send_error(
                    &writer,
                    &Value::Null,
                    -32700,
                    format!(
                        "parse error: message longer than the {}-byte limit",
                        options.max_line_bytes
                    ),
                );
                continue;
            }
            Ok(ReadLine::NotUtf8) => {
                eprintln!("codex-imagegen: discarded a message that is not UTF-8");
                send_error(
                    &writer,
                    &Value::Null,
                    -32700,
                    "parse error: message is not UTF-8".into(),
                );
                continue;
            }
            Err(e) => {
                eprintln!("codex-imagegen: stdin read error: {e}");
                break;
            }
        };
        // Some clients prefix their first write with a UTF-8 BOM, which is not valid JSON.
        let line = jsonrpc::strip_bom(line).trim();
        if line.is_empty() {
            continue;
        }

        let envelope = match Envelope::parse(line) {
            Ok(envelope) => envelope,
            Err(e) => {
                let code = jsonrpc::parse_failure_code(line);
                let kind = if code == -32700 {
                    "parse error"
                } else {
                    "invalid request"
                };
                eprintln!("codex-imagegen: rejected a message ({kind}): {e}");
                send_error(&writer, &Value::Null, code, format!("{kind}: {e}"));
                continue;
            }
        };

        match envelope.kind() {
            // Notifications are never answered. Cancellation is the only one we act on.
            Kind::Notification { method } => {
                if method == "notifications/cancelled" {
                    handle_cancellation(&pending, &envelope.params_value());
                }
            }
            Kind::Request { method, .. } => {
                let id = envelope.id_value().unwrap_or(Value::Null);
                let params = envelope.params_value();
                if method == "tools/call" {
                    // A tool call can take minutes, so it gets its own thread, and pings and
                    // cancellations keep flowing on this one. Finished handles are reaped first,
                    // so the vector does not grow by one handle per call for the process's life.
                    in_flight.retain(|h| !h.is_finished());
                    let builder = std::thread::Builder::new().name("tools-call".to_string());
                    if let Some(handle) = start_tool_call(
                        builder,
                        &host,
                        &writer,
                        &pending,
                        id,
                        params,
                        options.progress_interval,
                    ) {
                        in_flight.push(handle);
                    }
                } else {
                    jsonrpc::send(&writer, &handle_sync(host.as_ref(), method, &params, &id));
                }
            }
            // We send the client no requests, so there is nothing a response could answer.
            Kind::Response { .. } => {
                eprintln!(
                    "codex-imagegen: ignoring a response to a request this server never sent"
                );
            }
            Kind::Invalid => {
                eprintln!("codex-imagegen: rejected a message with neither a method nor an id");
                send_error(
                    &writer,
                    &Value::Null,
                    -32600,
                    "invalid request: no method".into(),
                );
            }
        }
    }

    drain_in_flight(host.as_ref(), in_flight);
}

/// Tell the host to shut down, then join every handler thread.
///
/// The order is the point. A call parked on something only shutdown releases would otherwise hold
/// the join for its whole budget, then write to a stdout nobody reads.
fn drain_in_flight(host: &dyn ToolHost, in_flight: Vec<JoinHandle<()>>) {
    host.begin_shutdown();
    let unfinished = in_flight.iter().filter(|h| !h.is_finished()).count();
    if unfinished > 0 {
        eprintln!("codex-imagegen: finishing {unfinished} in-flight tool call(s)");
    }
    for handle in in_flight {
        let _ = handle.join();
    }
}

/// Run one `tools/call` on its own thread, and answer the request directly if the thread cannot
/// be started. Returns the handle to join at shutdown, or `None` when it answered.
///
/// Takes the `Builder` it spawns from so the failure branch is testable: a `Builder` asked for an
/// impossible stack fails deterministically, without the process having to run out of threads.
///
/// Call it from the reader thread. That is what makes the pending map safe to touch here without
/// further synchronisation, since `handle_cancellation` runs there too: a cancellation can never
/// overtake the insert for the request it names.
fn start_tool_call(
    builder: std::thread::Builder,
    host: &Arc<dyn ToolHost>,
    writer: &Writer,
    pending: &Pending,
    id: Value,
    params: Value,
    progress_interval: Duration,
) -> Option<JoinHandle<()>> {
    let key = jsonrpc::request_key(&id);
    let request = Arc::new(RequestCancel::new());
    {
        let mut map = lock(pending);
        // A second request under an id that is still in flight would make the two share one
        // cancellation entry, and a cancel meant for one would stop the other.
        if map.contains_key(&key) {
            drop(map);
            eprintln!(
                "codex-imagegen: rejected tools/call {}: that request id is already in flight",
                jsonrpc::clamp(&key, 200)
            );
            send_error(
                writer,
                &id,
                -32600,
                "invalid request: this request id is already in flight".into(),
            );
            return None;
        }
        map.insert(key.clone(), Arc::clone(&request));
    }

    // Read off before `params` moves into the closure, for the failure message below.
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    // The captures are cloned inside a block rather than shadowing the originals: a failed spawn
    // drops the closure instead of handing it back, so `writer` and `id` must survive it to answer
    // the request.
    let spawned = {
        let host = Arc::clone(host);
        let writer = Arc::clone(writer);
        let pending_here = Arc::clone(pending);
        let thread_key = key.clone();
        let id = id.clone();
        builder.spawn(move || {
            let mut entry = PendingEntry {
                pending: &pending_here,
                key: &thread_key,
                released: false,
            };
            let reporter = ProgressReporter::start(&writer, &params, &request, progress_interval);
            let ctx = CallContext {
                cancel: Arc::clone(&request),
                progress: reporter
                    .as_ref()
                    .map(ProgressReporter::handle)
                    .unwrap_or_default(),
            };
            let result = run_tool(host.as_ref(), &params, &ctx);
            // Stop and join the reporter before the response. It sends while holding its state
            // lock and checks `done` under that lock first, so once this returns no progress
            // notification can follow the result.
            drop(reporter);
            // Released before the send, so a cancellation arriving after this point finds nothing
            // in the map at all.
            entry.release();
            // A cancelled request gets no response: the client has stopped waiting for one, and
            // the spec says not to send it. Claiming rather than merely checking settles the one
            // remaining window -- a cancellation already past the map lookup and about to call
            // `cancel` -- so the response and the cancel hook can never both happen.
            if !request.try_claim_response() {
                return;
            }
            jsonrpc::send(
                &writer,
                &json!({"jsonrpc": "2.0", "id": id, "result": result}),
            );
        })
    };

    match spawned {
        Ok(handle) => Some(handle),
        Err(e) => {
            lock(pending).remove(&key);
            eprintln!("codex-imagegen: could not start a thread for tools/call: {e}");
            // Unconditional, with no claim: the claim settles a race with `handle_cancellation`,
            // and there is none here, because this runs on the reader thread -- the only thread
            // that handles cancellations -- so nothing can have landed between the insert and the
            // remove above.
            let text = errors::handler_thread_unavailable(&tool, &e.to_string());
            jsonrpc::send(
                writer,
                &json!({"jsonrpc": "2.0", "id": id, "result": text_result(text, true)}),
            );
            None
        }
    }
}

/// Dispatch to the host, turning a missing tool name or a panic into an `isError` result.
///
/// A panic is caught rather than left to unwind the thread: an unwound handler sends nothing, and
/// the client would wait for its own timeout -- potentially hours -- with no cause attached.
fn run_tool(host: &dyn ToolHost, params: &Value, ctx: &CallContext) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return failure_result(&errors::bad_request(
            "tools/call needs the tool's name in params.name.",
        ));
    };
    let args = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(args) => args.clone(),
    };
    match catch_unwind(AssertUnwindSafe(|| host.call_tool(name, &args, ctx))) {
        Ok(result) => result,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_string());
            eprintln!("codex-imagegen: the {name} call panicked: {message}");
            failure_result(
                &errors::internal_error(format!(
                    "The {name} call failed unexpectedly inside codex-imagegen."
                ))
                .with_detail(message),
            )
        }
    }
}

/// Honour `notifications/cancelled`: mark the request so it gets no response, and run the hook
/// its call installed to stop the work it started.
///
/// The hook runs here, on the reader thread, so it must not block: it should send (a
/// `turn/interrupt`, say) and not wait for the answer.
fn handle_cancellation(pending: &Pending, params: &Value) {
    let reason = jsonrpc::clamp(
        params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("no reason given"),
        200,
    );
    let Some(request_id) = params.get("requestId") else {
        eprintln!("codex-imagegen: ignoring notifications/cancelled with no requestId ({reason})");
        return;
    };
    // The lookup uses the id exactly as sent; only the echo of it is clamped.
    let key = jsonrpc::request_key(request_id);
    let shown = jsonrpc::clamp(&key, 200);

    // Removed here rather than by the handler, so a duplicate notification finds nothing and
    // cancels nothing. The handler's own removal is then a no-op.
    let entry = lock(pending).remove(&key);
    let Some(entry) = entry else {
        // Routine: a cancellation racing a response that already went out lands here, as does one
        // for a request answered on the reader thread, including `initialize`, which the spec
        // says must not be cancelled.
        eprintln!(
            "codex-imagegen: cancellation for request {shown}, which is not in flight ({reason})"
        );
        return;
    };
    if entry.cancel() {
        eprintln!("codex-imagegen: request {shown} cancelled ({reason}); stopped its work");
    } else {
        eprintln!("codex-imagegen: request {shown} cancelled ({reason})");
    }
}

/// Answer every request that is not a tool call. These are all instant.
fn handle_sync(host: &dyn ToolHost, method: &str, params: &Value, id: &Value) -> Value {
    let result = match method {
        "initialize" => json!({
            "protocolVersion": negotiate_protocol(params),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": crate::VERSION},
            "instructions": host.instructions(),
        }),
        "tools/list" => json!({"tools": host.tool_definitions()}),
        "ping" => json!({}),
        // Not declared in our capabilities, but some clients probe anyway, and an empty list is
        // friendlier than an error.
        "resources/list" => json!({"resources": []}),
        "resources/templates/list" => json!({"resourceTemplates": []}),
        "prompts/list" => json!({"prompts": []}),
        other => {
            return error_response(
                id,
                -32601,
                format!("method not found: {}", jsonrpc::clamp(other, 200)),
            )
        }
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Echo the client's protocol version when we support it; otherwise -- unknown, missing, or not
/// a string -- answer the newest we support.
fn negotiate_protocol(params: &Value) -> &'static str {
    params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .and_then(|requested| {
            SUPPORTED_PROTOCOLS
                .iter()
                .copied()
                .find(|v| *v == requested)
        })
        .unwrap_or(SUPPORTED_PROTOCOLS[0])
}

/// A text-only tool result. Image blocks join the content list once generation exists.
pub fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}], "isError": is_error})
}

/// A failure as a tool result, rendered in the form its code calls for.
pub fn failure_result(failure: &Failure) -> Value {
    text_result(failure.render_for_agent(), true)
}

fn error_response(id: &Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn send_error(writer: &Writer, id: &Value, code: i64, message: String) {
    jsonrpc::send(writer, &error_response(id, code, message));
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Removes a handler's entry from the pending map however the handler ends.
///
/// Normally `release` does it the moment the work is done. The `Drop` is the net for a handler
/// that unwinds anyway, which would otherwise strand the entry for the life of the process.
struct PendingEntry<'a> {
    pending: &'a Pending,
    key: &'a str,
    released: bool,
}

impl PendingEntry<'_> {
    fn release(&mut self) {
        if !self.released {
            lock(self.pending).remove(self.key);
            self.released = true;
        }
    }
}

impl Drop for PendingEntry<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

struct ProgressShared {
    state: Mutex<ProgressState>,
    wake: Condvar,
}

impl ProgressShared {
    fn lock(&self) -> MutexGuard<'_, ProgressState> {
        lock(&self.state)
    }
}

#[derive(Default)]
struct ProgressState {
    /// Set when the call has finished. Checked under the lock before every send.
    done: bool,
    phase: Option<String>,
    /// The phase changed since the last notification, which is reported without waiting.
    changed: bool,
}

/// Sends MCP `notifications/progress` for one call, from a thread of its own.
///
/// MCP makes progress opt-in: the client puts a `progressToken` in the request's `_meta`. Without
/// one this is never created and the call sends nothing but its result.
struct ProgressReporter {
    shared: Arc<ProgressShared>,
    thread: Option<JoinHandle<()>>,
}

impl ProgressReporter {
    fn start(
        writer: &Writer,
        params: &Value,
        request: &Arc<RequestCancel>,
        interval: Duration,
    ) -> Option<Self> {
        // Only a string or a number is a valid token. It is echoed exactly as sent, type
        // included, or the client cannot match the notification to its request.
        let token = params
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"))
            .filter(|token| token.is_string() || token.is_number())?
            .clone();
        // Do not announce a request the client has already abandoned.
        if request.is_cancelled() {
            return None;
        }
        let shared = Arc::new(ProgressShared {
            state: Mutex::new(ProgressState::default()),
            wake: Condvar::new(),
        });
        let thread = {
            let shared = Arc::clone(&shared);
            let writer = Arc::clone(writer);
            let request = Arc::clone(request);
            std::thread::Builder::new()
                .name("tools-progress".to_string())
                .spawn(move || report_progress(&shared, &writer, &token, &request, interval))
                // No thread, no progress: the call itself is unaffected.
                .ok()?
        };
        Some(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn handle(&self) -> Progress {
        Progress {
            shared: Some(Arc::clone(&self.shared)),
        }
    }
}

impl Drop for ProgressReporter {
    fn drop(&mut self) {
        {
            let mut state = self.shared.lock();
            state.done = true;
            self.shared.wake.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The reporter thread's loop: wait for a phase change or the interval, then send.
fn report_progress(
    shared: &ProgressShared,
    writer: &Writer,
    token: &Value,
    request: &RequestCancel,
    interval: Duration,
) {
    let started = Instant::now();
    let mut last_sent: Option<(u64, Instant)> = None;
    let mut state = shared.lock();
    loop {
        let next_tick = last_sent.map_or(started, |(_, at)| at) + interval;
        while !state.done && !state.changed {
            let now = Instant::now();
            if now >= next_tick {
                break;
            }
            state = shared
                .wake
                .wait_timeout(state, next_tick - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        if state.done || request.is_cancelled() {
            return;
        }
        let elapsed = started.elapsed().as_secs();
        // Elapsed whole seconds, nudged up by one when two notifications fall in the same second:
        // the spec requires progress to increase with every notification. The message always shows
        // the true elapsed time.
        let progress = match last_sent {
            Some((previous, _)) if elapsed <= previous => previous + 1,
            _ => elapsed,
        };
        let message = progress_message(state.phase.as_deref().unwrap_or("working"), elapsed);
        // Sent while holding the state lock. `Drop` cannot mark the reporter done until this
        // finishes, which is what guarantees no notification follows the final response.
        jsonrpc::send(
            writer,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                // No `total`: a percentage of an image generation would be invented.
                "params": {"progressToken": token, "progress": progress, "message": message},
            }),
        );
        state.changed = false;
        last_sent = Some((progress, Instant::now()));
    }
}

/// `<phase> (<n> s)`, within the length Claude Code shows.
fn progress_message(phase: &str, elapsed_secs: u64) -> String {
    let suffix = format!(" ({elapsed_secs} s)");
    let room = MAX_PROGRESS_MESSAGE_CHARS - suffix.len() - 1;
    format!("{}{suffix}", jsonrpc::clamp(phase, room))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::Recorder;
    use std::io::{BufReader, Read};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    const WAIT: Duration = Duration::from_secs(10);

    /// A one-shot flag another thread can wait on.
    #[derive(Default)]
    struct Signal {
        set: Mutex<bool>,
        changed: Condvar,
    }

    impl Signal {
        fn set(&self) {
            *self.set.lock().unwrap() = true;
            self.changed.notify_all();
        }

        fn wait(&self, timeout: Duration) -> bool {
            let guard = self.set.lock().unwrap();
            let (guard, _) = self
                .changed
                .wait_timeout_while(guard, timeout, |set| !*set)
                .unwrap();
            *guard
        }

        fn is_set(&self) -> bool {
            *self.set.lock().unwrap()
        }
    }

    /// A tool layer with tools that exercise the protocol loop.
    #[derive(Default)]
    struct FakeHost {
        shutdown: Signal,
        hook_installed: Signal,
        hook_ran: Signal,
        hook_runs: AtomicUsize,
        call_running: Signal,
    }

    impl ToolHost for FakeHost {
        fn instructions(&self) -> String {
            "fake instructions".to_string()
        }

        fn tool_definitions(&self) -> Vec<Value> {
            vec![json!({"name": "echo", "inputSchema": {"type": "object"}})]
        }

        fn call_tool(&self, name: &str, args: &Value, ctx: &CallContext) -> Value {
            match name {
                "echo" => text_result(args.to_string(), false),
                "phases" => {
                    let pause = args["pause_ms"].as_u64().unwrap_or(0);
                    for phase in args["phases"].as_array().unwrap() {
                        ctx.progress().set_phase(phase.as_str().unwrap());
                        std::thread::sleep(Duration::from_millis(pause));
                    }
                    text_result("done", false)
                }
                "block_until_cancelled" => {
                    // The hook signals a flag of its own, since it must be 'static and cannot
                    // borrow the host.
                    let ran = Arc::new(Signal::default());
                    let installed = {
                        let ran = Arc::clone(&ran);
                        ctx.cancel().set_hook(Box::new(move || ran.set()))
                    };
                    assert!(installed, "cancelled before the hook was installed");
                    self.hook_installed.set();
                    if ran.wait(WAIT) {
                        self.hook_runs.fetch_add(1, Ordering::SeqCst);
                        self.hook_ran.set();
                    }
                    text_result("finished", false)
                }
                "block_until_shutdown" => {
                    self.call_running.set();
                    let released = self.shutdown.wait(WAIT);
                    text_result(if released { "released" } else { "timed out" }, false)
                }
                "panic" => panic!("deliberate test panic"),
                other => failure_result(&errors::bad_request(format!("unknown tool {other}"))),
            }
        }

        fn begin_shutdown(&self) {
            self.shutdown.set();
        }
    }

    /// Feeds the server's stdin from the test, one chunk at a time, and reports end of stream
    /// when the sender is dropped.
    struct ChannelReader {
        rx: mpsc::Receiver<Vec<u8>>,
        chunk: Vec<u8>,
        pos: usize,
    }

    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.chunk.len() {
                match self.rx.recv() {
                    Ok(chunk) => {
                        self.chunk = chunk;
                        self.pos = 0;
                    }
                    Err(_) => return Ok(0),
                }
            }
            let n = buf.len().min(self.chunk.len() - self.pos);
            buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    /// The protocol loop running on its own thread, with the test holding both ends.
    struct Harness {
        tx: Option<mpsc::Sender<Vec<u8>>>,
        recorder: Recorder,
        server: Option<JoinHandle<()>>,
    }

    impl Harness {
        fn start(host: Arc<FakeHost>, options: ServeOptions) -> Self {
            let (tx, rx) = mpsc::channel();
            let recorder = Recorder::default();
            let writer = recorder.writer();
            let server = std::thread::spawn(move || {
                let reader = BufReader::new(ChannelReader {
                    rx,
                    chunk: Vec::new(),
                    pos: 0,
                });
                serve_on(host, reader, writer, &options);
            });
            Self {
                tx: Some(tx),
                recorder,
                server: Some(server),
            }
        }

        fn send_raw(&self, bytes: &[u8]) {
            self.tx.as_ref().unwrap().send(bytes.to_vec()).unwrap();
        }

        fn send(&self, message: Value) {
            self.send_raw(format!("{message}\n").as_bytes());
        }

        /// Poll until `done` holds for the messages so far.
        fn wait_until(&self, what: &str, done: impl Fn(&[Value]) -> bool) -> Vec<Value> {
            let deadline = Instant::now() + WAIT;
            loop {
                let messages = self.recorder.messages();
                if done(&messages) {
                    return messages;
                }
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Close stdin, wait for the server to shut down, and return everything it wrote.
        fn finish(mut self) -> Vec<Value> {
            drop(self.tx.take());
            self.server.take().unwrap().join().expect("server thread");
            self.recorder.messages()
        }
    }

    fn run(host: Arc<FakeHost>, lines: &[Value]) -> Vec<Value> {
        let harness = Harness::start(host, ServeOptions::default());
        for line in lines {
            harness.send(line.clone());
        }
        harness.finish()
    }

    fn call(id: Value, name: &str, args: Value) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": "tools/call",
               "params": {"name": name, "arguments": args}})
    }

    fn with_id<'a>(messages: &'a [Value], id: &Value) -> Vec<&'a Value> {
        messages
            .iter()
            .filter(|m| m.get("id") == Some(id))
            .collect()
    }

    fn host() -> Arc<FakeHost> {
        Arc::new(FakeHost::default())
    }

    fn initialize(params: Value) -> Value {
        handle_sync(&FakeHost::default(), "initialize", &params, &json!(1))
    }

    #[test]
    fn initialize_echoes_every_supported_protocol_version() {
        for version in SUPPORTED_PROTOCOLS {
            let response = initialize(json!({"protocolVersion": version}));
            assert_eq!(response["result"]["protocolVersion"], *version);
        }
        // What Claude Code sends.
        let response = initialize(json!({"protocolVersion": "2025-11-25"}));
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
    }

    #[test]
    fn initialize_answers_the_newest_version_for_anything_else() {
        for params in [
            json!({"protocolVersion": "1999-01-01"}),
            json!({"protocolVersion": "2099-01-01"}),
            json!({"protocolVersion": 20251125}),
            json!({"protocolVersion": null}),
            json!({}),
            Value::Null,
        ] {
            let response = initialize(params.clone());
            assert_eq!(
                response["result"]["protocolVersion"], "2025-11-25",
                "for {params}"
            );
        }
    }

    #[test]
    fn initialize_describes_a_tools_only_server() {
        let response = initialize(json!({"protocolVersion": "2025-11-25"}));
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
        let result = &response["result"];
        assert_eq!(result["capabilities"], json!({"tools": {}}));
        assert_eq!(result["serverInfo"]["name"], "codex-imagegen");
        assert_eq!(result["serverInfo"]["version"], crate::VERSION);
        assert_eq!(result["instructions"], "fake instructions");
    }

    #[test]
    fn tools_list_returns_the_hosts_definitions() {
        let response = handle_sync(&FakeHost::default(), "tools/list", &json!({}), &json!(2));
        assert_eq!(response["result"]["tools"][0]["name"], "echo");
    }

    #[test]
    fn ping_and_the_empty_lists_are_answered() {
        let host = FakeHost::default();
        let ping = handle_sync(&host, "ping", &Value::Null, &json!(3));
        assert_eq!(ping["result"], json!({}));
        assert_eq!(
            handle_sync(&host, "resources/list", &Value::Null, &json!(4))["result"],
            json!({"resources": []})
        );
        assert_eq!(
            handle_sync(&host, "resources/templates/list", &Value::Null, &json!(5))["result"],
            json!({"resourceTemplates": []})
        );
        assert_eq!(
            handle_sync(&host, "prompts/list", &Value::Null, &json!(6))["result"],
            json!({"prompts": []})
        );
    }

    #[test]
    fn an_unknown_method_is_a_method_not_found_error() {
        let response = handle_sync(
            &FakeHost::default(),
            "does/not/exist",
            &Value::Null,
            &json!(7),
        );
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["id"], 7);
        assert!(response.get("result").is_none());
    }

    #[test]
    fn a_session_is_served_end_to_end_including_a_leading_bom() {
        let harness = Harness::start(host(), ServeOptions::default());
        harness.send_raw(
            b"\xef\xbb\xbf{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\
              \"params\":{\"protocolVersion\":\"2025-11-25\"}}\r\n",
        );
        harness.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        harness.send(json!({"jsonrpc": "2.0", "id": "list", "method": "tools/list"}));
        harness.send(call(json!(3), "echo", json!({"x": 1})));
        let messages = harness.finish();

        assert_eq!(messages.len(), 3, "{messages:?}");
        assert_eq!(messages[0]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(messages[1]["id"], "list");
        let result = &with_id(&messages, &json!(3))[0]["result"];
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["type"], "text");
        assert_eq!(result["content"][0]["text"], r#"{"x":1}"#);
    }

    #[test]
    fn bad_input_gets_an_error_with_a_null_id_and_the_loop_carries_on() {
        let harness = Harness::start(
            host(),
            ServeOptions {
                max_line_bytes: 64,
                ..ServeOptions::default()
            },
        );
        harness.send_raw(b"{not json\n");
        harness.send_raw(b"[1,2]\n");
        harness.send_raw(b"{}\n");
        harness.send_raw(b"\xff\xfe\n");
        harness.send_raw(format!("{{\"x\":\"{}\"}}\n", "a".repeat(100)).as_bytes());
        harness.send_raw(b"\n   \n");
        harness.send(json!({"jsonrpc": "2.0", "id": 9, "method": "ping"}));
        let messages = harness.finish();

        let codes: Vec<i64> = messages[..5]
            .iter()
            .map(|m| {
                assert_eq!(m["id"], Value::Null, "{m}");
                m["error"]["code"].as_i64().unwrap()
            })
            .collect();
        assert_eq!(codes, vec![-32700, -32600, -32600, -32700, -32700]);
        // Blank lines are skipped silently, and the next request is still answered.
        assert_eq!(messages.len(), 6);
        assert_eq!(messages[5]["id"], 9);
        assert_eq!(messages[5]["result"], json!({}));
    }

    #[test]
    fn notifications_and_stray_responses_are_never_answered() {
        let messages = run(
            host(),
            &[
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "method": "notifications/unknown", "params": {}}),
                json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                       "params": {"requestId": 99}}),
                json!({"jsonrpc": "2.0", "id": 5, "result": {}}),
                json!({"jsonrpc": "2.0", "id": null, "method": "ping"}),
            ],
        );
        assert!(messages.is_empty(), "{messages:?}");
    }

    #[test]
    fn a_tool_call_without_a_name_is_an_is_error_result() {
        let messages = run(
            host(),
            &[json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}})],
        );
        let result = &messages[0]["result"];
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.starts_with("REQUEST REJECTED\ncode: BAD_REQUEST"),
            "{text}"
        );
    }

    #[test]
    fn a_panicking_tool_is_answered_with_an_internal_error() {
        let messages = run(host(), &[call(json!(1), "panic", json!({}))]);
        let result = &messages[0]["result"];
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("INTERNAL_ERROR"), "{text}");
        assert!(text.contains("deliberate test panic"), "{text}");
    }

    #[test]
    fn a_cancelled_call_runs_its_hook_and_gets_no_response() {
        let host = host();
        let harness = Harness::start(Arc::clone(&host), ServeOptions::default());
        harness.send(call(json!(5), "block_until_cancelled", json!({})));
        assert!(
            host.hook_installed.wait(WAIT),
            "the call never installed its hook"
        );
        harness.send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                            "params": {"requestId": 5, "reason": "user pressed Esc"}}),
        );
        assert!(
            host.hook_ran.wait(WAIT),
            "the cancellation did not run the hook"
        );
        // A later request is still served, so the loop is alive and the call has had its chance
        // to (wrongly) answer.
        harness.send(json!({"jsonrpc": "2.0", "id": 6, "method": "ping"}));
        harness.wait_until("the ping", |m| !with_id(m, &json!(6)).is_empty());
        let messages = harness.finish();

        assert!(with_id(&messages, &json!(5)).is_empty(), "{messages:?}");
        assert_eq!(host.hook_runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_cancellation_names_the_request_by_id_type_as_well_as_value() {
        let host = host();
        let harness = Harness::start(Arc::clone(&host), ServeOptions::default());
        harness.send(call(json!(7), "block_until_cancelled", json!({})));
        assert!(host.hook_installed.wait(WAIT));
        // "7" is a different request from 7, so this cancels nothing.
        harness.send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                            "params": {"requestId": "7"}}),
        );
        harness.send(json!({"jsonrpc": "2.0", "id": 8, "method": "ping"}));
        harness.wait_until("the ping", |m| !with_id(m, &json!(8)).is_empty());
        assert!(!host.hook_ran.is_set());
        harness.send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                            "params": {"requestId": 7}}),
        );
        assert!(host.hook_ran.wait(WAIT));
        let messages = harness.finish();
        assert!(with_id(&messages, &json!(7)).is_empty());
    }

    #[test]
    fn a_request_id_already_in_flight_is_rejected() {
        let host = host();
        let harness = Harness::start(Arc::clone(&host), ServeOptions::default());
        harness.send(call(json!(9), "block_until_cancelled", json!({})));
        assert!(host.hook_installed.wait(WAIT));
        harness.send(call(json!(9), "echo", json!({})));
        harness.wait_until("the duplicate's rejection", |m| {
            !with_id(m, &json!(9)).is_empty()
        });
        // The original is still cancellable under its id: the duplicate did not displace it.
        harness.send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                            "params": {"requestId": 9}}),
        );
        assert!(host.hook_ran.wait(WAIT));
        let messages = harness.finish();

        let for_nine = with_id(&messages, &json!(9));
        assert_eq!(for_nine.len(), 1, "{messages:?}");
        assert_eq!(for_nine[0]["error"]["code"], -32600);
    }

    #[test]
    fn shutdown_releases_parked_calls_before_joining_them() {
        let host = host();
        let harness = Harness::start(Arc::clone(&host), ServeOptions::default());
        harness.send(call(json!(1), "block_until_shutdown", json!({})));
        assert!(host.call_running.wait(WAIT));
        let started = Instant::now();
        let messages = harness.finish();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the join waited out the parked call"
        );
        // Released rather than abandoned: it still answered.
        assert_eq!(
            with_id(&messages, &json!(1))[0]["result"]["content"][0]["text"],
            "released"
        );
    }

    fn progress_for<'a>(messages: &'a [Value], token: &Value) -> Vec<&'a Value> {
        messages
            .iter()
            .filter(|m| {
                m["method"] == "notifications/progress" && m["params"]["progressToken"] == *token
            })
            .collect()
    }

    fn fast_progress() -> ServeOptions {
        ServeOptions {
            progress_interval: Duration::from_millis(20),
            ..ServeOptions::default()
        }
    }

    #[test]
    fn progress_follows_phases_increases_strictly_and_never_follows_the_result() {
        for token in [json!("tok-1"), json!(42)] {
            let harness = Harness::start(host(), fast_progress());
            let mut request = call(
                json!(1),
                "phases",
                json!({"phases": ["starting Codex", "generating image", "saving image"],
                       "pause_ms": 70}),
            );
            request["params"]["_meta"] = json!({"progressToken": token});
            harness.send(request);
            harness.wait_until("the result", |m| !with_id(m, &json!(1)).is_empty());
            let messages = harness.finish();

            let progress = progress_for(&messages, &token);
            assert!(progress.len() >= 3, "{messages:?}");
            let values: Vec<u64> = progress
                .iter()
                .map(|m| m["params"]["progress"].as_u64().unwrap())
                .collect();
            assert!(values.windows(2).all(|w| w[0] < w[1]), "{values:?}");
            for note in &progress {
                assert!(note["params"].get("total").is_none());
                // The token is echoed exactly, type included.
                assert_eq!(note["params"]["progressToken"], token);
                let message = note["params"]["message"].as_str().unwrap();
                assert!(message.ends_with(" s)"), "{message}");
            }
            let messages_text: Vec<&str> = progress
                .iter()
                .map(|m| m["params"]["message"].as_str().unwrap())
                .collect();
            for phase in ["starting Codex", "generating image", "saving image"] {
                assert!(
                    messages_text.iter().any(|m| m.starts_with(phase)),
                    "{phase} missing from {messages_text:?}"
                );
            }
            // Nothing about this request follows its result.
            let result_at = messages.iter().position(|m| m["id"] == json!(1)).unwrap();
            assert_eq!(result_at, messages.len() - 1, "{messages:?}");
        }
    }

    #[test]
    fn progress_is_opt_in_and_needs_a_valid_token() {
        for meta in [None, Some(json!({"progressToken": true})), Some(json!({}))] {
            let harness = Harness::start(host(), fast_progress());
            let mut request = call(
                json!(1),
                "phases",
                json!({"phases": ["generating image"], "pause_ms": 80}),
            );
            if let Some(meta) = meta {
                request["params"]["_meta"] = meta;
            }
            harness.send(request);
            let messages = harness.finish();
            assert_eq!(messages.len(), 1, "{messages:?}");
            assert_eq!(messages[0]["id"], 1);
        }
    }

    #[test]
    fn progress_stops_once_the_request_is_cancelled() {
        let host = host();
        let harness = Harness::start(Arc::clone(&host), fast_progress());
        let mut request = call(json!(1), "block_until_cancelled", json!({}));
        request["params"]["_meta"] = json!({"progressToken": "p"});
        harness.send(request);
        assert!(host.hook_installed.wait(WAIT));
        harness.wait_until("a periodic notification", |m| {
            !progress_for(m, &json!("p")).is_empty()
        });
        harness.send(
            json!({"jsonrpc": "2.0", "method": "notifications/cancelled",
                            "params": {"requestId": 1}}),
        );
        assert!(host.hook_ran.wait(WAIT));
        // One notification may already have been past its check when the cancel landed; after
        // that the count must stay put.
        std::thread::sleep(Duration::from_millis(60));
        let settled = progress_for(&harness.recorder.messages(), &json!("p")).len();
        std::thread::sleep(Duration::from_millis(150));
        let later = progress_for(&harness.recorder.messages(), &json!("p")).len();
        assert_eq!(settled, later, "progress continued after the cancellation");
        let messages = harness.finish();
        assert!(with_id(&messages, &json!(1)).is_empty());
    }

    #[test]
    fn a_progress_message_fits_what_claude_code_shows() {
        assert_eq!(
            progress_message("generating image", 23),
            "generating image (23 s)"
        );
        let long = progress_message(&"x".repeat(500), 86_400);
        assert!(long.chars().count() <= MAX_PROGRESS_MESSAGE_CHARS, "{long}");
        assert!(long.ends_with("… (86400 s)"), "{long}");
    }

    #[test]
    fn a_handler_that_cannot_be_spawned_is_answered_rather_than_left_hanging() {
        // A Builder asking for a usize::MAX stack is refused by the OS outright, so spawn fails
        // deterministically with no threads exhausted and nothing left running.
        let expected = std::thread::Builder::new()
            .stack_size(usize::MAX)
            .spawn(|| {})
            .expect_err("an impossible stack size must be refused")
            .to_string();
        let recorder = Recorder::default();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let host: Arc<dyn ToolHost> = Arc::new(FakeHost::default());

        let handle = start_tool_call(
            std::thread::Builder::new().stack_size(usize::MAX),
            &host,
            &recorder.writer(),
            &pending,
            json!("req-11"),
            json!({"name": "codex_imagegen_generate", "arguments": {}}),
            PROGRESS_INTERVAL,
        );

        assert!(handle.is_none());
        assert!(pending.lock().unwrap().is_empty(), "nothing left to cancel");
        let messages = recorder.messages();
        assert_eq!(messages.len(), 1, "exactly one response, and not none");
        let response = &messages[0];
        assert_eq!(response["id"], "req-11");
        assert!(response.get("error").is_none());
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("INTERNAL_ERROR"));
        assert!(text.contains("codex_imagegen_generate"));
        assert!(text.contains(&expected), "want {expected:?} in: {text}");
    }

    #[test]
    fn a_set_phase_after_the_call_finished_is_ignored() {
        let shared = Arc::new(ProgressShared {
            state: Mutex::new(ProgressState {
                done: true,
                ..ProgressState::default()
            }),
            wake: Condvar::new(),
        });
        let progress = Progress {
            shared: Some(Arc::clone(&shared)),
        };
        progress.set_phase("late");
        assert!(shared.lock().phase.is_none());
        // And an inert handle does nothing at all.
        Progress::default().set_phase("anything");
    }
}
