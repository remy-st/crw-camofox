//! Idle-tab reaper. camofox-browser's own idle cleanup only fires once the
//! *entire server* has zero live tabs across every user (see
//! `LifecycleController` in camofox-browser) — an abandoned tab under one
//! `userId` blocks that reaper from ever running for anyone else. Pi has no
//! reliable shutdown hook to close tabs itself (TUI process, can be
//! killed/crashed), so this bridge owns cleanup instead: it tracks last
//! activity per `userId` and force-closes anything idle past the configured
//! threshold, independent of whether any client ever calls `close`.

use std::sync::Arc;
use std::time::Duration;

use crate::state::AppState;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

pub fn spawn(state: Arc<AppState>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
        loop {
            ticker.tick().await;
            sweep(&state).await;
        }
    });
}

async fn sweep(state: &Arc<AppState>) {
    let idle: Vec<String> = state
        .sessions
        .iter()
        .filter(|entry| entry.last_activity.elapsed() >= state.idle_timeout)
        .map(|entry| entry.key().clone())
        .collect();

    for user_id in idle {
        match state.camofox.delete_session(&user_id).await {
            Ok(()) => {
                state.sessions.remove(&user_id);
                tracing::info!(user_id, "reaped idle browser session");
            }
            Err(e) => {
                // Leave the entry tracked so we retry next sweep; a
                // transient camofox hiccup shouldn't make us forget about it.
                tracing::warn!(user_id, error = %e, "idle session reap failed, will retry");
            }
        }
    }
}
