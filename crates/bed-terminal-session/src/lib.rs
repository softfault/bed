//! Embedded terminal session lifecycle and event delivery for bed.
//!
//! Reader and writer threads move bytes across bounded queues. The caller that
//! owns [`TerminalStore`] remains the sole mutator of terminal-emulator state.

#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail};
use bed_pty::PtyProcess;
pub use bed_pty::PtySize;
use bed_terminal::{Key, MouseEvent, encode_child_key, encode_child_mouse};
use bed_vt100::{Screen, TerminalEmulator, TerminalModes};
use std::{
    collections::HashMap,
    io::{Read, Write},
    process::{Command, ExitStatus},
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
};

const DEFAULT_QUEUE_CAPACITY: usize = 256;
const READ_BUFFER_SIZE: usize = 8192;
const OUTPUT_EVENTS_PER_POLL: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TerminalSessionId(u64);

impl TerminalSessionId {
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PollResult {
    pub output_events: usize,
    pub output_bytes: usize,
    pub bells: usize,
    pub visual_bells: usize,
    pub reached_eof: bool,
    pub exited: bool,
}

#[derive(Debug)]
enum OutputEvent {
    Bytes(Vec<u8>),
    Eof,
    Error(String),
}

pub struct TerminalSession {
    process: PtyProcess,
    terminal: TerminalEmulator,
    output: Receiver<OutputEvent>,
    input: Option<SyncSender<Vec<u8>>>,
    status: Option<ExitStatus>,
    eof: bool,
    error: Option<String>,
    command: String,
    size: PtySize,
}

impl TerminalSession {
    pub fn spawn(command: Command, size: PtySize, scrollback_capacity: usize) -> Result<Self> {
        let label = command.get_program().to_string_lossy().into_owned();
        Self::spawn_labeled(
            command,
            label,
            size,
            scrollback_capacity,
            DEFAULT_QUEUE_CAPACITY,
        )
    }

    fn spawn_labeled(
        command: Command,
        label: String,
        size: PtySize,
        scrollback_capacity: usize,
        queue_capacity: usize,
    ) -> Result<Self> {
        anyhow::ensure!(
            queue_capacity > 0,
            "terminal queue capacity must be nonzero"
        );
        let process = PtyProcess::spawn(command, size)?;
        let reader = process.try_clone_reader()?;
        let writer = process.try_clone_writer()?;
        let (output_sender, output) = mpsc::sync_channel(queue_capacity);
        let (input, input_receiver) = mpsc::sync_channel(queue_capacity);
        spawn_output_thread(reader, output_sender.clone(), process.process_id())?;
        spawn_input_thread(writer, input_receiver, output_sender, process.process_id())?;
        Ok(Self {
            process,
            terminal: TerminalEmulator::new(
                usize::from(size.rows()),
                usize::from(size.columns()),
                scrollback_capacity,
            ),
            output,
            input: Some(input),
            status: None,
            eof: false,
            error: None,
            command: label,
            size,
        })
    }

    pub fn screen(&self) -> &Screen {
        self.terminal.screen()
    }

    pub fn modes(&self) -> TerminalModes {
        self.terminal.modes()
    }

    pub fn title(&self) -> &str {
        self.terminal.title()
    }

    pub fn status(&self) -> Option<ExitStatus> {
        self.status
    }

