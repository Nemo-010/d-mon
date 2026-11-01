//! The exec contract.
//!
//! Everything that can allocate, read `/etc/passwd`, walk `PATH` or resolve a
//! path happens **before** the fork, and lands in an [`ExecPlan`]. Between
//! `fork` and `execve` the child makes only async-signal-safe calls from the
//! list in PLAN.md §5.2 — no `malloc`, no NSS, no `getenv`.
//!
//! Two deliberate departures from an earlier draft of that section, both
//! because the draft was wrong about sessions:
//!
//! * the child does **not** call `setsid()`; after the double fork it is
//!   already in a session with no controlling terminal and is not a session
//!   leader, which is what makes a late `open("/dev/tty")` fail. Calling
//!   `setsid()` there would make it a leader again — the exact `daemon(3)`
//!   defect criticised in PLAN §1;
//! * there is therefore no `setpgid` fallback, since the only case that
//!   wanted it (`setsid` failed) cannot arise.

use std::ffi::{CStr, CString, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use crate::command::{Command, Id};
use crate::error::{Fail, Stage};
use crate::sys;

/// Where the target's descriptors come from, decided pre-fork.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StdioFds {
    pub stdin: i32,
    pub stdout: i32,
    pub stderr: i32,
}

impl StdioFds {
    pub fn as_array(self) -> [i32; 3] {
        [self.stdin, self.stdout, self.stderr]
    }
}

/// A fully resolved plan for one `execve`.
///
/// Everything the child needs is on the heap already, including the
/// NUL-terminated pointer arrays: the child must not allocate, because a
/// `fork` in a multi-threaded process can leave the allocator locked by a
/// thread that does not exist in the child.
#[derive(Debug, Clone)]
#[allow(dead_code)] // keeps the CStrings alive that the pointer arrays point at
pub struct ExecPlan {
    pub program: CString,
    argv: Vec<CString>,
    envp: Vec<CString>,
    argv_ptrs: Vec<*const std::ffi::c_char>,
    envp_ptrs: Vec<*const std::ffi::c_char>,
    cwd: Option<CString>,
    uid: Option<sys::uid_t>,
    gid: Option<sys::gid_t>,
    groups: Option<Vec<sys::gid_t>>,
    umask: Option<sys::mode_t>,
    fds: [i32; 3],
    ready_fd: i32,
}

/// The resolved program path, for messages.
impl ExecPlan {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.program.as_bytes().to_vec()))
    }
}

impl ExecPlan {
    /// Resolve and materialise. Any failure here is a pre-fork failure and is
    /// reported with the stage it belongs to.
    pub fn build(cmd: &Command, fds: [i32; 3], ready_fd: i32) -> Result<ExecPlan, Fail> {
        let (program, argv0) = resolve_program(cmd, &fds)?;

        let mut argv = vec![argv0];
        for a in &cmd.args[1.min(cmd.args.len())..] {
            argv.push(cstring(a, Stage::PreFork, "argv")?);
        }

        let envp = build_env(cmd)?;

        let cwd = match &cmd.cwd {
            Some(p) => Some(cstring(p.as_os_str(), Stage::Chdir, "chdir")?),
            None => None,
        };

        let (uid, gid, groups) = resolve_identity(cmd)?;

        // Pointer arrays, built once, here, so the child allocates nothing.
        let mut argv_ptrs: Vec<*const std::ffi::c_char> =
            argv.iter().map(|c| c.as_ptr()).collect();
        argv_ptrs.push(std::ptr::null());
        let mut envp_ptrs: Vec<*const std::ffi::c_char> =
            envp.iter().map(|c| c.as_ptr()).collect();
        envp_ptrs.push(std::ptr::null());

        Ok(ExecPlan {
            program,
            argv,
            envp,
            argv_ptrs,
            envp_ptrs,
            cwd,
            uid,
            gid,
            groups,
            umask: cmd.umask,
            fds,
            ready_fd,
        })
    }

