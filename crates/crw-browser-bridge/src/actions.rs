//! `POST /browser` — single action-dispatch endpoint. One tool schema on the
//! caller's side (see the Pi `crw-browser.ts` extension); the action-specific
//! request shapes for the underlying camofox-browser REST calls live here
//! instead of being re-declared per action in every client.
//!
//! The bridge holds exactly one tab per `userId` (callers are expected to
//! mint a stable id per conversation/process, not per call), so there is no
//! tab-listing or multi-tab management surface here — that's the
//! camofox-mcp server's job for its multi-tenant use case, not this one's.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::camofox::CamofoxError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct BrowserRequest {
    #[serde(rename = "userId")]
    pub user_id: String,
    pub action: Option<String>,
    /// Everything else in the request body — action-specific fields (`url`,
    /// `ref`, `text`, ...). Kept as a raw bag rather than an enum-of-structs
    /// so adding action #14 later never touches this type.
    #[serde(flatten)]
    pub args: Value,
}

#[derive(Debug, Default, Serialize)]
pub struct BrowserResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(rename = "imageBase64", skip_serializing_if = "Option::is_none")]
    pub image_base64: Option<String>,
}

impl BrowserResponse {
    fn ok(result: impl Into<String>) -> Self {
        Self {
            ok: true,
            result: Some(result.into()),
            ..Default::default()
        }
    }

    fn ok_with_image(result: impl Into<String>, image_base64: String) -> Self {
        Self {
            ok: true,
            result: Some(result.into()),
            image_base64: Some(image_base64),
            ..Default::default()
        }
    }
}

#[derive(Debug)]
enum DispatchError {
    BadRequest(String),
    Upstream(CamofoxError),
}

impl DispatchError {
    fn status(&self) -> StatusCode {
        match self {
            DispatchError::BadRequest(_) => StatusCode::BAD_REQUEST,
            DispatchError::Upstream(CamofoxError::Timeout) => StatusCode::GATEWAY_TIMEOUT,
            DispatchError::Upstream(CamofoxError::Upstream { status, .. }) => {
                StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            DispatchError::Upstream(CamofoxError::Transport(_)) => StatusCode::BAD_GATEWAY,
        }
    }
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DispatchError::BadRequest(m) => write!(f, "{m}"),
            DispatchError::Upstream(e) => write!(f, "{e}"),
        }
    }
}

/// A 404 from camofox for a tab-scoped call means our tracked session is
/// stale (camofox restarted, was manually cleaned up, or the reaper raced
/// us). Drop it locally so the next `navigate` recreates cleanly instead of
/// looping on the same 404.
fn classify(state: &AppState, user_id: &str, e: CamofoxError) -> DispatchError {
    if e.is_not_found() {
        state.drop_stale(user_id);
        DispatchError::BadRequest(
            "Browser session expired or was already closed — call `navigate` again.".into(),
        )
    } else {
        DispatchError::Upstream(e)
    }
}

