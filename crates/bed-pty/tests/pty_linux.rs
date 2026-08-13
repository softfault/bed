//! Linux integration tests against a real pseudoterminal.

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use anyhow::{Context, Result, bail, ensure};
use bed_pty::{PtyProcess, PtySize};
use bed_terminal::{Key, encode_child_key};
use bed_vt100::TerminalEmulator;
use std::{
    io::{Read, Write},
    process::Command,
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn exchanges_data_and_resizes_a_real_pty() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "stty size; printf 'TERM:%s READY' \"$TERM\"; IFS= read -r line; printf ':%s:' \"$line\"; stty size",
    ]);
    let mut pty = PtyProcess::spawn(&mut command, PtySize::new(24, 80)?)?;
    ensure!(pty.process_id() > 0);

    let output = spawn_reader(pty.try_clone_reader()?);
    let mut bytes = Vec::new();
    read_until(&output, &mut bytes, b"READY")?;
    ensure!(
        String::from_utf8_lossy(&bytes).contains("24 80"),
        "initial PTY size was not reported: {:?}",
        String::from_utf8_lossy(&bytes)
    );
    ensure!(
        String::from_utf8_lossy(&bytes).contains("TERM:xterm-256color"),
        "default terminal type was not provided: {:?}",
        String::from_utf8_lossy(&bytes)
    );

    pty.resize(PtySize::new(40, 100)?)?;
    pty.write_all(b"hello\n")?;
    pty.flush()?;
    read_until(&output, &mut bytes, b"40 100")?;

    let text = String::from_utf8_lossy(&bytes);
    ensure!(text.contains(":hello:"), "PTY input was not read: {text:?}");
    ensure!(
        text.contains("40 100"),
        "resized PTY was not reported: {text:?}"
    );
    ensure!(pty.wait()?.success(), "PTY child did not exit successfully");
    Ok(())
}

#[test]
fn preserves_an_explicit_terminal_type() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "printf 'TERM:%s' \"$TERM\""])
        .env("TERM", "bed-test-terminal");
    let mut pty = PtyProcess::spawn(&mut command, PtySize::new(4, 40)?)?;
    let output = spawn_reader(pty.try_clone_reader()?);
    let mut bytes = Vec::new();

    read_until(&output, &mut bytes, b"TERM:bed-test-terminal")?;

    ensure!(pty.wait()?.success(), "TERM-check child failed");
    Ok(())
}

#[test]
fn routes_terminal_responses_through_a_real_pty() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        concat!(
            "printf '\\033[2;3H\\033[6n'; ",
            "IFS= read -r response; ",
            "case \"$response\" in ",
            "$(printf '\\033[2;3R')) printf '\\r\\nRESPONSE_OK\\r\\n' ;; ",
            "*) printf '\\r\\nRESPONSE_BAD:%s\\r\\n' \"$response\"; exit 1 ;; ",
            "esac"
        ),
    ]);
    let mut pty = PtyProcess::spawn(&mut command, PtySize::new(6, 20)?)?;
    let output = spawn_reader(pty.try_clone_reader()?);
    let mut terminal = TerminalEmulator::new(6, 20, 20);
    let mut raw = Vec::new();

    let responses = read_until_response(&output, &mut raw, &mut terminal)?;
    ensure!(!responses.is_empty(), "terminal generated no DSR response");
    pty.write_all(&responses)?;
    pty.write_all(b"\n")?;
    pty.flush()?;

    read_and_process_until(&output, &mut raw, &mut terminal, |terminal| {
        terminal.screen().contents().contains("RESPONSE_OK")
    })?;
    terminal.finish();
    ensure!(
        terminal.screen().contents().contains("RESPONSE_OK"),
        "child rejected terminal response: {:?}",
        terminal.screen().contents()
    );
    ensure!(pty.wait()?.success(), "response-check child failed");
    Ok(())
}

#[test]
fn encodes_mode_dependent_input_for_a_real_pty() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        concat!(
            "stty raw -echo; ",
            "bytes=$(dd bs=1 count=22 2>/dev/null | od -An -tx1 | tr -d ' \\n'); ",
            "case \"$bytes\" in ",
            "1b4f411b5b3230307e6f6e650a74776f1b5b3230317e) ",
            "printf '\\r\\nINPUT_OK\\r\\n' ;; ",
            "*) printf '\\r\\nINPUT_BAD:%s\\r\\n' \"$bytes\"; exit 1 ;; ",
            "esac"
        ),
    ]);
    let mut pty = PtyProcess::spawn(&mut command, PtySize::new(6, 30)?)?;
    let output = spawn_reader(pty.try_clone_reader()?);
    let mut terminal = TerminalEmulator::new(6, 30, 0);
    terminal.process(b"\x1b[?1h\x1b[?2004h");

    pty.write_all(&encode_child_key(&Key::ArrowUp, terminal.modes()))?;
    pty.write_all(&encode_child_key(
        &Key::Paste("one\ntwo".to_owned()),
        terminal.modes(),
    ))?;
    pty.flush()?;

    let mut raw = Vec::new();
    read_and_process_until(&output, &mut raw, &mut terminal, |terminal| {
        terminal.screen().contents().contains("INPUT_OK")
    })?;
    ensure!(
        terminal.screen().contents().contains("INPUT_OK"),
        "child rejected encoded input: {:?}",
        String::from_utf8_lossy(&raw)
    );
    ensure!(pty.wait()?.success(), "input-check child failed");
    Ok(())
}

