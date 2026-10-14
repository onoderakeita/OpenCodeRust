use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: crate::identifier::SessionID,
    pub slug: String,
    pub project_id: crate::project::ProjectID,
    pub directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<crate::identifier::SessionID>,
    pub title: String,
    pub version: String,
    pub time: crate::session::TimeInfo,
    #[serde(skip)]
    pub messages: Vec<crate::message::SessionMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::session::SessionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<crate::session::SessionShareInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert: Option<crate::session::SessionRevert>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<crate::session::PermissionRuleset>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::session::TokenUsage>,
    #[serde(default)]
    pub status: crate::session::SessionStatus,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
    #[serde(default, skip_serializing)]
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing)]
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub additions: u64,
    pub deletions: u64,
    pub files: u64,
    pub diffs: Vec<FileDiff>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq, Default)]
#[sqlx(rename_all = "lowercase")]
pub enum SessionStatus {
    #[default]
    Active,
    Completed,
    Archived,
    Compacting,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionShareInfo {
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input: f64,
    pub output: f64,
    pub reasoning: f64,
    pub cache: CacheUsage,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheUsage {
    pub read: f64,
    pub write: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRevert {
    pub message_id: String,
    pub part_id: Option<String>,
    pub snapshot: Option<String>,
    pub diff: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PermissionRuleset {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeInfo {
    pub created: u64,
    pub updated: u64,
    pub compacting: Option<u64>,
    pub archived: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInput {
    pub id: String,
    pub session_id: String,
    pub prompt: String,
    pub delivery: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    pub status: String,
    pub created_at: u64,
}
