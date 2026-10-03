use codex_async_utils::OrCancelExt;
use codex_exec_server::GetMetadataOptions;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use futures::StreamExt;
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;

pub(super) const NAME: &str = "codex_read_file";
const MAX_BYTES: usize = 128 * 1024;

pub(super) struct ReadFile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    path: String,
}

fn failed(message: impl Into<String>) -> FunctionCallError {
    FunctionCallError::RespondToModel(message.into())
}

impl ToolExecutor<ToolInvocation> for ReadFile {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(NAME)
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::Function(ResponsesApiTool {
            name: NAME.into(),
            description: "Read a UTF-8 text file in the primary environment using Codex filesystem permissions. Relative paths resolve from the working directory. Files must be at most 128 KiB. No writes or commands are supported.".into(),
            strict: false,
            defer_loading: None,
            parameters: JsonSchema::object(
                BTreeMap::from([("path".into(), JsonSchema::string(None))]),
                Some(vec!["path".into()]), Some(false.into())),
            output_schema: None,
        })
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        false
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        let cancellation = invocation.cancellation_token.clone();
        Box::pin(async move {
            let operation = async move {
                let ToolPayload::Function { arguments } = &invocation.payload else {
                    return Err(failed("unsupported read-file payload"));
                };
                let arguments: Arguments = parse_arguments(arguments)?;
                if arguments.path.is_empty() || arguments.path.contains('\0') {
                    return Err(failed("invalid file path"));
                }
                let environment =
                    resolve_tool_environment(&invocation.step_context.environments, None)?
                        .ok_or_else(|| failed("file reading is unavailable in this environment"))?;
                let path = environment
                    .cwd()
                    .join(&arguments.path)
                    .map_err(|_| failed("invalid file path"))?;
                let sandbox = environment.sandbox_context(None);
                let filesystem = environment.environment.get_filesystem();
                let metadata = filesystem
                    .get_metadata(&path, GetMetadataOptions::default(), Some(&sandbox))
                    .await
                    .map_err(|error| {
                        failed(format!("file metadata denied or unavailable: {error}"))
                    })?;
                if !metadata.is_file {
                    return Err(failed("only regular text files are supported"));
                }
                let mut stream = filesystem
                    .read_file_stream(&path, Some(&sandbox))
                    .await
                    .map_err(|error| failed(format!("file read denied or unavailable: {error}")))?;
                let mut bytes = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let chunk =
                        chunk.map_err(|error| failed(format!("file read failed: {error}")))?;
                    if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
                        return Err(failed("file exceeds 128 KiB"));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                let text =
                    String::from_utf8(bytes).map_err(|_| failed("file is not UTF-8 text"))?;
                if text.contains('\0') {
                    return Err(failed("binary files are not supported"));
                }
                Ok(Box::new(FunctionToolOutput::from_text(text, Some(true)))
                    as Box<dyn crate::tools::context::ToolOutput>)
            };
            operation
                .or_cancel(&cancellation)
                .await
                .map_err(|_| FunctionCallError::Fatal("file read interrupted".into()))?
        })
    }
}

impl CoreToolRuntime for ReadFile {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PermissionProfileSnapshot;
    use crate::environment_selection::TurnEnvironmentState;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::tools::context::ToolCallSource;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::permissions::FileSystemSandboxPolicy;
    use codex_protocol::permissions::NetworkSandboxPolicy;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    async fn invocation(path: &std::path::Path, restricted: bool) -> ToolInvocation {
        let (session, mut turn) = make_session_and_context().await;
        Arc::make_mut(&mut turn.config)
            .permissions
            .set_permission_profile(PermissionProfile::Disabled)
            .unwrap();
        let TurnEnvironmentState::Ready(environment) =
            &mut turn.initial_environments.environments[0]
        else {
            panic!("expected primary environment");
        };
        environment.config_mut().permission_profile =
            PermissionProfileSnapshot::legacy(if restricted {
                PermissionProfile::from_runtime_permissions(
                    &FileSystemSandboxPolicy::restricted(Vec::new()),
                    NetworkSandboxPolicy::Restricted,
                )
            } else {
                PermissionProfile::Disabled
            });
        let turn = Arc::new(turn);
        ToolInvocation {
            session: Arc::new(session),
            step_context: StepContext::for_test(Arc::clone(&turn)),
            turn,
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "read-test".into(),
            tool_name: ToolName::plain(NAME),
            source: ToolCallSource::Direct,
            payload: ToolPayload::Function {
                arguments: json!({"path": path}).to_string(),
            },
        }
    }

