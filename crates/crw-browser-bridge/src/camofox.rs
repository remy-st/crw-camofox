//! Thin reqwest client for the camofox-browser REST API. Only wraps the
//! calls the bridge's action set actually needs — see
//! `skills/camofox-browser/references/api-endpoints.md` for the full
//! 47-route catalog this is a deliberate subset of.

use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CamofoxError {
    #[error("camofox request timed out")]
    Timeout,
    #[error("camofox transport error: {0}")]
    Transport(String),
    #[error("camofox returned {status}: {body}")]
    Upstream { status: u16, body: String },
}

impl CamofoxError {
    /// True when camofox reports the tab/session no longer exists. The
    /// dispatcher uses this to drop its local session entry and tell the
    /// caller to `navigate` again, instead of surfacing a raw 404.
    pub fn is_not_found(&self) -> bool {
        matches!(self, CamofoxError::Upstream { status: 404, .. })
    }
}

#[derive(Clone)]
pub struct CamofoxClient {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl CamofoxClient {
    /// `base_url` e.g. `http://camofox:9377`. `api_key`, when set, is sent
    /// as `Authorization: Bearer` (mirrors `CamofoxSearchClient::auth`).
    pub fn new(base_url: impl Into<String>, api_key: Option<String>, timeout: Duration) -> Self {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
        }
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.api_key {
            Some(k) => req.bearer_auth(k),
            None => req,
        }
    }

    fn map_transport_err(e: reqwest::Error) -> CamofoxError {
        if e.is_timeout() {
            CamofoxError::Timeout
        } else {
            CamofoxError::Transport(e.without_url().to_string())
        }
    }

    async fn to_json(res: reqwest::Response) -> Result<Value, CamofoxError> {
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            return Err(CamofoxError::Upstream {
                status: status.as_u16(),
                body,
            });
        }
        res.json::<Value>()
            .await
            .map_err(|e| CamofoxError::Transport(e.to_string()))
    }

    pub async fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, CamofoxError> {
        let res = self
            .auth(self.http.get(format!("{}{path}", self.base_url)))
            .query(query)
            .send()
            .await
            .map_err(Self::map_transport_err)?;
        Self::to_json(res).await
    }

    pub async fn post(&self, path: &str, body: Value) -> Result<Value, CamofoxError> {
        let res = self
            .auth(self.http.post(format!("{}{path}", self.base_url)))
            .json(&body)
            .send()
            .await
            .map_err(Self::map_transport_err)?;
        Self::to_json(res).await
    }

    /// `DELETE /sessions/:userId` — tears down the whole context. Treated as
    /// idempotent: a 404 (already gone) is not an error for our purposes,
    /// since the caller (reaper or `close` action) just wants it gone.
    pub async fn delete_session(&self, user_id: &str) -> Result<(), CamofoxError> {
        let res = self
            .auth(
                self.http
                    .delete(format!("{}/sessions/{user_id}", self.base_url)),
            )
            .send()
            .await
            .map_err(Self::map_transport_err)?;
        let status = res.status();
        if !status.is_success() && status.as_u16() != 404 {
            let body = res.text().await.unwrap_or_default();
            return Err(CamofoxError::Upstream {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }

    /// `GET /tabs/:tabId/screenshot` — returns raw PNG bytes, not JSON.
    pub async fn screenshot(
        &self,
        tab_id: &str,
        user_id: &str,
        full_page: bool,
    ) -> Result<Vec<u8>, CamofoxError> {
        let res = self
            .auth(self.http.get(format!(
                "{}/tabs/{tab_id}/screenshot",
                self.base_url
            )))
            .query(&[
                ("userId", user_id),
                ("fullPage", if full_page { "true" } else { "false" }),
            ])
            .send()
            .await
            .map_err(Self::map_transport_err)?;
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            return Err(CamofoxError::Upstream {
                status: status.as_u16(),
                body,
            });
        }
        res.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| CamofoxError::Transport(e.to_string()))
    }

    /// `GET /health` — used at startup only (best-effort log, not a hard
    /// dependency: camofox may still be warming up when the bridge starts).
    pub async fn health(&self) -> Result<Value, CamofoxError> {
        let res = self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .map_err(Self::map_transport_err)?;
        Self::to_json(res).await
    }
}
