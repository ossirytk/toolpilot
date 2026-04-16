use std::io;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, stdin, stdout};
use toolpilot::{
    FsGlobInput, FsTreeInput, GitLogInput, JsonSelectInput, ServerState, TextSearchInput,
    YamlSelectInput, execute_fs_glob, execute_fs_tree, execute_git_log, execute_json_select,
    execute_text_search, execute_yaml_select, tool_definitions,
};

#[derive(Debug, Deserialize)]
struct RpcRequest {
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Serialize)]
struct RpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

fn invalid_params(message: &str) -> Value {
    json!({
        "error": {
            "code": "InvalidInput",
            "message": message
        }
    })
}

fn read_call_arguments(params: &Value) -> Result<(&str, Value), Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_params("Missing tool name"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok((name, arguments))
}

fn execute_tool(state: &mut ServerState, name: &str, args: Value) -> Value {
    state.record_request(name);
    let tool_result = match name {
        "fs_glob" => serde_json::from_value::<FsGlobInput>(args)
            .map_err(|_| invalid_params("Invalid fs_glob arguments"))
            .and_then(|input| {
                serde_json::to_value(execute_fs_glob(input).map_err(|e| json!({ "error": e }))?)
                    .map_err(|_| invalid_params("Serialization failed"))
            }),
        "text_search" => serde_json::from_value::<TextSearchInput>(args)
            .map_err(|_| invalid_params("Invalid text_search arguments"))
            .and_then(|input| {
                serde_json::to_value(
                    execute_text_search(state, input).map_err(|e| json!({ "error": e }))?,
                )
                .map_err(|_| invalid_params("Serialization failed"))
            }),
        "json_select" => serde_json::from_value::<JsonSelectInput>(args)
            .map_err(|_| invalid_params("Invalid json_select arguments"))
            .and_then(|input| {
                serde_json::to_value(
                    execute_json_select(state, input).map_err(|e| json!({ "error": e }))?,
                )
                .map_err(|_| invalid_params("Serialization failed"))
            }),
        "fs_tree" => serde_json::from_value::<FsTreeInput>(args)
            .map_err(|_| invalid_params("Invalid fs_tree arguments"))
            .and_then(|input| {
                serde_json::to_value(execute_fs_tree(input).map_err(|e| json!({ "error": e }))?)
                    .map_err(|_| invalid_params("Serialization failed"))
            }),
        "yaml_select" => serde_json::from_value::<YamlSelectInput>(args)
            .map_err(|_| invalid_params("Invalid yaml_select arguments"))
            .and_then(|input| {
                serde_json::to_value(
                    execute_yaml_select(state, input).map_err(|e| json!({ "error": e }))?,
                )
                .map_err(|_| invalid_params("Serialization failed"))
            }),
        "git_log" => serde_json::from_value::<GitLogInput>(args)
            .map_err(|_| invalid_params("Invalid git_log arguments"))
            .and_then(|input| {
                serde_json::to_value(execute_git_log(input).map_err(|e| json!({ "error": e }))?)
                    .map_err(|_| invalid_params("Serialization failed"))
            }),
        "server_stats" => Ok(state.metrics_json()),
        _ => Err(json!({
            "error": {
                "code": "UnknownTool",
                "message": "Tool not found"
            }
        })),
    };

    match tool_result {
        Ok(value) => value,
        Err(value) => value,
    }
}

fn handle_request(state: &mut ServerState, request: RpcRequest) -> RpcResponse {
    let result = match request.method.as_str() {
        "initialize" => Some(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "toolpilot", "version": "0.1.0"}
        })),
        "tools/list" => Some(tool_definitions()),
        "tools/call" => {
            let payload = match read_call_arguments(&request.params) {
                Ok((name, args)) => execute_tool(state, name, args),
                Err(error) => error,
            };
            Some(json!({"structuredContent": payload}))
        }
        _ => None,
    };
    if let Some(result) = result {
        RpcResponse {
            jsonrpc: "2.0",
            id: request.id,
            result: Some(result),
            error: None,
        }
    } else {
        RpcResponse {
            jsonrpc: "2.0",
            id: request.id,
            result: None,
            error: Some(json!({
                "code": -32601,
                "message": "Method not found"
            })),
        }
    }
}

async fn read_message(reader: &mut BufReader<tokio::io::Stdin>) -> io::Result<Option<Vec<u8>>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            let parsed = value.trim().parse::<usize>().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length header")
            })?;
            content_length = Some(parsed);
        }
    }
    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn write_message(writer: &mut tokio::io::Stdout, response: &RpcResponse) -> io::Result<()> {
    let payload = serde_json::to_vec(response)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "serialization failed"))?;
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let mut state = ServerState::new();
    let mut reader = BufReader::new(stdin());
    let mut writer = stdout();

    while let Some(payload) = read_message(&mut reader).await? {
        let request: RpcRequest = match serde_json::from_slice(&payload) {
            Ok(request) => request,
            Err(_) => {
                let response = RpcResponse {
                    jsonrpc: "2.0",
                    id: Value::Null,
                    result: None,
                    error: Some(json!({
                        "code": -32700,
                        "message": "Parse error"
                    })),
                };
                write_message(&mut writer, &response).await?;
                continue;
            }
        };
        #[cfg(feature = "verbose-logging")]
        eprintln!("received method={}", request.method);
        let response = handle_request(&mut state, request);
        write_message(&mut writer, &response).await?;
    }
    Ok(())
}
