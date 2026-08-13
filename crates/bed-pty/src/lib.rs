//! Native pseudoterminal process boundary for bed.
//!
//! A [`PtyProcess`] owns one child process and its PTY or ConPTY. Terminal
//! emulation, session identity, scrollback, and UI placement belong to higher
//! layers.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::{Result, ensure};
use std::{
    fs::File,
    io::{Read, Write},
    process::{Command, ExitStatus},
};

#[cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "freebsd", target_arch = "x86_64"),
    all(target_os = "netbsd", target_arch = "x86_64")
))]
mod unix;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
mod windows;

#[cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "freebsd", target_arch = "x86_64"),
    all(target_os = "netbsd", target_arch = "x86_64")
))]
use unix as platform;
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
use windows as platform;

#[cfg(not(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "freebsd", target_arch = "x86_64"),
    all(target_os = "netbsd", target_arch = "x86_64"),
    all(target_os = "windows", target_arch = "x86_64")
)))]
compile_error!(
    "bed-pty currently supports Linux and macOS on x86_64/aarch64, and Windows, FreeBSD, and NetBSD on x86_64"
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtySize {
    rows: u16,
    columns: u16,
}

impl PtySize {
    pub fn new(rows: u16, columns: u16) -> Result<Self> {
        ensure!(rows > 0 && columns > 0, "PTY size must be nonzero");
        ensure!(
            rows <= i16::MAX as u16 && columns <= i16::MAX as u16,
            "PTY size exceeds the native coordinate range"
        );
        Ok(Self { rows, columns })
    }

    pub fn rows(self) -> u16 {
        self.rows
    }

    pub fn columns(self) -> u16 {
        self.columns
    }
}

pub struct PtyProcess {
    platform: platform::PlatformPtyProcess,
}

impl PtyProcess {
    /// Spawns a command attached to a native PTY or ConPTY.
    ///
    /// Explicit environment additions and removals are preserved on Windows.
    /// Rust's stable [`Command`] inspection API does not expose whether
    /// [`Command::env_clear`] was called, so that operation cannot currently
    /// be reproduced by the ConPTY backend.
    pub fn spawn(command: &mut Command, size: PtySize) -> Result<Self> {
        Ok(Self {
            platform: platform::PlatformPtyProcess::spawn(command, size)?,
        })
    }

    pub fn resize(&mut self, size: PtySize) -> Result<()> {
        self.platform.resize(size)
    }

    pub fn try_clone_reader(&self) -> Result<File> {
        self.platform.try_clone_reader()
    }

    pub fn try_clone_writer(&self) -> Result<File> {
        self.platform.try_clone_writer()
    }

    pub fn process_id(&self) -> u32 {
        self.platform.process_id()
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.platform.try_wait()
    }

    pub fn wait(&mut self) -> Result<ExitStatus> {
        self.platform.wait()
    }

    pub fn terminate(&mut self) -> Result<()> {
        self.platform.terminate()
    }
}

impl Read for PtyProcess {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.platform.read(bytes)
    }
}

impl Write for PtyProcess {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.platform.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.platform.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::PtySize;

    #[test]
    fn validates_native_pty_dimensions() {
        assert_eq!(
            PtySize::new(24, 80).unwrap(),
            PtySize {
                rows: 24,
                columns: 80
            }
        );
        assert_eq!(PtySize::new(24, 80).unwrap().rows(), 24);
        assert_eq!(PtySize::new(24, 80).unwrap().columns(), 80);
        assert!(PtySize::new(0, 80).is_err());
        assert!(PtySize::new(24, 0).is_err());
        assert!(PtySize::new(u16::MAX, 80).is_err());
    }
}
