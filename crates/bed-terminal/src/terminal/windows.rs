//! Windows Console backend.
//!
//! Input remains in the structured Win32 Console API so modifier state,
//! resize records, repeat counts, and UTF-16 units are not lost. Output uses
//! virtual-terminal processing, which current Windows console hosts expose
//! independently through the output mode.
//!
//! Authoritative references:
//! - Microsoft Learn [`GetStdHandle`](https://learn.microsoft.com/en-us/windows/console/getstdhandle),
//!   [`GetConsoleMode`](https://learn.microsoft.com/en-us/windows/console/getconsolemode), and
//!   [`SetConsoleMode`](https://learn.microsoft.com/en-us/windows/console/setconsolemode)
//! - Microsoft Learn [`ReadConsoleInput`](https://learn.microsoft.com/en-us/windows/console/readconsoleinput),
//!   [`INPUT_RECORD`](https://learn.microsoft.com/en-us/windows/console/input-record-str), and
//!   [`KEY_EVENT_RECORD`](https://learn.microsoft.com/en-us/windows/console/key-event-record-str)
//! - Microsoft Learn [`CONSOLE_SCREEN_BUFFER_INFO`](https://learn.microsoft.com/en-us/windows/console/console-screen-buffer-info-str)
//!   and [virtual-key codes](https://learn.microsoft.com/en-us/windows/win32/inputdev/virtual-key-codes)
//! - xterm [`ctlseqs`](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html)

use super::{
    HostInput, Key, Modifiers, MouseAction, MouseButton, MouseEvent, SpecialKey, TerminalSize,
};
use anyhow::{Context, Result, ensure};
use std::{
    ffi::c_void,
    io::{self, Write},
    mem::MaybeUninit,
};

type Bool = i32;
type Dword = u32;
type Handle = *mut c_void;
type Word = u16;

// GetStdHandle values from the Windows SDK's winbase.h.
const STD_INPUT_HANDLE: Dword = -10_i32 as Dword;
const STD_OUTPUT_HANDLE: Dword = -11_i32 as Dword;
const INVALID_HANDLE_VALUE: Handle = -1_isize as Handle;

// Console mode flags from wincon.h. ENABLE_WINDOW_INPUT makes buffer-size
// changes available as input records; ENABLE_VIRTUAL_TERMINAL_PROCESSING makes
// the output handle interpret the xterm sequences emitted by the renderer.
const ENABLE_PROCESSED_INPUT: Dword = 0x0001;
const ENABLE_LINE_INPUT: Dword = 0x0002;
const ENABLE_ECHO_INPUT: Dword = 0x0004;
const ENABLE_WINDOW_INPUT: Dword = 0x0008;
const ENABLE_MOUSE_INPUT: Dword = 0x0010;
const ENABLE_QUICK_EDIT_MODE: Dword = 0x0040;
const ENABLE_EXTENDED_FLAGS: Dword = 0x0080;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: Dword = 0x0004;
const DISABLE_NEWLINE_AUTO_RETURN: Dword = 0x0008;

// INPUT_RECORD event tags from wincon.h.
const KEY_EVENT: Word = 0x0001;
const MOUSE_EVENT: Word = 0x0002;
const WINDOW_BUFFER_SIZE_EVENT: Word = 0x0004;

const FROM_LEFT_1ST_BUTTON_PRESSED: Dword = 0x0001;
const RIGHTMOST_BUTTON_PRESSED: Dword = 0x0002;
const FROM_LEFT_2ND_BUTTON_PRESSED: Dword = 0x0004;
const MOUSE_MOVED: Dword = 0x0001;
const MOUSE_WHEELED: Dword = 0x0004;
const MOUSE_HWHEELED: Dword = 0x0008;

