//! Native terminal boundary for bed.
//!
//! Each supported operating system selects one handwritten backend at compile
//! time. Backends translate native input into [`Key`] and own terminal setup
//! and restoration; no platform ABI escapes this crate.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(clippy::undocumented_unsafe_blocks)]

use anyhow::Result;
use std::{
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread::{self, JoinHandle},
    time::Duration,
};

const EVENT_QUEUE_CAPACITY: usize = 256;
const BEGIN_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026h";
const END_SYNCHRONIZED_UPDATE: &[u8] = b"\x1b[?2026l";

mod child;
mod frame;

pub use child::{encode_child_key, encode_child_mouse};

#[cfg(all(
    any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "netbsd"
    ),
    any(
        all(any(target_os = "linux", target_os = "macos"), target_arch = "aarch64"),
        target_arch = "x86_64"
    )
))]
#[path = "terminal/vt.rs"]
mod vt;

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "terminal/linux.rs"]
mod platform;

#[cfg(all(
    target_os = "macos",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "terminal/macos.rs"]
mod platform;

#[cfg(all(target_os = "freebsd", target_arch = "x86_64"))]
#[path = "terminal/freebsd.rs"]
mod platform;

#[cfg(all(target_os = "netbsd", target_arch = "x86_64"))]
#[path = "terminal/netbsd.rs"]
mod platform;

#[cfg(windows)]
#[path = "terminal/windows.rs"]
mod platform;

