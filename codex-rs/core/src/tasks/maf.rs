use std::path::PathBuf;
use std::sync::Arc;

use codex_async_utils::OrCancelExt;
use codex_protocol::ResponseItemId;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::items::DynamicToolCallItem;
use codex_protocol::items::DynamicToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use codex_skills_extension::maf::HostSnapshotBridge;
use codex_skills_extension::maf_runtime::FoundryTransport;
use codex_skills_extension::maf_runtime::MafNativeReadContext;
use codex_skills_extension::maf_runtime::MafProcess;
use codex_skills_extension::maf_runtime::MafToolRequest;
use codex_thread_store::PersistContext;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn::run_hooks_and_record_inputs;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use codex_tools::ToolName;

use super::SessionTaskResult;

#[cfg(test)]
mod read_file;

struct MafThread {
    mode: String,
    assembly: Option<PathBuf>,
    transport: FoundryTransport,
    state: Mutex<MafThreadState>,
    history_requires_maf: tokio::sync::OnceCell<bool>,
}

#[derive(Default)]
struct MafThreadState {
    attempted: bool,
    process: Option<MafProcess>,
    context: Option<(String, PathBuf, Vec<u8>)>,
    active_tool: Option<DynamicToolCallItem>,
}

fn thread_state(session: &Session) -> Arc<MafThread> {
    session
        .services
        .thread_extension_data
        .get_or_init(|| MafThread {
            mode: std::env::var("CODEX_AGENT_BACKEND").unwrap_or_default(),
            assembly: std::env::var_os("CODEX_MAF_BRIDGE_DLL").map(PathBuf::from),
            transport: FoundryTransport {
                az: std::env::var_os("CODEX_MAF_AZ")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("az")),
                endpoint: std::env::var("CODEX_MAF_FOUNDRY_ENDPOINT").unwrap_or_default(),
                deployment: std::env::var("CODEX_MAF_DEPLOYMENT").unwrap_or_default(),
            },
            state: Mutex::new(MafThreadState::default()),
            history_requires_maf: tokio::sync::OnceCell::new(),
        })
}

pub(crate) async fn enabled(session: &Session) -> bool {
    let state = thread_state(session);
    if !matches!(state.mode.as_str(), "" | "rust") {
        return true;
    }
    *state.history_requires_maf.get_or_init(|| async {
        session.clone_history().await.raw_items().any(|item| {
            matches!(item, ResponseItem::Message { id: Some(id), .. } if id.as_str().starts_with("maf_backend_"))
        })
    }).await
}

