//! OpenAPI 3.1 document for `/api/v1`, generated from the types the server
//! serialises. `examples/openapi.rs` prints it; `GET /api/v1/openapi.json`
//! serves the same text.

#![allow(dead_code)]

use utoipa::OpenApi;

use super::files::LocalEntry;
use super::publish::Envelope;
use super::routes::{
    AutoSyncBody, CodeBody, ConfirmBody, LoginBody, PairIn, PairOut, PasswordBody, RemoteEntry,
    Settings, SyncBody,
};
use crate::service::AccountState;
use crate::stats::{ConflictRecord, HeldDeletes};

/// Pretty-printed OpenAPI JSON with a trailing newline.
///
/// Object keys come out alphabetically: `serde_json::Map` is a `BTreeMap`
/// unless `preserve_order` is on, which this crate does not enable. That
/// order is stable from run to run.
pub(crate) fn json_pretty() -> String {
    let mut value =
        serde_json::to_value(ApiDoc::openapi()).expect("the OpenAPI document serialises");
    drop_empty_tags(&mut value);
    let mut text = serde_json::to_string_pretty(&value).expect("the OpenAPI document formats");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// utoipa writes `"tags": []` on operations that have no tag. Drop those so
/// the committed document does not change when a tag list is merely empty.
fn drop_empty_tags(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.retain(|key, child| {
                !(key == "tags" && child.as_array().is_some_and(|a| a.is_empty()))
            });
            for child in map.values_mut() {
                drop_empty_tags(child);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                drop_empty_tags(item);
            }
        }
        _ => {}
    }
}

struct BearerAuth;

impl utoipa::Modify for BearerAuth {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearerAuth",
            utoipa::openapi::security::SecurityScheme::Http(
                utoipa::openapi::security::HttpBuilder::new()
                    .scheme(utoipa::openapi::security::HttpAuthScheme::Bearer)
                    .description(Some(
                        "API token on TCP, as `Authorization: Bearer <token>`. \
                         Required on TCP only when a token is configured. \
                         The unix socket does not use a bearer token. \
                         `GET /api/v1/health` never requires one.",
                    ))
                    .build(),
            ),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "NeutronSync HTTP API",
        description = "The `/api/v1` contract served by `neutronsync serve` and the desktop tray. No field or endpoint is removed, and none changes its meaning. New optional response fields and new endpoints may be added; clients ignore fields they do not know. Request bodies refuse unknown fields. Bodies are JSON, at most 64 KiB. The prose contract is `docs/HTTP_API.md`."
    ),
    servers(
        (url = "http://127.0.0.1:8090", description = "Default loopback TCP listener. The desktop tray serves the same paths on its unix socket.")
    ),
    paths(
        health,
        openapi_json,
        version,
        state,
        events,
        sync,
        cancel,
        reset,
        auto_sync,
        held,
        held_restore,
        held_delete,
        conflicts,
        conflict_dismiss,
        conflicts_dismiss_all,
        settings_get,
        settings_put,
        pairs_get,
        pairs_add,
        pairs_replace,
        pairs_remove,
        account,
        login,
        two_factor,
        mailbox_password,
        logout,
        remote_list,
        local_list,
        logs
    ),
    security((), ("bearerAuth" = [])),
    modifiers(&BearerAuth),
    version = "3.1.0"
)]
struct ApiDoc;

#[derive(utoipa::ToSchema)]
struct Health {
    ok: bool,
}

#[derive(utoipa::ToSchema)]
struct ApiVersion {
    api_version: u32,
    /// Crate version (`neutronsync --version` without the build).
    version: String,
    /// Commit the binary was built from, the same string `neutronsync --version` shows.
    build: String,
}

#[derive(utoipa::ToSchema)]
struct Accepted {
    /// Always true when the status is 202.
    accepted: bool,
}

#[derive(utoipa::ToSchema)]
struct ApiErrorBody {
    error: ApiErrorDetail,
}