#[cfg(not(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(
        target_os = "macos",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "freebsd", target_arch = "x86_64"),
    all(target_os = "netbsd", target_arch = "x86_64"),
    windows
)))]
compile_error!(
    "bed currently supports Linux and macOS on x86_64/aarch64, and Windows, FreeBSD, and NetBSD on x86_64"
);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press(MouseButton),
    Release(MouseButton),
    Drag(MouseButton),
    Move,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseEvent {
    pub row: usize,
    pub column: usize,
    pub action: MouseAction,
    pub modifiers: Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialKey {
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Paste(String),
    Tab,
    BackTab,
    Enter,
    Backspace,
    Delete,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    PageUp,
    PageDown,
    Ctrl(char),
    Modified(SpecialKey, Modifiers),
    Resize,
    Escape,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: usize,
    pub columns: usize,
}

pub struct Terminal {
    platform: platform::PlatformTerminal,
    frame_renderer: frame::FrameRenderer,
    input_claimed: bool,
}

/// Bounded host-terminal input delivery for an application event loop.
pub struct TerminalEvents {
    receiver: Receiver<Result<TerminalEvent>>,
    _thread: JoinHandle<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalEvent {
    Key(Key),
    Mouse(MouseEvent),
    Resize(TerminalSize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostInput {
    Key(Key),
    Mouse(MouseEvent),
}

impl Terminal {
    pub fn new() -> Result<Self> {
        Ok(Self {
            platform: platform::PlatformTerminal::new()?,
            frame_renderer: frame::FrameRenderer::default(),
            input_claimed: false,
        })
    }

    pub fn size(&self) -> Result<TerminalSize> {
        self.platform.size()
    }

    pub fn read_key(&mut self) -> Result<Key> {
        anyhow::ensure!(
            !self.input_claimed,
            "host terminal input is owned by the event thread"
        );
        self.platform.read_key()
    }

    pub fn events(&mut self) -> Result<TerminalEvents> {
        anyhow::ensure!(
            !self.input_claimed,
            "host terminal input is already claimed"
        );
        let mut reader = self.platform.input_reader();
        let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let input_thread = thread::Builder::new()
            .name("bed-terminal-input".to_owned())
            .spawn(move || {
                loop {
                    let event = reader
                        .read_event()
                        .and_then(|input| classify_event(input, || reader.size()));
                    let failed = event.is_err();
                    if sender.send(event).is_err() || failed {
                        break;
                    }
                }
            })?;
        self.input_claimed = true;
        Ok(TerminalEvents {
            receiver,
            _thread: input_thread,
        })
    }

    pub fn draw(&mut self, bytes: &[u8]) -> Result<()> {
        self.frame_renderer.reset();
        self.draw_synchronized(bytes)
    }

    /// Draws a complete logical frame using retained cell-level updates.
    ///
    /// The first frame and frames after a resize are emitted unchanged. Later
    /// frames update only changed cell runs and cursor state.
    pub fn draw_frame(&mut self, bytes: &[u8], size: TerminalSize) -> Result<()> {
        let output = self.frame_renderer.render(bytes, size);
        if output.is_empty() {
            return Ok(());
        }
        self.draw_synchronized(&output)
    }

    fn draw_synchronized(&mut self, bytes: &[u8]) -> Result<()> {
        let mut output = Vec::with_capacity(
            BEGIN_SYNCHRONIZED_UPDATE.len() + bytes.len() + END_SYNCHRONIZED_UPDATE.len(),
        );
        output.extend_from_slice(BEGIN_SYNCHRONIZED_UPDATE);
        output.extend_from_slice(bytes);
        output.extend_from_slice(END_SYNCHRONIZED_UPDATE);
        self.platform.draw(&output)
    }
}

impl TerminalEvents {
    /// Waits for one event, then drains a bounded batch without blocking.
    ///
    /// An empty batch means the timeout elapsed. This lets the application
    /// advance child terminal sessions even when the user provides no input.
    pub fn next_batch(&self, timeout: Duration) -> Result<Vec<TerminalEvent>> {
        let first = match self.receiver.recv_timeout(timeout) {
            Ok(event) => event?,
            Err(RecvTimeoutError::Timeout) => return Ok(Vec::new()),
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("host terminal input thread stopped")
            }
        };
        let mut events = vec![first];
        for event in self.receiver.try_iter().take(EVENT_QUEUE_CAPACITY - 1) {
            events.push(event?);
        }
        Ok(events)
    }
}

fn classify_event(
    input: HostInput,
    size: impl FnOnce() -> Result<TerminalSize>,
) -> Result<TerminalEvent> {
    match input {
        HostInput::Key(Key::Resize) => size().map(TerminalEvent::Resize),
        HostInput::Key(key) => Ok(TerminalEvent::Key(key)),
        HostInput::Mouse(mouse) => Ok(TerminalEvent::Mouse(mouse)),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HostInput, Key, Modifiers, MouseAction, MouseButton, MouseEvent, TerminalEvent,
        TerminalEvents, TerminalSize, classify_event,
    };
    use anyhow::Result;
    use std::{
        cell::Cell,
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn turns_resize_keys_into_sized_events() -> Result<()> {
        let queried = Cell::new(false);
        let key = classify_event(HostInput::Key(Key::BackTab), || {
            queried.set(true);
            Ok(TerminalSize {
                rows: 20,
                columns: 80,
            })
        })?;
        assert_eq!(key, TerminalEvent::Key(Key::BackTab));
        assert!(!queried.get());

        let resize = classify_event(HostInput::Key(Key::Resize), || {
            queried.set(true);
            Ok(TerminalSize {
                rows: 40,
                columns: 120,
            })
        })?;
        assert_eq!(
            resize,
            TerminalEvent::Resize(TerminalSize {
                rows: 40,
                columns: 120
            })
        );
        assert!(queried.get());

        queried.set(false);
        let mouse = MouseEvent {
            row: 2,
            column: 4,
            action: MouseAction::Press(MouseButton::Left),
            modifiers: Modifiers::default(),
        };
        assert_eq!(
            classify_event(HostInput::Mouse(mouse), || {
                queried.set(true);
                Ok(TerminalSize {
                    rows: 1,
                    columns: 1,
                })
            })?,
            TerminalEvent::Mouse(mouse)
        );
        assert!(!queried.get());
        Ok(())
    }

    #[test]
    fn batches_ready_events_and_times_out_when_idle() -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(4);
        sender.send(Ok(TerminalEvent::Key(Key::Char('a'))))?;
        sender.send(Ok(TerminalEvent::Key(Key::Char('b'))))?;
        let events = TerminalEvents {
            receiver,
            _thread: thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                drop(sender);
            }),
        };

        assert_eq!(
            events.next_batch(Duration::from_secs(1))?,
            [
                TerminalEvent::Key(Key::Char('a')),
                TerminalEvent::Key(Key::Char('b'))
            ]
        );
        let started = Instant::now();
        assert!(events.next_batch(Duration::from_millis(10))?.is_empty());
        assert!(started.elapsed() >= Duration::from_millis(5));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            events
                .next_batch(Duration::from_secs(1))
                .unwrap_err()
                .to_string()
                .contains("input thread stopped")
        );
        Ok(())
    }
}
