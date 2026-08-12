# Configuration reference

Everything `mult` reads from `config.json`, with its type, default and effect.
Values here are checked against `src/config.rs` (defaults and deserialization)
and `src/ui.rs` (what each colour actually paints).

For environment variables see the table in [../README.md](../README.md#environment-variables);
for failure modes see [TROUBLESHOOTING.md](TROUBLESHOOTING.md).

## The file

`mult` picks the first of:

1. `--config <path>`, used verbatim (including a relative path);
2. `$MULT_CONFIG_PATH`, used verbatim if set;
3. `$XDG_CONFIG_HOME/mult/config.json`, if `XDG_CONFIG_HOME` is absolute;
4. `~/.config/mult/config.json`, taking `~` from an absolute `$HOME` and
   otherwise from the effective user's passwd entry.

That order is the general rule for every path `mult` takes: **flag > environment
variable > default**. `mult --help` lists the three (`--config`, `--state`,
`--socket`), and both `--config <path>` and `--config=<path>` are accepted.

If none of those resolves, startup fails rather than writing into the current
directory. A **missing** file is not an error — `mult` starts on the built-in
defaults, so the whole file is optional.

The file also has to pass an ownership check before a byte of it is read, for a
sharper reason than state has: `pi_agent_command` and `claude_code_command` are
handed to `$SHELL -lc` and auto-started by default, so whoever controls those
bytes runs code as you without a keystroke — and both environment variables
above steer the path there. A config is read only when:

Symlinks are **resolved first**, and every check below is then applied to the
file the path resolved *to*:

- it is a **regular file**;
- it is **owned by you** and has exactly one hard link;
- its **directory is owned by you** and is not group- or other-writable;
- it is **under 1 MiB**.

The file's own mode is repaired rather than refused: a `0644` config is
`chmod`ed to `0600` as it is read, and only a mode that could not be tightened
fails. Anything else on that list is a startup error, not a quiet fall back to
the defaults — a rejected config is a signal. A link that points at nothing
counts as a missing file, so it means defaults rather than an error.

**The usual dotfile-manager layout works.** `home-manager`, GNU stow and
chezmoi all leave `~/.config/mult` or `~/.config/mult/config.json` as a link
into a repository; `mult` follows it and checks the repository copy. What it
still refuses is a config whose *resolved* directory anyone else can write, so
a link aimed into `/tmp` gains nothing. The property being enforced is that the
bytes came from a file only you can write, not that no link was traversed —
`SECURITY.md` has the reasoning. See
[the config is refused at startup](TROUBLESHOOTING.md#the-config-is-refused-at-startup)
for the messages.

## What is an error and what is a warning

The rule is whether `mult` can still act on the file as written.

**Startup errors** — `mult` prints one line and exits non-zero, having changed
nothing:

- the file is not valid JSON;
- a value has the wrong type (`"mouse_capture": "yes"`);
- a key is not one `mult` knows. `deny_unknown_fields` is on for the top-level
  object, for `colorscheme`, and for a `projects` entry, so a typo like
  `auto_start_terminal` (no `s`) or `colorscheme.foreground` fails instead of
  being accepted and doing nothing;
- the file, or the directory holding it, fails one of the ownership checks above.

Parse errors name the file and the position, and the accepted keys are listed:

```
mult: config error at /home/you/.config/mult/config.json:2:23: unknown field
`auto_start_terminal`, expected one of `pi_agent_command`, `claude_code_command`,
`file_manager_command`, `auto_start_pi_agent`, `auto_start_claude_code_agent`,
`auto_start_terminals`, `mouse_capture`, `clipboard_osc52`, `projects`,
`colorscheme`
```

**Warnings** — `mult` starts, uses a documented fallback, and tells you. Each
warning is printed to stderr at startup and shown in the app's notice area:

- a colour that does not parse (`"text": "blue"`) keeps the built-in default for
  that key; the rest of the scheme is unaffected;
- a `projects` entry whose `path` is not a directory right now, or whose `name`
  is empty. The shortcut is still offered — the directory may simply not be
  mounted yet — and fails if you open it.

Nothing is ignored silently. If you set something and nothing happens, `mult`
either refused to start or said so.

Also worth knowing: **every key is optional.** Any key you omit falls back to its
default below.

## Top-level keys

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `pi_agent_command` | string | `"pi"` | The command run for a `pi` agent chat (`Ctrl+a`). Executed as `$SHELL -lc "<command>"`, so pipelines, `$VAR` expansion, globbing and aliases from your login shell all apply. |
| `claude_code_command` | string | `"claude"` | The command run for a Claude Code agent chat (`Ctrl+x`). Also executed through `$SHELL -lc`. |
| `file_manager_command` | string | `"yazi"` | The command run by **Open file manager** (`Ctrl+n`), in the selected workspace's root directory. Also executed through `$SHELL -lc`. The workspace keeps one file manager pane: `Ctrl+n` reuses the terminal whose command matches this key before adding another. |
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

Both spellings reject unknown keys, so `{"name": "mult", "pathh": "…"}` is a
startup error rather than a shortcut that silently has no path.

A leading `~` or `~/` in `path` is expanded from `$HOME`. If `$HOME` is unset the
path is used literally. The expanded path is checked once, at load: a shortcut
pointing at something that is not a directory is reported as a warning and kept,
and it fails if you open it. The check is a snapshot — a directory that appears
(or disappears) later is not re-checked until the next start or config reload.

### `colorscheme`

Values are `#rrggbb` or `rrggbb` (case-insensitive, surrounding whitespace
trimmed). Anything else — a named colour, a 3-digit hex, an `rgb()` function —
does not parse, keeps the default for that key, and is reported as a warning
naming the key and the value you wrote.

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
| `love` | `#eb6f92` | Errors and destructive intent: save-failure text, invalid prompt input, and the status dot of a failed chat. |
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
  "file_manager_command": "yazi",
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
