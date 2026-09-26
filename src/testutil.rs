//! Shared test helpers: scratch directories, and a writer that records what was sent.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::jsonrpc::Writer;

static SEQ: AtomicU32 = AtomicU32::new(0);

/// All test directories live under `%TEMP%\codex-imagegen-tests\<area>`.
const ROOT: &str = "codex-imagegen-tests";

/// A fresh directory under `%TEMP%\codex-imagegen-tests\<area>`, removed when the returned guard
/// drops.
///
/// It is cleared before it is created *and* removed after, and the two cover different failures.
/// The name is unique only per (pid, counter), and Windows recycles process ids, so without the
/// clear a run that inherits an aborted run's directory would read state it did not create and
/// assert against it. `Drop` does not run when the harness aborts, which is exactly that case; the
/// clear does not stop a passing run from leaving anything behind, which is what `Drop` is for.
///
/// Clearing *this* directory rather than sweeping the shared parent is also deliberate: two live
/// runs cannot share a pid, so a per-pid clear can never delete a concurrent test process's state,
/// while a sweep of the parent could.
pub fn temp_dir(area: &str) -> TempDir {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir()
            .join(ROOT)
            .join(area)
            .join(format!("{}-{}", std::process::id(), n));
    match std::fs::remove_dir_all(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        // A clear that fails leaves inherited state in place -- the failure this guards against --
        // so say so here rather than let it surface as an unexplained assertion further down.
        Err(e) => panic!("clear temp dir {}: {e}", path.display()),
    }
    std::fs::create_dir_all(&path).expect("create temp dir");
    TempDir { path }
}

/// Owns one test's directory and removes it when the test ends.
pub struct TempDir {
    path: PathBuf,
}

impl AsRef<Path> for TempDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for TempDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A failing test keeps its directory, because it is the evidence. Safe only because the
        // clear in `temp_dir` means a retained directory can never be read by a later run.
        if std::thread::panicking() {
            eprintln!("kept {} for inspection", self.path.display());
            return;
        }
        // Best effort, unlike the clear: a failure here cannot corrupt anything, since the next
        // user of this path clears it first, and panicking in a drop would replace the result of
        // a test that had already passed.
        std::fs::remove_dir_all(&self.path).ok();
    }
}

/// Make `link` a junction (a directory mount point, a reparse point) to the folder `target`, as
/// `mklink /J` does. Unlike a symbolic link, a junction needs no privilege or Developer Mode, so
/// every test run can make one. Removing the test's directory removes the junction, never what it
/// points at.
pub fn make_junction(link: &Path, target: &Path) {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("cmd")
        .raw_arg(format!(
            "/D /C mklink /J \"{}\" \"{}\"",
            link.display(),
            target.display()
        ))
        .output()
        .expect("run mklink");
    assert!(
        output.status.success(),
        "mklink /J {} {}: {}{}",
        link.display(),
        target.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Set the read-only attribute on the file or folder at `path`, as a user protecting it would.
/// Removing the test's directory still removes it: the standard library's `remove_dir_all` deletes
/// read-only entries.
pub fn set_read_only(path: &Path) {
    let mut permissions = std::fs::metadata(path)
        .expect("read metadata")
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions).expect("set read-only");
}

/// Collects what a code path writes, standing in for our stdout.
#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<Vec<u8>>>);

impl Recorder {
    pub fn writer(&self) -> Writer {
        Arc::new(Mutex::new(self.clone()))
    }

    /// Every complete line written so far, parsed as JSON.
    ///
    /// Only complete lines: a writer on another thread may be mid-message, and polling from here
    /// must not turn that harmless instant into a flaky parse failure.
    pub fn messages(&self) -> Vec<Value> {
        let bytes = self.0.lock().unwrap().clone();
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|last| &bytes[..=last])
            .unwrap_or_default();
        String::from_utf8(complete.to_vec())
            .expect("utf-8")
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is one JSON message"))
            .collect()
    }
}

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
