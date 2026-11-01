//! What the child's descriptors should be.

use std::path::PathBuf;

/// Descriptor policy for the child.
///
/// `Discard` is the only variant that wants a null device, and it degrades:
/// `/dev/null`, then a pipe drained by the supervisor, then leave it inherited.
/// It never falls back to `open("/")`, which would hand the child a directory
/// descriptor it could `openat` its way out of a chroot with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stdio {
    /// Leave whatever was inherited.
    Inherit,
    /// Send to a null sink.
    Discard,
    /// Capture and write to the configured `log_file`.
    Capture,
    /// Append to this file.
    File(PathBuf),
}

impl Default for Stdio {
    fn default() -> Self {
        Stdio::Inherit
    }
}

/// How `Discard` found a sink. Reported so the operator is not surprised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    NullDevice,
    DrainedPipe,
    Inherited,
}