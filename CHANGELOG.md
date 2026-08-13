# Changelog

All notable changes to bed are documented in this file.

## [Unreleased]

### Added

- Independently maintained `bed-vt100`, native PTY/ConPTY process backends,
  bounded terminal sessions, and embedded terminal splits.
- Terminal Input and Terminal Normal modes with explicit child-input escape,
  window commands, and view-local scrollback navigation.
- Terminal session listing, reattachment, and explicit process cleanup commands.
- Cell-based terminal selection and register copying across scrollback, wide
  graphemes, and soft-wrapped rows.

## [0.3.0] - 2026-08-13

### Added

- Character and line Visual modes with counted motions, grapheme-safe selection, yank, delete, and replacement.
- Selected-line substitution through Visual `:s`, while explicit `:%s` continues to address the complete buffer.
- Block cursors outside Insert mode and a bar cursor while inserting.

### Changed

- Cache document line starts for navigation, selection, substitution, and rendering of large buffers.

### Fixed

- Clamp view-local cursors and selection anchors when another view shortens a shared buffer.
- Restore the terminal-selected default cursor shape on normal exit and panic unwinding.

## [0.2.1] - 2026-08-13

### Fixed

- Refuse to overwrite files changed, created, or deleted externally after opening; `:w!` and `:wall!` explicitly override conflicts.
- Reuse one buffer for relative, absolute, normalized, and symbolic-link aliases of the same path.
- Start without arguments or from a directory in the file tree instead of failing or treating the directory as a document.
- Bound full-copy undo and redo snapshots by both entry count and total memory.

## [0.2.0] - 2026-08-12

### Added

- Modern regular-expression search through `/`, `n`, and `N`.
- Current-line and whole-buffer substitution with capture expansion, global replacement, count-only mode, and one-step undo.

## [0.1.0] - 2026-08-12

### Added

- Cross-platform modal text editing with Unicode grapheme-aware movement and editing.
- Multiple buffers, split windows, tab pages, and a navigable file tree.
- Bounded undo and redo history, search, and Vim-inspired motions.
- Native terminal backends for Linux, Windows, macOS, FreeBSD, and NetBSD.
- Native runtime coverage on Linux x86_64 and Windows x86_64, with cross-target checks for the other supported targets.

[Unreleased]: https://github.com/softfault/bed/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/softfault/bed/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/softfault/bed/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/softfault/bed/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/softfault/bed/releases/tag/v0.1.0
