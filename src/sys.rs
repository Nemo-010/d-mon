//! Raw syscall surface.
//!
//! Everything here is `unsafe` and every declaration names the libc symbol it
//! binds to. The point of this module is that no other module in the crate
//! calls libc directly, so the unsafe surface is exactly this file.
//!
//! # EINTR policy
//!
//! Each wrapper documents whether it retries. The rule:
//! * `read`, `write`, `openat`, `close`, `fsync`, `ftruncate`, `flock` retry.
//! * `poll`, `waitpid` do **not** retry: `EINTR` is how the event loop learns
//!   that a signal arrived.

#![allow(non_camel_case_types)]
#![allow(missing_docs)]

use core::ffi::{c_char, c_int, c_void};

pub type pid_t = i32;
pub type uid_t = u32;
pub type gid_t = u32;
pub type mode_t = u32;

// Linux (asm-generic) values. Hardcoded because this crate has no libc
// dependency; they are stable ABI and identical on x86_64 and aarch64.
pub const O_RDONLY: c_int = 0;
pub const O_WRONLY: c_int = 1;
pub const O_RDWR: c_int = 2;
pub const O_CREAT: c_int = 0o100;
pub const O_EXCL: c_int = 0o200;
pub const O_APPEND: c_int = 0o2000;
pub const O_NONBLOCK: c_int = 0o4000;
pub const O_NOFOLLOW: c_int = 0o400000;
pub const O_CLOEXEC: c_int = 0o2000000;

pub const FD_CLOEXEC: c_int = 1;
pub const F_GETFD: c_int = 1;
pub const F_SETFD: c_int = 2;
pub const F_GETFL: c_int = 3;
pub const F_SETFL: c_int = 4;

pub const LOCK_EX: c_int = 2;
pub const LOCK_NB: c_int = 4;

pub const WNOHANG: c_int = 1;

pub const X_OK: c_int = 1;

pub const RLIMIT_NOFILE: c_int = 7;

pub const POLLIN: i16 = 0x001;
pub const POLLOUT: i16 = 0x004;
pub const POLLERR: i16 = 0x008;
pub const POLLHUP: i16 = 0x010;
pub const POLLNVAL: i16 = 0x020;

pub const SIGHUP: c_int = 1;
pub const SIGINT: c_int = 2;
pub const SIGQUIT: c_int = 3;
pub const SIGKILL: c_int = 9;
pub const SIGUSR1: c_int = 10;
pub const SIGUSR2: c_int = 12;
pub const SIGPIPE: c_int = 13;
pub const SIGTERM: c_int = 15;
pub const SIGCHLD: c_int = 17;

pub const SIG_DFL: usize = 0;
pub const SIG_IGN: usize = 1;

pub const PR_SET_PDEATHSIG: c_int = 1;
pub const PR_CAPBSET_DROP: c_int = 24;
pub const PR_SET_SECUREBITS: c_int = 28;
pub const PR_SET_CHILD_SUBREAPER: c_int = 36;
pub const PR_SET_NO_NEW_PRIVS: c_int = 38;

/// `SECBIT_NOROOT`: a setuid-root binary does not gain capabilities.
pub const SECBIT_NOROOT: usize = 1;

pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const ESRCH: i32 = 3;
pub const EINTR: i32 = 4;
pub const EIO: i32 = 5;
pub const E2BIG: i32 = 7;
pub const EBADF: i32 = 9;
pub const ECHILD: i32 = 10;
pub const EAGAIN: i32 = 11;
pub const EWOULDBLOCK: i32 = 11;
pub const ENOMEM: i32 = 12;
pub const EACCES: i32 = 13;
pub const EFAULT: i32 = 14;
pub const EBUSY: i32 = 16;
pub const EEXIST: i32 = 17;
pub const ENOTDIR: i32 = 20;
pub const EISDIR: i32 = 21;
pub const EINVAL: i32 = 22;
pub const EMFILE: i32 = 24;
pub const ENOSPC: i32 = 28;
pub const EROFS: i32 = 30;
pub const ENOSYS: i32 = 38;
pub const ELOOP: i32 = 40;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PollFd {
    pub fd: c_int,
    pub events: i16,
    pub revents: i16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RLimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

/// `sigset_t` as Linux defines it for 64-bit: 1024 bits.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SigSet {
    bits: [u64; 16],
}

impl SigSet {
    /// The empty set, which is what a child wants its blocked mask to be.
    pub const fn empty() -> SigSet {
        SigSet { bits: [0; 16] }
    }

    pub fn add(&mut self, sig: c_int) {
        let s = (sig - 1) as usize;
        self.bits[s / 64] |= 1u64 << (s % 64);
    }
}

pub const SIG_SETMASK: c_int = 2;

/// `struct passwd`, glibc/musl x86_64 layout.
#[repr(C)]
pub struct Passwd {
    pub pw_name: *mut c_char,
    pub pw_passwd: *mut c_char,
    pub pw_uid: uid_t,
    pub pw_gid: gid_t,
    pub pw_gecos: *mut c_char,
    pub pw_dir: *mut c_char,
    pub pw_shell: *mut c_char,
}

/// `struct group`, glibc/musl x86_64 layout.
#[repr(C)]
pub struct Group {
    pub gr_name: *mut c_char,
    pub gr_passwd: *mut c_char,
    pub gr_gid: gid_t,
    pub gr_mem: *mut *mut c_char,
}

