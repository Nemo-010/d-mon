//! The supervisor: one `poll(2)` loop that owns exactly the processes it
//! created.
//!
//! Ownership is explicit. `waitpid(-1, ...)` is used **only** when this process
//! is PID 1 or has deliberately become a child subreaper; otherwise the loop
//! waits on the pid it forked. That distinction exists because `waitpid(-1)`
//! from a library caller silently steals the host's children, and because
//! `PR_SET_CHILD_SUBREAPER` survives `execve` — setting it in an embedding
//! process would quietly convert the host into a reaper.

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use crate::command::{Command, Restart};
use crate::error::{Error, Fail, Result, Stage};
use crate::exec::ExecPlan;
use crate::log::Log;
use crate::pidfile::Pidfile;
use crate::reaper::Reap;
use crate::stdio::{self, Drain};
use crate::sys;

/// Write end of the self-pipe, for the signal handler. `-1` when unset.
static SIGW: AtomicI32 = AtomicI32::new(-1);

/// A signal handler belongs to the process, not to a `Supervisor`, so exactly
/// one supervisor may be live in a process at a time. Two of them would write
/// to whichever self-pipe the static last named, and one would sleep through a
/// signal meant for it. Refusing the second is better than that silent bug.
static ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn claim_process() -> Result<()> {
    if ACTIVE.swap(true, Ordering::AcqRel) {
        return Err(Error::Env {
            op: "supervisor",
            errno: sys::EBUSY,
            syscall: "another supervisor is already running in this process",
        });
    }
    Ok(())
}

extern "C" fn on_signal(sig: i32) {
    let fd = SIGW.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = [sig as u8];
        // SAFETY: write is async-signal-safe. A full pipe means the loop is
        // about to drain it anyway; the signal is coalesced, not lost.
        unsafe {
            sys::write(fd, byte.as_ptr() as *const _, 1);
        }
    }
}

const HANDLED: [i32; 7] = [
    sys::SIGHUP,
    sys::SIGINT,
    sys::SIGQUIT,
    sys::SIGTERM,
    sys::SIGCHLD,
    sys::SIGUSR1,
    sys::SIGUSR2,
];

/// What the loop is currently doing about the main child.
enum Phase {
    /// Child is running; nothing has asked us to stop.
    Running,
    /// We asked the child to stop and are waiting for the grace period.
    Stopping { deadline: Instant, killed: bool },
    /// Waiting to respawn after a crash, honouring the backoff.
    Backoff { at: Instant },
}

pub(crate) struct Supervisor {
    cmd: Command,
    log: Option<Log>,
    pidfile: Option<Pidfile>,
    sig_r: i32,
    sig_w: i32,
    drains: Vec<Drain>,
    child: Option<i32>,
    phase: Phase,
    attempts: u32,
    last_status: Option<i32>,
    /// Somebody asked us to stop. The restart policy must not overrule it, or a
    /// `-r` supervisor answers SIGTERM by starting the child again.
    stop_requested: bool,
    reap: Reap,
    pid1: bool,
    /// Restored on drop so `run()` leaves the host's signal state as it found it.
    old_handlers: Vec<(i32, usize)>,
}

impl Supervisor {
    pub fn new(cmd: Command) -> Result<Supervisor> {
        claim_process()?;
        let log = match &cmd.log_file {
            Some(p) => Some(Log::open(p).map_err(|f| f.into_error(cmd.program.clone()))?),
            None => None,
        };

        let pid = unsafe { sys::getpid() };
        let pidfile = match &cmd.pid_file {
            Some(p) => Some(Pidfile::acquire(p, pid)?),
            None => None,
        };

        let (sig_r, sig_w) = sys::self_pipe()
            .map_err(|e| Error::Env {
                op: "self-pipe",
                errno: e,
                syscall: "pipe2",
            })?;

        Ok(Supervisor {
            pid1: sys::is_pid1(),
            cmd,
            log,
            pidfile,
            sig_r,
            sig_w,
            drains: Vec::new(),
            child: None,
            phase: Phase::Running,
            attempts: 0,
            last_status: None,
            stop_requested: false,
            reap: Reap::Unavailable,
            old_handlers: Vec::new(),
        })
    }

    /// Install handlers. Called after the child is forked, so the child never
    /// inherits them and has nothing to undo.
    pub fn install_handlers(&mut self) {
        if !self.old_handlers.is_empty() {
            return;
        }
        SIGW.store(self.sig_w, Ordering::Relaxed);
        for sig in HANDLED {
            // SAFETY: a handler that only calls write(2).
            let old = unsafe { sys::signal(sig, on_signal as *const () as usize) };
            self.old_handlers.push((sig, old));
        }
    }

