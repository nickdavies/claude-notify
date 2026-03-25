use std::io::Read;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Approval hook for Claude Code and Cursor.
///
/// Exit codes:
///   0 = success (output on stdout if decision made, no output for fall-through)
///   2 = block (error occurred, fail-closed)
#[derive(Parser)]
#[command(name = "claude-approve", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Standalone approval hook — checks session mode and requests remote approval
    Approve(ApproveArgs),
    /// Route tool calls via config file — delegates, auto-approves, or requests remote approval
    Delegate(DelegateArgs),
}

#[derive(Args)]
struct SharedArgs {
    /// Server URL (e.g. https://notify.example.com)
    #[arg(long, env = "CLAUDE_NOTIFY_SERVER")]
    server: String,

    /// Bearer token for server auth
    #[arg(long, env = "CLAUDE_NOTIFY_TOKEN")]
    token: String,

    /// Maximum time to wait for approval in seconds
    #[arg(long, default_value = "600")]
    timeout: u64,

    /// Output format
    #[arg(long)]
    format: OutputFormat,
}

#[derive(Clone, ValueEnum)]
enum OutputFormat {
    Cursor,
    Claude,
}

#[derive(Args)]
struct ApproveArgs {
    #[command(flatten)]
    shared: SharedArgs,
}

#[derive(Args)]
struct DelegateArgs {
    #[command(flatten)]
    shared: SharedArgs,

    /// Path to tool routing config file (JSON)
    #[arg(long)]
    config: String,
}

// --- Wire types ---

/// Minimal hook payload (stdin). Accepts both Claude Code and Cursor field names.
#[derive(Deserialize)]
struct HookInput {
    session_id: Option<String>,
    conversation_id: Option<String>,
    hook_event_name: Option<String>,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
    cwd: Option<String>,
}

impl HookInput {
    fn session_id(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .or(self.conversation_id.as_deref())
    }
}

#[derive(Deserialize)]
struct ApprovalModeResponse {
    approval_mode: String,
}

#[derive(Serialize)]
struct ApprovalRequest {
    request_id: String,
    session_id: String,
    cwd: String,
    tool_name: String,
    tool_input: serde_json::Value,
}

#[derive(Deserialize)]
struct ApprovalResponse {
    id: Uuid,
    #[serde(rename = "type")]
    status_type: String,
    message: Option<String>,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct WaitResponse {
    #[serde(rename = "type")]
    status_type: String,
    message: Option<String>,
    reason: Option<String>,
}

// --- Tool routing config ---

#[derive(Deserialize)]
struct ToolConfig {
    #[allow(dead_code)]
    version: u32,
    default: DefaultAction,
    rules: Vec<Rule>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "snake_case")]
enum DefaultAction {
    Allow,
    Deny,
    Ask,
}

#[derive(Deserialize)]
struct Rule {
    tools: Vec<String>,
    action: RuleAction,
    command: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuleAction {
    Allow,
    Deny,
    Ask,
    Delegate,
}

enum ResolvedAction {
    Allow,
    Deny,
    Ask,
    Delegate(String),
}

fn load_tool_config(path: &str) -> Result<ToolConfig, String> {
    let contents =
        std::fs::read_to_string(path).map_err(|e| format!("failed to read config {path}: {e}"))?;
    let config: ToolConfig = serde_json::from_str(&contents)
        .map_err(|e| format!("failed to parse config {path}: {e}"))?;
    for rule in &config.rules {
        if matches!(rule.action, RuleAction::Delegate) && rule.command.is_none() {
            return Err(format!(
                "rule for {:?} has action 'delegate' but no 'command'",
                rule.tools
            ));
        }
    }
    Ok(config)
}

fn resolve_action(config: &ToolConfig, tool_name: &str) -> ResolvedAction {
    for rule in &config.rules {
        if rule.tools.iter().any(|t| t == tool_name) {
            return match &rule.action {
                RuleAction::Allow => ResolvedAction::Allow,
                RuleAction::Deny => ResolvedAction::Deny,
                RuleAction::Ask => ResolvedAction::Ask,
                RuleAction::Delegate => {
                    ResolvedAction::Delegate(rule.command.clone().unwrap())
                }
            };
        }
    }
    default_to_resolved(&config.default)
}

fn default_to_resolved(default: &DefaultAction) -> ResolvedAction {
    match default {
        DefaultAction::Allow => ResolvedAction::Allow,
        DefaultAction::Deny => ResolvedAction::Deny,
        DefaultAction::Ask => ResolvedAction::Ask,
    }
}

// --- Entrypoint ---

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Approve(args) => run_approve(&args).await,
        Command::Delegate(args) => run_delegate(&args).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("claude-approve: {e}");
            ExitCode::from(2)
        }
    }
}

