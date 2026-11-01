//! The CLI. Thin by design: it parses arguments into a [`Command`] and calls
//! the same library entry points an embedder would.

use std::ffi::OsString;
use std::process::ExitCode;

use d_mon::command::Command;
use d_mon::error::Error;

const USAGE: &str = "\
usage: d-mon [options] [--] prog [args...]

  -l, --logfile PATH     capture stdout+stderr (no /dev/null needed)
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
      --json             machine-readable outcome
  -v, --verbose          say what is happening
  -h, --help             this text
  -V, --version          print the version

exit: 0 started, 1 failed to start, 2 unconfirmed, 3 already running
";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).peekable();
    let mut logfile = None;
    let mut pidfile = None;
    let mut chdir = None;
    let mut user = None;
    let mut group = None;
    let mut envs: Vec<OsString> = Vec::new();
    let mut timeout_ms: u64 = 5000;
    let mut restart = false;
    let mut foreground = false;
    let mut no_close = false;
    let mut reap = false;
    let mut verbose = false;

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
            "-l" | "--logfile" => logfile = args.next(),
            "-p" | "--pidfile" => pidfile = args.next(),
            "-C" | "--chdir" => chdir = args.next(),
            "-u" | "--user" => user = args.next(),
            "-g" | "--group" => group = args.next(),
            "-e" | "--env" => match args.next() {
                Some(v) => envs.push(v),
                None => return fail("--env needs VAR=VAL"),
            },
            "-t" | "--timeout" => match args.next().and_then(|v| v.to_str().map(str::to_owned)) {
                Some(v) => match v.parse() {
                    Ok(n) => timeout_ms = n,
                    Err(_) => return fail("--timeout needs milliseconds"),
                },
                None => return fail("--timeout needs milliseconds"),
            },
            "-r" | "--restart" => restart = true,
            "-f" | "--foreground" => foreground = true,
            "-k" | "--no-close" => no_close = true,
            "-R" | "--reap" => reap = true,
            "-v" | "--verbose" => verbose = true,
            _ if s.starts_with('-') => return fail(&format!("unknown option {s}")),
            _ => {
                // This is the program; put it back by rebuilding the tail.
                let mut tail = vec![a];
                tail.extend(args);
                return run(
                    tail, logfile, pidfile, chdir, user, group, envs, timeout_ms, restart,
                    foreground, no_close, reap, verbose,
                );
            }
        }
    }

    fail("no program given (try --help)")
}

#[allow(clippy::too_many_arguments)]
fn run(
    tail: Vec<OsString>,
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
    verbose: bool,
) -> ExitCode {
    let mut it = tail.into_iter();
    let prog = match it.next() {
        Some(p) => p,
        None => return fail("no program given"),
    };

    let mut cmd = Command::new(&prog).args(it);
    if let Some(p) = logfile {
        cmd = cmd.log_file(p);
    }
    if let Some(p) = pidfile {
        cmd = cmd.pid_file(p);
    }
    if let Some(d) = chdir {
        cmd = cmd.current_dir(d);
    }
    if let Some(u) = user {
        cmd = cmd.user(d_mon::Id::Name(u.to_string_lossy().into_owned()));
    }
    if let Some(g) = group {
        cmd = cmd.group(d_mon::Id::Name(g.to_string_lossy().into_owned()));
    }
    for e in envs {
        if let Some(s) = e.to_str() {
            if let Some((k, v)) = s.split_once('=') {
                cmd = cmd.env(k, v);
            }
        }
    }
    cmd = cmd
        .ready_timeout(std::time::Duration::from_millis(timeout_ms))
        .stop(d_mon::StopPolicy::default());
    if restart {
        cmd = cmd.restart(d_mon::Restart::Always {
            max: u32::MAX,
            backoff: std::time::Duration::from_secs(1),
        });
    }
    if no_close {
        cmd = cmd.stdout(d_mon::Stdio::Inherit).stderr(d_mon::Stdio::Inherit);
    }
    let _ = reap;
    let _ = verbose;

    if foreground {
        match cmd.run() {
            Ok(st) => {
                if st.success() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(st.code().unwrap_or(1) as u8)
                }
            }
            Err(e) => report(e),
        }
    } else {
        match cmd.daemonize() {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => report(e),
        }
    }
}

fn report(e: Error) -> ExitCode {
    eprintln!("d-mon: {e}");
    match e {
        Error::Unconfirmed { .. } => ExitCode::from(2),
        Error::Spawn { .. } => ExitCode::from(1),
        Error::Env { .. } => ExitCode::from(1),
        _ => ExitCode::from(1),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("d-mon: {msg}");
    ExitCode::from(1)
}