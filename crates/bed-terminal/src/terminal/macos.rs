//! macOS terminal backend.
//!
//! This module uses Darwin's C `termios` and `ioctl` ABI directly. Darwin's
//! `tcflag_t` and `speed_t` are 64-bit on both supported Apple targets, so this
//! layout must not be shared with the superficially similar BSD backends.
//!
//! Authoritative references:
//! - Apple XNU [`termios.h`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/termios.h)
//!   and [`ttycom.h`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/ttycom.h)
//! - POSIX [`<termios.h>`](https://pubs.opengroup.org/onlinepubs/9799919799/basedefs/termios.h.html)
//!   and [`tcgetattr`/`tcsetattr`](https://pubs.opengroup.org/onlinepubs/9799919799/functions/tcgetattr.html)
//! - xterm [`ctlseqs`](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)

use super::{HostInput, Key, TerminalSize, vt::VtInput};
use anyhow::{Context, Result, ensure};
use std::{
    cell::Cell,
    ffi::{c_int, c_ulong},
    io::{self, IsTerminal, Write},
    mem::MaybeUninit,
    os::fd::AsRawFd,
};

// Attribute action from XNU termios.h and window-size request from ttycom.h.
const TCSAFLUSH: c_int = 2;
const TIOCGWINSZ: c_ulong = 0x4008_7468;

// Raw-mode flags and control-character indices from XNU termios.h.
const BRKINT: u64 = 0x0000_0002;
const ICRNL: u64 = 0x0000_0100;
const INPCK: u64 = 0x0000_0010;
const ISTRIP: u64 = 0x0000_0020;
const IXON: u64 = 0x0000_0200;
const OPOST: u64 = 0x0000_0001;
const CS8: u64 = 0x0000_0300;
const ECHO: u64 = 0x0000_0008;
const ICANON: u64 = 0x0000_0100;
const IEXTEN: u64 = 0x0000_0400;
const ISIG: u64 = 0x0000_0080;
const VMIN: usize = 16;
const VTIME: usize = 17;
const NCCS: usize = 20;

