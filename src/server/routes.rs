//! The `/api/v1` routes. See `docs/HTTP_API.md` for the contract.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body as HttpBody;
use axum::extract::{Path, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_stream::wrappers::WatchStream;
use tokio_stream::{Stream, StreamExt};

use super::error::{ApiError, Body, PrivateBody, Q};
use super::publish::{trim_activity, Envelope, MAX_ACTIVITY, STREAM_ACTIVITY};
use super::{files, restart_watch, Ctx, API_VERSION, BODY_LIMIT};
use crate::config::{self, Config};

type Ctl = State<Arc<Ctx>>;
type ApiResult<T> = Result<T, ApiError>;

pub fn router(ctx: Arc<Ctx>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/version", get(version))
        .route("/api/v1/state", get(state))
        .route("/api/v1/events", get(events))
        .route("/api/v1/sync", post(sync))
        .route("/api/v1/cancel", post(cancel))
        .route("/api/v1/auto-sync", post(auto_sync))
        .route("/api/v1/held", get(held))
        .route("/api/v1/held/{pair}/restore", post(held_restore))
        .route("/api/v1/held/{pair}/delete", post(held_delete))
        .route("/api/v1/conflicts", get(conflicts))
        .route("/api/v1/conflicts/dismiss-all", post(conflicts_dismiss_all))
        .route("/api/v1/conflicts/{id}/dismiss", post(conflict_dismiss))
        .route("/api/v1/settings", get(settings_get).put(settings_put))
        .route("/api/v1/pairs", get(pairs_get).post(pairs_add))
        .route(
            "/api/v1/pairs/{name}",
            put(pairs_replace).delete(pairs_remove),
        )
        .route("/api/v1/account", get(account))
        .route("/api/v1/account/login", post(login))
        .route("/api/v1/account/2fa", post(two_factor))
        .route("/api/v1/account/mailbox-password", post(mailbox_password))
        .route("/api/v1/account/logout", post(logout))
        .route("/api/v1/remote/list", get(remote_list))
        .route("/api/v1/local/list", get(local_list))
        .route("/api/v1/logs", get(logs))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(axum::extract::DefaultBodyLimit::max(BODY_LIMIT))
        .layer(middleware::from_fn_with_state(ctx.clone(), gate))
        .with_state(ctx)
}

/// Compare two secrets in time that depends only on their lengths.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for (i, x) in a.iter().enumerate() {
        diff |= (*x ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

/// Whether a request came from a web page. Browsers add `Origin` to every
/// cross-site request and to every `POST`, and `Sec-Fetch-Site` to all of
/// them; the server-side clients this API is for (curl, a web app's backend)
/// send neither. Refusing them is what stops a page the user happens to open
/// from driving a server that has no token.
fn from_a_browser(req: &Request) -> bool {
    let h = req.headers();
    h.contains_key(header::ORIGIN)
        || h.get("sec-fetch-site")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v != "none" && v != "same-origin")
}

/// On a loopback listener, the `Host` a request names must be a loopback
/// name. A page served from another name that resolves to 127.0.0.1 (DNS
/// rebinding) is refused, so it cannot read the state either.
fn host_allowed(ctx: &Ctx, req: &Request) -> bool {
    if !ctx.loopback {
        return true;
    }
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        // HTTP/1.0 clients may send none; a browser always does.
        return true;
    };
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(n, _)| n)
    };
    let name = name.to_ascii_lowercase();
    name == "localhost"
        || name.ends_with(".localhost")
        || name
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Token, origin, host and body checks for every request. `/api/v1/health`
/// is open so a container health check needs no secret.
async fn gate(State(ctx): Ctl, req: Request, next: Next) -> Response {
    if req.uri().path() != "/api/v1/health" {
        if from_a_browser(&req) {
            ctx.refused("request from a web page");
            return ApiError::forbidden("Requests from web pages are not accepted.")
                .into_response();
        }
        if !host_allowed(&ctx, &req) {
            ctx.refused("unexpected Host name");
            return ApiError::forbidden("Requests for that host name are not accepted.")
                .into_response();
        }
        if let Some(token) = &ctx.token {
            let given = req
                .headers()
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or("");
            if !same_secret(given.as_bytes(), token.as_bytes()) {
                ctx.refused("missing or wrong API token");
                return ApiError::unauthorized().into_response();
            }
        }
    }
    // The body is read here, whatever the handler does with it, so the cap
    // holds for chunked bodies and for commands that take no body at all.
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(bytes) => bytes,
        Err(_) => {
            ctx.refused("body over 64 KiB");
            return ApiError::too_large().into_response();
        }
    };
    if !bytes.iter().all(u8::is_ascii_whitespace) {
        let json = parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"));
        if !json {
            ctx.refused("body that is not application/json");
            return ApiError::unsupported_body().into_response();
        }
    }
    next.run(Request::from_parts(parts, HttpBody::from(bytes)))
        .await
}

