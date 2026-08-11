use serde::{Deserialize, Serialize};
use crate::identifier::SessionID; // Assume exists
use crate::project::ProjectID; // Assume exists
use crate::agent::AgentID; // Assume exists

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionID,
    pub parent_id: Option<SessionID>,
    pub project_id: ProjectID,
    pub agent: Option<AgentID>,
    pub model: Option<String>,
    pub cost: f64,
    pub tokens: TokenUsage,
    pub time: TimeInfo,
    pub title: String,
    pub location: String,
    pub subpath: Option<String>,
    pub revert: Option<String>,
    // Added fields expected by storage
    pub slug: String,
    pub directory: String,
    pub version: i32,
    pub summary: Option<String>,
    pub permission: Option<serde_json::Value>,
    pub usage: Option<TokenUsage>,
    pub status: String,
    pub metadata: std::collections::HashMap<String, serde_json::Value>,
    pub created_at: u64,
    pub updated_at: u64,
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
pub struct TimeInfo {
    pub created: u64, // Millis
    pub updated: u64, // Millis
    pub archived: Option<u64>,
}
