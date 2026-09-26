//! Client and supervisor for one `codex app-server` child.
//!
//! The child speaks newline-delimited JSON-RPC on its stdio (docs/design.md, "Message
//! handling"). This module owns that conversation and nothing about images:
//!
//! - spawning the child suspended inside a kill-on-close job, so its whole tree dies with us;
//! - one reader thread that routes every line: replies to the request waiting for them (they can
//!   arrive out of order), notifications to an installed handler, and server-to-client requests
//!   to an immediate refusal, so a turn can never hang waiting for an approval nobody will give;
//! - a pending-reply table with per-request deadlines and cancellation, where a reply that
//!   arrives after its request gave up is dropped rather than handed to the next caller;
//! - a stderr drain that keeps a bounded tail, so a failure can say what Codex printed;
//! - shutdown: close the child's stdin, give it a grace period, then terminate the job.
//!
//! What the calls mean (initialize, preflight, threads) lives in `codex.rs`.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::value::RawValue;
use serde_json::{json, Value};

use crate::cancel::RequestCancel;
use crate::jsonrpc::{self, Envelope, Kind, LineReader, ReadLine, Writer};
use crate::winjob::{self, JobObject};

/// How much of the child's stderr is kept for failure details. Codex logs little on stderr
/// (warnings and errors only, in every run observed), so this holds the whole story of a failed
/// start with room to spare.
const STDERR_TAIL_BYTES: usize = 64 * 1024;

/// Longest stderr line kept. A longer one is dropped whole; the tail is for humans.
const STDERR_MAX_LINE_BYTES: usize = 16 * 1024;

/// How often a request waiting for its reply checks for cancellation. A cancellation arrives
/// from the MCP reader thread as a flag flip; polling it keeps the one cancel hook a call may
/// install free for the work that needs it (interrupting a turn).
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// How long the reader, having seen the child's stdout close, waits for stderr to close too
/// before failing the pending requests. Both pipes close together when the child exits, and
/// waiting lets the failure detail include the child's last words.
const STDERR_SETTLE: Duration = Duration::from_secs(1);

/// The refusal sent for every server-to-client request (docs/design.md, "Server-to-client
/// requests").
const DECLINED_MESSAGE: &str = "not supported by codex-imagegen";

/// What to run. Built by `codex.rs`; kept as plain data so the spawn line can be asserted in a
/// test without starting anything.
#[derive(Clone, Debug)]
pub struct SpawnSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Removed from the inherited environment.
    pub env_remove: Vec<&'static str>,
    /// Added to it.
    pub env_set: Vec<(&'static str, OsString)>,
    pub cwd: PathBuf,
}

/// Why a request produced no result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RpcError {
    /// No reply before the request's deadline.
    Timeout { method: String },
    /// The caller's request was cancelled while waiting.
    Cancelled,
    /// The child exited, or closed its output, before replying. `detail` carries the exit status
    /// and the tail of its stderr.
    ChildExited { detail: String },
    /// The child replied with a JSON-RPC error.
    Remote { code: i64, message: String },
    /// Writing the request failed, or the reply could not be read as JSON.
    Io(String),
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout { method } => write!(f, "no reply to {method} before its deadline"),
            Self::Cancelled => f.write_str("the request was cancelled"),
            Self::ChildExited { detail } => write!(f, "the Codex app-server exited: {detail}"),
            Self::Remote { code, message } => write!(f, "error {code}: {message}"),
            Self::Io(detail) => f.write_str(detail),
        }
    }
}

/// Receives each notification: its method and its params, borrowed from the line they arrived on
/// (JSON `null` when absent). Borrowed so an `item/completed` carrying a multi-megabyte image is
/// routed without a copy; a handler that needs to keep something parses out just that.
///
/// Runs on the reader thread, so it must not block: every other reply and notification waits
/// behind it.
pub type NotificationHandler = dyn Fn(&str, &RawValue) + Send + Sync;

type Reply = Result<Value, RpcError>;

/// One `codex app-server` child, or a test transport standing in for one.
pub struct AppServer {
    shared: Arc<Shared>,
    /// Kills the child's whole tree when dropped. `None` for a test transport.
    job: Option<JobObject>,
    pid: Option<u32>,
}

struct Shared {
    /// The child's stdin, as the shared framing's writer type.
    writer: Writer,
    /// The same object, concretely, so shutdown can close it.
    stdin: Arc<Mutex<Closable>>,
    table: Mutex<Table>,
    handler: Mutex<Option<Arc<NotificationHandler>>>,
    stderr: Arc<StderrTail>,
    /// The child process, for its exit status. `None` for a test transport.
    child: Mutex<Option<Child>>,
    next_id: AtomicU64,
}

