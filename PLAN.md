# d-mon — design (v3, after two review rounds)

`d-mon` daemonizes a program, supervises it, and reaps when it is allowed to.
CLI and library. Rust, zero dependencies, static build. 0BSD.

v3 is the result of two rounds of adversarial review (8 reviewers). Every
change is marked with the round that forced it. v1/v2 are in git history.

---

## 0. Defects found in review, and what replaced them

| v2 said | round | verdict | v3 |
|---|---|---|---|
| one pipe, supervisor writes `'S'` | 2 | supervisor holding a write end means EOF never arrives → every start `Unconfirmed` | **two record types on one pipe, explicit close-after-fork** |
| child writes `'A'` before `execve` | 2 | `'A'`+errno and `'A'`+SIGKILL are reachable; `'A'` alone reports Started for a dead process | **`'A'` deleted**; `HELLO` carries the pid, EOF carries execve |
| `{magic, errno}` | 2 | no stage byte; `EACCES` from chdir vs setuid indistinguishable | **`{magic, stage, errno}`** |
| timeout → exit 2, daemon left running | 2 | operator retries, two daemons | **kill the process group, reap, then report** |
| pidfile: `O_EXCL` tmp + rename + flock | 2 | locks the tmp inode, not the path; B renames over A; a stale tmp bricks future starts | **flock the final inode** |
| `PR_CAPBSET_DROP` after `setuid` | 2 | needs `CAP_SETPCAP`; fails `EPERM`; fatal → every `--user` run dies | **scrub before setuid** |
| `/dev/zero` then `memfd` as `/dev/null` | 2 | `/dev/zero` never EOFs on read (spin); memfd grows unbounded (OOM) | **`/dev/null` → drained pipe → inherit** |
| `waitpid(-1)` under `Reap::Subreaper` | 2 | steals the host's children in library mode; subreaper is not inherited but *is* preserved across exec | **subreaper only in `daemonize()`, never in `run()`** |
| no `PR_SET_PDEATHSIG` | 2 | kill the supervisor → orphan tree survives, unfindable | **added, with the `getppid()==1` recheck** |
| `setsid` without a second fork | 2 | the process stays a session leader and can reacquire a ctty — the exact glibc bug criticised in §1 | **double fork mandatory** |
| PID 1 forwards to "the child group" | 2 | adopted orphans are in their own groups | **`kill(-1, sig)` as PID 1** |
| `Id::Name` via `getpwnam` | 2 | no `/etc/passwd` in a scratch container | **`Id::Numeric` is the scratch path; name is an explicit pre-fork error** |
| matrix: god exit 1 on setsid fail | 2 | false: god's `main` ignores `waitpid` status and returns 0 | **corrected, with source** |
| matrix omits tini/catatonit/s6/supervisord | 2 | the omission decides the verdict | **included** |
| `open("/")` as `/dev/null` | 1 | `O_RDONLY` dirfd: writes `EBADF`, and an escape primitive | **removed entirely** |
| blocked mask / `SIGPIPE=SIG_IGN` inherited | 1 | target starts with SIGTERM blocked | **reset in child, asserted in tests** |

## 1. Problem

| Defect in the field | Evidence |
|---|---|
| a daemonizer cannot report a failed start | `busybox start-stop-daemon -S -b -x /nonexistent/prog` → **exit 0** |
| ...and leaves a pidfile anyway | `-m -p` → pidfile written, process absent |
| `daemon(3)` half-fails silently | forks and `_exit`s the parent, *then* returns -1 in a child nobody watches |
| glibc `daemon(3)` is not a double fork | man page: "does not employ the double-fork technique" |
| `god` reports the exec error but not the exit | `god.c`: `main` does `waitpid(pid, NULL, 0)` and `return 0` |
| `/dev/null` assumed | `bmc/daemonize`: `die("Can't open /dev/null")` |
| correct readiness exists only in libraries | `immortal/fork::checked_daemon` |

## 2. Goals

G1 **Startup is reported honestly**, with `?`-safe types and three states.
G2 **Degrade for environmental failures**; privilege drop and an explicit
   `--chdir`/`--user` failure are fatal.
