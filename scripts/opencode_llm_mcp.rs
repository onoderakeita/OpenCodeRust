//! Local LLM MCP Server - rust-script version
//!
//! Run with: `rust-script scripts/opencode_llm_mcp.rs`
//!
//! ```cargo
//! [dependencies]
//! serde = { version = "1.0", features = ["derive"] }
//! serde_json = "1.0"
//! tokio = { version = "1.0", features = ["full"] }
//! anyhow = "1.0"
//! tracing = "0.1"
//! ```

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Write};
use std::process::Command;

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: Value,
    #[serde(rename = "clientInfo")]
    client_info: Value,
}

#[derive(Debug, Serialize)]
struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
    capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    server_info: ServerInfo,
}

#[derive(Debug, Serialize)]
struct ServerCapabilities {
    tools: ToolsCapability,
}

#[derive(Debug, Serialize)]
struct ToolsCapability {
    #[serde(rename = "listChanged")]
    list_changed: bool,
}

#[derive(Debug, Serialize)]
struct ServerInfo {
    name: String,
    version: String,
}

#[derive(Debug, Serialize)]
struct Tool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout();
    let mut line = String::new();

    let mut initialized = false;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break; // EOF
        }

        let line_trimmed = line.trim();
        if line_trimmed.is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(line_trimmed) {
            Ok(req) => req,
            Err(e) => {
                send_error(
                    &mut stdout,
                    None,
                    -32700,
                    "Parse error",
                    Some(json!(e.to_string())),
                )?;
                continue;
            }
        };

        let response = handle_request(request, &mut initialized)?;
        let response_json = serde_json::to_string(&response)?;
        writeln!(stdout, "{}", response_json)?;
        stdout.flush()?;
    }

    Ok(())
}

fn send_error(
    stdout: &mut io::Stdout,
    id: Option<Value>,
    code: i32,
    message: &str,
    data: Option<Value>,
) -> io::Result<()> {
    let response = JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
            data,
        }),
    };
    let response_json = serde_json::to_string(&response)?;
    writeln!(stdout, "{}", response_json)?;
    stdout.flush()
}

fn handle_request(
    request: JsonRpcRequest,
    initialized: &mut bool,
) -> Result<JsonRpcResponse, Box<dyn std::error::Error>> {
    let id = request.id.clone();

    match request.method.as_str() {
        "initialize" => {
            let _params: InitializeParams =
                serde_json::from_value(request.params.unwrap_or(json!({})))?;
            *initialized = true;
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!(InitializeResult {
                    protocol_version: "2024-11-05".to_string(),
                    capabilities: ServerCapabilities {
                        tools: ToolsCapability {
                            list_changed: false
                        },
                    },
                    server_info: ServerInfo {
                        name: "opencode-local-llm".to_string(),
                        version: "0.1.0".to_string(),
                    },
                })),
                error: None,
            })
        }
        "initialized" => Ok(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: None,
            result: Some(json!({})),
            error: None,
        }),
        "tools/list" => {
            if !*initialized {
                return Ok(error_response(id, -32002, "Server not initialized"));
            }
            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!({
                    "tools": [
                        Tool {
                            name: "local_llm_query".to_string(),
                            description: "Query a local LLM via ollama".to_string(),
                            input_schema: json!({
                                "type": "object",
                                "properties": {
                                    "model": { "type": "string", "description": "Model name (e.g., gemma4:4b)" },
                                    "prompt": { "type": "string", "description": "Input prompt" },
                                    "system": { "type": "string", "description": "System prompt (optional)" }
                                },
                                "required": ["model", "prompt"]
                            }),
                        },
                        Tool {
                            name: "local_llm_list_models".to_string(),
                            description: "List available local models".to_string(),
                            input_schema: json!({ "type": "object", "properties": {} }),
                        }
                    ]
                })),
                error: None,
            })
        }
        "tools/call" => {
            if !*initialized {
                return Ok(error_response(id, -32002, "Server not initialized"));
            }
            let params = request.params.unwrap_or(json!({}));
            let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

            let result = match tool_name {
                "local_llm_query" => call_local_llm(arguments)?,
                "local_llm_list_models" => list_local_models()?,
                _ => {
                    return Ok(error_response(
                        id,
                        -32601,
                        &format!("Unknown tool: {}", tool_name),
                    ))
                }
            };

            Ok(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: Some(json!({
                    "content": [{ "type": "text", "text": result }],
                    "isError": false
                })),
                error: None,
            })
        }
        _ => Ok(error_response(
            id,
            -32601,
            &format!("Method not found: {}", request.method),
        )),
    }
}

fn error_response(id: Option<Value>, code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        }),
    }
}

fn call_local_llm(args: Value) -> Result<String, Box<dyn std::error::Error>> {
    let model = args
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("model required")?;
    let prompt = args
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or("prompt required")?;
    let system = args.get("system").and_then(|v| v.as_str()).unwrap_or("");

    let mut cmd = Command::new("ollama");
    cmd.args(["run", model]);
    if !system.is_empty() {
        cmd.env("OLLAMA_SYSTEM", system);
    }
    cmd.arg(prompt);

    let output = cmd.output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(format!("ollama error: {}", String::from_utf8_lossy(&output.stderr)).into())
    }
}

fn list_local_models() -> Result<String, Box<dyn std::error::Error>> {
    let output = Command::new("ollama").args(["list"]).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(format!(
            "ollama list error: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into())
    }
}