async fn not_found() -> ApiError {
    ApiError::not_found("No such endpoint.")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "bad_request",
        "This endpoint does not take that method.",
    )
}

/// Run blocking work off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("internal task failed: {e}")))
}

fn accepted() -> (StatusCode, Json<Value>) {
    (StatusCode::ACCEPTED, Json(json!({"accepted": true})))
}

// ---- health, version, state, events ---------------------------------------

async fn health() -> Json<Value> {
    Json(json!({"ok": true}))
}

async fn version() -> Json<Value> {
    Json(json!({
        "api_version": API_VERSION,
        "version": env!("CARGO_PKG_VERSION"),
        "build": env!("NEUTRONSYNC_BUILD_SHA"),
    }))
}

#[derive(Deserialize)]
struct StateQuery {
    activity: Option<usize>,
}

async fn state(State(ctx): Ctl, Q(q): Q<StateQuery>) -> ApiResult<Json<Envelope>> {
    let keep = q.activity.unwrap_or(STREAM_ACTIVITY).min(MAX_ACTIVITY);
    // Same source, and so the same rev, as the event stream.
    let revisions = ctx.revisions.clone();
    let mut envelope = blocking(move || revisions.current()).await?;
    trim_activity(&mut envelope.state, keep);
    Ok(Json(envelope))
}

async fn events(State(ctx): Ctl) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // The current envelope first, then each new one. The stream ends when the
    // publisher stops at shutdown.
    let stream = WatchStream::new(ctx.published.clone())
        .filter(|p| p.rev > 0)
        .map(|p| Ok(Event::default().event("state").data(p.json.clone())));
    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}

// ---- sync ------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncBody {
    #[serde(default)]
    pairs: Vec<String>,
    #[serde(default)]
    dry_run: bool,
}

async fn sync(State(ctx): Ctl, Body(body): Body<SyncBody>) -> ApiResult<impl IntoResponse> {
    let cfg = ctx.ctrl.config();
    if let Some(unknown) = body
        .pairs
        .iter()
        .find(|n| !cfg.pairs.iter().any(|p| &p.name == *n))
    {
        return Err(ApiError::bad_request(format!(
            "There is no synced folder named {unknown:?}."
        )));
    }
    start_run(&ctx, move |ctrl| ctrl.sync(body.pairs, body.dry_run))?;
    Ok(accepted())
}

/// Start a run if none is going, as one step. `Controller::sync` and its
/// siblings set the busy flag before they return, and every start in this
/// server goes through here under `start_lock`, so of two simultaneous
/// requests one starts the run and the other is told `busy`; neither is told
/// "accepted" for a run that never started.
fn start_run(ctx: &Ctx, start: impl FnOnce(&crate::service::Controller)) -> ApiResult<()> {
    let _one_at_a_time = ctx.start_lock.lock().unwrap();
    if ctx.ctrl.is_busy() {
        return Err(ApiError::busy());
    }
    start(&ctx.ctrl);
    Ok(())
}

