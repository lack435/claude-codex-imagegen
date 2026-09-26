//! Windows job objects, so the Codex app-server's whole process tree dies with this server.
//!
//! `Child::kill` is a single-process TerminateProcess. On Windows that orphans descendants rather
//! than reaping them, and an orphan that inherited one of our pipe handles keeps that pipe open.
//! `codex app-server` starts helpers of its own, so without a job it could outlive both its own
//! exit and our kill, leaking processes and holding our reader threads open.
//!
//! A job object fixes that at the right level. The child is assigned to the job before it runs a
//! single instruction, and `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means every process still in the
//! job dies when the job's last handle closes. That includes the case where codex-imagegen itself
//! crashes: the OS closes the handle, and the tree goes with it (docs/design.md, "Job object").
//!
//! The alternative, shelling out to `taskkill /T`, was rejected on two counts. It is post-hoc, so
//! it cannot help once the direct child has exited and the parent/child links are gone; and
//! invoking it by bare name is an execution hazard, because Windows resolves an unqualified
//! executable through the calling program's directory before System32.

use std::io;
use std::os::raw::c_void;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

type Handle = *mut c_void;

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
// JOBOBJECTINFOCLASS::JobObjectExtendedLimitInformation
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
// JOBOBJECTINFOCLASS::JobObjectBasicAccountingInformation
#[cfg(test)]
const JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION: u32 = 1;
// CreateProcess creation flag: start the primary thread suspended, so the process can be assigned
// to the job before it runs anything.
const CREATE_SUSPENDED: u32 = 0x0000_0004;
// CreateToolhelp32Snapshot flag and thread-access right, for resuming the suspended primary thread.
const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;
const THREAD_SUSPEND_RESUME: u32 = 0x0002;
const INVALID_HANDLE_VALUE: isize = -1;

