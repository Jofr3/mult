# Contributor / agent guide

This repository is a Rust workspace for `mult`, a terminal UI plus a small persistent PTY daemon.

## Before editing

- Read `README.md` for user-facing behavior and controls.
- For planned work, start at `docs/ROADMAP.md` (it fronts `docs/BACKLOG.md` and `docs/PLAN.md`). Refer to work by its backlog ID.
- For daemon/socket behavior, read `docs/DAEMON.md`.
- For config keys, `docs/CONFIG.md`; for failure modes, `docs/TROUBLESHOOTING.md`.
- Keep changes narrow and buildable; do not rewrite large subsystems unless the task explicitly requires it.
- Preserve existing public behavior unless the change fixes a documented bug or improves documented behavior.

## Validation

Use the strict local gate when possible:

```sh
nix develop
just ci
```

`just ci` runs `version-check`, formatting checks, clippy with `-D warnings`, tests, `cargo deny check` (advisories/licenses/bans/sources — it supersedes the standalone `cargo audit`, which no longer runs), and a `tsc --noEmit` typecheck of the bundled status extension that skips with a notice when npm is unavailable. If you are outside the Nix shell, install `just` and `cargo-deny` first.

For smaller iterations, prefer:

```sh
cargo check --workspace --all-targets --all-features
cargo test --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

## Editing notes

- Run `cargo fmt --all` before final validation.
- Keep `Cargo.lock` in sync with dependency changes.
- Do not add dependencies unless they clearly reduce risk or replace an unsafe/unmaintained dependency.
- State files and runtime IPC are security-sensitive; keep paths private and avoid predictable public `/tmp` files.
- `MULT_AGENT_CMD` is parsed by `mult`, not a shell: basic quotes and backslash escapes are supported, but shell expansion is intentionally not.
- `pi_agent_command` and `claude_code_command` (the chat-agent backends, selected per chat by `AgentKind`), `file_manager_command` (`Ctrl+n`), `editor_command` (`Ctrl+e`, resolved from `$VISUAL`/`$EDITOR` when unset) and `TerminalLaunch::Command` are the opposite: they are run through the login shell (`$SHELL -lc <command>`), so they *are* shell-evaluated — pipelines, `$VAR` expansion, and globbing all apply. This is the user's own config, not a privilege boundary, but the two agent-launch paths deliberately have different semantics, so keep them distinct.
- In a **remote** workspace (`Workspace::remote`, from a `projects` entry with a `remote`) the same commands are wrapped in `ssh` by `src/remote.rs` and evaluated by the *remote* login shell instead. Two shells therefore parse them, which is why everything that module builds is quoted twice and sticks to forms that mean the same thing in `sh`, `bash`, `zsh` and `fish`. Do not add `${VAR:-default}` or other POSIX-only spellings to a remote command line.
- The remote git branch is read by `runtime::remote_branch` on a background thread (`cat <path>/.git/HEAD` over `ssh`, `BatchMode=yes`, at most once per 30 s per workspace). Never run `git` on the far side, and never probe on the render thread — `src/git.rs`'s module comment explains the first, and an `ssh` in the 2 s refresh explains the second.
- `tmux` belongs to **agent chats only**, never to terminals. A terminal is its connection and ends with its pane; an agent is a conversation that has to survive a dropped link, so `agent_launch::remote_agent_launch` puts it in the project's `tmux` session, re-entered with `new-session -A -d` + `attach-session`. One session per remote workspace means one chat: `App::add_chat_to_selected_workspace_and_return` returns the existing chat instead of adding a second. A remote chat also runs the *plain* agent command (`agent_command::remote_agent_command`): the `-e` extension and `--settings` hooks are local files, and pointing an agent on another machine at them would stop it starting.
- The same `tmux` line sets `set-titles`/`set-titles-string` **on the session** (`-t`), never globally — a remote machine's tmux config is not `mult`'s to rewrite. Without it an agent's window title never leaves `tmux`, and the sidebar row cannot follow it.
