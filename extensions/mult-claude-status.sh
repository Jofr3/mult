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
tmp=${path}.$$.tmp
if printf '{"version":1,"status":"%s","chatId":"%s"}\n' "$status" "$chat" >"$tmp" 2>/dev/null; then
    mv -f "$tmp" "$path" 2>/dev/null || rm -f "$tmp" 2>/dev/null || true
fi
exit 0
