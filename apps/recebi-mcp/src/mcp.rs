use std::io::{self, BufRead, Write};

use recebi_core::limits::MAX_TOOL_RESULT_BYTES;
use serde_json::{Value, json};
use thiserror::Error;

use crate::health::HealthService;
use crate::receivable::{CreateRequestInput, ReceivableService};
use crate::{
    close_month::{CloseMonthInput, CloseMonthService, SnapshotMonthInput},
    ptax::HttpBcbPtax,
    reconcile::{CheckInput, ReconcileOpenInput, ReconciliationService},
    rpc::HttpSolanaRpc,
};

const MAX_MCP_INPUT_BYTES: usize = 16 * 1024;
const PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Debug, Error)]
pub enum McpError {
    #[error("stdio is unavailable")]
    Stdio,
}

pub fn serve(
    health: &HealthService,
    receivables: &ReceivableService,
    reconciliation: &ReconciliationService<HttpSolanaRpc>,
    closing: &CloseMonthService<HttpBcbPtax>,
) -> Result<(), McpError> {
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
                Ok(request) => dispatch(health, receivables, reconciliation, closing, &request),
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

fn dispatch(
    health: &HealthService,
    receivables: &ReceivableService,
    reconciliation: &ReconciliationService<HttpSolanaRpc>,
    closing: &CloseMonthService<HttpBcbPtax>,
    request: &Value,
) -> Value {
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
        "tools/list" => success_response(
            &id,
            &json!({"tools": [
                health_tool_schema(),
                create_request_tool_schema(),
                check_tool_schema(),
                reconcile_open_tool_schema(),
                snapshot_month_tool_schema(),
                close_month_tool_schema()
            ]}),
        ),
        "tools/call" => call_tool(
            health,
            receivables,
            reconciliation,
            closing,
            &id,
            object.get("params"),
        ),
        _ => error_response(&id, -32601, "method_not_found"),
    }
}

fn call_tool(
    health: &HealthService,
    receivables: &ReceivableService,
    reconciliation: &ReconciliationService<HttpSolanaRpc>,
    closing: &CloseMonthService<HttpBcbPtax>,
    id: &Value,
    params: Option<&Value>,
) -> Value {
    let Some(params) = params.and_then(Value::as_object) else {
        return error_response(id, -32602, "invalid_tool_request");
    };
    match params.get("name").and_then(Value::as_str) {
        Some("recebi_health") => call_health(health, id, params.get("arguments")),
        Some("recebi_create_request") => {
            call_create_request(receivables, id, params.get("arguments"))
        }
        Some("recebi_check") => call_check(reconciliation, id, params.get("arguments")),
        Some("recebi_reconcile_open") => {
            call_reconcile_open(reconciliation, id, params.get("arguments"))
        }
        Some("recebi_close_month") => call_close_month(closing, id, params.get("arguments")),
        Some("recebi_snapshot_month") => call_snapshot_month(closing, id, params.get("arguments")),
        _ => error_response(id, -32602, "unknown_tool"),
    }
}

fn call_snapshot_month(
    closing: &CloseMonthService<HttpBcbPtax>,
    id: &Value,
    arguments: Option<&Value>,
) -> Value {
    let Some(arguments) = arguments else {
        return error_response(id, -32602, "invalid_snapshot_month_arguments");
    };
    let Ok(input) = serde_json::from_value::<SnapshotMonthInput>(arguments.clone()) else {
        return error_response(id, -32602, "invalid_snapshot_month_arguments");
    };
    tool_result(id, closing.snapshot(&input))
}

fn call_close_month(
    closing: &CloseMonthService<HttpBcbPtax>,
    id: &Value,
    arguments: Option<&Value>,
) -> Value {
    let Some(arguments) = arguments else {
        return error_response(id, -32602, "invalid_close_month_arguments");
    };
    let Ok(input) = serde_json::from_value::<CloseMonthInput>(arguments.clone()) else {
        return error_response(id, -32602, "invalid_close_month_arguments");
    };
    tool_result(id, closing.close(&input))
}

