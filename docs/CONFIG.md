# Configuration reference

Everything `mult` reads from `config.json`, with its type, default and effect.
Values here are checked against `src/config.rs` (defaults and deserialization)
and `src/ui.rs` (what each colour actually paints).

For environment variables see the table in [../README.md](../README.md#environment-variables);
for failure modes see [TROUBLESHOOTING.md](TROUBLESHOOTING.md).

## The file

`mult` picks the first of:

1. `$MULT_CONFIG_PATH`, used verbatim if set (including a relative path);
2. `$XDG_CONFIG_HOME/mult/config.json`, if `XDG_CONFIG_HOME` is absolute;
3. `~/.config/mult/config.json`, taking `~` from an absolute `$HOME` and
   otherwise from the effective user's passwd entry.

If none of those resolves, startup fails rather than writing into the current
directory. A **missing** file is not an error — `mult` starts on the built-in
defaults, so the whole file is optional. A file that exists but is not valid
JSON, or whose values have the wrong type, is a startup error.

The file also has to pass an ownership check before a byte of it is read, for a
sharper reason than state has: `pi_agent_command` and `claude_code_command` are
handed to `$SHELL -lc` and auto-started by default, so whoever controls those
bytes runs code as you without a keystroke — and both environment variables
above steer the path there. A config is read only when:

- it is a **regular file** reached **without traversing a symlink** — neither
  `config.json` itself nor any directory component above it may be one;
- it is **owned by you** and has exactly one hard link;
- its **directory is owned by you** and is not group- or other-writable;
- it is **under 1 MiB**.

The file's own mode is repaired rather than refused: a `0644` config is
`chmod`ed to `0600` as it is read, and only a mode that could not be tightened
fails. Anything else on that list is a startup error, not a quiet fall back to
the defaults — a rejected config is a signal.

**This refuses the usual dotfile-manager layout.** GNU stow and friends leave
`~/.config/mult/config.json` as a link into a repository, and such a config no
longer loads. See
[Too many levels of symbolic links](TROUBLESHOOTING.md#too-many-levels-of-symbolic-links-at-startup-symlinked-config)
for the messages and the workaround.

Current parsing behaviour worth knowing:

- **Every key is optional.** Any key you omit falls back to its default below.
- **Unknown keys are ignored silently.** There is no `deny_unknown_fields` yet,
  so a typo (`colorscheme.foreground`, `auto_start_pi`) is accepted and does
  nothing. Check spelling against this file. (Tracked as `E6` in
  [BACKLOG.md](BACKLOG.md).)
- **A colour that does not parse falls back to its default silently.** The parse
  failure is captured per key inside the renderer but nothing surfaces it yet
  (also `E6`). If a colour "doesn't take", check its syntax first.

## Top-level keys

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `pi_agent_command` | string | `"pi"` | The command run for a `pi` agent chat (`Ctrl+a`). Executed as `$SHELL -lc "<command>"`, so pipelines, `$VAR` expansion, globbing and aliases from your login shell all apply. |
| `claude_code_command` | string | `"claude"` | The command run for a Claude Code agent chat (`Ctrl+x`). Also executed through `$SHELL -lc`. |
| `auto_start_pi_agent` | bool | `true` | Whether selecting a `pi` chat starts its PTY immediately. With `false` the chat stays idle until you type into it or run **Start selected PTY**. |
| `auto_start_claude_code_agent` | bool | `true` | The same, for Claude Code chats. |
| `auto_start_terminals` | bool | `true` | Whether selecting a terminal starts it. This governs *shell* terminals; a persisted **command** terminal is never relaunched during restoration regardless of this setting — see [Command terminals are not auto-relaunched](TROUBLESHOOTING.md#a-pane-says-the-session-is-unavailable-after-restarting-the-daemon). |
| `mouse_capture` | bool | `true` | Whether `mult` puts the terminal into mouse-reporting mode at startup. `false` gives your emulator's native selection and scrollback back, at the cost of wheel scrolling, drag-selection and OSC 52 copy inside `mult`. |
| `clipboard_osc52` | bool | `true` | Whether a copied selection is written to the host terminal as an OSC 52 "set clipboard" escape. `false` stops `mult` from ever emitting one: drag-selection still highlights and `Ctrl+Shift+C` is still consumed, but nothing leaves for the clipboard. The escape carries the selected text to whatever is on the far end of the terminal — an SSH client, a multiplexer, an emulator that logs sequences — which is not always where pane contents should go. |
| `projects` | array | `[]` | Shortcuts offered by the **Open workspace** prompt (`Ctrl+f`). See below. |
| `colorscheme` | object | Rosé Pine Moon | Twelve colours. See below. |

### `projects`

Each entry may be written either as an object or as a two-element array; both
produce the same `{name, path}` pair:

```json
"projects": [
  { "name": "mult", "path": "~/projects/mult" },
  ["scratch", "/tmp/scratch"]
]
```

A leading `~` or `~/` in `path` is expanded from `$HOME`. If `$HOME` is unset the
path is used literally. Paths are not validated when the config is loaded; a
shortcut pointing at a directory that does not exist fails when you open it.

### `colorscheme`

Values are `#rrggbb` or `rrggbb` (case-insensitive, surrounding whitespace
trimmed). Anything else — a named colour, a 3-digit hex, an `rgb()` function —
does not parse and silently keeps the default for that key.

The defaults are Rosé Pine Moon and live in one place,
`config::DEFAULT_COLOR_SCHEME`; the renderer derives its compile-time fallbacks
from the same literals, so the values in this table cannot drift from the code
without failing the build.

| Key | Default | Effect |
| --- | --- | --- |
| `_nc` | `#1f1d30` | Background of **un**focused panes. Also used as the foreground drawn *on* `cursor` and on the `foam` selection highlight, so it should contrast with both. See the note on the name below. |
| `base` | `#232136` | Background of the **focused** pane, of every prompt/overlay, and of the cell under the terminal cursor. |
| `muted` | `#6e6a86` | Secondary text: field labels (`Command: `, `Search `), hints, help text in the command palette, workspace paths, and the status dot for an idle chat, a finished-and-seen chat, and a stopped or shell terminal. |
| `text` | `#e0def4` | Primary foreground for pane and overlay text. |
| `love` | `#eb6f92` | Errors and destructive intent: save-failure text, the delete-confirmation prompt, invalid prompt input, and the status dot of a failed chat. |
| `gold` | `#f6c177` | The active search query, and the status dot of a chat waiting on you. |
| `pine` | `#3e8fb0` | Work in progress: the status dot of a thinking chat and of a running command terminal. |
| `foam` | `#9ccfd8` | Workspace icon and name in the sidebar, the search-scope label, and the background of the mouse text selection. |
| `iris` | `#c4a7e7` | The git branch icon and branch name shown next to a workspace. |
| `highlight_med` | `#44415a` | Background of the selected sidebar row and of the selected row in list prompts (open-workspace, command palette). |
| `cursor` | `#ffffff` | The prompt cursor glyph (`▌`) and the background of the terminal's cursor cell. |
| `success` | `#3e8f54` | The status dot of a chat that finished while you were not looking, and of a command terminal that exited successfully and has not been seen. |

**Why `_nc`.** The key is serialized as `_nc` (with `nc` accepted as an alias, so
either spelling works when reading a config). It is the "not-current" colour —
the background of panes that do not have focus. Written as `_nc` it sorts to the
top of the object next to `base`, the colour it pairs with.

Only the keys you want to change need to be present:

```json
{
  "colorscheme": {
    "base": "#1e1e2e",
    "_nc": "#181825",
    "text": "#cdd6f4"
  }
}
```

Contrast is handled for you where it matters: text drawn on the `cursor` and
`foam` backgrounds keeps `_nc` while that stays legible (WCAG AA, 4.5:1) and
otherwise falls back to black or white. A light or inverted palette therefore
stays readable rather than washing out.

## A complete example

Every key, at its default value:

```json
{
  "pi_agent_command": "pi",
  "claude_code_command": "claude",
  "auto_start_pi_agent": true,
  "auto_start_claude_code_agent": true,
  "auto_start_terminals": true,
  "mouse_capture": true,
  "clipboard_osc52": true,
  "projects": [],
  "colorscheme": {
    "_nc": "#1f1d30",
    "base": "#232136",
    "muted": "#6e6a86",
    "text": "#e0def4",
    "love": "#eb6f92",
    "gold": "#f6c177",
    "pine": "#3e8fb0",
    "foam": "#9ccfd8",
    "iris": "#c4a7e7",
    "highlight_med": "#44415a",
    "cursor": "#ffffff",
    "success": "#3e8f54"
  }
}
```

## Not yet configurable

Frequently asked for, and deliberately absent today:

- **Keybindings.** The global keys listed in the README are fixed. A shared
  binding table is a prerequisite (`E4`/`F13`).
- **Scrollback size.** Retained history is a fixed per-pane budget in the daemon.
- **Clipboard mode.** `clipboard_osc52` is a switch, not a mode: there is no
  native-clipboard fallback and no read-from-clipboard path, so with it off copy
  does nothing at all rather than routing somewhere else.
- **Reload without restart.** The config is read once at startup (`E9`).