// KEY_EVENT_RECORD control-key-state flags from wincon.h.
const RIGHT_ALT_PRESSED: Dword = 0x0001;
const LEFT_ALT_PRESSED: Dword = 0x0002;
const RIGHT_CTRL_PRESSED: Dword = 0x0004;
const LEFT_CTRL_PRESSED: Dword = 0x0008;
const SHIFT_PRESSED: Dword = 0x0010;

// Virtual-key codes from winuser.h.
const VK_BACK: Word = 0x08;
const VK_TAB: Word = 0x09;
const VK_RETURN: Word = 0x0d;
const VK_ESCAPE: Word = 0x1b;
const VK_PRIOR: Word = 0x21;
const VK_NEXT: Word = 0x22;
const VK_END: Word = 0x23;
const VK_HOME: Word = 0x24;
const VK_LEFT: Word = 0x25;
const VK_UP: Word = 0x26;
const VK_RIGHT: Word = 0x27;
const VK_DOWN: Word = 0x28;
const VK_DELETE: Word = 0x2e;

// xterm private modes 1049 and 25 select the alternate screen and cursor
// visibility respectively.
const ENTER_TERMINAL_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?25l";
const LEAVE_TERMINAL_SCREEN: &[u8] = b"\x1b[?2026l\x1b[?25h\x1b[?1049l\x1b[0 q";

/// Win32 `COORD`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Coord {
    x: i16,
    y: i16,
}

/// Win32 `SMALL_RECT`.
#[repr(C)]
struct SmallRect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}

/// Win32 `CONSOLE_SCREEN_BUFFER_INFO`.
#[repr(C)]
#[allow(dead_code)]
struct ConsoleScreenBufferInfo {
    size: Coord,
    cursor_position: Coord,
    attributes: Word,
    window: SmallRect,
    maximum_window_size: Coord,
}

/// Win32 `KEY_EVENT_RECORD` with its anonymous character union represented by
/// the `UnicodeChar` member used by `ReadConsoleInputW`.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct KeyEventRecord {
    key_down: Bool,
    repeat_count: Word,
    virtual_key_code: Word,
    virtual_scan_code: Word,
    unicode_char: u16,
    control_key_state: Dword,
}

/// Win32 `WINDOW_BUFFER_SIZE_RECORD`.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct WindowBufferSizeRecord {
    size: Coord,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseEventRecord {
    position: Coord,
    button_state: Dword,
    control_key_state: Dword,
    event_flags: Dword,
}

/// The largest members of Win32 `INPUT_RECORD.Event` are 16 bytes.
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
union InputEvent {
    key: KeyEventRecord,
    mouse: MouseEventRecord,
    window_size: WindowBufferSizeRecord,
    padding: [Dword; 4],
}

/// Win32 `INPUT_RECORD`.
#[repr(C)]
#[derive(Clone, Copy)]
struct InputRecord {
    event_type: Word,
    event: InputEvent,
}

// Win32 declarations from winbase.h and wincon.h. `link_name` keeps Rust names
// idiomatic while binding the exact exported symbols.
#[link(name = "kernel32")]
unsafe extern "system" {
    #[link_name = "GetStdHandle"]
    fn get_std_handle(standard_handle: Dword) -> Handle;
    #[link_name = "GetConsoleMode"]
    fn get_console_mode_ffi(console: Handle, mode: *mut Dword) -> Bool;
    #[link_name = "SetConsoleMode"]
    fn set_console_mode_ffi(console: Handle, mode: Dword) -> Bool;
    #[link_name = "GetConsoleScreenBufferInfo"]
    fn get_console_screen_buffer_info(console: Handle, info: *mut ConsoleScreenBufferInfo) -> Bool;
    #[link_name = "ReadConsoleInputW"]
    fn read_console_input(
        console: Handle,
        records: *mut InputRecord,
        length: Dword,
        records_read: *mut Dword,
    ) -> Bool;
}

pub(super) struct PlatformTerminal {
    input: Handle,
    output: Handle,
    original_input_mode: Dword,
    original_output_mode: Dword,
    stdout: io::Stdout,
    repeated_key: Option<(Key, Word)>,
    high_surrogate: Option<u16>,
}

