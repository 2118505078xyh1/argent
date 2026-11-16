//! Process-level confinement for tools.
//!
//! This is deliberately honest about what it is: a **process sandbox**, not a
//! filesystem jail. On Windows every tool child is put in a Job Object with:
//!
//! * kill-on-close, so a crashed agent cannot leave a compiler running;
//! * an active-process ceiling, so a fork bomb cannot take the desktop down;
//! * per-process and per-job memory ceilings;
//! * UI restrictions (no desktop, clipboard or global atoms).
//!
//! Path and command policy still live in `argent-security`; this layer answers
//! "what happens if a permitted command goes wrong", not "which paths may be
//! read". A real filesystem sandbox needs AppContainer/Seatbelt/namespace
//! support and is tracked separately.

use crate::error::{Error, Result};

#[cfg(windows)]
mod imp {
    use super::{Error, Result};
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    #[repr(C)]
    struct BasicLimit {
        per_process_user_time: i64,
        per_job_user_time: i64,
        limit_flags: u32,
        minimum_working_set: usize,
        maximum_working_set: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct IoCounters {
        read_operations: u64,
        write_operations: u64,
        other_operations: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }

    #[repr(C)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory: usize,
        peak_job_memory: usize,
    }

    #[repr(C)]
    struct BasicUiRestrictions {
        ui_restrictions_class: u32,
    }

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_BASIC_UI_RESTRICTIONS: i32 = 4;
    const JOB_OBJECT_LIMIT_ACTIVE_PROCESS: u32 = 0x0000_0008;
    const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x0000_0100;
    const JOB_OBJECT_LIMIT_JOB_MEMORY: u32 = 0x0000_0200;
    const JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION: u32 = 0x0000_0400;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const JOB_OBJECT_UILIMIT_HANDLES: u32 = 0x0000_0001;
    const JOB_OBJECT_UILIMIT_READCLIPBOARD: u32 = 0x0000_0002;
    const JOB_OBJECT_UILIMIT_WRITECLIPBOARD: u32 = 0x0000_0004;
    const JOB_OBJECT_UILIMIT_GLOBALATOMS: u32 = 0x0000_0020;
    const JOB_OBJECT_UILIMIT_DESKTOP: u32 = 0x0000_0040;

    const PROCESS_MEMORY_LIMIT: usize = 8 * 1024 * 1024 * 1024;
    const JOB_MEMORY_LIMIT: usize = 16 * 1024 * 1024 * 1024;
    const ACTIVE_PROCESS_LIMIT: u32 = 64;

    extern "system" {
        fn CreateJobObjectW(attributes: *mut core::ffi::c_void, name: *const u16) -> isize;
        fn SetInformationJobObject(
            job: isize,
            class: i32,
            information: *const core::ffi::c_void,
            length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: isize, process: isize) -> i32;
        fn CloseHandle(handle: isize) -> i32;
        fn GetLastError() -> u32;
    }

    /// Keeps the job handle alive while the child runs. Closing it is the
    /// kill-on-close action, so drop this only after the child is reaped.
    pub struct JobGuard {
        handle: isize,
    }

    impl Drop for JobGuard {
        fn drop(&mut self) {
            if self.handle != 0 {
                unsafe {
                    CloseHandle(self.handle);
                }
            }
        }
    }

    pub fn attach(child: &Child) -> Result<JobGuard> {
        unsafe {
            let job = CreateJobObjectW(core::ptr::null_mut(), core::ptr::null());
            if job == 0 {
                return Err(Error::io(format!(
                    "sandbox: CreateJobObject failed ({})",
                    GetLastError()
                )));
            }

            let mut limits: ExtendedLimit = core::mem::zeroed();
            limits.basic.limit_flags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY
                | JOB_OBJECT_LIMIT_JOB_MEMORY
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
                | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            limits.basic.active_process_limit = ACTIVE_PROCESS_LIMIT;
            limits.process_memory_limit = PROCESS_MEMORY_LIMIT;
            limits.job_memory_limit = JOB_MEMORY_LIMIT;

            let set = SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                &limits as *const ExtendedLimit as *const core::ffi::c_void,
                core::mem::size_of::<ExtendedLimit>() as u32,
            );
            if set == 0 {
                let error = GetLastError();
                CloseHandle(job);
                return Err(Error::io(format!(
                    "sandbox: SetInformationJobObject failed ({})",
                    error
                )));
            }

            let ui = BasicUiRestrictions {
                ui_restrictions_class: JOB_OBJECT_UILIMIT_HANDLES
                    | JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                    | JOB_OBJECT_UILIMIT_GLOBALATOMS
                    | JOB_OBJECT_UILIMIT_DESKTOP,
            };
            let ui_set = SetInformationJobObject(
                job,
                JOB_OBJECT_BASIC_UI_RESTRICTIONS,
                &ui as *const BasicUiRestrictions as *const core::ffi::c_void,
                core::mem::size_of::<BasicUiRestrictions>() as u32,
            );
            if ui_set == 0 {
                let error = GetLastError();
                CloseHandle(job);
                return Err(Error::io(format!(
                    "sandbox: SetInformationJobObject(UI) failed ({})",
                    error
                )));
            }

            let assigned = AssignProcessToJobObject(job, child.as_raw_handle() as isize);
            if assigned == 0 {
                let error = GetLastError();
                CloseHandle(job);
                return Err(Error::io(format!(
                    "sandbox: AssignProcessToJobObject failed ({})",
                    error
                )));
            }

            Ok(JobGuard { handle: job })
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::Result;
    use std::process::Child;

    /// On Unix the process group is the confinement mechanism today; there is
    /// no Job Object to attach to.
    pub struct JobGuard;

    pub fn attach(_child: &Child) -> Result<JobGuard> {
        Ok(JobGuard)
    }
}

pub use imp::{attach, JobGuard};

/// Whether the operator asked for the process sandbox.
pub fn enabled_from_env() -> bool {
    matches!(
        std::env::var("ARGENT_SANDBOX").as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("guarded")
    )
}