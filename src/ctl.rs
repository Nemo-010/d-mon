//! Control verbs: ask a running supervisor what it is doing, or tell it
//! something.
//!
//! # Why the lock, and not the pid
//!
//! A pidfile is a claim, not evidence. Between "the supervisor wrote 4711" and
//! "kill 4711", the pid can be recycled by any other process on the machine —
//! and then a `stop` kills a stranger. Testing `kill(pid, 0)` does not help: it
//! answers "somebody has that pid", which is exactly what is in doubt.
//!
//! The supervisor holds `flock(LOCK_EX)` on the pidfile for its whole life, and
//! the kernel drops that lock when the process dies, however it dies. So the
//! verbs here try to take the lock themselves:
//!
//! * `LOCK_EX` succeeds → no holder → the file is stale, and the pid in it is
//!   not to be trusted or signalled;
//! * `EWOULDBLOCK` → a live holder exists, and because the holder is the one
//!   that wrote the file, the pid inside is that holder's.
//!
//! That is the whole identity check: no `/proc`, no start-time comparison, no
//! heuristics.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::sys;

/// What a pidfile says about the process behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// A live supervisor holds the lock.
    Running {
        supervisor: i32,
        /// The delegate, when the supervisor has one right now.
        child: Option<i32>,
    },
    /// Nothing holds the lock.
    Stopped { reason: Stopped },
    /// The lock is held but the pid inside could not be read: the holder is
    /// rewriting the file, or something else holds it.
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stopped {
    /// There is no pidfile at that path.
    NoPidfile,
    /// The file exists, but no live process holds its lock: a leftover.
    Stale,
    /// The lock is held, but the recorded pid is not signalable.
    Vanished,
}

impl State {
    pub fn is_running(&self) -> bool {
        matches!(self, State::Running { .. })
    }
}

/// What to say about this pidfile.
pub fn status(pidfile: &Path) -> Result<State> {
    let mut file = match open_read(pidfile) {
        Ok(f) => f,
        Err(e) if e.raw_os_error() == Some(sys::ENOENT) => {
            return Ok(State::Stopped {
                reason: Stopped::NoPidfile,
            })
        }
        Err(e) => {
            return Err(Error::Env {
                op: "pidfile",
                errno: e.raw_os_error().unwrap_or(sys::EIO),
                syscall: "open",
            })
        }
    };

    if !lock_held_by_other(&file) {
        return Ok(State::Stopped {
            reason: Stopped::Stale,
        });
    }

    // A holder exists. Its rewrite is a truncate followed by a write, so a read
    // that lands between them sees nothing; ask again rather than give up.
    for _ in 0..40 {
        if let Some((supervisor, child)) = read_pids(&mut file) {
            if sys::alive(supervisor) {
                return Ok(State::Running { supervisor, child });
            }
            return Ok(State::Stopped {
                reason: Stopped::Vanished,
            });
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(State::Uncertain)
}

/// Send `sig` to the supervisor recorded in `pidfile`.
///
/// Returns the state that authorised the signal, so the caller can say what it
/// did. A stale pidfile is refused, not signalled.
pub fn signal(pidfile: &Path, sig: i32) -> Result<State> {
    let state = status(pidfile)?;
    match &state {
        State::Running { supervisor, .. } => {
            // SAFETY: the pid was read from a locked pidfile whose holder is
            // live; that is the identification this module promises.
            if unsafe { sys::kill(*supervisor, sig) } < 0 {
                let e = sys::errno();
                if e != sys::ESRCH {
                    return Err(Error::Env {
                        op: "signal",
                        errno: e,
                        syscall: "kill",
                    });
                }
            }
            Ok(state)
        }
        _ => Ok(state),
    }
}

/// Send `sig`, then wait until the supervisor is really gone.
///
/// `Ok(true)` means the lock was released: the process is gone, not merely
/// asked. That distinction is the reason this verb exists as something other
/// than `signal`.
pub fn stop(pidfile: &Path, sig: i32, wait: Duration) -> Result<(State, bool)> {
    let state = signal(pidfile, sig)?;
    if !state.is_running() {
        return Ok((state, true));
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        match status(pidfile)? {
            State::Running { .. } => std::thread::sleep(Duration::from_millis(25)),
            _ => return Ok((state, true)),
        }
    }
    Ok((state, false))
}

fn open_read(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(sys::O_NOFOLLOW)
        .open(path)
}

/// True when somebody other than us holds the lock. The probe lock is released
/// again immediately, so this is safe to call on a live supervisor.
fn lock_held_by_other(file: &File) -> bool {
    // SAFETY: flock on a valid descriptor.
    let rc = unsafe { sys::flock(file.as_raw_fd(), sys::LOCK_EX | sys::LOCK_NB) };
    if rc == 0 {
        // We got it, so nobody else had it. Release straight away.
        // SAFETY: as above, releasing what we just took.
        unsafe {
            sys::flock(file.as_raw_fd(), sys::LOCK_UN);
        }
        return false;
    }
    let e = sys::errno();
    e == sys::EWOULDBLOCK || e == sys::EAGAIN
}

fn read_pids(file: &mut File) -> Option<(i32, Option<i32>)> {
    let _ = file.seek(SeekFrom::Start(0));
    let mut s = String::new();
    let _ = file.read_to_string(&mut s);
    let mut lines = s.lines().filter(|l| !l.trim().is_empty());
    let supervisor: i32 = lines.next()?.trim().parse().ok()?;
    if supervisor <= 1 {
        return None;
    }
    let child = lines.next().and_then(|l| l.trim().parse().ok());
    Some((supervisor, child))
}

/// A pidfile path, or an error that says which verb wanted one.
pub fn require(pidfile: Option<&PathBuf>, verb: &'static str) -> Result<PathBuf> {
    match pidfile {
        Some(p) => Ok(p.clone()),
        None => Err(Error::Env {
            op: verb,
            errno: sys::EINVAL,
            syscall: "no --pidfile given; there is nothing to look at",
        }),
    }
}