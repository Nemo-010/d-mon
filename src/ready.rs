//! The readiness protocol.
//!
//! One pipe, fixed 16-byte records, `O_CLOEXEC` write end. The supervisor
//! writes [`Rec::Super`], forks the child, and **closes its own copy before
//! anything else** — that close discipline is what makes EOF mean "the child
//! exec'd". The child writes [`Rec::Hello`] immediately before `execve`, or
//! [`Rec::Fail`] instead of it.
//!
//! Records are single `write(2)` calls of at most `PIPE_BUF` bytes, so they are
//! atomic and a reader never sees half of one.

use crate::error::Stage;
use crate::sys;

/// Wire magic, so a stray byte cannot be mistaken for a record.
pub const MAGIC: u32 = 0x644d_6f6e; // "dMon"

/// A record on the readiness pipe. Never larger than `PIPE_BUF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rec {
    /// The supervisor is alive and about to fork the child. `pgid` is the
    /// group the launcher kills if the start is not confirmed.
    Super { pid: i32, pgid: i32 },
    /// The child reached its last step; `execve` follows immediately.
    Hello { pid: i32 },
    /// The child failed before `execve`.
    Fail { stage: Stage, errno: i32 },
    /// A live supervisor already holds the pidfile; `pid` is its pid when it
    /// could be read.
    Busy { pid: i32 },
}

impl Rec {
    /// Size of every record on the wire: magic + tag + stage + 2 pad + 2 words.
    pub const LEN: usize = 16;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&MAGIC.to_ne_bytes());
        match *self {
            Rec::Super { pid, pgid } => {
                b[4] = 1;
                b[8..12].copy_from_slice(&pid.to_ne_bytes());
                b[12..16].copy_from_slice(&pgid.to_ne_bytes());
            }
            Rec::Hello { pid } => {
                b[4] = 2;
                b[8..12].copy_from_slice(&pid.to_ne_bytes());
            }
            Rec::Fail { stage, errno } => {
                b[4] = 3;
                b[5] = stage_byte(stage);
                b[8..12].copy_from_slice(&errno.to_ne_bytes());
            }
            Rec::Busy { pid } => {
                b[4] = 4;
                b[8..12].copy_from_slice(&pid.to_ne_bytes());
            }
        }
        b
    }

    pub fn decode(b: &[u8; Self::LEN]) -> Option<Rec> {
        let magic = u32::from_ne_bytes([b[0], b[1], b[2], b[3]]);
        if magic != MAGIC {
            return None;
        }
        let word = i32::from_ne_bytes([b[8], b[9], b[10], b[11]]);
        match b[4] {
            1 => Some(Rec::Super {
                pid: word,
                pgid: i32::from_ne_bytes([b[12], b[13], b[14], b[15]]),
            }),
            2 => Some(Rec::Hello { pid: word }),
            3 => Some(Rec::Fail {
                stage: stage_from_byte(b[5]),
                errno: word,
            }),
            4 => Some(Rec::Busy { pid: word }),
            _ => None,
        }
    }
}

fn stage_byte(s: Stage) -> u8 {
    match s {
        Stage::PreFork => 0,
        Stage::Open => 1,
        Stage::Chdir => 2,
        Stage::Privileges => 3,
        Stage::Exec => 4,
    }
}

fn stage_from_byte(b: u8) -> Stage {
    match b {
        0 => Stage::PreFork,
        1 => Stage::Open,
        2 => Stage::Chdir,
        3 => Stage::Privileges,
        _ => Stage::Exec,
    }
}

/// Write one record. Retries on `EINTR`; a short write is an error.
pub fn send(fd: i32, rec: Rec) -> Result<(), i32> {
    sys::write_all(fd, &rec.encode())
}

/// The verdict reached by the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Started {
        supervisor: i32,
        pgid: i32,
        child: i32,
    },
    Failed {
        stage: Stage,
        errno: i32,
    },
    Unconfirmed,
    Busy { pid: Option<i32> },
}

/// What the launcher has seen so far.
///
/// Kept separate from the I/O so the state machine is testable without forks.
#[derive(Debug, Default, Clone)]
pub struct Tracker {
    pub supervisor: Option<i32>,
    pub pgid: Option<i32>,
    pub hello: Option<i32>,
    pub failure: Option<(Stage, i32)>,
    pub busy: Option<i32>,
    pub garbage: usize,
}

impl Tracker {
    pub fn feed(&mut self, rec: Rec) {
        match rec {
            Rec::Super { pid, pgid } => {
                self.supervisor = Some(pid);
                self.pgid = Some(pgid);
            }
            Rec::Hello { pid } => self.hello = Some(pid),
            Rec::Fail { stage, errno } => self.failure = Some((stage, errno)),
            Rec::Busy { pid } => self.busy = Some(pid),
        }
    }

    /// Called when the pipe reports EOF: the last holder of the write end is
    /// gone, and `execve` is the only way that happens after `REC_HELLO`.
    pub fn on_eof(&self) -> Verdict {
        if let Some(pid) = self.busy {
            return Verdict::Busy {
                pid: if pid > 0 { Some(pid) } else { None },
            };
        }
        if let Some((stage, errno)) = self.failure {
            return Verdict::Failed { stage, errno };
        }
        match (self.hello, self.supervisor) {
            (Some(child), Some(supervisor)) => Verdict::Started {
                supervisor,
                pgid: self.pgid.unwrap_or(supervisor),
                child,
            },
            // EOF with no Hello: the process died during setup, before it
            // could reach execve. This is the case that catches the std
            // sanitize_standard_fds abort, an OOM kill, or a SIGKILL.
            _ => Verdict::Failed {
                stage: Stage::PreFork,
                errno: sys::ENOENT,
            },
        }
    }
}