fn require_tab(state: &AppState, user_id: &str) -> Result<String, DispatchError> {
    state.tab_id_for(user_id).ok_or_else(|| {
        DispatchError::BadRequest("No active browser session — call `navigate` first.".into())
    })
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn bool_arg(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn u64_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BrowserRequest>,
) -> (StatusCode, Json<BrowserResponse>) {
    let user_id = req.user_id.trim();
    if user_id.is_empty() {
        return err_response(StatusCode::BAD_REQUEST, "userId is required");
    }

    let action = match req.action.as_deref() {
        Some(a) if !a.is_empty() => a,
        _ => return (StatusCode::OK, Json(BrowserResponse::ok(catalog_text()))),
    };

    match dispatch(&state, user_id, action, &req.args).await {
        Ok(resp) => (StatusCode::OK, Json(resp)),
        Err(e) => err_response(e.status(), &e.to_string()),
    }
}

fn err_response(status: StatusCode, message: &str) -> (StatusCode, Json<BrowserResponse>) {
    (
        status,
        Json(BrowserResponse {
            ok: false,
            error: Some(message.to_string()),
            ..Default::default()
        }),
    )
}

async fn dispatch(
    state: &Arc<AppState>,
    user_id: &str,
    action: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    match action {
        "navigate" => navigate(state, user_id, args).await,
        "snapshot" => snapshot(state, user_id, args).await,
        "click" => click(state, user_id, args).await,
        "type" => type_text(state, user_id, args).await,
        "press" => press(state, user_id, args).await,
        "scroll" => scroll(state, user_id, args).await,
        "evaluate" => evaluate(state, user_id, args).await,
        "screenshot" => screenshot(state, user_id, args).await,
        "wait" => wait(state, user_id, args).await,
        "close" => close(state, user_id).await,
        "console" => console(state, user_id, args).await,
        "cookies" => cookies(state, user_id, args).await,
        "download" => download(state, user_id, args).await,
        other => Err(DispatchError::BadRequest(format!(
            "Unknown action `{other}`. Call browser() with no action to see the catalog."
        ))),
    }
}

async fn navigate(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let url = str_arg(args, "url");
    let macro_ = str_arg(args, "macro");
    let query = str_arg(args, "query");
    if url.is_none() && macro_.is_none() {
        return Err(DispatchError::BadRequest(
            "navigate needs `url`, or `macro` + `query` (e.g. macro: \"@google_search\")".into(),
        ));
    }

    let (tab_id, just_created) = match state.tab_id_for(user_id) {
        Some(id) => (id, false),
        None => {
            let mut body = json!({ "userId": user_id, "sessionKey": "pi" });
            // A macro always needs an explicit /navigate call below, so don't
            // pre-seed the tab with a plain `url` in that case.
            if macro_.is_none()
                && let Some(u) = url
            {
                body["url"] = json!(u);
            }
            let created = state
                .camofox
                .post("/tabs", body)
                .await
                .map_err(|e| classify(state, user_id, e))?;
            let id = created
                .get("tabId")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    DispatchError::Upstream(CamofoxError::Transport(
                        "camofox /tabs response missing tabId".into(),
                    ))
                })?
                .to_string();
            (id, true)
        }
    };
    state.touch(user_id, &tab_id);

    let needs_explicit_navigate = !just_created || macro_.is_some();
    if needs_explicit_navigate {
        let mut body = json!({ "userId": user_id });
        if let Some(m) = macro_ {
            body["macro"] = json!(m);
            if let Some(q) = query {
                body["query"] = json!(q);
            }
        } else if let Some(u) = url {
            body["url"] = json!(u);
        }
        state
            .camofox
            .post(&format!("/tabs/{tab_id}/navigate"), body)
            .await
            .map_err(|e| classify(state, user_id, e))?;
        state.touch(user_id, &tab_id);
    }

    let target = match (macro_, url) {
        (Some(m), _) => format!("{m} \"{}\"", query.unwrap_or_default()),
        (None, Some(u)) => u.to_string(),
        (None, None) => String::new(),
    };
    Ok(BrowserResponse::ok(format!(
        "Navigated to {target}. Call `snapshot` to read the page."
    )))
}

async fn snapshot(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let mut query: Vec<(&str, &str)> = vec![("userId", user_id)];
    let offset_str;
    if let Some(o) = u64_arg(args, "offset") {
        offset_str = o.to_string();
        query.push(("offset", &offset_str));
    }
    let res = state
        .camofox
        .get(&format!("/tabs/{tab_id}/snapshot"), &query)
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);
    let text = res
        .get("snapshot")
        .and_then(Value::as_str)
        .unwrap_or("(empty snapshot)");
    let mut out = text.to_string();
    if let Some(next) = res.get("nextOffset").and_then(Value::as_u64) {
        out.push_str(&format!(
            "\n\n(truncated — call snapshot again with offset: {next} for more)"
        ));
    }
    Ok(BrowserResponse::ok(out))
}

async fn click(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let ref_ = str_arg(args, "ref");
    let selector = str_arg(args, "selector");
    if ref_.is_none() && selector.is_none() {
        return Err(DispatchError::BadRequest(
            "click needs `ref` (from snapshot) or `selector`".into(),
        ));
    }
    let mut body = json!({ "userId": user_id });
    if let Some(r) = ref_ {
        body["ref"] = json!(r);
    }
    if let Some(s) = selector {
        body["selector"] = json!(s);
    }
    let res = state
        .camofox
        .post(&format!("/tabs/{tab_id}/click"), body)
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);

    let mut msg = format!("Clicked {}.", ref_.or(selector).unwrap_or("element"));
    if let Some(downloads) = res.get("downloads").and_then(Value::as_array)
        && !downloads.is_empty()
    {
        msg.push_str(&format!(
            " Started {} download(s) — use the `download` action to fetch them.",
            downloads.len()
        ));
    }
    Ok(BrowserResponse::ok(msg))
}