// --- Approve subcommand ---

async fn run_approve(args: &ApproveArgs) -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed to read stdin: {e}"))?;

    let payload: HookInput =
        serde_json::from_str(&input).map_err(|e| format!("failed to parse hook payload: {e}"))?;

    let session_id = payload
        .session_id()
        .ok_or("missing session_id/conversation_id in payload")?;

    let client = reqwest::Client::new();
    let base = args.shared.server.trim_end_matches('/');

    // Check session approval mode
    let mode_url = format!("{base}/api/v1/sessions/{session_id}/approval-mode");
    let mode_resp = client
        .get(&mode_url)
        .bearer_auth(&args.shared.token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("failed to check approval mode: {e}"))?;

    if !mode_resp.status().is_success() {
        eprintln!(
            "claude-approve: session {session_id} not found on server ({}), defaulting to remote",
            mode_resp.status()
        );
    } else {
        let mode: ApprovalModeResponse = mode_resp
            .json()
            .await
            .map_err(|e| format!("failed to parse approval mode: {e}"))?;

        if mode.approval_mode == "terminal" {
            // Fall through: exit 0 with no output -> host shows normal dialog
            return Ok(());
        }
    }

    // Remote mode: register approval and wait
    let (status_type, message, reason) =
        request_remote_approval(&client, &args.shared, &payload).await?;

    let hook_event = payload.hook_event_name.as_deref().unwrap_or("PreToolUse");
    let output = format_output(
        &args.shared.format,
        hook_event,
        &status_type,
        message.as_deref(),
        reason.as_deref(),
    )?;
    print!("{output}");
    Ok(())
}

// --- Delegate subcommand ---

async fn run_delegate(args: &DelegateArgs) -> Result<(), String> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .map_err(|e| format!("failed to read stdin: {e}"))?;

    let payload: HookInput =
        serde_json::from_str(&input).map_err(|e| format!("failed to parse hook payload: {e}"))?;

    let config = load_tool_config(&args.config)?;
    let tool_name = payload.tool_name.as_deref().unwrap_or("unknown");
    let action = resolve_action(&config, tool_name);

    match action {
        ResolvedAction::Allow | ResolvedAction::Deny | ResolvedAction::Ask => {
            execute_simple_action(&action, &args.shared, &payload).await
        }
        ResolvedAction::Delegate(ref command) => {
            let result = spawn_delegate(command, &input, &args.shared.format).await?;

            match result.permission.as_deref() {
                Some("allow") | Some("deny") => {
                    print!("{}", result.raw_output);
                    Ok(())
                }
                Some("ask") => {
                    execute_simple_action(&ResolvedAction::Ask, &args.shared, &payload).await
                }
                None => {
                    let fallback = default_to_resolved(&config.default);
                    execute_simple_action(&fallback, &args.shared, &payload).await
                }
                Some(other) => Err(format!("unexpected permission from delegate: {other}")),
            }
        }
    }
}

