//! macOS PTY integration tests.
//!
//! `openpty` provides a real Darwin pseudoterminal so raw mode, resize polling,
//! UTF-8 input, and restoration are tested against native kernel behavior.
//!
//! References:
//! - Apple XNU [`termios.h`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/termios.h),
//!   [`ttycom.h`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/ttycom.h), and
//!   [`fcntl.h`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/fcntl.h)

#![cfg(all(
    target_os = "macos",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::{
    ffi::{c_char, c_int, c_ulong},
    fs::{self, File},
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    ptr,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use bed_terminal::Terminal;

// fcntl values and ioctl request from the XNU headers linked above.
const O_NONBLOCK: c_int = 0x0004;
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const TIOCSWINSZ: c_ulong = 0x8008_7467;
const ICANON: u64 = 0x0000_0100;
const NCCS: usize = 20;
const TIMEOUT: Duration = Duration::from_secs(5);
const PANIC_HELPER_ENV: &str = "BED_TERMINAL_PANIC_HELPER";

static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DarwinTermios {
    input_flags: u64,
    output_flags: u64,
    control_flags: u64,
    local_flags: u64,
    control_characters: [u8; NCCS],
    input_speed: u64,
    output_speed: u64,
}

#[repr(C)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

const _: () = {
    assert!(size_of::<DarwinTermios>() == 72);
    assert!(align_of::<DarwinTermios>() == 8);
    assert!(size_of::<WindowSize>() == 8);
    assert!(align_of::<WindowSize>() == 2);
};

// PTY and descriptor functions declared by the Apple SDK headers.
unsafe extern "C" {
    fn openpty(
        master: *mut c_int,
        slave: *mut c_int,
        name: *mut c_char,
        termios: *const DarwinTermios,
        size: *const WindowSize,
    ) -> c_int;
    fn tcgetattr(file_descriptor: c_int, termios: *mut DarwinTermios) -> c_int;
    fn ioctl(file_descriptor: c_int, request: c_ulong, ...) -> c_int;
    fn fcntl(file_descriptor: c_int, command: c_int, ...) -> c_int;
}

struct PseudoTerminal {
    master: File,
    slave: File,
}

impl PseudoTerminal {
    fn new(rows: u16, columns: u16) -> io::Result<Self> {
        let size = WindowSize {
            rows,
            columns,
            x_pixels: 0,
            y_pixels: 0,
        };
        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: the output pointers are writable, the optional name and
        // termios pointers are null, and `size` has Darwin `winsize` layout.
        check_not_minus_one(unsafe {
            openpty(
                &mut master_fd,
                &mut slave_fd,
                ptr::null_mut(),
                ptr::null_mut(),
                &size,
            )
        })?;

        // SAFETY: openpty returned two new descriptors; ownership of each is
        // transferred exactly once to a `File`.
        let master = unsafe { File::from_raw_fd(master_fd) };
        // SAFETY: see above; this is the distinct slave descriptor.
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        let terminal = Self { master, slave };
        terminal.set_master_nonblocking()?;
        Ok(terminal)
    }

    fn spawn(&self, path: &PathBuf) -> io::Result<ChildGuard> {
        self.spawn_command(Command::new(env!("CARGO_BIN_EXE_bed")).arg(path))
    }

    fn spawn_command(&self, command: &mut Command) -> io::Result<ChildGuard> {
        let child = command
            .stdin(Stdio::from(self.slave.try_clone()?))
            .stdout(Stdio::from(self.slave.try_clone()?))
            .stderr(Stdio::null())
            .spawn()?;
        Ok(ChildGuard(Some(child)))
    }

    fn resize(&mut self, rows: u16, columns: u16) -> io::Result<()> {
        let size = WindowSize {
            rows,
            columns,
            x_pixels: 0,
            y_pixels: 0,
        };
        // SAFETY: the slave fd is live and `size` has Darwin `winsize` layout.
        check_not_minus_one(unsafe { ioctl(self.slave.as_raw_fd(), TIOCSWINSZ, &size) }).map(|_| ())
    }

    fn termios(&self) -> io::Result<DarwinTermios> {
        let mut termios = std::mem::MaybeUninit::<DarwinTermios>::uninit();
        // SAFETY: the slave fd is live and `termios` is writable storage with
        // Darwin's exact C layout.
        check_not_minus_one(unsafe { tcgetattr(self.slave.as_raw_fd(), termios.as_mut_ptr()) })?;
        // SAFETY: successful tcgetattr initialized the complete structure.
        Ok(unsafe { termios.assume_init() })
    }

    fn wait_for_raw_mode(&self) -> io::Result<()> {
        wait_until(|| Ok(self.termios()?.local_flags & ICANON == 0))
    }

    fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.master.write_all(bytes)
    }

    fn wait_for_frame(&mut self) -> io::Result<Vec<u8>> {
        self.wait_for_output(b"NORMAL")
    }

    fn wait_for_output(&mut self, needle: &[u8]) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        wait_until(|| {
            self.read_available(&mut output)?;
            Ok(contains(&output, needle))
        })
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("terminal output did not contain {needle:?}: {error}"),
            )
        })?;
        Ok(output)
    }

    fn read_available(&mut self, output: &mut Vec<u8>) -> io::Result<()> {
        let mut buffer = [0; 4096];
        loop {
            match self.master.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(length) => output.extend_from_slice(&buffer[..length]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn set_master_nonblocking(&self) -> io::Result<()> {
        // SAFETY: the master fd is live; F_GETFL has no variadic argument.
        let flags = check_not_minus_one(unsafe { fcntl(self.master.as_raw_fd(), F_GETFL) })?;
        // SAFETY: the master fd is live and the third argument is the flag word
        // required by F_SETFL.
        check_not_minus_one(unsafe {
            fcntl(self.master.as_raw_fd(), F_SETFL, flags | O_NONBLOCK)
        })?;
        Ok(())
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn wait(mut self) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.0.as_mut().expect("child exists").try_wait()? {
                self.0 = None;
                return Ok(status);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "bed did not exit"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct TempPath(PathBuf);

impl TempPath {
    fn new() -> Self {
        let id = NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed);
        Self(
            std::env::temp_dir().join(format!("bed-terminal-test-{}-{id}.txt", std::process::id())),
        )
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn edits_resizes_and_restores_a_real_pseudo_terminal() -> io::Result<()> {
    let path = TempPath::new();
    let mut terminal = PseudoTerminal::new(24, 80)?;
    let original = terminal.termios()?;
    let child = terminal.spawn(&path.0)?;

    terminal.wait_for_raw_mode()?;
    terminal.wait_for_frame()?;
    terminal.resize(30, 100)?;
    terminal.wait_for_frame()?;
    terminal.write_input("ihello好\x1b:wq\r".as_bytes())?;

    terminal.wait_for_output(b"\x1b[?25h\x1b[?1006l\x1b[?1003l\x1b[?2004l\x1b[?1049l\x1b[0 q")?;
    let status = child.wait()?;
    assert!(status.success());
    assert_eq!(fs::read(&path.0)?, "hello好".as_bytes());
    assert_eq!(terminal.termios()?, original);
    Ok(())
}

#[test]
fn restores_a_real_pseudo_terminal_during_panic_unwind() -> io::Result<()> {
    let mut terminal = PseudoTerminal::new(24, 80)?;
    let original = terminal.termios()?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", "terminal_panic_helper", "--nocapture"])
        .env(PANIC_HELPER_ENV, "1");
    let child = terminal.spawn_command(&mut command)?;

    terminal.wait_for_output(b"PANIC_READY")?;
    terminal.write_input(b"x")?;
    terminal.wait_for_output(b"\x1b[?25h\x1b[?1006l\x1b[?1003l\x1b[?2004l\x1b[?1049l\x1b[0 q")?;
    let status = child.wait()?;

    assert!(!status.success());
    assert_eq!(terminal.termios()?, original);
    Ok(())
}

#[test]
fn terminal_panic_helper() {
    if std::env::var_os(PANIC_HELPER_ENV).is_none() {
        return;
    }
    let mut terminal = Terminal::new().unwrap();
    terminal.draw(b"PANIC_READY").unwrap();
    terminal.read_key().unwrap();
    panic!("intentional terminal restoration test");
}

fn wait_until(mut predicate: impl FnMut() -> io::Result<bool>) -> io::Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    while Instant::now() < deadline {
        if predicate()? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "terminal state did not change",
    ))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn check_not_minus_one(result: c_int) -> io::Result<c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