#[derive(Default)]
struct Table {
    /// Requests waiting for a reply, by request key.
    pending: HashMap<String, SyncSender<Reply>>,
    /// Set, with the failure detail, once the child's output has closed. From then on every
    /// request fails at once instead of waiting out its deadline.
    exited: Option<String>,
}

impl AppServer {
    /// Spawn the child inside a new kill-on-close job, with piped stdio, and start the threads
    /// that read its stdout and drain its stderr.
    pub fn spawn(spec: &SpawnSpec) -> io::Result<Self> {
        let mut cmd = Command::new(&spec.program);
        cmd.args(&spec.args)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in &spec.env_remove {
            cmd.env_remove(name);
        }
        for (name, value) in &spec.env_set {
            cmd.env(name, value);
        }
        let (job, mut child) = winjob::spawn_in_new_job(&mut cmd)?;
        let pid = child.id();
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            // Cannot happen with all three piped; dropping the job kills the child.
            return Err(io::Error::other("the child's stdio was not piped"));
        };

        let tail = Arc::new(StderrTail::default());
        {
            let tail = Arc::clone(&tail);
            std::thread::Builder::new()
                .name("codex-stderr".to_string())
                .spawn(move || drain_stderr(stderr, &tail))?;
        }
        let shared = Shared::new(Box::new(stdin), tail, Some(child));
        start_reader(
            &shared,
            BufReader::new(stdout),
            jsonrpc::APP_SERVER_MAX_LINE_BYTES,
        )?;
        Ok(Self {
            shared,
            job: Some(job),
            pid: Some(pid),
        })
    }

    /// A client over an arbitrary transport, with no process behind it. For tests: the other end
    /// is a scripted fake server, and a small `max_line_bytes` tests the line cap.
    #[cfg(test)]
    pub fn from_transport(
        reader: impl BufRead + Send + 'static,
        writer: impl Write + Send + 'static,
        max_line_bytes: usize,
    ) -> Self {
        let tail = Arc::new(StderrTail::default());
        tail.finish();
        let shared = Shared::new(Box::new(writer), tail, None);
        start_reader(&shared, reader, max_line_bytes).expect("start the reader thread");
        Self {
            shared,
            job: None,
            pid: None,
        }
    }

    /// The child's process id, when there is a process.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Install the notification handler, replacing any earlier one. Notifications that arrive
    /// while none is installed are dropped, so install it before the first request.
    pub fn set_notification_handler(&self, handler: Arc<NotificationHandler>) {
        *lock(&self.shared.handler) = Some(handler);
    }

    /// True until the child's output closes or the process exits.
    pub fn is_alive(&self) -> bool {
        if lock(&self.shared.table).exited.is_some() {
            return false;
        }
        match lock(&self.shared.child).as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => true,
        }
    }

    /// Send a request and wait for its reply, until `deadline` or until `cancel` is cancelled.
    ///
    /// On a deadline or a cancellation the pending entry is removed before returning, so a reply
    /// that arrives later is dropped by the reader instead of lingering in the table.
    pub fn request(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
        cancel: Option<&RequestCancel>,
    ) -> Result<Value, RpcError> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let key = jsonrpc::request_key(&json!(id));
        // Room for one reply, so the reader never blocks on a caller that has not started
        // waiting yet.
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut table = lock(&self.shared.table);
            if let Some(detail) = &table.exited {
                return Err(RpcError::ChildExited {
                    detail: detail.clone(),
                });
            }
            // Registered before the write, so even an instant reply finds its entry.
            table.pending.insert(key.clone(), tx);
        }

        // No "jsonrpc" member: Codex documents that it neither sends nor expects one.
        let message = json!({"id": id, "method": method, "params": params});
        if let Err(e) = jsonrpc::try_send(&self.shared.writer, &message) {
            let mut table = lock(&self.shared.table);
            table.pending.remove(&key);
            return Err(match &table.exited {
                Some(detail) => RpcError::ChildExited {
                    detail: detail.clone(),
                },
                None => RpcError::Io(format!("could not send {method}: {e}")),
            });
        }

        loop {
            if cancel.is_some_and(RequestCancel::is_cancelled) {
                lock(&self.shared.table).pending.remove(&key);
                return Err(RpcError::Cancelled);
            }
            let now = Instant::now();
            if now >= deadline {
                lock(&self.shared.table).pending.remove(&key);
                return Err(RpcError::Timeout {
                    method: method.to_string(),
                });
            }
            let wait = match cancel {
                Some(_) => (deadline - now).min(CANCEL_POLL),
                None => deadline - now,
            };
            match rx.recv_timeout(wait) {
                Ok(reply) => return reply,
                Err(RecvTimeoutError::Timeout) => continue,
                // The sender is only ever dropped after sending, so this means the table was torn
                // down without a reply; report it as the exit it must have been.
                Err(RecvTimeoutError::Disconnected) => {
                    let detail = lock(&self.shared.table)
                        .exited
                        .clone()
                        .unwrap_or_else(|| "the reply channel closed".to_string());
                    return Err(RpcError::ChildExited { detail });
                }
            }
        }
    }

    /// Send a notification. `params` is omitted when `None`.
    pub fn notify(&self, method: &str, params: Option<Value>) -> Result<(), RpcError> {
        let message = match params {
            Some(params) => json!({"method": method, "params": params}),
            None => json!({"method": method}),
        };
        jsonrpc::try_send(&self.shared.writer, &message).map_err(|e| {
            match lock(&self.shared.table).exited.clone() {
                Some(detail) => RpcError::ChildExited { detail },
                None => RpcError::Io(format!("could not send {method}: {e}")),
            }
        })
    }

    /// Stop the child: close its stdin (an idle app-server exits within about 0.07 s of that
    /// [verified]), wait up to `grace` for it to exit, then terminate its job, which takes every
    /// descendant with it. Safe to call more than once, and from any thread.
    pub fn shutdown(&self, grace: Duration) {
        lock(&self.shared.stdin).close();
        let Some(job) = &self.job else {
            return;
        };
        let deadline = Instant::now() + grace;
        loop {
            let exited = match lock(&self.shared.child).as_mut() {
                Some(child) => !matches!(child.try_wait(), Ok(None)),
                None => true,
            };
            if exited {
                // The direct child is gone, but a helper it started may not be; the job reaps
                // those too.
                job.terminate();
                return;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "codex-imagegen: the Codex app-server did not exit within {} ms of its stdin \
                     closing; terminating it",
                    grace.as_millis()
                );
                job.terminate();
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        // Closing stdin first gives the child a chance to exit on its own; dropping the job right
        // after kills whatever is left either way.
        lock(&self.shared.stdin).close();
    }
}