extern "C" {
    pub fn fork() -> pid_t;
    pub fn setsid() -> pid_t;
    pub fn setpgid(pid: pid_t, pgid: pid_t) -> c_int;
    pub fn getpid() -> pid_t;
    pub fn getppid() -> pid_t;
    pub fn getpgrp() -> pid_t;
    pub fn getuid() -> uid_t;
    pub fn geteuid() -> uid_t;
    pub fn getgid() -> gid_t;
    pub fn getegid() -> gid_t;
    pub fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    pub fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    pub fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
    pub fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
    pub fn waitpid(pid: pid_t, status: *mut c_int, options: c_int) -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn chdir(path: *const c_char) -> c_int;
    pub fn umask(mask: mode_t) -> mode_t;
    pub fn setuid(uid: uid_t) -> c_int;
    pub fn setgid(gid: gid_t) -> c_int;
    pub fn setgroups(n: usize, list: *const gid_t) -> c_int;
    pub fn access(path: *const c_char, mode: c_int) -> c_int;
    pub fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char)
        -> c_int;
    pub fn _exit(code: c_int) -> !;
    #[link_name = "openat"]
    pub fn openat(dirfd: c_int, path: *const c_char, flags: c_int, mode: mode_t) -> c_int;
    pub fn fsync(fd: c_int) -> c_int;
    pub fn ftruncate(fd: c_int, len: i64) -> c_int;
    pub fn flock(fd: c_int, operation: c_int) -> c_int;
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    pub fn signal(sig: c_int, handler: usize) -> usize;
    pub fn pthread_sigmask(how: c_int, set: *const SigSet, old: *mut SigSet) -> c_int;    pub fn prctl(option: c_int, a2: usize, a3: usize, a4: usize, a5: usize) -> c_int;
    pub fn getrlimit(resource: c_int, rlim: *mut RLimit) -> c_int;
    pub fn getpwnam(name: *const c_char) -> *mut Passwd;    pub fn getgrnam(name: *const c_char) -> *mut Group;
    pub fn getgrouplist(
        user: *const c_char,
        group: gid_t,
        groups: *mut gid_t,
        ngroups: *mut c_int,
    ) -> c_int;
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
        ESRCH => "ESRCH",
        EINTR => "EINTR",
        EIO => "EIO",
        E2BIG => "E2BIG",
        EBADF => "EBADF",
        ECHILD => "ECHILD",
        EAGAIN => "EAGAIN",
        ENOMEM => "ENOMEM",
        EACCES => "EACCES",
        EFAULT => "EFAULT",
        EBUSY => "EBUSY",
        EEXIST => "EEXIST",
        ENOTDIR => "ENOTDIR",
        EISDIR => "EISDIR",
        EINVAL => "EINVAL",
        EMFILE => "EMFILE",
        ENOSPC => "ENOSPC",
        EROFS => "EROFS",
        ENOSYS => "ENOSYS",
        ELOOP => "ELOOP",
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
            return Err(EIO);
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

/// Mark every descriptor above 2 close-on-exec, except `keep`.
///
/// This is what makes the target inherit nothing but 0/1/2 *and* lets the
/// readiness record be written afterwards: the ready fd survives until
/// `execve`, which is exactly when the kernel closes it and the launcher sees
/// EOF.
pub fn cloexec_all_from_3(keep: c_int) {
    let mut lim = RLimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: valid out-pointer, known resource.
    let n = if unsafe { getrlimit(RLIMIT_NOFILE, &mut lim) } == 0 && lim.rlim_cur > 0 {
        lim.rlim_cur.min(65536) as c_int
    } else {
        4096
    };
    let mut fd = 3;
    while fd < n {
        if fd != keep {
            // SAFETY: fcntl on a closed fd returns EBADF, which is harmless
            // and cheaper than an existence check.
            unsafe {
                fcntl(fd, F_SETFD, FD_CLOEXEC);
            }
        }
        fd += 1;
    }
}

/// A pipe whose both ends are close-on-exec and non-blocking, for the
/// supervisor's signal self-pipe.
pub fn self_pipe() -> Result<(c_int, c_int), i32> {
    let mut fds = [0 as c_int; 2];
    // SAFETY: two integers of writable storage.
    let rc = unsafe { pipe2(fds.as_mut_ptr(), O_CLOEXEC | O_NONBLOCK) };
    if rc < 0 {
        return Err(errno());
    }
    Ok((fds[0], fds[1]))
}

/// Set `O_NONBLOCK` on `fd`.
pub fn set_nonblock(fd: c_int) -> Result<(), i32> {
    // SAFETY: fcntl get/set on a valid descriptor.
    let flags = unsafe { fcntl(fd, F_GETFL) };
    if flags < 0 {
        return Err(errno());
    }
    // SAFETY: as above.
    let rc = unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) };
    if rc < 0 {
        Err(errno())
    } else {
        Ok(())
    }
}

/// `kill(pid, 0)`: is this pid still something the caller may signal?
pub fn alive(pid: pid_t) -> bool {
    // SAFETY: signal 0 performs the permission and existence check without
    // delivering anything.
    let rc = unsafe { kill(pid, 0) };
    rc == 0 || errno() == EPERM
}

/// Non-retrying `waitpid`. `EINTR` is returned so the event loop can notice
/// that a signal arrived.
pub fn wait(pid: pid_t, options: c_int) -> Result<(pid_t, i32), i32> {
    let mut status: c_int = 0;
    // SAFETY: status is a valid out-pointer for this call.
    let rc = unsafe { waitpid(pid, &mut status, options) };
    if rc < 0 {
        Err(errno())
    } else {
        Ok((rc, status))
    }
}

/// Human-readable name of the signal a wait status carries, if any.
pub fn term_signal(status: i32) -> Option<i32> {
    let sig = status & 0x7f;
    if sig == 0 || sig == 0x7f {
        None
    } else {
        Some(sig)
    }
}

/// Exit code a wait status carries, if the child exited normally.
pub fn exit_code(status: i32) -> Option<i32> {
    if status & 0x7f == 0 {
        Some((status >> 8) & 0xff)
    } else {
        None
    }
}
