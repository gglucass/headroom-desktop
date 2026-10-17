//! Windows process plumbing that PowerShell and `taskkill` cannot give us.
//!
//! - A kill-on-close Job Object that owns the proxy tree, so the backend dies
//!   with the app however the app dies (crash, Task Manager, the updater's
//!   installer). Before this a dead app left the proxy holding port 6768 and
//!   the venv's file locks until the next launch reclaimed it, and a quit that
//!   raced a spawn orphaned the fresh child outright.
//! - CPU accounting for that job: the boot-validation "is it doing work" signal
//!   shelled out to `ps`, which Windows does not have, and the tracked pid is
//!   the idle `headroom.exe` launcher anyway, not the python doing the work.
//! - A process's image path from the kernel, for identity checks that must
//!   keep working where AppLocker/SRP blocks powershell.exe.
//! - The process table (pid, parent pid) from one kernel snapshot, plus each
//!   process's command line and a TerminateProcess, so the process sweep runs
//!   without starting PowerShell, and a tree kill runs without `taskkill`.
//!
//! Only proxy spawns join the job. The updater's installer, the relaunch
//! helper and anything else that has to outlive the app never do.

use std::os::windows::io::AsRawHandle;
use std::sync::OnceLock;

use windows_sys::Wdk::System::Threading::{
    NtQueryInformationProcess, ProcessCommandLineInformation,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, UNICODE_STRING,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};

/// The app's proxy job, created once. Stored as usize: a raw HANDLE is not
/// Send/Sync, and this one is never closed (the OS closes it when the app
/// exits, which is exactly the moment the kill-on-close has to fire). `None`
/// when creation failed; spawns then run unjobbed, as they did before.
fn proxy_job() -> Option<HANDLE> {
    static JOB: OnceLock<Option<usize>> = OnceLock::new();
    JOB.get_or_init(|| {
        // SAFETY: null attributes and name are documented as valid; the
        // returned handle is checked before use.
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            log::info!(
                "winproc: CreateJobObjectW failed ({}); proxy runs without a job",
                std::io::Error::last_os_error()
            );
            return None;
        }
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is a correctly sized, initialised struct for this
        // information class, and `job` is a live handle.
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            log::info!(
                "winproc: setting kill-on-close failed ({}); proxy runs without a job",
                std::io::Error::last_os_error()
            );
            // SAFETY: `job` is a live handle we own.
            unsafe { CloseHandle(job) };
            return None;
        }
        Some(job as usize)
    })
    .map(|job| job as HANDLE)
}

/// Put a freshly spawned proxy into the app's kill-on-close job. Its children
/// (the python a `headroom.exe` launcher starts) are created inside it too.
/// Best-effort: a failure leaves the old behaviour, where `stop_headroom` and
/// the next launch's orphan reclaim clean up, so it only logs.
pub(crate) fn adopt_into_proxy_job(child: &std::process::Child) {
    let Some(job) = proxy_job() else {
        return;
    };
    // SAFETY: both handles are live for the duration of the call: the job is
    // never closed, and `child` owns its process handle.
    let ok = unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) };
    if ok == 0 {
        log::info!(
            "winproc: assigning proxy pid {} to the job failed ({})",
            child.id(),
            std::io::Error::last_os_error()
        );
    }
}

