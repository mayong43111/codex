# MAF Agent runtime replacement: implementation stages

## Goal and ownership

Replace the Codex Agent, not merely add MAF as a plugin to the Rust Agent.
MAF must own model/tool iteration and decide when a turn completes. Keep Codex
clients, skill assets, tool behavior, approval/sandbox guarantees, and historical
data. The user changed the tool migration direction on 2026-10-03: implement
tools natively in .NET and let MAF invoke them directly, rather than extending
Rust tool callbacks. Rust temporarily retains the client/host boundary, model
transport, and persistence. The ordinary Rust Agent remains the default outside
the explicit MAF launcher. Never run both Agent loops for one turn.

The [earlier investigation](maf-dotnet-migration.md) and phase-2 snapshot bridge
remain useful infrastructure. The sequence below supersedes that document's old
ordering of skill/script replacement before Agent-loop replacement.

```text
Codex client and host
  -> one selected Agent runtime per session
     -> existing Rust Agent (default)
     -> MAF Agent (.NET, experimental)
        -> model transport owned by the host
        -> native .NET AIFunction tools and restricted .NET execution
        -> existing Rust skill snapshot reader (not yet migrated)
  -> existing client events and authoritative history
```

## Stage 3A: MAF-owned read-only turn

Status: implemented and verified with the deterministic Rust test host.

Hypothesis: the real MAF `ChatClientAgent.RunAsync` can request a skill-loading
function, invoke the existing `AgentSkillsSource`, receive content through the
Rust `HostSnapshotBridge`, and issue a subsequent model request containing the
tool result, without entering the Rust Agent turn loop.

Implement this in the existing .NET bridge, behind an explicit process mode:

1. Retain the phase-2 catalog/load protocol and default offline mode.
2. Add a narrow model-transport callback over the same private host pipes. MAF
   owns iteration; the transport must not implement a second Agent loop.
3. Let an explicit run command invoke MAF with the current session and skill
   provider. Do not implement model-selected tools with a handwritten dispatcher.
4. Preserve snapshot authority checks, bounded messages, cancellation/deadlines,
   response correlation, and fail-closed channel behavior.
5. Test with deterministic model responses supplied by the Rust test host. These
   are transport fixtures, not proof of a live model or production integration.

Acceptance: a cross-process test observes a first model request, an actual MAF
skill-function invocation, a read authorized by the original Rust provider, a
second model request containing the exact tool result, and the final MAF reply.
Also test direct completion, host/model errors, invalid response identities,
iteration limits, and session isolation. Existing phase-2 tests must still pass.

This step reuses a Codex skill reader, not the full Core tool registry. It must
not be advertised as shell/tool-policy parity or as a user-facing backend switch.

### Implemented components and ownership

- [Program](../codex-dotnet/SkillsBridge/Program.cs): `--agent` creates one MAF
  session and invokes `ChatClientAgent.RunAsync` for each valid `run` command.
  No arguments retain the phase-2 offline behavior; other argument combinations
  are rejected. There is no hand-written model/tool iteration loop.
- [HostModelClient](../codex-dotnet/SkillsBridge/HostModelClient.cs): an
  `IChatClient` transport callback, not another Agent. It serializes MAF's
  messages and selected tool schema, receives one model reply, and returns SDK
  `ChatResponse`/`FunctionCallContent` objects. MAF's default function-invocation
  middleware executes the skill function and requests the next model response.
- [CodexSkillsSource](../codex-dotnet/SkillsBridge/CodexSkillsSource.cs): reused
  unchanged. The actual skill content still comes from the existing Rust
  `HostSnapshotBridge` and `HostSkillProvider` authorization path.
- [Tests](../codex-rs/ext/skills/src/maf_tests.rs): reuse the phase-2 child-process
  harness. Deterministic replies emulate only model transport, not Agent control
  flow. No Rust Agent turn loop is called in these tests.

