//! Linux terminal backend.
//!
//! This module calls the C `ioctl` entry point with Linux kernel UAPI values.
//! It deliberately declares only the ABI used by bed. The x86_64 and aarch64
//! targets are enabled because both use the asm-generic definitions below.
//!
//! Authoritative references:
//! - Linux UAPI [`termbits.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/asm-generic/termbits.h)
//!   and [`termbits-common.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/asm-generic/termbits-common.h)
//! - Linux UAPI [`ioctls.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/asm-generic/ioctls.h)
//!   and [`termios.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/asm-generic/termios.h)
//! - Linux man-pages [`termios(3)`](https://man7.org/linux/man-pages/man3/termios.3.html)
//!   and [`ioctl_tty(2)`](https://man7.org/linux/man-pages/man2/ioctl_tty.2.html)
//! - xterm [`ctlseqs`](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)

use super::{Key, TerminalSize, vt::VtInput};
use anyhow::{Context, Result, ensure};
use std::{
    cell::Cell,
    ffi::{c_int, c_ulong},
    io::{self, IsTerminal, Write},
    mem::MaybeUninit,
    os::fd::AsRawFd,
};

// Requests from asm-generic/ioctls.h. TCSETSF applies the new attributes after
// pending output is written and discards unread input.
const TCGETS: c_ulong = 0x5401;
const TCSETSF: c_ulong = 0x5404;
const TIOCGWINSZ: c_ulong = 0x5413;

// Raw-mode flags and control-character indices from asm-generic/termbits.h.
const BRKINT: u32 = 0x0002;
const ICRNL: u32 = 0x0100;
const INPCK: u32 = 0x0010;
const ISTRIP: u32 = 0x0020;
const IXON: u32 = 0x0400;
const OPOST: u32 = 0x0001;
const CS8: u32 = 0x0030;
const ECHO: u32 = 0x0008;
const ICANON: u32 = 0x0002;
const IEXTEN: u32 = 0x8000;
const ISIG: u32 = 0x0001;
const VTIME: usize = 5;
const VMIN: usize = 6;
const NCCS: usize = 19;