    #[tokio::test]
    async fn reads_exact_text_and_rejects_non_text_large_or_directory_inputs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("proof.txt");
        std::fs::write(&path, "host-only proof\n").unwrap();
        let result = ReadFile
            .handle(invocation(&path, false).await)
            .await
            .unwrap();
        assert_eq!(result.log_output(), "host-only proof\n");
        for contents in [vec![0xff], vec![0], vec![b'x'; MAX_BYTES + 1]] {
            std::fs::write(&path, contents).unwrap();
            assert!(
                ReadFile
                    .handle(invocation(&path, false).await)
                    .await
                    .is_err()
            );
        }
        assert!(
            ReadFile
                .handle(invocation(directory.path(), false).await)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn environment_sandbox_does_not_fall_back_to_ambient_reading() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("proof.txt");
        std::fs::write(&path, "must not be read").unwrap();
        let result = ReadFile.handle(invocation(&path, true).await).await;
        let Err(FunctionCallError::RespondToModel(message)) = result else {
            panic!("expected sandbox denial");
        };
        assert!(message.contains("sandbox"), "{message}");
        assert!(!message.contains("must not be read"));
    }

    #[tokio::test]
    async fn cancelled_read_does_not_return_contents() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("proof.txt");
        std::fs::write(&path, "must not be read").unwrap();
        let invocation = invocation(&path, false).await;
        invocation.cancellation_token.cancel();
        assert!(matches!(
            ReadFile.handle(invocation).await,
            Err(FunctionCallError::Fatal(_))
        ));
    }

    #[test]
    fn schema_and_arguments_allow_only_a_path() {
        assert!(matches!(ReadFile.spec(), ToolSpec::Function(spec) if spec.name == NAME));
        assert!(serde_json::from_value::<Arguments>(json!({"path": "README.md"})).is_ok());
        assert!(
            serde_json::from_value::<Arguments>(json!({"path": "README.md", "sandbox": false}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<Arguments>(json!({"path": "README.md", "command": "pwd"}))
                .is_err()
        );
    }

    async fn native_read(path: &std::path::Path, root: &std::path::Path) -> serde_json::Value {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new("dotnet")
            .arg(
                std::env::var_os("CODEX_MAF_BRIDGE_DLL").expect("build and select the .NET bridge"),
            )
            .arg("--native-read-worker")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let input = json!({"context": {"cwd": root, "readRoots": [root]}, "path": path});
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(format!("{input}\n").as_bytes())
            .await
            .unwrap();
        drop(stdin);
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    #[tokio::test]
    #[ignore = "requires built .NET bridge and Linux x64 Landlock ABI 3+"]
    async fn native_reader_matches_rust_text_and_rejection_contract() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("proof.txt");
        for contents in [
            Vec::new(),
            b"native proof\n".to_vec(),
            "\u{feff}Unicode \u{1f600}\r\n".as_bytes().to_vec(),
            vec![b'x'; MAX_BYTES],
            vec![b'x'; MAX_BYTES + 1],
            vec![0xff],
            vec![0],
        ] {
            std::fs::write(&path, contents).unwrap();
            let original = ReadFile.handle(invocation(&path, false).await).await;
            let native = native_read(&path, directory.path()).await;
            assert_eq!(native["success"], original.is_ok(), "{native}");
            if let Ok(original) = original {
                assert_eq!(native["text"], original.log_output());
            }
        }
        for rejected in [
            directory.path().to_path_buf(),
            directory.path().join("missing"),
        ] {
            assert!(
                ReadFile
                    .handle(invocation(&rejected, false).await)
                    .await
                    .is_err()
            );
            assert_eq!(
                native_read(&rejected, directory.path()).await["success"],
                false
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "requires built .NET bridge and non-root Linux x64 Landlock ABI 3+"]
    async fn native_reader_enforces_roots_symlinks_and_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = directory.path().join("proof.txt");
        let external = outside.path().join("secret.txt");
        std::fs::write(&path, "allowed").unwrap();
        std::fs::write(&external, "must not leak").unwrap();
        let link = directory.path().join("link.txt");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(
            native_read(&link, directory.path()).await["text"],
            ReadFile
                .handle(invocation(&link, false).await)
                .await
                .unwrap()
                .log_output()
        );
        assert_eq!(
            native_read(std::path::Path::new("proof.txt"), directory.path()).await["text"],
            "allowed"
        );
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&external, &link).unwrap();
        for denied in [&external, &link] {
            let native = native_read(denied, directory.path()).await;
            assert_eq!(native["success"], false);
            assert!(!native.to_string().contains("must not leak"));
        }
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0)).unwrap();
        assert!(
            ReadFile
                .handle(invocation(&path, false).await)
                .await
                .is_err()
        );
        assert_eq!(native_read(&path, directory.path()).await["success"], false);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[tokio::test]
    #[ignore = "requires built .NET bridge and Linux x64 Landlock ABI 3+"]
    async fn native_reader_rejects_process_information_filesystems() {
        for path in ["/proc/self/status", "/proc/self/environ"] {
            let result = native_read(std::path::Path::new(path), std::path::Path::new("/")).await;
            assert_eq!(result["success"], false);
            assert_eq!(result["text"], "native read does not support this filesystem");
        }
    }
}