    /// The child half. Never returns on success; returns the failure on the
    /// path that would have called `execve`.
    ///
    /// # Safety
    ///
    /// Must be called in a freshly forked child, before it does anything else.
    pub unsafe fn exec(self) -> ! {
        let rfd = self.ready_fd;
        // 1. Parent death. First, because the window is largest here.
        if !crate::reaper::arm_parent_death(sys::SIGTERM) {
            die(rfd, Stage::PreFork, sys::ESRCH, "prctl(PR_SET_PDEATHSIG)");
        }

        // 2. Descriptors.
        for (dst, src) in self.fds.iter().enumerate() {
            let dst = dst as i32;
            if *src != dst {
                // SAFETY: dup2 is async-signal-safe.
                if unsafe { sys::dup2(*src, dst) } < 0 {
                    die(rfd, Stage::Exec, sys::errno(), "dup2");
                }
            }
        }
        // Everything above 2 is close-on-exec except the readiness pipe, so
        // the target inherits only 0/1/2 and the record can still be written.
        sys::cloexec_all_from_3(self.ready_fd);

        // 3. Signals: neither a blocked mask nor an ignored disposition may
        // survive execve. Rust ignores SIGPIPE on our behalf, and an ignored
        // SIGPIPE in a daemon is a silent-truncation bug.
        reset_signals();

        // 4. Working directory.
        if let Some(cwd) = &self.cwd {
            // SAFETY: cwd is a valid NUL-terminated string.
            if unsafe { sys::chdir(cwd.as_ptr()) } < 0 {
                die(rfd, Stage::Chdir, sys::errno(), "chdir");
            }
        }

        // 5. Privileges, in the only order that works: the bounding set must
        // be scrubbed while CAP_SETPCAP is still held, i.e. before setuid.
        if let Err(f) = unsafe { self.drop_privileges() } {
            crate::error::child_exit(rfd, f);
        }

        // 6. umask.
        if let Some(m) = self.umask {
            // SAFETY: umask has no preconditions.
            unsafe {
                sys::umask(m);
            }
        }

        // 7. Report, then exec. Nothing may come between these two.
        let _ = crate::ready::send(
            self.ready_fd,
            crate::ready::Rec::Hello {
                pid: unsafe { sys::getpid() },
            },
        );

        // SAFETY: both pointer vectors are NUL-terminated, were built before
        // the fork so the child allocates nothing, and outlive the call.
        unsafe {
            sys::execve(
                self.program.as_ptr(),
                self.argv_ptrs.as_ptr(),
                self.envp_ptrs.as_ptr(),
            );
        }
        die(rfd, Stage::Exec, sys::errno(), "execve")
    }

