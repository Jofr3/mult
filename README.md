# mult

`mult` is an early Ratatui prototype for an AI agent multiplexer: multiple workspaces, nested agent chats, and per-workspace terminals in one TUI.

## Quick start

```sh
nix develop
just run
```

Or without `just`:

```sh
cargo run
```

State is auto-saved to `$XDG_DATA_HOME/mult/state.json` or `~/.local/share/mult/state.json`. Override with `MULT_STATE_PATH=/path/to/state.json`. Chat transcripts are persisted with workspace state; terminal scrollback remains in-memory.

Configuration is loaded from `$XDG_CONFIG_HOME/mult/config.json` or `~/.config/mult/config.json`:

```json
{
  "pi_agent_command": "pi"
}
```

Use `MULT_CONFIG_PATH=/path/to/config.json` to point at another config file.

## Current controls

- `j`/`k` or arrows: move selection
- `o`: open/import workspace by directory path
- `w`: add scratch workspace
- `c`: add chat to selected workspace and start/focus its pi agent
- `p`: start/focus the selected chat's embedded pi agent
- `t`: add shell terminal to selected workspace
- `d`: add a command/dev-server terminal to selected workspace
- `s`: start the selected terminal PTY
- `x`: stop the selected terminal PTY or selected chat's pi agent
- `i`: focus terminal/pi input for the selected running PTY
- `r`: rotate selected chat/terminal status
- `D` or Delete: delete selected workspace, agent chat, or terminal
- `q`/Esc: quit

When the open/import or command prompt is active:

- Type a directory path or command
- Enter: submit it
- Esc or Ctrl-C: cancel

Delete actions show a confirmation prompt. Deleting a workspace also deletes its agent chats and terminals; deleting a chat or terminal stops its running PTY if needed.

When terminal or pi-agent input is focused, typing is sent to the PTY; press Esc to return to mult controls. Running PTYs are sized from the visible pane instead of a fixed 80x24 size. Chat panes run `pi` by default; set `pi_agent_command` in the config file to override it.

See `docs/PLAN.md` for the roadmap and `AGENTS.md` for contributor/agent guidance.
