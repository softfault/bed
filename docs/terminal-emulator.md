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
Terminal Normal owns a view-local navigation cursor. `h`, `j`, `k`, `l`,
arrows, row edges, page keys, `Ctrl-U`, and `Ctrl-D` move it through terminal
cells and scroll the viewport only as needed; `G` or End returns it to the live
child cursor. `i`, `a`, and Enter return to Terminal Input and live output. `:`
opens bed's command mode and `Ctrl-W` applies a window command. These are bed
semantics rather than a Vim compatibility contract.

Host mouse input is normalized to zero-based cell coordinates on every native
backend. In Terminal Input, events inside the focused terminal's live content
area are translated into child coordinates and filtered by the child's
`1000`, `1002`, or `1003` tracking mode. SGR `1006` reports preserve press,
release, drag, motion, wheel, and modifier information. Status rows, other
windows, scrolled-back views, and bed-owned modes do not forward mouse input.

`v` enters cell-based Terminal Visual and `V` enters logical-line Terminal
Visual. The active key cancels its selection, while the other key switches the
selection kind. Cell motions account for wide graphemes. Logical-line bounds
expand across soft-wrapped rows, so `y` writes a linewise bed register without
introducing display-only newlines. Selections belong to the view rather than
the session, remain anchored while output advances, and are clamped when
bounded history evicts its oldest rows.

Several views may refer to one running session, but only the focused view owns
its PTY dimensions. A view scrolled into history remains anchored as output
arrives, including after the bounded history reaches capacity.

The host loop forwards ordinary Terminal Input keys without redrawing stale
emulator state, then redraws after PTY output is applied. Output events are
coalesced within a short processing budget, and complete host frames are
wrapped in synchronized-update markers so supporting terminals present them
atomically instead of exposing clear-and-repaint intermediates.

Closing a terminal window detaches its view without implicitly terminating the
session. `:terminals` lists retained sessions, `:terminalattach ID` creates a
new view, and `:terminalclose[!] ID` removes an unviewed session, optionally
terminating a running process. This keeps process lifetime distinct from layout
lifetime without leaving hidden sessions unmanaged.

The terminal status line prefers a non-empty OSC title and otherwise shows the
spawned command. Host rendering replaces control characters in titles rather
than forwarding them to the outer terminal. BEL and visual bell requests are
surfaced as concise bed messages; several events delivered in one poll are
combined into counts instead of emitting host control bytes or flooding the
message area.

## Compatibility scope

The first implementation covers sequences emitted by common shells and
interactive command-line programs:

- ASCII, UTF-8, combining marks, wide graphemes, and invalid-input recovery.
- C0 controls: BEL, BS, HT, LF, VT, FF, CR, SO, and SI.
- CSI cursor movement, positioning, erasing, insertion/deletion, scrolling, SGR, margins, save/restore, and device status reports.
- DEC private modes for cursor keys, alternate screen, cursor visibility, mouse reporting, and bracketed paste.
- OSC window titles terminated by BEL or ST.
- DEC private erase aliases (`CSI ? J` and `CSI ? K`) and visual bell (`ESC g`).
- Resize behavior for both screens, cursor, margins, wide cells, and scrollback.

Unsupported sequences are ignored safely and counted for diagnostics. The OSC
payload buffer is limited to 4096 bytes. An oversized string is consumed
through BEL or ST but rejected as a whole, so a truncated title is never
applied and parsing resumes at the next byte. CSI parameter storage is limited
to 256 bytes; an oversized sequence is likewise consumed through its final byte
without exposing its discarded tail as screen text. Bare C1 controls are
non-printing and ignored without interfering with valid UTF-8 continuation
bytes. The initial implementation
excludes sixel, ReGIS, printer control, Tektronix modes, and complete historical
VT100 hardware emulation.

## Testing contract

1. Every fixture produces the same state at every possible input split.
2. Dimensions never reach zero, cursors stay in bounds, and visible rows always equal screen height.
3. Combining marks and joined emoji remain one cell, wide cells retain a continuation cell, and resizing leaves no orphan continuations.
4. Scrollback remains bounded and alternate-screen output never enters primary history.
5. Application cursor, alternate screen, cursor visibility, mouse, and bracketed-paste modes transition independently.
6. Expected screen states for shell and application transcripts live in this repository.
7. Native PTY/ConPTY tests cover output, input, resize, exit, forced termination, and teardown on runtime-tested platforms.
8. OSC title termination, size bounds, recovery, and BEL delivery are covered across emulator, session, and TUI boundaries.
9. Host parsing, child encoding, mode filtering, coordinate translation, and real PTY mouse delivery are tested independently.

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
