//! Chat domain types.
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: Option<String>,
    pub updated_at: String,
    pub preview: Option<String>,
    pub message_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessageRow {
    pub id: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub id: String,
    pub session_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub status: String, // running | done | failed
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Selection {
    pub provider: String,
    pub model: Option<String>,
}
