use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use strum::{Display, EnumString};

// ===========================================================================
// SessionId — opaque session identifier newtype
// ===========================================================================

/// Opaque session identifier.
///
/// Wraps a `String` with `#[serde(transparent)]` so the JSON wire format
/// stays a bare string — no migration needed for TypeScript plugins, shell
/// scripts, or persisted state files.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    /// Create a new `SessionId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// View the underlying string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for SessionId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl PartialEq<str> for SessionId {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for SessionId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

// ===========================================================================
// Provider — which editor/agent owns a session
// ===========================================================================

/// The editor/agent that owns a session.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Default,
    JsonSchema,
    Display,
    EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Provider {
    Claude,
    Cursor,
    Opencode,
    #[default]
    Unknown,
}

/// Backwards-compatible alias while downstream code migrates.
pub type EditorType = Provider;

/// Stored session status as reported by the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Active,
    Idle,
    Waiting,
    Ended,
}

/// Effective session status after server-side resolution (e.g. pending approvals
/// override to Waiting). Used in API responses to the web UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum EffectiveSessionStatus {
    Active,
    Idle,
    Waiting { reason: Option<String> },
    Ended,
}

/// Per-session approval mode: remote (web UI) or terminal (CLI).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionApprovalMode {
    #[default]
    Remote,
    Terminal,
}

/// Per-session notification and approval config.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionNotifyConfig {
    pub stop_enabled: bool,
    pub permission_enabled: bool,
    #[serde(default)]
    pub approval_mode: SessionApprovalMode,
}

impl SessionNotifyConfig {
    pub fn with_default_approval_mode(mode: SessionApprovalMode) -> Self {
        Self {
            stop_enabled: true,
            permission_enabled: true,
            approval_mode: mode,
        }
    }
}

impl Default for SessionNotifyConfig {
    fn default() -> Self {
        Self {
            stop_enabled: true,
            permission_enabled: true,
            approval_mode: SessionApprovalMode::default(),
        }
    }
}

/// Partial update for per-session config (PUT /api/v1/sessions/{id}).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionConfigUpdate {
    pub stop_enabled: Option<bool>,
    pub permission_enabled: Option<bool>,
    pub approval_mode: Option<SessionApprovalMode>,
}

impl SessionNotifyConfig {
    pub fn apply(&mut self, update: &SessionConfigUpdate) {
        if let Some(v) = update.stop_enabled {
            self.stop_enabled = v;
        }
        if let Some(v) = update.permission_enabled {
            self.permission_enabled = v;
        }
        if let Some(v) = update.approval_mode {
            self.approval_mode = v;
        }
    }
}

/// API response for a session (with effective status resolved).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SessionView {
    pub session_id: SessionId,
    pub project: String,
    pub config: SessionNotifyConfig,
    pub editor_type: Provider,
    pub status: EffectiveSessionStatus,
    pub display_name: Option<String>,
}

/// GET /api/v1/sessions/{id}/approval-mode — response.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalModeResponse {
    pub approval_mode: SessionApprovalMode,
}
