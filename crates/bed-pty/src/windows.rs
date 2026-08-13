//! Windows ConPTY process backend.

use super::PtySize;
use anyhow::{Context, Result, bail, ensure};
use std::{
    cmp::Ordering,
    ffi::{OsStr, OsString, c_void},
    fs::File,
    io::{Read, Write},
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, RawHandle},
        process::ExitStatusExt,
    },
    process::{Command, ExitStatus},
    ptr,
};

type Bool = i32;
type Dword = u32;
type Handle = *mut c_void;
type HResult = i32;
type SizeT = usize;
type Word = u16;

const EXTENDED_STARTUPINFO_PRESENT: Dword = 0x0008_0000;
const CREATE_UNICODE_ENVIRONMENT: Dword = 0x0000_0400;
const CREATE_SUSPENDED: Dword = 0x0000_0004;
const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;
const WAIT_OBJECT_0: Dword = 0;
const WAIT_TIMEOUT: Dword = 258;
const WAIT_FAILED: Dword = Dword::MAX;
const INFINITE: Dword = Dword::MAX;
const RESUME_FAILED: Dword = Dword::MAX;

#[repr(C)]
#[derive(Clone, Copy)]
struct Coord {
    x: i16,
    y: i16,
}

#[repr(C)]
struct StartupInfoW {
    size: Dword,
    reserved: *mut u16,
    desktop: *mut u16,
    title: *mut u16,
    x: Dword,
    y: Dword,
    x_size: Dword,
    y_size: Dword,
    x_count_chars: Dword,
    y_count_chars: Dword,
    fill_attribute: Dword,
    flags: Dword,
    show_window: Word,
    reserved_size: Word,
    reserved_bytes: *mut u8,
    standard_input: Handle,
    standard_output: Handle,
    standard_error: Handle,
}

impl StartupInfoW {
    fn new() -> Self {
        Self {
            size: size_of::<StartupInfoExW>() as Dword,
            reserved: ptr::null_mut(),
            desktop: ptr::null_mut(),
            title: ptr::null_mut(),
            x: 0,
            y: 0,
            x_size: 0,
            y_size: 0,
            x_count_chars: 0,
            y_count_chars: 0,
            fill_attribute: 0,
            flags: 0,
            show_window: 0,
            reserved_size: 0,
            reserved_bytes: ptr::null_mut(),
            standard_input: ptr::null_mut(),
            standard_output: ptr::null_mut(),
            standard_error: ptr::null_mut(),
        }
    }
}

#[repr(C)]
struct StartupInfoExW {
    startup_info: StartupInfoW,
    attribute_list: *mut c_void,
}

#[repr(C)]
struct ProcessInformation {
    process: Handle,
    thread: Handle,
    process_id: Dword,
    thread_id: Dword,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "CreatePipe"]
    fn create_pipe(
        read_pipe: *mut Handle,
        write_pipe: *mut Handle,
        pipe_attributes: *const c_void,
        size: Dword,
    ) -> Bool;
    #[link_name = "CreatePseudoConsole"]
    fn create_pseudo_console(
        size: Coord,
        input: Handle,
        output: Handle,
        flags: Dword,
        pseudo_console: *mut Handle,
    ) -> HResult;
    #[link_name = "ResizePseudoConsole"]
    fn resize_pseudo_console(pseudo_console: Handle, size: Coord) -> HResult;
    #[link_name = "ClosePseudoConsole"]
    fn close_pseudo_console(pseudo_console: Handle);
    #[link_name = "InitializeProcThreadAttributeList"]
    fn initialize_proc_thread_attribute_list(
        attribute_list: *mut c_void,
        attribute_count: Dword,
        flags: Dword,
        bytes: *mut SizeT,
    ) -> Bool;
    #[link_name = "UpdateProcThreadAttribute"]
    fn update_proc_thread_attribute(
        attribute_list: *mut c_void,
        flags: Dword,
        attribute: usize,
        value: *mut c_void,
        size: SizeT,
        previous_value: *mut c_void,
        return_size: *mut SizeT,
    ) -> Bool;
    #[link_name = "DeleteProcThreadAttributeList"]
    fn delete_proc_thread_attribute_list(attribute_list: *mut c_void);
    #[link_name = "CreateProcessW"]
    fn create_process(
        application_name: *const u16,
        command_line: *mut u16,
        process_attributes: *const c_void,
        thread_attributes: *const c_void,
        inherit_handles: Bool,
        creation_flags: Dword,
        environment: *mut c_void,
        current_directory: *const u16,
        startup_info: *const StartupInfoW,
        process_information: *mut ProcessInformation,
    ) -> Bool;
    #[link_name = "CloseHandle"]
    fn close_handle(handle: Handle) -> Bool;
    #[link_name = "WaitForSingleObject"]
    fn wait_for_single_object(handle: Handle, milliseconds: Dword) -> Dword;
    #[link_name = "GetExitCodeProcess"]
    fn get_exit_code_process(process: Handle, exit_code: *mut Dword) -> Bool;
    #[link_name = "TerminateProcess"]
    fn terminate_process(process: Handle, exit_code: Dword) -> Bool;
    #[link_name = "CreateJobObjectW"]
    fn create_job_object(attributes: *const c_void, name: *const u16) -> Handle;
    #[link_name = "AssignProcessToJobObject"]
    fn assign_process_to_job_object(job: Handle, process: Handle) -> Bool;
    #[link_name = "TerminateJobObject"]
    fn terminate_job_object(job: Handle, exit_code: Dword) -> Bool;
    #[link_name = "ResumeThread"]
    fn resume_thread(thread: Handle) -> Dword;
}

