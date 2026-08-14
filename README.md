# bed

> bad editor

`bed` is a suckless-style cross-platform terminal editor with a small Vim-like core. It owns its native terminal backends and uses small Unicode helpers for grapheme-safe editing and terminal display width.

See [ARCHITECTURE.md](ARCHITECTURE.md) for the backend boundary and platform matrix, and [TESTING.md](TESTING.md) for native and cross-target verification.

## Build and run

Install the released package from crates.io (the installed command is `bed`):

```sh
cargo install bad-editor
```

Or build it from source:

```sh
cargo build --release
target/release/bed path/to/file.txt
target/release/bed first.txt second.txt
target/release/bed path/to/directory
```

Linux x86_64 and Windows x86_64 are runtime-tested. Linux aarch64, macOS x86_64/aarch64, FreeBSD x86_64, and NetBSD x86_64 are cross-checked but still require native validation. OpenBSD is not yet supported.

Each file path opens in its own buffer. If a path does not exist, `bed` starts that buffer empty and creates the file on `:w`. Starting without a path opens the current directory in the file tree; passing a directory opens that directory directly.

## Keys

Normal mode:

- `[count] h/j/k/l` or arrows: move, optionally by a counted number of positions
- `[count] w/b/e`: move by Unicode-aware words
- `0` / `$`, `gg` / `G`: line start/end, file start/end
- `Ctrl-U` / `Ctrl-D` or `Page Up` / `Page Down`: scroll by half/full pages
- `i a I A`, `o O`: enter insert mode
- `x`, `dd`, `dw`, `d$`: delete character, line, word, or to line end
- `yy`, `yw`, `y$`, `p`, `P`: yank and put characterwise or linewise text
- `/PATTERN`, `n`, `N`: regular-expression search, repeat forward, repeat backward
- `v` / `V`: start character or line selection
- `u`, `Ctrl-R`: undo and redo
- `Ctrl-W` followed by `h`, `j`, `k`, `l`, or `w`: move between split windows
- `[count] Ctrl-W <` / `>`: decrease/increase the active window width
- `[count] Ctrl-W -` / `+`: decrease/increase the active window height
- `[count] Ctrl-W |` / `_`: set the active window width/height exactly; without a count, maximize it
- `Ctrl-W =`: equalize every split in the current tab page
- `gt` / `gT`: move to the next/previous tab page
- `[count]gt`: switch directly to the 1-based tab position
- `Tab` / `[count]Tab`: move to the next tab page or directly to a 1-based tab position
- `Shift-Tab` / `[count]Shift-Tab`: move to the previous tab page
- `:`: enter command mode

Insert mode:

- Type or paste to insert; `Tab`, `Enter`, `Backspace`, and `Delete` edit text
- `Escape` or `Ctrl-C`: return to normal mode

Visual modes:

- Use Normal-mode motions and counts to extend a character or line selection
- `y`: yank the selection; `d`, `x`, or `Delete`: delete it
- `p` / `P`: replace the selection with the register and retain the replaced text
- `v` / `V`: switch selection kind; repeat the active key, `Escape`, or `Ctrl-C` to leave Visual mode
- `:s/PATTERN/REPLACEMENT/[FLAGS]`: substitute on the selected lines

Visual commands are intentionally bed-specific. Entering `:` does not insert
Vim's `'<,'>` range markers, and an explicit `:%s` still addresses the whole
buffer.

Terminal Input mode:

