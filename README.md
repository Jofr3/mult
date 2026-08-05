# mult

`mult` is a terminal UI for multiplexing project workspaces, `pi` agent chats, and per-workspace PTY terminals.

The current implementation is a Ratatui/Crossterm client plus a small `mult-server` process that owns long-lived PTYs. The client renders workspaces, chats, terminals, prompt overlays, status indicators, git branches, and terminal output while the server keeps terminal sessions alive across client reconnects.

## Current capabilities

- Multiple workspaces, each with a working directory, agent chats, and terminals.
- Persistent JSON project state for workspace/chat/terminal metadata.
- Runtime PTY sessions managed by `mult-server` over a Unix socket.
- Shell terminals and command terminals.
- Auto-start for the selected terminal or selected agent chat.
- Two agent backends per chat — `pi` (via a bundled status extension) and
  Claude Code (via generated lifecycle hooks) — chosen when the chat is created,
  shown in the sidebar as `agent: pi` / `agent: cc`, and both reporting live
  status into the sidebar dot.
- Terminal scrollback, paste handling, mouse wheel scrolling, mouse text selection, and OSC52 clipboard copy.
- Configurable project shortcuts and colorscheme, reloadable without restarting.
- A dismissible status line for runtime problems that belong to no pane — a daemon that could not be reached, a state file that could not be saved, a config warning.
- A `?`/`F1` keybinding overlay generated from the same table as the command palette, a confirmation step in front of destructive deletes, readline-style prompt editing, and sidebar status glyphs that differ by shape as well as colour.

## Install

### Release archives