pub(super) struct PlatformPtyProcess {
    input: Option<File>,
    output: Option<File>,
    pseudo_console: Handle,
    process: Handle,
    job: Handle,
    process_id: u32,
}

impl PlatformPtyProcess {
    pub(super) fn spawn(command: &mut Command, size: PtySize) -> Result<Self> {
        let (pseudo_input, parent_input) = create_anonymous_pipe("ConPTY input")?;
        let (parent_output, pseudo_output) = create_anonymous_pipe("ConPTY output")?;

        let mut pseudo_console = ptr::null_mut();
        // SAFETY: both supplied handles are live ends of anonymous pipes, the
        // output pointer is writable, and the validated dimensions fit COORD.
        let result = unsafe {
            create_pseudo_console(
                native_size(size),
                pseudo_input.raw(),
                pseudo_output.raw(),
                0,
                &mut pseudo_console,
            )
        };
        ensure!(
            result >= 0,
            "failed to create pseudoconsole (HRESULT 0x{:08x})",
            result as u32
        );
        ensure!(
            !pseudo_console.is_null(),
            "CreatePseudoConsole returned no handle"
        );
        let pseudo_console = PseudoConsole::new(pseudo_console);

        // ConPTY retained its pipe ends. The application owns only the
        // opposite ends used to send input and receive output.
        drop(pseudo_input);
        drop(pseudo_output);
        let input = parent_input.into_file();
        let output = parent_output.into_file();

        if !command.get_envs().any(|(name, _)| name == "TERM") {
            command.env("TERM", "xterm-256color");
        }
        let application_name = wide_string(command.get_program(), "program")?;
        let mut command_line = command_line(command)?;
        let mut environment = environment_block(command)?;
        let current_directory = command
            .get_current_dir()
            .map(|directory| wide_string(directory.as_os_str(), "current directory"))
            .transpose()?;

        let mut attributes = AttributeList::new(pseudo_console.raw())?;
        let startup = StartupInfoExW {
            startup_info: StartupInfoW::new(),
            attribute_list: attributes.raw(),
        };
        let mut process_information = ProcessInformation {
            process: ptr::null_mut(),
            thread: ptr::null_mut(),
            process_id: 0,
            thread_id: 0,
        };
        // SAFETY: null attributes and name request an unnamed job with default
        // security settings.
        let job = unsafe { create_job_object(ptr::null(), ptr::null()) };
        let job = OwnedHandle::new(job).context("failed to create ConPTY job object")?;
        // SAFETY: all UTF-16 buffers are NUL-terminated and live for the call;
        // the mutable command/environment buffers meet CreateProcessW's ABI;
        // startup and process-information structures have asserted layouts.
        let success = unsafe {
            create_process(
                application_name.as_ptr(),
                command_line.as_mut_ptr(),
                ptr::null(),
                ptr::null(),
                0,
                EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED,
                environment.as_mut_ptr().cast(),
                current_directory
                    .as_ref()
                    .map_or(ptr::null(), |directory| directory.as_ptr()),
                &startup.startup_info,
                &mut process_information,
            )
        };
        ensure!(
            success != 0,
            "failed to spawn {:?} in ConPTY: {}",
            command.get_program(),
            std::io::Error::last_os_error()
        );

        let process = OwnedHandle::new(process_information.process)
            .context("CreateProcessW returned no process handle")?;
        let Some(thread) = OwnedHandle::new(process_information.thread) else {
            // SAFETY: the newly created process is suspended and uniquely owned.
            let _ = unsafe { terminate_process(process.raw(), 1) };
            // SAFETY: the process handle remains live for this cleanup wait.
            let _ = unsafe { wait_for_single_object(process.raw(), INFINITE) };
            bail!("CreateProcessW returned no thread handle");
        };
        // SAFETY: both handles are live and the process remains suspended, so
        // no descendant can escape before assignment.
        if unsafe { assign_process_to_job_object(job.raw(), process.raw()) } == 0 {
            let error = std::io::Error::last_os_error();
            // SAFETY: the suspended process is uniquely owned here.
            let _ = unsafe { terminate_process(process.raw(), 1) };
            // SAFETY: the process handle remains live for this cleanup wait.
            let _ = unsafe { wait_for_single_object(process.raw(), INFINITE) };
            return Err(error).context("failed to assign ConPTY process to job object");
        }
        // SAFETY: the primary thread is suspended and uniquely owned here.
        if unsafe { resume_thread(thread.raw()) } == RESUME_FAILED {
            let error = std::io::Error::last_os_error();
            // SAFETY: the job contains the suspended root process.
            let _ = unsafe { terminate_job_object(job.raw(), 1) };
            // SAFETY: the process handle remains live for this cleanup wait.
            let _ = unsafe { wait_for_single_object(process.raw(), INFINITE) };
            return Err(error).context("failed to resume ConPTY process");
        }
        drop(thread);
        drop(attributes);

        Ok(Self {
            input: Some(input),
            output: Some(output),
            pseudo_console: pseudo_console.into_raw(),
            process: process.into_raw(),
            job: job.into_raw(),
            process_id: process_information.process_id,
        })
    }

