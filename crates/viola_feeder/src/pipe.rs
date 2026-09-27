// SPDX-License-Identifier: GPL-3.0-or-later
//! Consumer side of `\\.\pipe\orender.input`.
//!
//! Measured behaviour of `orender` (see `docs/cloud-boundary.md` and the README):
//! when the pipe does not exist yet, **the engine creates it and waits for a
//! client**. So this side only has to open the path — which `std::fs` can do on
//! Windows — with no Win32 named-pipe calls and no FFI of our own.
//!
//! The engine tears the pipe down and re-creates it between streams, so a
//! dropped connection is normal: callers re-`connect` (and re-send the WAV
//! header) instead of treating it as fatal.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(crate) struct PipeWriter {
    path: PathBuf,
    file: Option<File>,
}

impl PipeWriter {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            file: None,
        }
    }

    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Open the pipe, retrying until `timeout` elapses.
    ///
    /// The engine may not have created the pipe yet, and opening a
    /// not-yet-existing pipe is an ordinary "file not found" here.
    pub(crate) fn connect(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        loop {
            match File::options().write(true).open(&self.path) {
                Ok(file) => {
                    self.file = Some(file);
                    return Ok(());
                }
                Err(err) => {
                    last = format!("{}: {}", self.path.display(), err);
                    if Instant::now() >= deadline {
                        return Err(format!("could not open the pipe ({last})"));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }

    pub(crate) fn write_all(&mut self, buf: &[u8]) -> Result<(), String> {
        let Some(file) = self.file.as_mut() else {
            return Err("not connected".into());
        };
        match file.write_all(buf).and_then(|()| file.flush()) {
            Ok(()) => Ok(()),
            Err(err) => {
                // Drop the handle so the next attempt reconnects from scratch.
                self.file = None;
                Err(format!("write failed: {err}"))
            }
        }
    }
}
