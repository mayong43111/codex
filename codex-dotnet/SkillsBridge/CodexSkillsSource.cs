using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Microsoft.Agents.AI;

namespace Codex.SkillsBridge;

public sealed class CodexSkillsSource : AgentSkillsSource
{
    private readonly IList<AgentSkill> skills;

    public CodexSkillsSource(
        Snapshot snapshot,
        Func<SnapshotEntry, CancellationToken, ValueTask<string>> readContent)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        ArgumentNullException.ThrowIfNull(readContent);
        if (snapshot.Version != JsonLines.Version || string.IsNullOrWhiteSpace(snapshot.SnapshotId)
            || snapshot.SnapshotId.Length > 128 || snapshot.Entries is null || snapshot.Entries.Length > 1024
            || snapshot.Warnings is null || snapshot.Warnings.Any(warning => warning is null))
        {
            throw new BridgeException("invalid_snapshot");
        }
        var identities = new HashSet<string>(StringComparer.Ordinal);
        foreach (var entry in snapshot.Entries)
        {
            if (entry?.Authority is null || entry.Authority.Kind != "host"
                || string.IsNullOrWhiteSpace(entry.Authority.Id)
                || string.IsNullOrWhiteSpace(entry.PackageId) || string.IsNullOrWhiteSpace(entry.ResourceId)
                || string.IsNullOrWhiteSpace(entry.Name) || string.IsNullOrWhiteSpace(entry.Description)
                || !identities.Add(Identity(entry)))
            {
                throw new BridgeException("invalid_snapshot_entry");
            }
        }
        var visible = snapshot.Entries.Where(entry => entry.Enabled && entry.PromptVisible).ToArray();
        var nameCounts = visible.GroupBy(entry => entry.Name, StringComparer.Ordinal)
            .ToDictionary(group => group.Key, group => group.Count(), StringComparer.Ordinal);
        var reserved = visible.Where(entry => IsValidName(entry.Name)).Select(entry => entry.Name)
            .ToHashSet(StringComparer.Ordinal);
        var assigned = new HashSet<string>(StringComparer.Ordinal);
        var result = new List<AgentSkill>();
        foreach (var entry in visible)
        {
            string name = entry.Name;
            if (!IsValidName(name) || nameCounts[name] != 1)
            {
                name = "codex-" + Convert.ToHexString(SHA256.HashData(Encoding.UTF8.GetBytes(Identity(entry))))
                    .ToLowerInvariant()[..48];
                if (reserved.Contains(name))
                {
                    throw new BridgeException("alias_collision");
                }
            }
            if (!assigned.Add(name))
            {
                throw new BridgeException("alias_collision");
            }
            string description = string.IsNullOrWhiteSpace(entry.ShortDescription)
                ? entry.Description : entry.ShortDescription;
            if (description.Length > 1024)
            {
                int length = char.IsHighSurrogate(description[1023]) ? 1023 : 1024;
                description = description[..length];
            }
            result.Add(new CodexSkill(entry, new AgentSkillFrontmatter(name, description), readContent));
        }
        skills = result.AsReadOnly();
    }

    public override Task<IList<AgentSkill>> GetSkillsAsync(
        AgentSkillsSourceContext context, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return Task.FromResult(skills);
    }

    private static string Identity(SnapshotEntry entry) =>
        JsonSerializer.Serialize(new[] { entry.Authority.Kind, entry.Authority.Id, entry.PackageId });

    private static bool IsValidName(string name)
    {
        try
        {
            _ = new AgentSkillFrontmatter(name, "Name validation.");
            return true;
        }
        catch (ArgumentException)
        {
            return false;
        }
    }
}

public sealed class CodexSkill(
    SnapshotEntry original,
    AgentSkillFrontmatter frontmatter,
    Func<SnapshotEntry, CancellationToken, ValueTask<string>> readContent) : AgentSkill
{
    public SnapshotEntry Original { get; } = original;
    public override AgentSkillFrontmatter Frontmatter { get; } = frontmatter;

    public override ValueTask<string> GetContentAsync(CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return readContent(Original, cancellationToken);
    }
}