    pub(super) fn resize(&mut self, size: PtySize) -> Result<()> {
        // SAFETY: the pseudoconsole handle remains owned by self, and the
        // validated dimensions fit the native COORD fields.
        let result = unsafe { resize_pseudo_console(self.pseudo_console, native_size(size)) };
        ensure!(
            result >= 0,
            "failed to resize pseudoconsole (HRESULT 0x{:08x})",
            result as u32
        );
        Ok(())
    }

    pub(super) fn try_clone_reader(&self) -> Result<File> {
        self.output
            .as_ref()
            .context("ConPTY output is closed")?
            .try_clone()
            .context("failed to clone ConPTY reader")
    }

    pub(super) fn try_clone_writer(&self) -> Result<File> {
        self.input
            .as_ref()
            .context("ConPTY input is closed")?
            .try_clone()
            .context("failed to clone ConPTY writer")
    }

    pub(super) fn process_id(&self) -> u32 {
        self.process_id
    }

    pub(super) fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        // SAFETY: the process handle remains live until Drop closes it.
        match unsafe { wait_for_single_object(self.process, 0) } {
            WAIT_OBJECT_0 => self.exit_status().map(Some),
            WAIT_TIMEOUT => Ok(None),
            WAIT_FAILED => bail!(
                "failed to query ConPTY child status: {}",
                std::io::Error::last_os_error()
            ),
            status => bail!("unexpected process wait status {status}"),
        }
    }

    pub(super) fn wait(&mut self) -> Result<ExitStatus> {
        // SAFETY: the process handle remains live until Drop closes it.
        let status = unsafe { wait_for_single_object(self.process, INFINITE) };
        ensure!(
            status == WAIT_OBJECT_0,
            "failed to wait for ConPTY child: {}",
            std::io::Error::last_os_error()
        );
        self.exit_status()
    }

    pub(super) fn terminate(&mut self) -> Result<()> {
        if self.try_wait()?.is_none() {
            // SAFETY: the job remains live and owns the complete ConPTY process
            // tree. Exit code 1 denotes forced termination.
            let success = unsafe { terminate_job_object(self.job, 1) };
            ensure!(
                success != 0,
                "failed to terminate ConPTY process tree: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(())
    }

    fn exit_status(&self) -> Result<ExitStatus> {
        let mut code = 0;
        // SAFETY: the process handle is live and code points to writable data.
        let success = unsafe { get_exit_code_process(self.process, &mut code) };
        ensure!(
            success != 0,
            "failed to read ConPTY child exit status: {}",
            std::io::Error::last_os_error()
        );
        Ok(ExitStatus::from_raw(code))
    }
}

impl Read for PlatformPtyProcess {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.output
            .as_mut()
            .expect("ConPTY output is live before Drop")
            .read(bytes)
    }
}

