/**
 * Agent Hub opencode plugin.
 *
 * Routes every permission.ask decision through agent-hub-gateway so that
 * opencode's tool approvals run through the same rule engine and server
 * escalation path as Claude Code and Cursor.
 *
 * Environment variables (set in the shell that launches opencode):
 *   AGENT_HUB_GATEWAY   path to the gateway binary (default: "agent-hub-gateway")
 *   AGENT_HUB_SERVER    server URL, e.g. https://hub.example.com
 *   AGENT_HUB_TOKEN     bearer token for server auth
 *   AGENT_HUB_CONFIG    path to tools.json rule config (optional)
 *
 * Installation: add to .opencode/config.json:
 *   { "plugin": ["file:///path/to/opencode-plugin"] }
 */

import type { Hooks, PluginInput, PluginModule } from "@opencode-ai/plugin"
import path from "path"
import fs from "fs"
import { spawn } from "child_process"

// ---------------------------------------------------------------------------
// Minimal structured logger — writes to the same dev.log opencode uses.
// Format matches opencode's Log.create output: LEVEL  ISO +Xms key=value msg
// ---------------------------------------------------------------------------

const logfile = path.join(
  process.env.XDG_DATA_HOME ?? path.join(process.env.HOME ?? "~", ".local", "share"),
  "opencode",
  "log",
  "dev.log",
)

let last = Date.now()
function log(level: "INFO " | "WARN " | "ERROR", msg: string, extra?: Record<string, string>) {
  const now = Date.now()
  const diff = now - last
  last = now
  const ts = new Date(now).toISOString().split(".")[0]
  const kv = Object.entries({ service: "agent-hub-plugin", ...extra })
    .map(([k, v]) => `${k}=${v}`)
    .join(" ")
  fs.appendFileSync(logfile, `${level} ${ts} +${diff}ms ${kv} ${msg}\n`)
}

// ---------------------------------------------------------------------------
// Permission name → canonical gateway tool name
// Must stay in sync with TOOL_NAME_MAP in gateway/src/providers/opencode.rs
// ---------------------------------------------------------------------------

const PERM_TO_TOOL: Record<string, string> = {
  bash: "Bash",
  edit: "Write",
  read: "Read",
  webfetch: "WebFetch",
}

// ---------------------------------------------------------------------------
// Single-pattern gateway call
// ---------------------------------------------------------------------------

interface GatewayResult {
  allowed: boolean
  reason?: string
}

async function callGateway(payload: object): Promise<GatewayResult> {
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER
  const token = process.env.AGENT_HUB_TOKEN
  const config = process.env.AGENT_HUB_CONFIG

  const args: string[] = ["--opencode"]
  args.push("--server", server ?? "http://localhost:0")
  args.push("--token", token ?? "none")
  if (config) args.push("--config", config)

  log("INFO ", "callGateway", { bin, config: config ?? "(none)" })

  return new Promise((resolve) => {
    const proc = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"] })

    let stdout = ""
    let stderr = ""
    proc.stdout.on("data", (chunk: Buffer) => { stdout += chunk.toString() })
    proc.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString() })

    proc.on("close", (code) => {
      if (stderr) log("INFO ", "gateway stderr", { output: stderr.trim() })
      if (code === 2) return resolve({ allowed: false, reason: "gateway: fail-closed" })
      if (code !== 0) return resolve({ allowed: false, reason: `gateway: exit ${code}` })
      try {
        resolve(JSON.parse(stdout.trim()) as GatewayResult)
      } catch {
        log("ERROR", "gateway invalid response", { stdout: stdout.trim() })
        resolve({ allowed: false, reason: "gateway: invalid response" })
      }
    })

    proc.on("error", (err) => {
      log("ERROR", "gateway spawn error", { err: err.message, bin })
      resolve({ allowed: false, reason: `gateway: ${err.message}` })
    })

    proc.stdin.end(JSON.stringify(payload))
  })
}

// ---------------------------------------------------------------------------
// Build a gateway payload for a single tool_input value
// ---------------------------------------------------------------------------

function makePayload(
  sessionId: string,
  tool: string,
  toolInput: Record<string, string>,
  cwd: string,
): object {
  return {
    session_id: sessionId,
    tool_name: tool,
    tool_input: toolInput,
    cwd,
    workspace_roots: [cwd],
    hook_event_name: "permission.ask",
  }
}

// Resolve a path value against base, but only if it is relative.
function resolvePath(value: string, base: string): string {
  return path.isAbsolute(value) ? value : path.resolve(base, value)
}

// ---------------------------------------------------------------------------
// Plugin export (new PluginModule format)
// ---------------------------------------------------------------------------

