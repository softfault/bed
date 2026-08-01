//! Native terminal boundary for bed.
//!
//! Each supported operating system selects one handwritten backend at compile
//! time. Backends translate native input into [`Key`] and own terminal setup
//! and restoration; no platform ABI escapes this crate.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::Result;

#[cfg(all(
    any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd"
    ),
    any(
        all(any(target_os = "linux", target_os = "macos"), target_arch = "aarch64"),
        target_arch = "x86_64"
    )
))]
#[path = "terminal/vt.rs"]
mod vt;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "terminal/linux.rs"]
mod platform;

#[cfg(all(
    target_os = "macos",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "terminal/macos.rs"]
mod platform;

#[cfg(all(target_os = "freebsd", target_arch = "x86_64"))]
#[path = "terminal/freebsd.rs"]
mod platform;

#[cfg(all(target_os = "netbsd", target_arch = "x86_64"))]
#[path = "terminal/netbsd.rs"]
mod platform;

#[cfg(windows)]
#[path = "terminal/windows.rs"]
mod platform;

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
    windows
)))]
compile_error!(
    "bed currently supports Linux and macOS on x86_64/aarch64, and Windows, FreeBSD, and NetBSD on x86_64"
);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialKey {
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Paste(String),
    Tab,
    BackTab,
    Enter,
    Backspace,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Ctrl(char),
    Modified(SpecialKey, Modifiers),
    Resize,
    Escape,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: usize,
    pub columns: usize,
}

pub struct Terminal {
    platform: platform::PlatformTerminal,
}

impl Terminal {
    pub fn new() -> Result<Self> {
        Ok(Self {
            platform: platform::PlatformTerminal::new()?,
        })
    }

    pub fn size(&self) -> Result<TerminalSize> {
        self.platform.size()
    }

    pub fn read_key(&mut self) -> Result<Key> {
        self.platform.read_key()
    }

    pub fn draw(&mut self, bytes: &[u8]) -> Result<()> {
        self.platform.draw(bytes)
    }
}