impl Write for PlatformPtyProcess {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.input
            .as_mut()
            .expect("ConPTY input is live before Drop")
            .write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.input
            .as_mut()
            .expect("ConPTY input is live before Drop")
            .flush()
    }
}

impl Drop for PlatformPtyProcess {
    fn drop(&mut self) {
        // The root process may have exited while a descendant remains alive.
        // SAFETY: self still owns the job handle during teardown.
        let _ = unsafe { terminate_job_object(self.job, 1) };
        // SAFETY: self still owns the process handle during teardown.
        let _ = unsafe { wait_for_single_object(self.process, INFINITE) };

        // Closing the application pipe ends first prevents synchronous ConPTY
        // teardown from waiting for unread output.
        self.input.take();
        self.output.take();
        // SAFETY: all handles are live and uniquely owned by self.
        unsafe {
            close_pseudo_console(self.pseudo_console);
            let _ = close_handle(self.process);
            let _ = close_handle(self.job);
        }
    }
}

struct OwnedHandle(Handle);

impl OwnedHandle {
    fn new(handle: Handle) -> Option<Self> {
        (!handle.is_null()).then_some(Self(handle))
    }

    fn raw(&self) -> Handle {
        self.0
    }

    fn into_raw(mut self) -> Handle {
        let handle = self.0;
        self.0 = ptr::null_mut();
        handle
    }

    fn into_file(self) -> File {
        let handle = self.into_raw();
        // SAFETY: ownership of the live Win32 handle is transferred once into
        // File, whose Drop will close it.
        unsafe { File::from_raw_handle(handle as RawHandle) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a non-null value remains uniquely owned by this guard.
            let _ = unsafe { close_handle(self.0) };
        }
    }
}

struct PseudoConsole(Handle);

impl PseudoConsole {
    fn new(handle: Handle) -> Self {
        Self(handle)
    }

    fn raw(&self) -> Handle {
        self.0
    }

    fn into_raw(mut self) -> Handle {
        let handle = self.0;
        self.0 = ptr::null_mut();
        handle
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: a non-null value remains uniquely owned by this guard.
            unsafe { close_pseudo_console(self.0) };
        }
    }
}

struct AttributeList {
    storage: Vec<usize>,
    initialized: bool,
}

impl AttributeList {
    fn new(pseudo_console: Handle) -> Result<Self> {
        let mut bytes = 0;
        // SAFETY: a null list is the documented sizing call and bytes is
        // writable. Its expected false return is intentionally ignored.
        unsafe {
            initialize_proc_thread_attribute_list(ptr::null_mut(), 1, 0, &mut bytes);
        }
        ensure!(bytes > 0, "failed to size process attribute list");
        let words = bytes.div_ceil(size_of::<usize>());
        let mut list = Self {
            storage: vec![0; words],
            initialized: false,
        };
        // SAFETY: storage is aligned for pointers and contains at least the
        // byte count requested by the Win32 sizing call.
        let success =
            unsafe { initialize_proc_thread_attribute_list(list.raw(), 1, 0, &mut bytes) };
        ensure!(
            success != 0,
            "failed to initialize process attribute list: {}",
            std::io::Error::last_os_error()
        );
        list.initialized = true;
        // SAFETY: the initialized list is writable. For this attribute Win32
        // requires the HPCON value itself as lpValue, not a pointer to HPCON.
        let success = unsafe {
            update_proc_thread_attribute(
                list.raw(),
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE,
                pseudo_console,
                size_of::<Handle>(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        ensure!(
            success != 0,
            "failed to attach pseudoconsole process attribute: {}",
            std::io::Error::last_os_error()
        );
        Ok(list)
    }

    fn raw(&mut self) -> *mut c_void {
        self.storage.as_mut_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: storage contains an initialized attribute list and stays
            // live until this deletion call returns.
            unsafe { delete_proc_thread_attribute_list(self.raw()) };
        }
    }
}

fn create_anonymous_pipe(name: &str) -> Result<(OwnedHandle, OwnedHandle)> {
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    // SAFETY: both output pointers are writable; null security attributes
    // select non-inheritable handles and size zero selects the system default.
    let success = unsafe { create_pipe(&mut read, &mut write, ptr::null(), 0) };
    ensure!(
        success != 0,
        "failed to create {name} pipe: {}",
        std::io::Error::last_os_error()
    );
    let read = OwnedHandle::new(read).context("CreatePipe returned no read handle")?;
    let write = OwnedHandle::new(write).context("CreatePipe returned no write handle")?;
    Ok((read, write))
}

fn native_size(size: PtySize) -> Coord {
    Coord {
        x: size.columns as i16,
        y: size.rows as i16,
    }
}

fn wide_string(value: &OsStr, name: &str) -> Result<Vec<u16>> {
    let mut wide: Vec<_> = value.encode_wide().collect();
    ensure!(!wide.contains(&0), "{name} contains a NUL character");
    wide.push(0);
    Ok(wide)
}

fn command_line(command: &Command) -> Result<Vec<u16>> {
    let mut line = Vec::new();
    push_quoted_argument(&mut line, command.get_program())?;
    for argument in command.get_args() {
        line.push(u16::from(b' '));
        push_quoted_argument(&mut line, argument)?;
    }
    line.push(0);
    Ok(line)
}

fn push_quoted_argument(line: &mut Vec<u16>, argument: &OsStr) -> Result<()> {
    let argument: Vec<_> = argument.encode_wide().collect();
    ensure!(
        !argument.contains(&0),
        "process argument contains a NUL character"
    );
    line.push(u16::from(b'\"'));
    let mut backslashes = 0;
    for unit in argument {
        if unit == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        if unit == u16::from(b'\"') {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2 + 1));
        } else {
            line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes));
        }
        backslashes = 0;
        line.push(unit);
    }
    line.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    line.push(u16::from(b'\"'));
    Ok(())
}