G3 **Reap when allowed**, never silently.
G4 **No shell, no interpreter, no helper binaries.** One static executable.
G5 **CLI and library share one code path.**
G6 **Capture output without `/dev/null`.**

## 3. Constraint model

| Assumption | If absent |
|---|---|
| Linux ≥ 3.4 | no subreaper → `Reap::Unavailable` |
| `/dev/null` | drained pipe, else inherit |
| `/proc` | never read; `close_range` if present, else `_SC_OPEN_MAX` loop |
| `setsid(2)` | fatal: without a session the process can reacquire a ctty |
| `fork(2)` | fatal with a clear error |
| `/etc/passwd` | `Id::Numeric` works without it; `Id::Name` is a pre-fork error |
| a shell | never invoked |

**Known limitation:** Rust's std calls `sanitize_standard_fds()` before `main`
and `abort()`s if fd 0/1/2 are closed *and* `/dev/null` is missing. `d-mon`
cannot fix that; the OCI spec requires a runtime to provide `/dev/null`. The
readiness protocol reports it as a failure (§5.1, "EOF with no record").

## 4. Architecture

Zero crates. `std` plus one `sys` module of `extern "C"`.

```
src/
  lib.rs        public API
  sys.rs        unsafe FFI, raw syscalls; every call names its EINTR policy
  error.rs      Error { op, stage, errno, path, pid, syscall }
  command.rs    Command builder
  stdio.rs      Stdio: Inherit | Discard | Capture | File
  exec.rs       ExecPlan: everything materialised pre-fork
  ready.rs      ReadyPipe records, launcher state machine
  daemon.rs     detach sequence, Mode, Started
  supervisor.rs event loop, owned-pid set, forwarding, restart, stop policy
  reaper.rs     Reap state, subreaper lifecycle
  log.rs        dirfd-pinned append sink, inode-verified reopen
  pidfile.rs    flock on the final inode, no rename
  bin/dm.rs     CLI
```

### API

```rust
pub struct Command { /* ... */ }

impl Command {
    pub fn new(program: impl AsRef<OsStr>) -> Self;
    pub fn arg(self, a: impl AsRef<OsStr>) -> Self;
    pub fn env(self, k: impl AsRef<OsStr>, v: impl AsRef<OsStr>) -> Self;
    pub fn env_clear(self) -> Self;
    pub fn current_dir(self, p: impl AsRef<Path>) -> Self;
    pub fn umask(self, mode: u32) -> Self;
    pub fn user(self, id: Id) -> Self;          // Id::Numeric(u32) | Id::Name(String)
    pub fn group(self, id: Id) -> Self;
    pub fn stdout(self, s: Stdio) -> Self;
    pub fn stderr(self, s: Stdio) -> Self;
    pub fn log_file(self, p: impl AsRef<Path>) -> Self;
    pub fn pid_file(self, p: impl AsRef<Path>) -> Self;
    pub fn restart(self, r: Restart) -> Self;
    pub fn stop(self, s: StopPolicy) -> Self;   // [R2] signal + grace
    pub fn ready_timeout(self, d: Duration) -> Self;

    pub fn daemonize(self) -> Result<Started, Error>;
    pub fn run(self) -> Result<ExitStatus, Error>;
}

pub enum Mode {                                  // [R2]
    Resident { supervisor: Pid, child: Pid },    // d-mon survives to supervise
    Replaced { child: Pid },                     // d-mon exec'd itself away
}

pub struct Started { pub mode: Mode }

pub enum Error {
    Spawn   { program: OsString, stage: Stage, errno: i32, syscall: &'static str, path: Option<PathBuf> },
    Unconfirmed { waited: Duration, killed: bool },
    Env     { op: &'static str, errno: i32, syscall: &'static str },
}

pub enum Stage { PreFork, Open, Chdir, Privileges, Exec }
pub enum Reap  { Pid1, Subreaper, Unavailable }
```

`daemonize()` returns `Result`, so `?` never swallows a failure.
CLI exit codes: `0` started, `1` failed, `2` unconfirmed, `3` already running.