    pub fn reached_eof(&self) -> bool {
        self.eof
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn size(&self) -> PtySize {
        self.size
    }

    pub fn scrollback_len(&self) -> usize {
        self.terminal.primary_screen().scrollback().len()
    }

    pub fn history_rows_pushed(&self) -> u64 {
        self.terminal.primary_screen().history_rows_pushed()
    }

    pub fn history_rows_discarded(&self) -> u64 {
        self.terminal.primary_screen().history_rows_discarded()
    }

    pub fn reset_count(&self) -> u64 {
        self.terminal.reset_count()
    }

    pub fn poll(&mut self) -> Result<PollResult> {
        let mut result = PollResult::default();
        let bells_before = self.terminal.bell_count();
        let visual_bells_before = self.terminal.visual_bell_count();
        while result.output_events < OUTPUT_EVENTS_PER_POLL {
            match self.output.try_recv() {
                Ok(OutputEvent::Bytes(bytes)) => {
                    result.output_events += 1;
                    result.output_bytes += bytes.len();
                    self.terminal.process(&bytes);
                    let responses = self.terminal.take_responses();
                    if !responses.is_empty() && self.input.is_some() {
                        self.send_bytes(responses)?;
                    }
                }
                Ok(OutputEvent::Eof) => {
                    result.output_events += 1;
                    result.reached_eof = !self.eof;
                    if !self.eof {
                        self.terminal.finish();
                        self.eof = true;
                    }
                }
                Ok(OutputEvent::Error(error)) => {
                    result.output_events += 1;
                    self.error = Some(error);
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        if self.status.is_none()
            && let Some(status) = self.process.try_wait()?
        {
            self.status = Some(status);
            self.input.take();
            result.exited = true;
        }
        result.bells = self.terminal.bell_count().saturating_sub(bells_before);
        result.visual_bells = self
            .terminal
            .visual_bell_count()
            .saturating_sub(visual_bells_before);
        Ok(result)
    }

    pub fn send_key(&self, key: &Key) -> Result<()> {
        let bytes = encode_child_key(key, self.terminal.modes());
        if bytes.is_empty() {
            return Ok(());
        }
        self.send_bytes(bytes)
    }

    pub fn send_mouse(&self, event: MouseEvent) -> Result<()> {
        let bytes = encode_child_mouse(event, self.terminal.modes());
        if bytes.is_empty() {
            return Ok(());
        }
        self.send_bytes(bytes)
    }

    pub fn send_bytes(&self, bytes: Vec<u8>) -> Result<()> {
        let input = self
            .input
            .as_ref()
            .context("terminal session no longer accepts input")?;
        match input.try_send(bytes) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => bail!("terminal input queue is full"),
            Err(TrySendError::Disconnected(_)) => bail!("terminal input writer stopped"),
        }
    }

    pub fn resize(&mut self, size: PtySize) -> Result<()> {
        if self.size == size {
            return Ok(());
        }
        if self.status.is_none() {
            self.process.resize(size)?;
        }
        self.terminal
            .set_size(usize::from(size.rows()), usize::from(size.columns()));
        self.size = size;
        Ok(())
    }

    pub fn terminate(&mut self) -> Result<()> {
        self.process.terminate()
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.process.terminate();
    }
}

#[derive(Default)]
pub struct TerminalStore {
    sessions: HashMap<TerminalSessionId, TerminalSession>,
    next_id: u64,
}

impl std::fmt::Debug for TerminalStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalStore")
            .field("session_count", &self.sessions.len())
            .field("next_id", &self.next_id)
            .finish()
    }
}

impl TerminalStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(
        &mut self,
        command: Command,
        size: PtySize,
        scrollback_capacity: usize,
    ) -> Result<TerminalSessionId> {
        let session = TerminalSession::spawn(command, size, scrollback_capacity)?;
        self.insert(session)
    }

    pub fn spawn_shell(
        &mut self,
        command: Option<&str>,
        size: PtySize,
        scrollback_capacity: usize,
    ) -> Result<TerminalSessionId> {
        let (command, label) = shell_command(command);
        let session = TerminalSession::spawn_labeled(
            command,
            label,
            size,
            scrollback_capacity,
            DEFAULT_QUEUE_CAPACITY,
        )?;
        self.insert(session)
    }

    fn insert(&mut self, session: TerminalSession) -> Result<TerminalSessionId> {
        let id = TerminalSessionId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .context("terminal session ID space exhausted")?;
        self.sessions.insert(id, session);
        Ok(id)
    }

    pub fn get(&self, id: TerminalSessionId) -> Option<&TerminalSession> {
        self.sessions.get(&id)
    }

    pub fn get_mut(&mut self, id: TerminalSessionId) -> Option<&mut TerminalSession> {
        self.sessions.get_mut(&id)
    }

    pub fn ids(&self) -> impl Iterator<Item = TerminalSessionId> + '_ {
        let mut ids: Vec<_> = self.sessions.keys().copied().collect();
        ids.sort_unstable();
        ids.into_iter()
    }

    pub fn poll(&mut self) -> Result<Vec<(TerminalSessionId, PollResult)>> {
        let ids: Vec<_> = self.ids().collect();
        let mut activity = Vec::new();
        for id in ids {
            let result = self
                .sessions
                .get_mut(&id)
                .expect("collected terminal session ID remains present")
                .poll()?;
            if result != PollResult::default() {
                activity.push((id, result));
            }
        }
        Ok(activity)
    }

