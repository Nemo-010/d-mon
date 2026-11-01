//! Errors, and where they happened.

use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use crate::sys;

/// Where a start failed. `#[non_exhaustive]` so adding one is not a break.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    /// Pipe creation, resolution, or the first `fork`.
    PreFork,
    /// Opening the log, the pidfile, or a `/dev/null` substitute.
    Open,
    /// `chdir` to the requested directory.
    Chdir,
    /// `setgroups` / `setgid` / `setuid` / capability scrub.
    Privileges,
    /// The final `execve`.
    Exec,
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Stage::PreFork => "pre-fork",
            Stage::Open => "open",
            Stage::Chdir => "chdir",
            Stage::Privileges => "privileges",
            Stage::Exec => "exec",
        };
        f.write_str(s)
    }
}

/// A failure with its location but without the program it belongs to. The
/// builder produces these; [`Command`](crate::Command) attaches the name.
#[derive(Debug, Clone)]
pub struct Fail {
    pub stage: Stage,
    pub errno: i32,
    pub syscall: &'static str,
    pub path: Option<PathBuf>,
}

impl Fail {
    pub fn new(stage: Stage, errno: i32, syscall: &'static str) -> Fail {
        Fail {
            stage,
            errno,
            syscall,
            path: None,
        }
    }

    pub fn with_path(mut self, p: PathBuf) -> Fail {
        self.path = Some(p);
        self
    }

    pub fn into_error(self, program: OsString) -> Error {
        Error::Spawn {
            program,
            stage: self.stage,
            errno: self.errno,
            syscall: self.syscall,
            path: self.path,
        }
    }
}

/// Everything that can go wrong.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The program did not start. Carries enough to debug it at 3am.
    Spawn {
        program: OsString,
        stage: Stage,
        errno: i32,
        syscall: &'static str,
        path: Option<PathBuf>,
    },
    /// The start was neither confirmed nor refused within the budget. Any
    /// process created by the attempt has already been killed.
    Unconfirmed { waited: Duration, killed: bool },
    /// A pidfile for this program is already locked by a live supervisor.
    AlreadyRunning { path: PathBuf, pid: Option<i32> },
    /// The environment refused an operation that is not about starting.
    Env {
        op: &'static str,
        errno: i32,
        syscall: &'static str,
    },
}

impl Error {
    /// Attach the program name to a spawn failure reported by a process that
    /// did not have it. Leaves a name that is already set alone.
    pub fn named(self, program: &std::ffi::OsStr) -> Error {
        match self {
            Error::Spawn {
                program: ref p,
                stage,
                errno,
                syscall,
                ref path,
            } if p.is_empty() => Error::Spawn {
                program: program.to_os_string(),
                stage,
                errno,
                syscall,
                path: path.clone(),
            },
            other => other,
        }
    }

    /// The process exit code this error maps to.
    pub fn exit_code(&self) -> u8 {
        match self {
            Error::Spawn { .. } | Error::Env { .. } => 1,
            Error::Unconfirmed { .. } => 2,
            Error::AlreadyRunning { .. } => 3,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Spawn {
                program,
                stage,
                errno,
                syscall,
                path,
            } => {
                write!(
                    f,
                    "{}: failed at {}: {} ({})",
                    program.to_string_lossy(),
                    stage,
                    sys::errno_name(*errno),
                    syscall
                )?;
                if let Some(p) = path {
                    write!(f, ": {}", p.display())?;
                }
                Ok(())
            }
            Error::Unconfirmed { waited, killed } => write!(
                f,
                "start not confirmed within {:?}; {}",
                waited,
                if *killed {
                    "process group killed"
                } else {
                    "nothing to kill"
                }
            ),
            Error::AlreadyRunning { path, pid } => {
                write!(f, "already running (pidfile {})", path.display())?;
                if let Some(p) = pid {
                    write!(f, ", pid {p}")?;
                }
                Ok(())
            }
            Error::Env {
                op,
                errno,
                syscall,
            } => {
                write!(f, "{}: {} ({})", op, sys::errno_name(*errno), syscall)
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Terminate the forked child, reporting why on the readiness pipe.
///
/// This is the last resort of the child half: the record is the only way the
/// launcher learns that setup died, and `_exit` is the only way to leave
/// without running the host's atexit handlers inside a half-configured child.
pub fn child_exit(ready_fd: i32, fail: Fail) -> ! {
    if ready_fd >= 0 {
        let _ = crate::ready::send(
            ready_fd,
            crate::ready::Rec::Fail {
                stage: fail.stage,
                errno: fail.errno,
            },
        );
    }
    // SAFETY: _exit is async-signal-safe and cannot fail.
    unsafe { sys::_exit(127) }
}