fn environment_block(command: &Command) -> Result<Vec<u16>> {
    let mut variables: Vec<(OsString, OsString)> = std::env::vars_os().collect();
    for (name, value) in command.get_envs() {
        variables.retain(|(existing, _)| !environment_names_equal(existing, name));
        if let Some(value) = value {
            variables.push((name.to_owned(), value.to_owned()));
        }
    }

    let mut variables: Vec<(Vec<u16>, Vec<u16>)> = variables
        .into_iter()
        .map(|(name, value)| {
            let name: Vec<_> = name.encode_wide().collect();
            let value: Vec<_> = value.encode_wide().collect();
            ensure!(
                !name.is_empty() && !name.contains(&0) && !name.contains(&u16::from(b'=')),
                "invalid Windows environment variable name"
            );
            ensure!(
                !value.contains(&0),
                "Windows environment variable value contains a NUL character"
            );
            Ok((name, value))
        })
        .collect::<Result<_>>()?;
    variables.sort_by(|(left, _), (right, _)| compare_environment_names(left, right));

    let mut block = Vec::new();
    for (name, value) in variables {
        block.extend(name);
        block.push(u16::from(b'='));
        block.extend(value);
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

fn environment_names_equal(left: &OsStr, right: &OsStr) -> bool {
    left.as_encoded_bytes()
        .eq_ignore_ascii_case(right.as_encoded_bytes())
}

fn compare_environment_names(left: &[u16], right: &[u16]) -> Ordering {
    left.iter()
        .map(|unit| lowercase_ascii_unit(*unit))
        .cmp(right.iter().map(|unit| lowercase_ascii_unit(*unit)))
}

fn lowercase_ascii_unit(unit: u16) -> u16 {
    if unit >= u16::from(b'A') && unit <= u16::from(b'Z') {
        unit + u16::from(b'a' - b'A')
    } else {
        unit
    }
}

const _: () = {
    assert!(size_of::<Coord>() == 4);
    assert!(size_of::<StartupInfoW>() == 104);
    assert!(size_of::<StartupInfoExW>() == 112);
    assert!(size_of::<ProcessInformation>() == 24);
};

#[cfg(test)]
mod tests {
    use super::{command_line, compare_environment_names};
    use std::{cmp::Ordering, process::Command};

    #[test]
    fn quotes_windows_command_arguments() {
        let mut command = Command::new(r"C:\Program Files\tool.exe");
        command.args(["", "plain", "two words", r#"quote\"here"#, r"ends\\"]);
        let actual = String::from_utf16(&command_line(&command).unwrap()).unwrap();
        assert_eq!(
            actual,
            concat!(
                r#""C:\Program Files\tool.exe" "" "plain" "two words" "quote\\\"here" "ends\\\\""#,
                "\0"
            )
        );
    }

    #[test]
    fn compares_environment_names_without_ascii_case() {
        assert_eq!(
            compare_environment_names(
                &"Path".encode_utf16().collect::<Vec<_>>(),
                &"PATH".encode_utf16().collect::<Vec<_>>()
            ),
            Ordering::Equal
        );
    }
}
