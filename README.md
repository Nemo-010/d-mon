# d-mon

Daemonize a program, supervise it, and reap when it is allowed to.
One static binary, zero dependencies, usable as a CLI and as a library.

**Status: skeleton.** The design is settled (see [PLAN.md](PLAN.md)) and the
protocol types are final; the fork sequence and supervisor loop land in M2–M5.
`daemonize()` and `run()` currently return `ENOSYS`.

## Why another one

Because the existing ones cannot tell you whether your program started.

Measured:

| | result |
|---|---|
| `busybox start-stop-daemon -S -b -x /nonexistent/prog` | exit **0**, no output |
| the same with `-m -p x.pid` | pidfile written, process absent |
| `fiorix/go-daemon` | reports the exec error, but `main` ignores `waitpid` and exits **0** |
| glibc `daemon(3)` with a broken `/dev/null` | forks, `_exit`s the parent, *then* returns `-1` in a child nobody is watching |

`d-mon` returns `Ok` only when the program really reached `execve`, and a
failed start is an `Err`, so `?` cannot swallow it.

## Design contract

* **Startup is honest.** Three outcomes: started, failed (with stage, errno and
  syscall), unconfirmed. Exit codes `0` / `1` / `2`.
* **Nothing is assumed.** No shell, no `/proc`, no `/dev/null`, no `setsid(1)`,
  no `/etc/passwd` for numeric ids. Each absence degrades, and the degradation
  is reported.
* **Reaping is explicit.** `getpid() == 1` reaps automatically; otherwise
  becoming a subreaper is opt-in, because the attribute survives `execve` and
  silently converting an embedding host into a reaper is a bug, not a feature.
* **The child gets a clean signal state.** The blocked mask, and Rust's
  `SIGPIPE = SIG_IGN`, are reset before `execve` — otherwise the daemon starts
  with `SIGTERM` blocked and `docker stop` becomes `docker kill`.

## Layout

```
src/sys.rs    raw FFI; the only unsafe surface in the crate
src/ready.rs  readiness records and the launcher state machine
src/daemon.rs detach sequence
src/reaper.rs subreaper lifecycle, waitpid policy
src/command.rs the builder
```

## Build

```sh
cargo build --release
```

Static:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Licence

0BSD. See [LICENSE](LICENSE).