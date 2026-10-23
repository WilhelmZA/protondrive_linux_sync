//! Trash endpoints; filesystem work never runs on the async executor.
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{error::Body, ApiError, Ctx};
use crate::trash_browser::{self, TrashItem};

pub(super) fn router() -> Router<Arc<Ctx>> {
    Router::new()
        .route("/api/v1/trash", get(list))
        .route("/api/v1/trash/{pair}/{id}/restore", post(restore))
        .route("/api/v1/trash/{pair}/{id}/delete", post(delete))
}

fn entries(ctx: &Ctx) -> Result<Vec<TrashItem>, ApiError> {
    let mut items = Vec::new();
    for pair in &ctx.ctrl.config().pairs {
        let roots = trash_browser::roots(&pair.local);
        items.extend(
            trash_browser::list(&pair.name, &pair.local, &roots).map_err(|_| {
                ApiError::conflict(
                    "Cannot read a synced folder's trash. Check that the folder is mounted.",
                )
            })?,
        );
    }
    Ok(items)
}

async fn list(State(ctx): State<Arc<Ctx>>) -> Result<Json<Vec<TrashItem>>, ApiError> {
    tokio::task::spawn_blocking(move || entries(&ctx).map(Json))
        .await
        .map_err(|_| ApiError::internal("Cannot read trash."))?
}

async fn action(
    ctx: Arc<Ctx>,
    pair: String,
    id: String,
    restore: bool,
) -> Result<Json<Value>, ApiError> {
    let _lock = ctx.config_lock.lock().await;
    let ctx = ctx.clone();
    tokio::task::spawn_blocking(move || {
        let item = entries(&ctx)?
            .into_iter()
            .find(|i| i.pair == pair && i.id == id)
            .ok_or_else(|| ApiError::not_found("No such trash entry. Refresh the list."))?;
        trash_browser::act(&item, restore).map_err(|e| {
            ApiError::conflict(format!("Trash action failed: {e}. Nothing is overwritten."))
        })?;
        Ok(Json(json!({"ok": true})))
    })
    .await
    .map_err(|_| ApiError::internal("Trash action failed."))?
}

async fn restore(
    State(ctx): State<Arc<Ctx>>,
    Path((pair, id)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    action(ctx, pair, id, true).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    #[serde(default)]
    confirm: bool,
}

async fn delete(
    State(ctx): State<Arc<Ctx>>,
    Path((pair, id)): Path<(String, String)>,
    Body(body): Body<Confirmation>,
) -> Result<Json<Value>, ApiError> {
    if !body.confirm {
        return Err(ApiError::bad_request(
            "Permanent deletion needs {\"confirm\":true}. Nothing was deleted.",
        ));
    }
    action(ctx, pair, id, false).await
}
