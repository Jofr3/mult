# mult

`mult` is an early Ratatui prototype for an AI agent multiplexer: multiple workspaces, nested agent chats, and per-workspace terminals in one TUI.

## Quick start

```sh
nix develop
just server   # keep this running in one terminal during development
just run      # start/discard TUI clients in another terminal
```

Or without `just`:

```sh
cargo run --bin mult-server
cargo run
```

Installed `mult` clients autospawn `mult-server` if the socket is missing, but the recommended long-lived setup is a systemd user service; see `docs/DAEMON.md`.

State is auto-saved to `$XDG_DATA_HOME/mult/state.json` or `~/.local/share/mult/state.json`. Override with `MULT_STATE_PATH=/path/to/state.json`. Saved state files are written with owner-only permissions. Chat transcripts, terminal definitions, and running/restorable chat-terminal status are persisted with workspace state; terminal scrollback remains in-memory.

Configuration is loaded from `$XDG_CONFIG_HOME/mult/config.json` or `~/.config/mult/config.json`:

```json
{
  "pi_agent_command": "pi",
  "auto_start_pi_agent": true,
  "auto_start_terminals": true,
  "colorscheme": {
    "_nc": "#1f1d30",
    "base": "#232136",
    "surface": "#2a273f",
    "overlay": "#393552",
    "muted": "#6e6a86",
    "subtle": "#908caa",
    "text": "#e0def4",
    "love": "#eb6f92",
    "gold": "#f6c177",
    "rose": "#ea9a97",
    "pine": "#3e8fb0",
    "foam": "#9ccfd8",
    "iris": "#c4a7e7",
    "leaf": "#95b1ac",
    "highlight_low": "#2a283e",
    "highlight_med": "#44415a",
    "highlight_high": "#56526e"
  }
}
```

Use `MULT_CONFIG_PATH=/path/to/config.json` to point at another config file. `auto_start_pi_agent` and `auto_start_terminals` default to `true`; set either to `false` if you want panes to wait for manual start. The default colorscheme is Rosé Pine Moon; any color key can be overridden with a `#rrggbb` value.

## Current controls

- `Enter`: focus the selected chat/terminal pane from the sidebar
- `Esc`: return focus to the sidebar
- Mouse wheel over a chat/terminal output pane: scroll that pane without moving the outer terminal history
- `j`/`k` or `Up`/`Down`: move selection when the sidebar is focused, or scroll the focused output pane by one line
- `PageUp`/`PageDown`: page the focused chat/terminal output pane
- `Home`/`End`: jump the focused chat/terminal output pane to top/bottom
- `n` then `a`: add agent to selected workspace and start/focus its pi agent
- `n` then `t`: add shell terminal to selected workspace
- `n` then `c`: add a command/dev-server terminal to selected workspace
- `n` then `w`: open/import workspace by directory path
- `d` then `d`: delete selected workspace, agent chat, or terminal immediately
- `i`: start/focus terminal or pi-agent input for the selected pane
- `q`: quit

When the open/import or command prompt is active:

- Type a directory path or command
- Enter: submit it
- Esc or Ctrl-C: cancel

Deleting a workspace also deletes its agent chats and terminals; deleting a chat or terminal stops its running PTY if needed.

Focus controls whether the sidebar or selected main pane is active; the inactive pane uses a darker background. Input mode is only for typing into a terminal or pi agent; press Esc to return to normal mode for keyboard scrolling. Mouse wheel scrolling works over the pane under the cursor. Esc does not quit mult. Running PTYs are sized from the visible pane instead of a fixed 80x24 size. Chat panes run `pi` by default; set `pi_agent_command` in the config file to override it.

See `docs/PLAN.md` for the roadmap and `AGENTS.md` for contributor/agent guidance.