#[derive(utoipa::ToSchema)]
struct ApiErrorDetail {
    /// `bad_request`, `unauthorized`, `sign_in_failed`, `forbidden`, `not_found`, `busy`, `conflict`, `signed_out`, `too_large`, `invalid_config`, `backend`, or `internal`.
    code: String,
    /// A sentence. Passwords and codes are never included.
    message: String,
}

#[derive(utoipa::ToSchema)]
struct AutoSyncReply {
    auto_sync: bool,
}

#[derive(utoipa::ToSchema)]
struct PairRemoved {
    removed: String,
    /// Always false. Removing a folder does not delete a file on either side.
    files_deleted: bool,
    message: String,
}

#[derive(utoipa::ToSchema)]
struct LogLines {
    lines: Vec<String>,
}

/// The next sign-in step. `url`, `token`, and `methods` are present only when `step` is `need_human_verification`.
#[derive(utoipa::ToSchema)]
struct AuthStep {
    /// `done`, `need_2fa`, `need_mailbox_password`, or `need_human_verification`.
    step: String,
    url: Option<String>,
    /// Proton human-verification token, not the API bearer token.
    token: Option<String>,
    methods: Option<Vec<String>>,
}

#[utoipa::path(
    get,
    path = "/api/v1/health",
    operation_id = "health",
    security(()),
    responses(
        (status = 200, description = "The process is up. No token, origin, or Host check.", body = Health)
    )
)]
fn health() {}