    pub fn running_count(&mut self) -> Result<usize> {
        self.poll()?;
        Ok(self
            .sessions
            .values()
            .filter(|session| session.status().is_none())
            .count())
    }

    pub fn close(&mut self, id: TerminalSessionId, force: bool) -> Result<()> {
        let session = self
            .sessions
            .get_mut(&id)
            .context("terminal session does not exist")?;
        session.poll()?;
        if session.status().is_none() {
            if !force {
                bail!("terminal session is still running");
            }
            session.terminate()?;
        }
        self.sessions.remove(&id);
        Ok(())
    }
}

fn spawn_output_thread(
    mut reader: std::fs::File,
    sender: SyncSender<OutputEvent>,
    process_id: u32,
) -> Result<()> {
    thread::Builder::new()
        .name(format!("bed-pty-output-{process_id}"))
        .spawn(move || {
            let mut buffer = vec![0; READ_BUFFER_SIZE];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if sender
                            .send(OutputEvent::Bytes(buffer[..read].to_vec()))
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) if pty_eof(&error) => break,
                    Err(error) => {
                        let _ = sender.send(OutputEvent::Error(format!(
                            "failed to read PTY output: {error}"
                        )));
                        break;
                    }
                }
            }
            let _ = sender.send(OutputEvent::Eof);
        })
        .context("failed to start PTY output thread")?;
    Ok(())
}

#[cfg(unix)]
fn shell_command(command: Option<&str>) -> (Command, String) {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut process = Command::new(&shell);
    let label = command.map_or_else(|| shell.to_string_lossy().into_owned(), str::to_owned);
    if let Some(command) = command {
        process.args(["-c", command]);
    }
    (process, label)
}

#[cfg(windows)]
fn shell_command(command: Option<&str>) -> (Command, String) {
    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let mut process = Command::new(&shell);
    let label = command.map_or_else(|| shell.to_string_lossy().into_owned(), str::to_owned);
    if let Some(command) = command {
        process.args(["/D", "/S", "/C", command]);
    }
    (process, label)
}

fn spawn_input_thread(
    mut writer: std::fs::File,
    receiver: Receiver<Vec<u8>>,
    errors: SyncSender<OutputEvent>,
    process_id: u32,
) -> Result<()> {
    thread::Builder::new()
        .name(format!("bed-pty-input-{process_id}"))
        .spawn(move || {
            for bytes in receiver {
                if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                    let _ = errors.send(OutputEvent::Error(format!(
                        "failed to write PTY input: {error}"
                    )));
                    break;
                }
            }
        })
        .context("failed to start PTY input thread")?;
    Ok(())
}

#[cfg(unix)]
fn pty_eof(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(5)
}

#[cfg(windows)]
fn pty_eof(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(109 | 232))
}

#[cfg(all(
    test,
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod tests {
    use super::TerminalSession;
    use bed_pty::PtySize;
    use std::{
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn reports_input_backpressure() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        let session = TerminalSession::spawn_labeled(
            command,
            "input-backpressure".to_owned(),
            PtySize::new(2, 10).unwrap(),
            0,
            1,
        )
        .unwrap();
        let payload = vec![b'x'; 1024 * 1024];

        let error = (0..16)
            .find_map(|_| session.send_bytes(payload.clone()).err())
            .expect("bounded input queue did not report backpressure");

        assert!(error.to_string().contains("input queue is full"));
    }

    #[test]
    fn drains_output_after_reader_backpressure() {
        const OUTPUT_BYTES: usize = 1024 * 1024;
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "stty raw -echo; dd if=/dev/zero bs=8192 count=128 2>/dev/null; printf DONE",
        ]);
        let mut session = TerminalSession::spawn_labeled(
            command,
            "output-backpressure".to_owned(),
            PtySize::new(2, 10).unwrap(),
            0,
            1,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(20));

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut received = 0;
        while !(session.reached_eof() && session.status().is_some()) {
            received += session.poll().unwrap().output_bytes;
            assert!(Instant::now() < deadline, "timed out draining PTY output");
            thread::yield_now();
        }

        assert_eq!(received, OUTPUT_BYTES + b"DONE".len());
        assert!(session.screen().contents().contains("DONE"));
    }
}
