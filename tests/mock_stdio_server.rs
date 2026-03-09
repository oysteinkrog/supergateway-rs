//! Mock MCP stdio server for compatibility testing.
//!
//! Speaks JSON-RPC 2.0 over stdin/stdout. Responds to:
//! - `initialize` → server capabilities
//! - `tools/list` → tool list with "echo" tool
//! - `tools/call` with name="echo" → echoes arguments back
//! - `echo` → echoes params back (legacy)
//! - notifications (no id) → ignored
//!
//! Flags:
//!   --notify    Send periodic server-initiated notifications (every 500ms)
//!
//! Exits cleanly on stdin EOF or SIGTERM.

use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

static RUNNING: AtomicBool = AtomicBool::new(true);

fn main() {
    let notify = std::env::args().any(|a| a == "--notify");

    // If --notify, spawn a thread that writes periodic notifications to stdout.
    let stdout_lock = Arc::new(std::sync::Mutex::new(io::stdout()));
    if notify {
        let stdout_lock = stdout_lock.clone();
        std::thread::spawn(move || {
            let mut seq = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if !RUNNING.load(Ordering::Relaxed) {
                    break;
                }
                seq += 1;
                let notification = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/progress",
                    "params": {
                        "progressToken": "mock-progress",
                        "progress": seq,
                        "total": 100
                    }
                });
                let msg = serde_json::to_string(&notification).unwrap();
                let mut out = stdout_lock.lock().unwrap();
                if writeln!(out, "{msg}").is_err() {
                    break;
                }
                if out.flush().is_err() {
                    break;
                }
            }
        });
    }

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // stdin closed
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32700, "message": format!("Parse error: {e}")},
                    "id": null
                });
                let mut out = stdout_lock.lock().unwrap();
                let _ = writeln!(out, "{}", serde_json::to_string(&err_resp).unwrap());
                let _ = out.flush();
                continue;
            }
        };

        // Notifications have no id — don't respond.
        if req.get("id").is_none() {
            continue;
        }

        let id = req["id"].clone();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        let response = match method {
            "initialize" => {
                json!({
                    "jsonrpc": "2.0",
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {
                            "tools": { "listChanged": true }
                        },
                        "serverInfo": {
                            "name": "mock-mcp-server",
                            "version": "1.0.0"
                        }
                    },
                    "id": id
                })
            }
            "tools/list" => {
                json!({
                    "jsonrpc": "2.0",
                    "result": {
                        "tools": [{
                            "name": "echo",
                            "description": "Echoes input back",
                            "inputSchema": {
                                "type": "object",
                                "properties": {
                                    "message": { "type": "string" }
                                }
                            }
                        }]
                    },
                    "id": id
                })
            }
            "tools/call" => {
                let params = req.get("params").cloned().unwrap_or(json!({}));
                let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if tool_name == "echo" {
                    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                    json!({
                        "jsonrpc": "2.0",
                        "result": {
                            "content": [{
                                "type": "text",
                                "text": serde_json::to_string(&arguments).unwrap()
                            }]
                        },
                        "id": id
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "error": {"code": -32601, "message": format!("Unknown tool: {tool_name}")},
                        "id": id
                    })
                }
            }
            "echo" => {
                let params = req.get("params").cloned().unwrap_or(json!(null));
                json!({
                    "jsonrpc": "2.0",
                    "result": params,
                    "id": id
                })
            }
            _ => {
                json!({
                    "jsonrpc": "2.0",
                    "error": {"code": -32601, "message": format!("Method not found: {method}")},
                    "id": id
                })
            }
        };

        let msg = serde_json::to_string(&response).unwrap();
        let mut out = stdout_lock.lock().unwrap();
        let _ = writeln!(out, "{msg}");
        let _ = out.flush();
    }

    RUNNING.store(false, Ordering::Relaxed);
}