    fn restore_handlers(&mut self) {
        for (sig, old) in self.old_handlers.drain(..) {
            // SAFETY: restoring a disposition `signal` returned earlier.
            unsafe {
                sys::signal(sig, old);
            }
        }
        SIGW.store(-1, Ordering::Relaxed);
    }

    /// Become a subreaper if this process is allowed to.
    ///
    /// Only the detached supervisor and an explicit `reap(true)` take this
    /// path. `run()` never does it behind the caller's back.
    pub fn claim_reaping(&mut self, allow: bool) {
        self.reap = if self.pid1 {
            Reap::Pid1
        } else if allow {
            crate::reaper::become_subreaper()
        } else {
            Reap::Unavailable
        };
    }

    /// Fork the target and, if `ready_fd >= 0`, leave the readiness pipe to the
    /// caller. Returns the child pid.
    pub(crate) fn spawn(&mut self, ready_fd: i32) -> Result<i32> {
        let mut planned = stdio::plan(&self.cmd, self.log.as_ref())
            .map_err(|f| f.into_error(self.cmd.program.clone()))?;
        let plan = ExecPlan::build(&self.cmd, planned.fds, ready_fd)
            .map_err(|f| f.into_error(self.cmd.program.clone()))?;

        // SAFETY: fork is the point of this module.
        let pid = unsafe { sys::fork() };
        if pid < 0 {
            let e = sys::errno();
            drop(planned);
            return Err(Error::Env {
                op: "fork",
                errno: e,
                syscall: "fork",
            });
        }

        if pid == 0 {
            // Child. No allocation, no unwinding, no atexit.
            // SAFETY: we are the freshly forked child and the plan is complete.
            unsafe { plan.exec() }
        }

        // Parent: the child's ends are ours to close, or the drains never see
        // EOF. This is the close discipline the readiness protocol depends on.
        planned.close_child_ends();
        for d in &mut planned.drains {
            let _ = sys::set_nonblock(d.fd);
        }
        self.drains.append(&mut planned.drains);
        self.child = Some(pid);
        self.phase = Phase::Running;
        Ok(pid)
    }

    /// Wait for the first start to be confirmed or refused.
    pub fn await_start(&mut self, ready_fd: i32, budget: Duration) -> Result<()> {
        use crate::ready::{self, Tracker, Verdict};

        let mut t = Tracker::default();
        // This process is the supervisor, so the record the launcher would have
        // received from a detached supervisor is known here already. Without
        // it a successful start would look like "EOF with no record".
        t.feed(crate::ready::Rec::Super {
            pid: unsafe { sys::getpid() },
            pgid: unsafe { sys::getpgrp() },
        });
        match ready::await_records(ready_fd, budget, &mut t) {
            Ok(()) => {}
            Err(e) if e == sys::EAGAIN => {
                self.abort_start();
                return Err(Error::Unconfirmed {
                    waited: budget,
                    killed: true,
                });
            }
            Err(e) => {
                self.abort_start();
                return Err(Error::Env {
                    op: "readiness",
                    errno: e,
                    syscall: "read",
                });
            }
        }

        match t.on_eof() {
            Verdict::Started { child, .. } => {
                self.child = Some(child);
                Ok(())
            }
            Verdict::Failed { stage, errno } => {
                self.abort_start();
                Err(Fail::new(stage, errno, stage_syscall(stage))
                    .into_error(self.cmd.program.clone()))
            }
            Verdict::Unconfirmed => {
                self.abort_start();
                Err(Error::Unconfirmed {
                    waited: budget,
                    killed: true,
                })
            }
            Verdict::Busy { .. } => {
                self.abort_start();
                Err(Error::AlreadyRunning {
                    path: Default::default(),
                    pid: None,
                })
            }
        }
    }

