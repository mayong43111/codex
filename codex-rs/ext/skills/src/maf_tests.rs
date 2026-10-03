use std::sync::Arc;

use codex_exec_server::LOCAL_FS;
use codex_protocol::protocol::SkillScope;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tokio::sync::Semaphore;

use super::*;
use crate::loader::HostSkillRoot;
use crate::loader::load_and_merge_host_skill_roots;

async fn fixture() -> (tempfile::TempDir, HostSnapshotBridge) {
    let directory = tempfile::tempdir().unwrap();
    let skill = directory.path().join("demo");
    std::fs::create_dir(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: Legacy_Skill\ndescription: Original description.\n---\nOriginal instructions.\n",
    )
    .unwrap();
    let outcome = load_and_merge_host_skill_roots(
        vec![HostSkillRoot::host(
            AbsolutePathBuf::try_from(directory.path()).unwrap(),
            SkillScope::User,
            Arc::clone(&LOCAL_FS),
        )],
        &Semaphore::new(1),
        None,
        None,
    )
    .await;
    let bridge = HostSnapshotBridge::new(
        "snapshot-1".into(),
        Arc::new(HostSkillsSnapshot::new(Arc::new(outcome))),
    )
    .await
    .unwrap();
    (directory, bridge)
}

fn request(bridge: &HostSnapshotBridge) -> ReadRequest {
    let entry = &bridge.snapshot().entries[0];
    ReadRequest {
        version: PROTOCOL_VERSION,
        kind: "read".into(),
        request_id: "read-1".into(),
        snapshot_id: bridge.snapshot().snapshot_id.clone(),
        authority: entry.authority.clone(),
        package_id: entry.package_id.clone(),
        resource_id: entry.resource_id.clone(),
    }
}

#[tokio::test]
async fn snapshot_preserves_metadata_and_reads_through_host_provider() {
    let (_directory, bridge) = fixture().await;
    assert_eq!(bridge.snapshot().entries.len(), 1);
    assert_eq!(bridge.snapshot().entries[0].name, "Legacy_Skill");
    let wire = serde_json::to_value(bridge.snapshot()).unwrap();
    assert_eq!(wire["snapshotId"], "snapshot-1");
    let response = bridge.read(request(&bridge)).await;
    assert!(response.error.is_none());
    assert!(
        response
            .contents
            .unwrap()
            .contains("Original instructions.")
    );
}

