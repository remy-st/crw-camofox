use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::camofox::CamofoxClient;

/// One entry per active `userId` — the bridge holds exactly one tab per
/// caller identity (see module docs on `main.rs`), so "session" and "tab"
/// are the same lifetime here.
pub struct SessionState {
    pub tab_id: String,
    pub last_activity: Instant,
}

pub struct AppState {
    pub camofox: CamofoxClient,
    pub sessions: DashMap<String, SessionState>,
    pub idle_timeout: Duration,
    /// Bearer key callers must present. `None` disables auth (loopback-only
    /// dev use, mirrors camofox-mcp's `AUTH_MODE=disabled` default).
    pub bridge_api_key: Option<String>,
    /// Host-visible path prefix for completed downloads, reported back to
    /// callers in place of the container-side path. e.g. `./downloads` when
    /// compose bind-mounts that directory to the same path camofox's
    /// `CAMOFOX_DOWNLOADS_DIR` writes into.
    pub downloads_host_dir: String,
}

impl AppState {
    pub fn touch(&self, user_id: &str, tab_id: &str) {
        self.sessions.insert(
            user_id.to_string(),
            SessionState {
                tab_id: tab_id.to_string(),
                last_activity: Instant::now(),
            },
        );
    }

    pub fn tab_id_for(&self, user_id: &str) -> Option<String> {
        self.sessions.get(user_id).map(|e| e.tab_id.clone())
    }

    /// Drop a session that camofox no longer recognizes (stale after a
    /// restart, manual cleanup, or eviction), so the next `navigate`
    /// recreates it fresh rather than looping on 404s.
    pub fn drop_stale(&self, user_id: &str) {
        self.sessions.remove(user_id);
    }
}
