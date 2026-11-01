//! The append-only log sink.
//!
//! Opened before the fork, written by the supervisor, never by the child. The
//! sink is opened `O_NOFOLLOW`: a log path that has been replaced by a symlink
//! is an attack on the process that holds root's writing hand, so the open
//! fails instead of following it.
//!
//! On `SIGHUP` the path is re-checked by `st_dev`/`st_ino` and reopened only if
//! the file was rotated. A mismatch that cannot be reopened leaves the old
//! descriptor in place: a broken rotation must not cost the daemon its logs.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::error::{Fail, Stage};
use crate::sys;

pub struct Log {
    path: PathBuf,
    file: File,
    dev: u64,
    ino: u64,
    /// Set when a reopen failed, so the supervisor can say so once.
    pub degraded: bool,
}

impl Log {
    pub fn open(path: &Path) -> Result<Log, Fail> {
        let file = open_append(path)?;
        let md = file
            .metadata()
            .map_err(|e| Fail::new(Stage::Open, errno_of(&e), "fstat"))?;
        Ok(Log {
            path: path.to_path_buf(),
            dev: md.dev(),
            ino: md.ino(),
            file,
            degraded: false,
        })
    }

    pub fn fd(&self) -> i32 {
        use std::os::unix::io::AsRawFd;
        self.file.as_raw_fd()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append, best effort. A log that cannot be written must never take the
    /// supervised process down with it.
    pub fn write(&mut self, buf: &[u8]) {
        let _ = self.file.write_all(buf);
    }

    /// Reopen if the path now names a different inode.
    pub fn reopen_if_rotated(&mut self) {
        let Ok(md) = std::fs::metadata(&self.path) else {
            self.degraded = true;
            return;
        };
        if md.dev() == self.dev && md.ino() == self.ino {
            return;
        }
        match open_append(&self.path) {
            Ok(f) => {
                self.file = f;
                self.dev = md.dev();
                self.ino = md.ino();
                self.degraded = false;
            }
            // Keep writing to the old, still-open inode rather than losing
            // output to a path that is momentarily missing.
            Err(_) => self.degraded = true,
        }
    }
}

fn open_append(path: &Path) -> Result<File, Fail> {
    OpenOptions::new()
        .append(true)
        .create(true)
        .write(true)
        .mode(0o640)
        .custom_flags(sys::O_NOFOLLOW)
        .open(path)
        .map_err(|e| {
            Fail::new(Stage::Open, errno_of(&e), "open").with_path(path.to_path_buf())
        })
}

fn errno_of(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(sys::EIO)
}