pub(super) struct PlatformTerminalReader {
    input: usize,
    output: usize,
    repeated_key: Option<(Key, Word)>,
    high_surrogate: Option<u16>,
    mouse_buttons: Dword,
}

impl PlatformTerminal {
    pub(super) fn new() -> Result<Self> {
        let input = get_standard_handle(STD_INPUT_HANDLE)?;
        let output = get_standard_handle(STD_OUTPUT_HANDLE)?;
        let original_input_mode = get_console_mode(input, "standard input")?;
        let original_output_mode = get_console_mode(output, "standard output")?;

        let input_mode = (original_input_mode
            & !(ENABLE_PROCESSED_INPUT
                | ENABLE_LINE_INPUT
                | ENABLE_ECHO_INPUT
                | ENABLE_QUICK_EDIT_MODE))
            | ENABLE_WINDOW_INPUT
            | ENABLE_MOUSE_INPUT
            | ENABLE_EXTENDED_FLAGS;
        set_console_mode(input, input_mode, "standard input")?;

        // VT output is enabled independently of structured console input. If
        // this second transition fails, input mode is rolled back immediately.
        let output_mode =
            original_output_mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;
        if let Err(error) = set_console_mode(output, output_mode, "standard output") {
            let _ = set_console_mode(input, original_input_mode, "standard input");
            return Err(error);
        }

        let mut terminal = Self {
            input,
            output,
            original_input_mode,
            original_output_mode,
            stdout: io::stdout(),
            repeated_key: None,
            high_surrogate: None,
        };
        terminal.draw(ENTER_TERMINAL_SCREEN)?;
        Ok(terminal)
    }

    pub(super) fn size(&self) -> Result<TerminalSize> {
        get_terminal_size(self.output)
    }

    pub(super) fn read_key(&mut self) -> Result<Key> {
        read_key(self.input, &mut self.repeated_key, &mut self.high_surrogate)
    }

    pub(super) fn input_reader(&self) -> PlatformTerminalReader {
        PlatformTerminalReader {
            input: self.input as usize,
            output: self.output as usize,
            repeated_key: None,
            high_surrogate: None,
            mouse_buttons: 0,
        }
    }

    pub(super) fn draw(&mut self, bytes: &[u8]) -> Result<()> {
        self.stdout
            .write_all(bytes)
            .context("failed to write terminal output")?;
        self.stdout
            .flush()
            .context("failed to flush terminal output")
    }
}

impl PlatformTerminalReader {
    pub(super) fn size(&self) -> Result<TerminalSize> {
        get_terminal_size(self.output as Handle)
    }

    pub(super) fn read_event(&mut self) -> Result<HostInput> {
        read_event(
            self.input as Handle,
            self.output as Handle,
            &mut self.repeated_key,
            &mut self.high_surrogate,
            &mut self.mouse_buttons,
        )
    }
}

fn read_key(
    input: Handle,
    repeated_key: &mut Option<(Key, Word)>,
    high_surrogate: &mut Option<u16>,
) -> Result<Key> {
    let output = get_standard_handle(STD_OUTPUT_HANDLE)?;
    let mut mouse_buttons = 0;
    loop {
        if let HostInput::Key(key) = read_event(
            input,
            output,
            repeated_key,
            high_surrogate,
            &mut mouse_buttons,
        )? {
            return Ok(key);
        }
    }
}