MAF 1.23.0 requires approval for `load_skill` by default. The experimental mode
sets only `DisableLoadSkillApproval = true`: it delegates this pre-authorized
snapshot body read to Rust. It does not auto-approve arbitrary tools, resources,
or scripts. Stage 3A advertises only `load_skill`; stage 3C below adds an explicit
host-provided read-tool allowlist. Other model-selected names are rejected. The
default offline mode keeps its previous settings.

A pending approval or unhandled function call must not become a successful
completion: non-text final Agent messages terminate the experimental channel.
The experiment does not implement an approval UI or approval response protocol.

### Model callback and run protocol

Keep the phase-2 version-1 snapshot handshake, catalog, read/readResult, and close
messages. All traffic uses one private, sequential, newline-delimited JSON pipe
pair. The additions are:

```text
Host -> .NET: run {version, type, requestId, snapshotId, text}
.NET -> Host: modelRequest {version, type, requestId, snapshotId, turnId, messages, tools}
Host -> .NET: modelResult {version, type, requestId, snapshotId, turnId, text, calls, error}
  [MAF may invoke load_skill -> existing read/readResult exchange]
  [MAF may issue another modelRequest with the function result]
.NET -> Host: completed {version, type, requestId, snapshotId, text}
```

The run command's `requestId` becomes `turnId` for model callbacks. Model request
IDs are separately generated and increase across the process lifetime. Responses
must match version, type, model request ID, snapshot ID, and turn ID.

`messages` contains `{role, contents}`. Supported content items are:

- `{type: "text", text}`
- `{type: "functionCall", callId, name, arguments}`
- `{type: "functionResult", callId, result}`

`tools` contains `{name, description, parameters}` with the actual MAF JSON
schema. Host model adapters must use that schema rather than hardcoding argument
names. `modelResult.calls` is a required array of `{callId, name, arguments}`;
there can be zero or one call. Call IDs must be nonblank, at most 128 characters,
and unique within the turn. `text` can be null when a function call is present.
`error` is required and null for successful replies; a non-null error aborts the
turn with a bounded error code, without relaying private provider error details.

This is a deliberately limited transport contract, not an OpenAI or full Codex
model protocol. Images, reasoning-specific content, streaming, server-managed
conversation IDs, usage accounting, provider-specific options, and arbitrary
tool types are not mapped. The production model adapter belongs to stage 3B.

### Limits and failure behavior

- Run input and each model text reply: at most 256 KiB UTF-8.
- Each JSON frame, including serialized conversation history: at most 2 MiB.
- A run: at most eight model requests and a 60-second overall deadline.
- Each model transport response: a ten-second deadline. Existing skill-read
  deadlines remain unchanged.
- Failed model transport, malformed or mismatched responses, exhausted iteration
  limits, or a broken skill channel terminate the process. A host must not replay
  the turn automatically or reuse that session after failure.
- A normal Rust reader denial is returned to MAF as a failed function result;
  MAF can then produce an explanatory answer. It does not gain a fallback reader.
- Valid consecutive turns share the in-memory MAF session. Separate child
  processes do not share that history. No persistence or restart/resume contract
  has been introduced.
- Cancellation uses the existing terminal channel behavior: close the host pipe
  or terminate the child. There is no concurrent cancel message while a model
  response is pending and no Codex interrupt-event adapter yet.

## Stage 3B: selected Codex host integration

Status: implemented as an opt-in Linux TUI/app-server pilot for new text
sessions. Real .NET/Foundry skill iteration and TUI execution have passed.

- Retain the existing TUI and add explicit Rust/MAF backend selection in its host.
- Launch and supervise one .NET process per authorized session; propagate close,
  disconnect, interruption, and initialization failures.
- Connect existing model configuration/authentication through the transport
  adapter without sending credentials to model-generated tool arguments.
- Translate MAF progress/completion into the selected existing event contract.
- Bind backend choice for the lifetime of the session. Failed MAF turns are not
  silently replayed by the Rust Agent, especially after side effects.

