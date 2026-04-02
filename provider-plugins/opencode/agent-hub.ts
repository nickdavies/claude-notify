/**
 * Agent Hub opencode plugin.
 *
 * Routes every permission.ask decision through agent-hub-gateway so that
 * opencode's tool approvals run through the same rule engine and server
 * escalation path as Claude Code and Cursor.
 *
 * Environment variables (set in the shell that launches opencode):
 *   AGENT_HUB_GATEWAY   path to the gateway binary (default: "agent-hub-gateway")
 *   AGENT_HUB_SERVER    server URL (default: "http://localhost:8080")
 *   AGENT_HUB_TOKEN     bearer token for server auth (default: none)
 *   AGENT_HUB_CONFIG    path to tools.json rule config (optional)
 *
 * Installation: add to .opencode/config.json:
 *   { "plugin": ["file:///path/to/opencode-plugin"] }
 */

import type { Hooks, Plugin, PluginInput } from "@opencode-ai/plugin"
import type { PermissionRequest } from "@opencode-ai/sdk/v2"
import type { OpencodeClient } from "@opencode-ai/sdk"
import path from "path"
import fs from "fs"
import { spawn, type ChildProcess } from "child_process"

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
// Gateway subprocess tracking — kill on session abort
// ---------------------------------------------------------------------------

const procs: Map<string, Set<ChildProcess>> = new Map()

function track(sid: string, proc: ChildProcess) {
  let set = procs.get(sid)
  if (!set) {
    set = new Set()
    procs.set(sid, set)
  }
  set.add(proc)
}

function untrack(sid: string, proc: ChildProcess) {
  const set = procs.get(sid)
  if (!set) return
  set.delete(proc)
  if (set.size === 0) procs.delete(sid)
}

function killGateways(sid: string) {
  const set = procs.get(sid)
  if (!set || set.size === 0) return
  log("INFO ", "killing gateway subprocesses on abort", { sid, count: String(set.size) })
  for (const proc of set) {
    try { proc.kill("SIGTERM") } catch {}
  }
}

// ---------------------------------------------------------------------------
// Status reporting — fire-and-forget via gateway status-report subcommand
// ---------------------------------------------------------------------------

const statusTimers: Map<string, NodeJS.Timeout> = new Map()
const DEBOUNCE_MS = 2000

// Track sessions we've already registered on the server so the first
// permission.ask for a new session triggers an immediate "active" report.
const registeredSessions: Set<string> = new Set()

// On process exit, report all tracked sessions as ended so the dashboard
// doesn't leave stale "active" / "idle" entries after opencode shuts down.
let exitHandled = false
function onExit() {
  if (exitHandled) return
  exitHandled = true
  for (const sid of registeredSessions) {
    // Cancel any pending debounced reports — we want "ended" immediately.
    const pending = statusTimers.get(sid)
    if (pending) clearTimeout(pending)
    statusTimers.delete(sid)
    spawnStatusReport(sid, "ended")
  }
  registeredSessions.clear()
}
process.on("exit", onExit)
process.on("SIGINT", onExit)
process.on("SIGTERM", onExit)

function reportStatus(
  sid: string,
  status: "active" | "idle" | "waiting" | "ended",
  opts?: { waitingReason?: string; displayName?: string },
) {
  // "ended" should fire immediately — no debounce
  if (status === "ended") {
    const existing = statusTimers.get(sid)
    if (existing) clearTimeout(existing)
    statusTimers.delete(sid)
    spawnStatusReport(sid, status, opts)
    return
  }

  const existing = statusTimers.get(sid)
  if (existing) clearTimeout(existing)

  const timer = setTimeout(() => {
    statusTimers.delete(sid)
    spawnStatusReport(sid, status, opts)
  }, DEBOUNCE_MS)

  statusTimers.set(sid, timer)
}

function spawnStatusReport(
  sid: string,
  status: string,
  opts?: { waitingReason?: string; displayName?: string },
) {
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER ?? "http://localhost:8080"
  const token = process.env.AGENT_HUB_TOKEN ?? ""

  const payload = JSON.stringify({
    session_id: sid,
    cwd: process.cwd(),
    status,
    waiting_reason: opts?.waitingReason ?? null,
    display_name: opts?.displayName ?? null,
    editor_type: "opencode",
  })

  log("INFO ", "spawnStatusReport", { sid, status })

  const proc = spawn(
    bin,
    ["status-report", "--server", server, "--token", token],
    { stdio: ["pipe", "ignore", "pipe"] },
  )

  proc.stdin.end(payload)
  proc.stderr?.on("data", (chunk: Buffer) => {
    log("INFO ", "status-report stderr", { output: chunk.toString().trim() })
  })
  proc.on("error", (err: Error) => {
    log("WARN ", "status report spawn failed", { err: err.message })
  })
}