fn read_event(
    input: Handle,
    output: Handle,
    repeated_key: &mut Option<(Key, Word)>,
    high_surrogate: &mut Option<u16>,
    mouse_buttons: &mut Dword,
) -> Result<HostInput> {
    if let Some((key, remaining)) = repeated_key.take() {
        if remaining > 1 {
            *repeated_key = Some((key.clone(), remaining - 1));
        }
        return Ok(HostInput::Key(key));
    }

    loop {
        let record = read_input_record(input)?;
        match record.event_type {
            WINDOW_BUFFER_SIZE_EVENT => return Ok(HostInput::Key(Key::Resize)),
            KEY_EVENT => {
                // SAFETY: INPUT_RECORD's event tag identifies the active
                // union member as KEY_EVENT_RECORD.
                let event = unsafe { record.event.key };
                if event.key_down == 0 {
                    continue;
                }
                if let Some(key) = translate_key(event, high_surrogate) {
                    // ReadConsoleInputW coalesces held keys in repeat_count;
                    // preserve that behavior as individual editor events.
                    if event.repeat_count > 1 {
                        *repeated_key = Some((key.clone(), event.repeat_count - 1));
                    }
                    return Ok(HostInput::Key(key));
                }
            }
            MOUSE_EVENT => {
                // SAFETY: INPUT_RECORD's event tag identifies the active
                // union member as MOUSE_EVENT_RECORD.
                let event = unsafe { record.event.mouse };
                if let Some(mouse) = translate_mouse(event, mouse_buttons, terminal_origin(output)?)
                {
                    return Ok(HostInput::Mouse(mouse));
                }
            }
            _ => {}
        }
    }
}

fn translate_mouse(
    event: MouseEventRecord,
    previous_buttons: &mut Dword,
    origin: Coord,
) -> Option<MouseEvent> {
    if event.event_flags & MOUSE_HWHEELED != 0 {
        return None;
    }
    let row = usize::try_from(i32::from(event.position.y) - i32::from(origin.y)).ok()?;
    let column = usize::try_from(i32::from(event.position.x) - i32::from(origin.x)).ok()?;
    let modifiers = decode_modifiers(event.control_key_state);
    let action = if event.event_flags & MOUSE_WHEELED != 0 {
        if (event.button_state >> 16) as i16 > 0 {
            MouseAction::ScrollUp
        } else {
            MouseAction::ScrollDown
        }
    } else {
        let changed = *previous_buttons ^ event.button_state;
        let button = changed_button(changed).or_else(|| changed_button(event.button_state));
        let action = if event.event_flags & MOUSE_MOVED != 0 {
            button.map_or(MouseAction::Move, MouseAction::Drag)
        } else if let Some(button) = changed_button(changed) {
            if event.button_state & button_mask(button) != 0 {
                MouseAction::Press(button)
            } else {
                MouseAction::Release(button)
            }
        } else {
            *previous_buttons = event.button_state;
            return None;
        };
        *previous_buttons = event.button_state;
        action
    };
    Some(MouseEvent {
        row,
        column,
        action,
        modifiers,
    })
}

fn changed_button(state: Dword) -> Option<MouseButton> {
    if state & FROM_LEFT_1ST_BUTTON_PRESSED != 0 {
        Some(MouseButton::Left)
    } else if state & FROM_LEFT_2ND_BUTTON_PRESSED != 0 {
        Some(MouseButton::Middle)
    } else if state & RIGHTMOST_BUTTON_PRESSED != 0 {
        Some(MouseButton::Right)
    } else {
        None
    }
}

fn button_mask(button: MouseButton) -> Dword {
    match button {
        MouseButton::Left => FROM_LEFT_1ST_BUTTON_PRESSED,
        MouseButton::Middle => FROM_LEFT_2ND_BUTTON_PRESSED,
        MouseButton::Right => RIGHTMOST_BUTTON_PRESSED,
    }
}

fn read_input_record(input: Handle) -> Result<InputRecord> {
    let mut record = MaybeUninit::<InputRecord>::uninit();
    let mut records_read = 0;
    // SAFETY: `self.input` is a validated console handle. The record and
    // count pointers are writable for the one requested input record.
    let success = unsafe { read_console_input(input, record.as_mut_ptr(), 1, &mut records_read) };
    ensure!(
        success != 0,
        "failed to read terminal input: {}",
        io::Error::last_os_error()
    );
    ensure!(records_read == 1, "terminal input returned no records");
    // SAFETY: ReadConsoleInputW reported that it initialized one record.
    Ok(unsafe { record.assume_init() })
}