Acceptance: an opted-in user request reaches MAF, uses a real configured model,
and returns through the existing host. The Rust Agent loop is not invoked for
that request. The default Rust path remains unchanged. No historical session
conversion is implied by creating a new MAF session.

### Selected TUI and process boundary

The [TUI app-server targets](../codex-rs/tui/src/lib.rs) already support embedded,
local-daemon, and remote app-server connections. Preserve this client boundary;
do not embed a second model/tool loop in the TUI widget. The current
[turn processor](../codex-rs/app-server/src/request_processors/turn_processor.rs)
loads a Rust thread before starting a turn. Backend selection therefore needs a
real host/thread abstraction, not simply launching .NET from the input handler.

The implemented path is TUI -> app-server -> existing Core thread/task lifecycle
-> real .NET MAF process -> Foundry transport / authorized Rust skill reads.
[Task dispatch](../codex-rs/core/src/tasks/mod.rs) selects MAF before invoking
the Rust task implementation; it does not enter `RegularTask::run` or the Rust
`run_turn` model/tool loop. Core remains the lifecycle/storage host, not the
Agent decision maker. The existing thread-local extension state owns the MAF
backend selection and process. No parallel UI or separate thread database was
introduced.

### Implemented runtime and startup

- [Thread adapter](../codex-rs/core/src/tasks/maf.rs) binds environment-selected
  backend/model settings to the thread, lazily starts .NET, reuses its Agent
  session after successful turns, records accepted inputs through existing
  hooks, and publishes assistant items through existing persistence/events.
- [Process and model transport](../codex-rs/ext/skills/src/maf_runtime.rs) uses
  bounded private JSON pipes and the original snapshot reader. Azure CLI `rest`
  performs each model HTTP request using its existing Entra identity; credentials
  are never returned to MAF or put in model-generated arguments. Request bodies
  go through stdin rather than command-line arguments. There is no transport
  retry or fallback Agent loop.
- [Input admission](../codex-rs/core/src/codex_thread.rs) maps MAF input to the
  existing atomic start-if-idle operation. Steering and internal continuation
  modes are rejected instead of accepting input that MAF cannot consume.
- The `maf_backend_` history marker prevents a previously executed MAF thread
  from silently resuming on Rust. Reading old history is possible; continuing
  its execution is deliberately rejected until state conversion is implemented.
- Cancellation closes the active or retained process and invalidates that MAF
  session. A model failure does the same. Start a new thread after either event;
  do not replay a partly executed turn.

From the repository root, after the builds below:

```sh
export CODEX_MAF_FOUNDRY_ENDPOINT="https://YOUR_RESOURCE.openai.azure.com/openai/v1/responses"
export CODEX_MAF_DEPLOYMENT="gpt-5.4"
bash scripts/maf_tui.sh
```

Replace `YOUR_RESOURCE` with your Azure resource name and select your deployment.

The [launcher](../scripts/maf_tui.sh) sets `CODEX_AGENT_BACKEND=maf`, selects the
matching model/provider and `reasoning=none`, and forces `--no-daemon` so an
already running Rust daemon cannot accidentally receive the request. It does
not rewrite user config or change the Azure default subscription. The ordinary
Codex command retains its default Rust backend.

Optional launcher overrides:

| Variable | Default / meaning |
| --- | --- |
| `CODEX_MAF_BRIDGE_DLL` | Built Debug `Codex.SkillsBridge.dll` in this repository |
| `CODEX_MAF_AZ` | `$HOME/.local/share/codex-tools/azure-cli/bin/az` |
| `CODEX_MAF_CODEX_BIN` | `codex-rs/target/debug/codex` |
| `CODEX_MAF_FOUNDRY_ENDPOINT` | Required HTTPS `*.openai.azure.com/openai/v1/responses` endpoint |
| `CODEX_MAF_DEPLOYMENT` | Required deployment name; must match the selected TUI model |