fn call_check(
    reconciliation: &ReconciliationService<HttpSolanaRpc>,
    id: &Value,
    arguments: Option<&Value>,
) -> Value {
    let Some(arguments) = arguments else {
        return error_response(id, -32602, "invalid_check_arguments");
    };
    let Ok(input) = serde_json::from_value::<CheckInput>(arguments.clone()) else {
        return error_response(id, -32602, "invalid_check_arguments");
    };
    tool_result(id, reconciliation.check(input))
}

fn call_reconcile_open(
    reconciliation: &ReconciliationService<HttpSolanaRpc>,
    id: &Value,
    arguments: Option<&Value>,
) -> Value {
    let arguments = arguments.cloned().unwrap_or_else(|| json!({}));
    let Ok(input) = serde_json::from_value::<ReconcileOpenInput>(arguments) else {
        return error_response(id, -32602, "invalid_reconcile_open_arguments");
    };
    tool_result(id, reconciliation.reconcile_open(input))
}

fn tool_result<T: serde::Serialize, E: std::fmt::Display>(
    id: &Value,
    result: Result<T, E>,
) -> Value {
    match result {
        Ok(result) => success_response(
            id,
            &json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(&result).expect("serializable tool result")
                }],
                "isError": false
            }),
        ),
        Err(error) => success_response(
            id,
            &json!({
                "content": [{
                    "type": "text",
                    "text": format!("{{\"status\":\"error\",\"reason\":\"{error}\"}}")
                }],
                "isError": true
            }),
        ),
    }
}

fn call_health(health: &HealthService, id: &Value, arguments: Option<&Value>) -> Value {
    if let Some(arguments) = arguments
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

fn call_create_request(
    receivables: &ReceivableService,
    id: &Value,
    arguments: Option<&Value>,
) -> Value {
    let Some(arguments) = arguments else {
        return error_response(id, -32602, "invalid_create_request_arguments");
    };
    let Ok(input) = serde_json::from_value::<CreateRequestInput>(arguments.clone()) else {
        return error_response(id, -32602, "invalid_create_request_arguments");
    };
    match receivables.create(input) {
        Ok(result) => success_response(
            id,
            &json!({"content": [{"type": "text", "text": serde_json::to_string(&result).expect("serializable create result")}], "isError": false}),
        ),
        Err(error) => success_response(
            id,
            &json!({"content": [{"type": "text", "text": format!("{{\"status\":\"error\",\"reason\":\"{}\"}}", error)}], "isError": true}),
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

fn create_request_tool_schema() -> Value {
    json!({
        "name": "recebi_create_request",
        "description": "Create or return a durable, reference-bound USDC receivable. It never signs, submits, or refunds a transaction.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "receivable_id": {"type": "string", "maxLength": 64},
                "amount": {"type": "string", "description": "Positive decimal USDC amount; no exponent notation."},
                "public_label": {"type": "string", "maxLength": 120, "description": "Public wallet-display label; do not include sensitive data."}
            },
            "required": ["receivable_id", "amount", "public_label"],
            "additionalProperties": false
        }
    })
}

fn close_month_tool_schema() -> Value {
    json!({
        "name": "recebi_close_month",
        "description": "Finalize a completed UTC month, attach bounded official same-day BCB PTAX evidence where available, and atomically publish deterministic accountant-ready JSON/CSV/manifest files. The active or a future month is rejected.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "month": {
                    "type": "string",
                    "pattern": "^[0-9]{4}-(0[1-9]|1[0-2])$",
                    "description": "UTC settlement month in YYYY-MM form."
                }
            },
            "required": ["month"],
            "additionalProperties": false
        }
    })
}

fn snapshot_month_tool_schema() -> Value {
    json!({
        "name": "recebi_snapshot_month",
        "description": "Create a provisional, hash-verified snapshot for the active or a completed UTC month without calling it a final close. A future month is rejected.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "month": {
                    "type": "string",
                    "pattern": "^[0-9]{4}-(0[1-9]|1[0-2])$",
                    "description": "UTC settlement month in YYYY-MM form."
                }
            },
            "required": ["month"],
            "additionalProperties": false
        }
    })
}

fn check_tool_schema() -> Value {
    json!({
        "name": "recebi_check",
        "description": "Locate and deterministically verify finalized Solana settlement for one receivable. It never signs or submits.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "receivable_id": {"type": "string", "maxLength": 64}
            },
            "required": ["receivable_id"],
            "additionalProperties": false
        }
    })
}

