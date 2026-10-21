//! JSON errors and body extraction for the HTTP API.
//!
//! Every failure is `{"error":{"code":"<word>","message":"<plain sentence>"}}`
//! with the status the code implies, so a client can branch on `code` and
//! show `message` as it is.

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::sync::Arc;

use super::Ctx;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "The request needs a valid API token.",
        )
    }
    pub fn unsupported_body() -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "bad_request",
            "A request body must be sent as Content-Type: application/json.",
        )
    }
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
    pub fn busy() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "busy",
            "A sync is already running. Wait for it to finish or cancel it.",
        )
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }
    pub fn signed_out() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "signed_out",
            "Signed out of Proton. Sign in on the Account page.",
        )
    }
    pub fn too_large() -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "The request body is over 64 KiB.",
        )
    }
    pub fn invalid_config(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_config", message)
    }
    pub fn backend(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "backend", message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({"error": {"code": self.code, "message": self.message}});
        (self.status, Json(body)).into_response()
    }
}

/// A JSON request body. An empty body reads as `{}` so a command that needs
/// no fields can be sent bare. A body over the limit is 413, anything that
/// does not parse is 400 with the parser's reason.
pub struct Body<T>(pub T);

/// Like [`Body`], for bodies that carry a password or a code: a parse failure
/// is reported without the parser's text, so no part of the body can be
/// echoed into an error message.
pub struct PrivateBody<T>(pub T);

async fn read_json<T: DeserializeOwned>(
    req: Request,
    ctx: &Arc<Ctx>,
    private: bool,
) -> Result<T, ApiError> {
    let bytes = match Bytes::from_request(req, ctx).await {
        Ok(bytes) => bytes,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            ctx.refused("body over 64 KiB");
            return Err(ApiError::too_large());
        }
        Err(_) => return Err(ApiError::bad_request("The request body could not be read.")),
    };
    let text: &[u8] = if bytes.iter().all(u8::is_ascii_whitespace) {
        b"{}"
    } else {
        &bytes
    };
    serde_json::from_slice(text).map_err(|e| {
        if private {
            ApiError::bad_request("The request body is not the JSON object this call takes.")
        } else {
            ApiError::bad_request(friendly_json_error(text, &e))
        }
    })
}

/// Turn serde's parse errors into a short message that names the field when possible.
fn friendly_json_error(text: &[u8], e: &serde_json::Error) -> String {
    if let Ok(val) = serde_json::from_slice::<serde_json::Value>(text) {
        if let Some(obj) = val.as_object() {
            for (key, v) in obj {
                if let Some(n) = v.as_i64() {
                    if n < 0 {
                        return format!("{key} must be a whole number.");
                    }
                }
                if v.is_f64() && v.as_f64().unwrap_or(0.0).fract() != 0.0 {
                    return format!("{key} must be a whole number.");
                }
            }
        }
    }
    let raw = e.to_string();
    if raw.contains("expected u64") || raw.contains("expected i64") {
        if let Some(col) = raw.find("column ") {
            let rest = &raw[col + 7..];
            let end = rest
                .find(')')
                .or_else(|| rest.find(' '))
                .unwrap_or(rest.len());
            if let Ok(pos) = rest[..end].trim().parse::<usize>() {
                if let Ok(s) = std::str::from_utf8(text) {
                    if let Some(field) = field_at_column(s, pos) {
                        return format!("{field} must be a whole number.");
                    }
                }
            }
        }
        return "A number in the request body must be a whole number.".to_string();
    }
    if raw.contains("invalid type") {
        return "The request body is not the JSON object this call takes.".to_string();
    }
    "The request body could not be read.".to_string()
}

fn field_at_column(json: &str, column: usize) -> Option<String> {
    let bytes = json.as_bytes();
    if column == 0 || column > bytes.len() {
        return None;
    }
    let before = &json[..column.min(json.len())];
    let key_start = before.rfind('"')?;
    let rest = &json[key_start + 1..];
    let key_end = rest.find('"')?;
    Some(rest[..key_end].to_string())
}

impl<T: DeserializeOwned> FromRequest<Arc<Ctx>> for Body<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, ctx: &Arc<Ctx>) -> Result<Self, ApiError> {
        read_json(req, ctx, false).await.map(Body)
    }
}

impl<T: DeserializeOwned> FromRequest<Arc<Ctx>> for PrivateBody<T> {
    type Rejection = ApiError;
    async fn from_request(req: Request, ctx: &Arc<Ctx>) -> Result<Self, ApiError> {
        read_json(req, ctx, true).await.map(PrivateBody)
    }
}

/// A query string, with a malformed one answered as the JSON `bad_request`
/// error like every other failure.
pub struct Q<T>(pub T);

impl<T: DeserializeOwned + Send, S: Send + Sync> FromRequestParts<S> for Q<T> {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(v)| Q(v))
            .map_err(|e| ApiError::bad_request(format!("Invalid query string: {}", e.body_text())))
    }
}