async fn type_text(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let ref_ = str_arg(args, "ref");
    let selector = str_arg(args, "selector");
    let text = str_arg(args, "text");
    if ref_.is_none() && selector.is_none() {
        return Err(DispatchError::BadRequest(
            "type needs `ref` (from snapshot) or `selector`".into(),
        ));
    }
    let Some(text) = text else {
        return Err(DispatchError::BadRequest("type needs `text`".into()));
    };
    let mut body = json!({ "userId": user_id, "text": text });
    if let Some(r) = ref_ {
        body["ref"] = json!(r);
    }
    if let Some(s) = selector {
        body["selector"] = json!(s);
    }
    state
        .camofox
        .post(&format!("/tabs/{tab_id}/type"), body)
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);

    let mut msg = format!("Typed into {}.", ref_.or(selector).unwrap_or("element"));
    if bool_arg(args, "submit").unwrap_or(false) {
        state
            .camofox
            .post(
                &format!("/tabs/{tab_id}/press"),
                json!({ "userId": user_id, "key": "Enter" }),
            )
            .await
            .map_err(|e| classify(state, user_id, e))?;
        state.touch(user_id, &tab_id);
        msg.push_str(" Pressed Enter.");
    }
    Ok(BrowserResponse::ok(msg))
}

async fn press(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let Some(key) = str_arg(args, "key") else {
        return Err(DispatchError::BadRequest(
            "press needs `key` (e.g. \"Enter\", \"Tab\", \"Escape\")".into(),
        ));
    };
    state
        .camofox
        .post(
            &format!("/tabs/{tab_id}/press"),
            json!({ "userId": user_id, "key": key }),
        )
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);
    Ok(BrowserResponse::ok(format!("Pressed {key}.")))
}

async fn scroll(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let direction = str_arg(args, "direction").unwrap_or("down");
    let amount = u64_arg(args, "amount");
    let mut body = json!({ "userId": user_id, "direction": direction });
    if let Some(a) = amount {
        body["amount"] = json!(a);
    }
    state
        .camofox
        .post(&format!("/tabs/{tab_id}/scroll"), body)
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);
    Ok(BrowserResponse::ok(format!("Scrolled {direction}.")))
}

async fn evaluate(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let Some(expression) = str_arg(args, "expression") else {
        return Err(DispatchError::BadRequest(
            "evaluate needs `expression` (JavaScript, evaluated in the page)".into(),
        ));
    };
    let res = state
        .camofox
        .post(
            &format!("/tabs/{tab_id}/evaluate"),
            json!({ "userId": user_id, "expression": expression }),
        )
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);
    let result = res
        .get("result")
        .map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .unwrap_or_else(|| "undefined".to_string());
    Ok(BrowserResponse::ok(result))
}

async fn screenshot(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let full_page = bool_arg(args, "fullPage").unwrap_or(false);
    let bytes = state
        .camofox
        .screenshot(&tab_id, user_id, full_page)
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);
    let encoded = BASE64.encode(&bytes);
    Ok(BrowserResponse::ok_with_image(
        format!("Screenshot captured ({} bytes).", bytes.len()),
        encoded,
    ))
}

async fn wait(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let mut body = json!({ "userId": user_id });
    if let Some(t) = u64_arg(args, "timeoutMs") {
        body["timeout"] = json!(t);
    }
    if let Some(n) = bool_arg(args, "waitForNetwork") {
        body["waitForNetwork"] = json!(n);
    }
    state
        .camofox
        .post(&format!("/tabs/{tab_id}/wait"), body)
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);
    Ok(BrowserResponse::ok("Wait complete."))
}

async fn close(state: &Arc<AppState>, user_id: &str) -> Result<BrowserResponse, DispatchError> {
    if state.tab_id_for(user_id).is_none() {
        return Ok(BrowserResponse::ok("No active session to close."));
    }
    state
        .camofox
        .delete_session(user_id)
        .await
        .map_err(DispatchError::Upstream)?;
    state.drop_stale(user_id);
    Ok(BrowserResponse::ok("Browser session closed."))
}

async fn console(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let want_errors = bool_arg(args, "errors").unwrap_or(false);
    let limit = u64_arg(args, "limit").unwrap_or(50).to_string();

    if want_errors {
        let res = state
            .camofox
            .get(
                &format!("/tabs/{tab_id}/errors"),
                &[("userId", user_id), ("limit", &limit)],
            )
            .await
            .map_err(|e| classify(state, user_id, e))?;
        state.touch(user_id, &tab_id);
        let entries = res.get("errors").and_then(Value::as_array);
        let text = format_page_errors(entries);
        return Ok(BrowserResponse::ok(text));
    }

    let type_filter = str_arg(args, "type");
    let mut query: Vec<(&str, &str)> = vec![("userId", user_id), ("limit", &limit)];
    if let Some(t) = type_filter {
        query.push(("type", t));
    }
    let res = state
        .camofox
        .get(&format!("/tabs/{tab_id}/console"), &query)
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);
    let entries = res.get("messages").and_then(Value::as_array);
    let text = format_console_messages(entries);
    Ok(BrowserResponse::ok(text))
}