/// Execute a non-delegate action (allow, deny, or remote approval).
async fn execute_simple_action(
    action: &ResolvedAction,
    shared: &SharedArgs,
    payload: &HookInput,
) -> Result<(), String> {
    let hook_event = payload.hook_event_name.as_deref().unwrap_or("PreToolUse");
    match action {
        ResolvedAction::Allow => {
            let output = format_output(&shared.format, hook_event, "approved", None, None)?;
            print!("{output}");
            Ok(())
        }
        ResolvedAction::Deny => {
            let output = format_output(
                &shared.format,
                hook_event,
                "denied",
                None,
                Some("denied by policy"),
            )?;
            print!("{output}");
            Ok(())
        }
        ResolvedAction::Ask => {
            let client = reqwest::Client::new();
            let (status_type, message, reason) =
                request_remote_approval_with_retry(&client, shared, payload).await?;
            let output = format_output(
                &shared.format,
                hook_event,
                &status_type,
                message.as_deref(),
                reason.as_deref(),
            )?;
            print!("{output}");
            Ok(())
        }
        ResolvedAction::Delegate(_) => unreachable!(),
    }
}

struct DelegateResult {
    permission: Option<String>,
    raw_output: String,
}

/// Spawn a delegate command, pipe hook input via stdin, and parse the result.
async fn spawn_delegate(
    command: &str,
    input: &str,
    format: &OutputFormat,
) -> Result<DelegateResult, String> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let (cmd, cmd_args) = parts.split_first().ok_or("empty delegate command")?;

    let mut child = tokio::process::Command::new(cmd)
        .args(cmd_args.iter())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn delegate '{cmd}': {e}"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(input.as_bytes())
            .await
            .map_err(|e| format!("failed to write to delegate stdin: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| format!("delegate command failed: {e}"))?;

    let stdout = String::from_utf8(output.stdout)
        .map_err(|e| format!("delegate output is not valid UTF-8: {e}"))?;

    if output.status.code() == Some(2) {
        return Ok(DelegateResult {
            permission: Some("deny".to_string()),
            raw_output: stdout,
        });
    }

    if !output.status.success() {
        return Err(format!("delegate exited with status {}", output.status));
    }

    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("failed to parse delegate output: {e}"))?;

    let permission = extract_permission(format, &json).map(String::from);

    Ok(DelegateResult {
        permission,
        raw_output: stdout,
    })
}

/// Extract the permission decision string from delegate output based on format.
fn extract_permission<'a>(format: &OutputFormat, json: &'a serde_json::Value) -> Option<&'a str> {
    match format {
        OutputFormat::Cursor => json.get("permission").and_then(|v| v.as_str()),
        OutputFormat::Claude => json
            .get("hookSpecificOutput")
            .and_then(|h| h.get("permissionDecision"))
            .and_then(|v| v.as_str()),
    }
}

// --- Shared approval logic ---

/// Register an approval request with the server and long-poll until resolved.
async fn request_remote_approval(
    client: &reqwest::Client,
    shared: &SharedArgs,
    payload: &HookInput,
) -> Result<(String, Option<String>, Option<String>), String> {
    let base = shared.server.trim_end_matches('/');
    let session_id = payload
        .session_id()
        .ok_or("missing session_id/conversation_id")?;
    let tool_name = payload.tool_name.as_deref().unwrap_or("unknown");
    let tool_input = payload
        .tool_input
        .clone()
        .unwrap_or(serde_json::Value::Object(Default::default()));
    let cwd = payload.cwd.as_deref().unwrap_or(".");

    let request_id = Uuid::new_v4().to_string();
    let register_url = format!("{base}/api/v1/hooks/approval");
    let register_resp = client
        .post(&register_url)
        .bearer_auth(&shared.token)
        .json(&ApprovalRequest {
            request_id,
            session_id: session_id.to_string(),
            cwd: cwd.to_string(),
            tool_name: tool_name.to_string(),
            tool_input,
        })
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("failed to register approval: {e}"))?;

    if !register_resp.status().is_success() {
        return Err(format!(
            "server returned {} for approval registration",
            register_resp.status()
        ));
    }

    let approval: ApprovalResponse = register_resp
        .json()
        .await
        .map_err(|e| format!("failed to parse approval response: {e}"))?;

    if approval.status_type != "pending" {
        return Ok((approval.status_type, approval.message, approval.reason));
    }

    // Long-poll until resolved or timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(shared.timeout);
    let wait_url = format!("{base}/api/v1/approvals/{}/wait", approval.id);

    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err("approval timed out".to_string());
        }

        let resp = client
            .get(&wait_url)
            .bearer_auth(&shared.token)
            .timeout(Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| format!("wait request failed: {e}"))?;

        let status = resp.status();
        let wait: WaitResponse = resp
            .json()
            .await
            .map_err(|e| format!("failed to parse wait response: {e}"))?;

        if status.as_u16() == 202 || wait.status_type == "pending" {
            continue;
        }

        return Ok((wait.status_type, wait.message, wait.reason));
    }
}

