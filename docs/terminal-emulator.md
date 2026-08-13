# bed-vt100 terminal emulator

This document defines the implementation boundary for the embedded terminal
planned for bed 0.4.0. `bed-vt100` is a native bed component, not an
API-compatible replacement for the `vt100` crate.

## Goals

- Parse terminal output incrementally without panicking on malformed or partial input.
- Keep bounded primary-screen scrollback and a separate alternate screen.
- Expose cells, cursor state, wrapping, terminal modes, titles, and terminal responses needed by bed.
- Preserve deterministic behavior across Linux PTY, Windows ConPTY, macOS, FreeBSD, and NetBSD sessions.
- Keep process ownership, input encoding, and UI placement outside the emulator.

## State model

The emulator owns these concepts:

- `TerminalEmulator`: incremental parser and current terminal state.
- `Screen`: primary or alternate grid, dimensions, margins, cursor, and scrollback.
- `Cell`: grapheme content, display width, colors, and text attributes.
- `TerminalModes`: application cursor/keypad, bracketed paste, origin, insert, wrapping, and mouse-reporting modes.

PTY spawning, process status, event delivery, copy selection, and split layout
remain separate layers. The public API is shaped by bed's consumers rather
than by an existing terminal crate.

## TUI interaction

`:terminal [COMMAND]` opens a new session in a bottom split and starts in
Terminal Input mode. Keys are encoded from bed's semantic input events using
the child terminal modes tracked by `bed-vt100`.

Terminal Input reserves a bed-owned `Ctrl-\` prefix:

- `Ctrl-\ Ctrl-N` enters Terminal Normal.
- `Ctrl-\ Ctrl-W` applies one bed window command without sending it to the child.
- `Ctrl-\ Ctrl-\` sends a literal `Ctrl-\` to the child.

Other prefixed keys are rejected instead of being forwarded ambiguously.
Terminal Normal uses `j`, `k`, arrows, page keys, `Ctrl-U`, and `Ctrl-D` for
view-local scrollback, while `G` or End returns to live output. `i`, `a`, and
Enter return to Terminal Input and live output. `:` opens bed's command mode
and `Ctrl-W` applies a window command. These are bed semantics rather than a
Vim compatibility contract.

Several views may refer to one running session, but only the focused view owns
its PTY dimensions. A view scrolled into history remains anchored as output
arrives, including after the bounded history reaches capacity.

Closing a terminal window detaches its view without implicitly terminating the
session. `:terminals` lists retained sessions, `:terminalattach ID` creates a
new view, and `:terminalclose[!] ID` removes an unviewed session, optionally
terminating a running process. This keeps process lifetime distinct from layout
lifetime without leaving hidden sessions unmanaged.

## Compatibility scope

The first implementation covers sequences emitted by common shells and
interactive command-line programs:

- ASCII, UTF-8, combining marks, wide graphemes, and invalid-input recovery.
- C0 controls: BEL, BS, HT, LF, VT, FF, CR, SO, and SI.
- CSI cursor movement, positioning, erasing, insertion/deletion, scrolling, SGR, margins, save/restore, and device status reports.
- DEC private modes for cursor keys, alternate screen, cursor visibility, mouse reporting, and bracketed paste.
- OSC window titles terminated by BEL or ST.
- Resize behavior for both screens, cursor, margins, wide cells, and scrollback.

Unsupported sequences are ignored safely and counted for diagnostics. The
initial implementation excludes sixel, ReGIS, printer control, Tektronix
modes, and complete historical VT100 hardware emulation.

## Testing contract

1. Every fixture produces the same state at every possible input split.
2. Dimensions never reach zero, cursors stay in bounds, and visible rows always equal screen height.
3. Combining marks and joined emoji remain one cell, wide cells retain a continuation cell, and resizing leaves no orphan continuations.
4. Scrollback remains bounded and alternate-screen output never enters primary history.
5. Application cursor, alternate screen, cursor visibility, mouse, and bracketed-paste modes transition independently.
6. Expected screen states for shell and application transcripts live in this repository.
7. Native PTY/ConPTY tests cover output, input, resize, exit, forced termination, and teardown on runtime-tested platforms.

Arbitrary byte and resize streams should become fuzz targets after the core
state model stabilizes.

## Source and license policy

Terminal standards and public control-sequence documentation are authoritative.
The upstream `vt100` crate is MIT licensed, but no implementation code is
copied into `bed-vt100`. Existing emulators may inform compatibility cases and
test ideas. Imported fixtures or code require explicit provenance and a
compatible license.

## Release gate

The embedded terminal remains pre-release until:

- parser, invariant, scrollback, mode, Unicode, response, and resize tests pass;
- Linux x86_64 has end-to-end PTY coverage under sustained output and input backpressure;
- Windows x86_64 has native ConPTY validation;
- other native targets are documented accurately as preview or runtime-tested;
- no patched or vendored terminal-emulator dependency remains in release packages.

Workspace publishing must follow dependency order: publish `bed-vt100` before
`bed-terminal` and `bed-pty`, then `bed-terminal-session` and the application
crates. Until
`bed-vt100` exists in the registry, Cargo can list the dependent package
contents but cannot prepare or verify those crates for upload in isolation.
