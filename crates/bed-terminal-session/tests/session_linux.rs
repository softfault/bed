//! Linux integration tests for the asynchronous terminal-session boundary.

#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use anyhow::{Result, bail, ensure};
use bed_pty::PtySize;
use bed_terminal::{Key, Modifiers, MouseAction, MouseButton, MouseEvent};
use bed_terminal_session::{TerminalSession, TerminalStore};
use std::{
    process::Command,
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn drives_modes_input_responses_eof_and_exit() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        concat!(
            "stty raw -echo; ",
            "printf '\\033[?1h\\033[?2004h\\033[2;3H\\033[6n'; ",
            "bytes=$(dd bs=1 count=28 2>/dev/null | od -An -tx1 | tr -d ' \\n'); ",
            "case \"$bytes\" in ",
            "1b5b323b33521b4f411b5b3230307e6f6e650a74776f1b5b3230317e) ",
            "printf '\\r\\nSESSION_OK\\r\\n' ;; ",
            "*) printf '\\r\\nSESSION_BAD:%s\\r\\n' \"$bytes\"; exit 1 ;; ",
            "esac"
        ),
    ]);
    let mut session = TerminalSession::spawn(command, PtySize::new(6, 30)?, 100)?;
    poll_until(&mut session, |session| {
        session.modes().application_cursor && session.modes().bracketed_paste
    })?;

    session.send_key(&Key::ArrowUp)?;
    session.send_key(&Key::Paste("one\ntwo".to_owned()))?;
    poll_until(&mut session, |session| {
        session.screen().contents().contains("SESSION_OK")
            && session.reached_eof()
            && session.status().is_some()
    })?;

    ensure!(
        session.screen().contents().contains("SESSION_OK"),
        "child rejected session input: {:?}",
        session.screen().contents()
    );
    ensure!(session.reached_eof(), "session did not observe PTY EOF");
    ensure!(session.status().is_some_and(|status| status.success()));
    Ok(())
}

#[test]
fn resizes_emulator_and_pty_together() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "stty raw -echo; dd bs=1 count=1 2>/dev/null; stty size",
    ]);
    let mut session = TerminalSession::spawn(command, PtySize::new(4, 20)?, 0)?;
    session.resize(PtySize::new(8, 40)?)?;
    ensure!(session.screen().size() == (8, 40));
    session.send_bytes(vec![b'x'])?;
    poll_until(&mut session, |session| {
        session.screen().contents().contains("8 40") && session.status().is_some()
    })?;
    ensure!(session.screen().contents().contains("8 40"));
    Ok(())
}

#[test]
fn store_keeps_stable_session_ids_and_protects_running_sessions() -> Result<()> {
    let mut first = Command::new("/bin/sleep");
    first.arg("30");
    let mut second = Command::new("/bin/sleep");
    second.arg("30");
    let mut store = TerminalStore::new();
    let first_id = store.spawn(first, PtySize::new(4, 20)?, 0)?;
    let second_id = store.spawn(second, PtySize::new(4, 20)?, 0)?;

    ensure!(first_id.get() == 0 && second_id.get() == 1);
    ensure!(store.ids().collect::<Vec<_>>() == [first_id, second_id]);
    ensure!(store.running_count()? == 2);
    ensure!(store.close(first_id, false).is_err());
    store.close(first_id, true)?;
    ensure!(store.get(first_id).is_none());
    ensure!(store.ids().collect::<Vec<_>>() == [second_id]);
    store.close(second_id, true)?;
    Ok(())
}

#[test]
fn finalizes_incomplete_utf8_at_eof() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf '\\303'"]);
    let mut session = TerminalSession::spawn(command, PtySize::new(2, 10)?, 0)?;

    poll_until(&mut session, |session| {
        session.reached_eof() && session.status().is_some()
    })?;

    ensure!(session.screen().contents().contains('\u{fffd}'));
    Ok(())
}

#[test]
fn reports_bells_from_each_poll() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf '\\007\\007ready'"]);
    let mut session = TerminalSession::spawn(command, PtySize::new(2, 10)?, 0)?;
    let mut bells = 0;

    poll_until_with(&mut session, |session, result| {
        bells += result.bells;
        session.reached_eof() && session.status().is_some()
    })?;

    ensure!(bells == 2, "expected two bells, observed {bells}");
    Ok(())
}

#[test]
fn reports_visual_bells_from_each_poll() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf '\\033g\\033gready'"]);
    let mut session = TerminalSession::spawn(command, PtySize::new(2, 10)?, 0)?;
    let mut visual_bells = 0;

    poll_until_with(&mut session, |session, result| {
        visual_bells += result.visual_bells;
        session.reached_eof() && session.status().is_some()
    })?;

    ensure!(
        visual_bells == 2,
        "expected two visual bells, observed {visual_bells}"
    );
    Ok(())
}

