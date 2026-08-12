# bed architecture

`bed` is a suckless-style cross-platform editor. Its editing core is platform-neutral, while each supported terminal environment has a small native backend owned by the project.

## Layers

The workspace is split into five packages:

| Package | Responsibility | Internal dependencies |
| --- | --- | --- |
| `bed-core` | Buffers, view-local cursor state, editing operations, and undo/redo | `bed-file` |
| `bed-file` | Recoverable file replacement and its narrow native boundary | None |
| `bed-terminal` | Semantic input types and native terminal backends | None |
| `bed-tui` | Modes, commands, layout, and frame rendering | `bed-core`, `bed-terminal` |
| `bed` | Argument parsing and the main event loop | All three libraries |

`bed-core`, `bed-tui`, and the `bed` binary forbid unsafe code. Native ABI calls are confined to `bed-terminal` and the Windows replacement operation in `bed-file`; both deny unsafe operations outside explicit unsafe blocks.

The terminal boundary is deliberately small:

```rust
Terminal::new() -> Result<Terminal>
Terminal::size(&self) -> Result<TerminalSize>
Terminal::read_key(&mut self) -> Result<Key>
Terminal::draw(&mut self, bytes: &[u8]) -> Result<()>
```

Backends are selected at compile time. There is no dynamic backend trait or platform state in the editor core.

## Editing state

`bed-core` separates shared file content from the state used to view it:

- `BufferStore` assigns a stable `BufferId` to each open `Buffer`.
- `Buffer` owns one `Document` and its bounded undo/redo history.
- `EditorView` identifies a buffer and owns its cursor and preferred column; stable `ViewId` values allow several views to refer to one buffer.
- `Editor` coordinates buffers and views, and presents the active view through the editing API.

`RegexPattern` compiles byte-oriented modern regular expressions once and is
shared by search and substitution. Search wraps across the complete buffer and
only selects offsets where Normal mode can place a cursor. Substitution is
line-oriented, preserves native line separators, and expands the regex crate's
`$0`, `$1`, and `${name}` capture syntax. Command parsing remains in `bed-tui`;
the core owns matching, replacement, and undo semantics.

Switching views never moves or copies document content. Undo/redo remains buffer-owned, while two split windows showing the same buffer retain independent cursors. Terminal-sized scrolling remains in `bed-tui`; each window associates its views with independent `Viewport` state.

`bed-tui` represents each tab page as a lightweight layout workspace above the global buffer store. A tab has a stable `TabId`, a binary split tree, an active window, recent-window focus history, a stable automatic or explicit title, and file-tree navigation state. Split-tree leaves are stable window IDs; split nodes retain equal or side-anchored cell sizes, clamp them against the recursive minimum size of each child layout, and allocate terminal rectangles without owning editor content. Recent-tab history also uses stable IDs, so reordering tabs does not change close-time focus restoration.

Windows refer to core `ViewId` values and retain one view and viewport per buffer they have shown. Cloning a tab duplicates those views and the complete split tree, preserving cursor, viewport, split-size, and focus state while continuing to refer to the same global buffers. Creating a new tab instead produces a single-window layout. Consequently, edits and undo/redo remain shared, while navigation state remains independent across windows and tabs. Only the active tab's layout is rendered.

The file tree is also owned by `bed-tui`. Its root, expanded paths, selection, and scroll offset are tab-local, while width is a session-wide UI preference. It reads directories through `std::fs`, stores exact `PathBuf` values for navigation, and converts only display labels lossily when necessary. Directories are traversed only after explicit expansion, and directory symbolic links are not recursively followed. The tree opens files through the same buffer/view path as `:edit`; it is not an editor buffer and does not affect undo history.

## Terminal backends

- `crates/bed-terminal/src/terminal/linux.rs` uses the Linux kernel terminal UAPI directly. The project declares only the `ioctl` entry point and the ABI structures/constants it uses.
- `crates/bed-terminal/src/terminal/windows.rs` uses the Win32 Console API directly. It reads structured UTF-16 input events and enables virtual-terminal output.
- `crates/bed-terminal/src/terminal/macos.rs` uses Darwin's native `termios` and `ioctl` ABI directly.
- `crates/bed-terminal/src/terminal/freebsd.rs` and `netbsd.rs` declare their respective native terminal ABIs separately.
- `crates/bed-terminal/src/terminal/vt.rs` decodes UTF-8 and VT input sequences for byte-stream terminals. It is not compiled into the Windows build.

`Key::Char(char)` is the platform boundary for typed text. VT backends also deliver bracketed paste as one `Key::Paste(String)` event, while the Windows console continues to provide structured character events. Native UTF-8 or UTF-16 fragments never enter the editing core.

## Supported targets

| Target | Backend | Status |
| --- | --- | --- |
| Linux x86_64 | Linux UAPI + VT input | Implemented and runtime-tested |
| Linux aarch64 | Linux UAPI + VT input | Implemented and cross-checked; runtime validation pending |
| Windows x86_64 | Win32 Console | Implemented, cross-checked, and runtime-tested |
| macOS x86_64/aarch64 | Darwin terminal ABI + VT input | Implemented and cross-checked; runtime validation pending |
| FreeBSD x86_64 | FreeBSD terminal ABI + VT input | Implemented and cross-checked; runtime validation pending |
| NetBSD x86_64 | NetBSD terminal ABI + VT input | Implemented and cross-checked; runtime validation pending |
| OpenBSD | OpenBSD terminal ABI + VT input | Planned; no prebuilt Rust standard library is available for cross-checking |

Unsupported targets fail at compile time instead of silently using an ABI from a different operating system.

## Dependencies

The workspace has four external dependencies, all in platform-independent domains outside bed's terminal-backend focus:

- `anyhow` supplies error context.
- `regex` provides bounded byte-oriented regular expressions for search and substitution.
- `unicode-segmentation` implements Unicode grapheme boundaries.
- `unicode-width` implements terminal display width.

There is no platform abstraction dependency. Terminal ABI declarations, mode handling, input translation, and restoration remain part of bed's own terminal layer.
