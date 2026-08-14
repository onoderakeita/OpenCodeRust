use crate::identifier::PtyID;
use serde::{Deserialize, Serialize}; // Need to make sure identifier exists

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PtyInfo {
    pub id: PtyID,
    pub title: String,
    pub command: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub status: PtyStatus,
    pub pid: u64,
    pub exit_code: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PtyStatus {
    Running,
    Exited,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateInput {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub env: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateInput {
    pub title: Option<String>,
    pub size: Option<PtySize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PtySize {
    pub rows: u32,
    pub cols: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_pty_info() {
        let info = PtyInfo {
            id: PtyID("pty_123".to_string()),
            title: "Terminal".to_string(),
            command: "bash".to_string(),
            args: vec![],
            cwd: "/".to_string(),
            status: PtyStatus::Running,
            pid: 100,
            exit_code: None,
        };
        let serialized = serde_json::to_string(&info).unwrap();
        let deserialized: PtyInfo = serde_json::from_str(&serialized).unwrap();
        assert_eq!(info, deserialized);
    }
}