async fn cancel(State(ctx): Ctl) -> impl IntoResponse {
    ctx.ctrl.cancel();
    accepted()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AutoSyncBody {
    enabled: bool,
}

async fn auto_sync(State(ctx): Ctl, Body(body): Body<AutoSyncBody>) -> ApiResult<Json<Value>> {
    let enabled = body.enabled;
    edit_config(&ctx, move |doc| {
        options(doc).insert("auto_sync".into(), toml::Value::Boolean(enabled));
        Ok(())
    })
    .await?;
    let ctrl = ctx.ctrl.clone();
    if enabled {
        // Started here, not in the background, so the next state read already
        // says watching; restart_watch waits for a stopping watcher first.
        blocking(move || restart_watch(&ctrl, true)).await?;
    } else {
        ctrl.stop_watch();
    }
    Ok(Json(json!({"auto_sync": enabled})))
}

// ---- held deletions ---------------------------------------------------------

async fn held(State(ctx): Ctl) -> ApiResult<impl IntoResponse> {
    let ctrl = ctx.ctrl.clone();
    Ok(Json(blocking(move || ctrl.held_deletes()).await?))
}

async fn check_held(ctx: &Ctx, pair: &str) -> ApiResult<()> {
    if !ctx.ctrl.config().pairs.iter().any(|p| p.name == pair) {
        return Err(ApiError::not_found(format!(
            "There is no synced folder named {pair:?}."
        )));
    }
    let ctrl = ctx.ctrl.clone();
    let held = blocking(move || ctrl.held_deletes()).await?;
    if !held.iter().any(|h| h.pair == pair) {
        return Err(ApiError::not_found(format!(
            "No deletions are held back for {pair:?}."
        )));
    }
    Ok(())
}

async fn held_restore(State(ctx): Ctl, Path(pair): Path<String>) -> ApiResult<impl IntoResponse> {
    check_held(&ctx, &pair).await?;
    start_run(&ctx, move |ctrl| ctrl.restore_held(pair))?;
    Ok(accepted())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfirmBody {
    #[serde(default)]
    confirm: bool,
}

async fn held_delete(
    State(ctx): Ctl,
    Path(pair): Path<String>,
    Body(body): Body<ConfirmBody>,
) -> ApiResult<impl IntoResponse> {
    check_held(&ctx, &pair).await?;
    if !body.confirm {
        return Err(ApiError::bad_request(
            "Deleting held-back files needs {\"confirm\":true}. Nothing was deleted.",
        ));
    }
    start_run(&ctx, move |ctrl| ctrl.delete_held(pair))?;
    Ok(accepted())
}

// ---- conflicts --------------------------------------------------------------

async fn conflicts(State(ctx): Ctl) -> ApiResult<impl IntoResponse> {
    let ctrl = ctx.ctrl.clone();
    Ok(Json(blocking(move || ctrl.conflicts()).await?))
}

async fn conflict_dismiss(State(ctx): Ctl, Path(id): Path<String>) -> ApiResult<impl IntoResponse> {
    let Ok(id) = id.parse::<i64>() else {
        return Err(ApiError::not_found("No such conflict."));
    };
    let ctrl = ctx.ctrl.clone();
    let found = blocking(move || {
        let found = ctrl.conflicts().iter().any(|c| c.id == id);
        if found {
            ctrl.dismiss_conflict(id);
        }
        found
    })
    .await?;
    if !found {
        return Err(ApiError::not_found("No such conflict."));
    }
    Ok(accepted())
}

async fn conflicts_dismiss_all(State(ctx): Ctl) -> ApiResult<impl IntoResponse> {
    let ctrl = ctx.ctrl.clone();
    blocking(move || ctrl.dismiss_all_conflicts()).await?;
    Ok(accepted())
}

// ---- config editing ----------------------------------------------------------

fn options(doc: &mut toml::Table) -> &mut toml::Table {
    table(doc, "options")
}

fn table<'a>(doc: &'a mut toml::Table, name: &str) -> &'a mut toml::Table {
    let entry = doc
        .entry(name.to_string())
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !entry.is_table() {
        *entry = toml::Value::Table(toml::Table::new());
    }
    entry.as_table_mut().expect("a table")
}

