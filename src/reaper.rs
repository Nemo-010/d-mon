//! Reaping, and knowing whether you are allowed to.
//!
//! `PR_SET_CHILD_SUBREAPER` is **not inherited** across `fork`, and it **is
//! preserved** across `execve`. So it may only be set in the resident
//! supervisor process, never in an embedding host: doing the latter silently
//! converts the host into a reaper and leaves it holding the host's zombies.

use crate::sys;

/// What this process is allowed to reap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reap {
    /// `getpid() == 1` in this PID namespace.
    Pid1,
    /// `PR_SET_CHILD_SUBREAPER` succeeded.
    Subreaper,
    /// The kernel refused, or it was not asked for.
    Unavailable,
}

/// Try to become a child subreaper. Must run in the supervisor, after its own
/// fork and before the target is forked.
pub fn become_subreaper() -> Reap {
    // SAFETY: prctl with a fixed integer argument list.
    let rc = unsafe { sys::prctl(sys::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if rc == 0 {
        Reap::Subreaper
    } else {
        Reap::Unavailable
    }
}

/// Set `PR_SET_PDEATHSIG` in the child, and close the race where the parent is
/// already gone.
///
/// Returns `false` if the parent has already exited, in which case the caller
/// must not exec.
pub fn arm_parent_death(sig: i32) -> bool {
    // SAFETY: fixed integer arguments.
    let rc = unsafe { sys::prctl(sys::PR_SET_PDEATHSIG, sig as usize, 0, 0, 0) };
    if rc != 0 {
        return true; // best effort; not all kernels allow it
    }
    // SAFETY: getppid takes no arguments.
    unsafe { sys::getppid() != 1 }
}

/// Drain every reapable child. Terminates on `ECHILD` **or** `0`: with live
/// children `waitpid(-1, WNOHANG)` returns 0, and a loop that only stops on
/// `ECHILD` spins forever.
pub fn drain() -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    loop {
        let mut status: i32 = 0;
        // SAFETY: status is a valid out-pointer for this call.
        let pid = unsafe { sys::waitpid(-1, &mut status, sys::WNOHANG) };
        if pid > 0 {
            out.push((pid, status));
            continue;
        }
        break;
    }
    out
}