# Testing

This document defines the validation procedure for supported bed targets. Cross-target checks validate compilation and platform-specific Rust code; native tests validate operating-system behavior. Both are required before a backend is considered runtime-tested.

## Baseline

Run the following commands on the native host:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --release --locked
```

On Linux, macOS, FreeBSD, and NetBSD, `cargo test` includes a native pseudoterminal test. It covers raw-mode entry, idle resize detection, UTF-8 input and saving, normal restoration, default cursor-shape restoration, and restoration during panic unwinding.

## Cross-Target Checks

Install each available target with `rustup target add <target>`, then run:

```sh
cargo check --workspace --all-targets --target <target>
cargo clippy --workspace --all-targets --target <target> -- -D warnings
```

The current cross-check matrix is:

| Target |
| --- |
| `aarch64-unknown-linux-gnu` |
| `x86_64-pc-windows-gnu` |
| `x86_64-apple-darwin` |
| `aarch64-apple-darwin` |
| `x86_64-unknown-freebsd` |
| `x86_64-unknown-netbsd` |

A cross-target check does not execute the resulting binary or validate the host terminal API.

## Native Terminal Check

Build the release binary and open a temporary file in a real interactive terminal. Do not redirect standard input or output.

Verify the following behavior:

1. bed enters the alternate screen and displays line numbers.
2. Insert `ASCII 你好 👩🏽‍💻`; cursor movement and deletion preserve the complete emoji grapheme.
3. Paste multiple lines containing a tab and the same emoji in insert mode; one undo removes the complete paste, and pasting in normal mode does not execute its text as commands.
4. Exercise arrows, Home, End, Page Up, Page Down, and terminal-supported Shift/Ctrl/Alt navigation combinations.
5. Exercise word motions, delete/yank operators, regex search, `n`, `N`, `u`, and `Ctrl-R`. Search with a capture, character class, and `(?i)` modifier; an invalid pattern must report an error without replacing the previous repeatable search. Run `:%s/(?P<key>[a-z]+)=([0-9]+)/$2:${key}/g`, a current-line substitution without `g`, and a count-only substitution with `n`; verify capture expansion, malformed-pattern rejection, and one-step undo.
6. Enter character Visual mode with `v` and line Visual mode with `V`. Exercise motions, counts, reverse selection, Unicode graphemes, `y`, `d`/`x`, and `p`/`P`. Confirm `:s` affects selected lines without displaying Vim range markers, while explicit `:%s` still affects the complete buffer. An empty register must leave the selection active.
7. Open two buffers. Verify `:bn`, `:bp`, `:b N`, `:ls`, `:bd`, and that each buffer restores its cursor and scroll position.
8. Exercise horizontal and vertical splits, including `:split PATH` and `:vsplit PATH`. Verify `Ctrl-W h/j/k/l/w`, counted relative resizing, exact resizing with `|` and `_`, `Ctrl-W =`, `:resize`, `:vertical resize`, `:close`, `:only`, independent cursors, view-local selections, shared edits when two windows show one buffer, and return to the most recently focused surviving window after `:close`.
9. Create at least three tab pages. Verify that `:tabnew [PATH]` inserts after the current tab with one window; `:tabclone` preserves the complete layout, split sizes, cursors, selections, and viewports; and edits remain shared between cloned views. Check `Tab`, `[count]Tab`, `Shift-Tab`, `[count]Shift-Tab`, `gt`, `gT`, `[count]gt`, `:tabnext N`, `:tabmove N`, `:tabclose`, and `:tabonly`. Closing a tab must return to the most recently used surviving page, while `Tab` in Insert mode must continue to insert a tab character.
10. Rename a tab with `:tabrename NAME`, switch its active window to another buffer, and confirm that the title remains stable. Clear the name with `:tabrename`, verify the automatic title returns, modify a visible buffer to produce `+`, and narrow the terminal until the tab line scrolls while retaining the active label. Only the focused tab may be highlighted.
11. Exercise the file tree with nested directories and non-ASCII names. Verify that its header follows the current root directory name, use `Ctrl-W h/l/w` to move between it and the editor, navigate upward with `..`, adjust and reset its width, set an exact `:treewidth`, expand, collapse, refresh, change its root with `:tree`, and open a file into the active window. Give two tabs different roots and selections; switching tabs must restore each tree while retaining one global panel width.
12. Modify both buffers. Verify that hidden changes block `:q`, then save them with `:wall`.
13. Resize the terminal while bed is idle; all windows redraw without another key press, and the file tree disappears below its narrow-terminal threshold.
14. Confirm Normal and Visual modes use a block cursor, Insert mode uses a bar cursor, and command/search/tree modes return to a block cursor.
15. Exit with `:q`; each saved file contains the exact UTF-8 text and line structure entered above.
16. Confirm that the original screen, cursor visibility, default cursor shape, bracketed-paste mode, input echo, and canonical line input are restored.

Windows validation must use a native Windows console session because redirected CI streams do not provide the `ReadConsoleInputW` behavior used by bed. The Unix-like backends likewise require a native run in addition to their PTY test before runtime support is recorded.

Record the operating-system version, architecture, terminal application, shell, and any failed step when reporting a native result.