The real host sets `CODEX_MAF_LIVE_HOST=1` only on its .NET child, allowing
45 seconds per model response. The offline bridge retains its ten-second
deadline. The 60-second turn limit, eight-model-request limit, two-MiB frame
limit, and read-only snapshot authorization remain in force. Foundry requests
use `store=false`, `parallel_tool_calls=false`, `reasoning.effort=none`, and
at most 2048 output tokens. Other reasoning settings are rejected explicitly.

### Scope and remaining parity work

This is a runnable replacement-loop pilot, not complete Codex feature parity.
Stage 3B delivered regular user text and MAF's `load_skill`; stage 3C introduced
a Rust text reader, superseded by the native .NET reader in stage 3D. Images, attachments,
explicit skill-reference input objects, dynamic tools, structured output,
review/compaction/user-shell tasks, streaming, and general tool execution are
rejected or not exposed. Changing model, workspace, or skill metadata requires
a new thread. No MAF execution resume, historical session conversion, complete
Codex instruction/context rendering, usage accounting, or provider-specific
model option parity is claimed.

The transport currently targets public Azure OpenAI endpoints on Linux and uses
Azure CLI per HTTP request. It is not a general model-provider client, and it
does not replace approvals with exec-server access. Stage 3C records the earlier
bridge experiment; stage 3D is the current tool implementation direction.

### Foundry connection verified on 2026-10-03

- Azure CLI's existing login was reused; no API key or access token was displayed.
- The deployment was discovered using explicit subscription arguments without
  changing the default. Environment-specific resource identifiers are omitted.
- Deployment and model: `gpt-5.4`, version `2026-03-05`, state `Succeeded`.
- Responses endpoint shape:
  `https://YOUR_RESOURCE.openai.azure.com/openai/v1/responses`.
- Entra authentication through Azure CLI, token resource
  `https://cognitiveservices.azure.com/`, successfully executed a minimal Responses
  request. With `store: false`, `reasoning.effort: none`, and a 32-token output
  limit, the response was `status: completed`, model `gpt-5.4`, text `OK`.
- The request contained no workspace content. No cloud resources, roles, keys,
  or default subscription settings were changed.

Azure CLI was absent from PATH and common installation locations. An isolated
installation now exists at `$HOME/.local/share/codex-tools/azure-cli/bin/az`;
future CLI-based credential providers must receive the corresponding PATH.
There was no current Codex user/workspace model configuration or related model
environment variable to reuse. Host configuration still needs to be connected
to this verified deployment; it must not hardcode these environment-specific
values in the Agent implementation.

That initial Azure CLI probe verified endpoint and identity access only.
Subsequent .NET, app-server, and TUI integration checks are recorded below.

## Stage 3C: existing Codex tool execution

Status: historical bridge experiment, implemented and verified. Superseded in
the running MAF host by stage 3D-1 below. The Rust reader is now compiled only
for comparison tests; this section describes the previous execution path.

This checkout has no standalone workspace text-reading handler. Its
`notes.read_file` tool addresses private model notes, not workspace files. With
explicit user approval, this step adds a thin `codex_read_file` adapter rather
than misusing notes or exposing shell as a supposedly read-only tool.

### Implemented read-tool path

- [Read adapter](../codex-rs/core/src/tasks/maf/read_file.rs) reuses the same
  primary-environment resolver and sandboxed filesystem APIs as `view_image`.
  It does not use ambient .NET/Rust file reads, spawn shell commands, grant
  permissions, or fall back when the sandbox is unavailable.
- [Core host](../codex-rs/core/src/tasks/maf.rs) exports the real typed schema
  only when the session tool policy permits it. Execution goes through the
  policy-aware `ToolRegistry`, pre/post hooks, and lifecycle notifications.
  Original function-call/result history and native tool item events are retained.
- The MAF `AIFunction` sends a `toolRequest`; Rust requires an exact match to
  the pending model call's name, arguments, and call ID. Snapshot, turn, and
  request identity must also match. The host supplies all environment and
  permission context. Arguments cannot contain a policy or command override.
