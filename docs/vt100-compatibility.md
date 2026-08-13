# `bed-vt100` Compatibility Matrix

This matrix compares the behavior needed by bed with the public behavior of
`vt100 0.15.2`. It is a compatibility guide, not an API promise: bed owns its
emulator state and rendering model and does not expose the upstream crate's
snapshot or diff types.

| Area | `bed-vt100` | `vt100 0.15.2` | Decision |
| --- | --- | --- | --- |
| C0 controls, cursor, margins, insert/delete, scrolling | Supported | Supported | Shared terminal baseline |
| UTF-8, combining marks, wide graphemes | Supported | Supported | Shared baseline; bed tests split-input recovery |
| SGR colors and attributes | Supported, including dim/blink/hidden/strikethrough | Supported, with a smaller attribute surface | Bed extension |
| Primary/alternate screens | Supported | Supported | Shared behavior |
| Bounded primary scrollback | Supported | Supported | Bed exposes view-local history navigation |
| Application cursor/keypad and bracketed paste | Supported | Supported | Shared behavior |
| Mouse modes `1000/1002/1003/1006` | Supported | Supported | Bed also forwards native host mouse events |
| Mouse mode `9` | Not yet | Supported | Low-priority legacy compatibility |
| Mouse encoding `1005` | Not yet | Supported | Deliberately deferred; SGR `1006` is the bed path |
| OSC `0`/`2` title | Supported and bounded | Supported | Bed has one sanitized title model |
| OSC `1` icon name | Not modeled | Supported | No bed UI consumer currently exists |
| BEL and `ESC g` visual bell | Supported as poll feedback | Supported as parser diff state | Bed surfaces messages; never emits host controls |
| DEC private erase aliases `CSI ? J/K` | Supported | Supported | Shared behavior |
| Device status response (`CSI 6n`) | Supported | Supported | Required by interactive children |
| Formatted state/content diffs | Not exposed | Supported | Intentional API difference; TUI renders directly |
| Parser/session/PTY lifecycle | Supported in separate bed crates | Not provided | Bed extension |

Both implementations intentionally omit the full historical VT100 hardware
surface. Focus reporting, kitty keyboard protocols, `modifyOtherKeys`, and
other modern extensions are future compatibility work rather than regressions
against `vt100 0.15.2`.

The upstream crate is MIT licensed. `bed-vt100` is a clean-room implementation
with no copied or vendored upstream source; standards documentation and
behavioral tests are the compatibility references.