fn get_terminal_size(output: Handle) -> Result<TerminalSize> {
    let mut info = MaybeUninit::<ConsoleScreenBufferInfo>::uninit();
    // SAFETY: `output` is a validated console handle and `info` points to
    // writable storage with the documented Win32 structure layout.
    let success = unsafe { get_console_screen_buffer_info(output, info.as_mut_ptr()) };
    ensure!(
        success != 0,
        "failed to read terminal size: {}",
        io::Error::last_os_error()
    );
    // SAFETY: successful GetConsoleScreenBufferInfo initialized `info`.
    let window = unsafe { info.assume_init() }.window;
    let columns = i32::from(window.right) - i32::from(window.left) + 1;
    let rows = i32::from(window.bottom) - i32::from(window.top) + 1;
    ensure!(rows > 0 && columns > 0, "terminal size is zero");
    Ok(TerminalSize {
        rows: rows as usize,
        columns: columns as usize,
    })
}

fn terminal_origin(output: Handle) -> Result<Coord> {
    let mut info = MaybeUninit::<ConsoleScreenBufferInfo>::uninit();
    // SAFETY: `output` is a validated console handle and `info` points to
    // writable storage with the documented Win32 structure layout.
    let success = unsafe { get_console_screen_buffer_info(output, info.as_mut_ptr()) };
    ensure!(
        success != 0,
        "failed to read terminal position: {}",
        io::Error::last_os_error()
    );
    // SAFETY: successful GetConsoleScreenBufferInfo initialized `info`.
    let window = unsafe { info.assume_init() }.window;
    Ok(Coord {
        x: window.left,
        y: window.top,
    })
}

impl Drop for PlatformTerminal {
    fn drop(&mut self) {
        // Reverse both owned mode transitions during normal return or Rust
        // unwind. Drop cannot run after process abort or forcible termination.
        let _ = self.stdout.write_all(LEAVE_TERMINAL_SCREEN);
        let _ = self.stdout.flush();
        let _ = set_console_mode(self.input, self.original_input_mode, "standard input");
        let _ = set_console_mode(self.output, self.original_output_mode, "standard output");
    }
}

fn get_standard_handle(kind: Dword) -> Result<Handle> {
    // SAFETY: GetStdHandle accepts the constant value supplied by the caller
    // and has no pointer preconditions.
    let handle = unsafe { get_std_handle(kind) };
    ensure!(
        !handle.is_null() && handle != INVALID_HANDLE_VALUE,
        "failed to get console handle: {}",
        io::Error::last_os_error()
    );
    Ok(handle)
}

fn get_console_mode(console: Handle, name: &str) -> Result<Dword> {
    let mut mode = 0;
    // SAFETY: `console` was returned by GetStdHandle, and `mode` is writable.
    let success = unsafe { get_console_mode_ffi(console, &mut mode) };
    ensure!(
        success != 0,
        "{name} is not a console: {}",
        io::Error::last_os_error()
    );
    Ok(mode)
}

fn set_console_mode(console: Handle, mode: Dword, name: &str) -> Result<()> {
    // SAFETY: `console` is a handle validated by GetConsoleMode, and `mode`
    // contains only documented console-mode bits plus bits already present.
    let success = unsafe { set_console_mode_ffi(console, mode) };
    ensure!(
        success != 0,
        "failed to set {name} mode: {}",
        io::Error::last_os_error()
    );
    Ok(())
}

fn decode_modifiers(state: Dword) -> Modifiers {
    Modifiers {
        shift: state & SHIFT_PRESSED != 0,
        alt: state & (LEFT_ALT_PRESSED | RIGHT_ALT_PRESSED) != 0,
        control: state & (LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED) != 0,
    }
}