/// Whole seconds of CPU the proxy job has used, across every process that has
/// ever run in it. Only ever compared against an earlier reading ("did it
/// grow"), so processes that already exited just add a constant. `None` when
/// there is no job or the query fails.
pub(crate) fn proxy_job_cpu_time_secs() -> Option<u64> {
    let job = proxy_job()?;
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    // SAFETY: `info` is correctly sized for this information class and `job`
    // is live; the return-length out-pointer may be null.
    let ok = unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAccountingInformation,
            (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    // FILETIME-style 100ns units.
    let total = info.TotalUserTime.max(0) as u64 + info.TotalKernelTime.max(0) as u64;
    Some(total / 10_000_000)
}

/// Full image path of `pid` straight from the kernel, or `None` when the
/// process is gone or not ours to query. Needs no PowerShell, so identity
/// checks keep working on hosts whose policy blocks powershell.exe, and it
/// is a single syscall instead of a PowerShell cold start.
pub(crate) fn process_image_path(pid: u32) -> Option<String> {
    ProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?.image_path()
}

/// An open process handle, closed on drop. While it is open the pid cannot be
/// reused, so what was checked through it is the process it terminates.
pub(crate) struct ProcessHandle(HANDLE);

impl ProcessHandle {
    fn open(pid: u32, access: u32) -> Option<Self> {
        // SAFETY: plain OpenProcess; a non-null handle is owned by the result
        // and closed once, in Drop.
        let handle = unsafe { OpenProcess(access, 0, pid) };
        (!handle.is_null()).then_some(Self(handle))
    }

    /// For the process sweep: query and terminate rights. `None` when the
    /// process is gone or not ours to kill (an elevated one).
    pub(crate) fn open_to_terminate(pid: u32) -> Option<Self> {
        Self::open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE)
    }

    pub(crate) fn image_path(&self) -> Option<String> {
        let mut buf = vec![0u16; 32_768];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` holds `len` u16s and the call writes at most that many.
        let ok = unsafe {
            QueryFullProcessImageNameW(self.0, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
        };
        (ok != 0).then(|| String::from_utf16_lossy(&buf[..len as usize]))
    }
}

/// Every running process as `(pid, parent pid)`, from one toolhelp snapshot:
/// the same table `Get-Process` reads, without starting PowerShell. `None`
/// when the snapshot fails or lists nothing (never true of a live system).
pub(crate) fn process_parents() -> Option<Vec<(u32, u32)>> {
    // SAFETY: documented flags; the handle is checked and closed below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes = Vec::new();
    // SAFETY: `entry.dwSize` is set as the API requires and `snapshot` is live.
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) };
    while more != 0 {
        processes.push((entry.th32ProcessID, entry.th32ParentProcessID));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot, &mut entry) };
    }
    // SAFETY: `snapshot` came from CreateToolhelp32Snapshot and is closed once.
    unsafe { CloseHandle(snapshot) };
    (!processes.is_empty()).then_some(processes)
}

impl ProcessHandle {
    /// Command line the process was started with, the string WMI's
    /// `Win32_Process.CommandLine` reads, or `None` when it cannot be read.
    pub(crate) fn command_line(&self) -> Option<String> {
        // A UNICODE_STRING header, then the string it points at; a command line
        // is at most 32,767 UTF-16 units. u64s keep the header pointer-aligned.
        let mut buf = vec![0u64; (std::mem::size_of::<UNICODE_STRING>() + 65_536) / 8];
        let mut written = 0u32;
        // SAFETY: `buf` is `buf.len() * 8` writable bytes, as passed.
        let status = unsafe {
            NtQueryInformationProcess(
                self.0,
                ProcessCommandLineInformation,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut written,
            )
        };
        if status < 0 {
            return None;
        }
        // SAFETY: on success the buffer starts with a UNICODE_STRING whose Buffer
        // points into `buf` and holds `Length` bytes.
        let text = unsafe {
            let header = &*(buf.as_ptr() as *const UNICODE_STRING);
            if header.Buffer.is_null() {
                return Some(String::new());
            }
            std::slice::from_raw_parts(header.Buffer, header.Length as usize / 2)
        };
        Some(String::from_utf16_lossy(text))
    }

    /// `TerminateProcess(-1)`, what `Stop-Process -Force` does, so the victim
    /// exits 0xffffffff either way. False when refused.
    pub(crate) fn terminate(&self) -> bool {
        // SAFETY: the handle is live; TerminateProcess fails cleanly without
        // the terminate right.
        unsafe { TerminateProcess(self.0, u32::MAX) != 0 }
    }
}

impl ProcessHandle {
    /// When the process started, in FILETIME ticks.
    fn created(&self) -> Option<u64> {
        let mut times = [FILETIME::default(); 4];
        let [created, exited, kernel, user] = &mut times;
        // SAFETY: four writable FILETIMEs and a live handle.
        let ok = unsafe { GetProcessTimes(self.0, created, exited, kernel, user) };
        (ok != 0)
            .then(|| (u64::from(times[0].dwHighDateTime) << 32) | u64::from(times[0].dwLowDateTime))
    }
}

/// What `taskkill /PID <pid> /T /F` does, without starting taskkill: terminate
/// `pid`, then every descendant in one toolhelp snapshot. Once the session is
/// ending (shut down, restart, sign out) Windows cannot start a process, so
/// each taskkill a quit spawned died with 0xC0000142 behind an "Application
/// Error" box that held up the shutdown until clicked, and its timeout spawned
/// another taskkill to kill it: up to four boxes per shutdown.
pub(crate) fn kill_tree(pid: u32) {
    let Some(root) = ProcessHandle::open_to_terminate(pid) else {
        return;
    };
    let processes = process_parents().unwrap_or_default();
    // Every handle stays open until the kill, so no pid can be reused between
    // the check and the TerminateProcess.
    let mut tree = vec![(pid, root.created(), root)];
    let mut next = 0;
    while next < tree.len() {
        let (parent, parent_created) = (tree[next].0, tree[next].1);
        next += 1;
        for &(child, ppid) in &processes {
            if ppid != parent || tree.iter().any(|(known, ..)| *known == child) {
                continue;
            }
            let Some(handle) = ProcessHandle::open_to_terminate(child) else {
                continue;
            };
            let created = handle.created();
            // A process older than its recorded parent was started by an
            // earlier owner of that pid, not by this tree.
            if created.zip(parent_created).is_some_and(|(c, p)| c >= p) {
                tree.push((child, created, handle));
            }
        }
    }
    for (_, _, process) in &tree {
        process.terminate();
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // SAFETY: the handle came from OpenProcess and is closed only here.
        unsafe { CloseHandle(self.0) };
    }
}
