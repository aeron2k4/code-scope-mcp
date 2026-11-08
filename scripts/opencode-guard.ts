// OpenCode guard installed by astlens (`astlens --install`), so the model uses
// the astlens MCP tools for source code. It maps OpenCode's read/grep/shell
// tools onto the shared astlens-guard.sh and turns a deny into a failed call.
import { spawn } from "node:child_process"
import { fileURLToPath } from "node:url"

import { Plugin } from "@opencode/plugin"

const guard = fileURLToPath(new URL("./astlens-guard.sh", import.meta.url))

interface GuardOutput {
  hookSpecificOutput?: {
    permissionDecision?: string
    permissionDecisionReason?: string
  }
}

/// A guard that has not answered by then lets the call run.
const GUARD_TIMEOUT_MS = 5000

/// The guard's stdout, or undefined when it failed or ran out of time. Async,
/// so that a slow guard never blocks OpenCode's other sessions or its UI.
function runGuard(payload: string): Promise<string | undefined> {
  return new Promise((resolve) => {
    // Its own process group, so that a timeout also stops the programs the
    // guard started, which would otherwise keep stdout open. Windows has no
    // groups, and detached would open a console there.
    const group = process.platform !== "win32"
    let child: ReturnType<typeof spawn>
    try {
      child = spawn("bash", [guard], {
        env: { ...process.env, ASTLENS_OPENCODE: "1" },
        stdio: ["pipe", "pipe", "ignore"],
        detached: group,
      })
    } catch {
      resolve(undefined)
      return
    }
    const timer = setTimeout(() => {
      try {
        if (group && child.pid) process.kill(-child.pid, "SIGKILL")
        else child.kill("SIGKILL")
      } catch {
        // Already gone.
      }
      resolve(undefined)
    }, GUARD_TIMEOUT_MS)
    let stdout = ""
    child.stdout?.setEncoding("utf8")
    child.stdout?.on("data", (chunk: string) => {
      stdout += chunk
    })
    child.on("error", () => {
      clearTimeout(timer)
      resolve(undefined)
    })
    child.on("close", (code) => {
      clearTimeout(timer)
      resolve(code === 0 && stdout ? stdout : undefined)
    })
    // A guard that exits without reading its input must not crash the plugin.
    child.stdin?.on("error", () => {})
    child.stdin?.end(payload)
  })
}

/// The reason to reject the call, or undefined to let it run. The guard fails
/// open: a missing bash, jq or script, or a guard that hangs, never blocks the
/// tool.
async function rejected(
  tool: string,
  input: Record<string, unknown>,
  cwd: string,
): Promise<string | undefined> {
  let toolName: string
  let toolInput: Record<string, unknown>
  switch (tool) {
    case "read":
      toolName = "Read"
      toolInput = { file_path: input.path, limit: input.limit }
      break
    case "grep":
      toolName = "Grep"
      toolInput = { path: input.path, glob: input.include }
      break
    case "shell":
      toolName = "Bash"
      // The command starts in workdir, but the workspace stays cwd: the guard
      // resolves workdir itself.
      toolInput = { command: input.command, workdir: input.workdir }
      break
    default:
      return undefined
  }

  const stdout = await runGuard(JSON.stringify({ tool_name: toolName, cwd, tool_input: toolInput }))
  if (!stdout) return undefined
  try {
    const answer = JSON.parse(stdout) as GuardOutput
    const decision = answer.hookSpecificOutput
    if (decision?.permissionDecision !== "deny") return undefined
    return decision.permissionDecisionReason || "Blocked by astlens; use the astlens MCP tools instead."
  } catch {
    return undefined
  }
}

const ACTION_PREFIX = "astlens_guard_"

export default Plugin.define({
  id: "astlens-guard",
  async setup(ctx) {
    // Our own permission is always denied, whatever the user's rules say: an
    // allow rule such as "*" would let the call through, and the default
    // "ask" would prompt the user on every blocked read.
    await ctx.permission.hook("evaluate", async (event) => {
      if (!event.action.startsWith(ACTION_PREFIX)) return
      event.effect = "deny"
      const reason = event.metadata?.reason
      event.message = typeof reason === "string" ? reason : "Blocked by astlens."
    })

    await ctx.tool.hook("execute.before", async (event) => {
      const input =
        typeof event.input === "object" && event.input !== null
          ? (event.input as Record<string, unknown>)
          : {}
      const reason = await rejected(event.tool, input, ctx.location.directory)
      if (!reason) return
      // V2 leaves a failed call to the agent, which can retry it; a denied
      // permission ends the step with the reason instead. Nothing is saved,
      // so later calls are judged afresh.
      await ctx.permission.assert({
        sessionID: event.sessionID,
        agent: event.agent,
        source: { type: "tool", messageID: event.messageID, id: event.id },
        action: `${ACTION_PREFIX}${event.tool}`,
        resources: ["*"],
        metadata: { reason },
      })
      throw new Error(reason)
    })
  },
})
