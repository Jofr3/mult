import type { ExtensionAPI } from "@mariozechner/pi-coding-agent";
import { mkdirSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

type MultAgentStatus = "idle" | "running" | "waiting" | "error" | "finished";

const statusPath = process.env.MULT_AGENT_STATUS_PATH;
const chatId = process.env.MULT_AGENT_CHAT_ID;

function emitStatus(status: MultAgentStatus, detail?: string) {
  if (!statusPath) return;

  try {
    mkdirSync(dirname(statusPath), { recursive: true });
    const payload = `${JSON.stringify({
      version: 1,
      status,
      chatId,
      detail,
      timestamp: Date.now(),
    })}\n`;
    const tempPath = `${statusPath}.${process.pid}.tmp`;
    writeFileSync(tempPath, payload, "utf8");
    renameSync(tempPath, statusPath);
  } catch {
    // Keep the extension silent: status reporting should never disturb pi.
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
