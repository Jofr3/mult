# mult server lifecycle

`mult` uses a tmux-style split: `mult-server` owns PTYs and terminal grids, while `mult` clients attach over a per-user Unix socket.

Socket path:

- `MULT_SOCKET_PATH=/path/to/mult.sock` when explicitly set
- `$XDG_RUNTIME_DIR/mult.sock`
- fallback: `/tmp/mult-$UID.sock`, or `/tmp/mult-$USER.sock` if `UID` is unset; unsafe path characters in the env value are replaced with `_`

The server removes a stale socket file when it starts, but refuses to remove an existing non-socket path. If a live server already owns the socket, a second server exits. The socket file is chmodded to `0600` after bind. Clients validate the socket protocol version on connect; after upgrading `mult`, restart `mult-server` if the client reports an incompatible protocol version. Client autospawn propagates the selected socket path to `mult-server`.

## Recommended: systemd user service on NixOS

Enable linger so the user service can keep running after logout:

```nix
{
  users.users.jofre.linger = true;
}
```

Add a user service. If this repo is exposed as a flake input named `mult`, one suitable NixOS module snippet is:

```nix
{ inputs, pkgs, ... }:
{
  systemd.user.services.mult-server = {
    description = "mult persistent terminal server";
    wantedBy = [ "default.target" ];

    serviceConfig = {
      ExecStart = "${inputs.mult.packages.${pkgs.system}.default}/bin/mult-server";
      Restart = "on-failure";
      RestartSec = "1s";
    };
  };
}
```

Apply the config, then start it:

```sh
systemctl --user daemon-reload
systemctl --user enable --now mult-server.service
```

Useful checks:

```sh
systemctl --user status mult-server.service
journalctl --user -u mult-server.service -f
```

## Development/autospawn path

For local development you can run the server manually:

```sh
just server
# or
cargo run --bin mult-server
```

The `mult` client also attempts a lightweight tmux-style autospawn when the socket is missing or stale. It looks for a `mult-server` executable next to the running `mult` executable, starts it detached from the launching terminal with stdio closed, waits briefly for the socket, then connects. Autospawned servers keep owning PTYs after the client terminal is closed, so reopening `mult` can reattach to the same panes.

Disable client autospawn with:

```sh
MULT_SERVER_AUTOSPAWN=0 mult
```

Autospawn is a convenience for interactive use and survives closing the terminal that launched the `mult` client. Prefer the systemd user service for a robust long-lived daemon across full logouts/restarts.