Prebuilt archives are attached to each [GitHub Release](https://github.com/Jofr3/mult/releases)
for `x86_64` Linux (gnu and musl) and macOS (`x86_64` and `aarch64`):

```sh
tar xzf mult-<version>-<target>.tar.gz
cd mult-<version>-<target>
install -m 755 mult mult-server ~/.local/bin/
```

**Install both binaries, into the same directory.** `mult` autospawns
`mult-server` from a path next to its own binary, and refuses to execute one that
is not a regular file owned by you (or root) with no group/other write bits. With
only `mult` installed, or with the two in different directories, you have to
start the daemon yourself every time.

Verify a download against the `SHA256SUMS` file published with the release.

### With cargo

```sh
cargo install --git https://github.com/Jofr3/mult --locked mult
```

That builds and installs both binaries into `~/.cargo/bin`, which satisfies the
adjacency requirement above. `--locked` builds from the committed lockfile.

### With Nix

```sh
nix run github:Jofr3/mult              # the client
nix run github:Jofr3/mult#server       # the daemon
nix profile install github:Jofr3/mult  # both, persistently
```

### From source

```sh
git clone https://github.com/Jofr3/mult && cd mult
cargo build --release --workspace
# binaries land together in target/release/
```

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

## Command line

Both binaries take the same shape: options only, no positional arguments. An unrecognised option is an error (exit status 2), never a silently ignored flag.

```text
mult [--config <PATH>] [--state <PATH>] [--socket <PATH>] [--help] [--version]
mult-server [--socket <PATH>] [--help] [--version]
```

| Option | Applies to | Purpose |
| --- | --- | --- |
| `-h`, `--help` | both | Print usage and exit |
| `-V`, `--version` | both | Print the version and exit |
| `--config <PATH>` | `mult` | Config file to read |
| `--state <PATH>` | `mult` | State file to read and write |
| `--socket <PATH>` | both | `mult-server` Unix socket to use |

`--config=<PATH>` and `--config <PATH>` are both accepted; a repeated option takes its last value. `mult-server` owns neither a config nor a state file, so it rejects `--config`/`--state` rather than accepting a flag that would do nothing.

**Precedence:** a flag beats the matching environment variable (`MULT_CONFIG_PATH`, `MULT_STATE_PATH`, `MULT_SOCKET_PATH`), which beats the default path.

Exit status: `0` success, `2` an unusable command line or an unreadable config/state file, `1` a session that started and then failed.

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
just check      # cargo check --locked --workspace --all-targets --all-features
just test       # cargo test --locked --workspace --all-targets --all-features
just fmt        # format Rust, and flake.nix when nixpkgs-fmt exists
just fmt-check  # check Rust formatting without modifying files
just lint       # clippy with warnings denied
just deny       # cargo deny check (requires cargo-deny)
just typecheck  # typecheck the bundled status extension (skipped without npm)
just version-check  # the release version agrees across Cargo.toml, flake.nix, package.json
just coverage   # cargo llvm-cov summary with a regression floor
just coverage-html  # browsable coverage report
just fuzz-build # build the fuzz targets (needs nightly + cargo-fuzz)
just install-hooks  # install a pre-commit hook running cargo fmt --check
just ci         # strict local CI: fmt-check, version-check, lint, test, deny, typecheck
just watch      # cargo-watch check/test loop
just nix-build  # nix build
just nix-check  # nix flake check
```

## Validation / CI

`just ci` is the local gate: `fmt-check`, `version-check`, `lint`, `test`, `deny`, and `typecheck`. It intentionally fails if `cargo-deny` is missing; `nix develop` provides `just` and `cargo-deny`. The extension typecheck skips with a notice when `npm` or `extensions/node_modules` is unavailable, so the gate still runs offline.

GitHub Actions runs the same `just ci` gate on **both Linux and macOS**, plus separate jobs for:

| Job | What it does |
| --- | --- |
| `msrv` | `cargo +1.88 check --workspace --locked --all-targets --all-features`, so the declared `rust-version` is tested rather than asserted |
| `deny` | `cargo deny check` — advisories, licenses, banned/duplicate crates, sources |
| `coverage` | `cargo llvm-cov`, with a floor set below the current number as a regression guard |
| `fuzz` | Builds both fuzz targets and runs a 60s smoke pass each |
| `extension` | Typechecks the bundled status extension |
| `nix` | `nix flake check` |

Supply-chain checks also run on a **weekly schedule**, so a new RustSec advisory filed against a dependency already in `Cargo.lock` is caught without waiting for someone to open a PR. `cargo deny check` is the single gate for this: it covers the RustSec database along with licences and sources, and the `cargo audit` step that used to run beside it only duplicated a subset.

The dependency tree currently carries no known RustSec advisories: local IPC framing uses `postcard`, and `ratatui`/`tui-term` are on current releases.

## Controls

Global controls when no prompt is open:

| Key | Action |
| --- | --- |
| `Ctrl+j` or `Ctrl+Enter` | Select next sidebar item |
| `Ctrl+k` | Select previous sidebar item |
| `Ctrl+a` | Add a new `pi` agent chat to the selected workspace |
| `Ctrl+x` | Add a new Claude Code agent chat to the selected workspace |
| `Ctrl+t` | Add a new shell terminal to the selected workspace |
| `Ctrl+f` | Open/import a workspace |
| `Ctrl+p` | Open the command palette |
| `Ctrl+s` | Search the selected chat/terminal pane |
| `Ctrl+q` | Delete the selected chat/terminal, or an empty workspace (asks first) |
| `Ctrl+g` | Dismiss the current status-line message (only when one is shown; otherwise it goes to the focused PTY) |
| `F1` | Show the keybinding overlay |
| `?` | Show the keybinding overlay, when no chat/terminal is selected to receive the key |
| `Ctrl+Esc` | Quit — the one binding that is *not* prompt-gated; it is checked before any prompt sees the key, so it also closes a prompt or the help overlay |

Typing in a selected chat or terminal starts/focuses its PTY and forwards input to it. `F1` is the one key `mult` keeps for itself and never forwards; a bare `?` goes to the selected chat/terminal whenever one is selected — starting its PTY if it is not running — so it only opens help when nothing is selected. The overlay is also in the command palette ("Show keybindings"), and is generated from the same table the palette is, so the two cannot disagree. Any key closes it; `↑`/`↓`, `Ctrl+k`/`Ctrl+j`, PageUp/PageDown and Home/End scroll it on a short terminal.

Prompt controls:

| Key | Action |
| --- | --- |
| `Enter` | Submit (or confirm a delete) |
| `Esc` or `Ctrl+c` | Cancel |
| `Backspace` / `Delete` | Delete before / under the cursor |
| `←`/`→` | Move the cursor one character |
| `Home`/`End`, `Ctrl+a`/`Ctrl+e` | Move the cursor to the start/end |
| `Ctrl+w` | Delete the word before the cursor |
| `Ctrl+u` | Delete to the start of the line |
| `Ctrl+k` | Delete to the end of the line — **except** in the command palette and the configured-project prompt, where it keeps its "select previous match" meaning |
| `Up`/`Down` or `Ctrl+k`/`Ctrl+j` | Move through prompt results where supported |

Filtering always uses the whole input, not just the text before the cursor, and the cursor is drawn at the right column for multi-byte and double-width characters.

Mouse support:

- Scroll wheel scrolls the selected output pane.
- Drag over terminal/chat-agent output to select visible text and copy it through OSC52.
- `Ctrl+Shift+C` copies the active `mult` text selection when the terminal forwards that key to `mult`.

The command palette includes discoverable actions for focus changes, starting input, adding/deleting sessions, opening workspaces, search, clearing search, reloading the config, dismissing the status message, showing the keybindings, and quitting. Palette entries and the help overlay come from one table in `src/app/bindings.rs`, so a command cannot appear in one and not the other.

## Deleting a chat or terminal

`Ctrl+q` (and the "Delete selected item" palette entry) opens a confirmation naming exactly what will go: the chat's title, or the terminal's name and the command it runs. When the parent workspace holds nothing else, the confirmation says on its own line that the workspace is removed with it — that cascade used to happen silently.

`Enter` or `y` deletes; `Esc`, `n` or `Ctrl+c` cancels; any other key does nothing, so no stray keypress deletes anything.

The confirmation is skipped only when the item is provably empty — there is nothing to ask about:

- a chat with an `idle` status whose PTY is not running and has no output on screen; or
- a shell terminal (no configured command) that is stopped, whose PTY is not running and has no output on screen.

Even then, a delete that would remove the parent workspace always asks.

## Status glyphs

Sidebar state is carried by **shape** first and colour second, so it survives `NO_COLOR` and colour vision deficiency:

| Glyph | Chat | Glyph | Terminal |
| --- | --- | --- | --- |
| `*` | Thinking (working) | `>` | A configured command is running |
| `?` | Waiting for an answer | `✓` | Last run exited cleanly |
| `!` | Failed | `!` | Last run exited non-zero or was signalled |
| `✓` | Finished, not seen yet | `$` | Idle / never started |
| `·` | Idle, or a finished chat you have already looked at | | |

## Status line

Problems that belong to no pane are shown one at a time on a single row above the prompt: a daemon that could not be reached or that reported a connection-wide failure, a state save that failed, a frame that could not be drawn, the config warnings from startup, a clipboard copy that failed, the result of a config reload, the count of command terminals left stopped after you decline the restore prompt, and the notices naming the backup when `state.json` had to be moved aside or had pre-11a chat transcripts copied out of it.

The row exists only while there is a message, so it never permanently costs terminal output space. `Ctrl+g` (or the "Dismiss status message" palette entry) clears the current message and reveals the next; the count of waiting messages is shown as `(+n more · ctrl-g)`, and `(ctrl-g dismisses)` when this is the last one. Messages are marked by shape as well as colour — `x` error, `!` warning, `·` info — so they stay legible under `NO_COLOR`.

## Configuration

Config path:

- `--config <PATH>`, if given
- otherwise `$MULT_CONFIG_PATH`, if set
- otherwise `$XDG_CONFIG_HOME/mult/config.json`
- otherwise `~/.config/mult/config.json`

Example:

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
    "base": "#232136",
    "text": "#e0def4",
    "iris": "#c4a7e7"
  }
}
```

`pi_agent_command` and `claude_code_command` select the binary for each agent backend (`Ctrl+a` starts a `pi` chat, `Ctrl+x` a Claude Code chat); `auto_start_*` toggle whether the selected chat of that kind starts on focus. Both commands are launched through your login shell (`$SHELL -lc …`), so shell features — pipelines, `$VAR` expansion, globbing — work inside them.

`clipboard_osc52` controls whether a text selection is copied to the system clipboard with OSC 52 (on by default). The payload is raw PTY output and the escape hands it to the host terminal's clipboard, so set it to `false` if you would rather selections stayed inside `mult`.

### Config keys

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `pi_agent_command` | string | `"pi"` | Command for a `pi` chat, run through `$SHELL -lc` |
| `claude_code_command` | string | `"claude"` | Command for a Claude Code chat, run through `$SHELL -lc` |
| `auto_start_pi_agent` | bool | `true` | Start a selected `pi` chat on focus |
| `auto_start_claude_code_agent` | bool | `true` | Start a selected Claude Code chat on focus |
| `auto_start_terminals` | bool | `true` | Start a selected terminal on focus |
| `mouse_capture` | bool | `true` | Capture mouse events (only applied at startup) |
| `clipboard_osc52` | bool | `true` | Copy selections to the host clipboard with OSC 52 |
| `projects` | list | `[]` | Project shortcuts, as `{"name":…,"path":…}` or `["name","path"]` |
| `colorscheme` | object | Rosé Pine Moon | Twelve `#rrggbb` keys — note the first is spelled `_nc`, with a leading underscore (`nc` is accepted as an alias) |

The twelve `colorscheme` keys are `_nc`, `base`, `muted`, `text`, `love`, `gold`, `pine`, `foam`, `iris`, `highlight_med`, `cursor` and `success`. **[docs/CONFIG.md](docs/CONFIG.md) is the complete reference** — every key with its type, default and what it actually colours, plus the read-time file requirements and the validation policy.

### Validation policy

Two rules, applied consistently:

- **A file that does not decode is a hard error.** Malformed JSON, an unknown key (`auto_start_terminal` instead of `auto_start_terminals`), or a value of the wrong type stops startup with `config error at <path>:<line>:<col>: <message>` on stderr and exit status 2. This file names commands that are shell-evaluated and auto-started, so quietly running with the defaults would run something you did not ask for. Unknown keys are rejected rather than ignored — a typo used to be a silent no-op.
- **A decodable file with a bad value warns and continues.** A `colorscheme` entry that is not `#rrggbb` keeps that key's default and reports it in the status line at startup. A `projects[].path` that does not exist is checked lazily, when the open-workspace prompt shows it, and marked `(missing)` there instead of failing the load or hiding the entry — a project on an unmounted share is a normal thing to have configured.

"Reload config" in the command palette re-reads the same file the session started from and applies it without restarting. A reload that fails reports through the status line and keeps the config that is already running. `mouse_capture` is the exception: it is pushed to the host terminal at startup, so a change to it only takes effect on the next start.

### Environment variables

Read by `mult` and `mult-server`:

| Variable | Purpose |
| --- | --- |
| `MULT_CONFIG_PATH` | Override config file path (`--config` wins over it) |
| `MULT_STATE_PATH` | Override state file path (`--state` wins over it) |
| `MULT_SOCKET_PATH` | Override `mult-server` Unix socket path (`--socket` wins over it). Read by both binaries |
| `MULT_SERVER_AUTOSPAWN` | Set to `0`/`false` to stop the client autospawning a daemon |
| `NO_COLOR` | Any non-empty value renders the whole UI in the terminal's default colors. The places that carried meaning in a background — the sidebar selection, a selected prompt row, the prompt cursor — switch to reverse video, and sidebar state is readable from the glyph shapes above, so the UI stays navigable. There is no light-theme default; on a light terminal, either set `NO_COLOR` or set the `colorscheme` keys. |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_RUNTIME_DIR`, `HOME` | Default config, state, socket and agent-runtime locations |
| `SHELL` | The login shell agent commands and `Command` terminals run under (`$SHELL -lc …`); falls back to `/bin/sh` |

Set **by** `mult` for the agent process it launches — not something you set:

| Variable | Purpose |
| --- | --- |
| `MULT_AGENT_STATUS_PATH` | Private file the agent writes its status into. Not exported at all if the runtime directory fails its privacy check |
| `MULT_AGENT_CHAT_ID` | Identifies which chat the agent belongs to |

Development and test only, documented in [CONTRIBUTING.md](CONTRIBUTING.md):
`MULT_SKIP_PTY_INTEGRATION`, `MULT_TEST_SHELL`.

The daemon is spawned with a minimal environment: `PATH`, `HOME`, `SHELL`,
`USER`, `LOGNAME`, `TERM`, `LANG`, and anything prefixed `LC_` or `MULT_`.

## State

State path:

- `--state <PATH>`, if given
- otherwise `$MULT_STATE_PATH`, if set
- otherwise `$XDG_DATA_HOME/mult/state.json`
- otherwise `~/.local/share/mult/state.json`

The state file is written atomically through an owner-only temporary file and final state files are set to `0600`. Newly-created state directories use owner-only permissions. State files with a newer schema version are rejected without rewriting them.

Decoding is lenient field by field: a missing or `null` key takes that field's default, and an unrecognised key is ignored, so a renamed or dropped field costs you that field rather than the whole project. When a file genuinely cannot be decoded — a whole workspace or chat entry with the wrong shape — it is moved aside with a `.corrupt-*` suffix and the session starts empty, and the status line then tells you where the backup went.

Durable state contains the workspace tree, chat metadata, terminal metadata, terminal launch commands, which terminals to restore at the next launch, and the daemon instance token that namespaces this client's sessions. PTY processes, raw terminal buffers, and scrollback are runtime/server-owned and are not stored in the JSON state file.

Because the state file stores command lines, it can cause execution. Terminals that were running when you quit are marked for restore and brought back on the next start — but only shell terminals restore automatically. A terminal with a stored command line is left stopped and listed in a confirmation prompt that shows each command; `y`/Enter runs them, Esc/`n` leaves them stopped. See [SECURITY.md](SECURITY.md).

## Project layout

```text
src/main.rs                 terminal setup/teardown; calls runtime::run
src/lib.rs                  the library every module below lives in
src/cli.rs                  argv parsing shared by both binaries
src/model.rs                durable project model and IDs
src/storage.rs              state loading/saving (StateStore)
src/config.rs               config loading, validation and the palette
src/git.rs                  off-thread git branch probe
src/layout.rs               AppLayout: where the frame's panes are
src/pty.rs                  client-side PTY runtime and server protocol adapter
src/bin/mult-server.rs      PTY server process

src/app/                    client UI state
  mod.rs                    the App itself and its interaction mode
  nav.rs                    sidebar order, selection, focus
  delete.rs                 deleting the selected item
  prompt.rs                 the modal prompts and the command palette
  open_workspace.rs         the open-workspace prompt and project matching
  text_input.rs             prompt text/cursor and list-selection cursors
  search.rs                 filtering a pane's lines
  selection.rs              the mouse text selection
  status.rs                 the status-line notice queue
  bindings.rs               the single keybinding/command table

src/runtime/                the event loop
  mod.rs                    the loop, config reload, saves, PTY event drain
  input.rs                  key/paste dispatch and the unprompted bindings
  prompts.rs                one key handler per prompt
  keymap.rs                 key -> PTY byte encoding
  mouse.rs                  mouse hit-testing and wheel handling
  clipboard.rs              text extraction, base64 and OSC 52
  session.rs                starting, restoring and sizing panes
  agent_launch.rs           agent command lines and generated runtime files
  agent_status.rs           per-chat agent status polling

src/ui/                     rendering (State -> Frame only)
  mod.rs                    draw()
  theme.rs                  palette and WCAG contrast helpers
  vt_screen.rs              vt100 -> tui_term adapter
  sidebar.rs                the sidebar
  main_pane.rs              the selected chat/terminal pane
  selection.rs              the text-selection highlight
  prompt.rs                 the prompt row
  status.rs                 the status line
  help.rs                   the keybinding overlay
  text.rs                   width measurement and truncation
  snapshots/                insta snapshots of whole rendered frames

  test_support.rs           shared fixtures for the ui tests

crates/protocol/src/        shared client/server protocol types
  lib.rs                    wire messages, framing, private file/dir checks
  peer.rs                   Unix-socket peer-credential verification
  rand.rs                   non-cryptographic random ids
  shell.rs                  $SHELL discovery, `-lc` argv and quoting

fuzz/fuzz_targets/          a separate workspace; see CONTRIBUTING.md
  protocol_read_message.rs  read_message over arbitrary bytes
  vt_response_detector.rs   the terminal-response scanner over arbitrary output

tests/pty_integration.rs    end-to-end tests against a real daemon and PTYs
extensions/mult-status.ts   bundled pi status extension
extensions/mult-claude-status.sh bundled Claude Code status hook script
```

## Runtime server and IPC

`mult-server` owns long-lived PTYs and communicates with clients over a Unix socket. See [docs/DAEMON.md](docs/DAEMON.md) for operational details, socket path selection, autospawn behavior, and security notes.

By default the socket lives at `$XDG_RUNTIME_DIR/mult.sock`; without `XDG_RUNTIME_DIR`, the fallback is a private `/tmp/mult-<uid>/mult.sock` directory. Server-created socket parents are mode `0700`, sockets are mode `0600`, and every supported platform verifies Unix-socket peer credentials when clients connect — `SO_PEERCRED` on Linux, `getpeereid(3)` on macOS and the BSDs, and a hard refusal on a Unix that offers neither.

Several `mult` instances can share one daemon: sessions are namespaced by the instance token in each client's state file, so two windows never collide and neither can attach to the other's panes, while a restarted client reclaims its own. Connecting happens in the background, so the UI stays responsive while the daemon is starting or unreachable and reports the outcome on the status line.

## Platform notes

`mult` currently uses Unix sockets and Unix PTY/process APIs, so the practical target is Linux/macOS-like systems.

## Contributing

Contributions are welcome — start with [CONTRIBUTING.md](CONTRIBUTING.md) and the detailed [AGENTS.md](AGENTS.md) guide. For security issues, see [SECURITY.md](SECURITY.md). Planned follow-up work is tracked in [docs/BACKLOG.md](docs/BACKLOG.md), with the execution order in [docs/PLAN.md](docs/PLAN.md).

## Documentation

| Document | Contents |
| --- | --- |
| [docs/CONFIG.md](docs/CONFIG.md) | Every config key: type, default, effect |
| [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) | Failure modes, the exact message each produces, and the fix |
| [docs/DAEMON.md](docs/DAEMON.md) | Daemon operation, socket paths, autospawn, security notes |
| [docs/RELEASING.md](docs/RELEASING.md) | Release checklist |
| [docs/BACKLOG.md](docs/BACKLOG.md) | Tracked work |
| [SECURITY.md](SECURITY.md) | Threat model and reporting |

## License

Licensed under either of:

- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)
