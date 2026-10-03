use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::ChildStdout;
use tokio::process::Command;

use crate::maf::HostSnapshotBridge;
use crate::maf::MAX_MESSAGE_BYTES;
use crate::maf::ReadRequest;

#[derive(Clone, serde::Serialize)]
pub struct MafToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MafNativeReadContext {
    pub cwd: String,
    pub read_roots: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MafNativeReadResult {
    pub success: bool,
    pub text: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MafToolRequest {
    pub version: u32,
    pub r#type: String,
    pub request_id: String,
    pub snapshot_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default)]
    pub result: Option<MafNativeReadResult>,
}

pub struct MafProcess {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    snapshot: Arc<HostSnapshotBridge>,
    poisoned: bool,
}

impl MafProcess {
    pub async fn start(assembly: &Path, snapshot: Arc<HostSnapshotBridge>) -> Result<Self, String> {
        let mut child = Command::new("dotnet")
            .arg(assembly)
            .arg("--agent")
            .env("CODEX_MAF_LIVE_HOST", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "maf_spawn_failed")?;
        let mut process = Self {
            input: child.stdin.take().ok_or("maf_stdin_unavailable")?,
            output: BufReader::new(child.stdout.take().ok_or("maf_stdout_unavailable")?),
            child,
            snapshot,
            poisoned: false,
        };
        let wire =
            serde_json::to_value(process.snapshot.snapshot()).map_err(|_| "invalid_snapshot")?;
        process.send(&wire).await?;
        let catalog = process.receive().await?;
        process.validate(&catalog)?;
        if catalog["type"] != "catalog" {
            return Err("invalid_maf_catalog".into());
        }
        Ok(process)
    }

    pub async fn run<F, Fut>(
        &mut self,
        turn_id: &str,
        text: &str,
        model: F,
    ) -> Result<String, String>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<Value, String>>,
    {
        self.run_with_tools(turn_id, text, &[], model, |_| async {
            Err("maf_tools_unavailable".into())
        })
        .await
    }

    pub async fn run_with_tools<F, Fut, Tool, ToolFuture>(
        &mut self,
        turn_id: &str,
        text: &str,
        tools: &[MafToolDefinition],
        model: F,
        tool: Tool,
    ) -> Result<String, String>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<Value, String>>,
        Tool: FnMut(MafToolRequest) -> ToolFuture,
        ToolFuture: Future<Output = Result<String, String>>,
    {
        self.run_configured(turn_id, text, tools, None, model, tool)
            .await
    }

