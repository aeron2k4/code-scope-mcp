// OpenCode guard installed by astlens (`astlens --install`), so the model uses
// the astlens MCP tools for source code. It maps OpenCode's read/grep/shell
// tools onto the shared astlens-guard.sh and turns a deny into a failed call.
import { spawnSync } from "node:child_process"
import { fileURLToPath } from "node:url"

import { Plugin } from "@opencode/plugin"

const guard = fileURLToPath(new URL("./astlens-guard.sh", import.meta.url))

interface GuardOutput {
  hookSpecificOutput?: {
    permissionDecision?: string
    permissionDecisionReason?: string
  }
}

/// The reason to reject the call, or undefined to let it run. The guard fails
/// open: a missing bash, jq or script never blocks the tool.
function rejected(tool: string, input: Record<string, unknown>, cwd: string): string | undefined {
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

  const payload = JSON.stringify({ tool_name: toolName, cwd, tool_input: toolInput })
  const result = spawnSync("bash", [guard], {
    input: payload,
    encoding: "utf8",
    env: { ...process.env, ASTLENS_OPENCODE: "1" },
  })
  if (result.error || result.status !== 0 || !result.stdout) return undefined

  try {
    const answer = JSON.parse(result.stdout) as GuardOutput
    const decision = answer.hookSpecificOutput
    if (decision?.permissionDecision !== "deny") return undefined
    return decision.permissionDecisionReason || "Blocked by astlens; use the astlens MCP tools instead."
  } catch {
    return undefined
  }
}

export default Plugin.define({
  id: "astlens-guard",
  async setup(ctx) {
    await ctx.tool.hook("execute.before", async (event) => {
      const input =
        typeof event.input === "object" && event.input !== null
          ? (event.input as Record<string, unknown>)
          : {}
      const reason = rejected(event.tool, input, ctx.location.directory)
      if (!reason) return
      // V2 leaves a failed call to the agent, which can retry it; asking for
      // a permission that no rule allows ends the step with the reason, as a
      // deny rule does. Nothing is saved, so later calls are judged afresh.
      await ctx.permission.assert({
        sessionID: event.sessionID,
        agent: event.agent,
        source: { type: "tool", messageID: event.messageID, id: event.id },
        action: `astlens_guard_${event.tool}`,
        resources: ["*"],
        message: reason,
      })
      throw new Error(reason)
    })
  },
})
