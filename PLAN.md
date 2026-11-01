# d-mon — design (v4, as implemented)

`d-mon` daemonizes a program, supervises it, and reaps when it is allowed to.
CLI and library. Rust, zero dependencies, static build. 0BSD.

v3 was the result of two rounds of adversarial review (8 reviewers). v4 is v3
after implementation: writing the fork sequence found ten places where the
design was wrong or incomplete. They are listed in §0.1. The third review round
was the compiler and the machine, so those changes are marked **[R3]**.

**Status: M1–M6 implemented and tested.** 35 integration tests and 13 unit
tests pass; one more is marked `#[ignore]` because it needs a PID namespace
(see §9). The static `x86_64-unknown-linux-musl` binary is 463,536 bytes.

---

## 0.1 Defects found while implementing v3, and what replaced them

| v3 said | why it broke | v4 |
|---|---|---|
| the child calls `setsid()` | after the double fork the child is already in a session with no controlling terminal *and* is not a session leader, which is what actually prevents ctty reacquisition. `setsid` there would make it a leader again — the `daemon(3)` defect of §1 | no `setsid` in the child; the intermediate calls it and the survivor keeps that session **[R3]** |
| `setpgid` fallback when `setsid` fails | `setsid` failure is fatal, so the branch is unreachable | removed **[R3]** |
| `Mode::Replaced` | nothing produces it; a variant no code returns is a lie in the API | `Mode::Resident` only, `#[non_exhaustive]` **[R3]** |
| stdio: `Inherit` everywhere | a detached supervisor that keeps the caller's stdout holds the write end of `out=$(d-mon prog)`, so the pipeline never returns | the supervisor points its own 0/1/2 at `/dev/null` after the fork; the CLI defaults a detached delegate to `Discard`, and `-k` restores inheritance **[R3]** |
| `waitpid(-1)` when a subreaper | `run()` became a subreaper whenever reaping was allowed, and then reaped *the embedding process's* children | subreaper only on the detached path or an explicit `reap(true)`; otherwise the loop waits on the pid it forked **[R3]** |
| pidfile: `flock` the final inode | correct, but a starter could lock an inode the exiting supervisor had just unlinked, and then hold a lock nobody can see | after `flock`, `fstat(fd)` and `stat(path)` are compared and the attempt retried on a mismatch; the file is unlinked *before* the lock is released **[R3]** |
| restart policy applies to any exit | a `-r` supervisor answered SIGTERM by spawning the child again, so it never stopped | a stop request outranks the restart policy **[R3]** |
| one readiness pipe, the supervisor announces itself | in `run()` the supervisor *is* the launcher, so no `Super` record is ever sent and a good start reads as "EOF with no record" | the tracker is seeded with the supervisor's own pid/pgid in `run()` **[R3]** |
| timeout kills "the process group" | with a zero budget the launcher may not have received `Super` yet, so it knew no pid and killed nothing | it always kills the intermediate, which is the session and process group of everything the attempt created **[R3]** |
| a pidfile is always removed | a SIGKILLed supervisor cannot remove it | removal is promised on clean exit only; the lock dies with the process, so a leftover never blocks a start **[R3]** |

Also **[R3]**: a signal handler belongs to the process, so exactly one
`Supervisor` may be live in a process. A second one gets `EBUSY` instead of
silently writing to the first one's self-pipe.

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
| `daemon(3)` leaves the survivor able to take a terminal | **measured**: `probes/daemon3-session.c` reports `getsid(0) == getpid()`, i.e. a session leader |
| `catatonit` is honest about a failed start | **measured**: `catatonit -- /nonexistent/prog` → exit 1, and it propagates the delegate's status |

Provenance: the two rows marked **measured** were run when M6 landed, on this
machine. The rest come from the source or man page named in the row and were
not re-run here — busybox, tini, s6 and supervisord are not installed in this
environment, so quoting them as "measured here" would be a claim I cannot
support.

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

