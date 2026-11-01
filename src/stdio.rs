//! What the child's descriptors should be.

use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::IntoRawFd;
use std::path::PathBuf;

use crate::error::{Fail, Stage};
use crate::log::Log;
use crate::sys;

/// Descriptor policy for the child.
///
/// `Discard` is the only variant that wants a null device, and it degrades:
/// `/dev/null`, then a pipe drained by the supervisor, then leave it inherited.
/// It never falls back to `open("/")`, which would hand the child a directory
/// descriptor it could `openat` its way out of a chroot with, and never to
/// `/dev/zero` (no EOF on read) or a `memfd` (unbounded growth on write).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stdio {
    /// Leave whatever was inherited.
    Inherit,
    /// Send to a null sink.
    Discard,
    /// Capture and write to the configured `log_file`.
    Capture,
    /// Append to this file.
    File(PathBuf),
}

impl Default for Stdio {
    fn default() -> Self {
        Stdio::Inherit
    }
}

/// How `Discard` found a sink. Reported so the operator is not surprised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    NullDevice,
    DrainedPipe,
    Inherited,
}

impl Sink {
    fn worst(self, other: Sink) -> Sink {
        let rank = |s: Sink| match s {
            Sink::NullDevice => 0,
            Sink::DrainedPipe => 1,
            Sink::Inherited => 2,
        };
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

/// A pipe the supervisor must read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Drain {
    pub fd: i32,
    /// Write what is read into the log, instead of discarding it.
    pub to_log: bool,
}

/// The resolved descriptor plan for one fork.
#[derive(Debug, Default)]
pub(crate) struct Planned {
    /// The descriptor the child should end up with at 0, 1 and 2.
    pub fds: [i32; 3],
    /// Ends that belong to the child: close them in the supervisor as soon as
    /// it has forked, or a drain pipe never reports EOF.
    pub child_only: Vec<i32>,
    pub drains: Vec<Drain>,
    pub sink: Sink,
    pub notes: Vec<&'static str>,
}

impl Default for Sink {
    fn default() -> Self {
        Sink::NullDevice
    }
}

impl Planned {
    pub fn close_child_ends(&mut self) {
        for fd in self.child_only.drain(..) {
            sys::close_fd(fd);
        }
    }
}

/// A plan that is dropped before it is used — a failed `fork`, a failed
/// resolution — must not leak the descriptors it opened. Both vectors are
/// emptied as their contents are handed over, so this is a no-op on the normal
/// path.
impl Drop for Planned {
    fn drop(&mut self) {
        for fd in self.child_only.drain(..) {
            sys::close_fd(fd);
        }
        for d in self.drains.drain(..) {
            sys::close_fd(d.fd);
        }
    }
}

/// Resolve the descriptor plan. Everything is opened here, pre-fork.
pub(crate) fn plan(cmd: &crate::Command, log: Option<&Log>) -> Result<Planned, Fail> {
    let mut p = Planned::default();
    p.fds = [0, 1, 2];
    p.sink = Sink::NullDevice;

    for (slot, want) in [cmd.stdin_stdio.clone(), cmd.stdout.clone(), cmd.stderr.clone()]
        .into_iter()
        .enumerate()
    {
        match want {
            Stdio::Inherit => p.fds[slot] = slot as i32,
            Stdio::File(path) => {
                let f = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .write(true)
                    .mode(0o640)
                    .custom_flags(sys::O_NOFOLLOW)
                    .open(&path)
                    .map_err(|e| {
                        Fail::new(Stage::Open, e.raw_os_error().unwrap_or(sys::EIO), "open")
                            .with_path(path.clone())
                    })?;
                let fd = f.into_raw_fd();
                p.fds[slot] = fd;
                p.child_only.push(fd);
            }
            Stdio::Capture => {
                if log.is_none() {
                    return Err(Fail::new(
                        Stage::PreFork,
                        sys::EINVAL,
                        "capture needs a log file",
                    ));
                }
                let (r, w) = pipe()?;
                p.fds[slot] = w;
                p.child_only.push(w);
                p.drains.push(Drain { fd: r, to_log: true });
            }
            Stdio::Discard => match open_null(slot == 0) {
                Ok(fd) => {
                    p.fds[slot] = fd;
                    p.child_only.push(fd);
                }
                Err(_) => match pipe() {
                    Ok((r, w)) => {
                        p.sink = p.sink.worst(Sink::DrainedPipe);
                        p.notes.push("no /dev/null; draining a pipe");
                        if slot == 0 {
                            // Read end for the child, write end closed now:
                            // reads return EOF instead of blocking, which is
                            // the null device's behaviour on stdin.
                            sys::close_fd(w);
                            p.fds[0] = r;
                            p.child_only.push(r);
                        } else {
                            p.fds[slot] = w;
                            p.child_only.push(w);
                            p.drains.push(Drain { fd: r, to_log: false });
                        }
                    }
                    Err(_) => {
                        p.sink = p.sink.worst(Sink::Inherited);
                        p.notes.push("no /dev/null and no pipe; inherited");
                        p.fds[slot] = slot as i32;
                    }
                },
            },
        }
    }

    Ok(p)
}

fn open_null(read: bool) -> Result<i32, i32> {
    let p = b"/dev/null\0";
    let flags = if read { sys::O_RDONLY } else { sys::O_WRONLY };
    // SAFETY: a NUL-terminated path and fixed flags.
    let fd = unsafe { sys::openat(-100, p.as_ptr() as *const _, flags | sys::O_CLOEXEC, 0) };
    if fd < 0 {
        Err(sys::errno())
    } else {
        Ok(fd)
    }
}

/// Point this process's own 0/1/2 at the null device.
///
/// A detached supervisor must not keep the caller's descriptors: they may be
/// the write end of a pipe (`out=$(d-mon prog)`) or an ssh channel, and
/// holding either means the caller's read never sees EOF. Where there is no
/// `/dev/null` the descriptors are closed instead — a write that fails is a
/// better outcome than a pipe that never closes.
pub(crate) fn detach_own_stdio() {
    let p = b"/dev/null\0";
    // SAFETY: a NUL-terminated path and fixed flags.
    let fd = unsafe { sys::openat(-100, p.as_ptr() as *const _, sys::O_RDWR | sys::O_CLOEXEC, 0) };
    if fd >= 0 {
        for t in 0..3 {
            // SAFETY: duplicating onto the standard descriptors.
            unsafe {
                sys::dup2(fd, t);
            }
        }
        if fd > 2 {
            sys::close_fd(fd);
        }
        return;
    }
    for t in 0..3 {
        sys::close_fd(t);
    }
}

fn pipe() -> Result<(i32, i32), Fail> {
    let mut fds = [0 as i32; 2];
    // SAFETY: two integers of writable storage.
    let rc = unsafe { sys::pipe2(fds.as_mut_ptr(), sys::O_CLOEXEC) };
    if rc < 0 {
        return Err(Fail::new(Stage::Open, sys::errno(), "pipe2"));
    }
    Ok((fds[0], fds[1]))
}