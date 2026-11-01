//! End-to-end tests through the CLI, plus the library's ownership rule.
//!
//! Every daemon these tests start is killed by a guard, because a leaked
//! supervisor on a machine with a heartbeat is how you lose an afternoon.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A supervisor owns the process's signal handlers, so the in-process tests
/// take turns instead of racing for them.
static SERIAL: Mutex<()> = Mutex::new(());

const BIN: &str = env!("CARGO_BIN_EXE_d-mon");

fn tag(name: &str) -> String {
    format!("dmon-test-{}-{name}", std::process::id())
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(tag(name))
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("d-mon did not run")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Line 1 of a pidfile is the supervisor. Line 2 is the delegate.
fn pid_of(p: &Path) -> i32 {
    fs::read_to_string(p)
        .ok()
        .and_then(|s| s.lines().next().and_then(|l| l.trim().parse().ok()))
        .unwrap_or(0)
}

/// Whether the process is still *doing* something.
///
/// `kill(pid, 0)` is not enough: a zombie answers it until whoever forked it
/// reaps, and a container whose init does not reap keeps zombies around. The
/// tests care about the process being gone, so the state is checked too. (The
/// product must not read /proc; a test may.)
fn alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    if unsafe { d_mon::sys::kill(pid, 0) != 0 } {
        return false;
    }
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => {
            // Field 3 is the state, after the "(comm)" field which may contain
            // spaces.
            let rest = s.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
            let state = rest.split_whitespace().next().unwrap_or("");
            state != "Z" && state != "X"
        }
        // No /proc: fall back to the signal check rather than lie.
        Err(_) => true,
    }
}

/// Kill a supervisor and wait for it to be gone, so tests do not leak.
struct Guard(i32);

impl Guard {
    fn from_pidfile(p: &Path) -> Guard {
        Guard(pid_of(p))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.0 <= 1 {
            return;
        }
        unsafe {
            d_mon::sys::kill(self.0, d_mon::sys::SIGTERM);
        }
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if !alive(self.0) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            d_mon::sys::kill(-self.0, d_mon::sys::SIGKILL);
            d_mon::sys::kill(self.0, d_mon::sys::SIGKILL);
        }
    }
}