pub enum Mode {                                  // [R3]
    Resident { supervisor: Pid, child: Pid },    // d-mon survives to supervise
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

### 5.2 Exec contract — **[R3] raw syscalls, not libc**

Pre-fork, materialised into `ExecPlan`: `argv`/`envp` as `CString` and their
`NULL`-terminated pointer arrays (the child must not allocate, because a `fork`
in a threaded process can leave the allocator locked by a thread that does not
exist in the child), program path resolved absolute, `getpwnam`/`getgrnam`/
getgrouplist` resolved, log, pidfile and stdio descriptors opened.

In the child, between `fork` and `execve`, **only** these:

```
prctl dup2 fcntl(F_SETFD) rt_sigprocmask sigaction setgroups setgid setuid
umask chdir write execve exit_group
```

`setsid`, `setpgid` and `close_range` are **not** in the list. The session was
created by the intermediate before the supervisor was forked, so the child
inherits a ctty-less session in which it is not a leader — the state that makes
ctty reacquisition impossible. Descriptors above 2 are marked `FD_CLOEXEC` one
by one rather than closed, because `close_range` would take the readiness pipe
with it and `CLOSE_RANGE_CLOEXEC` needs kernel 5.11.

`initgroups` is **not** used in the child (it mallocs, reads `/etc/group`, runs
NSS, is not async-signal-safe). The group list is resolved pre-fork and applied
with `setgroups`.

**Signal state is reset**: empty mask, `SIG_DFL` for every signal d-mon
touched *and* `SIGPIPE` (Rust sets it to `SIG_IGN`, and ignored dispositions
survive `execve`).

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

**[R3]** A stop request — `SIGTERM`, `SIGINT` or `SIGQUIT` — tells the delegate
`StopPolicy::signal`, not whichever signal asked for the stop. Otherwise
`--stop-signal INT` would only take effect when the start is aborted, since
`d-mon stop` (and every init that stops a service) arrives as `SIGTERM`.
`SIGHUP`, `SIGUSR1` and `SIGUSR2` are forwarded unchanged and are not stop
requests.

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

d-mon status  [-p PATH] [--json]
d-mon signal SIG  [-p PATH] [--json]
d-mon stop    [-p PATH] [--stop-timeout SECS] [--json]
```

The first word decides: a verb acts on a pidfile and starts nothing; anything
else is a program. `--` switches verb parsing off.

`--json` emits one line:
`{"outcome":"started","mode":"resident","supervisor":12,"child":13}`
`{"outcome":"failed","stage":"exec","errno":2,"syscall":"execve","program":"/x"}`

Detached, the delegate's stdio defaults to `Discard`, not `Inherit`: a daemon
holding the caller's stdout keeps `out=$(d-mon prog)` from ever returning.
`-k` asks for inheritance anyway; `-l` captures into the log file. `-f` keeps
inheritance, because attached supervision is the point of `-f`.

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
| status verb | **proves liveness by the pidfile lock** | own control socket | `--status` trusts the pid | n/a | n/a | n/a |
| stop verb | waits for the lock release, so exit 0 means gone | own control socket | fire and forget | n/a | n/a | n/a |

Honest note: on the *init* role d-mon does not beat tini or catatonit, which
are older and simpler; it beats them only by also being a daemonizer and a
library. On supervision it does not beat s6 or supervisord, which have richer
control interfaces (reload, per-service state) than the three verbs in §6.1.
What it does differently on control is the identity check: `status` does not
believe a pidfile, it takes the lock — see §6.1.

### 6.1 Control verbs — **[R3]**

A pidfile is a claim, not evidence. Between "the supervisor wrote 4711" and
"kill 4711" the pid can be recycled, and a `kill(pid, 0)` check answers
"somebody has that pid", which is exactly what is in doubt. So the verbs try to
take the pidfile's `flock` themselves:

| probe result | meaning | action |
|---|---|---|
| `LOCK_EX` succeeds | nobody holds it; the file is a leftover | report stopped, **never signal** |
| `EWOULDBLOCK` | a live holder exists, and it is what wrote the file | trust the pid |

```
d-mon status [-p PATH] [--json]        exit 0 alive, 1 not
 d-mon signal SIG [-p PATH] [--json]    exit 0 sent, 1 nothing to signal
d-mon stop [-p PATH] [--json]          exit 0 stopped, 1 nothing to stop, 2 still up
d-mon stop --stop-timeout SECS          how long to wait for the lock release
```

The pidfile holds two lines — supervisor, then delegate — and is rewritten under
the lock on every spawn and exit. Line 1 is unchanged in meaning, so
`kill $(cat pidfile)` still works. A reader can therefore see a torn file, which
is why the verbs re-read for up to 200 ms before reporting `Uncertain`.

`--` still means the end of options: `d-mon -- status` runs a program called
`status`.

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

M1 skeleton, sys FFI, `--help`, static build — **done**
M2 readiness (six states) + `--json` — **done** (six states unit-tested; `-t 0`
   exercises the timeout over a real pipe)
M3 exec contract, signal reset, privilege order — **done** (signal reset and fd
   hygiene asserted against `/proc/self/status` and `/proc/self/fd`)
M4 supervisor, log, pidfile, stop policy — **done** (SIGHUP rotation, grace then
   SIGKILL, exactly one winner under 25 concurrent starts)
M5 reaper, PID 1 — **done for the detached supervisor**, but the PID 1 path is
   **unverified**: this kernel refuses `unshare -pf`, so the test is `#[ignore]`d
   rather than counted as a pass it did not earn.
M6 control verbs (`status`/`signal`/`stop`), docs, matrix — **done** (§6.1)

Known gaps after M6:

* **PID 1 is untested here.** The code is written and the test exists, but this
  kernel refuses `unshare -pf`, so the test prints `SKIPPED` and the path is
  unverified until it runs somewhere that allows a PID namespace.
* `Restart::max` is the only crash-loop cap, and the CLI's `--restart` passes
  `u32::MAX`, so the CLI itself never gives up. Deliberate, but it is a gap for
  anyone who wanted `--restart` to be safe by default.
* A non-init subreaper cannot enumerate the orphans it adopted without reading
  `/proc`, which this design refuses to do, so it reports the main child's exit
  and leaves them to be reparented.
* One supervisor per process (a signal handler is process-wide), so a library
  cannot supervise two unrelated delegations from one process. Documented as
  `EBUSY`, not silently tolerated.
* The control verbs reach the supervisor, not the delegate: there is no
  `d-mon signal --delegate` and no per-service state, which is where s6 and
  supervisord are still ahead.

## 10. Open questions, answered

1. `StopPolicy` default: `SIGTERM`, 10 s grace, then `SIGKILL` — **yes**, and it
   is what `--stop-timeout` overrides.
2. Crash-loop cap: **the caller's policy**, via `Restart::max`. The CLI's
   `--restart` passes `u32::MAX`, so it does not cap; that is a deliberate
   choice for a CLI that should not invent a limit, and it is documented as a
   gap rather than a feature.
3. Control verbs: **in scope**, but as a separate `ctl` module talking to the
   pidfile rather than a socket, so there is no new listening surface and no
   `d-mon-ctl` binary to keep in step. See §6.1.