#[tokio::test]
#[ignore = "requires explicit Foundry configuration and performs live model inference"]
async fn live_foundry_runs_real_dotnet_host() {
    use crate::maf_runtime::FoundryTransport;
    use crate::maf_runtime::MafProcess;

    let (_directory, bridge) = fixture().await;
    let transport = FoundryTransport {
        az: std::env::var_os("CODEX_MAF_AZ")
            .expect("set CODEX_MAF_AZ")
            .into(),
        endpoint: std::env::var("CODEX_MAF_FOUNDRY_ENDPOINT")
            .expect("set CODEX_MAF_FOUNDRY_ENDPOINT"),
        deployment: std::env::var("CODEX_MAF_DEPLOYMENT").expect("set CODEX_MAF_DEPLOYMENT"),
    };
    let assembly = std::path::PathBuf::from(std::env::var_os("CODEX_MAF_BRIDGE_DLL").unwrap());
    let mut process = MafProcess::start(&assembly, Arc::new(bridge))
        .await
        .unwrap();
    let result = process
        .run("live-1", "Reply with MAF_CONNECTED only.", |request| {
            transport.respond(request)
        })
        .await;
    process.terminate().await;
    assert_eq!(result.unwrap().trim(), "MAF_CONNECTED");
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn host_runtime_never_reuses_a_failed_model_turn() {
    use crate::maf_runtime::MafProcess;

    let (_directory, bridge) = fixture().await;
    let assembly = std::path::PathBuf::from(std::env::var_os("CODEX_MAF_BRIDGE_DLL").unwrap());
    let mut process = MafProcess::start(&assembly, Arc::new(bridge))
        .await
        .unwrap();
    let error = process
        .run("turn-1", "hello", |_| async {
            Err("model_unavailable".into())
        })
        .await;
    assert_eq!(error.unwrap_err(), "model_unavailable");
    let error = process
        .run("turn-2", "do not replay", |_| async {
            panic!("failed sessions must not invoke the model")
        })
        .await;
    assert_eq!(error.unwrap_err(), "maf_session_unavailable");
    process.terminate().await;
}

#[tokio::test]
async fn stale_and_forged_resource_references_are_rejected() {
    let (_directory, bridge) = fixture().await;
    let mut stale = request(&bridge);
    stale.snapshot_id = "snapshot-2".into();
    assert_eq!(
        bridge.read(stale).await.error.as_deref(),
        Some("stale_snapshot")
    );
    for field in ["kind", "authority", "package", "resource"] {
        let mut forged = request(&bridge);
        match field {
            "kind" => forged.authority.kind = "executor".into(),
            "authority" => forged.authority.id = "another-host".into(),
            "package" => forged.package_id = "/outside".into(),
            "resource" => forged.resource_id = "/outside/SKILL.md".into(),
            _ => unreachable!(),
        }
        assert_eq!(
            bridge.read(forged).await.error.as_deref(),
            Some("resource_not_allowed")
        );
    }
}

#[tokio::test]
async fn disabled_and_hidden_entries_cannot_be_read() {
    let (_directory, mut bridge) = fixture().await;
    bridge.catalog.entries[0].enabled = false;
    assert_eq!(
        bridge.read(request(&bridge)).await.error.as_deref(),
        Some("resource_not_allowed")
    );
    bridge.catalog.entries[0].enabled = true;
    bridge.catalog.entries[0].prompt_visible = false;
    assert_eq!(
        bridge.read(request(&bridge)).await.error.as_deref(),
        Some("resource_not_allowed")
    );
}

#[tokio::test]
async fn failed_and_oversized_reads_return_bounded_errors() {
    let (_directory, bridge) = fixture().await;
    let path = &bridge.snapshot().entries[0].resource_id;
    std::fs::write(path, "x".repeat(MAX_CONTENT_BYTES + 1)).unwrap();
    assert_eq!(
        bridge.read(request(&bridge)).await.error.as_deref(),
        Some("resource_too_large")
    );
    std::fs::remove_file(path).unwrap();
    assert_eq!(
        bridge.read(request(&bridge)).await.error.as_deref(),
        Some("resource_read_failed")
    );
}

#[tokio::test]
async fn protocol_version_is_checked_before_reading() {
    let (_directory, bridge) = fixture().await;
    let mut invalid = request(&bridge);
    invalid.version = 2;
    assert_eq!(
        bridge.read(invalid).await.error.as_deref(),
        Some("invalid_request")
    );
}

struct DotnetBridge {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::BufReader<tokio::process::ChildStdout>,
}

impl DotnetBridge {
    async fn start(snapshot: &Snapshot) -> Self {
        Self::start_with_args(snapshot, &[]).await
    }

    async fn start_with_args(snapshot: &Snapshot, arguments: &[&str]) -> Self {
        let assembly = std::env::var("CODEX_MAF_BRIDGE_DLL")
            .expect("build the .NET bridge and set CODEX_MAF_BRIDGE_DLL");
        let mut child = tokio::process::Command::new("dotnet")
            .arg(assembly)
            .args(arguments)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut bridge = Self {
            input: child.stdin.take().unwrap(),
            output: tokio::io::BufReader::new(child.stdout.take().unwrap()),
            child,
        };
        bridge.send(snapshot).await;
        bridge
    }

    async fn send(&mut self, message: &impl Serialize) {
        use tokio::io::AsyncWriteExt;
        let mut bytes = serde_json::to_vec(message).unwrap();
        bytes.push(b'\n');
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            self.input.write_all(&bytes),
        )
        .await
        .unwrap()
        .unwrap();
    }

    async fn receive(&mut self) -> serde_json::Value {
        use tokio::io::AsyncBufReadExt;
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(15),
            (&mut self.output)
                .take((MAX_MESSAGE_BYTES + 1) as u64)
                .read_until(b'\n', &mut bytes),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(bytes.last(), Some(&b'\n'));
        assert!(bytes.len() <= MAX_MESSAGE_BYTES + 1);
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn close(mut self) {
        self.send(&serde_json::json!({
            "version": 1, "type": "close", "requestId": "close-1", "snapshotId": "snapshot-1"
        }))
        .await;
        assert!(self.wait().await.success());
    }

    async fn wait(&mut self) -> std::process::ExitStatus {
        tokio::time::timeout(std::time::Duration::from_secs(15), self.child.wait())
            .await
            .unwrap()
            .unwrap()
    }
}

fn load_command(name: &str) -> serde_json::Value {
    serde_json::json!({
        "version": 1, "type": "load", "requestId": "load-1", "snapshotId": "snapshot-1", "name": name
    })
}

fn run_command(request_id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "version": 1, "type": "run", "requestId": request_id,
        "snapshotId": "snapshot-1", "text": text
    })
}