export const id = "agent-hub"

const server: PluginModule["server"] = async (input: PluginInput): Promise<Hooks> => {
  const { directory, worktree } = input

  return {
    "permission.ask": async (info, output) => {
      // external_directory is opencode-internal — the in_workspace distinction
      // is already handled by the gateway's in_workspace flag on file rules.
      // Auto-approve it here; the subsequent read/write permission will be
      // sent to the gateway with the correct path for rule evaluation.
      if (info.permission === "external_directory") {
        output.status = "allow"
        return
      }

      const tool = PERM_TO_TOOL[info.permission] ?? info.permission
      const sid = info.sessionID

      // -----------------------------------------------------------------
      // bash: use the raw unparsed command from metadata rather than the
      // per-subcommand tree-sitter fragments in patterns.
      // -----------------------------------------------------------------
      if (info.permission === "bash" && typeof info.metadata.command === "string") {
        const result = await callGateway(
          makePayload(sid, tool, { command: info.metadata.command }, directory),
        )
        if (!result.allowed) {
          output.status = "deny"
          output.message = result.reason ?? "denied by agent-hub policy"
        } else {
          output.status = "allow"
        }
        return
      }

      // -----------------------------------------------------------------
      // edit (write.ts / edit.ts / apply_patch.ts):
      //   metadata.filepath is the absolute path (scalar, for write/edit).
      //   metadata.files is an array of per-file objects (for apply_patch).
      //   patterns contains worktree-relative paths as fallback.
      // -----------------------------------------------------------------
      if (info.permission === "edit") {
        // apply_patch: rich per-file metadata
        if (Array.isArray(info.metadata.files)) {
          for (const file of info.metadata.files as Array<{ filePath: string }>) {
            const result = await callGateway(
              makePayload(sid, tool, { path: file.filePath }, directory),
            )
            if (!result.allowed) {
              output.status = "deny"
              output.message = result.reason ?? "denied by agent-hub policy"
              return
            }
          }
          output.status = "allow"
          return
        }

        // write/edit: scalar filepath in metadata (already absolute)
        if (typeof info.metadata.filepath === "string") {
          const result = await callGateway(
            makePayload(sid, tool, { path: info.metadata.filepath }, directory),
          )
          if (!result.allowed) {
            output.status = "deny"
            output.message = result.reason ?? "denied by agent-hub policy"
          } else {
            output.status = "allow"
          }
          return
        }

        // Fallback: patterns are worktree-relative — resolve against worktree
        for (const pattern of info.patterns) {
          const result = await callGateway(
            makePayload(sid, tool, { path: resolvePath(pattern, worktree) }, directory),
          )
          if (!result.allowed) {
            output.status = "deny"
            output.message = result.reason ?? "denied by agent-hub policy"
            return
          }
        }
        output.status = "allow"
        return
      }

      // -----------------------------------------------------------------
      // read: patterns[0] is already an absolute path.
      // -----------------------------------------------------------------
      if (info.permission === "read") {
        const result = await callGateway(
          makePayload(sid, tool, { path: info.patterns[0] ?? "" }, directory),
        )
        if (!result.allowed) {
          output.status = "deny"
          output.message = result.reason ?? "denied by agent-hub policy"
          return
        }
        output.status = "allow"
        return
      }

      // -----------------------------------------------------------------
      // webfetch: metadata.url is the canonical arg the gateway expects.
      // -----------------------------------------------------------------
      if (info.permission === "webfetch" && typeof info.metadata.url === "string") {
        const result = await callGateway(
          makePayload(sid, tool, { url: info.metadata.url }, directory),
        )
        if (!result.allowed) {
          output.status = "deny"
          output.message = result.reason ?? "denied by agent-hub policy"
          return
        }
        output.status = "allow"
        return
      }

      // -----------------------------------------------------------------
      // All other tools (glob, grep, websearch, codesearch, list, task,
      // todowrite, external_directory, lsp, skill, ...):
      // Single call with empty tool_input. get_matchable_args returns []
      // for tools not in TOOL_DEFS and the rule engine falls to the
      // configured default (typically "ask", escalating to the server).
      // We do NOT loop over info.patterns — those are opencode-internal
      // representations unrelated to what the gateway expects.
      // -----------------------------------------------------------------
      const result = await callGateway(makePayload(sid, tool, {}, directory))
      if (!result.allowed) {
        output.status = "deny"
        output.message = result.reason ?? "denied by agent-hub policy"
        return
      }
      output.status = "allow"
    },
  }
}

export default { server } satisfies PluginModule
