# d-mon

Daemonize a program, supervise it, and reap when it is allowed to.
One static binary, zero dependencies, usable as a CLI and as a library.

**Status: M1–M6 done.** The detach sequence, the exec contract, the supervisor,
the log, the pidfile and the control verbs are implemented and tested (35
integration tests, 10 unit tests). 463,536-byte static `x86_64-unknown-linux-musl`
binary. The design and the defects it was corrected for are in
[PLAN.md](PLAN.md); the PID 1 path is written but untested here, because this
kernel refuses `unshare -pf`.

## Why another one

Because a daemonizer that cannot say whether the program started is not one you
can put in a unit file or a container entrypoint, and a daemonizer that leaves
its delegate able to acquire a terminal is not a daemonizer.

Measured in this checkout (`x86_64`, glibc, kernel 7.2):

| probe | result |
|---|---|
| `catatonit -- /nonexistent/prog` | exit **1**, names the exec failure — honest, and the standard to match |
| `catatonit -- /bin/sh -c 'exit 3'` | exit **3**, the delegate's status |
| glibc `daemon(0,0)`, via [`probes/daemon3-session.c`](probes/daemon3-session.c) | the surviving process has `getsid(0) == getpid()`: it is a **session leader**, so a single fork plus `setsid` |
| `d-mon … /bin/sleep 5` | the supervisor is **neither a session leader nor a process group leader**; both belong to the intermediate, which exited |

Quoted from [PLAN.md](PLAN.md) §1, not re-measured here because the tool is not
installed in this environment:

| claim | source |
|---|---|
| `busybox start-stop-daemon -S -b -x /nonexistent/prog` → exit **0**, no output; with `-m -p` it writes a pidfile for a process that does not exist | PLAN §1, from the `-b` path returning before `execve` |
| `fiorix/go-daemon` reports the exec error but `main` ignores `waitpid` and exits **0** | PLAN §1, from `god.c` |

```
$ d-mon /nonexistent/prog
d-mon: /nonexistent/prog: failed at exec: ENOENT (execve)
$ echo $?
1
```

## Design contract

* **Startup is honest.** Three outcomes: started, failed (with stage, errno and
  syscall), unconfirmed. Exit codes `0` / `1` / `2`, and `3` for a pidfile that
  is already locked.
* **Nothing is assumed.** No shell, no `/proc`, no `/dev/null`, no `/etc/passwd`
  for numeric ids. Each absence degrades, and the degradation is reported.
* **Reaping is explicit.** `getpid() == 1` reaps automatically; otherwise
  becoming a subreaper is opt-in, because the attribute survives `execve` and
  silently converting an embedding host into a reaper is a bug, not a feature.
* **The child gets a clean signal state.** The blocked mask, and Rust's
  `SIGPIPE = SIG_IGN`, are reset before `execve` — otherwise the daemon starts
  with `SIGTERM` blocked and `docker stop` becomes `docker kill`.
* **The child inherits nothing above fd 2**, and the readiness pipe closes on
  `execve`, which is how the launcher learns the program really ran.

## Usage

```sh
d-mon -l /var/log/app.log -p /run/app.pid -- /usr/bin/app --serve
d-mon -f -- /usr/bin/app --serve          # supervise in place (container entrypoint)
d-mon --json -p app.pid -- /usr/bin/app   # one machine-readable line

d-mon status -p /run/app.pid               # exit 0 alive, 1 not
d-mon signal USR2 -p /run/app.pid          # reached the delegate, not a stranger
d-mon stop -p /run/app.pid                 # exit 0 only once it is really gone
```

Detached, the delegate's stdio defaults to discarded; `-l` captures it into the
log, and `-k` leaves it as inherited.

## Control without trusting the pid

A pidfile is a claim, not evidence — the pid inside can have been recycled.
`status`, `signal` and `stop` therefore do not believe it: they try to take the
pidfile's `flock` themselves. If the lock can be taken, nobody holds it, the
file is a leftover, and nothing is signalled. If it cannot, the holder is alive
and is the process that wrote the pid. No `/proc`, no start-time comparison, and
no way to kill a stranger that inherited a recycled pid.

## Library

```rust
use d_mon::Command;

let started = Command::new("/usr/bin/app")
    .arg("--serve")
    .log_file("/var/log/app.log")
    .pid_file("/run/app.pid")
    .ready_timeout(std::time::Duration::from_secs(5))
    .daemonize()?;
println!("supervisor {}, child {}", started.supervisor(), started.child());
```

`run()` supervises in the current process and returns the delegate's exit
status; `daemonize()` returns as soon as the outcome is known.

One `Supervisor` per process at a time: a signal handler is process-wide, so a
second one gets `EBUSY` rather than racing the first for the self-pipe.

## Layout

```
src/sys.rs        raw FFI; the only unsafe surface in the crate
src/ready.rs      readiness records and the launcher state machine
src/exec.rs       ExecPlan: everything materialised before the fork
src/daemon.rs     the detach sequence
src/ctl.rs        status / signal / stop, verified by the pidfile lock
src/supervisor.rs the poll loop: forwarding, restart, stop policy, drains
src/reaper.rs     subreaper lifecycle, waitpid policy
src/log.rs        append sink, inode-verified reopen on SIGHUP
src/pidfile.rs    flock on the final inode, verified against the path
src/stdio.rs      Discard's degradation chain, capture pipes
src/main.rs       the CLI
```

## Build

```sh
cargo build --release
cargo test

rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Licence

0BSD. See [LICENSE](LICENSE).