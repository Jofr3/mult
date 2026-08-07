# mult

`mult` is a terminal UI for multiplexing project workspaces, `pi` agent chats, and per-workspace PTY terminals.

The current implementation is a Ratatui/Crossterm client plus a small `mult-server` process that owns long-lived PTYs. The client renders workspaces, chats, terminals, prompt overlays, status indicators, git branches, and terminal output while the server keeps terminal sessions alive across client reconnects.

## Current capabilities

- Multiple workspaces, each with a working directory, agent chats, and terminals.
- Persistent JSON project state for workspace/chat/terminal metadata and structured messages emitted by the experimental process-agent backend.
- Runtime PTY sessions managed by `mult-server` over a Unix socket.
- Shell terminals and command terminals.
- Auto-start for the selected terminal or selected agent chat.
- Two agent backends per chat — `pi` (via a bundled status extension) and
  Claude Code (via generated lifecycle hooks) — chosen when the chat is created,
  tagged in the sidebar as `: pi` / `: cc`, and both reporting live
  status into the sidebar dot.
- Sidebar rows follow the pane's own window title (OSC 0/2) where the label
  would otherwise be a guess: what an agent says it is working on, and a
  shell's `cwd` or the file an editor is on.
- Terminal scrollback, paste handling, mouse text selection, and OSC52 clipboard copy (opt-out via `clipboard_osc52`).
- Full xterm mouse reporting to programs that ask for it (press/release/drag/motion, every protocol mode and encoding, modifier bits), with `Shift` reserved for `mult`'s own selection; cursor-key, keypad, bracketed-paste and focus-reporting modes honoured.
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

## Command line

```text
mult [--config <path>] [--state <path>] [--socket <path>] [-h|--help] [-V|--version]
mult-server [--socket <path>] [-h|--help] [-V|--version]
```

Each option overrides its environment variable, which overrides the default:
**flag > environment > default**. `--flag value` and `--flag=value` are
equivalent. An unknown option or a stray argument is an error with a non-zero
exit, never a silent start. `mult-server` rejects `--config` and `--state`
rather than accepting a flag it would ignore — the daemon reads neither file.

## Useful commands

```sh
just run           # run the TUI client
just server        # run the persistent PTY server
just check         # cargo check --workspace --all-targets --all-features
just test          # cargo test --workspace --all-targets --all-features
just fmt           # format Rust, and flake.nix when nixpkgs-fmt exists
just fmt-check     # check Rust formatting without modifying files
just lint          # clippy with warnings denied
just deny          # cargo deny check: advisories, licenses, bans, sources
just typecheck     # tsc --noEmit for the bundled status extension
just version-check # assert Cargo.toml / flake.nix / package.json agree
just coverage      # cargo llvm-cov line/region summary
just ci            # strict local gate (see below)
just install-hooks # pre-commit hook running cargo fmt --check
just watch         # cargo-watch check/test loop
just nix-build     # nix build
just nix-check     # nix flake check
```

## Validation / CI

`just ci` is the local gate. It runs, in order:

1. `just version-check` — the version in `Cargo.toml`, `flake.nix` and `extensions/package.json` still agree
2. `just fmt-check` — `cargo fmt --all -- --check`
3. `just lint` — `cargo clippy --workspace --all-targets --all-features -D warnings`
4. `just test` — `cargo test --workspace --all-targets --all-features`
5. `just deny` — `cargo deny check` (advisories, licenses, bans, sources)
6. `just typecheck` — `tsc --noEmit` on the bundled status extension

`just deny` intentionally fails if `cargo-deny` is missing; `nix develop`
provides it, along with `just` and `cargo-llvm-cov`. `just typecheck` skips with
a notice when `npm` or `extensions/node_modules` is unavailable, so `just ci`
still completes offline and on a fresh clone.

GitHub Actions runs that same `just ci` gate on **both** Linux and macOS, and
adds four jobs it does not make sense to run on every local invocation:

| Job | What it adds |
| --- | --- |
| `msrv` | `cargo +1.88 check --locked --workspace --all-targets --all-features`, proving the declared MSRV really builds |
| `coverage` | `cargo llvm-cov` over the workspace |
| `extension` | a real `npm ci --ignore-scripts` before the typecheck, so it cannot skip |
| `nix` | `nix flake check` |

CI also runs on a weekly schedule and on demand, not only on push and pull
request, so a newly published RustSec advisory is caught without anyone opening
a PR.

`cargo deny check advisories` is the single supply-chain gate; the standalone
`cargo audit` it supersedes (see `deny.toml`) has been removed rather than run
alongside it. The dependency tree currently avoids known RustSec advisories by
using `postcard` for local IPC framing and current `ratatui`/`tui-term`
releases.

Tagging `vX.Y.Z` triggers a release workflow that verifies the tag matches the
crate version and runs the full gate before publishing anything. See
[docs/RELEASING.md](docs/RELEASING.md).

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
| `Ctrl+s` | Search the selected terminal pane (see the note below for chats) |
| `Ctrl+q` | Delete the selected chat/terminal, or an empty workspace — immediately, with no confirmation |
| `Ctrl+n` | Dismiss the status notices |
| `?` or `F1` | Show every key and command in an overlay |
| `Ctrl+Esc` | Quit |

Typing in a selected chat or terminal starts/focuses its PTY and forwards input to it.
Because of that, `?` only opens the overlay when no pane would have received it;
`F1` and the palette's "Show keybindings" always work.

Errors with no pane to report into — a daemon that will not connect, a failed
state save, a config warning — appear in a status surface above the prompt line.
Notices fade after 12 seconds, at most four are shown, and `Ctrl+n` clears them.

Prompt controls:

| Key | Action |
| --- | --- |
| `Enter` | Submit |
| `Esc` or `Ctrl+c` | Cancel |
| `Left`/`Right` | Move the cursor one character |
| `Home`/`End` or `Ctrl+a`/`Ctrl+e` | Move the cursor to the start/end |
| `Backspace`/`Delete` | Delete the character before/after the cursor |
| `Ctrl+w` | Delete the word before the cursor |
| `Ctrl+u` | Delete everything before the cursor |
| `Up`/`Down` or `Ctrl+k`/`Ctrl+j` | Move through prompt results where supported |

Mouse support:

- Scroll wheel scrolls the selected output pane.
- Drag over terminal/chat-agent output to select visible text and copy it through OSC52.
- `Ctrl+Shift+C` copies the active `mult` text selection when the terminal forwards that key to `mult`.
- Both copy paths are gated by `clipboard_osc52` (default `true`). Set it to `false` and `mult` never writes an OSC 52 escape: selection still highlights and `Ctrl+Shift+C` is still consumed, but nothing reaches the clipboard.
- **A program that has grabbed the mouse gets the pointer.** When the program
  running in the selected pane (Claude Code, `nvim`, `less`, a TUI file
  manager) enables mouse reporting, clicks, drags, releases, pointer movement
  and wheel notches are forwarded to *it* — encoded in the protocol and
  encoding it asked for, with the Shift/Alt/Ctrl bits — instead of driving
  `mult`'s own selection and scrollback. Only what its mode asked for is sent:
  a press-only program is never told about releases, and one that did not
  enable motion tracking is not sent pointer movement.
- **Hold `Shift` to take the pointer back.** As in every xterm-descended
  terminal, `Shift` bypasses the program's grab, so `Shift`+drag selects text
  and `Shift`+wheel scrolls `mult`'s own scrollback even inside a full-screen
  program. A pane whose program has exited needs no `Shift`: the pointer
  returns to `mult` on its own.

Sidebar labels:

- A pane's row follows the **window title its program sets** (OSC 0/2) wherever
  the label would otherwise be derived. An agent chat reads `<title>: cc` once
  the agent sets one — which is what tells several Claude Code chats apart,
  since they are all created with the same name and none can be renamed — and a
  shell terminal shows its title in place of the last command `mult` scraped
  from your keystrokes. The `: pi` / `: cc` tag always survives; a title too
  long for the sidebar is what gets truncated.
- A **command terminal keeps its command**. That is what you typed to create the
  pane, so it stays your landmark for it however the program renames its window.
- A title is program-supplied, so control characters are stripped from it and
  its length is capped before it is drawn.

Terminal emulation notes:

- Cursor-key mode (DECCKM) and keypad mode (DECKPAM) are honoured, so arrows
  and the numeric keypad reach a full-screen program in the SS3 form it asked
  for. Bracketed paste (DECSET 2004) is honoured on paste.
- Focus reporting (DECSET 1004) is honoured. The pane holding the keyboard is
  the selected one, so a program is told it lost focus when you switch panes,
  open the command palette or the help overlay, or when the terminal window
  `mult` itself runs in loses focus — and told it regained it on the way back.
- `Ctrl` with a digit or punctuation key reaches its control code (`Ctrl+2` is
  NUL, `Ctrl+/` and `Ctrl+_` are US, `Ctrl+8` is DEL), and `Ctrl+Backspace` is
  BS rather than the DEL that plain `Backspace` sends.
- Answers `mult` generates on a program's behalf — a mouse report, a focus
  notification, a cursor-position report — do not count as typing: they do not
  end a scrollback view the way a keystroke does.

The command palette includes discoverable actions for focus changes, starting input, adding/deleting sessions, opening workspaces, search, clearing search, showing the keybinding overlay, dismissing notices, reloading the config, and quitting. The palette and the overlay are generated from one binding table, so neither can drift from the other.

"Reload config" re-reads `config.json` in place. The colorscheme, `projects`, agent commands and auto-start settings apply to the next frame; `mouse_capture` is a terminal mode set once for the session and needs a restart, and an already-running PTY keeps the command it was started with. A config that fails to load leaves the running one in place and reports the error.