    /// Kill whatever this attempt created. The stop signal first, so a
    /// supervisor that got as far as its loop can clean its pidfile up, then
    /// `SIGKILL`, and reap so nothing is left as a zombie the caller cannot see.
    fn abort_start(&mut self) {
        let child = self.child.take();
        if let Some(pid) = child {
            for sig in [self.cmd.stop.signal, sys::SIGKILL] {
                // SAFETY: signalling a pid we forked.
                unsafe {
                    sys::kill(-pid, sig);
                    sys::kill(pid, sig);
                }
                let until = Instant::now() + Duration::from_millis(300);
                loop {
                    match sys::wait(pid, sys::WNOHANG) {
                        Ok((0, _)) => {}
                        Ok(_) => return,
                        Err(sys::ECHILD) => return,
                        Err(_) => return,
                    }
                    if Instant::now() > until {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    /// The main loop. Returns when the child is gone for good and the policy
    /// says not to restart it.
    pub fn run(&mut self) -> Result<ExitStatus> {
        self.install_handlers();

        loop {
            let timeout = self.next_timeout();
            self.wait_events(timeout);

            let sigs = self.drain_signals();
            self.read_drains();
            self.handle_signals(&sigs);

            if let Some(status) = self.reap() {
                if let Some(exit) = self.on_child_exit(status) {
                    self.restore_handlers();
                    self.close_everything();
                    return Ok(exit);
                }
            }

            if self.tick_phase()? {
                let st = self.last_status.unwrap_or(0);
                self.restore_handlers();
                self.close_everything();
                return Ok(ExitStatus::from_raw(st));
            }
        }
    }

    fn next_timeout(&self) -> i32 {
        match self.phase {
            Phase::Running => -1,
            Phase::Stopping { deadline, .. } | Phase::Backoff { at: deadline } => {
                let left = deadline.saturating_duration_since(Instant::now());
                left.as_millis().min(i32::MAX as u128) as i32
            }
        }
    }

    fn wait_events(&mut self, timeout_ms: i32) {
        let mut fds: Vec<sys::PollFd> = Vec::with_capacity(1 + self.drains.len());
        fds.push(sys::PollFd {
            fd: self.sig_r,
            events: sys::POLLIN,
            revents: 0,
        });
        for d in &self.drains {
            fds.push(sys::PollFd {
                fd: d.fd,
                events: sys::POLLIN,
                revents: 0,
            });
        }
        // SAFETY: the vector is a valid array of pollfd for its length.
        unsafe { sys::poll(fds.as_mut_ptr(), fds.len() as u64, timeout_ms) };
    }

    fn drain_signals(&mut self) -> Vec<i32> {
        let mut out = Vec::new();
        let mut buf = [0u8; 64];
        loop {
            match sys::read_some(self.sig_r, &mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend(buf[..n].iter().map(|b| *b as i32)),
                Err(e) if e == sys::EAGAIN => break,
                Err(_) => break,
            }
        }
        out
    }

    /// Move captured bytes to the log, drop the rest, and forget drains that
    /// have reached EOF.
    fn read_drains(&mut self) {
        let mut dead = Vec::new();
        let mut buf = [0u8; 8192];
        for d in &self.drains {
            loop {
                match sys::read_some(d.fd, &mut buf) {
                    Ok(0) => {
                        dead.push(d.fd);
                        break;
                    }
                    Ok(n) => {
                        if d.to_log {
                            if let Some(log) = self.log.as_mut() {
                                log.write(&buf[..n]);
                            }
                        }
                    }
                    Err(e) if e == sys::EAGAIN => break,
                    Err(_) => {
                        dead.push(d.fd);
                        break;
                    }
                }
            }
        }
        for fd in dead {
            sys::close_fd(fd);
            self.drains.retain(|d| d.fd != fd);
        }
    }

    fn handle_signals(&mut self, sigs: &[i32]) {
        for sig in sigs {
            match *sig {
                sys::SIGHUP => {
                    if let Some(log) = self.log.as_mut() {
                        log.reopen_if_rotated();
                    }
                    self.forward(*sig);
                }
                sys::SIGTERM | sys::SIGINT | sys::SIGQUIT => {
                    self.stop_requested = true;
                    if matches!(self.phase, Phase::Running | Phase::Backoff { .. }) {
                        self.phase = Phase::Stopping {
                            deadline: Instant::now() + self.cmd.stop.grace,
                            killed: false,
                        };
                    }
                    self.forward(*sig);
                }
                sys::SIGUSR1 | sys::SIGUSR2 => self.forward(*sig),
                _ => {}
            }
        }
    }

    /// Send `sig` to the child, and to the whole namespace when we are its
    /// init (adopted orphans sit in their own process groups, so `kill(-1)` is
    /// the only call that reaches them).
    fn forward(&mut self, sig: i32) {
        // SAFETY: signalling processes this supervisor owns, or, as PID 1, its
        // whole namespace.
        unsafe {
            if self.pid1 {
                sys::kill(-1, sig);
            } else if let Some(pid) = self.child {
                sys::kill(pid, sig);
            }
        }
    }

    /// Reap what we are allowed to, and return the status of the main child if
    /// it was among them.
    fn reap(&mut self) -> Option<i32> {
        let target = if self.reap == Reap::Unavailable { self.child? } else { -1 };
        let mut main = None;
        loop {
            match sys::wait(target, sys::WNOHANG) {
                Ok((0, _)) => break,
                Ok((pid, status)) => {
                    if Some(pid) == self.child {
                        main = Some(status);
                        self.child = None;
                    }
                }
                Err(sys::EINTR) => continue,
                Err(_) => break,
            }
        }
        main
    }

    fn on_child_exit(&mut self, status: i32) -> Option<ExitStatus> {
        self.last_status = Some(status);

        // As PID 1 the container is never done while descendants live: ask the
        // rest of the namespace to stop and leave only when it is empty. A
        // non-init subreaper cannot enumerate what it adopted without /proc,
        // so it reports the main child's exit and lets the caller decide.
        if self.pid1 {
            self.forward(sys::SIGTERM);
            self.phase = Phase::Stopping {
                deadline: Instant::now() + self.cmd.stop.grace,
                killed: false,
            };
            return None;
        }

        // A stop request outranks the restart policy: the caller asked for the
        // child to be gone, not for another one.
        if self.stop_requested {
            return Some(ExitStatus::from_raw(status));
        }

        let failed = status & 0x7f != 0 || (status >> 8) & 0xff != 0;
        match self.cmd.restart {
            Restart::Never => Some(ExitStatus::from_raw(status)),
            Restart::OnFailure { .. } if !failed => Some(ExitStatus::from_raw(status)),
            Restart::Always { max, backoff } | Restart::OnFailure { max, backoff } => {
                self.attempts += 1;
                if self.attempts > max {
                    return Some(ExitStatus::from_raw(status));
                }
                self.phase = Phase::Backoff {
                    at: Instant::now() + backoff,
                };
                None
            }
        }
    }

    /// Timers. Returns true when the supervisor should exit.
    fn tick_phase(&mut self) -> Result<bool> {
        let now = Instant::now();
        match &self.phase {
            Phase::Running => Ok(false),
            Phase::Backoff { at } => {
                if now >= *at {
                    match self.spawn(-1) {
                        Ok(_) => Ok(false),
                        Err(_) => Ok(true),
                    }
                } else {
                    Ok(false)
                }
            }
            Phase::Stopping { deadline, killed } => {
                let killed = *killed;
                let deadline = *deadline;
                // Nothing left to stop: waiting out a grace period for a child
                // that is already gone is how a stop takes ten seconds for no
                // reason.
                if self.child.is_none() && !killed && !self.pid1 {
                    return Ok(true);
                }
                if now < deadline {
                    return Ok(false);
                }
                if !killed {
                    // The grace period is over. The child did not take the
                    // polite signal.
                    if let Some(pid) = self.child {
                        // SAFETY: our own child, and its group if it made one.
                        unsafe {
                            sys::kill(-pid, sys::SIGKILL);
                            sys::kill(pid, sys::SIGKILL);
                        }
                    }
                    if self.pid1 {
                        // SAFETY: as PID 1, kill everything left in the
                        // namespace; nothing else can reach it.
                        unsafe {
                            sys::kill(-1, sys::SIGKILL);
                        }
                    }
                    self.phase = Phase::Stopping {
                        deadline: now + Duration::from_millis(200),
                        killed: true,
                    };
                    return Ok(false);
                }
                // Killed and still here: the child is unreapable, or gone and
                // we already reaped it. As PID 1 we only leave when the
                // namespace is empty, which is what `ECHILD` proves.
                if self.pid1 {
                    Ok(matches!(sys::wait(-1, sys::WNOHANG), Err(sys::ECHILD)))
                } else {
                    Ok(self.child.is_none())
                }
            }
        }
    }

    fn close_everything(&mut self) {
        for d in self.drains.drain(..) {
            sys::close_fd(d.fd);
        }
        for fd in [&mut self.sig_r, &mut self.sig_w] {
            if *fd >= 0 {
                sys::close_fd(*fd);
                *fd = -1;
            }
        }
        if let Some(p) = self.pidfile.take() {
            p.release();
        }
        ACTIVE.store(false, Ordering::Release);
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.restore_handlers();
        self.close_everything();
    }
}

fn stage_syscall(stage: Stage) -> &'static str {
    match stage {
        Stage::PreFork => "setup",
        Stage::Open => "open",
        Stage::Chdir => "chdir",
        Stage::Privileges => "setuid",
        Stage::Exec => "execve",
    }
}

/// Used by `run()`: everything above, with the caller's signals left untouched
/// on the way out.
pub(crate) fn run_foreground(cmd: Command) -> Result<ExitStatus> {
    let budget = cmd.ready_timeout;
    let mut sup = Supervisor::new(cmd)?;
    // Only when asked. Becoming a subreaper turns `waitpid(-1)` into a reap of
    // every child of the embedding process, which is the host's business.
    let allow = sup.cmd.reap;
    sup.claim_reaping(allow);

    let (rfd, wfd) = {
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
        (fds[0], fds[1])
    };

    sup.install_handlers();
    let spawned = sup.spawn(wfd);
    sys::close_fd(wfd);
    spawned?;

    if let Err(e) = sup.await_start(rfd, budget) {
        sys::close_fd(rfd);
        return Err(e);
    }
    sys::close_fd(rfd);

    sup.run()
}