// ---------------------------------------------------------------------------
// Single-pattern gateway call
// ---------------------------------------------------------------------------

interface GatewayResult {
  allowed: boolean
  ask?: boolean   // true when gateway exited 1 (server unreachable — fall back to human)
  reason?: string
}

async function callGateway(sid: string, payload: object): Promise<GatewayResult> {
  const bin = process.env.AGENT_HUB_GATEWAY ?? "agent-hub-gateway"
  const server = process.env.AGENT_HUB_SERVER
  const token = process.env.AGENT_HUB_TOKEN
  const config = process.env.AGENT_HUB_CONFIG

  const args: string[] = ["approval", "--opencode"]
  args.push("--server", server ?? "http://localhost:8080")
  args.push("--token", token ?? "")
  if (config) args.push("--config", config)

  log("INFO ", "callGateway", { bin, config: config ?? "(none)" })

  return new Promise((resolve) => {
    const proc = spawn(bin, args, { stdio: ["pipe", "pipe", "pipe"] })
    track(sid, proc)

    let stdout = ""
    let stderr = ""
    proc.stdout.on("data", (chunk: Buffer) => { stdout += chunk.toString() })
    proc.stderr.on("data", (chunk: Buffer) => { stderr += chunk.toString() })

    proc.on("close", (code: number | null) => {
      untrack(sid, proc)
      if (stderr) log("INFO ", "gateway stderr", { output: stderr.trim() })
      // Exit 1 = server unreachable or approval timed out — deny, do not fall back to opencode ask.
      if (code === 1) {
        log("WARN ", "gateway denied (server unreachable or approval timed out)", { reason: stderr.trim() })
        return resolve({ allowed: false, reason: stderr.trim() || "gateway: server unreachable or timed out" })
      }
      // Exit 2 = fail-closed (bad input, config error, policy denial).
      if (code !== 0) return resolve({ allowed: false, reason: "gateway: fail-closed" })
      try {
        resolve(JSON.parse(stdout.trim()) as GatewayResult)
      } catch {
        log("ERROR", "gateway invalid response", { stdout: stdout.trim() })
        resolve({ allowed: false, reason: "gateway: invalid response" })
      }
    })

    proc.on("error", (err: Error) => {
      untrack(sid, proc)
      log("ERROR", "gateway spawn error — falling back to ask", { err: err.message, bin })
      resolve({ allowed: false, ask: true, reason: err.message })
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
  title?: string,
): object {
  const payload: Record<string, unknown> = {
    session_id: sessionId,
    tool_name: tool,
    tool_input: toolInput,
    cwd,
    workspace_roots: [cwd],
    hook_event_name: "permission.ask",
  }
  if (title) payload.session_title = title
  return payload
}

// Resolve a path value against base, but only if it is relative.
function resolvePath(value: string, base: string): string {
  return path.isAbsolute(value) ? value : path.resolve(base, value)
}

// Apply a GatewayResult to the hook output object.
function apply(r: GatewayResult, output: { status: "allow" | "deny" | "ask"; message?: string }) {
  output.status = r.ask ? "ask" : r.allowed ? "allow" : "deny"
  if (r.reason && !r.allowed && !r.ask) output.message = r.reason
}

// ---------------------------------------------------------------------------
// Session title cache — avoids repeated SDK calls for the same session.
// Stores the LLM-generated title once it's available; null means we haven't
// fetched a non-default title yet and should retry next time.
// ---------------------------------------------------------------------------

const DEFAULT_TITLE_RE = /^(New session - |Child session - )\d{4}-\d{2}-\d{2}T/

type V1Client = OpencodeClient

const titles: Map<string, string> = new Map()

async function fetchTitle(
  client: V1Client,
  sid: string,
  dir: string,
): Promise<string | undefined> {
  const cached = titles.get(sid)
  if (cached) {
    log("INFO ", "fetchTitle cache hit", { sid, title: cached })
    return cached
  }

  log("INFO ", "fetchTitle calling session.get", { sid, directory: dir })
  try {
    // The plugin client is v1 — uses { path: { id }, query: { directory } }
    // and URL template /session/{id}.
    const res = await client.session.get({
      path: { id: sid },
      query: { directory: dir },
    })
    log("INFO ", "fetchTitle raw response", { sid, keys: Object.keys(res).join(","), hasData: String(!!res.data) })
    const title = res.data?.title
    log("INFO ", "fetchTitle got response", { sid, title: title ?? "(none)", hasData: String(!!res.data) })
    if (title && !DEFAULT_TITLE_RE.test(title)) {
      titles.set(sid, title)
      log("INFO ", "fetchTitle cached title", { sid, title })
      return title
    }
    log("INFO ", "fetchTitle title is default/empty, skipping cache", { sid })
    return undefined
  } catch (err) {
    log("WARN ", "failed to fetch session title", { sid, err: String(err) })
    return undefined
  }
}

// ---------------------------------------------------------------------------
// Plugin export
// ---------------------------------------------------------------------------

export const id = "agent-hub"

const server: Plugin = async (input: PluginInput): Promise<Hooks> => {
  // The runtime client is a v1 client (opencode imports from @opencode-ai/sdk, not /v2).
  const client = input.client as unknown as V1Client
  const { directory, worktree } = input

  return {
    "permission.ask": async (_info, output) => {
      log("INFO ", "permission.ask handler entered [v2]", { permission: (_info as any).permission ?? "?" })
      // The published Permission type from @opencode-ai/sdk still uses the old
      // field names (type/pattern). Cast to PermissionRequest which matches what
      // opencode actually sends at runtime (permission/patterns).
      const info = _info as unknown as PermissionRequest
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

      // First tool call for this session — immediately register it on the
      // server so it shows up in the session list right away, rather than
      // waiting for a debounced session.status event or an approval escalation.
      if (!registeredSessions.has(sid)) {
        registeredSessions.add(sid)
        spawnStatusReport(sid, "active")
      }

      log("INFO ", "about to call fetchTitle", { sid, tool })
      const title = await fetchTitle(client, sid, directory)
      log("INFO ", "fetchTitle returned", { sid, title: title ?? "(none)" })

      // -----------------------------------------------------------------
      // bash: use the raw unparsed command from metadata rather than the
      // per-subcommand tree-sitter fragments in patterns.
      // -----------------------------------------------------------------
      if (info.permission === "bash" && typeof info.metadata.command === "string") {
        const result = await callGateway(
          sid, makePayload(sid, tool, { command: info.metadata.command }, directory, title),
        )
        apply(result, output)
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
              sid, makePayload(sid, tool, { path: file.filePath }, directory, title),
            )
            if (!result.allowed) {
              apply(result, output)
              return
            }
          }
          output.status = "allow"
          return
        }

        // write/edit: scalar filepath in metadata (already absolute)
        if (typeof info.metadata.filepath === "string") {
          const result = await callGateway(
            sid, makePayload(sid, tool, { path: info.metadata.filepath }, directory, title),
          )
          apply(result, output)
          return
        }

        // Fallback: patterns entries are worktree-relative — resolve against worktree
        for (const pat of info.patterns) {
          const result = await callGateway(
            sid, makePayload(sid, tool, { path: resolvePath(pat, worktree) }, directory, title),
          )
          if (!result.allowed) {
            apply(result, output)
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
          sid, makePayload(sid, tool, { path: info.patterns[0] ?? "" }, directory, title),
        )
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // webfetch: metadata.url is the canonical arg the gateway expects.
      // -----------------------------------------------------------------
      if (info.permission === "webfetch" && typeof info.metadata.url === "string") {
        const result = await callGateway(
          sid, makePayload(sid, tool, { url: info.metadata.url }, directory, title),
        )
        apply(result, output)
        return
      }

      // -----------------------------------------------------------------
      // All other tools (glob, grep, websearch, codesearch, list, task,
      // todowrite, external_directory, lsp, skill, ...):
      // Single call with empty tool_input. get_matchable_args returns []
      // for tools not in TOOL_DEFS and the rule engine falls to the
      // configured default (typically "ask", escalating to the server).
      // We do NOT loop over info.pattern — those are opencode-internal
      // representations unrelated to what the gateway expects.
      // -----------------------------------------------------------------
      const result = await callGateway(sid, makePayload(sid, tool, {}, directory, title))
      apply(result, output)
    },

    event: async ({ event }) => {
      // session.status — map opencode statuses to agent-hub statuses
      if (event.type === "session.status") {
        const sid = event.properties.sessionID
        // opencode emits "idle", "busy", "retry"; map to agent-hub terms
        const mapped = event.properties.status.type === "idle" ? "idle" as const : "active" as const
        reportStatus(sid, mapped)
        return
      }

      // session.error — abort / error; kill gateways and report ended
      if (event.type === "session.error") {
        if (event.properties.error?.name !== "MessageAbortedError") return
        const sid = event.properties.sessionID
        if (sid) {
          registeredSessions.delete(sid)
          killGateways(sid)
          reportStatus(sid, "ended")
        }
      }
    },
  }
}

export default { id, server }