**Chat search is not wired up.** `Ctrl+s` on a chat searches the *structured* transcript, which only the experimental process-agent backend writes and which nothing calls today, so it is empty for every chat you can create — the pane says so instead of reporting "no matches". Terminal search works normally. See [docs/ROADMAP.md](docs/ROADMAP.md#open-decisions-carried-over).

`NO_COLOR` (set to any non-empty value) drops every colour: `mult` then emits nothing but the terminal's own foreground and background, and uses bold/reverse video and per-state glyphs so status is still readable.

## Configuration

Config path:

- `--config <path>`, if given
- otherwise `$MULT_CONFIG_PATH`, if set
- otherwise `$XDG_CONFIG_HOME/mult/config.json`
- otherwise the effective user's passwd home at `~/.config/mult/config.json`; startup fails clearly if no durable home can be resolved

Whichever path is used, the config must be a regular file owned by you, in a directory of yours that is not group- or other-writable, and under 1 MiB — the commands it carries are shell-evaluated and auto-started, so a path anyone else can steer is code execution. Symlinks are **resolved first** and those checks then apply to the file at the far end, so the layout dotfile managers leave behind — a `~/.config/mult` or `config.json` linked into a repository, which is all `home-manager`'s `xdg.configFile` can produce — loads fine, while a link aimed into a directory others can write is still refused: see [docs/CONFIG.md](docs/CONFIG.md#the-file) and [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md#the-config-is-refused-at-startup).

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

`pi_agent_command` and `claude_code_command` select the binary for each agent backend (`Ctrl+a` starts a `pi` chat, `Ctrl+x` a Claude Code chat); `auto_start_*` toggle whether the selected chat of that kind starts on focus. Both commands are launched through your login shell (`$SHELL -lc …`), so shell features — pipelines, `$VAR` expansion, globbing — work inside them. This is intentionally different from `MULT_AGENT_CMD` (below), which `mult` splits into arguments itself with no shell involved.

The example above sets 3 of the 12 colorscheme keys. **[docs/CONFIG.md](docs/CONFIG.md) is the complete reference** — every top-level and colorscheme key with its type, default and effect, including `_nc` (the unfocused-pane background, written with a leading underscore).

### Environment variables

Variables you may set. All are optional.

| Variable | Read by | Purpose |
| --- | --- | --- |
| `MULT_CONFIG_PATH` | client | Override the config file path, used verbatim. `--config` outranks it. |
| `MULT_STATE_PATH` | client | Override the state file path. `--state` outranks it. |
| `MULT_SOCKET_PATH` | client, daemon | Override the `mult-server` Unix socket path. Both ends must agree. `--socket` outranks it, on either binary. |
| `MULT_SERVER_AUTOSPAWN` | client | Set to `0`, `false`, `False` or `FALSE` to stop the client autospawning `mult-server`. Any other value, or unset, leaves autospawn on. |
| `MULT_AGENT_CMD` | client, daemon | **Experimental, and currently a no-op.** The process-agent backend it configures has no production call path, so setting it changes nothing today. When it is wired, it is argv-split with simple shell-style quotes and backslash escapes — no shell expansion, unlike `pi_agent_command`. Retained rather than removed because `src/transcript.rs` builds on the same types; see [docs/ROADMAP.md](docs/ROADMAP.md#open-decisions-carried-over). |
| `NO_COLOR` | client | Set to any non-empty value to disable colour entirely. Read once per process; the palette becomes the terminal's own foreground and background, with bold, reverse video and per-state glyphs carrying what colour used to. |
| `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `HOME` | client | Standard. Used to resolve config and state when the overrides above are unset; must be absolute. |
| `XDG_RUNTIME_DIR` | client, daemon | Standard. The default socket parent and the home of the agent status journals; falls back to `/tmp/mult-<euid>` when unset. |
| `SHELL` | client, daemon | The login shell used for agent commands and command terminals. |

`mult` also *sets* variables for the processes it spawns. These are the agent
status bridge's private interface, read by `extensions/mult-status.ts` and
`extensions/mult-claude-status.sh` — do not set them yourself:
`MULT_AGENT_STATUS_PATH`, `MULT_AGENT_STATUS_VERSION`, `MULT_AGENT_CHAT_ID`,
`MULT_AGENT_KIND`, `MULT_AGENT_NAMESPACE`, `MULT_AGENT_SESSION_TOKEN`,
`MULT_AGENT_GENERATION`. An autospawned daemon inherits only an allow-list of
the client's environment (`PATH`, `HOME`, `SHELL`, `USER`, `LOGNAME`, `TERM`,
`LANG`, plus everything prefixed `LC_` or `MULT_`), so it cannot re-export the
starting shell's secrets into every later pane; `TERM` and `COLORTERM` are then
set explicitly for PTY children.

That allow-list also decides what the daemon's PTYs see, so a variable exported
for an agent — `ANTHROPIC_API_KEY` is the obvious one — **does not reach panes of
an autospawned daemon**. Start the daemon yourself when a pane needs extra
environment (`just server` or `mult-server`, then `MULT_SERVER_AUTOSPAWN=0 mult`);
a manually started `mult-server` keeps whatever environment you give it.
Exporting from `~/.profile` also works, since agent commands run through a login
shell. See
[docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md#an-agent-cant-see-its-api-key-or-any-other-exported-variable)
and [docs/DAEMON.md](docs/DAEMON.md).

Two further variables affect only the test suite —
`MULT_SKIP_PTY_INTEGRATION` and `MULT_TEST_SHELL`. See
[CONTRIBUTING.md](CONTRIBUTING.md#test-environment-variables).

If something is not behaving, [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md)
maps the errors this code actually emits to their causes.

## State

State path:

- `--state <path>`, if given
- otherwise `$MULT_STATE_PATH`, if set
- otherwise `$XDG_DATA_HOME/mult/state.json`
- otherwise the effective user's passwd home at `~/.local/share/mult/state.json`; startup fails clearly if no durable home can be resolved

The state file is owned by one TUI at a time through a nonblocking process-lifetime lock acquired before loading. A second TUI using the same state path fails clearly instead of overwriting a stale snapshot. Writes are atomic through an owner-only temporary file; state and lock files are `0600`, and newly-created state directories are `0700`. A file that lost part of itself — a renamed or `null` field, missing ID hints, an identity table that no longer matches — is decoded for what it still holds and repaired in place rather than discarded; only a file the decoder can make nothing of is moved aside with a `.corrupt-*` suffix before resetting to defaults, and that reset is reported on stderr and in the app, naming the backup. State files with a newer schema version are rejected without rewriting them. Older state is explicitly migrated one version at a time (1 -> 2 -> 3) and saved before any daemon restoration or command launch. Version 3 persists a terminal's *intent* (`restore_on_launch`) rather than its liveness; whether a pane is actually live is the daemon's answer and is never read from disk.

Durable state contains the workspace tree, terminal metadata and launch commands, immutable session identities, statuses, and messages received through the structured experimental process-agent API. Normal Pi and Claude Code chats run in PTYs: their raw terminal output can survive a TUI reconnect while the same daemon session lives, but it is not treated as an authoritative structured transcript and does not survive daemon loss. Raw terminal buffers and scrollback are not stored in the JSON state file. The separate bounded transcript-journal codec is reserved for backends that provide real role/message boundaries; `mult` never invents those boundaries by scraping PTY bytes.

On startup, a command terminal persisted as running is attached only if its existing daemon session is available. `mult` never relaunches that command during restoration; an unavailable session is marked stopped and requires deliberate typing or **Start selected PTY** before it runs again. Persisted stopped command terminals are likewise not auto-started after a client restart.

If saving state fails, the TUI remains open, keeps the state dirty, and shows the error until a later user action retries successfully. A quit request that cannot save is cancelled rather than discarding unsaved state.

Pi and Claude Code lifecycle bridges write generation-scoped, append-only status journals under the private runtime directory. Records include the durable namespace/token, chat, backend, schema, and random process generation. The TUI validates and forwards them to the daemon; daemon state is authoritative across reconnects and rejects stale identities/generations or late updates after final failure/exit.

## Project layout

```text
src/lib.rs                        library root; the modules below marked (lib) are public
src/main.rs                       `mult` entry point: argument parsing, config load, state lock, signal handlers, panic-safe cleanup
src/terminal_guard.rs             RAII terminal mode setup and restore
src/runtime/                (lib) the TUI event loop and its wiring
  mod.rs                            the loop itself: per-tick ordering, config reload, host-terminal failure policy
  input.rs                          key and paste dispatch, global control-key shortcuts
  prompt.rs                         prompt key handlers and the command-palette actions
  keymap.rs                         key -> PTY byte encoder (cursor/keypad modes) and the control-key predicates
  mouse.rs                          mouse hit-testing, xterm mouse forwarding, text selection, scrollback
  clipboard.rs                      OSC 52 clipboard writes, base64, tmux passthrough
  session.rs                        PTY restore/start/resize, pane focus reports, drained PTY events
  agent_launch.rs                   starting and focusing chat agents; the process-agent backend
  agent_command.rs                  agent command lines and the generated extension/hook files
  agent_status.rs                   the per-chat agent status journals and the daemon reconciliation
  save.rs                           save scheduling and its exemptions
src/app/                    (lib) session state
  mod.rs                            the `App` struct, construction, save flags, help, `InteractionMode`
  nav.rs                            the sidebar walk and the selection over it
  prompt.rs                         prompt input, editing, the palette and command-terminal prompts
  open_workspace.rs                 project fuzzy matching, path expansion, workspace import
  search.rs                         pane search and the chat transcript
  selection.rs                      mouse text selection in cells
  notices.rs                        the status-surface notices
  bindings.rs                       the keybinding table and the palette generated from it
  mutate.rs                         workspace/chat/terminal creation and deletion
src/ui/                     (lib) Ratatui rendering
  mod.rs                            frame composition: which surface goes where, in what order
  theme.rs                          palette, colour parsing, WCAG contrast, NO_COLOR
  vt_screen.rs                      the vt100 -> tui_term adapter
  sidebar.rs                        the sidebar and its status glyphs
  main_pane.rs                      the selected chat or terminal pane
  terminal_view.rs                  drawing a live PTY screen and the selection highlight
  prompt.rs                         the prompt surface
  status.rs                         the notice surface
  help.rs                           the keybinding overlay
  text.rs                           display-width text helpers
  snapshots/                        insta snapshots for the ui render tests
src/layout.rs               (lib) `AppLayout`: the frame divided, resolved once per loop iteration
src/model.rs                (lib) durable project model, IDs, session identity, state schema
src/pty.rs                  (lib) client-side PTY runtime and server protocol adapter
src/cli.rs                  (lib) argument parsing for both binaries
src/config.rs               (lib) config loading, validation, defaults, and DEFAULT_COLOR_SCHEME
src/storage.rs              (lib) state load/save, ownership lock, migrations, corrupt-state backups
src/paths.rs                (lib) XDG/HOME resolution for the config and state directories
src/git.rs                  (lib) workspace git branch probe
src/agent.rs                (lib) experimental process-agent backend
src/transcript.rs           (lib) bounded append-only transcript journal (built, not yet wired)
src/bin/mult-server.rs            PTY server process
crates/protocol/                  shared client/server protocol types, framing, and peer checks
tests/pty_integration.rs          end-to-end PTY tests against a real daemon
tests/fixtures/state/             golden state files for the migration tests
extensions/mult-status.ts         bundled pi status extension (embedded via include_str! in src/runtime/agent_command.rs)
extensions/mult-claude-status.sh  bundled Claude Code status hook script (likewise)
```

`terminal_guard.rs` is the only module declared by `src/main.rs` rather than
`src/lib.rs`, so it is private to the `mult` binary and cannot be reached from
integration tests; `runtime` joined the library in R10b. Documentation lives in
[docs/](docs/): the
[roadmap](docs/ROADMAP.md), the [daemon design](docs/DAEMON.md), the
[config reference](docs/CONFIG.md), [troubleshooting](docs/TROUBLESHOOTING.md),
and [releasing](docs/RELEASING.md).

## Runtime server and IPC

`mult-server` owns long-lived PTYs and communicates with clients over a Unix socket. See [docs/DAEMON.md](docs/DAEMON.md) for operational details, socket path selection, autospawn behavior, and security notes.

By default the socket lives at `$XDG_RUNTIME_DIR/mult.sock`; without `XDG_RUNTIME_DIR`, the fallback is a private `/tmp/mult-<uid>/mult.sock` directory. Server-created socket parents are mode `0700`, sockets are mode `0600`, and Linux builds verify Unix-socket peer credentials when clients connect.

## Platform notes

`mult` currently uses Unix sockets and Unix PTY/process APIs, so the practical target is Linux/macOS-like systems.

## Contributing

Contributions are welcome — start with [CONTRIBUTING.md](CONTRIBUTING.md) and the detailed [AGENTS.md](AGENTS.md) guide. For security issues, see [SECURITY.md](SECURITY.md).

Planned work is tracked in **[docs/ROADMAP.md](docs/ROADMAP.md)** — the single entry point for the item list ([docs/BACKLOG.md](docs/BACKLOG.md)), the execution order ([docs/PLAN.md](docs/PLAN.md)), and the open design decisions that are not yet items.

## License

Licensed under either of:

- [MIT](LICENSE-MIT)
- [Apache-2.0](LICENSE-APACHE)
