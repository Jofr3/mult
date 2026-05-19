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

State is auto-saved to `$XDG_DATA_HOME/mult/state.json` or `~/.local/share/mult/state.json`. Override with `MULT_STATE_PATH=/path/to/state.json`.

## Current controls

- `j`/`k` or arrows: move selection
- `o`: open/import workspace by directory path
- `w`: add scratch workspace
- `c`: add chat to selected workspace
- `t`: add shell terminal to selected workspace
- `d`: add a command/dev-server terminal to selected workspace
- `s`: start the selected terminal PTY
- `x`: stop the selected terminal PTY
- `i`: focus terminal input for the selected running PTY
- `r`: rotate selected chat/terminal status
- `q`/Esc: quit

When the open/import or command prompt is active:

- Type a directory path or command
- Enter: submit it
- Esc or Ctrl-C: cancel

When terminal input is focused, typing is sent to the PTY; press Esc to return to mult controls. Running PTYs are sized from the visible terminal pane instead of a fixed 80x24 size.

See `docs/PLAN.md` for the roadmap and `AGENTS.md` for contributor/agent guidance.
# mult
