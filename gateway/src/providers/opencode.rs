use crate::types::{DecisionStatus, HookOutput, ParseError, ToolHookEvent, build_display_name};
use protocol::{OpenCodeHookInput, OpenCodeHookOutput, Tool, ToolCall};

impl TryFrom<OpenCodeHookInput> for ToolHookEvent {
    type Error = ParseError;

    fn try_from(input: OpenCodeHookInput) -> Result<Self, Self::Error> {
        let tool: Tool = input.tool.into();
        let tool_call =
            ToolCall::try_from((tool, input.tool_input)).map_err(|e| ParseError(e.to_string()))?;

        // OpenCode sends workspace_roots; fall back to cwd
        let workspace_roots = input
            .workspace_roots
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| vec![input.cwd.clone()]);

        // Prefer session_title if available, else build from session_id + roots
        let session_display_name = input
            .session_title
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| build_display_name(&input.session_id, &workspace_roots));

        Ok(ToolHookEvent {
            session_id: input.session_id,
            session_display_name,
            tool_call,
            cwd: input.cwd,
            workspace_roots,
            hook_event_name: input.hook_event_name,
        })
    }
}

/// Format a HookOutput into Opencode's wire format (stdout JSON).
///
/// Opencode currently has inline_approval = false due to the tool.execute.before
/// race condition. Once the upstream fix lands this will need to return a blocking
/// decision. For now we return the decision anyway so it's wired up and ready.
pub fn format_output(_event: &ToolHookEvent, decision: &HookOutput) -> String {
    let allowed = matches!(decision.status, DecisionStatus::Approved);
    let reason = match &decision.status {
        DecisionStatus::DeniedWithReason(r) => Some(r.clone()),
        DecisionStatus::Denied => Some("denied by policy".to_string()),
        DecisionStatus::Approved => None,
    };
    let output = OpenCodeHookOutput { allowed, reason };
    serde_json::to_string(&output).expect("OpenCodeHookOutput is always serializable")
}
