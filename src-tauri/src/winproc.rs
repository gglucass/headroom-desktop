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
//!
//! Only proxy spawns join the job. The updater's installer, the relaunch
//! helper and anything else that has to outlive the app never do.

use std::os::windows::io::AsRawHandle;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
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
    // SAFETY: OpenProcess with a limited query right; the handle is checked
    // and closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buf = vec![0u16; 32_768];
    let mut len = buf.len() as u32;
    // SAFETY: `buf` holds `len` u16s and the call writes at most that many.
    let ok = unsafe {
        QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len)
    };
    // SAFETY: `handle` came from OpenProcess above and is closed exactly once.
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}
