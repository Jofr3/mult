# Remaining work

This checklist tracks improvements that were too large or risky for the current pass.

- Consider making git branch refresh fully asynchronous so repository probes can never block a redraw.
- Review non-Linux peer credential support. Linux uses `SO_PEERCRED`; other Unix platforms currently fall back to socket path permissions.
- Add cleanup/rotation for private runtime files such as generated status extensions.
- Consider a more structured configuration format for the experimental process-agent backend if `MULT_AGENT_CMD` grows beyond simple command-line parsing.
- When neither `HOME` nor the relevant `XDG_*` variable is set, the config and state directories fall back to the current working directory. The directory is created `0700` and the state file `0600`, so contents stay private, but state/config can still land in an unexpected CWD. Consider refusing, or using a private per-UID temp directory, instead. (Tier 2 security review; minor, confidentiality already mitigated by the permission bits.)