impl Shared {
    fn new(
        writer: Box<dyn Write + Send>,
        stderr: Arc<StderrTail>,
        child: Option<Child>,
    ) -> Arc<Self> {
        let stdin = Arc::new(Mutex::new(Closable(Some(writer))));
        let writer: Writer = stdin.clone();
        Arc::new(Self {
            writer,
            stdin,
            table: Mutex::new(Table::default()),
            handler: Mutex::new(None),
            stderr,
            child: Mutex::new(child),
            next_id: AtomicU64::new(1),
        })
    }

    /// Route one line from the child.
    fn route(&self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let envelope = match Envelope::parse(line) {
            Ok(envelope) => envelope,
            Err(e) => {
                eprintln!(
                    "codex-imagegen: ignored an app-server line that is not a JSON-RPC message \
                     ({e}): {}",
                    jsonrpc::clamp(line, 200)
                );
                return;
            }
        };
        match envelope.kind() {
            Kind::Response { id } => self.deliver(id, &envelope),
            Kind::Request { id, method } => self.decline(id, method),
            Kind::Notification { method } => {
                let handler = lock(&self.handler).clone();
                if let Some(handler) = handler {
                    handler(method, envelope.params.unwrap_or(RawValue::NULL));
                }
            }
            Kind::Invalid => eprintln!(
                "codex-imagegen: ignored an app-server message with neither an id nor a method: \
                 {}",
                jsonrpc::clamp(line, 200)
            ),
        }
    }

