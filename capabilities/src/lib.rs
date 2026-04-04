//! Provider capability flags and trait.
//!
//! This crate defines the `Provider` trait and the boolean capability flags
//! that describe what each agent-coding-tool provider supports. The trait is
//! intentionally minimal today; it will grow as provider-specific behaviour
//! (e.g. rich context, plan-mode questions) is wired up.

// Re-export commonly used protocol types for convenience.
pub use protocol::ApprovalContext;
pub use protocol::Tool;

// --- Provider capabilities and trait ---

pub struct ProviderCapabilities {
    /// Hook can return approve/deny inline and block the agent.
    pub inline_approval: bool,
    /// Agent shows its own approval UI before the hook fires (informational only).
    pub agent_ui_prompt: bool,
    /// Provider exposes plan-mode questions through a hookable surface.
    pub plan_questions: bool,
    /// Hook payload carries rich conversation context beyond tool args.
    pub rich_context: bool,
}

pub trait Provider: Send + Sync + 'static {
    /// Short identifier used on the --flag and in log/server output.
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> &ProviderCapabilities;
}

// --- Known providers ---

pub struct ClaudeCode;

impl Provider for ClaudeCode {
    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &ProviderCapabilities {
            inline_approval: true,
            agent_ui_prompt: false,
            plan_questions: false,
            rich_context: true,
        }
    }
}

pub struct Cursor;

impl Provider for Cursor {
    fn name(&self) -> &'static str {
        "cursor"
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &ProviderCapabilities {
            inline_approval: true,
            agent_ui_prompt: false,
            plan_questions: false,
            rich_context: false,
        }
    }
}

pub struct Opencode;

impl Provider for Opencode {
    fn name(&self) -> &'static str {
        "opencode"
    }

    fn capabilities(&self) -> &ProviderCapabilities {
        &ProviderCapabilities {
            inline_approval: false,
            agent_ui_prompt: true,
            plan_questions: false,
            rich_context: false,
        }
    }
}