    pub async fn run_native_read<F, Fut, Observer, ObserverFuture>(
        &mut self,
        turn_id: &str,
        text: &str,
        context: &MafNativeReadContext,
        model: F,
        observer: Observer,
    ) -> Result<String, String>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<Value, String>>,
        Observer: FnMut(MafToolRequest) -> ObserverFuture,
        ObserverFuture: Future<Output = Result<String, String>>,
    {
        self.run_configured(turn_id, text, &[], Some(context), model, observer)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_configured<F, Fut, Tool, ToolFuture>(
        &mut self,
        turn_id: &str,
        text: &str,
        tools: &[MafToolDefinition],
        native: Option<&MafNativeReadContext>,
        mut model: F,
        mut tool: Tool,
    ) -> Result<String, String>
    where
        F: FnMut(Value) -> Fut,
        Fut: Future<Output = Result<Value, String>>,
        Tool: FnMut(MafToolRequest) -> ToolFuture,
        ToolFuture: Future<Output = Result<String, String>>,
    {
        if self.poisoned {
            return Err("maf_session_unavailable".into());
        }
        self.poisoned = true;
        self.send(&json!({
            "version": 1, "type": "run", "snapshotId": self.snapshot.snapshot().snapshot_id,
            "requestId": turn_id, "text": text, "tools": tools, "nativeRead": native
        }))
        .await?;
        let mut model_requests = 0;
        let mut pending_call: Option<Value> = None;
        let mut tool_request_ids = HashSet::new();
        let mut tool_call_ids = HashSet::new();
        let mut native_started: Option<String> = None;
        loop {
            let message = self.receive().await?;
            self.validate(&message)?;
            match message["type"].as_str() {
                Some("read") => {
                    if pending_call.is_some() {
                        return Err("invalid_maf_tool_sequence".into());
                    }
                    let request: ReadRequest =
                        serde_json::from_value(message).map_err(|_| "invalid_maf_read")?;
                    let response = self.snapshot.read(request).await;
                    self.send(
                        &serde_json::to_value(response).map_err(|_| "invalid_maf_read_result")?,
                    )
                    .await?;
                }
                Some("modelRequest") => {
                    model_requests += 1;
                    if message["turnId"] != turn_id || model_requests > 8 || pending_call.is_some()
                    {
                        return Err("invalid_maf_model_request".into());
                    }
                    let response = model(message).await?;
                    for call in response["calls"].as_array().into_iter().flatten() {
                        if tools.iter().any(|tool| call["name"] == tool.name)
                            || (native.is_some() && call["name"] == "codex_read_file")
                        {
                            if pending_call.is_some()
                                || !tool_call_ids.insert(call["callId"].clone().to_string())
                            {
                                return Err("invalid_maf_tool_sequence".into());
                            }
                            pending_call = Some(call.clone());
                        }
                    }
                    self.send(&response).await?;
                }
                Some("toolRequest") => {
                    if native.is_some() {
                        return Err("rust_tool_execution_disabled".into());
                    }
                    let request: MafToolRequest =
                        serde_json::from_value(message).map_err(|_| "invalid_maf_tool_request")?;
                    let expected = pending_call.take().ok_or("unsolicited_maf_tool_request")?;
                    if request.turn_id != turn_id
                        || request.request_id.is_empty()
                        || request.request_id.len() > 128
                        || !tool_request_ids.insert(request.request_id.clone())
                        || expected["callId"] != request.call_id
                        || expected["name"] != request.name
                        || expected["arguments"] != request.arguments
                        || !request.arguments.is_object()
                        || !tools.iter().any(|tool| tool.name == request.name)
                    {
                        return Err("invalid_maf_tool_request".into());
                    }
                    let request_id = request.request_id.clone();
                    let call_id = request.call_id.clone();
                    let result = tokio::time::timeout(Duration::from_secs(8), tool(request))
                        .await
                        .map_err(|_| "maf_tool_timeout")??;
                    if result.len() > 256 * 1024 {
                        return Err("maf_tool_result_too_large".into());
                    }
                    self.send(&json!({
                        "version": 1, "type": "toolResult", "snapshotId": self.snapshot.snapshot().snapshot_id,
                        "requestId": request_id, "turnId": turn_id, "callId": call_id, "result": result
                    })).await?;
                }
                Some("nativeToolStarted" | "nativeToolCompleted") if native.is_some() => {
                    let request: MafToolRequest =
                        serde_json::from_value(message).map_err(|_| "invalid_native_tool_event")?;
                    let expected = pending_call
                        .as_ref()
                        .ok_or("unsolicited_native_tool_event")?;
                    if request.turn_id != turn_id
                        || request.request_id.is_empty()
                        || request.request_id.len() > 128
                        || expected["callId"] != request.call_id
                        || expected["name"] != request.name
                        || expected["arguments"] != request.arguments
                        || request.name != "codex_read_file"
                    {
                        return Err("invalid_native_tool_event".into());
                    }
                    if request.r#type == "nativeToolStarted" {
                        if native_started.is_some()
                            || request.result.is_some()
                            || !tool_request_ids.insert(request.request_id.clone())
                        {
                            return Err("invalid_native_tool_sequence".into());
                        }
                        native_started = Some(request.request_id.clone());
                    } else {
                        if native_started.take().as_deref() != Some(request.request_id.as_str())
                            || request
                                .result
                                .as_ref()
                                .is_none_or(|result| result.text.len() > 256 * 1024)
                        {
                            return Err("invalid_native_tool_sequence".into());
                        }
                        pending_call = None;
                    }
                    tool(request).await?;
                }
                Some("completed") if message["requestId"] == turn_id => {
                    if pending_call.is_some() {
                        return Err("incomplete_maf_tool_call".into());
                    }
                    let text = message["text"]
                        .as_str()
                        .ok_or("invalid_maf_completion")?
                        .to_string();
                    self.poisoned = false;
                    return Ok(text);
                }
                _ => return Err("maf_turn_failed".into()),
            }
        }
    }