    /// Hand a reply to the request waiting for it, if any still is.
    fn deliver(&self, id: &RawValue, envelope: &Envelope<'_>) {
        let Some(key) = jsonrpc::request_key_raw(id) else {
            return;
        };
        let Some(waiter) = lock(&self.table).pending.remove(&key) else {
            // Routine after a deadline or a cancellation: the request stopped waiting and removed
            // its entry, and this is its reply arriving late.
            eprintln!(
                "codex-imagegen: dropped a late app-server reply to request {}",
                jsonrpc::clamp(&key, 40)
            );
            return;
        };
        let reply = match (envelope.error, envelope.result) {
            (Some(error), _) => Err(remote_error(error)),
            (None, Some(result)) => serde_json::from_str::<Value>(result.get())
                .map_err(|e| RpcError::Io(format!("unreadable reply: {e}"))),
            (None, None) => Ok(Value::Null),
        };
        // The waiter may have given up between the removal and now; its reply is then moot.
        let _ = waiter.send(reply);
    }

    /// Refuse a server-to-client request at once, so nothing in the child waits on an answer
    /// this server will never give (docs/design.md, "Server-to-client requests").
    fn decline(&self, id: &RawValue, method: &str) {
        eprintln!(
            "codex-imagegen: declined the app-server's '{}' request",
            jsonrpc::clamp(method, 100)
        );
        let answer = json!({
            "id": serde_json::from_str::<Value>(id.get()).unwrap_or(Value::Null),
            "error": {"code": -32601, "message": DECLINED_MESSAGE},
        });
        if let Err(e) = jsonrpc::try_send(&self.writer, &answer) {
            eprintln!("codex-imagegen: could not decline the app-server's request: {e}");
        }
    }

    /// The child's output has closed: fail every waiting request, and every later one, with the
    /// best account of why that is available.
    fn mark_exited(&self) {
        self.stderr.wait_finished(STDERR_SETTLE);
        let status = match lock(&self.child).as_mut() {
            None => "it closed its output".to_string(),
            Some(child) => match wait_briefly(child, STDERR_SETTLE) {
                Some(code) => format!("exit code {code}"),
                None => "it closed its output but is still running".to_string(),
            },
        };
        let tail = self.stderr.tail(3000);
        let detail = if tail.is_empty() {
            status
        } else {
            format!("{status}\n--- app-server stderr (last lines) ---\n{tail}")
        };
        let waiting: Vec<SyncSender<Reply>> = {
            let mut table = lock(&self.table);
            table.exited = Some(detail.clone());
            table.pending.drain().map(|(_, tx)| tx).collect()
        };
        for tx in waiting {
            let _ = tx.send(Err(RpcError::ChildExited {
                detail: detail.clone(),
            }));
        }
    }
}

fn start_reader(
    shared: &Arc<Shared>,
    reader: impl BufRead + Send + 'static,
    max_line_bytes: usize,
) -> io::Result<()> {
    let shared = Arc::clone(shared);
    std::thread::Builder::new()
        .name("codex-stdout".to_string())
        .spawn(move || read_loop(&shared, reader, max_line_bytes))?;
    Ok(())
}

/// The reader thread: route every line until the stream ends, then fail whatever is waiting.
fn read_loop(shared: &Shared, reader: impl BufRead, max_line_bytes: usize) {
    let mut lines = LineReader::new(reader, max_line_bytes);
    loop {
        match lines.next_line() {
            Ok(ReadLine::Line(line)) => shared.route(line),
            // A reply lost this way leaves its request to reach its deadline; nothing real comes
            // near the cap, so that is an acceptable price for bounding memory.
            Ok(ReadLine::TooLong) => eprintln!(
                "codex-imagegen: discarded an app-server line longer than {max_line_bytes} bytes"
            ),
            Ok(ReadLine::NotUtf8) => {
                eprintln!("codex-imagegen: discarded an app-server line that is not UTF-8")
            }
            Ok(ReadLine::Eof) => break,
            Err(e) => {
                eprintln!("codex-imagegen: reading the app-server's output failed: {e}");
                break;
            }
        }
    }
    shared.mark_exited();
}

fn remote_error(error: &RawValue) -> RpcError {
    let parsed: Value = serde_json::from_str(error.get()).unwrap_or(Value::Null);
    RpcError::Remote {
        code: parsed.get("code").and_then(Value::as_i64).unwrap_or(0),
        message: parsed
            .get("message")
            .and_then(Value::as_str)
            .map(|m| jsonrpc::clamp(m, 2000))
            .unwrap_or_else(|| jsonrpc::clamp(error.get(), 2000)),
    }
}