// xterm private modes 1049, 2004, and 25 select the alternate screen,
// bracketed paste, and cursor visibility respectively.
const ENTER_TERMINAL_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?2004h\x1b[?25l";
const LEAVE_TERMINAL_SCREEN: &[u8] = b"\x1b[?25h\x1b[?2004l\x1b[?1049l\x1b[0 q";

/// Linux UAPI `struct termios`, not the differently sized libc `termios`.
#[repr(C)]
#[derive(Clone, Copy)]
struct KernelTermios {
    input_flags: u32,
    output_flags: u32,
    control_flags: u32,
    local_flags: u32,
    line_discipline: u8,
    control_characters: [u8; NCCS],
}

/// Linux UAPI `struct winsize`.
#[repr(C)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

const _: () = {
    // Reject a target at compile time if its C layout differs from the UAPI
    // selected by this module.
    assert!(size_of::<KernelTermios>() == 36);
    assert!(align_of::<KernelTermios>() == 4);
    assert!(size_of::<WindowSize>() == 8);
    assert!(align_of::<WindowSize>() == 2);
};

// `ioctl` is the variadic C entry point declared by <sys/ioctl.h>; request
// argument types are fixed by the Linux UAPI links above.
unsafe extern "C" {
    fn ioctl(file_descriptor: c_int, request: c_ulong, ...) -> c_int;
}

pub(super) struct PlatformTerminal {
    original: KernelTermios,
    input_fd: c_int,
    output_fd: c_int,
    input: VtInput<io::Stdin>,
    output: io::Stdout,
    last_size: Cell<TerminalSize>,
}

impl PlatformTerminal {
    pub(super) fn new() -> Result<Self> {
        let input = io::stdin();
        let output = io::stdout();
        ensure!(input.is_terminal(), "standard input is not a terminal");
        ensure!(output.is_terminal(), "standard output is not a terminal");

        let input_fd = input.as_raw_fd();
        let output_fd = output.as_raw_fd();
        let original = get_termios(input_fd)?;
        let last_size = Cell::new(get_window_size(output_fd)?);
        let mut raw = original;
        // Keep ISIG disabled so Ctrl-C is delivered as input. VMIN=0 and
        // VTIME=1 bound an idle read to one decisecond, allowing resize polling
        // without taking ownership of the process-wide SIGWINCH handler.
        raw.input_flags &= !(BRKINT | ICRNL | INPCK | ISTRIP | IXON);
        raw.output_flags &= !OPOST;
        raw.control_flags |= CS8;
        raw.local_flags &= !(ECHO | ICANON | IEXTEN | ISIG);
        raw.control_characters[VMIN] = 0;
        raw.control_characters[VTIME] = 1;
        set_termios(input_fd, &raw)?;

        let mut terminal = Self {
            original,
            input_fd,
            output_fd,
            input: VtInput::new(input),
            output,
            last_size,
        };
        terminal.draw(ENTER_TERMINAL_SCREEN)?;
        Ok(terminal)
    }

    pub(super) fn size(&self) -> Result<TerminalSize> {
        let size = get_window_size(self.output_fd)?;
        self.last_size.set(size);
        Ok(size)
    }

    pub(super) fn read_key(&mut self) -> Result<Key> {
        loop {
            if let Some(key) = self.input.read_key()? {
                return Ok(key);
            }
            // A zero-byte read is the VTIME timeout configured in new().
            let size = get_window_size(self.output_fd)?;
            if size != self.last_size.replace(size) {
                return Ok(Key::Resize);
            }
        }
    }

    pub(super) fn draw(&mut self, bytes: &[u8]) -> Result<()> {
        self.output
            .write_all(bytes)
            .context("failed to write terminal output")?;
        self.output
            .flush()
            .context("failed to flush terminal output")
    }
}

impl Drop for PlatformTerminal {
    fn drop(&mut self) {
        // Restore the visible screen before restoring canonical input so a
        // successful return or Rust unwind leaves the caller's tty usable.
        // Drop cannot run after process abort or forcible termination.
        let _ = self.output.write_all(LEAVE_TERMINAL_SCREEN);
        let _ = self.output.flush();
        let _ = set_termios(self.input_fd, &self.original);
    }
}

fn get_termios(file_descriptor: c_int) -> Result<KernelTermios> {
    let mut termios = MaybeUninit::<KernelTermios>::uninit();
    // SAFETY: `file_descriptor` belongs to the live stdin handle, and
    // `termios` points to writable storage with the kernel UAPI layout.
    let status = unsafe { ioctl(file_descriptor, TCGETS, termios.as_mut_ptr()) };
    ensure!(
        status != -1,
        "failed to read terminal settings: {}",
        io::Error::last_os_error()
    );
    // SAFETY: a successful TCGETS initialized every field of `termios`.
    Ok(unsafe { termios.assume_init() })
}

fn set_termios(file_descriptor: c_int, termios: &KernelTermios) -> Result<()> {
    // SAFETY: the descriptor is live and `termios` has the exact TCSETSF
    // kernel layout for every target that can compile this module.
    let status = unsafe { ioctl(file_descriptor, TCSETSF, termios) };
    ensure!(
        status != -1,
        "failed to set terminal settings: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

fn get_window_size(file_descriptor: c_int) -> Result<TerminalSize> {
    let mut size = MaybeUninit::<WindowSize>::zeroed();
    // SAFETY: `file_descriptor` belongs to the live stdout handle, and `size`
    // points to writable storage matching Linux UAPI `struct winsize`.
    let status = unsafe { ioctl(file_descriptor, TIOCGWINSZ, size.as_mut_ptr()) };
    ensure!(
        status != -1,
        "failed to read terminal size: {}",
        io::Error::last_os_error()
    );
    // SAFETY: a successful TIOCGWINSZ initialized the output structure.
    let size = unsafe { size.assume_init() };
    ensure!(size.rows > 0 && size.columns > 0, "terminal size is zero");
    Ok(TerminalSize {
        rows: usize::from(size.rows),
        columns: usize::from(size.columns),
    })
}
