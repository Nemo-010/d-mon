//! Detaching a program, and saying honestly whether it started.
//!
//! The sequence, and why each step is there:
//!
//! ```text
//! launcher ──fork──▶ intermediate ──setsid──▶ ──fork──▶ supervisor ──fork──▶ target
//!    │                                                    │
//!    └──── reads the readiness pipe until EOF ────────────┘
//! ```
//!
//! * `setsid` happens in the **intermediate**, so the process that survives is
//!   the grandchild: a session leader that is not itself a leader cannot
//!   acquire a controlling terminal, and a supervisor that could would be the
//!   `daemon(3)` bug PLAN §1 complains about. The target inherits that session,
//!   which is why it does not call `setsid` again.
//! * the intermediate exits at once, so the supervisor is reparented to init
//!   and is nobody's child to wait on.
//! * the launcher is a plain blocking reader on a pipe, and it does not return
//!   `Ok` until the target's `execve` closed the last write end.

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use crate::command::Command;
use crate::error::{Error, Fail, Result, Stage};
use crate::ready::{self, Rec, Tracker, Verdict};
use crate::supervisor::Supervisor;
use crate::sys;

/// How the program ended up running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mode {
    /// `d-mon` survived to supervise it.
    Resident { supervisor: i32, child: i32 },
}

/// Proof that the program really started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Started {
    pub mode: Mode,
}

impl Started {
    /// The pid of the supervised program.
    pub fn child(&self) -> i32 {
        match self.mode {
            Mode::Resident { child, .. } => child,
        }
    }

    /// The pid of the process that supervises it.
    pub fn supervisor(&self) -> i32 {
        match self.mode {
            Mode::Resident { supervisor, .. } => supervisor,
        }
    }
}

/// Detach. Returns `Ok` only when the target reached `execve`.
pub(crate) fn daemonize(cmd: Command) -> Result<Started> {
    let budget = cmd.ready_timeout;
    let (rfd, wfd) = pipe()?;

    // SAFETY: fork is the point of this function.
    let intermediate = unsafe { sys::fork() };
    if intermediate < 0 {
        let e = sys::errno();
        sys::close_fd(rfd);
        sys::close_fd(wfd);
        return Err(Error::Env {
            op: "fork",
            errno: e,
            syscall: "fork",
        });
    }

    if intermediate == 0 {
        // From here to `_exit` this is the intermediate, then the supervisor.
        // Neither may return through the caller's code: the launcher is still
        // waiting on the pipe, and an `atexit` handler in a half-detached
        // process is exactly the kind of surprise that ruins a start.
        sys::close_fd(rfd);

        // SAFETY: we are not a process group leader here.
        if unsafe { sys::setsid() } < 0 {
            crate::error::child_exit(wfd, Fail::new(Stage::PreFork, sys::errno(), "setsid"));
        }

        // SAFETY: fork again so the survivor is not a session leader.
        let sup = unsafe { sys::fork() };
        if sup < 0 {
            crate::error::child_exit(wfd, Fail::new(Stage::PreFork, sys::errno(), "fork"));
        }
        if sup > 0 {
            // SAFETY: the intermediate has nothing left to do.
            unsafe { sys::_exit(0) };
        }

        let code = resident(cmd, wfd);
        // SAFETY: as above.
        unsafe { sys::_exit(code) };
    }

    // The launcher.
    sys::close_fd(wfd);
    let outcome = launcher(rfd, budget, intermediate);
    sys::close_fd(rfd);
    outcome
}

/// The surviving supervisor. Returns the process exit code.
fn resident(cmd: Command, wfd: i32) -> i32 {
    let pid = unsafe { sys::getpid() };
    let pgid = unsafe { sys::getpgrp() };
    if ready::send(wfd, Rec::Super { pid, pgid }).is_err() {
        // Nobody is listening; do not start something nobody can stop.
        return 1;
    }

    let mut sup = match Supervisor::new(cmd) {
        Ok(s) => s,
        Err(e) => {
            report_failure(wfd, &e);
            return e.exit_code() as i32;
        }
    };
    sup.claim_reaping(true);

    if let Err(e) = sup.spawn(wfd) {
        report_failure(wfd, &e);
        return e.exit_code() as i32;
    }
    // Close discipline: the child is now the only holder of the write end, so
    // `execve` — which closes it via O_CLOEXEC — is what produces EOF.
    sys::close_fd(wfd);

    // And let go of the caller's stdio: the child has its own copies, and a
    // supervisor holding the write end of the caller's pipe would make
    // `out=$(d-mon prog)` wait forever.
    crate::stdio::detach_own_stdio();

    match sup.run() {
        Ok(st) => st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0)),
        Err(e) => e.exit_code() as i32,
    }
}

