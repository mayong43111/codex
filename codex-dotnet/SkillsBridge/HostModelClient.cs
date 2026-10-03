using System.Text;
using System.Text.Json;
using Microsoft.Agents.AI;
using Microsoft.Extensions.AI;

namespace Codex.SkillsBridge;

internal sealed record ModelCall(string CallId, string Name, Dictionary<string, JsonElement> Arguments);
internal sealed record ModelReply(
    int Version, string Type, string RequestId, string SnapshotId, string TurnId,
    string? Text, ModelCall[] Calls, string? Error);

internal sealed class HostModelClient(
    JsonLines channel, string snapshotId, Func<bool> readerFaulted, TimeSpan? requestTimeout = null) : IChatClient
{
    internal const int MaxModelRequests = 8;
    private long sequence;
    private int requests;
    private string? turnId;
    private readonly HashSet<string> callIds = new(StringComparer.Ordinal);
    private readonly HashSet<string> hostToolNames = new(StringComparer.Ordinal);
    private ModelCall? pendingToolCall;
    private NativeReadContext? nativeRead;
    internal bool Faulted { get; private set; }

    internal IList<AITool> BeginTurn(string requestId, HostToolDefinition[]? definitions = null,
        NativeReadContext? nativeContext = null)
    {
        if (nativeContext is not null && definitions is { Length: > 0 })
            throw new BridgeException("mixed_tool_backends");
        nativeRead = nativeContext;
        if (nativeRead is not null) definitions = [NativeReadFile.Definition];
        turnId = requestId;
        requests = 0;
        callIds.Clear();
        hostToolNames.Clear();
        pendingToolCall = null;
        List<AITool> tools = [];
        foreach (var definition in definitions ?? [])
        {
            if (definition is null || definition.Name != "codex_read_file"
                || !hostToolNames.Add(definition.Name)
                || string.IsNullOrWhiteSpace(definition.Description)
                || definition.Parameters.ValueKind != JsonValueKind.Object)
            {
                throw new BridgeException("invalid_host_tools");
            }
            tools.Add(new HostFunction(this, definition));
        }
        return tools;
    }

    public async Task<ChatResponse> GetResponseAsync(
        IEnumerable<ChatMessage> messages, ChatOptions? options = null,
        CancellationToken cancellationToken = default)
    {
        try
        {
            if (Faulted || readerFaulted() || turnId is null || pendingToolCall is not null)
            {
                throw new BridgeException("channel_faulted");
            }
            if (++requests > MaxModelRequests)
            {
                throw new BridgeException("model_iteration_limit");
            }
            using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            deadline.CancelAfter(requestTimeout ?? TimeSpan.FromSeconds(10));
            string requestId = $"model-{++sequence}";
            var tools = options?.Tools?
                .Where(tool => tool is AIFunctionDeclaration function
                    && (function.Name == AgentSkillsProvider.LoadSkillToolName || hostToolNames.Contains(function.Name)))
                .Select(tool => tool is AIFunctionDeclaration function
                ? new { name = function.Name, description = function.Description, parameters = function.JsonSchema }
                : throw new BridgeException("unsupported_model_tool")).ToArray();
            await channel.WriteAsync(new
            {
                version = JsonLines.Version,
                type = "modelRequest",
                requestId,
                snapshotId,
                turnId,
                messages = messages.Select(message => new
                {
                    role = message.Role.ToString(),
                    contents = message.Contents.Select(ProjectContent).ToArray()
                }).ToArray(),
                tools
            }, deadline.Token);
            var response = await channel.ReadAsync<ModelReply>(deadline.Token);
            if (response.Version != JsonLines.Version || response.Type != "modelResult"
                || response.RequestId != requestId || response.SnapshotId != snapshotId
                || response.TurnId != turnId || response.Calls is null || response.Calls.Length > 1
                || (response.Text is not null && Encoding.UTF8.GetByteCount(response.Text) > JsonLines.MaxContentBytes))
            {
                throw new BridgeException("invalid_model_response");
            }
            if (response.Error is not null)
            {
                throw new BridgeException("model_request_failed");
            }
            if (response.Text is null && response.Calls.Length == 0)
            {
                throw new BridgeException("empty_model_response");
            }
            List<AIContent> contents = [];
            if (response.Text is not null)
            {
                contents.Add(new TextContent(response.Text));
            }
            foreach (var call in response.Calls)
            {
                if (call is null || string.IsNullOrWhiteSpace(call.CallId) || call.CallId.Length > 128
                    || !callIds.Add(call.CallId) || call.Arguments is null
                    || tools is null || !tools.Any(tool => tool.name == call.Name))
                {
                    throw new BridgeException("invalid_model_call");
                }
                contents.Add(new FunctionCallContent(call.CallId, call.Name,
                    call.Arguments.ToDictionary(argument => argument.Key, argument => (object?)argument.Value)));
                if (hostToolNames.Contains(call.Name))
                {
                    pendingToolCall = call;
                }
            }
            return new ChatResponse(new ChatMessage(ChatRole.Assistant, contents))
            {
                FinishReason = response.Calls.Length == 0 ? ChatFinishReason.Stop : ChatFinishReason.ToolCalls
            };
        }
        catch
        {
            Faulted = true;
            throw;
        }
    }

    private async ValueTask<object?> InvokeToolAsync(
        string name, AIFunctionArguments arguments, CancellationToken cancellationToken)
    {
        try
        {
            var call = pendingToolCall;
            if (Faulted || readerFaulted() || call is null || call.Name != name
                || !JsonElement.DeepEquals(JsonSerializer.SerializeToElement(arguments, JsonLines.Json),
                    JsonSerializer.SerializeToElement(call.Arguments, JsonLines.Json)))
            {
                throw new BridgeException("invalid_host_tool_call");
            }
            pendingToolCall = null;
            using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            deadline.CancelAfter(TimeSpan.FromSeconds(10));
            string requestId = $"tool-{++sequence}";
            if (nativeRead is not null)
            {
                await channel.WriteAsync(new
                {
                    version = JsonLines.Version, type = "nativeToolStarted", requestId, snapshotId, turnId,
                    callId = call.CallId, name, arguments = call.Arguments
                }, deadline.Token);
                NativeReadResult result;
                if (call.Arguments.Count != 1 || !call.Arguments.TryGetValue("path", out var path)
                    || path.ValueKind != JsonValueKind.String)
                {
                    result = new(false, "invalid file arguments");
                }
                else
                {
                    result = await NativeReadFile.ReadAsync(nativeRead, path.GetString()!, deadline.Token);
                }
                await channel.WriteAsync(new
                {
                    version = JsonLines.Version, type = "nativeToolCompleted", requestId, snapshotId, turnId,
                    callId = call.CallId, name, arguments = call.Arguments, result
                }, deadline.Token);
                return JsonSerializer.Serialize(result.Text, JsonLines.Json);
            }
            await channel.WriteAsync(new
            {
                version = JsonLines.Version, type = "toolRequest", requestId, snapshotId, turnId,
                callId = call.CallId, name, arguments = call.Arguments
            }, deadline.Token);
            var response = await channel.ReadAsync<ToolReply>(deadline.Token);
            if (response.Version != JsonLines.Version || response.Type != "toolResult"
                || response.RequestId != requestId || response.SnapshotId != snapshotId
                || response.TurnId != turnId || response.CallId != call.CallId || response.Result is null
                || Encoding.UTF8.GetByteCount(response.Result) > JsonLines.MaxContentBytes)
            {
                throw new BridgeException("invalid_host_tool_result");
            }
            return response.Result;
        }
        catch
        {
            Faulted = true;
            throw;
        }
    }

    private sealed class HostFunction(HostModelClient owner, HostToolDefinition definition) : AIFunction
    {
        public override string Name => definition.Name;
        public override string Description => definition.Description;
        public override JsonElement JsonSchema => definition.Parameters;
        protected override ValueTask<object?> InvokeCoreAsync(
            AIFunctionArguments arguments, CancellationToken cancellationToken)
            => owner.InvokeToolAsync(Name, arguments, cancellationToken);
    }

    private static object ProjectContent(AIContent content) => content switch
    {
        TextContent text => new { type = "text", text = text.Text },
        FunctionCallContent call => new { type = "functionCall", callId = call.CallId, name = call.Name, arguments = call.Arguments },
        FunctionResultContent result => new { type = "functionResult", callId = result.CallId, result = result.Result },
        _ => throw new BridgeException("unsupported_model_content")
    };

    public IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
        IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
        => throw new NotSupportedException("Streaming is not supported by the experimental model transport.");

    public object? GetService(Type serviceType, object? serviceKey = null) => null;
    public void Dispose() { }
}