fn format_console_messages(entries: Option<&Vec<Value>>) -> String {
    let Some(entries) = entries.filter(|e| !e.is_empty()) else {
        return "No console messages captured.".to_string();
    };
    entries
        .iter()
        .map(|m| {
            let kind = m.get("type").and_then(Value::as_str).unwrap_or("log");
            let text = m.get("text").and_then(Value::as_str).unwrap_or("");
            format!("[{kind}] {text}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_page_errors(entries: Option<&Vec<Value>>) -> String {
    let Some(entries) = entries.filter(|e| !e.is_empty()) else {
        return "No page errors captured.".to_string();
    };
    entries
        .iter()
        .map(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .unwrap_or("(unknown error)")
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn cookies(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let Some(cookies) = args.get("cookies").and_then(Value::as_array) else {
        return Err(DispatchError::BadRequest(
            "cookies needs a `cookies` array (name/value/domain/path objects)".into(),
        ));
    };
    let count = cookies.len();
    state
        .camofox
        .post(
            &format!("/sessions/{user_id}/cookies"),
            json!({ "cookies": cookies }),
        )
        .await
        .map_err(DispatchError::Upstream)?;
    Ok(BrowserResponse::ok(format!("Imported {count} cookie(s).")))
}

async fn download(
    state: &Arc<AppState>,
    user_id: &str,
    args: &Value,
) -> Result<BrowserResponse, DispatchError> {
    let tab_id = require_tab(state, user_id)?;
    let mut body = json!({ "userId": user_id });
    for key in ["selector", "resolveBlobs"] {
        if let Some(v) = args.get(key) {
            body[key] = v.clone();
        }
    }
    for key in ["types", "extensions"] {
        if let Some(v) = args.get(key).and_then(Value::as_array) {
            body[key] = Value::Array(v.clone());
        }
    }
    for key in ["concurrency", "maxFiles"] {
        if let Some(v) = u64_arg(args, key) {
            body[key] = json!(v);
        }
    }

    let res = state
        .camofox
        .post(&format!("/tabs/{tab_id}/batch-download"), body)
        .await
        .map_err(|e| classify(state, user_id, e))?;
    state.touch(user_id, &tab_id);

    let downloads = res.get("downloads").and_then(Value::as_array);
    let paths: Vec<String> = downloads
        .map(|list| {
            list.iter()
                .filter(|d| d.get("status").and_then(Value::as_str) == Some("completed"))
                .filter_map(|d| d.get("savedFilename").and_then(Value::as_str))
                .map(|filename| format!("{}/{}/{}", state.downloads_host_dir, user_id, filename))
                .collect()
        })
        .unwrap_or_default();

    let failed = downloads
        .map(|list| {
            list.iter()
                .filter(|d| d.get("status").and_then(Value::as_str) != Some("completed"))
                .count()
        })
        .unwrap_or(0);

    if paths.is_empty() && failed == 0 {
        return Ok(BrowserResponse::ok(
            "No matching files found to download on this page.",
        ));
    }

    let mut msg = if paths.is_empty() {
        String::new()
    } else {
        format!("Downloaded file(s): {}", paths.join(", "))
    };
    if failed > 0 {
        if !msg.is_empty() {
            msg.push(' ');
        }
        msg.push_str(&format!("({failed} download(s) failed.)"));
    }
    Ok(BrowserResponse::ok(msg))
}

fn catalog_text() -> &'static str {
    "Actions (call again with one of these as `action`, plus its args):\n\
     - navigate: url  |  macro + query (e.g. macro: \"@google_search\", query: \"...\") — opens the session's tab if none exists yet\n\
     - snapshot: (offset?) — accessibility-tree read of the current page; the primary way to see what's on screen\n\
     - click: ref | selector\n\
     - type: (ref | selector) + text, submit? (bool, presses Enter after)\n\
     - press: key (e.g. \"Enter\", \"Tab\", \"Escape\")\n\
     - scroll: direction? (up|down|left|right, default down), amount?\n\
     - evaluate: expression (JavaScript run in the page, returns the result)\n\
     - screenshot: fullPage? — visual verification only; prefer snapshot for reading content\n\
     - wait: timeoutMs?, waitForNetwork?\n\
     - console: errors? (bool, page exceptions instead of console.log), type?, limit?\n\
     - cookies: cookies (array) — import cookies to authenticate as a logged-in user\n\
     - download: selector?, types? ([\"images\",\"links\",\"media\",\"documents\"]), extensions?, maxFiles? — bulk-downloads matching links on the page\n\
     - close: closes the session's browser tab/context\n\
     All actions need `userId` (stable per your session) and, except navigate/close, an existing session — call navigate first."
}