pub(super) async fn run(
    session: Arc<Session>,
    context: Arc<TurnContext>,
    input: Vec<TurnInput>,
    kind: TaskKind,
    cancellation: CancellationToken,
) -> SessionTaskResult {
    session.emit_turn_started(&context).await;
    let backend = thread_state(&session);
    let operation = async {
        if backend.mode != "maf" || kind != TaskKind::Regular {
            return Err(
                "MAF only supports regular text turns; this operation is unavailable".to_string(),
            );
        }
        if context.final_output_json_schema.is_some() || !context.dynamic_tools.is_empty() {
            return Err("MAF does not support structured output or dynamic tools yet".into());
        }
        if let Some(effort) = context.effective_reasoning_effort()
            && serde_json::to_value(effort).map_err(|_| "invalid reasoning setting")?
                != serde_json::json!("none")
        {
            return Err("MAF currently requires reasoning effort none".into());
        }
        let assembly = backend
            .assembly
            .as_ref()
            .ok_or("CODEX_MAF_BRIDGE_DLL is required")?;
        if backend.transport.endpoint.is_empty() || backend.transport.deployment.is_empty() {
            return Err("CODEX_MAF_FOUNDRY_ENDPOINT and CODEX_MAF_DEPLOYMENT are required".into());
        }
        let (attempted, has_process, previous_context) = {
            let state = backend.state.lock().await;
            (
                state.attempted,
                state.process.is_some(),
                state.context.clone(),
            )
        };
        if attempted && !has_process {
            return Err(
                "MAF session ended after failure or interruption; start a new thread".into(),
            );
        }
        if !attempted && session.clone_history().await.raw_items().any(|item| {
            matches!(item, ResponseItem::Message { role, .. } if role == "user" || role == "assistant")
        }) {
            return Err("MAF only supports new threads; history conversion is not implemented".into());
        }
        let snapshot = Arc::new(
            HostSnapshotBridge::new(session.thread_id.to_string(), context.skills_snapshot())
                .await?,
        );
        let selected_context = (
            context.model_info().slug.clone(),
            context.config.cwd.to_path_buf(),
            serde_json::to_vec(&snapshot.snapshot().entries)
                .map_err(|_| "invalid skill snapshot")?,
        );
        if previous_context
            .as_ref()
            .is_some_and(|previous| previous != &selected_context)
        {
            return Err(
                "MAF model, workspace, or skill selection changed; start a new thread".into(),
            );
        }
        if selected_context.0 != backend.transport.deployment {
            return Err("The selected TUI model must match CODEX_MAF_DEPLOYMENT".into());
        }
        let mut accepted = Vec::new();
        for item in &input {
            let text = input_text(item)?;
            if !run_hooks_and_record_inputs(
                &session,
                &context,
                context.model_info(),
                std::slice::from_ref(item),
                PersistContext::Standard,
            )
            .await
            {
                accepted.push(text);
            }
        }
        if accepted.is_empty() {
            return Ok(None);
        }
        let text = accepted.join("\n");
        let native = native_read_context(&session, Arc::clone(&context), &cancellation).await?;
        let existing_process = {
            let mut state = backend.state.lock().await;
            state.context = Some(selected_context);
            state.attempted = true;
            state.process.take()
        };
        let mut process = match existing_process {
            Some(process) => process,
            None => {
                session.record_conversation_items(&context, context.model_info(), &[ResponseItem::Message {
                    id: Some(ResponseItemId::new("maf_backend")), role: "developer".into(),
                    content: vec![ContentItem::InputText {
                        text: "This thread uses the experimental MAF runtime. Resume and backend conversion are not supported.".into(),
                    }], phase: None, internal_chat_message_metadata_passthrough: None,
                }]).await;
                MafProcess::start(assembly, snapshot).await?
            }
        };
        let output = match process
            .run_native_read(
                &context.sub_id,
                &text,
                &native,
                |request| backend.transport.respond(request),
                |request| observe_native_tool(Arc::clone(&session), Arc::clone(&context), request),
            )
            .await
        {
            Ok(output) => output,
            Err(error) => {
                process.terminate().await;
                return Err(error);
            }
        };
        session
            .record_response_item_and_emit_turn_item(
                &context,
                context.model_info(),
                ResponseItem::Message {
                    id: Some(ResponseItemId::new("maf")),
                    role: "assistant".into(),
                    content: vec![ContentItem::OutputText {
                        text: output.clone(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
            )
            .await;
        backend.state.lock().await.process = Some(process);
        Ok(Some(output))
    };
    match operation.or_cancel(&cancellation).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => {
            finish_read_tool(&session, &context, false, &error).await;
            let process = {
                let mut state = backend.state.lock().await;
                state.attempted = true;
                state.process.take()
            };
            if let Some(mut process) = process {
                process.terminate().await;
            }
            Err(std::io::Error::other(format!("MAF: {error}")).into())
        }
        Err(_) => {
            finish_read_tool(&session, &context, false, "MAF tool interrupted").await;
            let process = {
                let mut state = backend.state.lock().await;
                state.attempted = true;
                state.process.take()
            };
            if let Some(mut process) = process {
                process.terminate().await;
            }
            Err(std::io::Error::other("MAF turn interrupted; start a new thread").into())
        }
    }
}

async fn native_read_context(
    session: &Arc<Session>,
    turn: Arc<TurnContext>,
    cancellation: &CancellationToken,
) -> Result<MafNativeReadContext, String> {
    if !session
        .tool_policy
        .allows(&ToolName::plain("codex_read_file"))
    {
        return Err("native read tool is disabled by session policy".into());
    }
    if session.hooks().has_tool_hooks() {
        return Err("native read does not yet support configured tool hooks".into());
    }
    let step = session
        .capture_step_context(turn, cancellation)
        .await
        .map_err(|_| "native read environment capture failed")?;
    let environment = crate::tools::handlers::resolve_tool_environment(&step.environments, None)
        .map_err(|_| "native read environment is ambiguous")?
        .ok_or("native read environment is unavailable")?;
    let policy = environment
        .sandbox_context(None)
        .permissions
        .file_system_sandbox_policy();
    if environment.environment.is_remote()
        || policy.kind == codex_protocol::permissions::FileSystemSandboxKind::ExternalSandbox
        || !policy.has_full_disk_read_access()
    {
        return Err("native read currently requires a local full-disk-read profile; restricted roots, deny rules and external sandboxes are not yet mapped".into());
    }
    Ok(MafNativeReadContext {
        cwd: environment
            .cwd()
            .to_abs_path()
            .map_err(|_| "native read requires a local cwd")?
            .to_string_lossy()
            .into_owned(),
        read_roots: vec!["/".into()],
    })
}

async fn observe_native_tool(
    session: Arc<Session>,
    turn: Arc<TurnContext>,
    request: MafToolRequest,
) -> Result<String, String> {
    if request.name != "codex_read_file"
        || request.turn_id != turn.sub_id
        || !session.tool_policy.allows(&ToolName::plain(&request.name))
    {
        return Err("MAF tool is not allowed".into());
    }
    let arguments =
        serde_json::to_string(&request.arguments).map_err(|_| "invalid tool arguments")?;
    if request.r#type == "nativeToolCompleted" {
        let result = request.result.ok_or("native result missing")?;
        let payload = ToolPayload::Function { arguments };
        let response = FunctionToolOutput::from_text(result.text.clone(), Some(result.success))
            .to_response_item(&request.call_id, &payload)
            .into();
        session
            .record_conversation_items(&turn, turn.model_info(), &[response])
            .await;
        finish_read_tool(&session, &turn, result.success, &result.text).await;
        return Ok(String::new());
    }
    if request.r#type != "nativeToolStarted" || request.result.is_some() {
        return Err("invalid native tool event".into());
    }
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[ResponseItem::FunctionCall {
                id: Some(ResponseItemId::new("maf_tool")),
                name: request.name.clone(),
                namespace: None,
                arguments: arguments.clone(),
                call_id: request.call_id.clone(),
                encrypted_function_args: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;
    let item = DynamicToolCallItem {
        id: request.call_id.clone(),
        namespace: None,
        tool: request.name.clone(),
        arguments: request.arguments,
        status: DynamicToolCallStatus::InProgress,
        content_items: None,
        success: None,
        error: None,
        duration: None,
    };
    thread_state(&session).state.lock().await.active_tool = Some(item.clone());
    session
        .emit_turn_item_started(&turn, &TurnItem::DynamicToolCall(item))
        .await;
    Ok(String::new())
}

async fn finish_read_tool(session: &Session, turn: &TurnContext, success: bool, text: &str) {
    let pending = thread_state(session).state.lock().await.active_tool.take();
    let Some(mut item) = pending else {
        return;
    };
    item.status = if success {
        DynamicToolCallStatus::Completed
    } else {
        DynamicToolCallStatus::Failed
    };
    item.success = Some(success);
    item.error = (!success).then(|| text.to_string());
    item.content_items = Some(vec![DynamicToolCallOutputContentItem::InputText {
        text: text.into(),
    }]);
    session
        .emit_turn_item_completed(turn, TurnItem::DynamicToolCall(item))
        .await;
}

fn input_text(input: &TurnInput) -> Result<String, String> {
    let TurnInput::UserInput { content, .. } = input else {
        return Err("MAF currently accepts only user text input".into());
    };
    let mut parts = Vec::new();
    for item in content {
        match item {
            UserInput::Text { text, .. } => parts.push(text.as_str()),
            _ => {
                return Err(
                    "MAF images, attachments and explicit skill references are not supported yet"
                        .into(),
                );
            }
        }
    }
    Ok(parts.join("\n"))
}
