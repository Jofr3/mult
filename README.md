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

Installed `mult` clients autospawn `mult-server` if the socket is missing. The autospawned server is detached from the client terminal, so running panes can survive closing and reopening the `mult` client. For persistence across full logouts/restarts, the recommended long-lived setup is a systemd user service; see `docs/DAEMON.md`.

State is auto-saved to `$XDG_DATA_HOME/mult/state.json` or `~/.local/share/mult/state.json`. Override with `MULT_STATE_PATH=/path/to/state.json`. Saved state files are written with owner-only permissions. Chat transcripts, terminal definitions, and running/restorable chat-terminal status are persisted with workspace state; terminal scrollback remains in-memory. If state JSON is corrupt, mult moves it aside to a `*.corrupt-*` backup and starts from default state.

Configuration is loaded from `$XDG_CONFIG_HOME/mult/config.json` or `~/.config/mult/config.json`:

```json
{
  "pi_agent_command": "pi",
  "auto_start_pi_agent": true,
  "auto_start_terminals": true,
  "mouse_capture": true,
  "projects": [
    { "name": "mult", "path": "~/projects/mult" },
    ["docs", "~/projects/docs"]
  ],
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

Use `MULT_CONFIG_PATH=/path/to/config.json` to point at another config file. `projects` is optional; when present, `Ctrl-F` opens a fuzzy project picker searched by project name and then imports the configured path. Entries may be objects (`{"name":"mult","path":"~/projects/mult"}`) or `["name","path"]` pairs. `auto_start_pi_agent` and `auto_start_terminals` default to `true`; set either to `false` if you want panes to wait for manual start. `mouse_capture` defaults to `true` so the left sidebar stays visible while mouse wheel scrolling and pane-local drag selection both work. Dragging in the selected chat/terminal pane highlights text and copies it through OSC 52 on release. Set `mouse_capture` to `false` to disable app mouse handling and fall back to your terminal emulator's native selection. The default colorscheme is Rosé Pine Moon; any color key can be overridden with a `#rrggbb` value.

## Current controls

mult is always in input mode: ordinary keys go to the selected terminal or agent PTY. Workspace actions use Ctrl chords:

- `Ctrl-J`: navigate down
- `Ctrl-K`: navigate up
- `Ctrl-Q`: delete the selected agent chat, terminal, or command terminal immediately; closing the last item under a workspace closes the workspace too
- `Ctrl-Esc`: quit mult
- `Ctrl-A`: add an agent chat to the selected workspace and start/focus its pi agent
- `Ctrl-T`: add a shell terminal to the selected workspace
- `Ctrl-F`: open/import a workspace; uses the configured fuzzy project list when `projects` is set, otherwise prompts for a directory path
- Drag within the selected chat/terminal pane: select visible pane text and copy it on release
- Mouse wheel over a chat/terminal output pane: scroll that pane without moving the outer terminal history

When the open/import or command prompt is active:

- Type a configured project name, directory path, or command
- Up/Ctrl-K and Down/Ctrl-J: select a configured project match
- Enter: submit it
- Esc or Ctrl-C: cancel

Workspace headers are labels, not selectable panes. Deleting a chat or terminal stops its running PTY if needed, and deleting the last item under a workspace closes that workspace too.

The selected chat/terminal pane receives keyboard input directly. If its PTY is not running, typing into it starts the PTY first. Mouse wheel scrolling works over the pane under the cursor. Running PTYs are sized from the visible pane instead of a fixed 80x24 size. Chat panes run `pi` by default; set `pi_agent_command` in the config file to override it.

See `docs/PLAN.md` for the roadmap and `AGENTS.md` for contributor/agent guidance.
