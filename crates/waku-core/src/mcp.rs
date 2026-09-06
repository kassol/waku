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
    #[serde(default)]
    dependencies: Vec<crate::model::WorkspaceDependency>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptArguments {
    #[serde(default)]
    delivery_id: Option<Uuid>,
    session_id: Uuid,
    prompt: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelArguments {
    session_id: Uuid,
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
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"waku","version":env!("CARGO_PKG_VERSION")},"instructions":"After delegating, do independent work if available. When only child work remains, call waku_wait. If waiting=true, end your current turn immediately; do not poll or keep calling tools. Waku will automatically resume you in a new turn when a watched child finishes or needs user attention. If waiting=false, handle the returned states now. Never approve permissions or answer questions on the user's behalf."}),
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
                                "dependencies": {
                                    "type":"array", "items":{"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"commit":{"type":"string"}},"required":["session_id","commit"],"additionalProperties":false}
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
                        "name": "waku_prompt",
                        "description": "Deliver input to a direct child: new turn when idle, native steer when working, persistent FIFO queue only when the active provider explicitly lacks steering support. Provide a stable delivery_id to recover the same input after disconnect. Same id with different text is rejected. Query waku_prompt_status for confirmation; uncertain input must never be resent with a new id. Approvals and questions remain for the user.",
                        "inputSchema": {"type":"object", "properties":{"session_id":{"type":"string","format":"uuid"},"prompt":{"type":"string","minLength":1},"delivery_id":{"type":"string","format":"uuid"}}, "required":["session_id","prompt"],"additionalProperties":false}
                    },
                    {
                        "name": "waku_prompt_status",
                        "description": "Read one durable input delivery for a direct child. Received means provider or transport acknowledgment, never model adoption. Do not resend uncertain input.",
                        "inputSchema": {"type":"object","properties":{"session_id":{"type":"string","format":"uuid"},"delivery_id":{"type":"string","format":"uuid"}},"required":["session_id","delivery_id"],"additionalProperties":false}
                    },
                    {
                        "name": "waku_cancel",
                        "description": "Request cancellation of the direct child's current turn. Repeatable. accepted means requested; stopped means the provider has ended the turn. Preserves history and files.",
                        "inputSchema": {"type":"object", "properties":{"session_id":{"type":"string","format":"uuid"}},"required":["session_id"],"additionalProperties":false}
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
                        "name": "waku_workspace",
                        "description": "Inspect this task or a direct child. Integrate an explicitly accepted full child commit into the daemon integration branch, with checks, environment and reviewer evidence bound to that commit. Deliver the accepted combined commit to the task's originally selected local target after checking its version and active users. Integration and delivery do not push or deploy. Repeating a confirmed fixed result reuses its record. On conflict or a moved target, inspect the retained state before retrying. Task workspaces are explicitly created by the user before execution.",
                        "inputSchema": {"type":"object","properties":{"operation":{
                            "oneOf":[
                                {"type":"object","properties":{"type":{"const":"inspect"},"sessionId":{"type":"string","format":"uuid"}},"required":["type","sessionId"],"additionalProperties":false},
                                {"type":"object","properties":{"type":{"const":"integrate"},"sessionId":{"type":"string","format":"uuid"},"commit":{"type":"string"},"expectedIntegrationCommit":{"type":"string"},"evidence":{"$ref":"#/$defs/evidence"}},"required":["type","sessionId","commit","expectedIntegrationCommit","evidence"],"additionalProperties":false},
                                {"type":"object","properties":{"type":{"const":"deliver"},"commit":{"type":"string"},"expectedTargetCommit":{"type":"string"},"evidence":{"$ref":"#/$defs/evidence"}},"required":["type","commit","expectedTargetCommit","evidence"],"additionalProperties":false}
                            ]
                        }},"required":["operation"],"additionalProperties":false,"$defs":{"evidence":{"type":"array","minItems":1,"items":{"type":"object","properties":{"commit":{"type":"string"},"checks":{"type":"string","minLength":1},"environment":{"type":"string","minLength":1},"reviewer":{"type":"string","minLength":1}},"required":["commit","checks","environment","reviewer"],"additionalProperties":false}}}}
                    },
                    {
                        "name": "waku_wait",
                        "description": "Persist a one-shot wait for the current turns of direct children. If waiting=true, finish your current turn now and stop polling; Waku automatically starts a follow-up turn when any watched child finishes, fails, is interrupted, or needs user input. If waiting=false, a child is already actionable: read its result now. New user input or cancellation revokes the wait. Repeating the same targets in this turn is safe.",
                        "inputSchema": {"type":"object","properties":{"session_ids":{"type":"array","items":{"type":"string","format":"uuid"},"minItems":1,"maxItems":128}},"required":["session_ids"],"additionalProperties":false}
                    },
                    {
                        "name": "waku_status",
                        "description": "Read direct children session and current/latest turn states. For passive waiting use waku_wait and end your turn instead of polling. Optional bounded wait is for compatibility; timeout is not failure.",
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
                    let workspace_operation = match &command {
                        Command::StewardWorkspace { operation } => Some(operation.clone()),
                        _ => None,
                    };
                    let request_id = Uuid::new_v4();
                    send(
                        &mut socket,
                        &ClientMessage::Request(Request {
                            request_id,
                            session_id,
                            runtime_id,
                            command,
                        }),
                    ).context("steward submission state is uncertain; query waku_status before retrying; do not automatically resend")?;
                    match read(&mut socket).context("steward submission state is uncertain; query waku_status before retrying; do not automatically resend")? {
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
                                ResponseOutcome::Ok { payload:ResponsePayload::ChildPromptAccepted {turn_id,delivery} } => (json!({"turn_id":turn_id,"delivery":delivery}).to_string(),false),
                                ResponseOutcome::Ok { payload:ResponsePayload::ChildInputStatus {delivery} } => (json!({"delivery":delivery}).to_string(),false),
                                ResponseOutcome::Ok { payload:ResponsePayload::ChildCancel {session,accepted,stopped} } => (json!({"session":session,"accepted":accepted,"stopped":stopped}).to_string(),false),
                                ResponseOutcome::Ok { payload:ResponsePayload::TaskWorkspace {session} } => {
                                    let failed = session.managed_workspace.as_ref().is_some_and(|workspace| {
                                        use crate::model::StewardWorkspaceOperation as Operation;
                                        match &workspace_operation {
                                            Some(Operation::Inspect { .. }) => false,
                                            Some(Operation::Integrate { commit, .. }) => !workspace.results.iter().any(|result| &result.commit == commit && result.integration_commit.is_some()),
                                            Some(Operation::Deliver { commit, .. }) => !workspace.deliveries.iter().any(|delivery| &delivery.commit == commit && delivery.completed),
                                            _ => workspace.error.is_some(),
                                        }
                                    });
                                    (json!({"session":session}).to_string(),failed)
                                },
                                ResponseOutcome::Ok { payload:ResponsePayload::StewardWait {wait,sessions} } => (json!({"waiting":wait.is_some(),"wait":wait,"sessions":sessions,"next_action":"If waiting=true, end this turn now. A child event will automatically resume you; do not poll. If waiting=false, handle the actionable child states now."}).to_string(),false),
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
    if name == "waku_prompt" {
        let args: PromptArguments = serde_json::from_value(arguments)
            .map_err(|error| format!("Invalid tool arguments: {error}"))?;
        if args.prompt.trim().is_empty() {
            return Err("prompt must not be empty".into());
        }
        Ok(Command::StewardPrompt {
            child_session_id: args.session_id,
            prompt: args.prompt,
            delivery_id: args.delivery_id,
        })
    } else if name == "waku_prompt_status" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments { session_id: Uuid, delivery_id: Uuid }
        let args: Arguments = serde_json::from_value(arguments).map_err(|e| format!("Invalid tool arguments: {e}"))?;
        Ok(Command::StewardInputStatus { child_session_id: args.session_id, delivery_id: args.delivery_id })
    } else if name == "waku_workspace" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WorkspaceArguments { operation: crate::model::StewardWorkspaceOperation }
        let args: WorkspaceArguments = serde_json::from_value(arguments)
            .map_err(|error| format!("Invalid tool arguments: {error}"))?;
        Ok(Command::StewardWorkspace { operation: args.operation })
    } else if name == "waku_wait" {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WaitArguments { session_ids: Vec<Uuid> }
        let args: WaitArguments = serde_json::from_value(arguments)
            .map_err(|error| format!("Invalid tool arguments: {error}"))?;
        if args.session_ids.is_empty() || args.session_ids.len() > 128 {
            return Err("session_ids must contain 1..128 direct children".into());
        }
        Ok(Command::StewardWait { session_ids: args.session_ids })
    } else if name == "waku_cancel" {
        let args: CancelArguments = serde_json::from_value(arguments)
            .map_err(|error| format!("Invalid tool arguments: {error}"))?;
        Ok(Command::StewardCancel {
            child_session_id: args.session_id,
        })
    } else if name == "waku_spawn_session" {
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
            dependencies: args.dependencies,
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
