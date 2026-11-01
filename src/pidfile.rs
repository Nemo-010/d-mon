//! The pidfile.
//!
//! `flock` on the **final** inode, not on a temporary one that is then renamed
//! over it: a rename-based pidfile locks an inode nobody else opens, so two
//! concurrent starts both "succeed" and the second overwrites the first's pid.
//! Writing in place after taking the lock means the lock is a real mutex on the
//! path, and a crashed supervisor's file is simply re-locked on the next start.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::sys;

pub struct Pidfile {
    path: PathBuf,
    file: File,
}

impl Pidfile {
    /// Take the lock and record `pid`. Fails with `AlreadyRunning` when a live
    /// process holds it, which the CLI maps to exit code 3.
    ///
    /// The lock is taken on the inode the path names *now*: after `flock`
    /// succeeds the descriptor and the path are compared, and a mismatch means
    /// the file was replaced or unlinked underneath us, so the whole attempt is
    /// retried. Without that check a supervisor that started while the previous
    /// one was releasing its file holds a lock on an inode no one can find.
    pub fn acquire(path: &Path, pid: i32) -> Result<Pidfile, Error> {
        for _ in 0..5 {
            match Self::try_acquire(path, pid)? {
                Some(p) => return Ok(p),
                None => continue,
            }
        }
        Err(Error::Env {
            op: "pidfile",
            errno: sys::EAGAIN,
            syscall: "flock (file kept being replaced)",
        })
    }

    fn try_acquire(path: &Path, pid: i32) -> Result<Option<Pidfile>, Error> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o644)
            .custom_flags(sys::O_NOFOLLOW)
            .open(path)
            .map_err(|e| Error::Env {
                op: "pidfile",
                errno: e.raw_os_error().unwrap_or(sys::EIO),
                syscall: "open",
            })?;

        // SAFETY: flock on a valid descriptor.
        let rc = unsafe { sys::flock(file.as_raw_fd(), sys::LOCK_EX | sys::LOCK_NB) };
        if rc < 0 {
            let e = sys::errno();
            if e == sys::EWOULDBLOCK || e == sys::EAGAIN {
                let mut s = String::new();
                let _ = file.read_to_string(&mut s);
                return Err(Error::AlreadyRunning {
                    path: path.to_path_buf(),
                    pid: s.trim().parse().ok(),
                });
            }
            return Err(Error::Env {
                op: "pidfile",
                errno: e,
                syscall: "flock",
            });
        }

        match (file.metadata(), std::fs::metadata(path)) {
            (Ok(held), Ok(named)) if held.dev() == named.dev() && held.ino() == named.ino() => {}
            (Ok(_), Ok(_)) => return Ok(None),
            _ => return Ok(None),
        }
        file.set_len(0).map_err(|e| Error::Env {
            op: "pidfile",
            errno: e.raw_os_error().unwrap_or(sys::EIO),
            syscall: "ftruncate",
        })?;
        let _ = file.seek(SeekFrom::Start(0));
        file.write_all(format!("{pid}\n").as_bytes())
            .map_err(|e| Error::Env {
                op: "pidfile",
                errno: e.raw_os_error().unwrap_or(sys::EIO),
                syscall: "write",
            })?;
        let _ = file.sync_all();

        Ok(Some(Pidfile {
            path: path.to_path_buf(),
            file,
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Remove the file on a clean exit, while the lock is still held, so no
    /// other starter can take the path between the removal and the release.
    pub fn release(self) {
        let _ = std::fs::remove_file(&self.path);
        drop(self.file);
    }
}