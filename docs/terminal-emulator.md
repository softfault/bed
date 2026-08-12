# Bed terminal emulator

This document defines the implementation boundary for the embedded terminal
planned for bed 0.3.0. It is a native bed component, not an API-compatible
replacement for the `vt100` crate.

## Goals

- Parse terminal output incrementally without panicking on malformed or partial input.
- Keep a bounded primary-screen scrollback and a separate alternate screen.
- Expose cells, cursor state, wrapping, and terminal modes needed by bed's renderer and input encoder.
- Preserve deterministic behavior across Linux PTY, Windows ConPTY, macOS, FreeBSD, and NetBSD sessions.
- Keep process ownership and UI placement outside the emulator.

## Initial model

The emulator owns these concepts:

- `TerminalEmulator`: parser entry point and current terminal state.
- `Screen`: primary grid, alternate grid, dimensions, scroll region, and bounded scrollback.
- `Cell`: grapheme content, display width, foreground/background colors, and text attributes.
- `Cursor`: row, column, visibility, saved position, and pending-wrap state.
- `TerminalModes`: application cursor keys, application keypad, bracketed paste, origin mode, insert mode, and automatic wrapping.

The first public API should be shaped by bed's consumers rather than by an
existing terminal crate. PTY spawning, process status, event delivery, copy
selection, and split layout remain separate layers.

## Compatibility scope

The first implementation must cover the sequences emitted by common shells and
interactive command-line programs:

- ASCII, UTF-8, combining marks, wide characters, and invalid input recovery.
- C0 controls: BEL, BS, HT, LF, VT, FF, CR, SO, and SI.
- CSI cursor movement, positioning, erasing, insertion/deletion, scrolling, SGR, scroll regions, save/restore, and device status reports.
- DEC private modes required for cursor keys, alternate screen, cursor visibility, mouse reporting state, and bracketed paste.
- OSC window title with both BEL and ST termination.
- Resize behavior for the primary screen, alternate screen, cursor, margins, and scrollback.

Unsupported sequences must be ignored safely and counted for diagnostics. Bed
does not initially need sixel, ReGIS, printer control, Tektronix modes, or a
complete historical VT100 hardware emulation.

## Testing contract

Implementation starts with tests, grouped by behavior rather than escape-code
number:

1. Parser chunking: every fixture produces the same state at every possible input split.
2. Grid invariants: dimensions never reach zero, the cursor stays in bounds, and visible rows always equal screen height.
3. Unicode: combining marks attach predictably, wide cells retain their continuation cell, and resizing never leaves orphan continuations.
4. Scrollback: capacity is bounded, alternate-screen output does not enter primary scrollback, and deep history never overflows or returns more visible rows than requested.
5. Modes: application cursor, alternate screen, cursor visibility, and bracketed paste transition independently and restore correctly.
6. Differential fixtures: selected shell and application transcripts may be compared against established terminal emulators, but expected bed state is checked into this repository.
7. Native integration: real PTY/ConPTY tests cover input, output, resize, normal exit, forced termination, and teardown on every supported runtime-tested platform.

Fuzz targets should feed arbitrary bytes and arbitrary resize/process chunks to
the parser once the core state model is in place.

## Source policy

Terminal standards and public control-sequence documentation are authoritative.
Existing emulators, including `vt100`, may inform compatibility cases and test
ideas, but the bed implementation must be independently written. Any imported
fixture or code requires explicit provenance and compatible licensing.

## Release gate

The embedded terminal remains pre-release until:

- all parser, invariant, scrollback, mode, and resize tests pass;
- Linux x86_64 has end-to-end PTY coverage under sustained output and input backpressure;
- Windows x86_64 has native ConPTY validation;
- unsupported native targets are documented as preview rather than runtime-tested;
- no patched or vendored terminal-emulator dependency remains in release packages.
