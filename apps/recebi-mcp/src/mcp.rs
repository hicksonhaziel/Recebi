use std::io::{self, BufRead, Write};

use recebi_core::limits::MAX_TOOL_RESULT_BYTES;
use serde_json::{Value, json};
use thiserror::Error;

use crate::health::HealthService;

const MAX_MCP_INPUT_BYTES: usize = 16 * 1024;
const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Error)]
pub enum McpError {
    #[error("stdio is unavailable")]
    Stdio,
}

pub fn serve(health: &HealthService) -> Result<(), McpError> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().split(b'\n') {
        let line = line.map_err(|_| McpError::Stdio)?;
        let response = if line.len() > MAX_MCP_INPUT_BYTES {
            Some(error_response(
                &Value::Null,
                -32600,
                "bounded_input_exceeded",
            ))
        } else if line.is_empty() {
            None
        } else {
            let request = serde_json::from_slice(&line);
            Some(match request {
                Ok(request) if is_notification(&request) => continue,
                Ok(request) => dispatch(health, &request),
                Err(_) => error_response(&Value::Null, -32700, "parse_error"),
            })
        };
        if let Some(response) = response {
            let encoded = encode_response(&response);
            stdout.write_all(&encoded).map_err(|_| McpError::Stdio)?;
            stdout.write_all(b"\n").map_err(|_| McpError::Stdio)?;
            stdout.flush().map_err(|_| McpError::Stdio)?;
        }
    }
    Ok(())
}

fn is_notification(request: &Value) -> bool {
    request
        .get("method")
        .and_then(Value::as_str)
        .is_some_and(|method| method.starts_with("notifications/"))
        && request.get("id").is_none()
}

fn dispatch(health: &HealthService, request: &Value) -> Value {
    let Some(object) = request.as_object() else {
        return error_response(&Value::Null, -32600, "invalid_request");
    };
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return error_response(&id, -32600, "invalid_request");
    };
    match method {
        "notifications/initialized" => Value::Null,
        "initialize" => success_response(
            &id,
            &json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "recebi-mcp", "version": env!("CARGO_PKG_VERSION")},
            }),
        ),
        "ping" => success_response(&id, &json!({})),
        "tools/list" => success_response(&id, &json!({"tools": [health_tool_schema()]})),
        "tools/call" => call_tool(health, &id, object.get("params")),
        _ => error_response(&id, -32601, "method_not_found"),
    }
}

fn call_tool(health: &HealthService, id: &Value, params: Option<&Value>) -> Value {
    let Some(params) = params.and_then(Value::as_object) else {
        return error_response(id, -32602, "invalid_tool_request");
    };
    if params.get("name").and_then(Value::as_str) != Some("recebi_health") {
        return error_response(id, -32602, "unknown_tool");
    }
    if let Some(arguments) = params.get("arguments")
        && arguments
            .as_object()
            .is_none_or(|arguments| !arguments.is_empty())
    {
        return error_response(id, -32602, "recebi_health_accepts_no_arguments");
    }
    match health.check() {
        Ok(result) => success_response(
            id,
            &json!({
                "content": [{"type": "text", "text": serde_json::to_string(&result).expect("serializable health result")}],
                "isError": false,
            }),
        ),
        Err(_) => success_response(
            id,
            &json!({
                "content": [{"type": "text", "text": "{\"status\":\"error\",\"reason\":\"health_check_failed\"}"}],
                "isError": true,
            }),
        ),
    }
}

fn health_tool_schema() -> Value {
    json!({
        "name": "recebi_health",
        "description": "Validate trusted local Recebi configuration and local data-directory availability. It has no financial capability.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
    })
}

fn success_response(id: &Value, result: &Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: &Value, code: i64, message: &'static str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn encode_response(response: &Value) -> Vec<u8> {
    let encoded = serde_json::to_vec(&response).expect("JSON values serialize");
    if encoded.len() <= MAX_TOOL_RESULT_BYTES {
        encoded
    } else {
        serde_json::to_vec(&error_response(
            &Value::Null,
            -32603,
            "bounded_output_exceeded",
        ))
        .expect("error response serializes")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{dispatch, encode_response, health_tool_schema};
    use crate::{config::AppConfig, health::HealthService};

    fn health() -> HealthService {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("recebi.toml");
        std::fs::write(
            &config_path,
            r#"
[recebi]
cluster = "devnet"
merchant_wallet = "11111111111111111111111111111111"
accepted_mint = "11111111111111111111111111111111"
token_decimals = 6
rpc_url = "https://api.devnet.solana.com"
data_dir = "."
ptax_policy = "strict_same_day"
max_open_reconcile = 10
"#,
        )
        .expect("write config");
        HealthService::new(AppConfig::load(&config_path).expect("load config"))
    }

    #[test]
    fn only_health_is_discoverable_and_it_has_no_input_surface() {
        let schema = health_tool_schema();
        assert_eq!(schema["name"], "recebi_health");
        assert_eq!(schema["inputSchema"]["properties"], json!({}));
        assert_eq!(schema["inputSchema"]["additionalProperties"], false);
        let schema_text = schema.to_string();
        for forbidden_surface in ["wallet", "private_key", "sign", "submit", "refund"] {
            assert!(!schema_text.contains(forbidden_surface));
        }
    }

    #[test]
    fn malformed_envelope_is_rejected() {
        let result = dispatch(&health(), &json!({"jsonrpc": "2.0", "id": 1}));
        assert_eq!(result["error"]["message"], "invalid_request");
    }

    #[test]
    fn health_rejects_configuration_override_arguments() {
        let result = dispatch(
            &health(),
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "recebi_health", "arguments": {"rpc_url": "https://attacker.invalid"}},
            }),
        );
        assert_eq!(
            result["error"]["message"],
            "recebi_health_accepts_no_arguments"
        );
    }

    #[test]
    fn oversized_response_is_replaced_with_bounded_error() {
        let encoded = encode_response(&json!({"payload": "x".repeat(5_000)}));
        assert!(encoded.len() < 512);
        assert!(
            String::from_utf8(encoded)
                .expect("UTF-8")
                .contains("bounded_output_exceeded")
        );
    }
}