// xterm private modes select the alternate screen, bracketed paste, all-motion
// SGR mouse reporting, and cursor visibility while bed owns the terminal.
const ENTER_TERMINAL_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?2004h\x1b[?1003h\x1b[?1006h\x1b[?25l";
const LEAVE_TERMINAL_SCREEN: &[u8] =
    b"\x1b[?2026l\x1b[?25h\x1b[?1006l\x1b[?1003l\x1b[?2004l\x1b[?1049l\x1b[0 q";

/// Darwin `struct termios` from XNU termios.h.
#[repr(C)]
#[derive(Clone, Copy)]
struct DarwinTermios {
    input_flags: u64,
    output_flags: u64,
    control_flags: u64,
    local_flags: u64,
    control_characters: [u8; NCCS],
    input_speed: u64,
    output_speed: u64,
}

/// Darwin `struct winsize` from XNU ttycom.h.
#[repr(C)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

const _: () = {
    // Reject a target at compile time if its C layout differs from the XNU ABI
    // selected by this module.
    assert!(size_of::<DarwinTermios>() == 72);
    assert!(align_of::<DarwinTermios>() == 8);
    assert!(size_of::<WindowSize>() == 8);
    assert!(align_of::<WindowSize>() == 2);
};

// Function signatures follow XNU termios.h and <sys/ioctl.h>.
unsafe extern "C" {
    fn tcgetattr(file_descriptor: c_int, termios: *mut DarwinTermios) -> c_int;
    fn tcsetattr(
        file_descriptor: c_int,
        optional_actions: c_int,
        termios: *const DarwinTermios,
    ) -> c_int;
    fn ioctl(file_descriptor: c_int, request: c_ulong, ...) -> c_int;
}

pub(super) struct PlatformTerminal {
    original: DarwinTermios,
    input_fd: c_int,
    output_fd: c_int,
    input: VtInput<io::Stdin>,
    output: io::Stdout,
    last_size: Cell<TerminalSize>,
}

pub(super) struct PlatformTerminalReader {
    input: VtInput<io::Stdin>,
    output_fd: c_int,
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
        // Keep ISIG disabled so Ctrl-C is delivered as input. POSIX
        // VMIN=0/VTIME=1 bounds an idle read to one decisecond, allowing resize
        // polling without taking ownership of the process-wide SIGWINCH handler.
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
        read_key(&mut self.input, self.output_fd, &self.last_size)
    }

    pub(super) fn input_reader(&self) -> PlatformTerminalReader {
        PlatformTerminalReader {
            input: VtInput::new(io::stdin()),
            output_fd: self.output_fd,
            last_size: Cell::new(self.last_size.get()),
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

impl PlatformTerminalReader {
    pub(super) fn size(&self) -> Result<TerminalSize> {
        let size = get_window_size(self.output_fd)?;
        self.last_size.set(size);
        Ok(size)
    }

    pub(super) fn read_event(&mut self) -> Result<HostInput> {
        read_event(&mut self.input, self.output_fd, &self.last_size)
    }
}

fn read_key(
    input: &mut VtInput<io::Stdin>,
    output_fd: c_int,
    last_size: &Cell<TerminalSize>,
) -> Result<Key> {
    loop {
        if let Some(key) = input.read_key()? {
            return Ok(key);
        }
        // A zero-byte read is the VTIME timeout configured in new().
        let size = get_window_size(output_fd)?;
        if size != last_size.replace(size) {
            return Ok(Key::Resize);
        }
    }
}

fn read_event(
    input: &mut VtInput<io::Stdin>,
    output_fd: c_int,
    last_size: &Cell<TerminalSize>,
) -> Result<HostInput> {
    loop {
        if let Some(event) = input.read_event()? {
            return Ok(event);
        }
        let size = get_window_size(output_fd)?;
        if size != last_size.replace(size) {
            return Ok(HostInput::Key(Key::Resize));
        }
    }
}

impl Drop for PlatformTerminal {
    fn drop(&mut self) {
        // Terminal construction owns both changes, so Drop reverses the screen
        // state and then the input mode during normal return and Rust unwind.
        // Drop cannot run after process abort or forcible termination.
        let _ = self.output.write_all(LEAVE_TERMINAL_SCREEN);
        let _ = self.output.flush();
        let _ = set_termios(self.input_fd, &self.original);
    }
}

fn get_termios(file_descriptor: c_int) -> Result<DarwinTermios> {
    let mut termios = MaybeUninit::<DarwinTermios>::uninit();
    // SAFETY: `file_descriptor` belongs to the live stdin handle, and the
    // output pointer is valid for a Darwin `struct termios`.
    let status = unsafe { tcgetattr(file_descriptor, termios.as_mut_ptr()) };
    ensure!(
        status != -1,
        "failed to read terminal settings: {}",
        io::Error::last_os_error()
    );
    // SAFETY: successful tcgetattr initialized the complete structure.
    Ok(unsafe { termios.assume_init() })
}

fn set_termios(file_descriptor: c_int, termios: &DarwinTermios) -> Result<()> {
    // SAFETY: the descriptor is live and `termios` has Darwin's exact C layout.
    let status = unsafe { tcsetattr(file_descriptor, TCSAFLUSH, termios) };
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
    // is writable storage for Darwin `struct winsize`.
    let status = unsafe { ioctl(file_descriptor, TIOCGWINSZ, size.as_mut_ptr()) };
    ensure!(
        status != -1,
        "failed to read terminal size: {}",
        io::Error::last_os_error()
    );
    // SAFETY: successful TIOCGWINSZ initialized the output structure.
    let size = unsafe { size.assume_init() };
    ensure!(size.rows > 0 && size.columns > 0, "terminal size is zero");
    Ok(TerminalSize {
        rows: usize::from(size.rows),
        columns: usize::from(size.columns),
    })
}
