//! Raw syscall surface.
//!
//! Everything here is `unsafe` and every declaration names the libc symbol it
//! binds to. The point of this module is that no other module in the crate
//! calls libc directly, so the unsafe surface is exactly this file.
//!
//! # EINTR policy
//!
//! Each wrapper documents whether it retries. The rule:
//! * `read`, `write`, `open`, `close`, `fsync`, `ftruncate`, `flock` retry.
//! * `ppoll`, `waitpid` do **not** retry: `EINTR` is how the event loop learns
//!   that a signal arrived.

#![allow(non_camel_case_types)]
#![allow(missing_docs)]

use core::ffi::{c_char, c_int, c_void};

pub type pid_t = i32;
pub type uid_t = u32;
pub type gid_t = u32;
pub type mode_t = u32;

pub const O_RDONLY: c_int = 0;
pub const O_WRONLY: c_int = 1;
pub const O_RDWR: c_int = 2;
pub const O_CREAT: c_int = 0o100;
pub const O_APPEND: c_int = 0o2000;
pub const O_NOFOLLOW: c_int = 0o400000;
pub const O_CLOEXEC: c_int = 0o2000000;
pub const O_NONBLOCK: c_int = 0o4000;

pub const F_GETFL: c_int = 3;
pub const F_SETFL: c_int = 4;
pub const LOCK_EX: c_int = 2;
pub const LOCK_NB: c_int = 4;

pub const WNOHANG: c_int = 1;

pub const SIGTERM: c_int = 15;
pub const SIGKILL: c_int = 9;
pub const SIGCHLD: c_int = 17;
pub const SIGPIPE: c_int = 13;

pub const SIG_DFL: usize = 0;

pub const PR_SET_PDEATHSIG: c_int = 1;
pub const PR_SET_CHILD_SUBREAPER: c_int = 36;
pub const PR_SET_NO_NEW_PRIVS: c_int = 38;

pub const EPERM: i32 = 1;
pub const EINTR: i32 = 4;
pub const EBADF: i32 = 9;
pub const ECHILD: i32 = 10;
pub const EAGAIN: i32 = 11;
pub const EWOULDBLOCK: i32 = 11;
pub const EACCES: i32 = 13;
pub const EEXIST: i32 = 17;
pub const ENOENT: i32 = 2;
pub const ENOSYS: i32 = 38;

extern "C" {
    pub fn fork() -> pid_t;
    pub fn setsid() -> pid_t;
    pub fn setpgid(pid: pid_t, pgid: pid_t) -> c_int;
    pub fn getpid() -> pid_t;
    pub fn getppid() -> pid_t;
    pub fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    pub fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    pub fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
    pub fn waitpid(pid: pid_t, status: *mut c_int, options: c_int) -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn chdir(path: *const c_char) -> c_int;
    pub fn umask(mask: mode_t) -> mode_t;
    pub fn setuid(uid: uid_t) -> c_int;
    pub fn setgid(gid: gid_t) -> c_int;
    pub fn setgroups(n: usize, list: *const gid_t) -> c_int;
    pub fn execve(
        path: *const c_char,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> c_int;
    pub fn _exit(code: c_int) -> !;
    #[link_name = "openat"]
    pub fn openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: mode_t) -> c_int;
    pub fn fsync(fd: c_int) -> c_int;
    pub fn ftruncate(fd: c_int, len: i64) -> c_int;
    pub fn flock(fd: c_int, operation: c_int) -> c_int;
    pub fn signal(sig: c_int, handler: usize) -> usize;
    pub fn prctl(option: c_int, a2: usize, a3: usize, a4: usize, a5: usize) -> c_int;
}

/// Last error, as a positive errno.
pub fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A name for an errno, so a message says `ENOENT` and not `2`.
pub fn errno_name(e: i32) -> &'static str {
    match e {
        EPERM => "EPERM",
        ENOENT => "ENOENT",
        EINTR => "EINTR",
        EBADF => "EBADF",
        ECHILD => "ECHILD",
        EAGAIN => "EAGAIN",
        EACCES => "EACCES",
        EEXIST => "EEXIST",
        12 => "ENOMEM",

        20 => "ENOTDIR",
        21 => "EISDIR",
        22 => "EINVAL",
        28 => "ENOSPC",
        30 => "EROFS",

        _ => "?",
    }
}

/// True when this process is the init of its PID namespace.
pub fn is_pid1() -> bool {
    // SAFETY: getpid has no preconditions.
    unsafe { getpid() == 1 }
}

/// `write` that retries on `EINTR` and reports a short write as an error.
pub fn write_all(fd: c_int, buf: &[u8]) -> Result<(), i32> {
    let mut off = 0usize;
    while off < buf.len() {
        // SAFETY: the slice is valid for buf.len() - off bytes.
        let n = unsafe { write(fd, buf.as_ptr().add(off) as *const c_void, buf.len() - off) };
        if n < 0 {
            let e = errno();
            if e == EINTR {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(EBADF);
        }
        off += n as usize;
    }
    Ok(())
}

/// `read` that retries on `EINTR`. Returns the count, `0` on EOF.
pub fn read_some(fd: c_int, buf: &mut [u8]) -> Result<usize, i32> {
    loop {
        // SAFETY: the buffer is valid for buf.len() bytes.
        let n = unsafe { read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
        if n < 0 {
            let e = errno();
            if e == EINTR {
                continue;
            }
            return Err(e);
        }
        return Ok(n as usize);
    }
}

/// `close`, treating `EINTR` as success (the fd is gone either way).
pub fn close_fd(fd: c_int) {
    // SAFETY: closing an fd twice is a caller bug, not UB.
    unsafe {
        close(fd);
    }
}
