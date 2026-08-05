import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { randomBytes } from "node:crypto";
import {
  closeSync,
  constants,
  mkdirSync,
  openSync,
  renameSync,
  statSync,
  unlinkSync,
  writeSync,
} from "node:fs";
import { dirname } from "node:path";

type MultAgentStatus = "idle" | "running" | "waiting" | "error" | "finished";

const statusPath = process.env.MULT_AGENT_STATUS_PATH;
const chatId = process.env.MULT_AGENT_CHAT_ID;

/**
 * Refuse to write into a directory anyone but the owner can modify.
 *
 * mult creates this directory 0700 and verifies it, but the extension can win
 * the race and create it first — and `mkdirSync` used to do so with the default
 * 0755, leaving the directory that holds the executed hook script readable (and,
 * with a loose umask, writable) by others. Creating it 0700 is half the fix;
 * this is the other half, for the case where it already exists.
 */
function parentDirIsPrivate(dir: string): boolean {
  try {
    const stats = statSync(dir);
    if (!stats.isDirectory()) return false;
    if (typeof process.getuid === "function" && stats.uid !== process.getuid()) {
      return false;
    }
    return (stats.mode & 0o077) === 0;
  } catch {
    return false;
  }
}

function emitStatus(status: MultAgentStatus, detail?: string) {
  if (!statusPath) return;

  const dir = dirname(statusPath);
  let tempPath: string | undefined;
  try {
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    if (!parentDirIsPrivate(dir)) return;

    const payload = `${JSON.stringify({
      version: 1,
      status,
      chatId,
      detail,
      timestamp: Date.now(),
    })}\n`;
    // A random name plus O_EXCL|O_NOFOLLOW: the old `<path>.<pid>.tmp` was
    // predictable and opened with a plain truncating create, so a symlink
    // planted at that name redirected — and truncated — whatever it pointed at.
    tempPath = `${statusPath}.${randomBytes(8).toString("hex")}.tmp`;
    const flags =
      constants.O_CREAT |
      constants.O_EXCL |
      constants.O_WRONLY |
      constants.O_NOFOLLOW;
    const fd = openSync(tempPath, flags, 0o600);
    try {
      writeSync(fd, payload, null, "utf8");
    } finally {
      closeSync(fd);
    }
    renameSync(tempPath, statusPath);
    tempPath = undefined;
  } catch {
    // Keep the extension silent: status reporting should never disturb pi.
  } finally {
    if (tempPath !== undefined) {
      try {
        unlinkSync(tempPath);
      } catch {
        // Nothing more to do; the leftover is a 0600 file in a private dir.
      }
    }
  }
}

export default function (pi: ExtensionAPI) {
  emitStatus("idle", "extension loaded");

  pi.on("session_start", () => {
    emitStatus("idle", "session started");
  });

  pi.on("input", () => {
    emitStatus("running", "input received");
  });

  pi.on("before_agent_start", () => {
    emitStatus("running", "agent starting");
  });

  pi.on("agent_start", () => {
    emitStatus("running", "agent running");
  });

  pi.on("turn_start", () => {
    emitStatus("running", "turn running");
  });

  pi.on("tool_execution_start", (event) => {
    emitStatus("running", `tool: ${event.toolName}`);
  });

  pi.on("tool_execution_end", (event) => {
    emitStatus(event.isError ? "error" : "running", `tool: ${event.toolName}`);
  });

  pi.on("after_provider_response", (event) => {
    if (event.status >= 400) {
      emitStatus("error", `provider response: ${event.status}`);
    }
  });

  pi.on("agent_end", () => {
    emitStatus("finished", "agent finished");
  });
}
