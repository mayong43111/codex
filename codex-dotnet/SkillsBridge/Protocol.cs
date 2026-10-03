using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Codex.SkillsBridge;

public sealed record Authority(string Kind, string Id);
public sealed record SnapshotEntry(
    Authority Authority, string PackageId, string ResourceId, string Name,
    string Description, string? ShortDescription, bool Enabled, bool PromptVisible);
public sealed record Snapshot(int Version, string SnapshotId, SnapshotEntry[] Entries, string[] Warnings);
internal sealed record Command(
    int Version, string Type, string RequestId, string SnapshotId, string? Name = null, string? Text = null,
    HostToolDefinition[]? Tools = null, NativeReadContext? NativeRead = null);
internal sealed record HostToolDefinition(string Name, string Description, JsonElement Parameters);
internal sealed record ToolReply(
    int Version, string Type, string RequestId, string SnapshotId, string TurnId, string CallId, string Result);
internal sealed record ReadResponse(
    int Version, string Type, string RequestId, string SnapshotId, string? Contents, string? Error);

public sealed class BridgeException(string code) : Exception(code)
{
    public string Code { get; } = code;
}

internal sealed class JsonLines(Stream input, Stream output) : IDisposable
{
    internal const int Version = 1;
    internal const int MaxMessageBytes = 2 * 1024 * 1024;
    internal const int MaxContentBytes = 256 * 1024;
    internal static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web)
    {
        PropertyNameCaseInsensitive = false,
        RespectRequiredConstructorParameters = true,
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
        AllowDuplicateProperties = false,
        MaxDepth = 16
    };

    private readonly StreamReader reader = new(input, new UTF8Encoding(false, true), false, 4096, true);
    private readonly StreamWriter writer = new(output, new UTF8Encoding(false), 4096, true);

    internal async Task<T> ReadAsync<T>(CancellationToken cancellationToken)
    {
        var line = new StringBuilder();
        var character = new char[1];
        while (await reader.ReadAsync(character.AsMemory(), cancellationToken)
            .AsTask().WaitAsync(cancellationToken) != 0)
        {
            if (character[0] == '\n')
            {
                string text = line.ToString();
                if (Encoding.UTF8.GetByteCount(text) > MaxMessageBytes)
                {
                    throw new BridgeException("message_too_large");
                }
                return JsonSerializer.Deserialize<T>(text, Json)
                    ?? throw new BridgeException("invalid_message");
            }
            if (line.Length >= MaxMessageBytes)
            {
                throw new BridgeException("message_too_large");
            }
            line.Append(character[0]);
        }
        if (line.Length != 0)
        {
            throw new BridgeException("incomplete_message");
        }
        throw new EndOfStreamException();
    }

    internal async Task WriteAsync<T>(T message, CancellationToken cancellationToken)
    {
        string text = JsonSerializer.Serialize(message, Json);
        if (Encoding.UTF8.GetByteCount(text) > MaxMessageBytes)
        {
            throw new BridgeException("message_too_large");
        }
        await writer.WriteLineAsync(text.AsMemory(), cancellationToken).WaitAsync(cancellationToken);
        await writer.FlushAsync(cancellationToken).WaitAsync(cancellationToken);
    }

    public void Dispose()
    {
        reader.Dispose();
        writer.Dispose();
    }
}