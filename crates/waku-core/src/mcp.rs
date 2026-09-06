//! Session-scoped MCP transport. Stdout contains newline-delimited JSON-RPC only.
use crate::model::{ProviderKind, RuntimeMode};
use crate::protocol::{
    ClientMessage, Command, MAX_WIRE_MESSAGE_BYTES, PROTOCOL_VERSION, Request, ResponseOutcome,
    ResponsePayload, ServerMessage,
};
use anyhow::{Context as _, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use tungstenite::{Message, WebSocket};
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnArguments {
    provider: ProviderKind,
    prompt: String,
    model: Option<String>,
    title: Option<String>,
    runtime_mode: Option<RuntimeMode>,
    idempotency_key: Option<String>,
    #[serde(default)]
    workspace: crate::protocol::CreationWorkspace,
}

pub fn run_stdio(
    input: impl BufRead,
    mut output: impl Write,
    address: &str,
    token: &str,
    session_id: Uuid,
    runtime_id: Uuid,
) -> anyhow::Result<()> {
    let stream = TcpStream::connect(address).context("could not connect to steward daemon")?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(90)))?;
    let (mut socket, _) = tungstenite::client(format!("ws://{address}/v1"), stream)?;
    send(
        &mut socket,
        &ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            token: token.into(),
            client_id: Uuid::new_v4(),
            resume_from: vec![],
        },
    )?;
    if !matches!(read(&mut socket)?, ServerMessage::Hello { .. }) {
        bail!("steward authentication failed");
    }
    let mut initialized = false;
    let mut input = input;
    loop {
        let mut line = Vec::new();
        let n = input
            .by_ref()
            .take((MAX_WIRE_MESSAGE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if n > MAX_WIRE_MESSAGE_BYTES {
            bail!("MCP input exceeds message limit");
        }
        let message: Value = match serde_json::from_slice(&line) {
            Ok(message) => message,
            Err(_) => {
                writeln!(
                    output,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})
                )?;
                output.flush()?;
                continue;
            }
        };
        let valid =
            message.is_object() && message["jsonrpc"] == "2.0" && message["method"].is_string();
        if valid && message.get("id").is_none() {
            continue;
        }
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or("");
        let result = if !valid || !(id.is_string() || id.is_i64() || id.is_u64() || id.is_null()) {
            Err((-32600, "Invalid Request".to_owned()))
        } else if method == "initialize" {
            initialized = true;
            Ok(
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"waku","version":env!("CARGO_PKG_VERSION")}}),
            )
        } else if !initialized {
            Err((-32000, "Initialize the MCP session first".into()))
        } else if method == "ping" {
            Ok(json!({}))
        } else if method == "tools/list" {
            Ok(json!({
                "tools": [
                    {
                        "name": "waku_spawn_session",
                        "description": "Create a direct Claude or Codex child. workspace defaults to worktree; inherit uses the parent directory and local uses the project checkout. Reuse idempotency_key with the same arguments to recover the same result after disconnect or restart. Without a key retries are not deduplicated; never automatically resend an uncertain creation.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "idempotency_key": {
                                    "type": "string",
                                    "minLength": 1
                                },
                                "workspace": {
                                    "type": "string",
                                    "enum": ["worktree", "inherit", "local"],
                                    "default": "worktree"
                                },
                                "provider": {
                                    "type": "string",
                                    "enum": [
                                        "claude",
                                        "codex"
                                    ]
                                },
                                "prompt": {
                                    "type": "string",
                                    "minLength": 1
                                },
                                "model": {
                                    "type": "string"
                                },
                                "title": {
                                    "type": "string"
                                },
                                "runtime_mode": {
                                    "type": "string",
                                    "enum": [
                                        "ask",
                                        "autoAcceptEdits",
                                        "auto",
                                        "fullAccess"
                                    ]
                                }
                            },
                            "required": [
                                "provider",
                                "prompt"
                            ],
                            "additionalProperties": false
                        }
                    },
                    {
                        "name": "waku_list_sessions",
                        "description": "List summaries of direct child sessions.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {},
                            "additionalProperties": false
                        }
                    },
                    {
                        "name": "waku_status",
                        "description": "Read direct children session and current/latest turn states; optionally wait for a change or timeout. Timeout is not failure.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "session_ids": {
                                    "type": "array",
                                    "items": {
                                        "type": "string",
                                        "format": "uuid"
                                    },
                                    "minItems": 1,
                                    "maxItems": 128
                                },
                                "wait_ms": {
                                    "type": "integer",
                                    "minimum": 0,
                                    "maximum": 60000,
                                    "default": 0
                                }
                            },
                            "required": [
                                "session_ids"
                            ],
                            "additionalProperties": false
                        }
                    },
                    {
                        "name": "waku_result",
                        "description": "Read current/latest turn reply in native text order. Turn status reports running/completed/failed/interrupted; null means no turn. Normal completion does not verify task correctness. Optional transcript includes prior turns. Text limits count Unicode characters independently for reply and transcript and report truncation.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "session_id": {
                                    "type": "string",
                                    "format": "uuid"
                                },
                                "include_transcript": {
                                    "type": "boolean",
                                    "default": false
                                },
                                "max_chars": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": 100000,
                                    "default": 20000
                                }
                            },
                            "required": [
                                "session_id"
                            ],
                            "additionalProperties": false
                        }
                    }
                ]
            }))
        } else if method == "tools/call" {
            match tool_command(
                message["params"]["name"].as_str().unwrap_or(""),
                message["params"]
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            ) {
                Err(error) => Err((-32602, error)),
                Ok(command) => {
                    let request_id = Uuid::new_v4();
                    send(
                        &mut socket,
                        &ClientMessage::Request(Request {
                            request_id,
                            session_id,
                            runtime_id,
                            command,
                        }),
                    )?;
                    match read(&mut socket)? {
                        ServerMessage::Response {
                            request_id: returned,
                            outcome,
                        } if returned == request_id => {
                            let (text,failed) = match outcome {
                                ResponseOutcome::Ok { payload:ResponsePayload::SessionCreated {session,workspace_path,branch,..} } => (json!({"session_id":session.id,"workspace_path":workspace_path,"branch":branch}).to_string(),false),
                                ResponseOutcome::Ok { payload: failure @ ResponsePayload::SessionCreationFailed { .. } } => (serde_json::to_string(&failure)?, true),
                                ResponseOutcome::Ok { payload:ResponsePayload::ChildSessions {sessions} } => (json!({"sessions":sessions}).to_string(),false),
                                ResponseOutcome::Ok { payload:ResponsePayload::ChildStatus {sessions,timed_out} } => (json!({"sessions":sessions,"timed_out":timed_out}).to_string(),false),
                                ResponseOutcome::Ok { payload:ResponsePayload::ChildResult {session,reply,reply_truncated,transcript,transcript_truncated} } => (json!({"session":session,"reply":reply,"reply_truncated":reply_truncated,"transcript":transcript,"transcript_truncated":transcript_truncated}).to_string(),false),
                                ResponseOutcome::Error {error} => (error.message,true),
                                _ => bail!("unexpected steward response"),
                            };
                            Ok(json!({"content":[{"type":"text","text":text}],"isError":failed}))
                        }
                        _ => bail!(
                            "unexpected steward response; check session state before retrying"
                        ),
                    }
                }
            }
        } else {
            Err((-32601, "Method not found".into()))
        };
        let response = match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err((code, message)) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
            }
        };
        writeln!(output, "{response}")?;
        output.flush()?;
    }
    let _ = socket.close(None);
    Ok(())
}
fn send(socket: &mut WebSocket<TcpStream>, message: &ClientMessage) -> anyhow::Result<()> {
    socket.send(Message::Text(serde_json::to_string(message)?.into()))?;
    Ok(())
}
fn read(socket: &mut WebSocket<TcpStream>) -> anyhow::Result<ServerMessage> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) => socket.flush()?,
            Message::Close(_) => {
                bail!("steward connection closed; check session state before retrying")
            }
            _ => {}
        }
    }
}

