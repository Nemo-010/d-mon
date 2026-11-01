//! The CLI. Thin by design: it parses arguments into a [`Command`] and calls
//! the same library entry points an embedder would.

use std::ffi::OsString;
use std::process::ExitCode;
use std::time::Duration;

use d_mon::command::{Command, Id, Restart, StopPolicy};
use d_mon::error::Error;
use d_mon::sys;
use d_mon::Stdio;

const USAGE: &str = "\
usage: d-mon [options] [--] prog [args...]

  -l, --logfile PATH     capture stdout+stderr into PATH
  -p, --pidfile PATH     lock and write a pidfile
  -C, --chdir DIR        chdir before exec
  -u, --user USER|UID    drop privileges
  -g, --group GROUP|GID  set the group
  -e, --env VAR=VAL      add an environment variable (repeatable)
  -t, --timeout MS       readiness budget (default 5000)
  -r, --restart          respawn the child when it exits
  -f, --foreground       do not detach; supervise in place
  -k, --no-close         leave stdio as inherited
  -R, --reap             become a child subreaper where the kernel allows
      --stop-signal SIG  signal sent on stop (default TERM)
      --stop-timeout S   grace before SIGKILL (default 10)
      --json             machine-readable outcome
  -v, --verbose          say what is happening
  -h, --help             this text
  -V, --version          print the version

exit: 0 started, 1 failed to start, 2 unconfirmed, 3 already running
";

struct Opts {
    logfile: Option<OsString>,
    pidfile: Option<OsString>,
    chdir: Option<OsString>,
    user: Option<OsString>,
    group: Option<OsString>,
    envs: Vec<OsString>,
    timeout_ms: u64,
    restart: bool,
    foreground: bool,
    no_close: bool,
    reap: bool,
    stop_signal: OsString,
    stop_secs: u64,
    json: bool,
    verbose: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            logfile: None,
            pidfile: None,
            chdir: None,
            user: None,
            group: None,
            envs: Vec::new(),
            timeout_ms: 5000,
            restart: false,
            foreground: false,
            no_close: false,
            reap: false,
            stop_signal: OsString::from("TERM"),
            stop_secs: 10,
            json: false,
            verbose: false,
        }
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).peekable();
    let mut o = Opts::default();

    while let Some(a) = args.next() {
        let s = a.to_string_lossy().into_owned();
        match s.as_str() {
            "--" => break,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("d-mon {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "-l" | "--logfile" => o.logfile = args.next(),
            "-p" | "--pidfile" => o.pidfile = args.next(),
            "-C" | "--chdir" => o.chdir = args.next(),
            "-u" | "--user" => o.user = args.next(),
            "-g" | "--group" => o.group = args.next(),
            "-e" | "--env" => match args.next() {
                Some(v) => o.envs.push(v),
                None => return fail(&o, "--env needs VAR=VAL"),
            },
            "-t" | "--timeout" => match number(&mut args) {
                Some(n) => o.timeout_ms = n,
                None => return fail(&o, "--timeout needs milliseconds"),
            },
            "--stop-timeout" => match number(&mut args) {
                Some(n) => o.stop_secs = n,
                None => return fail(&o, "--stop-timeout needs seconds"),
            },
            "--stop-signal" => match args.next() {
                Some(v) => o.stop_signal = v,
                None => return fail(&o, "--stop-signal needs a signal"),
            },
            "-r" | "--restart" => o.restart = true,
            "-f" | "--foreground" => o.foreground = true,
            "-k" | "--no-close" => o.no_close = true,
            "-R" | "--reap" => o.reap = true,
            "--json" => o.json = true,
            "-v" | "--verbose" => o.verbose = true,
            _ if s.starts_with('-') => return fail(&o, &format!("unknown option {s}")),
            _ => {
                let mut tail = vec![a];
                tail.extend(args);
                return run(&o, tail);
            }
        }
    }

    fail(&o, "no program given (try --help)")
}

fn number(args: &mut impl Iterator<Item = OsString>) -> Option<u64> {
    args.next()?.to_str()?.parse().ok()
}