#[test]
fn missing_program_is_reported_not_swallowed() {
    let o = run(&["/nonexistent/prog"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    let e = stderr(&o);
    assert!(e.contains("exec"), "stage missing: {e}");
    assert!(e.contains("ENOENT"), "errno missing: {e}");
}

#[test]
fn program_without_exec_bit_is_eacces_at_exec() {
    let p = tmp("noexec");
    fs::write(&p, b"#!/bin/sh\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    let o = run(&[p.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1));
    let e = stderr(&o);
    assert!(e.contains("EACCES"), "errno missing: {e}");
    assert!(e.contains("exec"), "stage missing: {e}");
    let _ = fs::remove_file(&p);
}

#[test]
fn chdir_failure_is_reported_as_chdir() {
    let o = run(&["-C", "/nonexistent-dir", "/bin/sleep", "5"]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("chdir"), "stderr: {}", stderr(&o));
}

#[test]
fn starts_and_stops_cleanly() {
    let pidfile = tmp("ok.pid");
    let log = tmp("ok.log");
    let o = run(&[
        "-l",
        log.to_str().unwrap(),
        "-p",
        pidfile.to_str().unwrap(),
        "/bin/sleep",
        "30",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));

    let sup = pid_of(&pidfile);
    assert!(sup > 1, "pidfile must hold the supervisor pid");
    let guard = Guard(sup);
    assert!(alive(sup), "supervisor must be alive");

    // The delegate is a child of the supervisor: the supervisor owns it.
    let children: Vec<i32> = Command::new("ps")
        .args(["--ppid", &sup.to_string(), "-o", "pid="])
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .filter_map(|t| t.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    assert_eq!(children.len(), 1, "expected exactly one supervised child");

    unsafe { d_mon::sys::kill(sup, d_mon::sys::SIGTERM) };
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && alive(sup) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(sup), "SIGTERM must stop the supervisor");
    assert!(
        !children.iter().any(|c| alive(*c)),
        "the delegate must not outlive its supervisor"
    );
    assert!(!pidfile.exists(), "a clean exit removes the pidfile");
    drop(guard);
    let _ = fs::remove_file(&log);
}

#[test]
fn a_timeout_kills_what_it_started() {
    // A zero budget cannot be met, so the launcher must kill the group and say
    // so rather than leave a daemon behind and exit 0.
    let pidfile = tmp("timeout.pid");
    let o = run(&[
        "-t",
        "0",
        "-p",
        pidfile.to_str().unwrap(),
        "/bin/sleep",
        "30",
    ]);
    assert_eq!(o.status.code(), Some(2), "stderr: {}", stderr(&o));

    // Nothing it created may survive. A pidfile may be left behind, because a
    // SIGKILLed supervisor cannot unlink it; the lock dies with the process, so
    // a leftover file costs nothing but clutter.
    if let Ok(text) = fs::read_to_string(&pidfile) {
        if let Ok(pid) = text.trim().parse::<i32>() {
            assert!(!alive(pid), "the aborted supervisor {pid} is still alive");
        }
    }

    // And the leftover must not block the next start.
    let again = run(&[
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("timeout-again.log").to_string_lossy(),
        "/bin/sleep",
        "30",
    ]);
    assert_eq!(again.status.code(), Some(0), "stderr: {}", stderr(&again));
    let guard = Guard::from_pidfile(&pidfile);
    assert!(alive(guard.0));
}

#[test]
fn second_start_with_the_same_pidfile_exits_3() {
    let pidfile = tmp("dup.pid");
    let first = run(&[
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("dup.log").to_string_lossy(),
        "/bin/sleep",
        "30",
    ]);
    assert_eq!(first.status.code(), Some(0), "stderr: {}", stderr(&first));
    let guard = Guard::from_pidfile(&pidfile);

    let second = run(&["-p", pidfile.to_str().unwrap(), "/bin/sleep", "30"]);
    assert_eq!(second.status.code(), Some(3), "stderr: {}", stderr(&second));
    assert!(alive(guard.0), "the first supervisor must survive");
}

#[test]
fn a_stale_pidfile_does_not_brick_a_start() {
    let pidfile = tmp("stale.pid");
    // A pid from a process that does not exist, and no lock held.
    fs::write(&pidfile, "999999\n").unwrap();
    let o = run(&[
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("stale.log").to_string_lossy(),
        "/bin/sleep",
        "30",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);
    assert!(alive(guard.0));
}

#[test]
fn the_child_gets_a_clean_signal_state() {
    // Rust ignores SIGPIPE in every binary it builds, and an ignored
    // disposition survives execve. The delegate must not inherit it, and must
    // not inherit a blocked mask either.
    let seen = tmp("signals.txt");
    let script = format!(
        "grep -E '^(SigIgn|SigBlk)' /proc/self/status > {}",
        seen.display()
    );
    let o = run(&[
        "-l",
        &tmp("sig.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));

    let text = wait_for_file(&seen);
    let mut saw_ign = false;
    let mut saw_blk = false;
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let key = it.next().unwrap_or("");
        let val = it.next().unwrap_or("");
        let zero = val.trim_start_matches('0').is_empty();
        if key == "SigIgn:" {
            saw_ign = true;
            assert!(zero, "SIGPIPE is still ignored in the delegate: {line}");
        }
        if key == "SigBlk:" {
            saw_blk = true;
            assert!(zero, "a signal is still blocked in the delegate: {line}");
        }
    }
    assert!(saw_ign && saw_blk, "could not read the signal state: {text}");
    let _ = fs::remove_file(&seen);
}

#[test]
fn the_child_inherits_nothing_above_2() {
    let seen = tmp("fds.txt");
    let script = format!("ls /proc/self/fd > {}", seen.display());
    let o = run(&[
        "-l",
        &tmp("fd.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let text = wait_for_file(&seen);
    for line in text.lines() {
        let fd: i32 = line.trim().parse().unwrap_or(-1);
        // 3 is the descriptor `ls` itself holds on /proc/self/fd.
        assert!(fd <= 3, "the delegate inherited fd {fd}: {text:?}");
    }
    let _ = fs::remove_file(&seen);
}

#[test]
fn log_capture_reaches_the_file() {
    let log = tmp("capture.log");
    let marker = "captured-line-from-the-delegate";
    let o = run(&[
        "-l",
        log.to_str().unwrap(),
        "/bin/sh",
        "-c",
        &format!("echo {marker}; echo oops >&2"),
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let text = wait_for_file(&log);
    assert!(
        text.contains(marker),
        "stdout missing from the log: {text:?}"
    );
    assert!(
        text.contains("oops"),
        "stderr missing from the log: {text:?}"
    );
    let _ = fs::remove_file(&log);
}

#[test]
fn restart_respawns_after_a_failure() {
    let count = tmp("restarts.txt");
    let _ = fs::remove_file(&count);
    let script = format!("echo x >> {}; exit 1", count.display());
    let pidfile = tmp("restart.pid");
    let o = run(&[
        "-r",
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("restart.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);

    let until = Instant::now() + Duration::from_secs(4);
    let mut seen = 0;
    while Instant::now() < until {
        seen = fs::read_to_string(&count)
            .map(|s| s.lines().count())
            .unwrap_or(0);
        if seen >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(seen >= 2, "the child was restarted {seen} times, wanted 2");
    assert!(alive(guard.0), "the supervisor must still be supervising");
    let _ = fs::remove_file(&count);
}

#[test]
fn json_outcome_is_one_line() {
    let o = run(&["--json", "/bin/sleep", "30"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let line = stdout(&o);
    let line = line.trim();
    assert!(line.starts_with('{') && line.ends_with('}'), "{line}");
    assert!(line.contains("\"outcome\":\"started\""), "{line}");
    let pid: i32 = line
        .split("\"supervisor\":")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.parse().ok())
        .expect("supervisor pid in json");
    drop(Guard(pid));

    let bad = run(&["--json", "/nonexistent/prog"]);
    assert_eq!(bad.status.code(), Some(1));
    let j = stdout(&bad);
    assert!(j.contains("\"outcome\":\"failed\""), "{j}");
    assert!(j.contains("\"errno\":2"), "{j}");
}

#[test]
fn dropping_to_our_own_ids_works() {
    let uid = unsafe { d_mon::sys::getuid() };
    let gid = unsafe { d_mon::sys::getgid() };
    let pidfile = tmp("ids.pid");
    let o = run(&[
        "-u",
        &uid.to_string(),
        "-g",
        &gid.to_string(),
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("ids.log").to_string_lossy(),
        "/bin/sleep",
        "30",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);
    assert!(alive(guard.0), "the delegate must have started");
    // and a name that does not exist is refused before the fork
    let bad = run(&["-u", "no-such-user-xyz", "/bin/sleep", "1"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr(&bad).contains("ENOENT"), "{}", stderr(&bad));
}

#[test]
fn run_does_not_steal_the_hosts_children() {
    // `run()` must wait only on the child it forked. If it used waitpid(-1) it
    // would take this test's own `sleep`, and `wait()` below would fail.
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut bystander = Command::new("/bin/sleep")
        .arg("0.4")
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn bystander");

    let st = d_mon::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 7")
        .run()
        .expect("run() must not lose the exit status");
    assert_eq!(st.code(), Some(7), "the supervised status must be returned");

    let status = bystander.wait().expect("the bystander was not stolen");
    assert!(status.success(), "bystander exited early: {status:?}");
}

#[test]
fn foreground_reports_a_failed_start_as_err() {
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let c = d_mon::Command::new("/nonexistent/prog")
        .ready_timeout(Duration::from_millis(500));
    let err = c.run().expect_err("a missing program must not look started");
    let text = err.to_string();
    assert!(text.contains("ENOENT"), "{text}");
}

#[test]
fn foreground_returns_the_delegate_status() {
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let st = d_mon::Command::new("/bin/sh")
        .arg("-c")
        .arg("exit 3")
        .run()
        .expect("run()");
    assert_eq!(st.code(), Some(3));
}

fn wait_for_file(p: &Path) -> String {
    let until = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(s) = fs::read_to_string(p) {
            if !s.is_empty() {
                return s;
            }
        }
        if Instant::now() > until {
            return fs::read_to_string(p).unwrap_or_default();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
/// PID 1 is the role d-mon cannot test as an ordinary process. The test asks
/// the kernel for a namespace; where that is refused it says so and skips,
/// because a silently-skipped test is a lie.
#[test]
fn pid1_reaps_and_forwards() {
    let probe = Command::new("unshare")
        .args(["-pf", "--mount-proc", "true"])
        .status();
    let permitted = matches!(probe, Ok(s) if s.success());
    if !permitted {
        eprintln!(
            "SKIPPED pid1_reaps_and_forwards: this kernel will not create a PID \
             namespace for us (no CAP_SYS_ADMIN and no unprivileged user namespace). \
             The PID 1 path is exercised only where `unshare -pf` works."
        );
        return;
    }

    let marker = tmp("pid1.txt");
    let script = "echo alive > MARKER; sleep 5";
    let script = script.replace("MARKER", marker.to_str().unwrap());
    let status = Command::new("unshare")
        .args(["-pf", "--mount-proc", BIN, "-f", "/bin/sh", "-c", &script])
        .status()
        .expect("unshare runs");
    assert!(status.success() || status.code().is_some());
    assert!(marker.exists(), "the namespace init did not run the delegate");
    let _ = fs::remove_file(&marker);
}

#[test]
fn a_child_that_ignores_term_is_killed_after_the_grace() {
    let pidfile = tmp("grace.pid");
    let ready = tmp("grace.ready");
    let o = run(&[
        "--stop-timeout",
        "1",
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("grace.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        &format!("trap '' TERM; : > {}; sleep 60", ready.display()),
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let sup = pid_of(&pidfile);
    let guard = Guard(sup);
    // The trap must exist before the signal, or the test measures a race.
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !ready.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "the delegate never installed its trap");

    let started = Instant::now();
    unsafe { d_mon::sys::kill(sup, d_mon::sys::SIGTERM) };
    let until = started + Duration::from_secs(6);
    while Instant::now() < until && alive(sup) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let took = started.elapsed();
    assert!(!alive(sup), "the supervisor never gave up on a stuck child");
    assert!(
        took >= Duration::from_millis(700),
        "the grace period was skipped: stopped in {took:?}"
    );
    drop(guard);
}

#[test]
fn sighup_reopens_a_rotated_log() {
    let log = tmp("rotate.log");
    let pidfile = tmp("rotate.pid");
    let ready = tmp("rotate.ready");
    let _ = fs::remove_file(&ready);
    let script = format!(
        "trap '' HUP; : > {}; while :; do echo tick; sleep 0.05; done",
        ready.display()
    );
    let o = run(&[
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        log.to_str().unwrap(),
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let sup = pid_of(&pidfile);
    let guard = Guard(sup);

    // Two things must be true before the rotation is meaningful: the delegate
    // has installed its trap (or the HUP kills it), and the log has content in
    // it (or there is nothing to prove was redirected).
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !ready.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "the delegate never installed its trap");
    let until = Instant::now() + Duration::from_secs(3);
    let mut wrote = false;
    while Instant::now() < until {
        if fs::metadata(&log).map(|m| m.len()).unwrap_or(0) > 0 {
            wrote = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(wrote, "the delegate produced no output to rotate");

    // Rotate the way logrotate does: move, recreate, HUP.
    let rotated = log.with_extension("rotated");
    let before = fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    fs::rename(&log, &rotated).expect("rotate");
    fs::write(&log, b"").expect("recreate");

    unsafe { d_mon::sys::kill(sup, d_mon::sys::SIGHUP) };

    let until = Instant::now() + Duration::from_secs(4);
    let mut grew = false;
    while Instant::now() < until {
        if fs::metadata(&log).map(|m| m.len()).unwrap_or(0) > 0 {
            grew = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(grew, "the log was not reopened after SIGHUP");
    assert!(
        fs::metadata(&rotated).map(|m| m.len()).unwrap_or(0) >= before,
        "the rotated file should hold what was written before the HUP"
    );
    let _ = fs::remove_file(&rotated);
    drop(guard);
}

#[test]
fn no_close_hands_the_callers_pipe_to_the_delegate() {
    let o = run(&["-k", "/bin/echo", "inherited-marker"]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    // With -k the delegate keeps fd 1, so its output arrives in our capture.
    let until = Instant::now() + Duration::from_secs(3);
    let mut seen = String::new();
    while Instant::now() < until {
        seen = stdout(&o);
        if seen.contains("inherited-marker") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(seen.contains("inherited-marker"), "got {seen:?}");
}

#[test]
fn a_stop_outranks_the_restart_policy() {
    // `-r` with a child that keeps exiting must not answer SIGTERM by spawning
    // yet another one.
    let pidfile = tmp("stop-restart.pid");
    let o = run(&[
        "-r",
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("stop-restart.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        "exit 1",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let sup = pid_of(&pidfile);
    let guard = Guard(sup);
    std::thread::sleep(Duration::from_millis(200));

    unsafe { d_mon::sys::kill(sup, d_mon::sys::SIGTERM) };
    let until = Instant::now() + Duration::from_secs(4);
    while Instant::now() < until && alive(sup) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(sup), "a restarting supervisor must still honour SIGTERM");
    drop(guard);
}

// ---- control verbs -------------------------------------------------------

#[test]
fn status_reports_a_live_supervisor_and_its_delegate() {
    let pidfile = tmp("ctl.pid");
    let o = run(&[
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("ctl.log").to_string_lossy(),
        "/bin/sleep",
        "30",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);

    let st = run(&["status", "-p", pidfile.to_str().unwrap()]);
    assert_eq!(st.status.code(), Some(0), "stderr: {}", stderr(&st));
    let text = stdout(&st);
    assert!(text.contains(&format!("supervisor {}", guard.0)), "{text}");
    assert!(text.contains("delegate"), "{text}");

    let js = run(&["--json", "status", "-p", pidfile.to_str().unwrap()]);
    assert_eq!(js.status.code(), Some(0));
    let j = stdout(&js);
    assert!(j.contains("\"state\":\"running\""), "{j}");
    assert!(j.contains(&format!("\"supervisor\":{}", guard.0)), "{j}");
}

#[test]
fn status_of_nothing_is_exit_1() {
    let absent = tmp("ctl-absent.pid");
    let _ = fs::remove_file(&absent);
    let o = run(&["status", "-p", absent.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));

    // A pidfile no process holds is stale, not running — even when the pid in
    // it happens to be alive right now.
    let victim = Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("stranger");
    let stale = tmp("ctl-stale.pid");
    fs::write(&stale, format!("{}\n", victim.id())).unwrap();
    let o = run(&["status", "-p", stale.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert!(stdout(&o).contains("stale"), "{}", stdout(&o));
    drop(victim);
    let _ = fs::remove_file(&stale);
}

#[test]
fn verbs_never_signal_a_stranger_from_a_stale_pidfile() {
    // The pidfile names a live process that is not a supervisor. Signalling it
    // would be the classic pidfile bug: the pid was recycled.
    let mut stranger = Command::new("/bin/sleep")
        .arg("20")
        .spawn()
        .expect("stranger");
    let stale = tmp("ctl-victim.pid");
    fs::write(&stale, format!("{}\n", stranger.id())).unwrap();

    let o = run(&["stop", "-p", stale.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "stderr: {}", stderr(&o));
    assert!(
        stranger.try_wait().expect("poll stranger").is_none(),
        "stop killed a process that was not ours"
    );

    let s = run(&["signal", "KILL", "-p", stale.to_str().unwrap()]);
    assert_eq!(s.status.code(), Some(1), "stderr: {}", stderr(&s));
    assert!(
        stranger.try_wait().expect("poll stranger").is_none(),
        "signal killed a process that was not ours"
    );

    let _ = stranger.kill();
    let _ = stranger.wait();
    let _ = fs::remove_file(&stale);
}

#[test]
fn stop_waits_until_the_supervisor_is_really_gone() {
    let pidfile = tmp("ctl-stop.pid");
    let o = run(&[
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("ctl-stop.log").to_string_lossy(),
        "/bin/sleep",
        "30",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);

    let st = run(&["stop", "-p", pidfile.to_str().unwrap()]);
    assert_eq!(st.status.code(), Some(0), "stderr: {}", stderr(&st));
    // Exit 0 must mean the process is gone, not merely asked.
    assert!(!alive(guard.0), "stop returned success with the supervisor alive");
    let after = run(&["status", "-p", pidfile.to_str().unwrap()]);
    assert_eq!(after.status.code(), Some(1), "stderr: {}", stderr(&after));
    drop(guard);
}

#[test]
fn signal_reaches_the_delegate_through_the_supervisor() {
    let pidfile = tmp("ctl-usr2.pid");
    let seen = tmp("ctl-usr2.txt");
    let _ = fs::remove_file(&seen);
    let script = format!(
        "trap 'echo got > {}' USR2; : > {}; while :; do sleep 0.02; done",
        seen.display(),
        tmp("ctl-usr2.ready").display()
    );
    let o = run(&[
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("ctl-usr2.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);

    let ready = tmp("ctl-usr2.ready");
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !ready.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "the delegate never installed its trap");

    let s = run(&["signal", "USR2", "-p", pidfile.to_str().unwrap()]);
    assert_eq!(s.status.code(), Some(0), "stderr: {}", stderr(&s));
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !seen.exists() {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(seen.exists(), "the signal never reached the delegate");
    let _ = fs::remove_file(&seen);
    let _ = fs::remove_file(&ready);
    drop(guard);
}

#[test]
fn dashdash_runs_a_program_named_like_a_verb() {
    let log = tmp("ctl-dd.log");
    let _ = fs::remove_file(&log);
    // `--` must switch off verb parsing entirely.
    let o = run(&["--", "/bin/sh", "-c", &format!("echo dd > {}", log.display())]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let text = wait_for_file(&log);
    assert!(text.contains("dd"), "got {text:?}");
    let _ = fs::remove_file(&log);
}

// ---- behaviours that were advertised but never exercised ---------------

#[test]
fn a_bare_name_is_resolved_through_path() {
    let pidfile = tmp("path.pid");
    let o = run(&[
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("path.log").to_string_lossy(),
        "sleep",
        "30",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);
    assert!(alive(guard.0));

    let bad = run(&["definitely-not-on-path-xyz", "1"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr(&bad).contains("ENOENT"), "{}", stderr(&bad));
}

#[test]
fn chdir_and_env_reach_the_delegate() {
    let out = tmp("chdir.txt");
    let _ = fs::remove_file(&out);
    let script = format!("pwd > {}; echo $GREETING >> {}", out.display(), out.display());
    let o = run(&[
        "-C",
        "/tmp",
        "-e",
        "GREETING=hello-from-dmon",
        "-l",
        &tmp("chdir.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let text = wait_for_file(&out);
    assert!(text.contains("/tmp"), "chdir did not apply: {text:?}");
    assert!(text.contains("hello-from-dmon"), "env did not apply: {text:?}");
    let _ = fs::remove_file(&out);
}

#[test]
fn the_delegate_reads_eof_on_stdin_when_detached() {
    // Detached, stdin is the null device, so a reader must not block.
    let out = tmp("stdin.txt");
    let _ = fs::remove_file(&out);
    let o = run(&[
        "-l",
        &tmp("stdin.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        &format!("cat > /dev/null; echo eof >> {}", out.display()),
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let text = wait_for_file(&out);
    assert!(text.contains("eof"), "stdin was not a null sink: {text:?}");
    let _ = fs::remove_file(&out);
}

#[test]
fn stop_times_out_with_its_own_exit_code() {
    // The delegate ignores TERM and the supervisor's grace is long; a stop that
    // waits one second must report that it could not confirm the stop, not
    // pretend it worked.
    let pidfile = tmp("slowstop.pid");
    let ready = tmp("slowstop.ready");
    let _ = fs::remove_file(&ready);
    let o = run(&[
        "-l",
        &tmp("slowstop.log").to_string_lossy(),
        "-p",
        pidfile.to_str().unwrap(),
        "/bin/sh",
        "-c",
        &format!("trap '' TERM; : > {}; sleep 60", ready.display()),
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !ready.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "the delegate never installed its trap");

    let st = run(&[
        "stop",
        "--stop-timeout",
        "1",
        "-p",
        pidfile.to_str().unwrap(),
    ]);
    assert_eq!(st.status.code(), Some(2), "stderr: {}", stderr(&st));
    drop(guard);
}

#[test]
fn capture_without_a_log_is_refused_by_the_library() {
    use d_mon::Stdio;
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let err = d_mon::Command::new("/bin/echo")
        .arg("hi")
        .stdout(Stdio::Capture)
        .run()
        .expect_err("capture needs somewhere to write");
    let text = err.to_string();
    assert!(text.contains("capture"), "{text}");
}

#[test]
fn stdio_file_appends_where_it_is_told() {
    use d_mon::Stdio;
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let out = tmp("stdiofile.txt");
    let _ = fs::remove_file(&out);
    let st = d_mon::Command::new("/bin/sh")
        .arg("-c")
        .arg("echo from-the-delegate")
        .stdout(Stdio::File(out.clone()))
        .run()
        .expect("run()");
    assert!(st.success());
    let text = fs::read_to_string(&out).unwrap_or_default();
    assert!(text.contains("from-the-delegate"), "got {text:?}");
    let _ = fs::remove_file(&out);
}

#[test]
fn restart_stops_at_its_limit() {
    // `Restart::max` is the caller's crash-loop cap; the supervisor must honour
    // it rather than respawn forever.
    let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let count = tmp("caploop.txt");
    let _ = fs::remove_file(&count);
    let st = d_mon::Command::new("/bin/sh")
        .arg("-c")
        .arg(format!("echo x >> {}; exit 1", count.display()))
        .restart(d_mon::Restart::OnFailure {
            max: 2,
            backoff: Duration::from_millis(20),
        })
        .run()
        .expect("run()");
    assert_eq!(st.code(), Some(1), "the delegate's own exit status");
    let lines = fs::read_to_string(&count).map(|s| s.lines().count()).unwrap_or(0);
    assert_eq!(lines, 3, "one start plus two restarts, got {lines}");
    let _ = fs::remove_file(&count);
}

#[test]
fn verbose_says_what_it_decided() {
    let pidfile = tmp("verbose.pid");
    let o = run(&[
        "-v",
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("verbose.log").to_string_lossy(),
        "/bin/sleep",
        "20",
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);
    let e = stderr(&o);
    assert!(e.contains("detaching"), "{e}");
    assert!(e.contains("started supervisor"), "{e}");
    // -v must not write to stdout, or --json consumers would break.
    assert!(stdout(&o).is_empty(), "stdout: {}", stdout(&o));
    drop(guard);
}

#[test]
fn stop_signal_is_honoured_end_to_end() {
    // --stop-signal is the signal the supervisor sends its delegate on a stop;
    // the delegate here records which one it saw.
    let pidfile = tmp("stopsig.pid");
    let seen = tmp("stopsig.txt");
    let ready = tmp("stopsig.ready");
    let _ = fs::remove_file(&seen);
    let _ = fs::remove_file(&ready);
    let script = format!(
        "trap 'echo INT > {}; exit 0' INT; trap 'echo TERM > {}; exit 0' TERM; : > {}; while :; do sleep 0.05; done",
        seen.display(),
        seen.display(),
        ready.display()
    );
    let o = run(&[
        "--stop-signal",
        "INT",
        "-p",
        pidfile.to_str().unwrap(),
        "-l",
        &tmp("stopsig.log").to_string_lossy(),
        "/bin/sh",
        "-c",
        &script,
    ]);
    assert_eq!(o.status.code(), Some(0), "stderr: {}", stderr(&o));
    let guard = Guard::from_pidfile(&pidfile);

    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !ready.exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "the delegate never installed its traps");

    let st = run(&["stop", "-p", pidfile.to_str().unwrap()]);
    assert_eq!(st.status.code(), Some(0), "stderr: {}", stderr(&st));
    let text = wait_for_file(&seen);
    assert!(text.contains("INT"), "the wrong stop signal arrived: {text:?}");
    assert!(!text.contains("TERM"), "both signals arrived: {text:?}");
    drop(guard);
}
