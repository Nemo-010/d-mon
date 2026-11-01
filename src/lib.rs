//! d-mon — daemonize a program, supervise it, and reap when allowed to.
//!
//! The design contract lives in `PLAN.md`. The short version:
//!
//! * [`Command::daemonize`] returns `Ok` only when the program really did
//!   `execve`. A failed start is an [`Error`], so `?` cannot swallow it.
//! * Nothing assumes `/dev/null`, `/proc`, a shell, or a working `setsid(1)`.
//! * As PID 1, or when the kernel permits a subreaper, `d-mon` reaps.
//!
//! ```no_run
//! use d_mon::Command;
//!
//! let started = Command::new("/usr/bin/mydaemon")
//!     .arg("--serve")
//!     .log_file("/var/log/mydaemon.log")
//!     .pid_file("/run/mydaemon.pid")
//!     .daemonize()?;
//! # Ok::<(), d_mon::Error>(())
//! ```

#![forbid(unsafe_op_in_unsafe_fn)]
// Re-enabled once the public surface is finished; tracked in PLAN.md M6.
#![allow(missing_docs)]

pub mod command;
pub mod daemon;
pub mod error;
pub mod exec;
pub mod log;
pub mod pidfile;
pub mod ready;
pub mod reaper;
pub mod stdio;
pub(crate) mod supervisor;
pub mod sys;

pub use command::{Command, Id, Restart, StopPolicy};
pub use daemon::{Mode, Started};
pub use error::{Error, Fail, Result, Stage};
pub use reaper::Reap;
pub use stdio::{Sink, Stdio};

/// True when this process is the init of its PID namespace.
pub fn is_pid1() -> bool {
    sys::is_pid1()
}