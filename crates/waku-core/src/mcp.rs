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
            Ok(
                json!({"tools":[{"name":"waku_spawn_session","description":"Create a direct Codex child in a new Git worktree. No automatic retry after a lost response.","inputSchema":{"type":"object","properties":{"provider":{"type":"string","enum":["codex"]},"prompt":{"type":"string","minLength":1},"model":{"type":"string"},"title":{"type":"string"},"runtime_mode":{"type":"string","enum":["ask","autoAcceptEdits","auto","fullAccess"]}},"required":["provider","prompt"],"additionalProperties":false}}]}),
            )
        } else if method == "tools/call" {
            if message["params"]["name"] != "waku_spawn_session" {
                Err((-32602, "Unknown tool".into()))
            } else {
                match serde_json::from_value::<SpawnArguments>(
                    message["params"]["arguments"].clone(),
                ) {
                    Err(error) => Err((-32602, format!("Invalid tool arguments: {error}"))),
                    Ok(args) => {
                        let request_id = Uuid::new_v4();
                        send(
                            &mut socket,
                            &ClientMessage::Request(Request {
                                request_id,
                                session_id,
                                runtime_id,
                                command: Command::CreateSession {
                                    provider: args.provider,
                                    prompt: args.prompt,
                                    model: args.model,
                                    title: args.title,
                                    runtime_mode: args.runtime_mode,
                                },
                            }),
                        )?;
                        match read(&mut socket)? {
                            ServerMessage::Response {
                                request_id: returned,
                                outcome,
                            } if returned == request_id => {
                                let (text, failed) = match outcome {
                                    ResponseOutcome::Ok { payload: ResponsePayload::SessionCreated { session, workspace_path, branch, .. } } => (json!({"session_id":session.id,"workspace_path":workspace_path,"branch":branch}).to_string(), false),
                                    ResponseOutcome::Error { error } => (error.message, true),
                                    _ => bail!("unexpected steward response"),
                                };
                                Ok(
                                    json!({"content":[{"type":"text","text":text}],"isError":failed}),
                                )
                            }
                            _ => bail!(
                                "unexpected steward response; check session state before retrying"
                            ),
                        }
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