fn translate_key(event: KeyEventRecord, high_surrogate: &mut Option<u16>) -> Option<Key> {
    let modifiers = decode_modifiers(event.control_key_state);
    let special = match event.virtual_key_code {
        VK_BACK => return Some(Key::Backspace),
        VK_TAB => {
            return Some(if modifiers.shift {
                Key::BackTab
            } else {
                Key::Tab
            });
        }
        VK_RETURN => return Some(Key::Enter),
        VK_ESCAPE => return Some(Key::Escape),
        VK_PRIOR => Some(SpecialKey::PageUp),
        VK_NEXT => Some(SpecialKey::PageDown),
        VK_DELETE => Some(SpecialKey::Delete),
        VK_UP => Some(SpecialKey::ArrowUp),
        VK_DOWN => Some(SpecialKey::ArrowDown),
        VK_LEFT => Some(SpecialKey::ArrowLeft),
        VK_RIGHT => Some(SpecialKey::ArrowRight),
        VK_HOME => Some(SpecialKey::Home),
        VK_END => Some(SpecialKey::End),
        _ => None,
    };
    if let Some(special) = special {
        return Some(navigation_key(special, modifiers));
    }

    let unit = event.unicode_char;
    if unit == 0 {
        return None;
    }
    if modifiers.control && (1..=26).contains(&unit) {
        *high_surrogate = None;
        return Some(Key::Ctrl(char::from_u32(u32::from(unit) + 0x60)?));
    }
    if modifiers.control && (0x1c..=0x1f).contains(&unit) {
        *high_surrogate = None;
        return Some(Key::Ctrl(char::from_u32(u32::from(unit) + 0x40)?));
    }
    if (0xd800..=0xdbff).contains(&unit) {
        // KEY_EVENT_RECORD carries one UTF-16 unit, so supplementary Unicode
        // characters arrive as two records and must be joined here.
        *high_surrogate = Some(unit);
        return None;
    }
    if (0xdc00..=0xdfff).contains(&unit) {
        let high = high_surrogate.take()?;
        let codepoint = 0x10000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(unit) - 0xdc00);
        return char::from_u32(codepoint).map(Key::Char);
    }
    *high_surrogate = None;
    char::from_u32(u32::from(unit)).map(Key::Char)
}

fn navigation_key(key: SpecialKey, modifiers: Modifiers) -> Key {
    if modifiers != Modifiers::default() {
        return Key::Modified(key, modifiers);
    }
    match key {
        SpecialKey::Delete => Key::Delete,
        SpecialKey::ArrowUp => Key::ArrowUp,
        SpecialKey::ArrowDown => Key::ArrowDown,
        SpecialKey::ArrowLeft => Key::ArrowLeft,
        SpecialKey::ArrowRight => Key::ArrowRight,
        SpecialKey::Home => Key::Home,
        SpecialKey::End => Key::End,
        SpecialKey::PageUp => Key::PageUp,
        SpecialKey::PageDown => Key::PageDown,
    }
}

const _: () = {
    // These assertions catch accidental field or union layout drift before an
    // FFI call can write through the handwritten declarations.
    assert!(size_of::<Coord>() == 4);
    assert!(size_of::<ConsoleScreenBufferInfo>() == 22);
    assert!(size_of::<KeyEventRecord>() == 16);
    assert!(size_of::<MouseEventRecord>() == 16);
    assert!(size_of::<InputRecord>() == 20);
};

#[cfg(test)]
mod tests {
    use super::{
        Coord, FROM_LEFT_1ST_BUTTON_PRESSED, Key, KeyEventRecord, LEFT_CTRL_PRESSED, MOUSE_MOVED,
        MOUSE_WHEELED, Modifiers, MouseAction, MouseButton, MouseEvent, MouseEventRecord,
        SHIFT_PRESSED, SpecialKey, VK_LEFT, VK_TAB, translate_key, translate_mouse,
    };