/// Poll a child for its exit code for up to `wait`.
fn wait_briefly(child: &mut Child, wait: Duration) -> Option<String> {
    let deadline = Instant::now() + wait;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Some(
                    status
                        .code()
                        .map_or_else(|| "unknown".to_string(), |c| c.to_string()),
                )
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

/// The child's stdin behind a switch, so shutdown can close it while other threads still hold
/// the writer: a write after closing fails as a broken pipe would.
struct Closable(Option<Box<dyn Write + Send>>);

impl Closable {
    fn close(&mut self) {
        // Dropping the pipe handle is what closes it; the child then reads end-of-file.
        self.0 = None;
    }
}

impl Write for Closable {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match &mut self.0 {
            Some(w) => w.write(buf),
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the app-server's stdin is closed",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.0 {
            Some(w) => w.flush(),
            None => Ok(()),
        }
    }
}

/// A bounded tail of the child's stderr, plus whether the stream has ended.
#[derive(Default)]
struct StderrTail {
    state: Mutex<TailState>,
    finished: Condvar,
}

#[derive(Default)]
struct TailState {
    lines: VecDeque<String>,
    bytes: usize,
    done: bool,
}

impl StderrTail {
    fn push(&self, line: String) {
        let mut state = lock(&self.state);
        state.bytes += line.len();
        state.lines.push_back(line);
        while state.bytes > STDERR_TAIL_BYTES {
            match state.lines.pop_front() {
                Some(old) => state.bytes -= old.len(),
                None => break,
            }
        }
    }

    fn finish(&self) {
        lock(&self.state).done = true;
        self.finished.notify_all();
    }

    fn wait_finished(&self, wait: Duration) {
        let state = lock(&self.state);
        let _ = self
            .finished
            .wait_timeout_while(state, wait, |s| !s.done)
            .unwrap_or_else(|e| e.into_inner());
    }

    fn tail(&self, max_chars: usize) -> String {
        let state = lock(&self.state);
        let joined = state
            .lines
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        drop(state);
        let count = joined.chars().count();
        if count <= max_chars {
            return joined;
        }
        let kept: String = joined.chars().skip(count - max_chars).collect();
        format!("…{kept}")
    }
}

/// The stderr thread: keep a tail and echo each line to our own stderr, where the MCP host logs
/// it. Colour codes are removed from both, since neither is a terminal.
fn drain_stderr(stderr: impl Read, tail: &StderrTail) {
    let mut lines = LineReader::new(BufReader::new(stderr), STDERR_MAX_LINE_BYTES);
    loop {
        match lines.next_line() {
            Ok(ReadLine::Line(line)) => {
                let clean = strip_ansi(line);
                if clean.trim().is_empty() {
                    continue;
                }
                eprintln!("codex app-server: {}", jsonrpc::clamp(&clean, 1000));
                tail.push(clean);
            }
            Ok(ReadLine::TooLong) | Ok(ReadLine::NotUtf8) => continue,
            Ok(ReadLine::Eof) | Err(_) => break,
        }
    }
    tail.finish();
}

/// Remove ANSI escape sequences (colour codes and the like) from a log line. Codex colours its
/// log output even into a pipe, and the codes are noise in a failure detail.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        // CSI: ESC '[' parameters, then one final byte in '@'..='~'.
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            // Any other escape: drop ESC and the one character after it.
            chars.next();
        }
    }
    out
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A scripted stand-in for `codex app-server`, so the client can be tested with no process.
#[cfg(test)]
pub mod fake {
    use super::*;
    use std::sync::mpsc::{Receiver, Sender};

    /// What the fake server sends back. Cloneable, so a script can hand it to another thread and
    /// reply later.
    #[derive(Clone)]
    pub struct Outbox(Sender<Vec<u8>>);

    impl Outbox {
        pub fn send(&self, message: Value) {
            self.send_raw(&message.to_string());
        }

        /// Send one line exactly as given, for whitespace and malformed-input cases.
        pub fn send_raw(&self, line: &str) {
            let _ = self.0.send(format!("{line}\n").into_bytes());
        }
    }

    /// What the script decides after each message.
    pub enum Flow {
        Continue,
        /// Close the server's output, as a child that exits does.
        Exit,
    }

    /// Connect a client to a fake server running `script` on its own thread. The script sees
    /// every message the client sends, parsed, and answers through the outbox.
    pub fn connect(script: impl FnMut(&Value, &Outbox) -> Flow + Send + 'static) -> AppServer {
        connect_capped(script, jsonrpc::APP_SERVER_MAX_LINE_BYTES)
    }

