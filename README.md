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
- `t`: add terminal to selected workspace
- `s`: start a PTY shell for the selected terminal
- `x`: stop the selected terminal PTY shell
- `r`: rotate selected chat/terminal status
- `q`/Esc: quit

When the open/import prompt is active:

- Type a directory path
- Enter: import it
- Esc or Ctrl-C: cancel

PTY shells currently stream output only; focused terminal input routing is the next M2 step.

See `docs/PLAN.md` for the roadmap and `AGENTS.md` for contributor/agent guidance.
# mult