fn report_failure(wfd: i32, e: &Error) {
    let rec = match e {
        Error::Spawn { stage, errno, .. } => Rec::Fail {
            stage: *stage,
            errno: *errno,
        },
        // A locked pidfile is not a start failure: it is a refusal to start a
        // second copy, and it has its own exit code.
        Error::AlreadyRunning { pid, .. } => Rec::Busy {
            pid: pid.unwrap_or(0),
        },
        Error::Env { errno, .. } => Rec::Fail {
            stage: Stage::PreFork,
            errno: *errno,
        },
        Error::Unconfirmed { .. } => Rec::Fail {
            stage: Stage::PreFork,
            errno: sys::EAGAIN,
        },
    };
    let _ = ready::send(wfd, rec);
}

fn launcher(rfd: i32, budget: Duration, intermediate: i32) -> Result<Started> {
    let mut t = Tracker::default();
    let verdict = match ready::await_records(rfd, budget, &mut t) {
        Ok(()) => Some(t.on_eof()),
        Err(e) if e == sys::EAGAIN => None,
        Err(e) => {
            reap_quietly(intermediate);
            return Err(Error::Env {
                op: "readiness",
                errno: e,
                syscall: "read",
            });
        }
    };

    reap_quietly(intermediate);

    match verdict {
        Some(Verdict::Started {
            supervisor,
            child,
            ..
        }) => {
            // REC_HELLO proves the last step was reached; EOF proves execve
            // closed the pipe. A pid that is already gone was exec'd and
            // exited before we looked, which is a failed start, not a success.
            if sys::alive(child) {
                Ok(Started {
                    mode: Mode::Resident { supervisor, child },
                })
            } else {
                Err(Fail::new(Stage::Exec, sys::ESRCH, "execve").into_error("".into()))
            }
        }
        Some(Verdict::Failed { stage, errno }) => Err(Fail::new(
            stage,
            errno,
            launcher_syscall(stage),
        )
        .into_error(Default::default())),
        Some(Verdict::Busy { pid }) => Err(Error::AlreadyRunning {
            path: Default::default(),
            pid,
        }),
        Some(Verdict::Unconfirmed) | None => {
            // We may not have seen REC_SUPER yet, so we cannot rely on the
            // reported group. What we always know is the intermediate: it called
            // `setsid`, so it is both the session leader and the process group
            // of everything the attempt created. Killing that group is what
            // stops the retry-and-get-two-daemons failure.
            let mut targets = vec![intermediate];
            if let Some(pg) = t.pgid {
                targets.push(pg);
            }
            if let Some(sup) = t.supervisor {
                targets.push(sup);
            }
            let killed = kill_groups(&targets);
            Err(Error::Unconfirmed {
                waited: budget,
                killed,
            })
        }
    }
}

fn launcher_syscall(stage: Stage) -> &'static str {
    match stage {
        Stage::PreFork => "setup",
        Stage::Open => "open",
        Stage::Chdir => "chdir",
        Stage::Privileges => "setuid",
        Stage::Exec => "execve",
    }
}

/// Stop everything the attempt created, politely first.
///
/// Each id is tried as a process group *and* as a pid: before `setsid` lands the
/// group does not exist, and afterwards the leader may already be a zombie.
fn kill_groups(ids: &[i32]) -> bool {
    let mut sent = false;
    for &id in ids {
        if id <= 1 {
            continue;
        }
        // SAFETY: a negative pid addresses a process group, a positive one a
        // single process; `id` is one we forked.
        unsafe {
            sent |= sys::kill(-id, sys::SIGTERM) == 0;
            sent |= sys::kill(id, sys::SIGTERM) == 0;
        }
    }
    let until = Instant::now() + Duration::from_millis(400);
    while Instant::now() < until {
        let all_gone = ids.iter().all(|&id| {
            // SAFETY: signal 0 only asks whether the target still exists.
            unsafe { sys::kill(-id, 0) < 0 && sys::kill(id, 0) < 0 }
        });
        if all_gone {
            return sent;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    for &id in ids {
        if id <= 1 {
            continue;
        }
        // SAFETY: as above; the group did not take the polite signal.
        unsafe {
            sys::kill(-id, sys::SIGKILL);
            sys::kill(id, sys::SIGKILL);
        }
    }
    sent
}

fn reap_quietly(pid: i32) {
    // SAFETY: waiting on our own child.
    unsafe {
        sys::waitpid(pid, std::ptr::null_mut(), 0);
    }
}

fn pipe() -> Result<(i32, i32)> {
    let mut fds = [0 as i32; 2];
    // SAFETY: two integers of writable storage.
    let rc = unsafe { sys::pipe2(fds.as_mut_ptr(), sys::O_CLOEXEC) };
    if rc < 0 {
        return Err(Error::Env {
            op: "readiness pipe",
            errno: sys::errno(),
            syscall: "pipe2",
        });
    }
    Ok((fds[0], fds[1]))
}

pub(crate) fn run(cmd: Command) -> Result<ExitStatus> {
    crate::supervisor::run_foreground(cmd)
}