    pub async fn terminate(&mut self) {
        self.poisoned = true;
        let _ = self.child.kill().await;
    }

    fn validate(&self, message: &Value) -> Result<(), String> {
        if message["version"] != 1 || message["snapshotId"] != self.snapshot.snapshot().snapshot_id
        {
            return Err("invalid_maf_envelope".into());
        }
        Ok(())
    }

    async fn send(&mut self, message: &Value) -> Result<(), String> {
        let mut bytes = serde_json::to_vec(message).map_err(|_| "invalid_maf_message")?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err("maf_message_too_large".into());
        }
        bytes.push(b'\n');
        tokio::time::timeout(Duration::from_secs(10), self.input.write_all(&bytes))
            .await
            .map_err(|_| "maf_write_timeout")?
            .map_err(|_| "maf_write_failed")
            .map_err(String::from)
    }

    async fn receive(&mut self) -> Result<Value, String> {
        let mut bytes = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(65),
            (&mut self.output)
                .take((MAX_MESSAGE_BYTES + 1) as u64)
                .read_until(b'\n', &mut bytes),
        )
        .await
        .map_err(|_| "maf_read_timeout")?
        .map_err(|_| "maf_read_failed")?;
        if bytes.last() != Some(&b'\n') || bytes.len() > MAX_MESSAGE_BYTES + 1 {
            return Err("invalid_maf_frame".into());
        }
        serde_json::from_slice(&bytes).map_err(|_| "invalid_maf_json".into())
    }
}

#[derive(Clone)]
pub struct FoundryTransport {
    pub az: PathBuf,
    pub endpoint: String,
    pub deployment: String,
}

