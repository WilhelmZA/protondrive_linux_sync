//! Remote notifications are reconcile hints, never filesystem operations.
use anyhow::Result;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct RemoteEvent {
    pub node_uid: String,
    pub parent_uid: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct EventBatch {
    pub scope_id: String,
    pub events: Vec<RemoteEvent>,
    pub last_event_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum FeedMessage {
    Batch(EventBatch),
    Refresh { scope_id: String, reason: String },
    SignedOut,
}

pub struct Subscription {
    pub scope_id: String,
    pub last_event_id: Option<String>,
}

pub trait RemoteChangeFeed: Send + Sync {
    fn subscribe(&self, scope: &str, since: Option<&str>) -> Result<Subscription>;
    fn next_batch(&self) -> Result<Option<FeedMessage>>;
    fn ack(&self, scope: &str, event_id: &str) -> Result<()>;
    fn resolve_root(&self, path: &str) -> Result<String>;
    fn node_path(&self, uid: &str) -> Result<String>;
    fn signed_in(&self) -> Result<bool>;
}
