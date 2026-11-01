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
    /// Pipe creation or the first `fork`.
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
    /// The environment refused an operation that is not about starting.
    Env {
        op: &'static str,
        errno: i32,
        syscall: &'static str,
    },
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
            Error::Env { op, errno, syscall } => {
                write!(f, "{}: {} ({})", op, sys::errno_name(*errno), syscall)
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;