impl FoundryTransport {
    pub async fn respond(&self, request: Value) -> Result<Value, String> {
        let authority = self
            .endpoint
            .strip_prefix("https://")
            .and_then(|endpoint| endpoint.strip_suffix("/openai/v1/responses"));
        if !authority.is_some_and(|host| {
            host.ends_with(".openai.azure.com")
                && host.bytes().all(|character| {
                    character.is_ascii_alphanumeric() || character == b'-' || character == b'.'
                })
        }) {
            return Err("invalid_foundry_endpoint".into());
        }
        let body = responses_request(&request, &self.deployment)?;
        let mut child = Command::new(&self.az)
            .args([
                "rest",
                "--method",
                "post",
                "--url",
                &self.endpoint,
                "--resource",
                "https://cognitiveservices.azure.com/",
                "--headers",
                "Content-Type=application/json",
                "--body",
                "@/dev/stdin",
                "--output",
                "json",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| "foundry_cli_unavailable")?;
        let response = tokio::time::timeout(Duration::from_secs(45), async {
            let mut stdin = child.stdin.take().ok_or("foundry_stdin_unavailable")?;
            stdin
                .write_all(&serde_json::to_vec(&body).map_err(|_| "invalid_foundry_request")?)
                .await
                .map_err(|_| "foundry_write_failed")?;
            drop(stdin);
            let mut bytes = Vec::new();
            child
                .stdout
                .take()
                .ok_or("foundry_stdout_unavailable")?
                .take((MAX_MESSAGE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "foundry_read_failed")?;
            if bytes.len() > MAX_MESSAGE_BYTES {
                return Err("foundry_response_too_large");
            }
            if !child
                .wait()
                .await
                .map_err(|_| "foundry_wait_failed")?
                .success()
            {
                return Err("foundry_request_failed");
            }
            serde_json::from_slice::<Value>(&bytes).map_err(|_| "invalid_foundry_json")
        })
        .await
        .map_err(|_| "foundry_timeout")??;
        responses_reply(&request, &response)
    }
}

fn responses_request(request: &Value, deployment: &str) -> Result<Value, String> {
    let mut input = Vec::new();
    for message in request["messages"]
        .as_array()
        .ok_or("invalid_model_messages")?
    {
        for content in message["contents"]
            .as_array()
            .ok_or("invalid_model_contents")?
        {
            input.push(match content["type"].as_str() {
                Some("text") => json!({"role": message["role"], "content": content["text"]}),
                Some("functionCall") => json!({"type": "function_call", "call_id": content["callId"],
                    "name": content["name"], "arguments": content["arguments"].to_string()}),
                Some("functionResult") => json!({"type": "function_call_output", "call_id": content["callId"],
                    "output": content["result"].as_str().map(str::to_owned).unwrap_or_else(|| content["result"].to_string())}),
                _ => return Err("unsupported_model_content".into()),
            });
        }
    }
    let tools = request["tools"]
        .as_array()
        .ok_or("invalid_model_tools")?
        .iter()
        .map(|tool| {
            json!({"type": "function", "name": tool["name"], "description": tool["description"],
            "parameters": tool["parameters"], "strict": false})
        })
        .collect::<Vec<_>>();
    Ok(
        json!({"model": deployment, "input": input, "tools": tools, "parallel_tool_calls": false,
        "reasoning": {"effort": "none"}, "max_output_tokens": 2048, "store": false}),
    )
}

fn responses_reply(request: &Value, response: &Value) -> Result<Value, String> {
    if response["status"] != "completed" {
        return Err("foundry_response_incomplete".into());
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    for item in response["output"]
        .as_array()
        .ok_or("invalid_foundry_output")?
    {
        match item["type"].as_str() {
            Some("message") => {
                for content in item["content"].as_array().ok_or("invalid_foundry_content")? {
                    match content["type"].as_str() {
                        Some("output_text") => text.push_str(content["text"].as_str().ok_or("invalid_foundry_text")?),
                        Some("refusal") => text.push_str(content["refusal"].as_str().ok_or("invalid_foundry_refusal")?),
                        _ => return Err("unsupported_foundry_content".into()),
                    }
                }
            }
            Some("function_call") => calls.push(json!({"callId": item["call_id"], "name": item["name"],
                "arguments": serde_json::from_str::<Value>(item["arguments"].as_str().ok_or("invalid_foundry_arguments")?)
                    .map_err(|_| "invalid_foundry_arguments")?})),
            _ => return Err("unsupported_foundry_output".into()),
        }
    }
    Ok(
        json!({"version": 1, "type": "modelResult", "requestId": request["requestId"],
        "snapshotId": request["snapshotId"], "turnId": request["turnId"],
        "text": if text.is_empty() { Value::Null } else { Value::String(text) }, "calls": calls, "error": null}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn responses_mapping_preserves_function_calls_and_results() {
        let request = json!({"messages": [{"role": "user", "contents": [{"type": "text", "text": "hello"}]},
            {"role": "assistant", "contents": [{"type": "functionCall", "callId": "call-1", "name": "load_skill", "arguments": {"name": "demo"}}]},
            {"role": "tool", "contents": [{"type": "functionResult", "callId": "call-1", "result": "original"}]}], "tools": []});
        let body = responses_request(&request, "deployment").unwrap();
        assert_eq!(body["model"], "deployment");
        assert_eq!(body["input"][1]["arguments"], "{\"name\":\"demo\"}");
        assert_eq!(body["input"][2]["output"], "original");
        assert_eq!(body["store"], false);
    }

    #[test]
    fn incomplete_responses_are_not_successful_turns() {
        assert_eq!(
            responses_reply(&json!({}), &json!({"status": "incomplete"})).unwrap_err(),
            "foundry_response_incomplete"
        );
    }

    #[tokio::test]
    async fn credentials_are_not_sent_to_non_azure_endpoints() {
        for endpoint in [
            "https://attacker.example/openai/v1/responses",
            "http://account.openai.azure.com/openai/v1/responses",
            "https://account.openai.azure.com@attacker.example/openai/v1/responses",
        ] {
            let transport = FoundryTransport {
                az: PathBuf::from("/must-not-execute"),
                endpoint: endpoint.into(),
                deployment: "deployment".into(),
            };
            assert_eq!(
                transport.respond(json!({})).await.unwrap_err(),
                "invalid_foundry_endpoint"
            );
        }
    }
}
