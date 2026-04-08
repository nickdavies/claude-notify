use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// User presence state for notification suppression.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    Present,
    Idle,
    Away,
}

/// POST /api/v1/presence — request body.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PresenceUpdate {
    pub state: PresenceState,
}