/// D13: change the running config through the real parser. The edit is made
/// on the TOML of the running config, written to a temp file beside the
/// config file and loaded with `config::load`. Only when that succeeds is the
/// controller given the new config and the file replaced (temp file plus
/// rename, mode kept). A parse failure is 422 with the parser's message and
/// leaves both the running config and the file as they were.
async fn edit_config(
    ctx: &Arc<Ctx>,
    edit: impl FnOnce(&mut toml::Table) -> ApiResult<()> + Send + 'static,
) -> ApiResult<Config> {
    let _guard = ctx.config_lock.lock().await;
    let ctx2 = ctx.clone();
    blocking(move || {
        let ctx = ctx2;
        let current = ctx.ctrl.config();
        let text = config::to_toml(&current)
            .map_err(|e| ApiError::internal(format!("cannot write the config: {e:#}")))?;
        let mut doc: toml::Table = toml::from_str(&text)
            .map_err(|e| ApiError::internal(format!("cannot read the config back: {e}")))?;
        edit(&mut doc)?;
        let edited = toml::to_string(&doc)
            .map_err(|e| ApiError::internal(format!("cannot write the config: {e}")))?;

        let dir = ctx
            .config_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| ".".into());
        let mut probe = tempfile::Builder::new()
            .prefix(".neutronsync-check-")
            .suffix(".toml")
            .tempfile_in(&dir)
            .map_err(|e| ApiError::internal(format!("cannot write in {}: {e}", dir.display())))?;
        std::io::Write::write_all(&mut probe, edited.as_bytes())
            .map_err(|e| ApiError::internal(format!("cannot write the config: {e}")))?;
        let probe_path = probe.path().to_path_buf();
        let mut next = match config::load(probe_path.to_str()) {
            Ok(cfg) => cfg,
            Err(e) => {
                let message = format!("{e:#}").replace(
                    &probe_path.display().to_string(),
                    &ctx.config_path.display().to_string(),
                );
                return Err(ApiError::invalid_config(message));
            }
        };
        drop(probe);
        next.source_path = Some(ctx.config_path.clone());
        let final_text = config::to_toml(&next)
            .map_err(|e| ApiError::internal(format!("cannot write the config: {e:#}")))?;
        super::replace_file(&ctx.config_path, &final_text).map_err(|e| {
            ApiError::internal(format!("cannot save {}: {e}", ctx.config_path.display()))
        })?;
        ctx.ctrl.commit_config(next.clone());
        Ok(next)
    })
    .await?
}

// ---- settings ----------------------------------------------------------------

/// The settings a server can use, in their TOML spelling. Every key is
/// optional on `PUT`; an unknown key is refused.
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    propagate_deletes: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_delete_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_delete_percent: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_delete: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    compare: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scan_threads: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    download_threads: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_walk_interval: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scan_interval: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    debounce: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    upload_limit_kbps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    download_limit_kbps: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    auto_sync: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_root: Option<String>,
}

impl Settings {
    fn of(cfg: &Config) -> Self {
        Settings {
            propagate_deletes: Some(cfg.propagate_deletes),
            max_delete_files: Some(cfg.max_delete_files as u64),
            max_delete_percent: Some(cfg.max_delete_percent as u64),
            local_delete: Some(config::local_delete_str(cfg.local_delete).into()),
            conflict: Some(config::conflict_str(cfg.conflict).into()),
            compare: Some(config::compare_str(cfg.compare).into()),
            scan_threads: Some(cfg.scan_threads as u64),
            download_threads: Some(cfg.download_threads as u64),
            full_walk_interval: Some(cfg.full_walk_interval),
            scan_interval: Some(cfg.scan_interval_secs),
            debounce: Some(cfg.debounce_secs),
            upload_limit_kbps: Some(cfg.upload_limit_kbps),
            download_limit_kbps: Some(cfg.download_limit_kbps),
            auto_sync: Some(cfg.auto_sync),
            remote_root: Some(cfg.remote_root.clone()),
        }
    }

    /// Write every given key into the TOML document, in the table it lives in.
    fn apply(self, doc: &mut toml::Table) -> ApiResult<()> {
        let value = serde_json::to_value(&self)
            .map_err(|e| ApiError::internal(format!("cannot read the settings: {e}")))?;
        let Value::Object(map) = value else {
            return Ok(());
        };
        for (key, v) in map {
            let tv = match v {
                Value::Bool(b) => toml::Value::Boolean(b),
                Value::String(s) => toml::Value::String(s),
                Value::Number(n) => match n.as_u64().and_then(|n| i64::try_from(n).ok()) {
                    Some(n) => toml::Value::Integer(n),
                    None => {
                        return Err(ApiError::bad_request(format!(
                            "{key} must be a whole number."
                        )))
                    }
                },
                _ => continue,
            };
            let section = if matches!(key.as_str(), "scan_threads" | "download_threads") {
                table(doc, "cli")
            } else {
                options(doc)
            };
            section.insert(key, tv);
        }
        Ok(())
    }
}

async fn settings_get(State(ctx): Ctl) -> Json<Settings> {
    Json(Settings::of(&ctx.ctrl.config()))
}

async fn settings_put(State(ctx): Ctl, Body(body): Body<Settings>) -> ApiResult<Json<Settings>> {
    let auto = body.auto_sync;
    let before = ctx.ctrl.config().auto_sync;
    let next = edit_config(&ctx, move |doc| body.apply(doc)).await?;
    // Auto sync follows the setting, as POST /auto-sync does.
    if let Some(on) = auto {
        if on != before || (on && !ctx.ctrl.is_watching()) {
            let ctrl = ctx.ctrl.clone();
            blocking(move || restart_watch(&ctrl, on)).await?;
        }
    }
    Ok(Json(Settings::of(&next)))
}

