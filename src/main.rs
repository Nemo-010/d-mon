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

verbs, which act on a pidfile instead of starting anything:

  status [-p PATH]                 is a supervisor alive? (exit 0 yes, 1 no)
  signal SIG [-p PATH]             send a signal to the supervisor (1 if none)
  stop [-p PATH] [--stop-timeout]  SIGTERM, then wait until it is really gone
                                   (0 stopped, 1 nothing to stop, 2 still up)

`d-mon -- status` runs a program called status instead.

exit: 0 ok, 1 failed / not running, 2 unconfirmed, 3 already running
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
    let mut verb: Option<Verb> = None;

    while let Some(a) = args.next() {
        let s = a.to_string_lossy().into_owned();
        match s.as_str() {
            "--" => {
                // Everything after `--` is the program, even a name that looks
                // like a verb.
                let mut tail: Vec<OsString> = Vec::new();
                if let Some(first) = args.next() {
                    tail.push(first);
                }
                tail.extend(args);
                if tail.is_empty() {
                    return fail(&o, "no program given after --");
                }
                return run(&o, tail);
            }
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
                // The first word decides: a verb acts on a pidfile, anything
                // else is a program to start.
                if verb.is_none() {
                    if let Some(v) = Verb::parse(&s) {
                        verb = Some(v);
                        continue;
                    }
                }
                if let Some(v) = verb.as_mut() {
                    if let Some(want) = v.needs_arg() {
                        if want {
                            v.give_arg(a);
                            continue;
                        }
                    }
                }
                let mut tail = vec![a];
                tail.extend(args);
                return run(&o, tail);
            }
        }
    }

    if let Some(v) = verb {
        return ctl(&o, v);
    }
    fail(&o, "no program given (try --help)")
}

/// A control verb. They never start anything; they read the pidfile.
enum Verb {
    Status,
    Signal(Option<OsString>),
    Stop,
}

impl Verb {
    fn parse(s: &str) -> Option<Verb> {
        match s {
            "status" => Some(Verb::Status),
            "signal" => Some(Verb::Signal(None)),
            "stop" => Some(Verb::Stop),
            _ => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Verb::Status => "status",
            Verb::Signal(_) => "signal",
            Verb::Stop => "stop",
        }
    }

    /// Whether the verb still wants its argument.
    fn needs_arg(&self) -> Option<bool> {
        match self {
            Verb::Signal(None) => Some(true),
            Verb::Signal(Some(_)) | Verb::Status | Verb::Stop => Some(false),
        }
    }

    fn give_arg(&mut self, a: OsString) {
        if let Verb::Signal(slot) = self {
            *slot = Some(a);
        }
    }
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
fn ctl(o: &Opts, verb: Verb) -> ExitCode {
    let Some(p) = o.pidfile.as_ref() else {
        return fail(o, &format!("{} needs --pidfile PATH", verb.name()));
    };
    let path = std::path::PathBuf::from(p);

    let outcome = match &verb {
        Verb::Status => d_mon::ctl::status(&path).map(|st| report_state(o, &path, &st, None)),
        Verb::Signal(sig) => {
            let Some(sig) = sig else {
                return fail(o, "signal needs a signal name or number");
            };
            match parse_signal(sig) {
                Some(n) => d_mon::ctl::signal(&path, n)
                    .map(|st| report_state(o, &path, &st, Some(n))),
                None => return fail(o, "unknown signal"),
            }
        }
        Verb::Stop => {
            let want = Duration::from_secs(o.stop_secs.saturating_add(1));
            d_mon::ctl::stop(&path, sys::SIGTERM, want).map(|(st, gone)| {
                let was_running = st.is_running();
                report_state(o, &path, &st, Some(sys::SIGTERM));
                match (was_running, gone) {
                    // Stopped something that was running: done.
                    (true, true) => 0,
                    // Still there: the caller must not be told it worked.
                    (true, false) => {
                        if !o.json {
                            eprintln!("d-mon: still running after the timeout");
                        }
                        2
                    }
                    // Nothing was running: say so, and do not let a mistyped
                    // pidfile pass as a successful stop.
                    _ => 1,
                }
            })
        }
    };

    match outcome {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => report(o, e),
    }
}

/// Print what the pidfile said. Returns the exit code the verb should use.
fn report_state(o: &Opts, path: &std::path::Path, st: &d_mon::ctl::State, sig: Option<i32>) -> i32 {
    use d_mon::ctl::{State, Stopped};

    if o.json {
        let body = match st {
            State::Running {
                supervisor,
                child,
            } => format!(
                "{{\"state\":\"running\",\"pidfile\":\"{}\",\"supervisor\":{supervisor},\"child\":{}}}",
                escape(&path.to_string_lossy()),
                child.map(|c| c.to_string()).unwrap_or_else(|| "null".into())
            ),
            State::Stopped { reason } => format!(
                "{{\"state\":\"stopped\",\"pidfile\":\"{}\",\"reason\":\"{}\"}}",
                escape(&path.to_string_lossy()),
                match reason {
                    Stopped::NoPidfile => "no-pidfile",
                    Stopped::Stale => "stale",
                    Stopped::Vanished => "vanished",
                }
            ),
            State::Uncertain => format!(
                "{{\"state\":\"uncertain\",\"pidfile\":\"{}\"}}",
                escape(&path.to_string_lossy())
            ),
        };
        println!("{body}");
    } else {
        match st {
            State::Running {
                supervisor,
                child,
            } => match child {
                Some(c) => println!(
                    "running: supervisor {supervisor}, delegate {c} ({})",
                    path.display()
                ),
                None => println!(
                    "running: supervisor {supervisor}, no delegate ({})",
                    path.display()
                ),
            },
            State::Stopped { reason } => println!(
                "not running: {} ({})",
                match reason {
                    Stopped::NoPidfile => "no pidfile",
                    Stopped::Stale => "stale pidfile, nothing holds its lock",
                    Stopped::Vanished => "the lock is held but that pid is gone",
                },
                path.display()
            ),
            State::Uncertain => println!(
                "running, but the pid could not be read ({})",
                path.display()
            ),
        }
    }

    match st {
        State::Running { .. } => {
            if let Some(sig) = sig {
                if !o.json {
                    println!("sent {}", signal_name(sig));
                }
            }
            0
        }
        State::Stopped { .. } => 1,
        State::Uncertain => 0,
    }
}

fn signal_name(sig: i32) -> String {
    match sig {
        sys::SIGHUP => "SIGHUP".into(),
        sys::SIGINT => "SIGINT".into(),
        sys::SIGQUIT => "SIGQUIT".into(),
        sys::SIGKILL => "SIGKILL".into(),
        sys::SIGUSR1 => "SIGUSR1".into(),
        sys::SIGUSR2 => "SIGUSR2".into(),
        sys::SIGTERM => "SIGTERM".into(),
        other => format!("signal {other}"),
    }
}
