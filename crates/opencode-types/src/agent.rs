use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AgentID(pub String);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentColor {
    Primary,
    Secondary,
    Accent,
    Success,
    Warning,
    Error,
    Info,
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    pub id: AgentID,
    pub model: Option<String>,
    pub request: ProviderRequest,
    pub system: Option<String>,
    pub description: Option<String>,
    pub mode: AgentMode,
    pub hidden: bool,
    pub color: Option<AgentColor>,
    pub steps: Option<u32>,
    pub permissions: Vec<String>, // Simplified for now
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRequest {
    pub headers: HashMap<String, String>,
    pub body: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    Subagent,
    Primary,
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_agent_info() {
        let agent = AgentInfo {
            id: AgentID("agent_1".to_string()),
            model: Some("gpt-4".to_string()),
            request: ProviderRequest {
                headers: HashMap::new(),
                body: HashMap::new(),
            },
            system: None,
            description: None,
            mode: AgentMode::All,
            hidden: false,
            color: Some(AgentColor::Primary),
            steps: Some(10),
            permissions: vec![],
        };
        let serialized = serde_json::to_string(&agent).unwrap();
        let deserialized: AgentInfo = serde_json::from_str(&serialized).unwrap();
        assert_eq!(agent, deserialized);
    }
}