fn model_reply(
    request: &serde_json::Value,
    text: Option<&str>,
    calls: serde_json::Value,
) -> serde_json::Value {
    assert_eq!(request["type"], "modelRequest");
    serde_json::json!({
        "version": 1, "type": "modelResult", "requestId": request["requestId"],
        "snapshotId": request["snapshotId"], "turnId": request["turnId"],
        "text": text, "calls": calls, "error": null
    })
}

fn skill_call(model: &serde_json::Value, name: &str, call_id: &str) -> serde_json::Value {
    let tool = model["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "load_skill")
        .unwrap_or_else(|| panic!("MAF must advertise load_skill: {}", model["tools"]));
    let parameters = tool["parameters"]["required"].as_array().unwrap();
    assert_eq!(parameters.len(), 1);
    let parameter = parameters[0].as_str().unwrap();
    let mut arguments = serde_json::Map::new();
    arguments.insert(parameter.into(), name.into());
    serde_json::json!([{
        "callId": call_id, "name": tool["name"], "arguments": arguments
    }])
}

fn host_read_tool() -> crate::maf_runtime::MafToolDefinition {
    crate::maf_runtime::MafToolDefinition {
        name: "codex_read_file".into(),
        description: "Read a bounded text file with host permissions.".into(),
        parameters: serde_json::json!({
            "type": "object", "properties": {"path": {"type": "string"}},
            "required": ["path"], "additionalProperties": false
        }),
    }
}

fn host_file_call() -> serde_json::Value {
    serde_json::json!([{"callId": "read-call", "name": "codex_read_file", "arguments": {"path": "proof.txt"}}])
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and Linux Landlock ABI 3+"]
async fn native_read_tool_runs_without_rust_execution() {
    use crate::maf_runtime::MafNativeReadContext;
    use crate::maf_runtime::MafProcess;
    let (directory, bridge) = fixture().await;
    std::fs::write(directory.path().join("proof.txt"), "native-only proof\n").unwrap();
    let assembly = std::path::PathBuf::from(std::env::var_os("CODEX_MAF_BRIDGE_DLL").unwrap());
    let mut process = MafProcess::start(&assembly, Arc::new(bridge))
        .await
        .unwrap();
    let native = MafNativeReadContext {
        cwd: directory.path().to_string_lossy().into_owned(),
        read_roots: vec![directory.path().to_string_lossy().into_owned()],
    };
    let mut requests = 0;
    let mut events = Vec::new();
    let result = process
        .run_native_read(
            "native-turn",
            "Read proof.txt.",
            &native,
            |request| {
                requests += 1;
                let reply = if requests == 1 {
                    model_reply(&request, None, host_file_call())
                } else {
                    assert!(
                        request["messages"]
                            .to_string()
                            .contains("native-only proof")
                    );
                    model_reply(
                        &request,
                        Some("native tool completed"),
                        serde_json::json!([]),
                    )
                };
                async { Ok(reply) }
            },
            |event| {
                assert!(matches!(
                    event.r#type.as_str(),
                    "nativeToolStarted" | "nativeToolCompleted"
                ));
                if let Some(result) = &event.result {
                    assert!(result.success, "{}", result.text);
                    assert_eq!(result.text, "native-only proof\n");
                }
                events.push(event.r#type);
                async { Ok("this observer response must never become a tool result".into()) }
            },
        )
        .await;
    process.terminate().await;
    assert_eq!(result.unwrap(), "native tool completed");
    assert_eq!(events, vec!["nativeToolStarted", "nativeToolCompleted"]);
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and Linux Landlock ABI 3+"]
async fn native_read_tool_cancellation_does_not_replay() {
    use crate::maf_runtime::MafNativeReadContext;
    use crate::maf_runtime::MafProcess;
    let (directory, bridge) = fixture().await;
    std::fs::write(directory.path().join("proof.txt"), "native-only proof").unwrap();
    let assembly = std::path::PathBuf::from(std::env::var_os("CODEX_MAF_BRIDGE_DLL").unwrap());
    let mut process = MafProcess::start(&assembly, Arc::new(bridge))
        .await
        .unwrap();
    let native = MafNativeReadContext {
        cwd: directory.path().to_string_lossy().into_owned(),
        read_roots: vec!["/".into()],
    };
    let (entered, received) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered);
    {
        let run = process.run_native_read(
            "native-turn",
            "Read proof.txt.",
            &native,
            |request| async move { Ok(model_reply(&request, None, host_file_call())) },
            |event| {
                assert_eq!(event.r#type, "nativeToolStarted");
                entered.take().unwrap().send(()).unwrap();
                std::future::pending::<Result<String, String>>()
            },
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("expected active native read: {result:?}"),
            result = tokio::time::timeout(std::time::Duration::from_secs(10), received) => { result.unwrap().unwrap(); }
        }
    }
    assert!(
        process
            .run("retry", "Do not replay.", |_| async {
                panic!("cancelled native session must not call a model")
            })
            .await
            .is_err()
    );
    process.terminate().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn host_read_tool_returns_success_and_denial_through_maf() {
    use crate::maf_runtime::MafProcess;
    for result in [
        "host-only proof",
        "{\"success\":false,\"error\":\"read denied\"}",
    ] {
        let (_directory, bridge) = fixture().await;
        let assembly = std::path::PathBuf::from(std::env::var_os("CODEX_MAF_BRIDGE_DLL").unwrap());
        let mut process = MafProcess::start(&assembly, Arc::new(bridge))
            .await
            .unwrap();
        let mut requests = 0;
        let mut executions = 0;
        let output = process
            .run_with_tools(
                "tool-turn",
                "Read proof.txt.",
                &[host_read_tool()],
                |request| {
                    requests += 1;
                    let reply = if requests == 1 {
                        assert!(
                            request["tools"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|tool| tool["name"] == "codex_read_file")
                        );
                        model_reply(&request, None, host_file_call())
                    } else {
                        assert!(
                            request["messages"]
                                .to_string()
                                .contains(&serde_json::to_string(result).unwrap())
                        );
                        model_reply(
                            &request,
                            Some("observed tool result"),
                            serde_json::json!([]),
                        )
                    };
                    async { Ok(reply) }
                },
                |request| {
                    executions += 1;
                    assert_eq!(request.call_id, "read-call");
                    assert_eq!(request.arguments, serde_json::json!({"path": "proof.txt"}));
                    async move { Ok(result.to_string()) }
                },
            )
            .await;
        process.terminate().await;
        assert_eq!(output.unwrap(), "observed tool result");
        assert_eq!(requests, 2);
        assert_eq!(executions, 1);
    }
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn host_read_tool_cancellation_prevents_session_replay() {
    use crate::maf_runtime::MafProcess;
    let (_directory, bridge) = fixture().await;
    let assembly = std::path::PathBuf::from(std::env::var_os("CODEX_MAF_BRIDGE_DLL").unwrap());
    let mut process = MafProcess::start(&assembly, Arc::new(bridge))
        .await
        .unwrap();
    let (entered, received) = tokio::sync::oneshot::channel();
    let mut entered = Some(entered);
    let tools = [host_read_tool()];
    {
        let run = process.run_with_tools(
            "cancel-turn",
            "Read proof.txt.",
            &tools,
            |request| async move { Ok(model_reply(&request, None, host_file_call())) },
            |_| {
                entered.take().unwrap().send(()).unwrap();
                std::future::pending::<Result<String, String>>()
            },
        );
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("tool should still be pending: {result:?}"),
            result = tokio::time::timeout(std::time::Duration::from_secs(10), received) => { result.unwrap().unwrap(); }
        }
    }
    assert!(
        process
            .run("after-cancel", "Do not replay.", |_| async {
                panic!("cancelled session must not call a model")
            })
            .await
            .is_err()
    );
    process.terminate().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_host_read_tool_rejects_mismatched_results() {
    for field in [
        "requestId",
        "snapshotId",
        "turnId",
        "callId",
        "type",
        "version",
        "result",
    ] {
        let (_directory, bridge) = fixture().await;
        let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
        process.receive().await;
        let mut command = run_command("tool-turn", "Read proof.txt.");
        command["tools"] = serde_json::to_value([host_read_tool()]).unwrap();
        process.send(&command).await;
        let model = process.receive().await;
        process
            .send(&model_reply(&model, None, host_file_call()))
            .await;
        let request = process.receive().await;
        assert_eq!(request["type"], "toolRequest");
        let mut reply = serde_json::json!({
            "version": 1, "type": "toolResult", "requestId": request["requestId"],
            "snapshotId": request["snapshotId"], "turnId": request["turnId"],
            "callId": request["callId"], "result": "proof"
        });
        reply[field] = if field == "version" {
            2.into()
        } else if field == "result" {
            serde_json::Value::Null
        } else {
            "mismatch".into()
        };
        process.send(&reply).await;
        assert!(
            !process.wait().await.success(),
            "accepted mismatched {field}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn host_read_tool_rejects_unadvertised_and_duplicate_calls() {
    use crate::maf_runtime::MafProcess;
    for tool_name in ["shell", "codex_read_file"] {
        let (_directory, bridge) = fixture().await;
        let assembly = std::path::PathBuf::from(std::env::var_os("CODEX_MAF_BRIDGE_DLL").unwrap());
        let mut process = MafProcess::start(&assembly, Arc::new(bridge))
            .await
            .unwrap();
        let mut executions = 0;
        let result = process
            .run_with_tools(
                "tool-turn",
                "Read proof.txt.",
                &[host_read_tool()],
                |request| {
                    let mut calls = host_file_call();
                    calls[0]["name"] = tool_name.into();
                    async move { Ok(model_reply(&request, None, calls)) }
                },
                |_| {
                    executions += 1;
                    async { Ok("proof".into()) }
                },
            )
            .await;
        process.terminate().await;
        assert!(result.is_err());
        assert_eq!(executions, usize::from(tool_name == "codex_read_file"));
    }
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_runs_maf_skill_loop_and_retains_session() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
    let catalog = process.receive().await;
    let name = catalog["skills"][0]["name"].as_str().unwrap();
    process
        .send(&run_command("turn-1", "Read the selected skill."))
        .await;
    let first = process.receive().await;
    assert_eq!(first["turnId"], "turn-1");
    process
        .send(&model_reply(
            &first,
            None,
            skill_call(&first, name, "call-1"),
        ))
        .await;
    let read: ReadRequest = serde_json::from_value(process.receive().await).unwrap();
    let response = bridge.read(read).await;
    let original = response.contents.clone().unwrap();
    process.send(&response).await;
    let second = process.receive().await;
    let result = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["contents"].as_array().unwrap())
        .find(|content| content["type"] == "functionResult" && content["callId"] == "call-1")
        .expect("MAF must send the tool result back to the model");
    assert!(result["result"].as_str().unwrap().contains(&original));
    assert_ne!(first["requestId"], second["requestId"]);
    process
        .send(&model_reply(
            &second,
            Some("Skill read by MAF."),
            serde_json::json!([]),
        ))
        .await;
    let completed = process.receive().await;
    assert_eq!(completed["type"], "completed");
    assert_eq!(completed["requestId"], "turn-1");
    assert_eq!(completed["text"], "Skill read by MAF.");
    process
        .send(&run_command("turn-2", "What did you read?"))
        .await;
    let next_turn = process.receive().await;
    assert_eq!(next_turn["turnId"], "turn-2");
    assert!(
        next_turn["messages"]
            .to_string()
            .contains("Skill read by MAF.")
    );
    process
        .send(&model_reply(
            &next_turn,
            Some("The selected skill."),
            serde_json::json!([]),
        ))
        .await;
    assert_eq!(process.receive().await["type"], "completed");
    process.close().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_default_mode_does_not_run_an_agent() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start(bridge.snapshot()).await;
    process.receive().await;
    process
        .send(&run_command("turn-1", "Do not call a model."))
        .await;
    assert_eq!(process.receive().await["error"], "skill_not_available");
    process.close().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_direct_completion_isolates_process_sessions() {
    let (_directory, bridge) = fixture().await;
    for (text, absent) in [
        ("first-session-only", "second-session-only"),
        ("second-session-only", "first-session-only"),
    ] {
        let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
        process.receive().await;
        process.send(&run_command("turn-1", text)).await;
        let model = process.receive().await;
        assert!(model["messages"].to_string().contains(text));
        assert!(!model["messages"].to_string().contains(absent));
        assert!(
            model["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|tool| tool["name"] == "load_skill")
        );
        process
            .send(&model_reply(&model, Some(text), serde_json::json!([])))
            .await;
        let completed = process.receive().await;
        assert_eq!(completed["type"], "completed");
        assert_eq!(completed["text"], text);
        process.close().await;
    }
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_rejects_invalid_model_responses() {
    let (_directory, bridge) = fixture().await;
    for field in [
        "requestId",
        "snapshotId",
        "turnId",
        "version",
        "type",
        "error",
        "empty",
        "calls",
        "unknownTool",
        "multipleCalls",
        "largeText",
    ] {
        let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
        let catalog = process.receive().await;
        process
            .send(&run_command("turn-1", "Read the skill."))
            .await;
        let model = process.receive().await;
        let mut response = model_reply(&model, Some("A response."), serde_json::json!([]));
        match field {
            "requestId" | "snapshotId" | "turnId" | "type" => response[field] = "wrong".into(),
            "version" => response[field] = 2.into(),
            "error" => response[field] = "host failure containing private details".into(),
            "empty" => response["text"] = serde_json::Value::Null,
            "calls" => response[field] = serde_json::Value::Null,
            "unknownTool" => {
                response["calls"] = serde_json::json!([{"callId": "call-1", "name": "run_skill_script", "arguments": {}}])
            }
            "multipleCalls" => {
                let calls = skill_call(
                    &model,
                    catalog["skills"][0]["name"].as_str().unwrap(),
                    "call-1",
                );
                response["calls"] = serde_json::json!([calls[0], calls[0]]);
            }
            "largeText" => response["text"] = "x".repeat(MAX_CONTENT_BYTES + 1).into(),
            _ => unreachable!(),
        }
        process.send(&response).await;
        assert!(
            !process.wait().await.success(),
            "accepted invalid field: {field}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_returns_host_read_failure_to_maf() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
    let catalog = process.receive().await;
    std::fs::remove_file(&bridge.snapshot().entries[0].resource_id).unwrap();
    process
        .send(&run_command("turn-1", "Read the skill."))
        .await;
    let model = process.receive().await;
    process
        .send(&model_reply(
            &model,
            None,
            skill_call(
                &model,
                catalog["skills"][0]["name"].as_str().unwrap(),
                "call-1",
            ),
        ))
        .await;
    let read = serde_json::from_value(process.receive().await).unwrap();
    let denied = bridge.read(read).await;
    assert_eq!(denied.error.as_deref(), Some("resource_read_failed"));
    process.send(&denied).await;
    let continuation = process.receive().await;
    let result = continuation["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["contents"].as_array().unwrap())
        .find(|content| content["type"] == "functionResult")
        .expect("MAF must receive the failed function result");
    assert!(
        !result
            .to_string()
            .contains(&bridge.snapshot().entries[0].resource_id)
    );
    assert!(!result.to_string().contains("Original instructions."));
    process
        .send(&model_reply(
            &continuation,
            Some("The skill could not be read."),
            serde_json::json!([]),
        ))
        .await;
    assert_eq!(
        process.receive().await["text"],
        "The skill could not be read."
    );
    process.close().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_stops_after_a_mismatched_skill_response() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
    let catalog = process.receive().await;
    process
        .send(&run_command("turn-1", "Read the skill."))
        .await;
    let model = process.receive().await;
    process
        .send(&model_reply(
            &model,
            None,
            skill_call(
                &model,
                catalog["skills"][0]["name"].as_str().unwrap(),
                "call-1",
            ),
        ))
        .await;
    let read = serde_json::from_value(process.receive().await).unwrap();
    let mut response = bridge.read(read).await;
    response.request_id = "wrong-id".into();
    process.send(&response).await;
    assert!(!process.wait().await.success());
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_bounds_model_iterations_without_reporting_completion() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
    process.receive().await;
    process
        .send(&run_command("turn-1", "Read an unavailable skill."))
        .await;
    for iteration in 0..8 {
        let model = process.receive().await;
        let calls = skill_call(&model, "missing-skill", &format!("call-{iteration}"));
        process.send(&model_reply(&model, None, calls)).await;
    }
    assert!(!process.wait().await.success());
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_bounds_waiting_for_model_transport() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
    process.receive().await;
    process
        .send(&run_command("turn-1", "Wait for the host model transport."))
        .await;
    assert_eq!(process.receive().await["type"], "modelRequest");
    assert!(!process.wait().await.success());
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_exits_on_model_transport_disconnect() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
    process.receive().await;
    process
        .send(&run_command("turn-1", "Wait for the host model transport."))
        .await;
    assert_eq!(process.receive().await["type"], "modelRequest");
    drop(process.input);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), process.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_agent_rejects_invalid_turns_before_model_invocation() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start_with_args(bridge.snapshot(), &["--agent"]).await;
    process.receive().await;
    for text in [String::new(), "x".repeat(MAX_CONTENT_BYTES + 1)] {
        process.send(&run_command("turn-1", &text)).await;
        assert_eq!(process.receive().await["error"], "invalid_turn_input");
    }
    let mut stale = run_command("turn-1", "A stale turn.");
    stale["snapshotId"] = "stale".into();
    process.send(&stale).await;
    assert_eq!(process.receive().await["error"], "stale_snapshot");
    process.close().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_reads_real_host_snapshot_lazily() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start(bridge.snapshot()).await;
    let catalog = process.receive().await;
    assert_eq!(catalog["type"], "catalog");
    assert_eq!(catalog["skills"][0]["original"]["name"], "Legacy_Skill");
    let name = catalog["skills"][0]["name"].as_str().unwrap();
    assert!(name.starts_with("codex-"));
    process.send(&load_command(name)).await;
    let read: ReadRequest = serde_json::from_value(process.receive().await).unwrap();
    assert_eq!(read.authority, bridge.snapshot().entries[0].authority);
    let response = bridge.read(read).await;
    let expected = response.contents.clone().unwrap();
    process.send(&response).await;
    let loaded = process.receive().await;
    assert_eq!(loaded["type"], "loaded");
    assert_eq!(loaded["requestId"], "load-1");
    assert_eq!(loaded["contents"], expected);
    process.close().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_preserves_identity_filters_visibility_and_bounds_descriptions() {
    let (_directory, bridge) = fixture().await;
    let mut snapshot = bridge.snapshot().clone();
    snapshot.entries[0].name = "duplicate".into();
    snapshot.entries[0].description = "z".repeat(1025);
    for (suffix, enabled, visible) in [
        ("duplicate", true, true),
        ("disabled", false, true),
        ("hidden", true, false),
    ] {
        let mut entry = snapshot.entries[0].clone();
        entry.package_id.push_str(suffix);
        entry.enabled = enabled;
        entry.prompt_visible = visible;
        snapshot.entries.push(entry);
    }
    let mut unique = snapshot.entries[0].clone();
    unique.name = "unique-skill".into();
    unique.package_id.push_str("-unique");
    snapshot.entries.push(unique);
    let mut process = DotnetBridge::start(&snapshot).await;
    let catalog = process.receive().await;
    let skills = catalog["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 3);
    assert_eq!(skills[2]["name"], "unique-skill");
    assert_ne!(skills[0]["name"], skills[1]["name"]);
    assert_eq!(skills[0]["description"].as_str().unwrap().len(), 1024);
    assert_eq!(
        skills[0]["original"]["description"].as_str().unwrap().len(),
        1025
    );
    let names: std::collections::BTreeMap<String, String> = skills
        .iter()
        .map(|skill| {
            (
                skill["original"]["packageId"].as_str().unwrap().to_string(),
                skill["name"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    process.close().await;
    snapshot.entries.reverse();
    let mut process = DotnetBridge::start(&snapshot).await;
    let reordered = process.receive().await;
    for skill in reordered["skills"].as_array().unwrap() {
        assert_eq!(
            skill["name"],
            names[skill["original"]["packageId"].as_str().unwrap()]
        );
    }
    process.send(&load_command("Legacy_Skill")).await;
    assert_eq!(process.receive().await["error"], "skill_not_available");
    process.close().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_propagates_host_read_failure_without_exposing_paths() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start(bridge.snapshot()).await;
    let catalog = process.receive().await;
    std::fs::remove_file(&bridge.snapshot().entries[0].resource_id).unwrap();
    process
        .send(&load_command(
            catalog["skills"][0]["name"].as_str().unwrap(),
        ))
        .await;
    let read = serde_json::from_value(process.receive().await).unwrap();
    process.send(&bridge.read(read).await).await;
    let error = process.receive().await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["error"], "resource_read_failed");
    process.close().await;
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_rejects_stale_commands_and_mismatched_responses() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start(bridge.snapshot()).await;
    let catalog = process.receive().await;
    let mut command = load_command(catalog["skills"][0]["name"].as_str().unwrap());
    command["snapshotId"] = "stale".into();
    process.send(&command).await;
    assert_eq!(process.receive().await["error"], "stale_snapshot");
    command["snapshotId"] = "snapshot-1".into();
    process.send(&command).await;
    let read = serde_json::from_value(process.receive().await).unwrap();
    let mut response = bridge.read(read).await;
    response.request_id = "wrong-id".into();
    process.send(&response).await;
    assert!(!process.wait().await.success());
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_rejects_duplicate_identities_versions_and_alias_collisions() {
    let (_directory, bridge) = fixture().await;
    let mut duplicate = bridge.snapshot().clone();
    duplicate.entries.push(duplicate.entries[0].clone());
    let mut process = DotnetBridge::start(&duplicate).await;
    assert!(!process.wait().await.success());
    let mut invalid_version = bridge.snapshot().clone();
    invalid_version.version = 2;
    let mut process = DotnetBridge::start(&invalid_version).await;
    assert!(!process.wait().await.success());
    let mut process = DotnetBridge::start(bridge.snapshot()).await;
    let catalog = process.receive().await;
    process.close().await;
    let mut collision = bridge.snapshot().clone();
    let mut entry = collision.entries[0].clone();
    entry.package_id.push_str("-collision");
    entry.name = catalog["skills"][0]["name"].as_str().unwrap().to_string();
    collision.entries.push(entry);
    let mut process = DotnetBridge::start(&collision).await;
    assert!(!process.wait().await.success());
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_exits_when_host_disconnects_during_read() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start(bridge.snapshot()).await;
    let catalog = process.receive().await;
    process
        .send(&load_command(
            catalog["skills"][0]["name"].as_str().unwrap(),
        ))
        .await;
    assert_eq!(process.receive().await["type"], "read");
    drop(process.input);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), process.child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_bounds_waiting_for_an_unresponsive_host() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start(bridge.snapshot()).await;
    let catalog = process.receive().await;
    process
        .send(&load_command(
            catalog["skills"][0]["name"].as_str().unwrap(),
        ))
        .await;
    assert_eq!(process.receive().await["type"], "read");
    assert!(!process.wait().await.success());
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_rejects_malformed_duplicate_and_oversized_frames() {
    use tokio::io::AsyncWriteExt;
    let (_directory, bridge) = fixture().await;
    let messages = [
        b"{bad-json}\n".to_vec(),
        b"{\"version\":1,\"version\":1,\"type\":\"close\",\"requestId\":\"close\",\"snapshotId\":\"snapshot-1\"}\n".to_vec(),
        vec![b'x'; MAX_MESSAGE_BYTES + 2],
    ];
    for message in messages {
        let mut process = DotnetBridge::start(bridge.snapshot()).await;
        assert_eq!(process.receive().await["type"], "catalog");
        let _write_result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            process.input.write_all(&message),
        )
        .await
        .unwrap();
        assert!(!process.wait().await.success());
    }
}

#[tokio::test]
#[ignore = "requires the built .NET bridge and CODEX_MAF_BRIDGE_DLL"]
async fn dotnet_rejects_oversized_host_content() {
    let (_directory, bridge) = fixture().await;
    let mut process = DotnetBridge::start(bridge.snapshot()).await;
    let catalog = process.receive().await;
    process
        .send(&load_command(
            catalog["skills"][0]["name"].as_str().unwrap(),
        ))
        .await;
    let read = serde_json::from_value(process.receive().await).unwrap();
    let mut response = bridge.read(read).await;
    response.contents = Some("x".repeat(MAX_CONTENT_BYTES + 1));
    process.send(&response).await;
    assert_eq!(process.receive().await["error"], "resource_too_large");
    process.close().await;
}