// LARGE_INTEGER -> i64, DWORD -> u32, SIZE_T / ULONG_PTR -> usize.
#[repr(C)]
#[derive(Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
#[derive(Default)]
struct BasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Default)]
struct ExtendedLimitInformation {
    basic_limit_information: BasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

// JOBOBJECT_BASIC_ACCOUNTING_INFORMATION: only `active_processes` is read, and only by tests.
#[cfg(test)]
#[repr(C)]
#[derive(Default)]
struct BasicAccountingInformation {
    total_user_time: i64,
    total_kernel_time: i64,
    this_period_total_user_time: i64,
    this_period_total_kernel_time: i64,
    total_page_fault_count: u32,
    total_processes: u32,
    active_processes: u32,
    total_terminated_processes: u32,
}

// THREADENTRY32, for the Toolhelp thread walk that resumes the suspended primary thread.
#[repr(C)]
#[derive(Default)]
struct ThreadEntry32 {
    dw_size: u32,
    cnt_usage: u32,
    th32_thread_id: u32,
    th32_owner_process_id: u32,
    tp_base_pri: i32,
    tp_delta_pri: i32,
    dw_flags: u32,
}

extern "system" {
    fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
    fn SetInformationJobObject(
        job: Handle,
        info_class: u32,
        info: *mut c_void,
        info_len: u32,
    ) -> i32;
    #[cfg(test)]
    fn QueryInformationJobObject(
        job: Handle,
        info_class: u32,
        info: *mut c_void,
        info_len: u32,
        return_len: *mut u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
    fn TerminateJobObject(job: Handle, exit_code: u32) -> i32;
    fn CloseHandle(object: Handle) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> Handle;
    fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
    fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry32) -> i32;
    fn OpenThread(desired_access: u32, inherit: i32, thread_id: u32) -> Handle;
    fn ResumeThread(thread: Handle) -> u32;
    fn GetProcessId(process: Handle) -> u32;
}

/// A job object that kills everything still inside it when dropped.
pub struct JobObject {
    handle: Handle,
}

impl JobObject {
    /// Create a job configured to terminate its members on close. `None` if the OS refuses, or if
    /// the kill-on-close limit cannot be set: a job without it would reap nothing, and handing one
    /// back would let a caller believe the tree is contained when it is not.
    pub fn new() -> Option<Self> {
        // SAFETY: a null attributes pointer and null name request the documented defaults
        // (unnamed job, default security descriptor).
        let handle = unsafe { CreateJobObjectW(std::ptr::null_mut(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }
        let job = Self { handle };

        let mut info = ExtendedLimitInformation::default();
        info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        // SAFETY: `info` is a correctly laid out JOBOBJECT_EXTENDED_LIMIT_INFORMATION living on
        // our stack for the duration of the call, and the length passed is its real size.
        let ok = unsafe {
            SetInformationJobObject(
                job.handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<ExtendedLimitInformation>() as u32,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(job)
    }

    /// Put a running child, and so everything it spawns from now on, into the job.
    pub fn assign(&self, child: &Child) -> bool {
        // SAFETY: the handle is valid while `child` is alive, which it is here.
        unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle() as Handle) != 0 }
    }

    /// Kill every process in the job now. Returns whether the OS accepted the request.
    pub fn terminate(&self) -> bool {
        // SAFETY: the handle is owned and valid until Drop.
        unsafe { TerminateJobObject(self.handle, 1) != 0 }
    }

    /// Spawn `cmd` already inside this job. The process is created suspended, assigned to the job
    /// before it runs a single instruction -- so every descendant it later starts is a member --
    /// and only then resumed. Assigning after an ordinary spawn would leave a window in which a
    /// helper started early escapes the job.
    ///
    /// Fail-closed: if the process cannot be assigned, the suspended child is killed and an error
    /// returned, rather than resumed uncontained.
    ///
    /// This sets the command's creation flags to `CREATE_SUSPENDED`, replacing any set before. No
    /// console flag is added, so a console child shares this server's console, as Windows does for
    /// any console program started without one; whether that console is visible was decided by
    /// the host that started this server.
    pub fn spawn_in_job(&self, cmd: &mut Command) -> io::Result<Child> {
        cmd.creation_flags(CREATE_SUSPENDED);
        let mut child = cmd.spawn()?;
        if !self.assign(&child) {
            let assign_err = io::Error::last_os_error();
            // Still suspended and uncontained: kill it directly rather than resume it.
            let _ = child.kill();
            return Err(io::Error::other(format!(
                "could not assign the process to its job object: {assign_err}"
            )));
        }
        // SAFETY: the child handle is valid and owned by `child`.
        let pid = unsafe { GetProcessId(child.as_raw_handle() as Handle) };
        if pid == 0 {
            let _ = child.kill();
            return Err(io::Error::other(
                "could not read the process id to resume it",
            ));
        }
        if let Err(e) = resume_process_threads(pid) {
            let _ = child.kill();
            return Err(e);
        }
        Ok(child)
    }

    /// How many processes are still live in the job. `Err` if the job cannot be queried. Only
    /// the tests ask: production relies on kill-on-close rather than counting.
    #[cfg(test)]
    pub fn active_processes(&self) -> io::Result<u32> {
        let mut info = BasicAccountingInformation::default();
        let mut ret_len: u32 = 0;
        // SAFETY: `info` is the correctly sized struct for the accounting information class.
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JOB_OBJECT_BASIC_ACCOUNTING_INFORMATION,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<BasicAccountingInformation>() as u32,
                &mut ret_len,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.active_processes)
    }
}

/// Create a kill-on-close job and spawn `cmd` inside it, for a caller that needs both or neither.
/// The job must outlive the child: dropping it kills the tree.
pub fn spawn_in_new_job(cmd: &mut Command) -> io::Result<(JobObject, Child)> {
    let job = JobObject::new().ok_or_else(|| {
        io::Error::other(format!(
            "could not create a kill-on-close job object: {}",
            io::Error::last_os_error()
        ))
    })?;
    let child = job.spawn_in_job(cmd)?;
    Ok((job, child))
}

/// Resume every thread owned by `pid`. A `CREATE_SUSPENDED` child has exactly one at this point,
/// the primary, still suspended; resuming it starts the process. Documented Toolhelp APIs only,
/// rather than an undocumented ntdll call.
fn resume_process_threads(pid: u32) -> io::Result<()> {
    // SAFETY: a snapshot of all threads; the pid filter is applied per entry below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot as isize == INVALID_HANDLE_VALUE || snapshot.is_null() {
        return Err(io::Error::last_os_error());
    }
    // Close the snapshot handle on every path.
    struct SnapshotGuard(Handle);
    impl Drop for SnapshotGuard {
        fn drop(&mut self) {
            // SAFETY: owned snapshot handle, closed once.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    let _guard = SnapshotGuard(snapshot);

    let mut entry = ThreadEntry32 {
        dw_size: std::mem::size_of::<ThreadEntry32>() as u32,
        ..Default::default()
    };
    let mut resumed_any = false;
    // SAFETY: `entry` is a correctly sized THREADENTRY32.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32_owner_process_id == pid {
            // SAFETY: opening the thread by id for resume access; null on failure is handled.
            let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32_thread_id) };
            if !thread.is_null() {
                // SAFETY: `thread` is a valid handle we own. ResumeThread returns the previous
                // suspend count, or u32::MAX (DWORD -1) on failure.
                let prev = unsafe { ResumeThread(thread) };
                // SAFETY: closing the owned thread handle exactly once.
                unsafe {
                    CloseHandle(thread);
                }
                if prev != u32::MAX {
                    resumed_any = true;
                }
            }
        }
        entry.dw_size = std::mem::size_of::<ThreadEntry32>() as u32;
        // SAFETY: same entry buffer.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    if !resumed_any {
        return Err(io::Error::other(
            "could not find the process's primary thread to resume it",
        ));
    }
    Ok(())
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // Closing the last handle triggers KILL_ON_JOB_CLOSE, so anything left in the job dies
        // here, even after the direct child exited cleanly.
        // SAFETY: the handle is owned, valid, and closed exactly once.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

