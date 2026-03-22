use std::env;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::error::AppError;

/// Immutable server configuration loaded from environment at startup.
pub struct ServerConfig {
    pub pushover_token: String,
    pub pushover_user: String,
    pub tokens: Vec<Token>,
    pub listen_addr: String,
    pub presence_ttl_secs: u64,
    pub session_ttl_secs: u64,
}

pub struct Token {
    pub label: String,
    pub secret: String,
}

impl ServerConfig {
    pub fn from_env() -> Result<Self, AppError> {
        let pushover_token = env::var("PUSHOVER_TOKEN")
            .map_err(|_| AppError::Config("PUSHOVER_TOKEN not set".into()))?;
        let pushover_user = env::var("PUSHOVER_USER")
            .map_err(|_| AppError::Config("PUSHOVER_USER not set".into()))?;

        let raw_tokens = env::var("CLAUDE_NOTIFY_TOKENS")
            .map_err(|_| AppError::Config("CLAUDE_NOTIFY_TOKENS not set".into()))?;
        let tokens = parse_tokens(&raw_tokens)?;
        if tokens.is_empty() {
            return Err(AppError::Config("CLAUDE_NOTIFY_TOKENS is empty".into()));
        }

        let listen_addr = env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
        let presence_ttl_secs = env::var("PRESENCE_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        let session_ttl_secs = env::var("SESSION_TTL")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(7200);

        Ok(Self {
            pushover_token,
            pushover_user,
            tokens,
            listen_addr,
            presence_ttl_secs,
            session_ttl_secs,
        })
    }
}

fn parse_tokens(raw: &str) -> Result<Vec<Token>, AppError> {
    raw.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|entry| {
            if let Some((label, secret)) = entry.split_once(':') {
                if secret.is_empty() {
                    return Err(AppError::Config(format!(
                        "token with label '{label}' has empty secret"
                    )));
                }
                Ok(Token {
                    label: label.into(),
                    secret: secret.into(),
                })
            } else {
                Ok(Token {
                    label: entry.into(),
                    secret: entry.into(),
                })
            }
        })
        .collect()
}

/// Runtime-mutable notification config.
#[derive(Clone, Serialize, Deserialize)]
pub struct NotifyConfig {
    pub stop_enabled: bool,
    pub permission_enabled: bool,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            stop_enabled: true,
            permission_enabled: true,
        }
    }
}

/// Partial update for NotifyConfig.
#[derive(Deserialize)]
pub struct NotifyConfigUpdate {
    pub stop_enabled: Option<bool>,
    pub permission_enabled: Option<bool>,
}

impl NotifyConfig {
    pub fn apply(&mut self, update: NotifyConfigUpdate) {
        if let Some(v) = update.stop_enabled {
            self.stop_enabled = v;
        }
        if let Some(v) = update.permission_enabled {
            self.permission_enabled = v;
        }
    }
}

/// Shared mutable notify config.
pub type SharedNotifyConfig = Arc<RwLock<NotifyConfig>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_labeled_tokens() {
        let tokens = parse_tokens("desktop:abc123,mobile:def456").unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].label, "desktop");
        assert_eq!(tokens[0].secret, "abc123");
        assert_eq!(tokens[1].label, "mobile");
        assert_eq!(tokens[1].secret, "def456");
    }

    #[test]
    fn parse_plain_tokens() {
        let tokens = parse_tokens("abc123,def456").unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].label, "abc123");
        assert_eq!(tokens[0].secret, "abc123");
    }

    #[test]
    fn parse_empty_secret_fails() {
        assert!(parse_tokens("desktop:").is_err());
    }

    #[test]
    fn notify_config_partial_update() {
        let mut cfg = NotifyConfig::default();
        cfg.apply(NotifyConfigUpdate {
            stop_enabled: Some(false),
            permission_enabled: None,
        });
        assert!(!cfg.stop_enabled);
        assert!(cfg.permission_enabled);
    }
}