#[utoipa::path(
    get,
    path = "/api/v1/openapi.json",
    operation_id = "openapi",
    responses(
        (status = 200, description = "This OpenAPI document. Same authentication as `GET /api/v1/state`.", content_type = "application/json", body = serde_json::Value),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn openapi_json() {}

#[utoipa::path(
    get,
    path = "/api/v1/version",
    operation_id = "version",
    responses(
        (status = 200, description = "API version, crate version, and build commit.", body = ApiVersion),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn version() {}

#[utoipa::path(
    get,
    path = "/api/v1/state",
    operation_id = "state",
    params(
        ("activity" = Option<usize>, Query, description = "Newest activity items to keep. Default 100, at most 500.")
    ),
    responses(
        (status = 200, description = "The state envelope. `rev` matches `GET /api/v1/events` for the same content.", body = Envelope),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn state() {}

#[utoipa::path(
    get,
    path = "/api/v1/events",
    operation_id = "events",
    responses(
        (
            status = 200,
            description = "Server-sent events (`text/event-stream`). Each event is named `state`. The `data` line is an Envelope: `rev`, `state`, `held`, `conflicts`, `auto_sync` (the newest 100 activity items). With nothing to report, the server sends an SSE comment `: keepalive` every 15 seconds. The stream ends when the server shuts down.",
            content_type = "text/event-stream",
            body = Envelope
        ),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    ),
    extensions(
        ("x-sse-events" = json!([{"name": "state", "data": {"$ref": "#/components/schemas/Envelope"}}])),
        ("x-sse-comments" = json!([": keepalive"]))
    )
)]
fn events() {}

#[utoipa::path(
    post,
    path = "/api/v1/sync",
    operation_id = "sync",
    request_body = SyncBody,
    responses(
        (status = 202, description = "The run was started.", body = Accepted),
        (status = 400, description = "Unknown folder name, or the body was refused.", body = ApiErrorBody),
        (status = 409, description = "`busy` (a sync is running), `conflict` (no folder yet), or `signed_out`.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn sync() {}

#[utoipa::path(
    post,
    path = "/api/v1/cancel",
    operation_id = "cancel",
    responses(
        (status = 202, description = "Stop was requested. No body is required.", body = Accepted),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn cancel() {}

#[utoipa::path(
    post,
    path = "/api/v1/reset",
    operation_id = "reset",
    request_body = ConfirmBody,
    responses(
        (status = 202, description = "Stored sync metadata was wiped. No file was deleted.", body = Accepted),
        (status = 400, description = "`confirm` was not true. Nothing was changed.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn reset() {}

#[utoipa::path(
    post,
    path = "/api/v1/auto-sync",
    operation_id = "autoSync",
    request_body = AutoSyncBody,
    responses(
        (status = 200, description = "Auto sync was saved and applied.", body = AutoSyncReply),
        (status = 422, description = "The config would not load. Nothing was changed.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn auto_sync() {}

#[utoipa::path(
    get,
    path = "/api/v1/held",
    operation_id = "held",
    responses(
        (status = 200, description = "Pairs whose deletions are held back.", body = Vec<HeldDeletes>),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn held() {}

#[utoipa::path(
    post,
    path = "/api/v1/held/{pair}/restore",
    operation_id = "heldRestore",
    params(("pair" = String, Path, description = "Synced folder name.")),
    responses(
        (status = 202, description = "A merge of that folder was started.", body = Accepted),
        (status = 404, description = "Unknown folder, or nothing is held for it.", body = ApiErrorBody),
        (status = 409, description = "A sync is already running.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn held_restore() {}

#[utoipa::path(
    post,
    path = "/api/v1/held/{pair}/delete",
    operation_id = "heldDelete",
    params(("pair" = String, Path, description = "Synced folder name.")),
    request_body = ConfirmBody,
    responses(
        (status = 202, description = "The held deletions will go through on the next run of that folder.", body = Accepted),
        (status = 400, description = "`confirm` was not true. Nothing was deleted.", body = ApiErrorBody),
        (status = 404, description = "Unknown folder, or nothing is held for it.", body = ApiErrorBody),
        (status = 409, description = "A sync is already running.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn held_delete() {}

#[utoipa::path(
    get,
    path = "/api/v1/conflicts",
    operation_id = "conflicts",
    responses(
        (status = 200, description = "Settled conflicts not yet dismissed, newest first.", body = Vec<ConflictRecord>),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn conflicts() {}

#[utoipa::path(
    post,
    path = "/api/v1/conflicts/{id}/dismiss",
    operation_id = "conflictDismiss",
    params(("id" = String, Path, description = "Conflict id. A non-integer is 404.")),
    responses(
        (status = 202, description = "The conflict was dismissed.", body = Accepted),
        (status = 404, description = "No such conflict.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn conflict_dismiss() {}

#[utoipa::path(
    post,
    path = "/api/v1/conflicts/dismiss-all",
    operation_id = "conflictsDismissAll",
    responses(
        (status = 202, description = "Every conflict was dismissed.", body = Accepted),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn conflicts_dismiss_all() {}

#[utoipa::path(
    get,
    path = "/api/v1/settings",
    operation_id = "settingsGet",
    responses(
        (status = 200, description = "The settings a server can use. A later version may add optional fields.", body = Settings),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn settings_get() {}

#[utoipa::path(
    put,
    path = "/api/v1/settings",
    operation_id = "settingsPut",
    request_body = Settings,
    responses(
        (status = 200, description = "The full settings object after the change.", body = Settings),
        (status = 400, description = "Unknown key or a value of the wrong type.", body = ApiErrorBody),
        (status = 422, description = "The config would not load. Nothing was changed.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn settings_put() {}

#[utoipa::path(
    get,
    path = "/api/v1/pairs",
    operation_id = "pairsGet",
    responses(
        (status = 200, description = "Synced folders, in config order.", body = Vec<PairOut>),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn pairs_get() {}

#[utoipa::path(
    post,
    path = "/api/v1/pairs",
    operation_id = "pairsAdd",
    request_body = PairIn,
    responses(
        (status = 201, description = "The folder list, including the new folder.", body = Vec<PairOut>),
        (status = 409, description = "A folder with that name already exists.", body = ApiErrorBody),
        (status = 422, description = "The path or the name is not a valid config.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn pairs_add() {}

#[utoipa::path(
    put,
    path = "/api/v1/pairs/{name}",
    operation_id = "pairsReplace",
    params(("name" = String, Path, description = "Current folder name.")),
    request_body = PairIn,
    responses(
        (status = 200, description = "The folder list after the replacement.", body = Vec<PairOut>),
        (status = 404, description = "No folder with that name.", body = ApiErrorBody),
        (status = 409, description = "The new name is already taken.", body = ApiErrorBody),
        (status = 422, description = "The path or the name is not a valid config.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn pairs_replace() {}

#[utoipa::path(
    delete,
    path = "/api/v1/pairs/{name}",
    operation_id = "pairsRemove",
    params(("name" = String, Path, description = "Folder name.")),
    responses(
        (status = 200, description = "The folder is no longer synced. No file was deleted.", body = PairRemoved),
        (status = 404, description = "No folder with that name.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn pairs_remove() {}

#[utoipa::path(
    get,
    path = "/api/v1/account",
    operation_id = "account",
    responses(
        (status = 200, description = "Account state, re-checked when the last check is more than 30 seconds old.", body = AccountState),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn account() {}

#[utoipa::path(
    post,
    path = "/api/v1/account/login",
    operation_id = "login",
    request_body = LoginBody,
    responses(
        (status = 200, description = "The next sign-in step.", body = AuthStep),
        (status = 401, description = "Proton refused the step (`sign_in_failed`).", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn login() {}

#[utoipa::path(
    post,
    path = "/api/v1/account/2fa",
    operation_id = "twoFactor",
    request_body = CodeBody,
    responses(
        (status = 200, description = "The next sign-in step.", body = AuthStep),
        (status = 401, description = "Proton refused the code (`sign_in_failed`).", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn two_factor() {}

#[utoipa::path(
    post,
    path = "/api/v1/account/mailbox-password",
    operation_id = "mailboxPassword",
    request_body = PasswordBody,
    responses(
        (status = 200, description = "The next sign-in step.", body = AuthStep),
        (status = 401, description = "Proton refused the password (`sign_in_failed`).", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn mailbox_password() {}

#[utoipa::path(
    post,
    path = "/api/v1/account/logout",
    operation_id = "logout",
    responses(
        (status = 200, description = "Signed out. `step` is `done`.", body = AuthStep),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn logout() {}

#[utoipa::path(
    get,
    path = "/api/v1/remote/list",
    operation_id = "remoteList",
    params(
        ("path" = Option<String>, Query, description = "Proton Drive folder. Defaults to `remote_root`. Must start with `/`.")
    ),
    responses(
        (status = 200, description = "Folders first, then files, each sorted by name.", body = Vec<RemoteEntry>),
        (status = 409, description = "Signed out of Proton.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn remote_list() {}

#[utoipa::path(
    get,
    path = "/api/v1/local/list",
    operation_id = "localList",
    params(
        ("path" = Option<String>, Query, description = "Folder on this machine. Without it, the folders the server may browse.")
    ),
    responses(
        (status = 200, description = "Child folders only, sorted by name.", body = Vec<LocalEntry>),
        (status = 403, description = "The path is outside the folders this server may browse.", body = ApiErrorBody),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn local_list() {}

#[utoipa::path(
    get,
    path = "/api/v1/logs",
    operation_id = "logs",
    params(
        ("lines" = Option<usize>, Query, description = "Lines from the end of `sync.log`. Default 200, at most 2000.")
    ),
    responses(
        (status = 200, description = "The last lines of the sync log.", body = LogLines),
        (status = "default", description = "Error. `error.code` is the stable name.", body = ApiErrorBody)
    )
)]
fn logs() {}

#[cfg(test)]
mod tests {
    #[test]
    fn spec_is_openapi_31_and_names_the_state_stream() {
        let doc: serde_json::Value = serde_json::from_str(&super::json_pretty()).unwrap();
        let version = doc["openapi"].as_str().unwrap();
        assert!(version.starts_with("3.1"), "{version}");
        assert!(doc["paths"].get("/api/v1/state").is_some());
        assert!(doc["paths"].get("/api/v1/reset").is_some());
        assert!(doc["paths"].get("/api/v1/openapi.json").is_some());
        let event = &doc["paths"]["/api/v1/events"]["get"];
        assert!(event["responses"]["200"]["content"]
            .get("text/event-stream")
            .is_some());
        let names = event["x-sse-events"].as_array().unwrap();
        assert_eq!(names[0]["name"], "state");
        let described = event["responses"]["200"]["description"].as_str().unwrap();
        assert!(described.contains("event is named `state`"));
        assert!(described.contains(": keepalive"));
    }
}