#[test]
fn routes_mode_aware_mouse_input() -> Result<()> {
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        concat!(
            "stty raw -echo; ",
            "printf '\\033[?1002h\\033[?1006hMOUSE_READY'; ",
            "bytes=$(dd bs=1 count=9 2>/dev/null | od -An -tx1 | tr -d ' \\n'); ",
            "[ \"$bytes\" = 1b5b3c303b353b334d ] && printf '\\r\\nMOUSE_OK\\r\\n'"
        ),
    ]);
    let mut session = TerminalSession::spawn(command, PtySize::new(4, 20)?, 0)?;
    poll_until(&mut session, |session| {
        session.modes().mouse_tracking == Some(1002) && session.modes().sgr_mouse
    })?;

    session.send_mouse(MouseEvent {
        row: 2,
        column: 4,
        action: MouseAction::Press(MouseButton::Left),
        modifiers: Modifiers::default(),
    })?;
    poll_until(&mut session, |session| {
        session.screen().contents().contains("MOUSE_OK") && session.status().is_some()
    })?;

    ensure!(session.screen().contents().contains("MOUSE_OK"));
    Ok(())
}

#[test]
fn readline_redraws_unicode_history_without_corrupting_the_screen() -> Result<()> {
    let mut command = Command::new("/bin/bash");
    command
        .args(["--noprofile", "--norc", "-i"])
        .env("PS1", "BED> ");
    let mut session = TerminalSession::spawn(command, PtySize::new(8, 100)?, 0)?;
    poll_until(&mut session, |session| {
        session.screen().contents().contains("BED>")
    })?;

    let command = "printf '中文 👩🏽‍💻 combining: e\\u0301\\n'";
    session.send_bytes(format!("{command}\n").into_bytes())?;
    poll_until(&mut session, |session| {
        session.screen().contents().matches("BED>").count() >= 2
            && session.screen().contents().contains("combining: é")
    })?;

    session.send_key(&Key::ArrowUp)?;
    let recalled_line = format!("BED> {command}");
    let recalled_column = unicode_width::UnicodeWidthStr::width(recalled_line.as_str());
    poll_until(&mut session, |session| {
        session
            .screen()
            .rows()
            .iter()
            .any(|row| row.text() == recalled_line)
            && session.screen().cursor().column == recalled_column
    })?;
    let recalled_cursor = session.screen().cursor();
    for _ in 0..24 {
        session.send_key(&Key::ArrowLeft)?;
        poll_until_with(&mut session, |_, result| result.output_bytes > 0)?;
    }
    for _ in 0..24 {
        session.send_key(&Key::ArrowRight)?;
        poll_until_with(&mut session, |_, result| result.output_bytes > 0)?;
    }
    ensure!(
        session.screen().cursor() == recalled_cursor,
        "readline cursor did not return after symmetric Unicode movement: start={recalled_cursor:?}, end={:?}",
        session.screen().cursor()
    );
    session.send_key(&Key::ArrowDown)?;
    poll_until_with(&mut session, |_, result| result.output_bytes > 0)?;
    poll_until(&mut session, |session| {
        session
            .screen()
            .rows()
            .iter()
            .any(|row| row.text() == "BED>")
    })?;

    for row in session.screen().rows() {
        for (column, cell) in row.cells().iter().enumerate() {
            if cell.is_continuation() {
                ensure!(column > 0, "continuation at the start of a row");
                ensure!(
                    unicode_width::UnicodeWidthStr::width(row.cells()[column - 1].contents()) == 2,
                    "orphan continuation in row {:?}",
                    row.text()
                );
            }
        }
    }
    ensure!(
        session
            .screen()
            .rows()
            .iter()
            .any(|row| row.text() == "BED>"),
        "readline left stale cells after clearing history: {:?}",
        session.screen().contents()
    );
    session.send_bytes(b"exit\n".to_vec())?;
    poll_until(&mut session, |session| {
        session.reached_eof() && session.status().is_some()
    })?;
    Ok(())
}

fn poll_until(
    session: &mut TerminalSession,
    mut ready: impl FnMut(&TerminalSession) -> bool,
) -> Result<()> {
    poll_until_with(session, |session, _| ready(session))
}

fn poll_until_with(
    session: &mut TerminalSession,
    mut ready: impl FnMut(&TerminalSession, bed_terminal_session::PollResult) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let result = session.poll()?;
        if ready(session, result) {
            return Ok(());
        }
        if let Some(error) = session.error() {
            bail!("terminal session failed: {error}");
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for terminal session; screen={:?}, status={:?}, eof={}",
                session.screen().contents(),
                session.status(),
                session.reached_eof()
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}