// The handle is owned exclusively by this value and every use goes through &self into a
// thread-safe Win32 call, so it is sound to move and share a job between threads.
unsafe impl Send for JobObject {}
unsafe impl Sync for JobObject {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    fn cmd_exe() -> Command {
        // System32's cmd.exe by full path, not a bare name resolved through search order.
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let mut cmd = Command::new(format!(r"{root}\System32\cmd.exe"));
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    fn wait_for_empty_job(job: &JobObject) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if job.active_processes().expect("query active") == 0 {
                return;
            }
            assert!(Instant::now() < deadline, "the job never emptied");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn spawn_in_job_associates_and_resumes_the_child() {
        // A CREATE_SUSPENDED child that is never resumed would never exit, so observing its real
        // exit code proves it was assigned and then resumed.
        let job = JobObject::new().expect("create job");
        let mut cmd = cmd_exe();
        cmd.args(["/C", "exit 7"]);
        let mut child = job.spawn_in_job(&mut cmd).expect("spawn in job");
        assert_eq!(child.wait().expect("wait").code(), Some(7));
        wait_for_empty_job(&job);
    }

    #[test]
    fn dropping_the_job_kills_what_is_still_inside_it() {
        // The property the app-server relies on: a child that would run for a long time dies as
        // soon as the job's last handle closes.
        let (job, mut child) = {
            let mut cmd = cmd_exe();
            cmd.args(["/C", "ping -n 60 127.0.0.1 >NUL"]);
            spawn_in_new_job(&mut cmd).expect("spawn")
        };
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "exited early"
        );
        drop(job);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait().expect("try_wait").is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "closing the job did not kill the child"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn terminate_kills_the_tree_now() {
        let (job, mut child) = {
            let mut cmd = cmd_exe();
            cmd.args(["/C", "ping -n 60 127.0.0.1 >NUL"]);
            spawn_in_new_job(&mut cmd).expect("spawn")
        };
        assert!(job.terminate());
        child.wait().expect("wait");
        wait_for_empty_job(&job);
    }

    #[test]
    fn a_program_that_does_not_exist_is_a_spawn_error() {
        let mut cmd = Command::new(r"C:\definitely\not\here\codex.exe");
        assert!(spawn_in_new_job(&mut cmd).is_err());
    }
}
