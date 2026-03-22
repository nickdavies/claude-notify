use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use tracing::{info, warn};

use super::AppState;
use crate::server::presence::PresenceState;
use crate::server::pushover::PushoverClient;

/// Common fields from hook payloads. Serde ignores unknown fields by default.
#[derive(Deserialize)]
pub struct HookPayload {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub message: Option<String>,
}

/// POST /hooks/stop
pub async fn stop(State(state): State<AppState>, Json(payload): Json<HookPayload>) -> StatusCode {
    let (session_id, cwd) = match extract_session(&payload) {
        Some(v) => v,
        None => {
            warn!("stop hook: missing session_id or cwd");
            return StatusCode::OK;
        }
    };

    let project = state.sessions.get_or_register(&session_id, &cwd).await;
    let session_cfg = state.sessions.get_config(&session_id).await;
    let presence = state.presence.get().await;
    let global = state.notify_config.read().await;

    let should_notify = presence != PresenceState::Present
        && global.stop_enabled
        && session_cfg.as_ref().is_some_and(|c| c.stop_enabled);

    if should_notify {
        let pushover = Arc::clone(&state.pushover);
        let title = "Claude Code".to_string();
        let message = format!("[{project}] Claude finished");
        tokio::spawn(async move {
            fire_and_forget(&pushover, &title, &message).await;
        });
    } else {
        info!(
            session_id,
            present = ?presence,
            "stop hook: notification suppressed"
        );
    }

    // Deregister session on stop
    state.sessions.deregister(&session_id).await;

    StatusCode::OK
}

/// POST /hooks/notification
pub async fn notification(
    State(state): State<AppState>,
    Json(payload): Json<HookPayload>,
) -> StatusCode {
    let (session_id, cwd) = match extract_session(&payload) {
        Some(v) => v,
        None => {
            warn!("notification hook: missing session_id or cwd");
            return StatusCode::OK;
        }
    };

    let project = state.sessions.get_or_register(&session_id, &cwd).await;
    let session_cfg = state.sessions.get_config(&session_id).await;
    let presence = state.presence.get().await;
    let global = state.notify_config.read().await;

    let should_notify = presence != PresenceState::Present
        && global.permission_enabled
        && session_cfg.as_ref().is_some_and(|c| c.permission_enabled);

    if should_notify {
        let pushover = Arc::clone(&state.pushover);
        let title = "Claude Code (waiting)".to_string();
        let msg_body = payload.message.as_deref().unwrap_or("Permission prompt");
        let message = format!("[{project}] {msg_body}");
        tokio::spawn(async move {
            fire_and_forget(&pushover, &title, &message).await;
        });
    } else {
        info!(
            session_id,
            present = ?presence,
            "notification hook: notification suppressed"
        );
    }

    StatusCode::OK
}

/// POST /hooks/session-end
pub async fn session_end(
    State(state): State<AppState>,
    Json(payload): Json<HookPayload>,
) -> StatusCode {
    if let Some(session_id) = &payload.session_id {
        state.sessions.deregister(session_id).await;
    }
    StatusCode::OK
}

fn extract_session(payload: &HookPayload) -> Option<(String, String)> {
    match (&payload.session_id, &payload.cwd) {
        (Some(sid), Some(cwd)) => Some((sid.clone(), cwd.clone())),
        _ => None,
    }
}

async fn fire_and_forget(pushover: &PushoverClient, title: &str, message: &str) {
    if let Err(e) = pushover.send(title, message).await {
        warn!("pushover failed: {e}");
    }
}
