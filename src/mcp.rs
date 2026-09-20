//! `hedron mcp`: Model Context Protocol server over stdio.
//!
//! One named-ask tool (`jev_intent`) mapped onto the same function the CLI
//! calls. JSON-RPC 2.0, one message per line. No HTTP transport.

use std::io::{self, BufRead, Write};

use serde_json::{json, Map, Value};

use crate::jev::{self, AskArgs, AskFormat, Transport};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub const MCP_HELP: &str = "\
hedron mcp: Model Context Protocol server over stdio (JSON-RPC 2.0, one message per line).
Tool: jev_intent — named-ask Choice {apply, wait, escalate, ignore} on {intent, evidence_digest}.
Shadow: Jev never writes store rows. See docs/jev-native.md.
";

pub const TOOLS: &[&str] = &["jev_intent"];

/// Serves MCP over the process's stdin/stdout until EOF.
pub fn run_cli(raw: Vec<String>) -> std::result::Result<(), String> {
    if raw.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{MCP_HELP}");
        return Ok(());
    }
    if !raw.is_empty() {
        return Err("mcp takes no arguments".into());
    }
    let stdin = io::stdin().lock();
    let stdout = io::stdout().lock();
    serve(stdin, stdout).map_err(|err| format!("mcp io: {err}"))
}

pub fn serve<R: BufRead, W: Write>(input: R, mut output: W) -> io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(error) => {
                write_message(
                    &mut output,
                    &rpc_error(Value::Null, -32700, format!("parse error: {error}")),
                )?;
                continue;
            }
        };
        if let Some(response) = handle(&message) {
            write_message(&mut output, &response)?;
        }
    }
    Ok(())
}

fn write_message<W: Write>(output: &mut W, message: &Value) -> io::Result<()> {
    let mut text = serde_json::to_string(message).expect("JSON value serialization cannot fail");
    text.push('\n');
    output.write_all(text.as_bytes())?;
    output.flush()
}

pub fn handle(message: &Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    if method.starts_with("notifications/") {
        return None;
    }
    let Some(id) = id else {
        return None;
    };
    Some(match method {
        "initialize" => rpc_result(id, initialize_result(&params)),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tool_descriptions() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let arguments = params
                .get("arguments")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            if !TOOLS.contains(&name) {
                return Some(rpc_error(id, -32602, format!("unknown tool: {name}")));
            }
            let (document, is_error) = call_jev_intent(&arguments);
            rpc_result(
                id,
                json!({
                    "content": [{ "type": "text", "text": pretty(&document) }],
                    "structuredContent": document,
                    "isError": is_error,
                }),
            )
        }
        other => rpc_error(id, -32601, format!("method not found: {other}")),
    })
}

fn initialize_result(params: &Value) -> Value {
    let protocol = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "hedrondb", "version": env!("CARGO_PKG_VERSION") },
        "instructions": concat!(
            "HedronDB: local-first named intent store. ",
            "jev_intent is a shadow Jev gate (Facet TypeSafe recipe) over ",
            "{intent, evidence_digest}. Low confidence → escalate; never auto-apply. ",
            "Named asks only; do not schedule grind clocks. ",
            "Hydrate typesafeApiKey via facet env set --secret (TYPESAFE_API_KEY), never fixtures. ",
            "empty/ask/escalate/hold ≠ approve."
        ),
    })
}

fn tool_descriptions() -> Value {
    json!([{
        "name": "jev_intent",
        "description": "Named-ask Jev gate: Choice {apply, wait, escalate, ignore} on {intent, evidence_digest} plus confidence. Shadow: never writes HedronDB rows. Low confidence escalates.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "intent": { "type": "string", "description": "Intent text. Mutually exclusive with name." },
                "evidence_digest": { "type": "string", "description": "Evidence digest (blake3 / tip SHA). Not a raw body or key." },
                "name": { "type": "string", "description": "Named desired state to load read-only (requires db)." },
                "db": { "type": "string", "description": "HedronDB sqlite file (read-only)." },
                "vault": { "type": "string", "description": "Vault that holds name." },
                "evidence": { "type": "string", "description": "Optional file to blake3-hash as the digest." }
            }
        }
    }])
}

fn call_jev_intent(arguments: &Map<String, Value>) -> (Value, bool) {
    let args = AskArgs {
        intent: string_arg(arguments, "intent"),
        name: string_arg(arguments, "name"),
        db: string_arg(arguments, "db").map(std::path::PathBuf::from),
        vault: string_arg(arguments, "vault"),
        evidence_digest: string_arg(arguments, "evidence_digest")
            .or_else(|| string_arg(arguments, "evidenceDigest")),
        evidence: string_arg(arguments, "evidence").map(std::path::PathBuf::from),
        format: AskFormat::Json,
    };
    match jev::resolve_ask(&args)
        .and_then(|(intent, digest)| jev::ask(&intent, &digest, Transport::resolve()))
    {
        Ok(decision) => (
            serde_json::to_value(decision).expect("decision json"),
            false,
        ),
        Err(err) => (
            json!({ "error": err.to_string(), "applied": false, "shadow": true }),
            true,
        ),
    }
}

fn string_arg(arguments: &Map<String, Value>, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("JSON value serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_and_tools_list_name_the_gate() {
        let init = handle(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }))
        .unwrap();
        let text = init.to_string();
        assert!(text.contains("jev_intent"));
        assert!(text.contains("named"));
        assert!(!text.to_ascii_lowercase().contains("duha"));
        assert!(!text.to_ascii_lowercase().contains("asr"));
        assert!(!text.to_ascii_lowercase().contains("maghrib"));
        assert!(!text.to_ascii_lowercase().contains("cron"));

        let list = handle(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }))
        .unwrap();
        assert!(list["result"]["tools"][0]["name"] == "jev_intent");
        let schema = list["result"]["tools"][0]["inputSchema"].to_string();
        assert!(schema.contains("intent"));
        assert!(schema.contains("evidence_digest"));
    }

    #[test]
    fn tools_call_without_transport_escalates() {
        let old = std::env::var_os("HEDRON_JEV_TRANSPORT");
        unsafe { std::env::set_var("HEDRON_JEV_TRANSPORT", "none") };
        let reply = handle(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "jev_intent",
                "arguments": {
                    "intent": "replicas=2",
                    "evidence_digest": "abc123"
                }
            }
        }))
        .unwrap();
        match old {
            Some(v) => unsafe { std::env::set_var("HEDRON_JEV_TRANSPORT", v) },
            None => unsafe { std::env::remove_var("HEDRON_JEV_TRANSPORT") },
        }
        let doc = &reply["result"]["structuredContent"];
        assert_eq!(doc["choice"], "escalate");
        assert_eq!(doc["applied"], false);
        assert_eq!(doc["shadow"], true);
        assert_eq!(reply["result"]["isError"], false);
    }
}