## 5. Protocols

### 5.1 Readiness — **[R2] final**

One pipe. Fixed 16-byte records, all writes ≤ `PIPE_BUF` (atomic). Write end
`O_CLOEXEC`.

```
REC_SUPER { magic, pid }     supervisor, before forking the child
REC_HELLO { magic, pid }     child, after all setup, immediately before execve
REC_FAIL  { magic, stage, errno }   instead of execve, on failure
EOF                          the write end closed
```

**Close discipline is normative:** the supervisor writes `REC_SUPER`, forks the
child, then closes its own copy *before* anything else. The child is the only
remaining holder, and `execve` closes it via `O_CLOEXEC`. Without this rule the
launcher waits forever — the v1/v2 bug.

Launcher state machine:

| record sequence | verdict |
|---|---|
| `REC_FAIL` | **Failed** at `stage`, with `errno` |
| `REC_HELLO` then EOF, pid alive (`kill(pid,0)`) | **Started** |
| `REC_HELLO` then EOF, pid gone | **Failed** at `Exec` |
| EOF with no record | **Failed** at `PreFork` (setup died; catches the std abort, OOM, SIGKILL) |
| nothing within timeout | **Unconfirmed** → `kill(-pgid, SIGKILL)`, reap, report |

`REC_HELLO` proves the process reached the last step; EOF proves `execve`
closed the fd; the `kill(pid,0)` check catches immediate death. A process that
execs and then fails to bind is still `Started` — no tool can know without
cooperation, and the docs say so.

### 5.2 Exec contract — **[R2] raw syscalls, not libc**

Pre-fork, materialised into `ExecPlan`: `argv`/`envp` as `CString`, program
path resolved absolute, `getpwnam`/`getgrnam`/`getgrouplist` resolved, log and
pidfile opened, `close_range` plan chosen.

In the child, between `fork` and `execve`, **only** these, via `SYS_*`:

```
dup2 close close_range rt_sigprocmask rt_sigaction setpgid setsid
setgroups setgid setuid umask chdir prctl write execve exit_group
```

`initgroups` is **not** used in the child (it mallocs, reads `/etc/group`, runs
NSS, is not async-signal-safe). The group list is resolved pre-fork and applied
with `setgroups`.

**Signal state is reset**: empty mask, `SIG_DFL` for every signal d-mon
touched *and* `SIGPIPE` (Rust sets it to `SIG_IGN`, and ignored dispositions
survive `execve`).

**`setpgid(0,0)` is only called when `setsid` failed** — after a successful
`setsid`, `setpgid` returns `EPERM` because the process is a session leader.

**`setsid` failure is fatal**: without a new session the process keeps the
caller's session and can reacquire a controlling terminal.

**`PR_SET_PDEATHSIG(SIGTERM)`** in the child, followed by
`getppid() == 1 → _exit`, closing the race.

### 5.3 Reaping — **[R2] ownership is explicit**

`Supervisor` owns a pid set. `waitpid(owned, WNOHANG)`; drain terminates on
`ECHILD` *or* `0`.

`waitpid(-1, WNOHANG)` and `PR_SET_CHILD_SUBREAPER` are used **only** in the
resident supervisor of `daemonize()`. `run()` never sets subreaper in the
embedding process: the attribute survives `execve` and would silently convert
the host into a reaper.

`Reap::Unavailable` is returned honestly, with the reason.

### 5.4 PID 1

Handlers for every signal acted on (a `SIG_DFL` signal is not deliverable to
namespace init from inside the namespace). Shutdown forwards with
`kill(-1, sig)` — init semantics, reaching orphans in their own groups — then
`SIGKILL` after `StopPolicy::grace`. Never exit while descendants live.

### 5.5 Log and pidfile — **[R2] no rename**

* log: `openat(dirfd, name, O_WRONLY|O_APPEND|O_CREAT|O_NOFOLLOW|O_CLOEXEC, 0640)`;
  dirfd pinned pre-drop; `st_dev`/`st_ino` compared on `SIGHUP`; old fd kept on
  mismatch.
