use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::sessions::{Provider, SessionId, SessionStatus};

/// Payload for the stop hook (POST /hooks/stop).
///
/// Requires `session_id` and `cwd` so the server can identify and register the
/// session. The `editor_type` is optional and used for session registration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StopPayload {
    pub session_id: SessionId,
    pub cwd: String,
    pub editor_type: Option<Provider>,
}

/// Payload for the session-end hook (POST /hooks/session-end).
///
/// Only the `session_id` is required — the server uses it to cancel pending
/// notifications and mark the session as ended.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionEndPayload {
    pub session_id: SessionId,
}

/// Payload for the notification hook (POST /hooks/notification).
///
/// Requires `session_id` and `cwd` for session identification. The `message`
/// field is used as the notification body text (defaults to "Permission prompt"
/// if absent).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotificationPayload {
    pub session_id: SessionId,
    pub cwd: String,
    pub message: Option<String>,
    pub editor_type: Option<Provider>,
}

/// POST /api/v1/hooks/status — request body sent by the gateway (status-report subcommand).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StatusReport {
    pub session_id: SessionId,
    pub cwd: String,
    pub status: SessionStatus,
    pub waiting_reason: Option<String>,
    pub display_name: Option<String>,
    pub editor_type: Option<Provider>,
}