fn run(o: &Opts, tail: Vec<OsString>) -> ExitCode {
    let mut it = tail.into_iter();
    let Some(prog) = it.next() else {
        return fail(o, "no program given");
    };

    let stop_signal = match parse_signal(&o.stop_signal) {
        Some(s) => s,
        None => return fail(o, "unknown --stop-signal"),
    };

    let mut cmd = Command::new(&prog).args(it);
    if let Some(p) = &o.logfile {
        cmd = cmd
            .log_file(p)
            .stdout(Stdio::Capture)
            .stderr(Stdio::Capture);
    }
    if let Some(p) = &o.pidfile {
        cmd = cmd.pid_file(p);
    }
    if let Some(d) = &o.chdir {
        cmd = cmd.current_dir(d);
    }
    if let Some(u) = &o.user {
        cmd = cmd.user(as_id(u));
    }
    if let Some(g) = &o.group {
        cmd = cmd.group(as_id(g));
    }
    for e in &o.envs {
        if let Some(s) = e.to_str() {
            if let Some((k, v)) = s.split_once('=') {
                cmd = cmd.env(k, v);
            }
        }
    }
    cmd = cmd
        .ready_timeout(Duration::from_millis(o.timeout_ms))
        .stop(StopPolicy {
            signal: stop_signal,
            grace: Duration::from_secs(o.stop_secs),
        })
        .reap(o.reap);
    if o.restart {
        cmd = cmd.restart(Restart::Always {
            max: u32::MAX,
            backoff: Duration::from_secs(1),
        });
    }
    if o.no_close {
        cmd = cmd
            .stdin(Stdio::Inherit)
            .stdout(Stdio::Inherit)
            .stderr(Stdio::Inherit);
    } else if !o.foreground {
        // A detached delegate that keeps the caller's stdout holds the pipe
        // open, so `out=$(d-mon prog)` never returns. Discard is the default
        // off the terminal; `-k` is how you ask for inheritance anyway.
        cmd = cmd.stdin(Stdio::Discard);
        if o.logfile.is_none() {
            cmd = cmd.stdout(Stdio::Discard).stderr(Stdio::Discard);
        }
    }

    let outcome = if o.foreground {
        match cmd.run() {
            Ok(st) => {
                let code = st.code().unwrap_or_else(|| 128 + st.signal().unwrap_or(0));
                if o.json {
                    println!("{{\"outcome\":\"exited\",\"code\":{code}}}");
                }
                return ExitCode::from(code.clamp(0, 255) as u8);
            }
            Err(e) => Err(e),
        }
    } else {
        cmd.daemonize().map(|s| {
            if o.json {
                println!(
                    "{{\"outcome\":\"started\",\"mode\":\"resident\",\"supervisor\":{},\"child\":{}}}",
                    s.supervisor(),
                    s.child()
                );
            }
        })
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => report(o, e),
    }
}

use std::os::unix::process::ExitStatusExt;

fn as_id(s: &OsString) -> Id {
    match s.to_str().and_then(|v| v.parse::<u32>().ok()) {
        Some(n) => Id::Numeric(n),
        None => Id::Name(s.to_string_lossy().into_owned()),
    }
}

fn parse_signal(s: &OsString) -> Option<i32> {
    let t = s.to_string_lossy().to_ascii_uppercase();
    let t = t.strip_prefix("SIG").unwrap_or(&t);
    Some(match t {
        "HUP" => sys::SIGHUP,
        "INT" => sys::SIGINT,
        "QUIT" => sys::SIGQUIT,
        "TERM" => sys::SIGTERM,
        "USR1" => sys::SIGUSR1,
        "USR2" => sys::SIGUSR2,
        "KILL" => sys::SIGKILL,
        other => other.parse().ok()?,
    })
}

fn report(o: &Opts, e: Error) -> ExitCode {
    let code = e.exit_code();
    if o.json {
        let body = match &e {
            Error::Spawn {
                program,
                stage,
                errno,
                syscall,
                path,
            } => {
                let mut s = format!(
                    "{{\"outcome\":\"failed\",\"stage\":\"{stage}\",\"errno\":{errno},\"syscall\":\"{syscall}\",\"program\":\"{}\"",
                    escape(&program.to_string_lossy())
                );
                if let Some(p) = path {
                    s.push_str(&format!(",\"path\":\"{}\"", escape(&p.to_string_lossy())));
                }
                s.push('}');
                s
            }
            Error::Unconfirmed { waited, killed } => format!(
                "{{\"outcome\":\"unconfirmed\",\"waited_ms\":{},\"killed\":{killed}}}",
                waited.as_millis()
            ),
            Error::AlreadyRunning { path, pid } => format!(
                "{{\"outcome\":\"already-running\",\"pidfile\":\"{}\",\"pid\":{}}}",
                escape(&path.to_string_lossy()),
                pid.map(|p| p.to_string()).unwrap_or_else(|| "null".into())
            ),
            Error::Env { op, errno, .. } => {
                format!("{{\"outcome\":\"env\",\"op\":\"{op}\",\"errno\":{errno}}}")
            }
            _ => "{\"outcome\":\"failed\"}".to_string(),
        };
        println!("{body}");
    } else {
        eprintln!("d-mon: {e}");
    }
    ExitCode::from(code)
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn fail(o: &Opts, msg: &str) -> ExitCode {
    if o.json {
        println!("{{\"outcome\":\"error\",\"message\":\"{}\"}}", escape(msg));
    } else {
        eprintln!("d-mon: {msg}");
    }
    ExitCode::from(1)
}