fn tool_command(name: &str, arguments: Value) -> Result<Command, String> {
    if name == "waku_spawn_session" {
        let args: SpawnArguments = serde_json::from_value(arguments)
            .map_err(|error| format!("Invalid tool arguments: {error}"))?;
        Ok(Command::CreateSession {
            provider: args.provider,
            prompt: args.prompt,
            model: args.model,
            title: args.title,
            runtime_mode: args.runtime_mode,
            idempotency_key: args.idempotency_key,
            workspace: args.workspace,
        })
    } else if name == "waku_list_sessions" || name == "waku_status" || name == "waku_result" {
        let mut arguments = arguments
            .as_object()
            .cloned()
            .ok_or("Tool arguments must be an object")?;
        let (kind, allowed): (&str, &[&str]) = if name == "waku_list_sessions" {
            ("listSessions", &[])
        } else if name == "waku_status" {
            ("status", &["session_ids", "wait_ms"])
        } else {
            ("result", &["session_id", "include_transcript", "max_chars"])
        };
        if arguments.keys().any(|key| !allowed.contains(&key.as_str())) {
            return Err("Unknown tool argument".into());
        }
        arguments.insert("type".into(), Value::String(kind.into()));
        let query = serde_json::from_value(Value::Object(arguments))
            .map_err(|error| format!("Invalid tool arguments: {error}"))?;
        Ok(Command::StewardQuery { query })
    } else {
        Err("Unknown tool".into())
    }
}