    unsafe fn drop_privileges(&self) -> Result<(), Fail> {
        // SAFETY: fixed integer arguments, async-signal-safe.
        let rc = unsafe { sys::prctl(sys::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
        if rc < 0 {
            return Err(Fail::new(
                Stage::Privileges,
                sys::errno(),
                "prctl(PR_SET_NO_NEW_PRIVS)",
            ));
        }

        let root = unsafe { sys::geteuid() } == 0;
        if root && self.uid.is_some_and(|u| u != 0) {
            // Best effort: a kernel without CAP_SETPCAP support here is not a
            // reason to refuse to drop privileges, but a silent failure is why
            // this is attempted before setuid at all.
            for cap in 0..=63usize {
                unsafe {
                    sys::prctl(sys::PR_CAPBSET_DROP, cap, 0, 0, 0);
                }
            }
            unsafe {
                sys::prctl(sys::PR_SET_SECUREBITS, sys::SECBIT_NOROOT, 0, 0, 0);
            }
        }

        if let Some(groups) = &self.groups {
            // SAFETY: the slice is valid for its length.
            let rc = unsafe { sys::setgroups(groups.len(), groups.as_ptr()) };
            if rc < 0 {
                return Err(Fail::new(
                    Stage::Privileges,
                    sys::errno(),
                    "setgroups",
                ));
            }
        }
        if let Some(gid) = self.gid {
            if unsafe { sys::setgid(gid) } < 0 {
                return Err(Fail::new(Stage::Privileges, sys::errno(), "setgid"));
            }
        }
        if let Some(uid) = self.uid {
            if unsafe { sys::setuid(uid) } < 0 {
                return Err(Fail::new(Stage::Privileges, sys::errno(), "setuid"));
            }
        }
        Ok(())
    }
}

fn die(ready_fd: i32, stage: Stage, errno: i32, syscall: &'static str) -> ! {
    crate::error::child_exit(ready_fd, Fail::new(stage, errno, syscall))
}

/// Empty mask, `SIG_DFL` for everything a d-mon process might have set.
fn reset_signals() {
    let set = sys::SigSet::empty();
    // SAFETY: a zeroed sigset_t is the empty mask.
    unsafe {
        sys::pthread_sigmask(sys::SIG_SETMASK, &set, std::ptr::null_mut());
    }
    let _ = &set;
    for sig in [
        sys::SIGHUP,
        sys::SIGINT,
        sys::SIGQUIT,
        sys::SIGTERM,
        sys::SIGCHLD,
        sys::SIGPIPE,
        sys::SIGUSR1,
        sys::SIGUSR2,
    ] {
        // SAFETY: installing SIG_DFL is async-signal-safe.
        unsafe {
            sys::signal(sig, sys::SIG_DFL);
        }
    }
}

fn build_env(cmd: &Command) -> Result<Vec<CString>, Fail> {
    let mut out: Vec<CString> = Vec::new();
    if !cmd.env_clear {
        for (k, v) in std::env::vars_os() {
            let mut kv = k.clone().into_vec();
            kv.push(b'=');
            kv.extend(v.into_vec());
            out.push(CString::new(kv).map_err(|_| {
                Fail::new(Stage::PreFork, sys::EINVAL, "environ")
            })?);
        }
    }
    for (k, v) in &cmd.env {
        let mut kv = k.as_bytes().to_vec();
        kv.push(b'=');
        kv.extend(v.as_bytes());
        let key = k.as_bytes();
        out.retain(|c| {
            let b = c.as_bytes();
            !(b.len() > key.len() && b.starts_with(key) && b[key.len()] == b'=')
        });
        out.push(
            CString::new(kv).map_err(|_| Fail::new(Stage::PreFork, sys::EINVAL, "environ"))?,
        );
    }
    Ok(out)
}

/// `uid`, `gid` and the supplementary group list, all resolved pre-fork.
fn resolve_identity(
    cmd: &Command,
) -> Result<
    (
        Option<sys::uid_t>,
        Option<sys::gid_t>,
        Option<Vec<sys::gid_t>>,
    ),
    Fail,
> {
    let mut uid = None;
    let mut gid = None;
    let mut groups = None;

    if let Some(Id::Numeric(n)) = &cmd.user {
        uid = Some(*n);
    } else if let Some(Id::Name(n)) = &cmd.user {
        let c = CString::new(n.as_bytes())
            .map_err(|_| Fail::new(Stage::PreFork, sys::EINVAL, "getpwnam"))?;
        // SAFETY: NUL-terminated name; the returned struct points at static
        // storage, copied out immediately.
        let pw = unsafe { sys::getpwnam(c.as_ptr()) };
        if pw.is_null() {
            return Err(Fail::new(Stage::PreFork, sys::ENOENT, "getpwnam"));
        }
        let (u, g) = unsafe { ((*pw).pw_uid, (*pw).pw_gid) };
        uid = Some(u);
        if gid.is_none() {
            gid = Some(g);
        }
        groups = Some(group_list(&c, g)?);
    }

    match &cmd.group {
        Some(Id::Numeric(n)) => gid = Some(*n),
        Some(Id::Name(n)) => {
            let c = CString::new(n.as_bytes())
                .map_err(|_| Fail::new(Stage::PreFork, sys::EINVAL, "getgrnam"))?;
            // SAFETY: as above.
            let gr = unsafe { sys::getgrnam(c.as_ptr()) };
            if gr.is_null() {
                return Err(Fail::new(Stage::PreFork, sys::ENOENT, "getgrnam"));
            }
            gid = Some(unsafe { (*gr).gr_gid });
        }
        None => {}
    }

    Ok((uid, gid, groups))
}

fn group_list(user: &CStr, gid: sys::gid_t) -> Result<Vec<sys::gid_t>, Fail> {
    let mut want: std::ffi::c_int = 32;
    for _ in 0..8 {
        let mut v = vec![0 as sys::gid_t; want.max(1) as usize];
        let mut have = want;
        // SAFETY: the buffer is valid for `have` entries; getgrouplist writes
        // the required count back through `have` when it does not fit.
        let rc = unsafe {
            sys::getgrouplist(user.as_ptr(), gid, v.as_mut_ptr(), &mut have)
        };
        if rc >= 0 {
            v.truncate(have.max(0) as usize);
            return Ok(v);
        }
        want = have.max(1) + 8;
    }
    Err(Fail::new(Stage::PreFork, sys::EINVAL, "getgrouplist"))
}

fn cstring(s: &std::ffi::OsStr, stage: Stage, what: &'static str) -> Result<CString, Fail> {
    CString::new(s.as_bytes()).map_err(|_| Fail::new(stage, sys::EINVAL, what))
}

/// Absolute path plus the `argv[0]` to use.
fn resolve_program(cmd: &Command, _fds: &[i32; 3]) -> Result<(CString, CString), Fail> {
    let argv0 = cstring(&cmd.program, Stage::PreFork, "argv")?;
    let raw = cmd.program.as_bytes();

    if raw.contains(&b'/') {
        let p = Path::new(&cmd.program);
        let abs: PathBuf = if p.is_absolute() {
            p.to_path_buf()
        } else {
            let base = cmd
                .cwd
                .clone()
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_else(|| PathBuf::from("/"));
            base.join(p)
        };
        return Ok((cstring(abs.as_os_str(), Stage::Exec, "execve")?, argv0));
    }

    // A bare name: search PATH now, so the Exec-stage error names the path
    // that was actually tried, and a missing PATH entry is not a mystery.
    let path_env = cmd
        .env
        .iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.clone())
        .or_else(|| {
            if cmd.env_clear {
                None
            } else {
                std::env::var_os("PATH")
            }
        })
        .unwrap_or_else(|| OsString::from(DEFAULT_PATH));

    for dir in path_env.as_bytes().split(|b| *b == b':') {
        let dir = if dir.is_empty() { b".".as_slice() } else { dir };
        let mut cand = PathBuf::from(OsString::from_vec(dir.to_vec()));
        cand.push(&cmd.program);
        if is_executable(&cand) {
            return Ok((cstring(cand.as_os_str(), Stage::Exec, "execve")?, argv0));
        }
    }
    Err(Fail::new(Stage::Exec, sys::ENOENT, "PATH").with_path(PathBuf::from(&cmd.program)))
}

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

pub fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}