// ---- pairs ---------------------------------------------------------------------

#[derive(Serialize)]
struct PairOut {
    name: String,
    local: String,
    remote: String,
    auto: bool,
    exclude: Vec<String>,
}

fn pairs_of(cfg: &Config) -> Vec<PairOut> {
    cfg.pairs
        .iter()
        .map(|p| PairOut {
            name: p.name.clone(),
            local: p.local.display().to_string(),
            remote: p.remote.clone(),
            auto: p.auto,
            exclude: p.exclude.clone(),
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PairIn {
    name: String,
    local: String,
    remote: String,
    #[serde(default = "yes")]
    auto: bool,
    #[serde(default)]
    exclude: Vec<String>,
}

fn yes() -> bool {
    true
}

impl PairIn {
    fn check(&self, ctx: &Ctx) -> ApiResult<()> {
        if self.name.trim().is_empty() {
            return Err(ApiError::invalid_config("A synced folder needs a name."));
        }
        files::check_pair_local(&ctx.pair_roots, &self.local)?;
        Ok(())
    }

    fn to_toml(&self) -> toml::Value {
        let mut t = toml::Table::new();
        t.insert("name".into(), toml::Value::String(self.name.trim().into()));
        t.insert("local".into(), toml::Value::String(self.local.clone()));
        t.insert("remote".into(), toml::Value::String(self.remote.clone()));
        t.insert("auto".into(), toml::Value::Boolean(self.auto));
        if !self.exclude.is_empty() {
            t.insert(
                "exclude".into(),
                toml::Value::Array(
                    self.exclude
                        .iter()
                        .map(|e| toml::Value::String(e.clone()))
                        .collect(),
                ),
            );
        }
        toml::Value::Table(t)
    }
}

fn pair_list(doc: &mut toml::Table) -> &mut Vec<toml::Value> {
    let entry = doc
        .entry("pair".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    if !entry.is_array() {
        *entry = toml::Value::Array(Vec::new());
    }
    entry.as_array_mut().expect("an array")
}

fn pair_name(v: &toml::Value) -> Option<&str> {
    v.get("name").and_then(toml::Value::as_str)
}

/// After a change to the folder set, a running auto sync is restarted so it
/// watches the new set. Done in the background: the response does not wait
/// for the old watcher to let go.
fn rewatch(ctx: &Ctx) {
    if ctx.ctrl.is_watching() {
        let ctrl = ctx.ctrl.clone();
        tokio::task::spawn_blocking(move || restart_watch(&ctrl, true));
    }
}

async fn pairs_get(State(ctx): Ctl) -> Json<Vec<PairOut>> {
    Json(pairs_of(&ctx.ctrl.config()))
}

async fn pairs_add(State(ctx): Ctl, Body(pair): Body<PairIn>) -> ApiResult<impl IntoResponse> {
    pair.check(&ctx)?;
    let name = pair.name.trim().to_string();
    let next = edit_config(&ctx, move |doc| {
        let list = pair_list(doc);
        if list.iter().any(|p| pair_name(p) == Some(name.as_str())) {
            return Err(ApiError::conflict(format!(
                "A synced folder named {name:?} already exists."
            )));
        }
        list.push(pair.to_toml());
        Ok(())
    })
    .await?;
    rewatch(&ctx);
    Ok((StatusCode::CREATED, Json(pairs_of(&next))))
}

async fn pairs_replace(
    State(ctx): Ctl,
    Path(old): Path<String>,
    Body(pair): Body<PairIn>,
) -> ApiResult<Json<Vec<PairOut>>> {
    pair.check(&ctx)?;
    let new_name = pair.name.trim().to_string();
    let old2 = old.clone();
    let next = edit_config(&ctx, move |doc| {
        let list = pair_list(doc);
        let Some(i) = list
            .iter()
            .position(|p| pair_name(p) == Some(old2.as_str()))
        else {
            return Err(ApiError::not_found(format!(
                "There is no synced folder named {old2:?}."
            )));
        };
        if new_name != old2 && list.iter().any(|p| pair_name(p) == Some(new_name.as_str())) {
            return Err(ApiError::conflict(format!(
                "A synced folder named {new_name:?} already exists."
            )));
        }
        list[i] = pair.to_toml();
        Ok(())
    })
    .await?;
    if !next.pairs.iter().any(|p| p.name == old) {
        // Renamed: the old name's stored sync state goes, as on removal, so
        // a later folder of that name cannot inherit it. The renamed folder
        // starts as a first sync, which merges both sides and deletes nothing.
        let ctrl = ctx.ctrl.clone();
        blocking(move || ctrl.forget_pair(&old)).await?;
        ctx.ctrl.commit_config(next.clone());
    }
    rewatch(&ctx);
    Ok(Json(pairs_of(&next)))
}

async fn pairs_remove(State(ctx): Ctl, Path(name): Path<String>) -> ApiResult<Json<Value>> {
    let name2 = name.clone();
    edit_config(&ctx, move |doc| {
        let list = pair_list(doc);
        let before = list.len();
        list.retain(|p| pair_name(p) != Some(name2.as_str()));
        if list.len() == before {
            return Err(ApiError::not_found(format!(
                "There is no synced folder named {name2:?}."
            )));
        }
        Ok(())
    })
    .await?;
    let ctrl = ctx.ctrl.clone();
    let n = name.clone();
    blocking(move || ctrl.forget_pair(&n)).await?;
    rewatch(&ctx);
    Ok(Json(json!({
        "removed": name,
        "files_deleted": false,
        "message": "The folder is no longer synced. No file was deleted on this computer or on Proton Drive.",
    })))
}

// ---- account -------------------------------------------------------------------

/// Re-check the account and wait (up to 20 s) for the answer.
fn refresh_and_wait(ctx: &Ctx, signal_watch: bool) {
    ctx.ctrl.refresh_account(signal_watch);
    let t0 = Instant::now();
    while ctx.ctrl.snapshot().account.checking && t0.elapsed() < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(25));
    }
    *ctx.account_checked.lock().unwrap() = Instant::now();
}

async fn account(State(ctx): Ctl) -> ApiResult<impl IntoResponse> {
    let ctx2 = ctx.clone();
    let state = blocking(move || {
        let stale = ctx2.account_checked.lock().unwrap().elapsed() > Duration::from_secs(30);
        if stale || ctx2.ctrl.snapshot().account.checking {
            refresh_and_wait(&ctx2, false);
        }
        ctx2.ctrl.snapshot().account
    })
    .await?;
    Ok(Json(state))
}

/// The sidecar's answer as one of the four steps.
fn step(reply: &Value) -> ApiResult<Value> {
    if reply["ok"] == true {
        Ok(json!({"step": "done"}))
    } else if reply["need_2fa"] == true {
        Ok(json!({"step": "need_2fa"}))
    } else if reply["need_mailbox_password"] == true {
        Ok(json!({"step": "need_mailbox_password"}))
    } else if reply["need_human_verification"] == true {
        Ok(json!({
            "step": "need_human_verification",
            "url": reply["url"],
            "token": reply["token"],
            "methods": reply["methods"],
        }))
    } else {
        Err(ApiError::backend(
            "The sign-in service gave an answer this version does not understand.",
        ))
    }
}

/// Plain words for a sign-in the sidecar refused, by its error code. Never
/// includes anything the client sent.
fn sign_in_message(code: &str) -> &'static str {
    match code {
        "auth" => "Proton did not accept the sign-in. Check the details and try again.",
        "rate_limited" => "Proton is limiting sign-in attempts. Wait a minute and try again.",
        "transient" => "Proton could not be reached. Try again in a moment.",
        _ => "Sign-in failed.",
    }
}

/// D15: one sign-in call through the sidecar, exactly as the window makes it,
/// then a fresh account check.
async fn auth_call(ctx: Arc<Ctx>, method: &'static str, params: Value) -> ApiResult<Json<Value>> {
    let _guard = ctx.auth_lock.lock().await;
    let ctx2 = ctx.clone();
    let result = blocking(move || {
        let cfg = ctx2.ctrl.config();
        let result = crate::driveapi::DriveApi::new(&cfg).and_then(|api| api.auth(method, params));
        refresh_and_wait(&ctx2, true);
        result
    })
    .await?;
    match result {
        Ok(reply) => Ok(Json(step(&reply)?)),
        Err(e) => match e.downcast_ref::<crate::driveapi::RpcError>() {
            Some(rpc) => {
                ctx.refused("sign-in refused by Proton");
                Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "sign_in_failed",
                    sign_in_message(&rpc.code),
                ))
            }
            None => Err(ApiError::backend(format!(
                "The sign-in service did not start: {e}"
            ))),
        },
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    username: String,
    password: String,
    hv_token: Option<String>,
    hv_type: Option<String>,
}