#[test]
fn dropping_a_running_pty_terminates_and_reaps_the_child() -> Result<()> {
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    let pty = PtyProcess::spawn(&mut command, PtySize::new(24, 80)?)?;

    let started = Instant::now();
    drop(pty);
    ensure!(
        started.elapsed() < Duration::from_secs(2),
        "dropping a running PTY took too long"
    );
    Ok(())
}

#[test]
fn explicitly_terminates_a_running_pty_child() -> Result<()> {
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    let mut pty = PtyProcess::spawn(&mut command, PtySize::new(24, 80)?)?;

    ensure!(pty.try_wait()?.is_none());
    pty.terminate()?;
    ensure!(
        !pty.wait()?.success(),
        "terminated PTY child reported success"
    );
    Ok(())
}

#[test]
fn terminating_a_pty_kills_the_child_process_group() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "sleep 30 & child=$!; printf 'CHILD:%s\\n' \"$child\"; wait",
    ]);
    let mut pty = PtyProcess::spawn(&mut command, PtySize::new(24, 80)?)?;
    let output = spawn_reader(pty.try_clone_reader()?);
    let mut bytes = Vec::new();
    read_until(&output, &mut bytes, b"\n")?;
    let text = String::from_utf8_lossy(&bytes);
    let child = text
        .split("CHILD:")
        .nth(1)
        .and_then(|value| value.lines().next())
        .and_then(|value| value.trim().parse::<u32>().ok())
        .context("PTY shell did not report its background child PID")?;

    pty.terminate()?;
    let _ = pty.wait()?;
    let child_path = std::path::PathBuf::from(format!("/proc/{child}"));
    let deadline = Instant::now() + Duration::from_secs(2);
    while child_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if child_path.exists() {
        let _ = Command::new("kill")
            .args(["-9", &child.to_string()])
            .status();
        bail!("PTY background child {child} survived process-group termination");
    }
    Ok(())
}

fn spawn_reader(mut reader: std::fs::File) -> Receiver<Result<Vec<u8>, std::io::Error>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut buffer = [0; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if sender.send(Ok(buffer[..read].to_vec())).is_err() {
                        break;
                    }
                }
                // Linux PTY masters report EIO after the final slave closes.
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    receiver
}

fn read_until(
    receiver: &Receiver<Result<Vec<u8>, std::io::Error>>,
    output: &mut Vec<u8>,
    marker: &[u8],
) -> Result<()> {
    let deadline = Instant::now() + READ_TIMEOUT;
    while !output.windows(marker.len()).any(|window| window == marker) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "timed out waiting for {:?}; received {:?}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(output)
            );
        }
        match receiver.recv_timeout(remaining) {
            Ok(Ok(bytes)) => output.extend(bytes),
            Ok(Err(error)) => return Err(error).context("failed to read PTY output"),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!(
                "PTY output ended before {:?}; received {:?}",
                String::from_utf8_lossy(marker),
                String::from_utf8_lossy(output)
            ),
        }
    }
    Ok(())
}

fn read_and_process_until(
    receiver: &Receiver<Result<Vec<u8>, std::io::Error>>,
    output: &mut Vec<u8>,
    terminal: &mut TerminalEmulator,
    mut ready: impl FnMut(&mut TerminalEmulator) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + READ_TIMEOUT;
    while !ready(terminal) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "timed out waiting for terminal state; received {:?}",
                String::from_utf8_lossy(output)
            );
        }
        match receiver.recv_timeout(remaining) {
            Ok(Ok(bytes)) => {
                terminal.process(&bytes);
                output.extend(bytes);
            }
            Ok(Err(error)) => return Err(error).context("failed to read PTY output"),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("PTY output ended before terminal state was reached")
            }
        }
    }
    Ok(())
}

fn read_until_response(
    receiver: &Receiver<Result<Vec<u8>, std::io::Error>>,
    output: &mut Vec<u8>,
    terminal: &mut TerminalEmulator,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "timed out waiting for terminal response; received {:?}",
                String::from_utf8_lossy(output)
            );
        }
        match receiver.recv_timeout(remaining) {
            Ok(Ok(bytes)) => {
                terminal.process(&bytes);
                output.extend(bytes);
                let responses = terminal.take_responses();
                if !responses.is_empty() {
                    return Ok(responses);
                }
            }
            Ok(Err(error)) => return Err(error).context("failed to read PTY output"),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("PTY output ended before a terminal response was generated")
            }
        }
    }
}
