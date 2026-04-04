use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::presence::PresenceState;

/// Runtime-mutable notification config.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct NotifyConfig {
    pub stop_enabled: bool,
    pub permission_enabled: bool,
    /// Delay in seconds before sending permission notifications (0 = immediate).
    pub notification_delay_secs: u64,
}

impl NotifyConfig {
    pub fn with_delay(delay_secs: u64) -> Self {
        Self {
            stop_enabled: true,
            permission_enabled: true,
            notification_delay_secs: delay_secs,
        }
    }

    pub fn apply(&mut self, update: NotifyConfigUpdate) {
        if let Some(v) = update.stop_enabled {
            self.stop_enabled = v;
        }
        if let Some(v) = update.permission_enabled {
            self.permission_enabled = v;
        }
        if let Some(v) = update.notification_delay_secs {
            self.notification_delay_secs = v;
        }
    }
}

/// Partial update for NotifyConfig (PUT /api/v1/config).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NotifyConfigUpdate {
    pub stop_enabled: Option<bool>,
    pub permission_enabled: Option<bool>,
    pub notification_delay_secs: Option<u64>,
}

/// GET /api/v1/config — response (notify config + presence).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigResponse {
    #[serde(flatten)]
    pub notify: NotifyConfig,
    pub presence: PresenceState,
}