* pidfile: `open(path, O_RDWR|O_CREAT|O_NOFOLLOW|O_CLOEXEC, 0644)` then
  `flock(LOCK_EX|LOCK_NB)`. `EWOULDBLOCK` → exit 3. Then `ftruncate` + write +
  `fsync`. **No tmp file, no rename** — the lock is on the inode the path
  actually names. Holds the supervisor pid. Unlinked on clean exit.

### 5.6 Privilege drop — **[R2] corrected order**

```
open log + pidfile
chdir
PR_SET_NO_NEW_PRIVS
PR_CAPBSET_DROP (loop, while CAP_SETPCAP still held)
SECBIT_NOROOT
setgroups(pre-resolved list)
setgid
setuid
umask
```
Each step fatal. The bounding-set scrub must precede `setuid`, which clears the
effective set and would make the scrub fail `EPERM`.

## 6. CLI

```
d-mon [options] [--] prog [args...]
  -l --logfile  -p --pidfile  -C --chdir  -u --user  -g --group
  -e --env  -t --timeout  -r --restart  -f --foreground  -k --no-close
  -R --reap  --stop-signal SIG  --stop-timeout SECS  --json  -v
```

`--json` emits one line:
`{"outcome":"started","mode":"resident","supervisor":12,"child":13}`
`{"outcome":"failed","stage":"exec","errno":2,"syscall":"execve","program":"/x"}`

## 7. Failure matrix — **[R2] corrected**

| Situation | d-mon | god | busybox s-s-d | bmc/daemonize | tini | catatonit |
|---|---|---|---|---|---|---|
| target missing | exit 1 + stage + errno | exit 1 + log | **exit 0** | n/a | n/a | n/a |
| died during setup | **exit 1** | logged only | **exit 0** | n/a | n/a | n/a |
| setsid fails | **fatal, explicit** | **exit 0** (status ignored) | ignored | ignored | n/a | n/a |
| no `/dev/null` | drained pipe→inherit | guarded | `open("/")` | **die** | n/a | n/a |
| no `/proc` | unaffected | unaffected | affected | unaffected | unaffected | unaffected |
| no `/etc/passwd` | numeric ids | n/a | n/a | n/a | n/a | n/a |
| PID 1 | reaps, `kill(-1)`, no early exit | no | no | no | **yes** (`-s`) | **yes** |
| supervisor death | `PDEATHSIG` | `PDEATHSIG` | n/a | n/a | `-p` | n/a |

Honest note: on the *init* role d-mon does not beat tini or catatonit, which
are older and simpler; it beats them only by also being a daemonizer and a
library. On supervision it does not beat s6 or supervisord, which have control
interfaces d-mon lacks (M6 scope).

## 8. Test plan

* readiness: exec-ok / exec-fail / hang / killed pre-exec / supervisor killed
  pre-fork / EOF with no record — six cases, one per state
* signal: target does not inherit blocked SIGTERM or ignored SIGPIPE
* fd: target inherits nothing above 2
* reaping: 100 grandchildren, zero zombies; `run()` does not touch a
  co-resident `std::process::Child`
* pidfile: two concurrent starts → second exits 3; stale file does not brick
* static: `--target x86_64-unknown-linux-musl`, `ldd` → "not a dynamic executable"
* PID 1: `unshare -pf` where permitted; skip loudly where not

## 9. Milestones

M1 skeleton, sys FFI, `--help`, static build
M2 readiness (six states) + `--json`
M3 exec contract, signal reset, privilege order
M4 supervisor, log, pidfile, stop policy
M5 reaper, PID 1
M6 control verbs (`status`/`signal`), docs, matrix

## 10. Remaining open questions

1. `StopPolicy` default: `SIGTERM` with a 10 s grace, then `SIGKILL`?
2. Should `--restart` cap crash-loops (`max` in window) by default, or is that
   policy the caller's?
3. Is a control verb (`status`) in scope for 1.0, or does it belong to a
   separate `d-mon-ctl` reading the pidfile?