async fn login(State(ctx): Ctl, PrivateBody(b): PrivateBody<LoginBody>) -> ApiResult<Json<Value>> {
    let mut params = json!({"username": b.username, "password": b.password});
    if let Some(token) = b.hv_token {
        params["hv_token"] = json!(token);
        params["hv_type"] = json!(b.hv_type.unwrap_or_else(|| "captcha".into()));
    }
    auth_call(ctx, "auth.login", params).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeBody {
    code: String,
}

async fn two_factor(
    State(ctx): Ctl,
    PrivateBody(b): PrivateBody<CodeBody>,
) -> ApiResult<Json<Value>> {
    auth_call(ctx, "auth.submit_2fa", json!({"code": b.code})).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordBody {
    password: String,
}

async fn mailbox_password(
    State(ctx): Ctl,
    PrivateBody(b): PrivateBody<PasswordBody>,
) -> ApiResult<Json<Value>> {
    auth_call(
        ctx,
        "auth.submit_mailbox_password",
        json!({"password": b.password}),
    )
    .await
}

async fn logout(State(ctx): Ctl) -> ApiResult<Json<Value>> {
    auth_call(ctx, "auth.logout", json!({})).await
}

// ---- browsing and logs ------------------------------------------------------------

#[derive(Deserialize)]
struct PathQuery {
    path: Option<String>,
}

#[derive(Serialize)]
struct RemoteEntry {
    name: String,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mtime: Option<i64>,
}

async fn remote_list(State(ctx): Ctl, Q(q): Q<PathQuery>) -> ApiResult<Json<Vec<RemoteEntry>>> {
    let cfg = ctx.ctrl.config();
    let path = q.path.unwrap_or_else(|| cfg.remote_root.clone());
    if !path.starts_with('/') {
        return Err(ApiError::bad_request("path must start with /."));
    }
    let listed = blocking(move || crate::backend::list_remote(&cfg, &path)).await?;
    let mut entries = match listed {
        Ok(entries) => entries,
        Err(e) if crate::remote::is_not_logged_in(&e) => return Err(ApiError::signed_out()),
        Err(e) => {
            return Err(ApiError::backend(format!(
                "Proton Drive could not list that folder: {e}"
            )))
        }
    };
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.path.cmp(&b.path)));
    Ok(Json(
        entries
            .into_iter()
            .map(|e| RemoteEntry {
                kind: if e.is_dir { "folder" } else { "file" },
                size: (!e.is_dir).then_some(e.size),
                mtime: if e.is_dir { None } else { e.mtime },
                name: e.path,
            })
            .collect(),
    ))
}

