//! A process and everything it starts, held in a Job Object that kills them
//! all when it is closed or terminated, and a handle to a director's wake
//! event opened with the rights the shim asks for.
#![allow(unsafe_code)]

use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    OpenEventW, OpenThread, ResumeThread, SetEvent, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
};

/// An unnamed job that kills every process in it when its handle closes
/// (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`). Processes a member starts are
/// members too, so a launcher's children go with it.
pub struct KillOnCloseJob {
    job: HANDLE,
}

// SAFETY: a job handle may be used from any thread.
unsafe impl Send for KillOnCloseJob {}
unsafe impl Sync for KillOnCloseJob {}

impl KillOnCloseJob {
    pub fn new() -> io::Result<KillOnCloseJob> {
        // SAFETY: FFI; null attributes and name make an unnamed job. The
        // handle is owned by the returned value or closed here.
        unsafe {
            let job = CreateJobObjectW(core::ptr::null(), core::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = KillOnCloseJob { job };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = core::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job.job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    /// Spawns `cmd` inside the job: created suspended, assigned, then
    /// resumed, so it cannot start a process outside the job first. Sets
    /// `cmd`'s creation flags to `CREATE_SUSPENDED`.
    pub fn spawn(&self, cmd: &mut Command) -> io::Result<Child> {
        let mut child = cmd.creation_flags(CREATE_SUSPENDED).spawn()?;
        let started = self
            .assign(&child)
            .and_then(|()| resume_threads(child.id()));
        if let Err(e) = started {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(child)
    }

    fn assign(&self, child: &Child) -> io::Result<()> {
        // SAFETY: FFI; both handles are live for the call.
        if unsafe { AssignProcessToJobObject(self.job, child.as_raw_handle() as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Kills every process in the job now.
    pub fn terminate(&self) {
        // SAFETY: FFI on a live job handle.
        unsafe {
            TerminateJobObject(self.job, 1);
        }
    }
}

impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        // SAFETY: closed exactly once; closing it kills what is left in it.
        unsafe {
            CloseHandle(self.job);
        }
    }
}

/// Resumes every thread of process `pid` (one, for a process created
/// suspended).
fn resume_threads(pid: u32) -> io::Result<()> {
    // SAFETY: FFI. The snapshot and thread handles are closed here; the
    // entry is sized as Thread32First requires.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = core::mem::zeroed();
        entry.dwSize = core::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        let mut ok = Thread32First(snap, &mut entry) != 0;
        while ok {
            if entry.th32OwnerProcessID == pid {
                let t = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if !t.is_null() {
                    if ResumeThread(t) != u32::MAX {
                        resumed += 1;
                    }
                    CloseHandle(t);
                }
            }
            ok = Thread32Next(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
        if resumed == 0 {
            return Err(io::Error::other(format!(
                "no thread of process {pid} could be resumed"
            )));
        }
        Ok(())
    }
}

/// `EVENT_MODIFY_STATE`: enough to `SetEvent`, which is all the shim asks
/// for (`aether-hooks`'s `director::connect_source`).
const EVENT_MODIFY_STATE: u32 = 0x0002;

/// A director's wake event, opened as the shim opens `VFS_SERVER_EV`.
pub struct WakeEvent {
    ev: HANDLE,
}

// SAFETY: SetEvent may be called on the handle from any thread.
unsafe impl Send for WakeEvent {}
unsafe impl Sync for WakeEvent {}

impl WakeEvent {
    /// Opens the named event with `EVENT_MODIFY_STATE` only.
    pub fn open(name: &str) -> io::Result<WakeEvent> {
        let wide: Vec<u16> = name.encode_utf16().chain(core::iter::once(0)).collect();
        // SAFETY: FFI; `wide` is NUL-terminated and lives for the call.
        let ev = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, wide.as_ptr()) };
        if ev.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(WakeEvent { ev })
    }

    /// Signals the event.
    pub fn set(&self) {
        // SAFETY: FFI on a live handle.
        unsafe {
            SetEvent(self.ev);
        }
    }
}

impl Drop for WakeEvent {
    fn drop(&mut self) {
        // SAFETY: closed exactly once.
        unsafe {
            CloseHandle(self.ev);
        }
    }
}