- Keys and bracketed paste are sent to the child session.
- Mouse input over the focused live terminal is sent when the child enables mouse reporting.
- `Ctrl-\ Ctrl-N`: enter Terminal Normal mode.
- `Ctrl-\ Ctrl-W`: apply one bed window command.
- `Ctrl-\ Ctrl-\`: send a literal `Ctrl-\` to the child.

Terminal Normal mode:

- `h` / `j` / `k` / `l` or arrows: move a view-local navigation cursor through terminal cells; the viewport scrolls when needed.
- `0` / `$` or Home: move to the start/end of the current terminal row.
- Page keys, `Ctrl-U`, and `Ctrl-D`: move by full or half pages.
- `G` or End: return the navigation cursor to the live child cursor.
- `v`: begin a cell-based Terminal Visual selection.
- `V`: select complete logical terminal lines, joining their soft-wrapped rows.
- `i`, `a`, or Enter: return to Terminal Input and live output.
- `Ctrl-W`: apply one bed window command; `:` enters command mode.

Terminal Visual mode:

- `h` / `j` / `k` / `l` or arrows: extend the selection by terminal cells.
- `0` / `$` or Home / End: move to the start/end of the current terminal row.
- `v` / `V`: switch between cell and logical-line selection; repeating the active key cancels.
- `y`: copy into bed's characterwise or linewise register; `Escape` or `Ctrl-C` cancels.

Terminal modes are intentionally bed-specific and do not promise Vim terminal
compatibility.

Each terminal split shows the child's OSC title when available and otherwise
falls back to the spawned command. Child BEL events appear as a concise editor
message instead of being sent to the outer terminal.

Commands:

- `:w`: save unless the file changed externally; `:w!` explicitly overwrites it
- `:wall` or `:wa`: save every modified buffer; append `!` to explicitly overwrite external changes
- `:bnext` / `:bprevious` (`:bn` / `:bp`): switch buffers
- `:buffer N` or `:b N`: switch to buffer number `N`
- `:buffers` or `:ls`: list open buffers
- `:edit PATH` or `:e PATH`: open or switch to a path
- `:s/PATTERN/REPLACEMENT/[FLAGS]`: substitute on the current line, or on selected lines when entered from Visual mode
- `:%s/PATTERN/REPLACEMENT/[FLAGS]`: substitute throughout the buffer
- `:bdelete [N]` or `:bd [N]`: close a clean buffer; append `!` to discard its changes
- `:split [PATH]` / `:vsplit [PATH]`: split the active window horizontally/vertically, optionally opening a path in the new window
- `:close` / `:only`: close the active window or keep only that window in its tab page
- `:wincmd h|j|k|l|w`: move between windows
- `:resize N` / `:resize +/-N`: set or adjust the active window height
- `:vertical resize N` / `:vertical resize +/-N`: set or adjust its width
- `:tabnew [PATH]`: open a new tab page, optionally with a path
- `:tabclone`: clone the current layout and its view state into a new tab page
- `:tabrename [NAME]`: set a stable tab title; omit `NAME` to restore its automatic title
- `:tabnext [N]` / `:tabprevious`: switch tab pages, optionally selecting 1-based position `N`
- `:tabmove N`: move the current tab to 1-based position `N`
- `:tabclose` / `:tabonly`: close the current tab page or all other tab pages
- `:terminal [COMMAND]`: open a shell or command in a new terminal split
- `:terminals`: list terminal sessions by stable ID
- `:terminalattach ID`: attach another view to an existing session
- `:terminalclose ID`: remove an exited detached session; append `!` to terminate a running one
- `:q`: quit if no open buffer has unsaved changes
- `:q!`: discard all changes and quit
- `:wq` or `:x`: save the current buffer and quit if every buffer is clean
- `:wqall` or `:wqa`: save every buffer and quit

`Ctrl-S` also saves from normal mode.

Search and substitute patterns use the Rust `regex` crate's modern syntax.
Matching options are written in the pattern, such as `(?i)` for
case-insensitive matching. Substitute accepts `g` to replace every match on
each addressed line and `n` to count without changing the buffer. Replacement
captures use `$0`, `$1`, and `${name}`. `&` and backslash-number forms have no
special meaning. A substitute is committed as one undoable edit, and malformed
expressions never partially modify the buffer.

## Buffers, windows, and tabs

Buffers belong to the editor session rather than to a tab. Their text, modified state, and undo/redo history are therefore shared wherever the same buffer is visible. A window selects one buffer and retains an independent cursor and viewport for each buffer it has shown.

A tab is a lightweight workspace containing a split layout, its active window and focus history, a stable title, and file-tree navigation state. `:tabnew` inserts a single-window workspace immediately after the current tab; without a path it starts from the current buffer and view position. `:tabclone` instead copies the complete layout and view positions. Closing a tab returns to the most recently used surviving tab, and closing a window similarly returns to the most recently focused surviving window.

The tab line is always present above the editor area. Only the active tab is highlighted, `+` marks a tab whose visible buffers contain unsaved changes, and an overlong tab line scrolls to keep the active label visible. Automatic titles are captured when a tab is created and do not change merely because a window switches buffers.

## File tree

On terminals at least 40 columns wide, bed keeps a file tree at the left edge. Its initial root is the first startup directory, or the directory containing the first startup file, and the header shows that directory's name. Each tab retains its own root, expanded directories, selection, and scroll position; the panel width is a global UI preference shared by all tabs. The panel uses only the standard library filesystem API and keeps exact platform-native paths internally.

- `Ctrl-N`: enter or leave the file tree
- `Ctrl-W l` or `Ctrl-W w`: return to the previously active editor window
- `j` / `k` or arrows: move the selection
- `Enter` or `l`: enter `..`, expand a directory, or open a file in the active window
- `h`: enter `..`, collapse a directory, or select its parent
- `[count] Ctrl-W <` / `>`: decrease/increase the file tree width
- `[count] Ctrl-W |`: set its width exactly; without a count, maximize it
- `Ctrl-W =`: restore the default file tree width
- `r`: refresh the tree
- `Escape` or `q`: return to the editor
- `:tree [PATH]`: focus the tree and optionally change its root
- `:treewidth N`: set the file tree width to at least 10 columns
- `:treerefresh`: refresh without changing focus

The sidebar is hidden automatically on narrower terminals so the editing area remains usable.
