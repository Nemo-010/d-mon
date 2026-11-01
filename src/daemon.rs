//! Detaching a program.
//!
//! Skeleton: the protocol types are final (see [`crate::ready`]); the fork
//! sequence lands in M2.

use std::process::ExitStatus;

use crate::command::Command;
use crate::error::{Error, Result, Stage};

/// How the program ended up running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `d-mon` survived to supervise it.
    Resident { supervisor: i32, child: i32 },
    /// `d-mon` exec'd itself away; only the program remains.
    Replaced { child: i32 },
}

/// Proof that the program really started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Started {
    pub mode: Mode,
}

pub(crate) fn daemonize(cmd: Command) -> Result<Started> {
    // M2: pipe2, fork, setsid, fork, close discipline, readiness read,
    // kill the group on timeout.
    let _ = cmd;
    Err(Error::Spawn {
        program: Default::default(),
        stage: Stage::PreFork,
        errno: crate::sys::ENOSYS,
        syscall: "daemonize",
        path: None,
    })
}

pub(crate) fn run(cmd: Command) -> Result<ExitStatus> {
    // M3: install handlers, set subreaper only when asked, spawn, supervise.
    let _ = cmd;
    Err(Error::Env {
        op: "run",
        errno: crate::sys::ENOSYS,
        syscall: "supervise",
    })
}