/// Read records into `t` until EOF or the deadline.
///
/// `Err(EAGAIN)` means the budget ran out: the caller kills the group and
/// reports `Unconfirmed` — there is no "give up quietly" path.
pub fn await_records(
    fd: i32,
    budget: std::time::Duration,
    t: &mut Tracker,
) -> Result<(), i32> {
    use std::time::Instant;

    let deadline = Instant::now() + budget;
    let mut buf = [0u8; Rec::LEN];
    let mut have = 0usize;

    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let mut pfd = sys::PollFd {
            fd,
            events: sys::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd for one fd.
        let rc = unsafe { sys::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) };
        if rc < 0 {
            let e = sys::errno();
            if e == sys::EINTR {
                continue;
            }
            return Err(e);
        }
        if rc == 0 {
            return Err(sys::EAGAIN);
        }
        match sys::read_some(fd, &mut buf[have..]) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                have += n;
                if have == Rec::LEN {
                    match Rec::decode(&buf) {
                        Some(r) => t.feed(r),
                        None => t.garbage += 1,
                    }
                    have = 0;
                }
            }
            Err(e) if e == sys::EAGAIN => continue,
            Err(e) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tracker(recs: &[Rec]) -> Tracker {
        let mut t = Tracker::default();
        for r in recs {
            t.feed(*r);
        }
        t
    }

    /// The six cases from PLAN.md §8, one per state.
    #[test]
    fn exec_ok_is_started() {
        let t = tracker(&[Rec::Super { pid: 10, pgid: 9 }, Rec::Hello { pid: 11 }]);
        assert_eq!(
            t.on_eof(),
            Verdict::Started {
                supervisor: 10,
                pgid: 9,
                child: 11
            }
        );
    }

    #[test]
    fn exec_failure_is_failed_at_its_stage() {
        let t = tracker(&[
            Rec::Super { pid: 10, pgid: 9 },
            Rec::Hello { pid: 11 },
            Rec::Fail {
                stage: Stage::Exec,
                errno: sys::ENOENT,
            },
        ]);
        assert_eq!(
            t.on_eof(),
            Verdict::Failed {
                stage: Stage::Exec,
                errno: sys::ENOENT
            }
        );
    }

    #[test]
    fn a_failure_before_hello_wins() {
        let t = tracker(&[
            Rec::Super { pid: 10, pgid: 9 },
            Rec::Fail {
                stage: Stage::Privileges,
                errno: sys::EPERM,
            },
        ]);
        assert_eq!(
            t.on_eof(),
            Verdict::Failed {
                stage: Stage::Privileges,
                errno: sys::EPERM
            }
        );
    }

    #[test]
    fn death_before_exec_is_not_a_start() {
        // Killed between fork and execve: HELLO never arrived.
        let t = tracker(&[Rec::Super { pid: 10, pgid: 9 }]);
        assert!(matches!(t.on_eof(), Verdict::Failed { stage: Stage::PreFork, .. }));
    }

    #[test]
    fn eof_with_no_record_at_all_is_not_a_start() {
        let t = Tracker::default();
        assert!(matches!(t.on_eof(), Verdict::Failed { stage: Stage::PreFork, .. }));
    }

    #[test]
    fn a_busy_pidfile_is_its_own_verdict() {
        let t = tracker(&[Rec::Busy { pid: 42 }]);
        assert_eq!(t.on_eof(), Verdict::Busy { pid: Some(42) });
    }

    #[test]
    fn records_round_trip() {
        for r in [
            Rec::Super { pid: 7, pgid: 3 },
            Rec::Hello { pid: 1_000_000 },
            Rec::Fail {
                stage: Stage::Chdir,
                errno: sys::EACCES,
            },
            Rec::Busy { pid: 0 },
        ] {
            let b = r.encode();
            assert_eq!(b.len(), Rec::LEN);
            assert_eq!(Rec::decode(&b), Some(r));
        }
    }

    #[test]
    fn a_stray_byte_is_not_a_record() {
        let mut b = Rec::Hello { pid: 5 }.encode();
        b[0] ^= 0xff;
        assert_eq!(Rec::decode(&b), None);
    }

    #[test]
    fn a_real_pipe_carries_the_verdict() {
        let mut fds = [0 as i32; 2];
        assert_eq!(unsafe { sys::pipe2(fds.as_mut_ptr(), sys::O_CLOEXEC) }, 0);
        let (r, w) = (fds[0], fds[1]);

        let writer = std::thread::spawn(move || {
            send(w, Rec::Super { pid: 4, pgid: 4 }).unwrap();
            send(w, Rec::Hello { pid: 5 }).unwrap();
            unsafe {
                sys::close(w);
            }
        });

        let mut t = Tracker::default();
        await_records(r, Duration::from_secs(2), &mut t).expect("EOF arrives");
        writer.join().unwrap();
        assert_eq!(
            t.on_eof(),
            Verdict::Started {
                supervisor: 4,
                pgid: 4,
                child: 5
            }
        );
        unsafe {
            sys::close(r);
        }
    }

    #[test]
    fn a_silent_pipe_times_out_instead_of_starting() {
        let mut fds = [0 as i32; 2];
        assert_eq!(unsafe { sys::pipe2(fds.as_mut_ptr(), sys::O_CLOEXEC) }, 0);
        let (r, w) = (fds[0], fds[1]);
        let mut t = Tracker::default();
        let e = await_records(r, Duration::from_millis(80), &mut t).unwrap_err();
        assert_eq!(e, sys::EAGAIN);
        unsafe {
            sys::close(r);
            sys::close(w);
        }
    }
}
