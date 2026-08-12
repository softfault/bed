//! Linux PTY integration tests.
//!
//! A real pseudoterminal is used so mode transitions, resize polling, UTF-8
//! input, and restoration are exercised through the same kernel ABI as an
//! interactive session.
//!
//! References:
//! - Linux UAPI [`fcntl.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/asm-generic/fcntl.h),
//!   [`ioctls.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/asm-generic/ioctls.h), and
//!   [`termbits.h`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/include/uapi/asm-generic/termbits.h)
//! - Linux man-pages [`posix_openpt(3)`](https://man7.org/linux/man-pages/man3/posix_openpt.3.html)
//!   and [`pts(4)`](https://man7.org/linux/man-pages/man4/pts.4.html)

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#![deny(clippy::undocumented_unsafe_blocks)]

use std::{
    ffi::{CStr, c_char, c_int, c_ulong},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
        unix::fs::OpenOptionsExt,
    },
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

use bed_terminal::Terminal;

// Open/fcntl values and ioctl requests from the UAPI headers linked above.
const O_RDWR: c_int = 0x0002;
const O_NOCTTY: c_int = 0x0100;
const O_NONBLOCK: c_int = 0x0800;
const O_CLOEXEC: c_int = 0x80000;
const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const TCGETS: c_ulong = 0x5401;
const TIOCSWINSZ: c_ulong = 0x5414;
const ICANON: u32 = 0x0002;
const NCCS: usize = 19;
const TIMEOUT: Duration = Duration::from_secs(5);
const PANIC_HELPER_ENV: &str = "BED_TERMINAL_PANIC_HELPER";

static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KernelTermios {
    input_flags: u32,
    output_flags: u32,
    control_flags: u32,
    local_flags: u32,
    line_discipline: u8,
    control_characters: [u8; NCCS],
}

#[repr(C)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

// PTY and descriptor functions declared by the system C library headers.
unsafe extern "C" {
    fn posix_openpt(flags: c_int) -> c_int;
    fn grantpt(file_descriptor: c_int) -> c_int;
    fn unlockpt(file_descriptor: c_int) -> c_int;
    fn ptsname_r(file_descriptor: c_int, buffer: *mut c_char, length: usize) -> c_int;
    fn ioctl(file_descriptor: c_int, request: c_ulong, ...) -> c_int;
    fn fcntl(file_descriptor: c_int, command: c_int, ...) -> c_int;
}

struct PseudoTerminal {
    master: File,
    slave: File,
}

impl PseudoTerminal {
    fn new(rows: u16, columns: u16) -> io::Result<Self> {
        // SAFETY: the flags are valid for posix_openpt and the call has no
        // pointer arguments.
        let master_fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC) };
        if master_fd == -1 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `master_fd` is newly returned and ownership is transferred
        // exactly once to `File`.
        let master = unsafe { File::from_raw_fd(master_fd) };
        // SAFETY: `master_fd` remains live and owned by `master`.
        check_zero_errno(unsafe { grantpt(master_fd) })?;
        // SAFETY: the same live PTY master is ready to be unlocked.
        check_zero_errno(unsafe { unlockpt(master_fd) })?;

        let mut path = [0; 128];
        // SAFETY: `path` is writable for its declared length and `master_fd`
        // identifies a successfully opened PTY master.
        check_error_number(unsafe { ptsname_r(master_fd, path.as_mut_ptr(), path.len()) })?;
        // SAFETY: successful ptsname_r wrote a NUL-terminated pathname.
        let path = unsafe { CStr::from_ptr(path.as_ptr()) };
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(std::ffi::OsStr::from_bytes(path.to_bytes()))?;

        let mut terminal = Self { master, slave };
        terminal.resize(rows, columns)?;
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
        // SAFETY: the slave fd is live and `size` has Linux `winsize` layout.
        check_not_minus_one(unsafe { ioctl(self.slave.as_raw_fd(), TIOCSWINSZ, &size) }).map(|_| ())
    }

    fn termios(&self) -> io::Result<KernelTermios> {
        let mut termios = std::mem::MaybeUninit::<KernelTermios>::uninit();
        // SAFETY: the slave fd is live and `termios` is writable storage with
        // the Linux kernel `termios` layout.
        check_not_minus_one(unsafe {
            ioctl(self.slave.as_raw_fd(), TCGETS, termios.as_mut_ptr())
        })?;
        // SAFETY: successful TCGETS initialized the complete structure.
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

    terminal.wait_for_output(b"\x1b[?2004l")?;
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

    terminal.wait_for_raw_mode()?;
    terminal.wait_for_output(b"PANIC_READY")?;
    terminal.write_input(b"x")?;
    terminal.wait_for_output(b"\x1b[?2004l")?;
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

fn check_zero_errno(result: c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn check_error_number(result: c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

fn check_not_minus_one(result: c_int) -> io::Result<c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
