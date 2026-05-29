# Security policy

`mult` is a local-first developer tool. The client and the `mult-server`
daemon communicate over a per-user Unix domain socket (mode `0600`) and verify
the peer's UID with `SO_PEERCRED` on Linux; project state and runtime files are
written with `0600`/`0700` permissions. The threat model is primarily
multi-user machines, hostile repositories or state files opened by the user,
and crash-safety — there is no network listener.

See [`AGENTS.md`](AGENTS.md) and [`docs/DAEMON.md`](docs/DAEMON.md) for the
security-sensitive areas (state files, runtime IPC, process spawning).

## Supported versions

This is a `0.x` prototype. Only the latest `main` is supported; fixes land on
`main` and are not backported.

## Reporting a vulnerability

Please report suspected vulnerabilities **privately** rather than opening a
public issue:

- Preferred: open a private report via GitHub Security Advisories — the
  **"Report a vulnerability"** button on the repository's **Security** tab.

Please include the affected version or commit, a description of the impact, and
ideally a reproduction. We aim to acknowledge reports within about a week and
ask for reasonable time to ship a fix before any public disclosure.