    fn event(virtual_key_code: u16, unicode_char: u16, control_key_state: u32) -> KeyEventRecord {
        KeyEventRecord {
            key_down: 1,
            repeat_count: 1,
            virtual_key_code,
            virtual_scan_code: 0,
            unicode_char,
            control_key_state,
        }
    }

    #[test]
    fn translates_modified_navigation_and_control_keys() {
        let mut surrogate = None;
        assert_eq!(
            translate_key(event(VK_LEFT, 0, SHIFT_PRESSED), &mut surrogate),
            Some(Key::Modified(
                SpecialKey::ArrowLeft,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                }
            ))
        );
        assert_eq!(
            translate_key(event(0, 18, LEFT_CTRL_PRESSED), &mut surrogate),
            Some(Key::Ctrl('r'))
        );
        assert_eq!(
            translate_key(event(0, 0x1c, LEFT_CTRL_PRESSED), &mut surrogate),
            Some(Key::Ctrl('\\'))
        );
    }

    #[test]
    fn translates_tab_and_shift_tab_to_distinct_events() {
        let mut surrogate = None;
        assert_eq!(
            translate_key(event(VK_TAB, b'\t'.into(), 0), &mut surrogate),
            Some(Key::Tab)
        );
        assert_eq!(
            translate_key(event(VK_TAB, b'\t'.into(), SHIFT_PRESSED), &mut surrogate),
            Some(Key::BackTab)
        );
    }

    #[test]
    fn combines_utf16_surrogate_pairs() {
        let mut surrogate = None;
        assert_eq!(translate_key(event(0, 0xd83d, 0), &mut surrogate), None);
        assert_eq!(
            translate_key(event(0, 0xde00, 0), &mut surrogate),
            Some(Key::Char('😀'))
        );
    }

    #[test]
    fn translates_mouse_coordinates_buttons_and_motion() {
        let origin = Coord { x: 10, y: 20 };
        let mut buttons = 0;
        let press = MouseEventRecord {
            position: Coord { x: 14, y: 22 },
            button_state: FROM_LEFT_1ST_BUTTON_PRESSED,
            control_key_state: SHIFT_PRESSED,
            event_flags: 0,
        };
        assert_eq!(
            translate_mouse(press, &mut buttons, origin),
            Some(MouseEvent {
                row: 2,
                column: 4,
                action: MouseAction::Press(MouseButton::Left),
                modifiers: Modifiers {
                    shift: true,
                    ..Modifiers::default()
                },
            })
        );
        assert_eq!(buttons, FROM_LEFT_1ST_BUTTON_PRESSED);

        let drag = MouseEventRecord {
            position: Coord { x: 15, y: 23 },
            event_flags: MOUSE_MOVED,
            ..press
        };
        assert_eq!(
            translate_mouse(drag, &mut buttons, origin).map(|mouse| mouse.action),
            Some(MouseAction::Drag(MouseButton::Left))
        );

        let release = MouseEventRecord {
            position: Coord { x: 15, y: 23 },
            button_state: 0,
            control_key_state: 0,
            event_flags: 0,
        };
        assert_eq!(
            translate_mouse(release, &mut buttons, origin).map(|mouse| mouse.action),
            Some(MouseAction::Release(MouseButton::Left))
        );
        assert_eq!(buttons, 0);
    }

    #[test]
    fn translates_vertical_wheel_direction() {
        let origin = Coord { x: 0, y: 0 };
        let mut buttons = 0;
        let wheel = |delta: i16| MouseEventRecord {
            position: origin,
            button_state: u32::from(delta as u16) << 16,
            control_key_state: 0,
            event_flags: MOUSE_WHEELED,
        };
        assert_eq!(
            translate_mouse(wheel(120), &mut buttons, origin).map(|mouse| mouse.action),
            Some(MouseAction::ScrollUp)
        );
        assert_eq!(
            translate_mouse(wheel(-120), &mut buttons, origin).map(|mouse| mouse.action),
            Some(MouseAction::ScrollDown)
        );
    }
}
