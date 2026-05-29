# mult

`mult` is a terminal UI for multiplexing project workspaces, `pi` agent chats, and per-workspace PTY terminals.

The current implementation is a Ratatui/Crossterm client plus a small `mult-server` process that owns long-lived PTYs. The client renders workspaces, chats, terminals, prompt overlays, status indicators, git branches, and terminal output while the server keeps terminal sessions alive across client reconnects.

## Current capabilities

- Multiple workspaces, each with a working directory, agent chats, and terminals.
- Persistent JSON project state for workspace/chat/terminal metadata and chat transcripts.
- Runtime PTY sessions managed by `mult-server` over a Unix socket.
- Shell terminals and command terminals.
- Auto-start for the selected terminal or selected `pi` chat.
- Per-chat `pi` processes using a bundled status extension.
- Terminal scrollback, paste handling, mouse wheel scrolling, mouse text selection, and OSC52 clipboard copy.
- Configurable project shortcuts and colorscheme.

## Quick start

With Nix:

```sh
nix develop
cargo build --workspace
just run
# before opening a PR or handing changes off:
just ci
```

Without Nix:

```sh
cargo build --workspace
cargo run
```

The client tries to autospawn `mult-server` when the `mult-server` binary is next to the `mult` binary. If that is not true in your workflow, start the server manually in another terminal:

```sh
just server
# or
cargo run --bin mult-server
```

## Useful commands

```sh
just run        # run the TUI client
just server     # run the persistent PTY server
just check      # cargo check --workspace --all-targets --all-features
just test       # cargo test --workspace --all-targets --all-features
just fmt        # format Rust, and flake.nix when nixpkgs-fmt exists
just fmt-check  # check Rust formatting without modifying files
just lint       # clippy with warnings denied
just audit      # cargo audit -D warnings (requires cargo-audit)
just ci         # strict local CI: fmt-check, lint, test, audit
just watch      # cargo-watch check/test loop
just nix-build  # nix build
just nix-check  # nix flake check
```

## Validation / CI

`just ci` is the local gate and intentionally fails if `cargo-audit` is missing. `nix develop` provides `just` and `cargo-audit`. GitHub Actions runs the same `just ci` gate on Linux and also runs `nix flake check` in a separate job.

The audit gate currently avoids known RustSec advisories by using `postcard` for local IPC framing and current `ratatui`/`tui-term` releases.

## Controls

Global controls when no prompt is open:

| Key | Action |
| --- | --- |
| `Ctrl+j` or `Ctrl+Enter` | Select next sidebar item |
| `Ctrl+k` | Select previous sidebar item |
| `Ctrl+a` | Add a new agent chat to the selected workspace |
| `Ctrl+t` | Add a new shell terminal to the selected workspace |
| `Ctrl+f` | Open/import a workspace |
| `Ctrl+p` | Open the command palette |
| `Ctrl+s` | Search the selected chat/terminal pane |
| `Ctrl+q` | Delete the selected chat/terminal, or an empty workspace |
| `Ctrl+Esc` | Quit |

Typing in a selected chat or terminal starts/focuses its PTY and forwards input to it.

Prompt controls:

| Key | Action |
| --- | --- |
| `Enter` | Submit |
| `Esc` or `Ctrl+c` | Cancel |
| `Backspace` | Delete one character |
| `Up`/`Down` or `Ctrl+k`/`Ctrl+j` | Move through prompt results where supported |

Mouse support:

- Scroll wheel scrolls the selected output pane.
- Drag over terminal/chat-agent output to select visible text and copy it through OSC52.
- `Ctrl+Shift+C` copies the active `mult` text selection when the terminal forwards that key to `mult`.

The command palette includes discoverable actions for focus changes, starting input, adding/deleting sessions, opening workspaces, search, clearing search, and quitting.

## Configuration

Config path:

- `$MULT_CONFIG_PATH`, if set
- otherwise `$XDG_CONFIG_HOME/mult/config.json`
- otherwise `~/.config/mult/config.json`

Example:

```json
{
  "pi_agent_command": "pi",
  "auto_start_pi_agent": true,
  "auto_start_terminals": true,
  "mouse_capture": true,
  "projects": [
    { "name": "mult", "path": "~/projects/mult" },
    ["scratch", "/tmp/scratch"]
  ],
  "colorscheme": {
    "base": "#232136",
    "text": "#e0def4",
    "iris": "#c4a7e7"
  }
}
```

`pi_agent_command` is launched through your login shell (`$SHELL -lc …`), so shell features — pipelines, `$VAR` expansion, globbing — work inside it. This is intentionally different from `MULT_AGENT_CMD` (below), which `mult` splits into arguments itself with no shell involved.

Environment variables:

| Variable | Purpose |
| --- | --- |
| `MULT_CONFIG_PATH` | Override config file path |
| `MULT_STATE_PATH` | Override state file path |
| `MULT_SOCKET_PATH` | Override `mult-server` Unix socket path |
| `MULT_SERVER_AUTOSPAWN=0` | Disable server autospawn |
| `MULT_AGENT_CMD` | Configure the experimental process-agent backend. Simple shell-style quotes and backslash escapes are supported; shell expansion is not performed. |

## State

State path:

- `$MULT_STATE_PATH`, if set
- otherwise `$XDG_DATA_HOME/mult/state.json`
- otherwise `~/.local/share/mult/state.json`

The state file is written atomically through an owner-only temporary file and final state files are set to `0600`. Newly-created state directories use owner-only permissions. Invalid JSON is moved aside with a `.corrupt-*` suffix before resetting to defaults; state files with a newer schema version are rejected without rewriting them.

Durable state contains the workspace tree, chat messages, terminal metadata, terminal launch commands, and statuses. PTY processes, raw terminal buffers, and scrollback are runtime/server-owned and are not stored in the JSON state file.

## Project layout

```text
src/main.rs              TUI event loop and runtime wiring
src/app.rs               app state, navigation, prompts, search, mutations
src/model.rs             durable project model and IDs
src/ui.rs                pure Ratatui rendering
src/pty.rs               client-side PTY runtime and server protocol adapter
src/bin/mult-server.rs   PTY server process
src/config.rs            config loading/defaults
src/storage.rs           state loading/saving
src/agent.rs             experimental process-agent backend
crates/protocol          shared client/server protocol types
extensions/mult-status.ts bundled pi status extension
```

## Runtime server and IPC

`mult-server` owns long-lived PTYs and communicates with clients over a Unix socket. See [docs/DAEMON.md](docs/DAEMON.md) for operational details, socket path selection, autospawn behavior, and security notes.

By default the socket lives at `$XDG_RUNTIME_DIR/mult.sock`; without `XDG_RUNTIME_DIR`, the fallback is a private `/tmp/mult-<uid>/mult.sock` directory. Server-created socket parents are mode `0700`, sockets are mode `0600`, and Linux builds verify Unix-socket peer credentials when clients connect.

## Platform notes

`mult` currently uses Unix sockets and Unix PTY/process APIs, so the practical target is Linux/macOS-like systems.

## Contributing

Contributions are welcome — start with [CONTRIBUTING.md](CONTRIBUTING.md) and the detailed [AGENTS.md](AGENTS.md) guide. For security issues, see [SECURITY.md](SECURITY.md). Planned follow-up work is tracked in [docs/REMAINING_WORK.md](docs/REMAINING_WORK.md).

## License

Licensed under either of:

- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)
