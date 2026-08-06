# Configuration reference

Complete reference for `config.json`. Every key, type and default below is taken
from [`src/config.rs`](../src/config.rs); the palette defaults come from the
`DEFAULT_COLOR_SCHEME` table, which is the single source the serialized defaults
and the renderer's fallback colours are both derived from.

The file is optional. A missing config means "use the defaults" — it is not an
error, *at the default location*. A path you named yourself (`--config` or
`$MULT_CONFIG_PATH`) has to exist: see [How the file is read](#how-the-file-is-read).

## Where the file is read from

In order of precedence:

1. `--config <PATH>`
2. `$MULT_CONFIG_PATH`
3. `$XDG_CONFIG_HOME/mult/config.json`
4. `~/.config/mult/config.json`

## How the file is read

`config.json` names commands that are run through your login shell and started
automatically, so whoever controls these bytes controls a process running as you.
The read is correspondingly strict — the file must be:

- a **regular file** (opened `O_NOFOLLOW`, so a symlink at the path is refused,
  not followed);
- **owned by you**;
- **not writable by group or others**;
- **at most 1 MiB** (`MAX_CONFIG_BYTES`).

Anything else is refused with `config error at <path>: …`.

"Not found" is treated as "use the defaults" **only for the default location**
(3 and 4 above). A path you gave explicitly with `--config` or
`$MULT_CONFIG_PATH` must exist; a missing one stops startup with
`config error at <path>: no such file (it was named explicitly, so the defaults
are not used)` and exit status 2. Falling back there is not neutral: the
built-in defaults auto-start `pi` and `claude` through your login shell, so one
mistyped character would run command lines your real config had turned off.

## Validation policy

Two rules, applied consistently:

- **A file that does not decode is a hard error.** Malformed JSON, an unknown
  key, or a value of the wrong type stops startup with
  `config error at <path>:<line>:<col>: <message>` on stderr and exit status 2.
  Unknown keys are rejected rather than ignored (`deny_unknown_fields`, at the
  top level *and* inside `colorscheme`), because `auto_start_terminal` —
  `auto_start_terminals` minus one letter — used to deserialize fine and do
  nothing.
- **A decodable file with a bad value warns and continues.** A `colorscheme`
  entry that is not `#rrggbb` keeps that key's default and is reported on the
  status line at startup:
  ``config: colorscheme.<key> is not a #rrggbb color (`<value>`); using the default``
  — an empty value is reported as `(empty)` rather than as empty backticks.
  A `projects[].path` that does not exist is checked lazily, when the
  open-workspace prompt shows it, and marked `(missing)` there.

"Reload config" in the command palette re-reads the same file the session started
from and applies it without restarting. A reload that fails reports on the status
line and keeps the config already running.

## Top-level keys

| Key | Type | Default | Effect |
| --- | --- | --- | --- |
| `pi_agent_command` | string | `"pi"` | Command for a `pi` chat (`Ctrl+a`). Run as `$SHELL -lc <command>`, so pipelines, `$VAR` expansion and globbing apply. An empty or whitespace-only value falls back to `pi`. |
| `claude_code_command` | string | `"claude"` | Command for a Claude Code chat (`Ctrl+x`), on the same `$SHELL -lc` terms. Falls back to `claude` when empty. |
| `auto_start_pi_agent` | bool | `true` | Start a selected `pi` chat when it gains focus, instead of waiting for a keystroke. |
| `auto_start_claude_code_agent` | bool | `true` | The same for Claude Code chats. |
| `auto_start_terminals` | bool | `true` | Start a selected terminal when it gains focus. |
| `mouse_capture` | bool | `true` | Ask the host terminal for mouse events (wheel scrolling and drag-to-select). Pushed to the terminal **at startup only** — changing it and reloading has no effect until the next start. |
| `clipboard_osc52` | bool | `true` | Whether a text selection is pushed to the system clipboard with OSC 52. Set `false` to keep selections inside `mult`: the payload is raw PTY output and OSC 52 hands it to the *host* terminal's clipboard. Selection and `Ctrl+Shift+C` keep working as a no-op rather than losing their bindings. |
| `projects` | array | `[]` | Project shortcuts offered by the open-workspace prompt (`Ctrl+f`). |
| `colorscheme` | object | Rosé Pine Moon | The twelve palette keys below. |

### `projects` entries

Each entry is either an object or a two-element array; the two forms are
equivalent.

```json
"projects": [
  { "name": "mult", "path": "~/projects/mult" },
  ["scratch", "/tmp/scratch"]
]
```

A path that does not resolve is still listed, marked `(missing)` — a project on
an unmounted share is a normal thing to have configured.

## `colorscheme` keys

All twelve are optional and independent; an omitted key keeps its default. Values
are `#rrggbb`, and a bare `rrggbb` without the `#` is accepted too. Surrounding
whitespace is ignored. The defaults are
[Rosé Pine Moon](https://rosepinetheme.com/palette/ingredients/).

Note the first row: the key is serialized as **`_nc`**, with a leading
underscore. `nc` is accepted as an alias when reading, but `_nc` is what a
generated file contains and what the error messages name.

| Key | Default | What it colours |
| --- | --- | --- |
| `_nc` (alias `nc`) | `#1f1d30` | The "not current" background — the whole sidebar or main pane while it does not have focus, so the focused one stands out against `base`. It is a pane background, not a per-row one: the selected sidebar row uses `highlight_med`. |
| `base` | `#232136` | The base background: the selected pane, the status line and prompt backgrounds. |
| `muted` | `#6e6a86` | De-emphasised text — hint and label text in the prompts, the status line's `ctrl-g` hint, the `·` glyph for an idle or already-seen item, and the keys column for a palette-only command. The most-used key in the UI. |
| `text` | `#e0def4` | Primary foreground text. |
| `love` | `#eb6f92` | Errors and danger: a failed chat, a terminal that exited non-zero or was signalled, the `x` status-line marker, delete confirmations. |
| `gold` | `#f6c177` | Warnings and "needs you": a chat waiting for an answer, the `!` status-line marker. |
| `pine` | `#3e8fb0` | Running state in the sidebar: a chat that is thinking, and a terminal whose command is still running. |
| `foam` | `#9ccfd8` | Headings and accents: the workspace icon and name in the sidebar, section headings in the help overlay, the search scope, the `·` info marker, and the text-selection highlight. |
| `iris` | `#c4a7e7` | The git branch icon and branch name on a workspace row. |
| `highlight_med` | `#44415a` | The selected-row background in the sidebar and in prompt lists. Under `NO_COLOR` this becomes reverse video instead. |
| `cursor` | `#ffffff` | The terminal cursor block and the prompt cursor. |
| `success` | `#3e8f54` | A terminal whose last run exited cleanly, and a finished chat. |

Text drawn on top of a user-supplied background is contrast-checked at draw time
(WCAG relative luminance, `src/ui/theme.rs`), so a very light `base` does not
render white-on-white.

### `NO_COLOR`

Setting `$NO_COLOR` to any non-empty value replaces every colour above with the
terminal's default and ignores the whole `colorscheme` block. The places that
carried meaning in a *background* — the sidebar selection, a selected prompt row,
the prompt cursor — switch to reverse video, and sidebar state is also carried by
glyph shape, so the UI stays navigable. There is no light-theme default: on a
light terminal, either set `NO_COLOR` or set the `colorscheme` keys.

## Full example

Every key at its default value. There is no need to write all of this — it is
here so the shape of each field is unambiguous.

```json
{
  "pi_agent_command": "pi",
  "claude_code_command": "claude",
  "auto_start_pi_agent": true,
  "auto_start_claude_code_agent": true,
  "auto_start_terminals": true,
  "mouse_capture": true,
  "clipboard_osc52": true,
  "projects": [
    { "name": "mult", "path": "~/projects/mult" },
    ["scratch", "/tmp/scratch"]
  ],
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

## Related

- Environment variables and CLI flags: [README](../README.md#environment-variables)
- When something does not work: [TROUBLESHOOTING.md](TROUBLESHOOTING.md)
- Why `config.json` is treated as a privilege boundary: [SECURITY.md](../SECURITY.md)
