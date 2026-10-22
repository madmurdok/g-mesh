//! A minimal synchronous MCP client over one daemon connection, for CLI
//! commands that need the daemon to do what a session's first tool call does:
//! start the walk and wait until structural tools may answer.
//!
//! It speaks the same newline-delimited JSON-RPC the shim proxies, so the
//! daemon cannot tell it from a session: `initialize`,
//! `notifications/initialized`, then one `get_file_outline` call whose answer
//! is read only for whether it is an error. The daemon answers that call only
//! after its `prepare` step (activation requested, structural wait finished or
//! capped, queued plugin changes replayed), which is what makes the answer's
//! arrival a readiness signal.

use std::io::BufReader;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::ipc;
use crate::protocol::ndjson_frame::{read_ndjson_frame, write_ndjson_frame};

/// The request id of the activating tool call.
const CALL_ID: u64 = 1;

/// The answer to the activating tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallAnswer {
    /// The tool result's `isError`.
    pub is_error: bool,
    /// Its text content blocks, joined by newlines.
    pub text: String,
}

/// Runs the handshake and the activating call on `stream` and blocks until the
/// call's answer arrives. No read timeout: a caller that needs one runs this
/// on a thread it can abandon. `client_name` goes into `clientInfo`.
/// `on_requested` runs once the tool call is written: from then on the daemon
/// builds the index even if this client goes away.
pub fn activate_and_wait(
    stream: ipc::Stream,
    client_name: &str,
    on_requested: impl FnOnce(),
) -> Result<CallAnswer> {
    let mut writer = stream.try_clone().context("failed to clone the daemon connection")?;
    let mut reader = BufReader::new(stream);
    let mut send = |message: Value| -> Result<()> {
        let body = serde_json::to_vec(&message).context("failed to serialize an MCP message")?;
        write_ndjson_frame(&mut writer, &body)
    };

    send(json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": env!("CARGO_PKG_VERSION") },
        },
    }))?;
    read_response(&mut reader, 0).context("the daemon did not answer initialize")?;
    send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))?;
    send(json!({
        "jsonrpc": "2.0",
        "id": CALL_ID,
        "method": "tools/call",
        "params": { "name": "get_file_outline", "arguments": { "file_path": "\u{0000}" } },
    }))?;
    on_requested();
    let response = read_response(&mut reader, CALL_ID).context("the daemon did not answer the tool call")?;
    Ok(answer_of(&response))
}

/// Reads frames until the response with `id`, skipping notifications and any
/// other message.
fn read_response(reader: &mut BufReader<ipc::Stream>, id: u64) -> Result<Value> {
    loop {
        let Some(frame) = read_ndjson_frame(reader)? else {
            bail!("the daemon closed the connection");
        };
        let message: Value = serde_json::from_slice(&frame).context("the daemon sent a malformed message")?;
        if message.get("method").is_some() || message.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        if let Some(error) = message.get("error") {
            bail!("the daemon answered with a JSON-RPC error: {error}");
        }
        return Ok(message);
    }
}

fn answer_of(response: &Value) -> CallAnswer {
    let result = response.get("result");
    let is_error = result.and_then(|r| r.get("isError")).and_then(Value::as_bool).unwrap_or(false);
    let text = result
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks.iter().filter_map(|b| b.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n")
        })
        .unwrap_or_default();
    CallAnswer { is_error, text }
}