- Only `{path: string}` is accepted. Relative paths resolve from the primary
  environment cwd; permitted absolute paths also work. Access is governed by
  the existing environment policy, not an invented workspace-root restriction.
- Only regular UTF-8 text files without NUL bytes, at most 128 KiB, are returned.
  Reads use the bounded-chunk streaming filesystem API and honor cancellation.
  Directories, binary/oversized files, inaccessible paths, and missing sandbox
  support return failures rather than partial successful content.
- Tool callbacks have an eight-second host deadline and ten-second .NET wait;
  the existing 60-second turn budget still applies. Serialized tool results
  remain bounded to 256 KiB, and transport frames to 2 MiB.
- Ordinary read denials/errors become failed function results that MAF may
  explain. Protocol errors, timeouts, and interruption invalidate the session;
  pending tool items are failed and calls are never automatically replayed.

The version-1 `run` command now optionally includes `tools`, containing the
host-approved `{name, description, parameters}` definitions. Omitting it retains
the stage-3A skill-only behavior. The additional messages are:

```text
.NET -> Host: toolRequest {version, type, requestId, snapshotId, turnId, callId, name, arguments}
Host -> .NET: toolResult {version, type, requestId, snapshotId, turnId, callId, result}
```

Use the existing launcher and ask to read a specific text file. No new opt-in
environment variables or user configuration edits are required. This is not
general dynamic-tool support, a new approval UI, or write/shell parity.

### Superseded bridge plan

- Export its real schema and invoke its owning Rust handler with host-provided
  context, never a model-provided approval or sandbox policy.
- Preserve permission denial, cancellation, bounded output, and call identity.
- Extend to side effects only after approval and interruption contracts pass.
- Do not use exec-server access as a substitute for Core approval orchestration.

Acceptance: MAF selects the tool, the existing Rust execution path enforces policy,
and MAF consumes its result. Denied actions never execute. No duplicate execution
on timeout, reconnect, or fallback.

## Stage 3D: .NET-native tool migration

Status: 3D-1 implemented and verified. Do not continue the planned 3C-2 Rust
`apply_patch` adapter. Subsequent tools must execute in .NET, with explicit
equivalence tests against the corresponding Codex behavior.

### 3D-1: native read validation

MAF now invokes the .NET-owned `codex_read_file` function directly. Its schema
is defined in [NativeReadFile](../codex-dotnet/SkillsBridge/NativeReadFile.cs).
The function starts a short-lived .NET worker, not a Rust helper, shell command,
or exec-server request. The existing name is a migration validation tool, not
proof that an upstream Codex tool has already been ported in full.

- The worker uses Linux Landlock ABI 3 or newer, restricted to Linux x64.
  It applies read capabilities and denies filesystem writes and execution on
  the synchronous I/O thread. This is a fixed reader, not a general sandbox for
  arbitrary .NET code or a replacement for the future shell/network sandbox.
- The worker environment is cleared, diagnostics are disabled, and parent death
  triggers termination. Cancellation/timeout kills the worker; there is no retry
  or fallback Rust read. Worker reads have an eight-second deadline.
- Paths are resolved from the host-provided cwd. A pinned file descriptor is
  checked before reopening for read, so symlink targets remain subject to the
  kernel policy. Only ordinary files on ext-family, overlay, tmpfs, XFS, and
  Btrfs filesystems are supported. Pseudo-filesystems such as `/proc` and `/sys`,
  and all unmapped filesystem types, are rejected.
- UTF-8 bytes, including BOM/newlines, are retained. NUL, invalid UTF-8,
  directories, files over 128 KiB, and inaccessible files return failures.
- The Core host currently admits only local profiles with full-disk read
  access, such as the normal read-only profile. It passes read roots as trusted
  configuration, never model arguments. Restricted-root/deny-rule projections,
  external sandboxes, remote environments, and configured tool hooks are
  explicitly rejected rather than silently bypassed. The .NET worker's narrower
  read-root enforcement is tested directly, but general Codex policy projection
  is not yet implemented.