    pub fn connect_capped(
        mut script: impl FnMut(&Value, &Outbox) -> Flow + Send + 'static,
        max_line_bytes: usize,
    ) -> AppServer {
        let (to_server, server_in) = mpsc::channel::<Vec<u8>>();
        let (server_out, from_server) = mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let outbox = Outbox(server_out);
            let mut input = BufReader::new(ChannelReader::new(server_in));
            let mut line = String::new();
            loop {
                line.clear();
                match input.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                if let Flow::Exit = script(&message, &outbox) {
                    break;
                }
            }
            // Dropping the outbox here closes the client's input, unless the script kept a
            // clone to reply from another thread.
        });
        AppServer::from_transport(
            BufReader::new(ChannelReader::new(from_server)),
            ChannelWriter(to_server),
            max_line_bytes,
        )
    }

    /// Bytes written here come out of the paired [`ChannelReader`].
    pub struct ChannelWriter(pub Sender<Vec<u8>>);

    impl Write for ChannelWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .send(buf.to_vec())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "peer gone"))?;
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Reads what the paired [`ChannelWriter`] sent; end of stream once every sender is gone.
    pub struct ChannelReader {
        rx: Receiver<Vec<u8>>,
        chunk: Vec<u8>,
        pos: usize,
    }

    impl ChannelReader {
        pub fn new(rx: Receiver<Vec<u8>>) -> Self {
            Self {
                rx,
                chunk: Vec::new(),
                pos: 0,
            }
        }
    }

    impl Read for ChannelReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
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
}

