import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { closeSync, constants, lstatSync, openSync, writeSync } from "node:fs";

type MultAgentStatus = "idle" | "running" | "waiting" | "error" | "finished";

const MAX_STATUS_FILE_BYTES = 1024 * 1024;
const statusPath = process.env.MULT_AGENT_STATUS_PATH;
const version = process.env.MULT_AGENT_STATUS_VERSION;
const namespace = process.env.MULT_AGENT_NAMESPACE;
const sessionToken = process.env.MULT_AGENT_SESSION_TOKEN;
const chatId = process.env.MULT_AGENT_CHAT_ID;
const agentKind = process.env.MULT_AGENT_KIND;
const generation = process.env.MULT_AGENT_GENERATION;

function emitStatus(status: MultAgentStatus) {
  if (
    !statusPath ||
    !version ||
    !namespace ||
    !sessionToken ||
    !chatId ||
    !agentKind ||
    !generation
  ) {
    return;
  }

  try {
    // mult creates and validates this generation-specific owner-private file.
    // Never create the file or its parents here: a stale hook must not
    // resurrect a generation that mult has cleaned up. `lstat` + O_NOFOLLOW
    // keep the append inside that file — a symlink at this path is refused
    // rather than written through.
    const stats = lstatSync(statusPath);
    if (!stats.isFile() || stats.size >= MAX_STATUS_FILE_BYTES) return;
    const payload = `${JSON.stringify({
      version: Number(version),
      namespace,
      sessionToken,
      chatId,
      agentKind,
      generation,
      status,
    })}\n`;
    const fd = openSync(
      statusPath,
      constants.O_WRONLY | constants.O_APPEND | (constants.O_NOFOLLOW ?? 0),
    );
    try {
      writeSync(fd, payload, null, "utf8");
    } finally {
      closeSync(fd);
    }
  } catch {
    // Status reporting must never disturb pi.
  }
}

export default function (pi: ExtensionAPI) {
  emitStatus("idle");

  pi.on("session_start", () => {
    emitStatus("idle");
  });

  pi.on("input", () => {
    emitStatus("running");
  });

  pi.on("before_agent_start", () => {
    emitStatus("running");
  });

  pi.on("agent_start", () => {
    emitStatus("running");
  });

  pi.on("turn_start", () => {
    emitStatus("running");
  });

  pi.on("tool_execution_start", () => {
    emitStatus("running");
  });

  pi.on("tool_execution_end", (event) => {
    emitStatus(event.isError ? "error" : "running");
  });

  pi.on("after_provider_response", (event) => {
    if (event.status >= 400) {
      emitStatus("error");
    }
  });

  pi.on("agent_end", () => {
    emitStatus("finished");
  });
}
