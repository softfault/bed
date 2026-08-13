//! FreeBSD terminal backend.
//!
//! This module uses the FreeBSD C `termios` and `ioctl` ABI directly. It is
//! limited to x86_64 because that is the target currently cross-checked by bed.
//!
//! Authoritative references:
//! - FreeBSD src [`_termios.h`](https://cgit.freebsd.org/src/tree/sys/sys/_termios.h),
//!   [`termios.h`](https://cgit.freebsd.org/src/tree/sys/sys/termios.h), and
//!   [`ttycom.h`](https://cgit.freebsd.org/src/tree/sys/sys/ttycom.h)
//! - FreeBSD man pages [`termios(4)`](https://man.freebsd.org/cgi/man.cgi?query=termios&sektion=4)
//!   and [`tcsetattr(3)`](https://man.freebsd.org/cgi/man.cgi?query=tcsetattr&sektion=3)
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

// Attribute action from termios.h and window-size request from ttycom.h.
const TCSAFLUSH: c_int = 2;
const TIOCGWINSZ: c_ulong = 0x4008_7468;

// Raw-mode flags and control-character indices from FreeBSD termios.h.
const BRKINT: u32 = 0x0000_0002;
const ICRNL: u32 = 0x0000_0100;
const INPCK: u32 = 0x0000_0010;
const ISTRIP: u32 = 0x0000_0020;
const IXON: u32 = 0x0000_0200;
const OPOST: u32 = 0x0000_0001;
const CS8: u32 = 0x0000_0300;
const ECHO: u32 = 0x0000_0008;
const ICANON: u32 = 0x0000_0100;
const IEXTEN: u32 = 0x0000_0400;
const ISIG: u32 = 0x0000_0080;
const VMIN: usize = 16;
const VTIME: usize = 17;
const NCCS: usize = 20;

// xterm private modes 1049, 2004, and 25 select the alternate screen,
// bracketed paste, and cursor visibility respectively.
const ENTER_TERMINAL_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?2004h\x1b[?25l";
const LEAVE_TERMINAL_SCREEN: &[u8] = b"\x1b[?25h\x1b[?2004l\x1b[?1049l\x1b[0 q";

/// FreeBSD `struct termios` from _termios.h.
#[repr(C)]
#[derive(Clone, Copy)]
struct FreeBsdTermios {
    input_flags: u32,
    output_flags: u32,
    control_flags: u32,
    local_flags: u32,
    control_characters: [u8; NCCS],
    input_speed: u32,
    output_speed: u32,
}

/// FreeBSD `struct winsize` from ttycom.h.
#[repr(C)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

const _: () = {
    // Reject a target at compile time if its C layout differs from the FreeBSD
    // ABI selected by this module.
    assert!(size_of::<FreeBsdTermios>() == 44);
    assert!(align_of::<FreeBsdTermios>() == 4);
    assert!(size_of::<WindowSize>() == 8);
    assert!(align_of::<WindowSize>() == 2);
};

// Function signatures follow FreeBSD termios.h and <sys/ioctl.h>.
unsafe extern "C" {
    fn tcgetattr(file_descriptor: c_int, termios: *mut FreeBsdTermios) -> c_int;
    fn tcsetattr(
        file_descriptor: c_int,
        optional_actions: c_int,
        termios: *const FreeBsdTermios,
    ) -> c_int;
    fn ioctl(file_descriptor: c_int, request: c_ulong, ...) -> c_int;
}

pub(super) struct PlatformTerminal {
    original: FreeBsdTermios,
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
        // Keep ISIG disabled so Ctrl-C is delivered as input. VMIN=0/VTIME=1
        // bounds an idle read to one decisecond, allowing resize polling
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
        // Terminal construction owns both changes, so Drop reverses the screen
        // state and then the input mode during normal return and Rust unwind.
        // Drop cannot run after process abort or forcible termination.
        let _ = self.output.write_all(LEAVE_TERMINAL_SCREEN);
        let _ = self.output.flush();
        let _ = set_termios(self.input_fd, &self.original);
    }
}

fn get_termios(file_descriptor: c_int) -> Result<FreeBsdTermios> {
    let mut termios = MaybeUninit::<FreeBsdTermios>::uninit();
    // SAFETY: `file_descriptor` belongs to the live stdin handle, and the
    // output pointer is valid for a FreeBSD `struct termios`.
    let status = unsafe { tcgetattr(file_descriptor, termios.as_mut_ptr()) };
    ensure!(
        status != -1,
        "failed to read terminal settings: {}",
        io::Error::last_os_error()
    );
    // SAFETY: successful tcgetattr initialized the complete structure.
    Ok(unsafe { termios.assume_init() })
}

fn set_termios(file_descriptor: c_int, termios: &FreeBsdTermios) -> Result<()> {
    // SAFETY: the descriptor is live and `termios` has FreeBSD's exact C layout.
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
    // is writable storage for FreeBSD `struct winsize`.
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
