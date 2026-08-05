#!/bin/sh
# mult <-> Claude Code status bridge.
#
# Claude Code hooks invoke this with the desired mult status as the first
# argument (e.g. `sh mult-claude-status.sh running`). It writes the chat's
# status file, which `mult` polls once per frame to color the sidebar dot. The
# generated `--settings` file maps each lifecycle event to a status:
#
#   SessionStart     -> idle      UserPromptSubmit -> running
#   PreToolUse       -> running   Notification     -> waiting
#   Stop             -> finished
#
# MULT_AGENT_STATUS_PATH / MULT_AGENT_CHAT_ID come from the environment that
# `mult` sets on the spawned `claude` process and that hooks inherit. The status
# values are a fixed vocabulary supplied by mult, never user input.
#
# It stays silent and always exits 0 so a status hiccup never disturbs the agent.
set -u

# Claude Code delivers the hook event as JSON on stdin; drain it first so the
# writer never sees a broken pipe, but ignore it (the status is in $1).
cat >/dev/null 2>&1 || true

status=${1:-}
path=${MULT_AGENT_STATUS_PATH:-}
[ -n "$status" ] || exit 0
[ -n "$path" ] || exit 0

chat=${MULT_AGENT_CHAT_ID:-}

# `mktemp` rather than a temp name built from the status path and the shell's
# pid: that name was predictable, and the plain `>` redirect neither refused an
# existing file nor declined to follow a symlink, so anything pre-planted at it
# was opened and truncated on the first hook — writing this JSON into, say, the
# user's authorized_keys. mktemp creates a fresh file with O_CREAT|O_EXCL and
# mode 0600, and the rename preserves that mode.
tmp=$(mktemp "${path}.XXXXXXXX" 2>/dev/null) || exit 0
if printf '{"version":1,"status":"%s","chatId":"%s"}\n' "$status" "$chat" >"$tmp" 2>/dev/null; then
    mv -f "$tmp" "$path" 2>/dev/null || rm -f "$tmp" 2>/dev/null || true
else
    rm -f "$tmp" 2>/dev/null || true
fi
exit 0
