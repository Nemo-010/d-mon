//! The readiness protocol.
//!
//! One pipe, fixed-size records, `O_CLOEXEC` write end. The supervisor writes
//! [`Rec::Super`], forks the child, and **closes its own copy before anything
//! else** — that close discipline is what makes EOF mean "the child exec'd".
//! The child writes [`Rec::Hello`] immediately before `execve`, or
//! [`Rec::Fail`] instead of it.

use crate::error::Stage;
use crate::sys;

/// Wire magic, so a stray byte cannot be mistaken for a record.
pub const MAGIC: u32 = 0x644d_6f6e; // "dMon"

/// A record on the readiness pipe. Never larger than `PIPE_BUF`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rec {
    /// The supervisor is alive and about to fork the child.
    Super { pid: i32 },
    /// The child reached its last step; `execve` follows immediately.
    Hello { pid: i32 },
    /// The child failed before `execve`.
    Fail { stage: Stage, errno: i32 },
}

impl Rec {
    /// Size of every record on the wire: magic + tag + two words.
    pub const LEN: usize = 16;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&MAGIC.to_ne_bytes());
        match *self {
            Rec::Super { pid } => {
                b[4] = 1;
                b[8..12].copy_from_slice(&pid.to_ne_bytes());
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
            1 => Some(Rec::Super { pid: word }),
            2 => Some(Rec::Hello { pid: word }),
            3 => Some(Rec::Fail {
                stage: stage_from_byte(b[5]),
                errno: word,
            }),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Started { supervisor: Option<i32>, child: i32 },
    Failed { stage: Stage, errno: i32 },
    Unconfirmed,
}

/// What the launcher has seen so far.
///
/// Kept separate from the I/O so the state machine is testable without forks.
#[derive(Debug, Default)]
pub struct Tracker {
    pub supervisor: Option<i32>,
    pub hello: Option<i32>,
    pub failure: Option<(Stage, i32)>,
}

impl Tracker {
    pub fn feed(&mut self, rec: Rec) {
        match rec {
            Rec::Super { pid } => self.supervisor = Some(pid),
            Rec::Hello { pid } => self.hello = Some(pid),
            Rec::Fail { stage, errno } => self.failure = Some((stage, errno)),
        }
    }

    /// Called when the pipe reports EOF.
    pub fn on_eof(&self) -> Verdict {
        if let Some((stage, errno)) = self.failure {
            return Verdict::Failed { stage, errno };
        }
        match self.hello {
            Some(child) => Verdict::Started {
                supervisor: self.supervisor,
                child,
            },
            // EOF with no Hello: the process died during setup, before it
            // could reach execve. This is the case that catches the std
            // sanitize_standard_fds abort, an OOM kill, or a SIGKILL.
            None => Verdict::Failed {
                stage: Stage::PreFork,
                errno: sys::ENOENT,
            },
        }
    }
}