# Remaining work

This checklist tracks improvements that were too large or risky for the current pass.

- Consider making git branch refresh fully asynchronous so repository probes can never block a redraw.
- Add broader end-to-end tests for `mult` + `mult-server` reconnect behavior across process restarts.
- Review non-Linux peer credential support. Linux uses `SO_PEERCRED`; other Unix platforms currently fall back to socket path permissions.
- Add cleanup/rotation for private runtime files such as generated status extensions.
- Consider a more structured configuration format for the experimental process-agent backend if `MULT_AGENT_CMD` grows beyond simple command-line parsing.