async fn local_list(State(ctx): Ctl, Q(q): Q<PathQuery>) -> ApiResult<impl IntoResponse> {
    let roots = ctx.browse_roots.clone();
    let listed = blocking(move || files::list_local(&roots, q.path.as_deref())).await?;
    match listed {
        Ok(entries) => Ok(Json(entries)),
        Err(e) => {
            if e.status == StatusCode::FORBIDDEN {
                ctx.refused("folder outside the allowed roots");
            }
            Err(e)
        }
    }
}

#[derive(Deserialize)]
struct LogsQuery {
    lines: Option<usize>,
}

async fn logs(State(ctx): Ctl, Q(q): Q<LogsQuery>) -> ApiResult<Json<Value>> {
    let n = q.lines.unwrap_or(200).min(2000);
    let path = ctx.ctrl.config().log_dir().join("sync.log");
    let lines = blocking(move || files::tail_lines(&path, n))
        .await?
        .map_err(|e| ApiError::internal(format!("cannot read the log: {e}")))?;
    Ok(Json(json!({"lines": lines})))
}

#[cfg(test)]
mod tests {
    use super::same_secret;

    #[test]
    fn secrets_compare_by_value_and_length() {
        assert!(same_secret(b"abc", b"abc"));
        assert!(!same_secret(b"abc", b"abd"));
        assert!(!same_secret(b"abc", b"abcd"));
        assert!(!same_secret(b"", b"a"));
        assert!(same_secret(b"", b""));
    }
}