fn reconcile_open_tool_schema() -> Value {
    json!({
        "name": "recebi_reconcile_open",
        "description": "Reconcile a bounded batch of open receivables using finalized Solana reads.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "max_count": {"type": "integer", "minimum": 1, "maximum": 10}
            },
            "additionalProperties": false
        }
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

    use super::{
        close_month_tool_schema, create_request_tool_schema, dispatch, encode_response,
        health_tool_schema, snapshot_month_tool_schema,
    };
    use crate::{
        close_month::CloseMonthService, config::AppConfig, health::HealthService,
        ptax::HttpBcbPtax, receivable::ReceivableService, reconcile::ReconciliationService,
        rpc::HttpSolanaRpc,
    };

    fn services() -> (
        tempfile::TempDir,
        HealthService,
        ReceivableService,
        ReconciliationService<HttpSolanaRpc>,
        CloseMonthService<HttpBcbPtax>,
    ) {
        let directory = tempfile::tempdir().expect("temporary directory");
        let config_path = directory.path().join("recebi.toml");
        let config = format!(
            r#"
[recebi]
cluster = "devnet"
genesis_hash = "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG"
merchant_wallet = "11111111111111111111111111111111"
accepted_mint = "11111111111111111111111111111111"
token_decimals = 6
rpc_url = "https://api.devnet.solana.com"
data_dir = "{}"
ptax_policy = "strict_same_day"
max_open_reconcile = 10
"#,
            directory.path().join("data").display(),
        );
        std::fs::write(&config_path, config).expect("write config");
        let config = AppConfig::load(&config_path).expect("load config");
        let health = HealthService::new(config.clone());
        let receivables = ReceivableService::new(config.clone()).expect("receivables");
        let reconciliation = ReconciliationService::live(config.clone()).expect("reconciliation");
        let closing =
            CloseMonthService::new(&config, HttpBcbPtax::new().expect("PTAX")).expect("closing");
        (directory, health, receivables, reconciliation, closing)
    }

    #[test]
    fn only_bounded_non_custodial_tools_are_discoverable() {
        let health_schema = health_tool_schema();
        assert_eq!(health_schema["name"], "recebi_health");
        assert_eq!(health_schema["inputSchema"]["properties"], json!({}));
        assert_eq!(health_schema["inputSchema"]["additionalProperties"], false);
        let create_schema = create_request_tool_schema();
        assert_eq!(create_schema["name"], "recebi_create_request");
        let schema_keys = create_schema["inputSchema"]["properties"]
            .as_object()
            .expect("properties object")
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(",");
        for forbidden_surface in ["wallet", "private_key", "sign", "submit", "refund"] {
            assert!(!schema_keys.contains(forbidden_surface));
        }
        let close_schema = close_month_tool_schema();
        assert_eq!(close_schema["name"], "recebi_close_month");
        assert_eq!(close_schema["inputSchema"]["required"], json!(["month"]));
        let snapshot_schema = snapshot_month_tool_schema();
        assert_eq!(snapshot_schema["name"], "recebi_snapshot_month");
        assert_eq!(snapshot_schema["inputSchema"]["required"], json!(["month"]));
    }

    #[test]
    fn malformed_envelope_is_rejected() {
        let (_directory, health, receivables, reconciliation, closing) = services();
        let result = dispatch(
            &health,
            &receivables,
            &reconciliation,
            &closing,
            &json!({"jsonrpc": "2.0", "id": 1}),
        );
        assert_eq!(result["error"]["message"], "invalid_request");
    }

    #[test]
    fn health_rejects_configuration_override_arguments() {
        let (_directory, health, receivables, reconciliation, closing) = services();
        let result = dispatch(
            &health,
            &receivables,
            &reconciliation,
            &closing,
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
    fn create_rejects_memo_and_configuration_override_input() {
        let (_directory, health, receivables, reconciliation, closing) = services();
        let result = dispatch(
            &health,
            &receivables,
            &reconciliation,
            &closing,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "recebi_create_request", "arguments": {"receivable_id": "ACME-412", "amount": "0.1", "public_label": "ACME", "memo": "private", "rpc_url": "https://attacker.invalid"}},
            }),
        );
        assert_eq!(
            result["error"]["message"],
            "invalid_create_request_arguments"
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