- Core no longer registers or executes the file tool. It observes start/result
  events, records original call/result history, and emits existing TUI items.
  Model transport and `load_skill` still use their existing Rust host paths;
  they are not claimed as native .NET migrations in this step.

`run.nativeRead` carries `{cwd, readRoots}`. It cannot be combined with legacy
host-executed `tools`. The native protocol has no tool execution request/reply:

```text
.NET -> Host: nativeToolStarted {version, type, requestId, snapshotId, turnId, callId, name, arguments}
.NET -> Host: nativeToolCompleted {version, type, requestId, snapshotId, turnId, callId, name, arguments, result: {success, text}}
```

Both events must match the pending model call; the host does not send a tool
result back. A `toolRequest` in native mode fails with
`rust_tool_execution_disabled`. Legacy bridge messages remain only for their
existing regression fixtures, with no automatic fallback from the running host.

The launcher and Foundry configuration are unchanged. Start a new MAF thread;
the file reader now executes in .NET. This step proves the native execution
architecture and the tested ordinary-file behavior, not full 1:1 parity.
Diagnostic wording, pseudo-file behavior, complex policies, tool hooks, remote
filesystems, other platforms, and approvals for side effects remain gaps.

### Next native-tool stages

1. 3D-2: select one upstream Codex tool, such as `apply_patch`, and write its
   contract matrix: schema, parsing, outputs/errors, diffs/events, authorization,
   approval, cancellation, and replay rules. Implement and test it in .NET.
2. 3D-3: migrate tools one at a time. Use shared fixtures and explicit backend
   ownership. Never run both implementations of a side-effecting tool against
   the same live workspace merely to compare them.
3. 3D-4: remove superseded Rust tool dependencies only after parity gates and
   rollback/data compatibility are established. Keep the TUI/host interfaces
   until their independent migration is approved.

## Stage 4: production parity and rollout

- Add streaming, budgets, context management, memory/MCP boundaries, durable
  events, and restart/resume compatibility incrementally.
- Characterize retained historical data before changing persistence formats.
- Expand supported tools and skill resources without rescanning arbitrary paths.
- Roll out only after focused compatibility gates pass; remove superseded Rust
  Agent code only when there are no required callers and rollback is agreed.

## Verification ledger

- Phase 2 baseline: 320 focused tests passed, including 14 bridge tests.
- Stage 3A: ten new cross-process tests passed. They cover the real MAF skill
  loop and exact source-body containment in the tool result, session continuity,
  process isolation, direct completion, unchanged default mode, invalid inputs,
  model identity/schema/error cases, failed Rust reads, broken skill responses,
  iteration limits, model timeout, and disconnect.
- Combined bridge suite: 24 passed. Full focused regression with `codex-skills`,
  `codex-skills-extension`, and `codex-tools`: **330 passed, 0 skipped**.
- Locked .NET restore and build passed without new dependencies.
- Module-scoped Rust Clippy and editor diagnostics passed. Updated local
  documentation links were checked.
- Stage 3B: real Rust process host -> .NET MAF -> Foundry test passed.
- Real app-server checks passed: MAF loaded an isolated fixture skill through
  the Rust reader, returned a random token only present in its body, emitted
  normal assistant item events, and retained the answer across a second turn.
  Interruption invalidated the session; a subsequent request failed without
  replay. Restarting with Rust selected still rejected the persisted MAF thread.
- Original TUI passed in a real pseudo-terminal: a supplied text prompt produced
  a MAF-prefixed assistant item with the real Foundry reply in normal Codex
  history. This is not a full visual/interactive TUI regression suite.
- Updated offline regression: **334 passed**, with the one live Foundry test
  explicitly excluded. Core/skills-extension Clippy and the full `codex-cli`
  executable build passed; .NET build passed.
