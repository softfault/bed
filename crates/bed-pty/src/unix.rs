//! Unix PTY process backend.

use super::PtySize;
use anyhow::{Context, Result, ensure};
use std::{
    ffi::{c_int, c_ulong, c_void},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::process::CommandExt,
    },
    process::{Child, Command, ExitStatus, Stdio},
    ptr,
};

const STDIN_FILENO: c_int = 0;
const F_GETFD: c_int = 1;
const F_SETFD: c_int = 2;
const FD_CLOEXEC: c_int = 1;
const SIGKILL: c_int = 9;

#[cfg(target_os = "linux")]
const TIOCSCTTY: c_ulong = 0x540e;
#[cfg(target_os = "linux")]
const TIOCSWINSZ: c_ulong = 0x5414;

#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "netbsd"))]
const TIOCSCTTY: c_ulong = 0x2000_7461;
#[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "netbsd"))]
const TIOCSWINSZ: c_ulong = 0x8008_7467;

#[repr(C)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

const _: () = {
    assert!(size_of::<WindowSize>() == 8);
    assert!(align_of::<WindowSize>() == 2);
};

#[cfg_attr(
    any(target_os = "linux", target_os = "freebsd", target_os = "netbsd"),
    link(name = "util")
)]
unsafe extern "C" {
    fn openpty(
        master: *mut c_int,
        slave: *mut c_int,
        name: *mut i8,
        termios: *const c_void,
        size: *const WindowSize,
    ) -> c_int;
}

unsafe extern "C" {
    fn setsid() -> c_int;
    fn ioctl(file_descriptor: c_int, request: c_ulong, ...) -> c_int;
    fn fcntl(file_descriptor: c_int, command: c_int, ...) -> c_int;
    #[link_name = "kill"]
    fn kill_process(process: c_int, signal: c_int) -> c_int;
}

pub(super) struct PlatformPtyProcess {
    master: File,
    child: Child,
}

impl PlatformPtyProcess {
    pub(super) fn spawn(command: &mut Command, size: PtySize) -> Result<Self> {
        let size = native_size(size);
        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: both descriptor pointers are writable, optional name and
        // termios pointers are null, and `size` has native `winsize` layout.
        let status = unsafe {
            openpty(
                &mut master_fd,
                &mut slave_fd,
                ptr::null_mut(),
                ptr::null(),
                &size,
            )
        };
        ensure!(
            status != -1,
            "failed to open pseudoterminal: {}",
            std::io::Error::last_os_error()
        );

        // SAFETY: successful openpty returned two distinct owned descriptors.
        let master = unsafe { File::from_raw_fd(master_fd) };
        // SAFETY: ownership of the separate slave descriptor is transferred
        // exactly once to this File.
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        set_close_on_exec(&master).context("failed to protect PTY master descriptor")?;
        set_close_on_exec(&slave).context("failed to protect PTY slave descriptor")?;
        let child_stdin = slave
            .try_clone()
            .context("failed to clone PTY slave for child stdin")?;
        let child_stdout = slave
            .try_clone()
            .context("failed to clone PTY slave for child stdout")?;

        command
            .stdin(Stdio::from(child_stdin))
            .stdout(Stdio::from(child_stdout))
            .stderr(Stdio::from(slave));
        if !command.get_envs().any(|(name, _)| name == "TERM") {
            command.env("TERM", "xterm-256color");
        }

        // SAFETY: the closure calls only async-signal-safe session/ioctl
        // functions between fork and exec and does not access parent state.
        unsafe {
            command.pre_exec(|| {
                if setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if ioctl(STDIN_FILENO, TIOCSCTTY, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command
            .spawn()
            .with_context(|| format!("failed to spawn {:?} in PTY", command.get_program()))?;
        Ok(Self { master, child })
    }

    pub(super) fn resize(&mut self, size: PtySize) -> Result<()> {
        let size = native_size(size);
        // SAFETY: the master descriptor is live and `size` matches the native
        // `struct winsize` consumed by TIOCSWINSZ.
        let status = unsafe { ioctl(self.master.as_raw_fd(), TIOCSWINSZ, &size) };
        ensure!(
            status != -1,
            "failed to resize pseudoterminal: {}",
            std::io::Error::last_os_error()
        );
        Ok(())
    }

    pub(super) fn try_clone_reader(&self) -> Result<File> {
        self.master
            .try_clone()
            .context("failed to clone PTY reader")
    }

    pub(super) fn try_clone_writer(&self) -> Result<File> {
        self.master
            .try_clone()
            .context("failed to clone PTY writer")
    }

    pub(super) fn process_id(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.child
            .try_wait()
            .context("failed to query PTY child status")
    }

    pub(super) fn wait(&mut self) -> Result<ExitStatus> {
        self.child.wait().context("failed to wait for PTY child")
    }

    pub(super) fn terminate(&mut self) -> Result<()> {
        if self.try_wait()?.is_none() {
            let result = terminate_process_group(self.child.id());
            if let Err(error) = result
                && self.try_wait()?.is_none()
            {
                return Err(error);
            }
        }
        Ok(())
    }
}

impl Read for PlatformPtyProcess {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.master.read(bytes)
    }
}

impl Write for PlatformPtyProcess {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.master.write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.master.flush()
    }
}

impl Drop for PlatformPtyProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = terminate_process_group(self.child.id());
        }
        let _ = self.child.wait();
    }
}

fn set_close_on_exec(file: &File) -> Result<()> {
    // SAFETY: the descriptor is live, and F_GETFD has no pointer arguments.
    let flags = unsafe { fcntl(file.as_raw_fd(), F_GETFD) };
    ensure!(
        flags != -1,
        "failed to read descriptor flags: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the descriptor remains live and F_SETFD consumes this flag word.
    let status = unsafe { fcntl(file.as_raw_fd(), F_SETFD, flags | FD_CLOEXEC) };
    ensure!(
        status != -1,
        "failed to set descriptor flags: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn terminate_process_group(process_id: u32) -> Result<()> {
    let process_id = c_int::try_from(process_id).context("PTY process ID does not fit pid_t")?;
    // The child called setsid before exec, so its PID is also its process-group
    // ID. A negative PID addresses the complete group.
    // SAFETY: kill accepts a process-group ID and has no pointer arguments.
    let status = unsafe { kill_process(-process_id, SIGKILL) };
    ensure!(
        status != -1,
        "failed to terminate PTY process group: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn native_size(size: PtySize) -> WindowSize {
    WindowSize {
        rows: size.rows,
        columns: size.columns,
        x_pixels: 0,
        y_pixels: 0,
    }
}
