//! The builder. This is the library surface most callers touch.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::daemon::{self, Started};
use crate::error::{Error, Result};
use crate::stdio::Stdio;

/// A user or group, resolved the way the target environment can support.
#[derive(Debug, Clone)]
pub enum Id {
    /// Works in a scratch container: no `/etc/passwd` needed.
    Numeric(u32),
    /// Requires `/etc/passwd`; resolved before the fork, and an error if absent.
    Name(String),
}

/// When to restart the supervised child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restart {
    Never,
    /// Restart whenever the child exits, `max` times in a row, with `backoff`
    /// between attempts.
    Always { max: u32, backoff: Duration },
    /// Restart only when it exited non-zero or was signalled.
    OnFailure { max: u32, backoff: Duration },
}

impl Default for Restart {
    fn default() -> Self {
        Restart::Never
    }
}

/// How to stop the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopPolicy {
    pub signal: i32,
    pub grace: Duration,
}

impl Default for StopPolicy {
    fn default() -> Self {
        StopPolicy {
            signal: crate::sys::SIGTERM,
            grace: Duration::from_secs(10),
        }
    }
}

/// A program to run, with everything `d-mon` needs to place it.
#[derive(Debug, Clone)]
pub struct Command {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) env_clear: bool,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) umask: Option<u32>,
    pub(crate) user: Option<Id>,
    pub(crate) group: Option<Id>,
    pub(crate) stdout: Stdio,
    pub(crate) stderr: Stdio,
    pub(crate) stdin_stdio: Stdio,
    pub(crate) log_file: Option<PathBuf>,
    pub(crate) pid_file: Option<PathBuf>,
    pub(crate) restart: Restart,
    pub(crate) stop: StopPolicy,
    pub(crate) ready_timeout: Duration,
    pub(crate) reap: bool,
}

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Command {
            program: program.as_ref().to_os_string(),
            args: vec![program.as_ref().to_os_string()],
            env: Vec::new(),
            env_clear: false,
            cwd: None,
            umask: None,
            user: None,
            group: None,
            stdout: Stdio::Inherit,
            stderr: Stdio::Inherit,
            stdin_stdio: Stdio::Inherit,
            log_file: None,
            pid_file: None,
            restart: Restart::Never,
            stop: StopPolicy::default(),
            ready_timeout: Duration::from_millis(5000),
            reap: false,
        }
    }

    /// Descriptor policy for the child's standard input.
    pub fn stdin(mut self, s: Stdio) -> Self {
        self.stdin_stdio = s;
        self
    }

    pub fn arg(mut self, a: impl AsRef<OsStr>) -> Self {
        self.args.push(a.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(mut self, it: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for a in it {
            self.args.push(a.as_ref().to_os_string());
        }
        self
    }

    pub fn env(mut self, k: impl AsRef<OsStr>, v: impl AsRef<OsStr>) -> Self {
        self.env
            .push((k.as_ref().to_os_string(), v.as_ref().to_os_string()));
        self
    }

    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }

    pub fn current_dir(mut self, p: impl AsRef<Path>) -> Self {
        self.cwd = Some(p.as_ref().to_path_buf());
        self
    }

    pub fn umask(mut self, mode: u32) -> Self {
        self.umask = Some(mode);
        self
    }

    pub fn user(mut self, id: Id) -> Self {
        self.user = Some(id);
        self
    }

    pub fn group(mut self, id: Id) -> Self {
        self.group = Some(id);
        self
    }

    pub fn stdout(mut self, s: Stdio) -> Self {
        self.stdout = s;
        self
    }

    pub fn stderr(mut self, s: Stdio) -> Self {
        self.stderr = s;
        self
    }

    pub fn log_file(mut self, p: impl AsRef<Path>) -> Self {
        self.log_file = Some(p.as_ref().to_path_buf());
        self
    }

    pub fn pid_file(mut self, p: impl AsRef<Path>) -> Self {
        self.pid_file = Some(p.as_ref().to_path_buf());
        self
    }

    pub fn restart(mut self, r: Restart) -> Self {
        self.restart = r;
        self
    }

    pub fn stop(mut self, s: StopPolicy) -> Self {
        self.stop = s;
        self
    }

    pub fn ready_timeout(mut self, d: Duration) -> Self {
        self.ready_timeout = d;
        self
    }

    /// Become a child subreaper in `run()`, so orphaned grandchildren are
    /// reparented to this process instead of init. Off by default: the
    /// attribute survives `execve`, so turning an embedding host into a reaper
    /// is a change the caller must ask for.
    pub fn reap(mut self, yes: bool) -> Self {
        self.reap = yes;
        self
    }

    /// Detach, and return as soon as the outcome is known.
    ///
    /// Returns `Ok` only when the program really did exec. A failed start is an
    /// `Err`, so `?` cannot swallow it.
    pub fn daemonize(self) -> Result<Started> {
        let program = self.program.clone();
        let pid_file = self.pid_file.clone();
        daemon::daemonize(self).map_err(|e| match (e, &pid_file) {
            // The launcher learns the fact from a record that does not carry the
            // path; the command does.
            (Error::AlreadyRunning { path, pid }, Some(p)) if path.as_os_str().is_empty() => {
                Error::AlreadyRunning {
                    path: p.clone(),
                    pid,
                }
            }
            (e, _) => e.named(&program),
        })
    }

    /// Stay in the foreground and supervise. This is what a container entrypoint
    /// should call: it reaps, forwards signals, and never claims to have started
    /// something it did not.
    pub fn run(self) -> Result<std::process::ExitStatus> {
        let program = self.program.clone();
        daemon::run(self).map_err(|e| e.named(&program))
    }
}