#[cfg(test)]
mod tests {
    use super::fake::{connect, connect_capped, Flow, Outbox};
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }

    /// A fake that answers every request with its method and params echoed back.
    fn echo(message: &Value, out: &Outbox) -> Flow {
        if let (Some(id), Some(method)) = (message.get("id"), message.get("method")) {
            out.send(json!({"id": id, "result": {"method": method, "params": message["params"]}}));
        }
        Flow::Continue
    }

    #[test]
    fn a_request_gets_its_own_reply() {
        let server = connect(echo);
        let reply = server
            .request("model/list", json!({"includeHidden": true}), soon(), None)
            .unwrap();
        assert_eq!(reply["method"], "model/list");
        assert_eq!(reply["params"]["includeHidden"], true);
        assert!(server.is_alive());
    }

    #[test]
    fn requests_carry_an_id_and_no_jsonrpc_member_and_notifications_carry_no_id() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let server = {
            let seen = Arc::clone(&seen);
            connect(move |message, out| {
                seen.lock().unwrap().push(message.clone());
                echo(message, out)
            })
        };
        server
            .request("initialize", json!({}), soon(), None)
            .unwrap();
        server.notify("initialized", None).unwrap();
        server
            .request("config/read", json!({}), soon(), None)
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen[0]["method"], "initialize");
        assert!(seen[0]["id"].is_u64());
        assert!(seen[0].get("jsonrpc").is_none());
        assert_eq!(seen[1], json!({"method": "initialized"}));
        assert_ne!(seen[0]["id"], seen[2]["id"]);
    }

    #[test]
    fn replies_that_arrive_out_of_order_reach_the_right_request() {
        // Hold the first request until the second arrives, then answer the second first.
        let held: Arc<Mutex<Option<Value>>> = Arc::default();
        let server = {
            let held = Arc::clone(&held);
            connect(move |message, out| {
                let mut held = held.lock().unwrap();
                match held.take() {
                    None => *held = Some(message.clone()),
                    Some(first) => {
                        out.send(json!({"id": message["id"], "result": message["method"]}));
                        out.send(json!({"id": first["id"], "result": first["method"]}));
                    }
                }
                Flow::Continue
            })
        };
        let server = Arc::new(server);
        let first = {
            let server = Arc::clone(&server);
            std::thread::spawn(move || server.request("first", json!({}), soon(), None))
        };
        // Make sure the first request is on the wire before the second.
        let deadline = Instant::now() + Duration::from_secs(5);
        while held.lock().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the first request never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        let second = server.request("second", json!({}), soon(), None).unwrap();
        assert_eq!(second, "second");
        assert_eq!(first.join().unwrap().unwrap(), "first");
    }

    #[test]
    fn a_remote_error_is_reported_with_its_code_and_message() {
        let server = connect(|message, out| {
            out.send(json!({"id": message["id"],
                            "error": {"code": -32600, "message": "thread x already has an active writer"}}));
            Flow::Continue
        });
        let err = server
            .request("thread/resume", json!({}), soon(), None)
            .unwrap_err();
        assert_eq!(
            err,
            RpcError::Remote {
                code: -32600,
                message: "thread x already has an active writer".into()
            }
        );
    }

    #[test]
    fn a_server_request_is_declined_at_once_and_the_conversation_continues() {
        let declined: Arc<Mutex<Option<Value>>> = Arc::default();
        let server = {
            let declined = Arc::clone(&declined);
            connect(move |message, out| {
                if message.get("method").is_some() {
                    // Ask for an approval before answering, as a turn can.
                    out.send(
                        json!({"id": "srv-1", "method": "item/commandExecution/requestApproval",
                                    "params": {"threadId": "t"}}),
                    );
                } else {
                    // The client's answer to that ask: record it, then answer the original.
                    *declined.lock().unwrap() = Some(message.clone());
                    out.send(json!({"id": 1, "result": "ok"}));
                }
                Flow::Continue
            })
        };
        assert_eq!(
            server.request("turn/start", json!({}), soon(), None),
            Ok(json!("ok"))
        );
        let answer = declined
            .lock()
            .unwrap()
            .clone()
            .expect("the ask was answered");
        assert_eq!(answer["id"], "srv-1");
        assert_eq!(answer["error"]["code"], -32601);
        assert_eq!(answer["error"]["message"], DECLINED_MESSAGE);
        assert!(answer.get("result").is_none());
    }

    #[test]
    fn notifications_reach_the_handler_borrowed_exactly_as_sent() {
        let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let server = connect(|message, out| {
            // Deliberately odd spacing: a re-serialised copy would normalise it.
            out.send_raw(r#"{"method":"account/updated","params":{ "authMode" :  "chatgpt" }}"#);
            out.send_raw(r#"{"method":"thread/closed"}"#);
            out.send(json!({"id": message["id"], "result": {}}));
            Flow::Continue
        });
        {
            let seen = Arc::clone(&seen);
            server.set_notification_handler(Arc::new(move |method: &str, params: &RawValue| {
                seen.lock()
                    .unwrap()
                    .push((method.to_string(), params.get().to_string()));
            }));
        }
        server
            .request("account/read", json!({}), soon(), None)
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                (
                    "account/updated".to_string(),
                    r#"{ "authMode" :  "chatgpt" }"#.to_string()
                ),
                ("thread/closed".to_string(), "null".to_string()),
            ]
        );
    }

    #[test]
    fn notifications_without_a_handler_are_dropped() {
        let server = connect(|message, out| {
            out.send(json!({"method": "thread/started", "params": {}}));
            out.send(json!({"id": message["id"], "result": 1}));
            Flow::Continue
        });
        assert_eq!(server.request("x", json!({}), soon(), None), Ok(json!(1)));
    }

    #[test]
    fn the_output_closing_fails_waiting_and_later_requests_with_child_exited() {
        let server = connect(|_, _| Flow::Exit);
        let err = server.request("initialize", json!({}), soon(), None);
        assert!(matches!(err, Err(RpcError::ChildExited { .. })), "{err:?}");
        let later = server.request("account/read", json!({}), soon(), None);
        assert!(
            matches!(later, Err(RpcError::ChildExited { .. })),
            "{later:?}"
        );
        assert!(!server.is_alive());
        assert!(matches!(
            server.notify("initialized", None),
            Err(RpcError::ChildExited { .. })
        ));
    }

    #[test]
    fn a_missed_deadline_times_out_and_the_late_reply_is_dropped() {
        let held: Arc<Mutex<Vec<Value>>> = Arc::default();
        let outbox: Arc<Mutex<Option<Outbox>>> = Arc::default();
        let server = {
            let held = Arc::clone(&held);
            let outbox = Arc::clone(&outbox);
            connect(move |message, out| {
                *outbox.lock().unwrap() = Some(out.clone());
                if message["method"] == "slow" {
                    held.lock().unwrap().push(message.clone());
                } else {
                    out.send(json!({"id": message["id"], "result": "fast"}));
                }
                Flow::Continue
            })
        };
        let started = Instant::now();
        let err = server
            .request(
                "slow",
                json!({}),
                Instant::now() + Duration::from_millis(100),
                None,
            )
            .unwrap_err();
        assert_eq!(
            err,
            RpcError::Timeout {
                method: "slow".into()
            }
        );
        assert!(started.elapsed() >= Duration::from_millis(100));
        // The slow reply now turns up. It must not be mistaken for the next request's reply.
        let slow = held.lock().unwrap()[0].clone();
        outbox
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(json!({"id": slow["id"], "result": "slow"}));
        assert_eq!(
            server.request("fast", json!({}), soon(), None),
            Ok(json!("fast"))
        );
        assert!(lock(&server.shared.table).pending.is_empty());
    }

    #[test]
    fn cancelling_stops_the_wait_and_removes_the_entry() {
        let server = connect(|_, _| Flow::Continue); // never answers
        let cancel = Arc::new(RequestCancel::new());
        {
            let cancel = Arc::clone(&cancel);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                cancel.cancel();
            });
        }
        let started = Instant::now();
        let err = server
            .request("config/read", json!({}), soon(), Some(&cancel))
            .unwrap_err();
        assert_eq!(err, RpcError::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(lock(&server.shared.table).pending.is_empty());
    }

    #[test]
    fn an_oversized_line_is_skipped_and_the_stream_stays_usable() {
        let server = connect_capped(
            |message, out| {
                out.send_raw(&format!(
                    r#"{{"method":"item/completed","params":{{"result":"{}"}}}}"#,
                    "A".repeat(500)
                ));
                out.send(json!({"id": message["id"], "result": "after"}));
                Flow::Continue
            },
            256,
        );
        let delivered = Arc::new(AtomicUsize::new(0));
        {
            let delivered = Arc::clone(&delivered);
            server.set_notification_handler(Arc::new(move |_: &str, _: &RawValue| {
                delivered.fetch_add(1, Ordering::SeqCst);
            }));
        }
        assert_eq!(
            server.request("x", json!({}), soon(), None),
            Ok(json!("after"))
        );
        assert_eq!(delivered.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn junk_lines_are_ignored() {
        let server = connect(|message, out| {
            out.send_raw("not json at all");
            out.send_raw("[1,2,3]");
            out.send_raw("{}");
            out.send_raw("");
            out.send(json!({"id": message["id"], "result": true}));
            Flow::Continue
        });
        assert_eq!(
            server.request("x", json!({}), soon(), None),
            Ok(json!(true))
        );
    }

    #[test]
    fn shutdown_closes_the_transport_and_later_requests_fail() {
        let server = connect(echo);
        server.request("x", json!({}), soon(), None).unwrap();
        server.shutdown(Duration::from_millis(100));
        // Closing our side ends the fake, which closes its output: the client sees an exit.
        let err = server.request("y", json!({}), soon(), None).unwrap_err();
        assert!(
            matches!(err, RpcError::ChildExited { .. } | RpcError::Io(_)),
            "{err:?}"
        );
    }

    #[test]
    fn the_stderr_tail_is_bounded_and_keeps_the_end() {
        let tail = StderrTail::default();
        for i in 0..10_000 {
            tail.push(format!("line {i:05} {}", "x".repeat(20)));
        }
        let state = lock(&tail.state);
        assert!(state.bytes <= STDERR_TAIL_BYTES);
        assert!(state.lines.back().unwrap().starts_with("line 09999"));
        drop(state);
        let text = tail.tail(50);
        assert!(text.starts_with('…'));
        assert!(text.ends_with(&"x".repeat(20)));
        assert_eq!(text.chars().count(), 51);
    }

    #[test]
    fn colour_codes_are_stripped_from_log_lines() {
        let line = "\u{1b}[2m2026-09-26T00:08:55Z\u{1b}[0m \u{1b}[31mERROR\u{1b}[0m codex: boom";
        assert_eq!(strip_ansi(line), "2026-09-26T00:08:55Z ERROR codex: boom");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn a_real_process_can_be_spawned_talked_to_and_shut_down() {
        // cmd.exe stands in for the child: `findstr` echoes each stdin line back, so a request
        // comes back as a line with an id and a method -- which the client treats as a server
        // request and declines. What this proves is the plumbing: the job, the pipes, the reader
        // and the shutdown, with no Codex anywhere.
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let spec = SpawnSpec {
            program: PathBuf::from(format!(r"{root}\System32\findstr.exe")),
            args: vec!["x*".to_string()],
            env_remove: vec!["OPENAI_API_KEY"],
            env_set: vec![],
            cwd: std::env::temp_dir(),
        };
        let server = AppServer::spawn(&spec).expect("spawn findstr");
        assert!(server.pid().is_some());
        assert!(server.is_alive());
        server
            .notify("initialized", Some(json!({"x": 1})))
            .expect("write to the child");
        let started = Instant::now();
        server.shutdown(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_secs(6));
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.is_alive() {
            assert!(Instant::now() < deadline, "the child outlived its shutdown");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
