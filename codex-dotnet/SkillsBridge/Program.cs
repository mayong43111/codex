using System.Text;
using System.Text.Json;
using Codex.SkillsBridge;
using Microsoft.Agents.AI;
using Microsoft.Extensions.AI;

if (args.SequenceEqual(["--native-read-worker"]))
{
    return NativeReadFile.RunWorker();
}

using var shutdown = new CancellationTokenSource();
Console.CancelKeyPress += (_, signal) =>
{
    signal.Cancel = true;
    shutdown.Cancel();
};
using var channel = new JsonLines(Console.OpenStandardInput(), Console.OpenStandardOutput());
try
{
    bool agentMode = args.SequenceEqual(["--agent"]);
    if (args.Length != 0 && !agentMode)
    {
        throw new BridgeException("invalid_mode");
    }
    using var initialization = CancellationTokenSource.CreateLinkedTokenSource(shutdown.Token);
    initialization.CancelAfter(TimeSpan.FromSeconds(10));
    var snapshot = await channel.ReadAsync<Snapshot>(initialization.Token);
    using var readGate = new SemaphoreSlim(1);
    bool faulted = false;
    long nextReadId = 0;
    using var source = new CodexSkillsSource(snapshot, async (entry, cancellationToken) =>
    {
        using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, shutdown.Token);
        deadline.CancelAfter(TimeSpan.FromSeconds(10));
        await readGate.WaitAsync(deadline.Token);
        try
        {
            if (faulted)
            {
                throw new BridgeException("channel_faulted");
            }
            string requestId = $"read-{++nextReadId}";
            ReadResponse response;
            try
            {
                await channel.WriteAsync(new
                {
                    version = JsonLines.Version,
                    type = "read",
                    requestId,
                    snapshotId = snapshot.SnapshotId,
                    authority = entry.Authority,
                    packageId = entry.PackageId,
                    resourceId = entry.ResourceId
                }, deadline.Token);
                response = await channel.ReadAsync<ReadResponse>(deadline.Token);
                if (response.Version != JsonLines.Version || response.Type != "readResult"
                    || response.RequestId != requestId || response.SnapshotId != snapshot.SnapshotId
                    || (response.Error is null) == (response.Contents is null))
                {
                    throw new BridgeException("invalid_read_response");
                }
            }
            catch
            {
                faulted = true;
                throw;
            }
            if (response.Error is not null)
            {
                throw new BridgeException("resource_read_failed");
            }
            if (Encoding.UTF8.GetByteCount(response.Contents!) > JsonLines.MaxContentBytes)
            {
                throw new BridgeException("resource_too_large");
            }
            return response.Contents!;
        }
        finally
        {
            readGate.Release();
        }
    });
    using var provider = new AgentSkillsProvider(source, options: new AgentSkillsProviderOptions
    {
        DisableLoadSkillApproval = agentMode
    }, ownsSource: false);
    var modelClient = agentMode ? new HostModelClient(channel, snapshot.SnapshotId, () => faulted,
        Environment.GetEnvironmentVariable("CODEX_MAF_LIVE_HOST") == "1" ? TimeSpan.FromSeconds(45) : null) : null;
    using IChatClient client = modelClient is not null ? modelClient : new OfflineChatClient();
    var agent = new ChatClientAgent(client, new ChatClientAgentOptions
    {
        AIContextProviders = [provider]
    });
    var session = agentMode ? await agent.CreateSessionAsync(initialization.Token) : null;
    var skills = await source.GetSkillsAsync(new AgentSkillsSourceContext(agent, null), shutdown.Token);
    var byName = skills.Cast<CodexSkill>().ToDictionary(skill => skill.Frontmatter.Name, StringComparer.Ordinal);
    await channel.WriteAsync(new
    {
        version = JsonLines.Version,
        type = "catalog",
        snapshotId = snapshot.SnapshotId,
        skills = byName.Values.Select(skill => new
        {
            name = skill.Frontmatter.Name,
            description = skill.Frontmatter.Description,
            original = skill.Original
        }),
        warnings = snapshot.Warnings
    }, initialization.Token);

    while (!shutdown.IsCancellationRequested && !faulted && modelClient?.Faulted != true)
    {
        Command command;
        try
        {
            command = await channel.ReadAsync<Command>(shutdown.Token);
        }
        catch (EndOfStreamException)
        {
            break;
        }
        if (command.Version != JsonLines.Version || string.IsNullOrWhiteSpace(command.RequestId)
            || command.RequestId.Length > 128)
        {
            throw new BridgeException("invalid_command");
        }
        using var operation = CancellationTokenSource.CreateLinkedTokenSource(shutdown.Token);
        operation.CancelAfter(TimeSpan.FromSeconds(command.Type == "run" ? 60 : 10));
        try
        {
            if (command.SnapshotId != snapshot.SnapshotId)
            {
                throw new BridgeException("stale_snapshot");
            }
            if (command.Type == "close")
            {
                break;
            }
            if (command.Type == "run" && modelClient is not null)
            {
                if (string.IsNullOrWhiteSpace(command.Text)
                    || Encoding.UTF8.GetByteCount(command.Text) > JsonLines.MaxContentBytes)
                {
                    throw new BridgeException("invalid_turn_input");
                }
                var tools = modelClient.BeginTurn(command.RequestId, command.Tools, command.NativeRead);
                var response = await agent.RunAsync(command.Text, session,
                    new ChatClientAgentRunOptions { ChatOptions = new ChatOptions { Tools = tools } }, operation.Token);
                if (faulted || modelClient.Faulted)
                {
                    throw new BridgeException("channel_faulted");
                }
                if (response.Messages.Count == 0
                    || response.Messages[^1].Contents.Any(content => content is not TextContent))
                {
                    faulted = true;
                    throw new BridgeException("unsupported_agent_response");
                }
                await channel.WriteAsync(new
                {
                    version = JsonLines.Version,
                    type = "completed",
                    requestId = command.RequestId,
                    snapshotId = snapshot.SnapshotId,
                    text = response.Text
                }, operation.Token);
                continue;
            }
            if (command.Type != "load" || command.Name is null || !byName.TryGetValue(command.Name, out var skill))
            {
                throw new BridgeException("skill_not_available");
            }
            string contents = await skill.GetContentAsync(operation.Token);
            await channel.WriteAsync(new
            {
                version = JsonLines.Version,
                type = "loaded",
                requestId = command.RequestId,
                snapshotId = snapshot.SnapshotId,
                name = skill.Frontmatter.Name,
                contents
            }, operation.Token);
        }
        catch (BridgeException error) when (!faulted && modelClient?.Faulted != true)
        {
            await channel.WriteAsync(new
            {
                version = JsonLines.Version,
                type = "error",
                requestId = command.RequestId,
                snapshotId = snapshot.SnapshotId,
                error = error.Code
            }, operation.Token);
        }
    }
    return 0;
}
catch (Exception error) when (error is BridgeException or JsonException or IOException
    or OperationCanceledException or ArgumentException)
{
    Console.Error.WriteLine(error is BridgeException bridge ? bridge.Code : "bridge_failed");
    return 1;
}

internal sealed class OfflineChatClient : IChatClient
{
    public Task<ChatResponse> GetResponseAsync(
        IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
        => throw new NotSupportedException("Model calls are disabled in the read-only bridge.");

    public IAsyncEnumerable<ChatResponseUpdate> GetStreamingResponseAsync(
        IEnumerable<ChatMessage> messages, ChatOptions? options = null, CancellationToken cancellationToken = default)
        => throw new NotSupportedException("Model calls are disabled in the read-only bridge.");

    public object? GetService(Type serviceType, object? serviceKey = null) => null;
    public void Dispose() { }
}