- Stage 3C-1 offline: **342 passed**, including four new real-.NET tool bridge
  tests and four Core adapter tests. Coverage includes exact file bytes, size
  and text limits, sandbox propagation without ambient fallback, cancellation,
  result identity mismatch, denied results, and unadvertised/duplicate calls.
  Core/skills-extension Clippy and .NET build passed.
- Stage 3C-1 live: full CLI build and real app-server/TUI file reads passed.
  A random token available only in the synthetic workspace file reached the
  final MAF answer. Native successful/failed tool events were observed. A
  separate existing file with mode `000` was denied, called only once, and its
  content never appeared in events. This is a filesystem permission-denial
  check, not a complete sandbox-policy matrix; the Core test separately verifies
  environment sandbox propagation and no ambient fallback. Existing interruption
  and backend-resume protections passed again. These live checks require a
  non-root Linux user and remain explicit rather than part of offline tests.
- Stage 3D-1: **528 focused tests passed**, including hooks-library regression,
  native MAF event-only invocation/cancellation, and Rust/.NET file comparisons.
  After clearing the worker environment and rejecting pseudo-filesystems,
  **five native-focused tests passed again**, including the new `/proc` denial
  test. Successful text is compared exactly; failure classes are compared, not
  byte-identical OS diagnostic strings. Core/skills-extension/hooks Clippy,
  .NET build, and full CLI build passed.
- Final native-mode live app-server and original TUI checks passed after worker
  hardening: exact isolated file token, successful/failed native tool events,
  mode-000 denial without leak or retry, interruption, and old-thread protection.
- Native write/shell tools, broader policy parity, and stage 4 remain pending.

No live model calls, credentials, default runtime changes, script execution, or
production backend claims are part of the stage-3A deterministic acceptance test.

Reproduce from the repository root:

```sh
dotnet restore codex-dotnet/SkillsBridge/Codex.SkillsBridge.csproj --locked-mode
dotnet build codex-dotnet/SkillsBridge/Codex.SkillsBridge.csproj --no-restore
export CODEX_MAF_BRIDGE_DLL="$PWD/codex-dotnet/SkillsBridge/bin/Debug/net10.0/Codex.SkillsBridge.dll"
CARGO_BUILD_JOBS=2 just test --locked -p codex-skills-extension --lib --run-ignored all -E '(test(maf::tests::) | test(maf_runtime::tests::)) & not test(live_foundry_)'
CARGO_BUILD_JOBS=2 just test --locked -p codex-skills -p codex-skills-extension -p codex-tools --lib --run-ignored all -E 'not test(live_foundry_)'
CARGO_BUILD_JOBS=2 just test --locked -p codex-core --lib --run-ignored all -E 'test(tasks::maf::)'
CARGO_BUILD_JOBS=2 cargo build --manifest-path codex-rs/Cargo.toml --locked -p codex-cli --bin codex
```

The host starts `dotnet "$CODEX_MAF_BRIDGE_DLL" --agent` with private pipes,
sends the initial snapshot, services model/skill-read callbacks, and observes
native tool events until completion. It does not execute the native file tool.

Explicit live checks, after setting the environment variables above and
`CODEX_MAF_AZ` to the installed CLI path:

```sh
CARGO_BUILD_JOBS=2 just test --locked -p codex-skills-extension --lib --run-ignored all -E 'test(live_foundry_runs_real_dotnet_host)'
python3 scripts/maf_host_smoke.py
python3 scripts/maf_host_smoke.py --tui
```

The [live host checks](../scripts/maf_host_smoke.py) create a temporary Codex home,
workspace, synthetic skill, and an independently generated file token. The
current checks exercise file reads through app-server and the original TUI,
including native success/failure tool events. They send only test content, incur small model
usage, and close their host processes afterward. They are never implicit parts
of the offline regression command. No keys, role assignments, deployments, or
default subscription settings are changed by these checks.