/// Wrapper with retry for transient failures (3 attempts, 2s between).
async fn request_remote_approval_with_retry(
    client: &reqwest::Client,
    shared: &SharedArgs,
    payload: &HookInput,
) -> Result<(String, Option<String>, Option<String>), String> {
    const MAX_ATTEMPTS: u32 = 3;
    const RETRY_DELAY: Duration = Duration::from_secs(2);

    let mut last_err = String::new();

    for attempt in 1..=MAX_ATTEMPTS {
        match request_remote_approval(client, shared, payload).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                last_err = e;
                if attempt < MAX_ATTEMPTS {
                    eprintln!(
                        "claude-approve: attempt {attempt}/{MAX_ATTEMPTS} failed: {last_err}, retrying in 2s"
                    );
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    Err(format!("failed after {MAX_ATTEMPTS} attempts: {last_err}"))
}

// --- Output formatting ---

/// Format the approval server response as hook output JSON.
fn format_output(
    format: &OutputFormat,
    hook_event: &str,
    status_type: &str,
    message: Option<&str>,
    reason: Option<&str>,
) -> Result<String, String> {
    match format {
        OutputFormat::Cursor => format_cursor_output(status_type, message, reason),
        OutputFormat::Claude => format_claude_output(hook_event, status_type, message, reason),
    }
}

fn format_cursor_output(
    status_type: &str,
    message: Option<&str>,
    reason: Option<&str>,
) -> Result<String, String> {
    let perm = match status_type {
        "approved" => "allow",
        "denied" => "deny",
        other => return Err(format!("unexpected approval status: {other}")),
    };
    let msg = reason
        .or(message)
        .unwrap_or("resolved via remote approval");
    let output = serde_json::json!({
        "permission": perm,
        "user_message": msg,
        "agent_message": msg,
    });
    serde_json::to_string(&output).map_err(|e| format!("JSON serialization failed: {e}"))
}

fn format_claude_output(
    hook_event: &str,
    status_type: &str,
    message: Option<&str>,
    reason: Option<&str>,
) -> Result<String, String> {
    match hook_event {
        "PermissionRequest" => {
            let decision = match status_type {
                "approved" => {
                    serde_json::json!({
                        "behavior": "allow"
                    })
                }
                "denied" => {
                    serde_json::json!({
                        "behavior": "deny",
                        "message": reason.or(message).unwrap_or("denied via remote approval")
                    })
                }
                other => return Err(format!("unexpected approval status: {other}")),
            };
            let output = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": decision
                }
            });
            serde_json::to_string(&output).map_err(|e| format!("JSON serialization failed: {e}"))
        }
        "PreToolUse" => {
            let (perm_decision, perm_reason) = match status_type {
                "approved" => ("allow", message.unwrap_or("")),
                "denied" => (
                    "deny",
                    reason.or(message).unwrap_or("denied via remote approval"),
                ),
                other => return Err(format!("unexpected approval status: {other}")),
            };
            let output = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": perm_decision,
                    "permissionDecisionReason": perm_reason
                }
            });
            serde_json::to_string(&output).map_err(|e| format!("JSON serialization failed: {e}"))
        }
        other => Err(format!("unsupported hook event: {other}")),
    }
}
