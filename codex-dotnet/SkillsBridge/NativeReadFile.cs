using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Microsoft.Win32.SafeHandles;

namespace Codex.SkillsBridge;

internal sealed record NativeReadContext(string Cwd, string[] ReadRoots);
internal sealed record NativeReadInput(NativeReadContext Context, string Path, int ParentPid = 0);
internal sealed record NativeReadResult(bool Success, string Text);

internal static class NativeReadFile
{
    internal const int MaxBytes = 128 * 1024;
    internal static HostToolDefinition Definition { get; } = new("codex_read_file",
        "Read a UTF-8 text file in the primary environment using Codex filesystem permissions. Relative paths resolve from the working directory. Files must be at most 128 KiB. No writes or commands are supported.",
        JsonSerializer.SerializeToElement(new
        {
            type = "object", properties = new { path = new { type = "string" } },
            required = new[] { "path" }, additionalProperties = false
        }));

    internal static async Task<NativeReadResult> ReadAsync(
        NativeReadContext context, string path, CancellationToken cancellationToken)
    {
        using var process = new Process
        {
            StartInfo = new ProcessStartInfo(Environment.ProcessPath!)
            {
                RedirectStandardInput = true, RedirectStandardOutput = true,
                RedirectStandardError = true, UseShellExecute = false
            }
        };
        process.StartInfo.Environment.Clear();
        process.StartInfo.Environment["DOTNET_EnableDiagnostics"] = "0";
        process.StartInfo.ArgumentList.Add(typeof(NativeReadFile).Assembly.Location);
        process.StartInfo.ArgumentList.Add("--native-read-worker");
        process.Start();
        try
        {
            using var deadline = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            deadline.CancelAfter(TimeSpan.FromSeconds(8));
            await process.StandardInput.WriteLineAsync(
                JsonSerializer.Serialize(new NativeReadInput(context, path, Environment.ProcessId), JsonLines.Json).AsMemory(), deadline.Token);
            process.StandardInput.Close();
            using var output = new JsonLines(process.StandardOutput.BaseStream, Stream.Null);
            var result = await output.ReadAsync<NativeReadResult>(deadline.Token);
            await process.WaitForExitAsync(deadline.Token);
            if (process.ExitCode != 0 || Encoding.UTF8.GetByteCount(result.Text) > JsonLines.MaxContentBytes)
            {
                throw new BridgeException("native_reader_failed");
            }
            return result;
        }
        finally
        {
            if (!process.HasExited)
            {
                process.Kill(entireProcessTree: true);
                await process.WaitForExitAsync();
            }
        }
    }

    internal static int RunWorker()
    {
        NativeReadResult result;
        try
        {
            if (!OperatingSystem.IsLinux() || RuntimeInformation.ProcessArchitecture != Architecture.X64)
                throw new BridgeException("native_read_sandbox_unavailable");
            int parent = GetParentPid();
            if (Prctl(1, 9, 0, 0, 0) != 0 || GetParentPid() != parent)
                throw new BridgeException("native_reader_parent_gone");
            var line = Console.ReadLine();
            if (line is null || line.Length > 64 * 1024)
                throw new BridgeException("invalid_native_read_input");
            var input = JsonSerializer.Deserialize<NativeReadInput>(line, JsonLines.Json)
                ?? throw new BridgeException("invalid_native_read_input");
            if (input.ParentPid != 0 && input.ParentPid != GetParentPid())
                throw new BridgeException("native_reader_parent_gone");
            if (!Path.IsPathFullyQualified(input.Context.Cwd) || string.IsNullOrEmpty(input.Path)
                || input.Path.Contains('\0') || input.Context.ReadRoots.Length > 64)
                throw new BridgeException("invalid_native_read_input");
            string path = Path.GetFullPath(input.Path, input.Context.Cwd);
            _ = JsonSerializer.Serialize(new NativeReadResult(false, string.Empty), JsonLines.Json);
            RestrictReads(input.Context.ReadRoots);
            result = Read(path);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException
            or ArgumentException or JsonException or BridgeException)
        {
            result = new(false, error is BridgeException bridge ? bridge.Code : "file read denied or unavailable");
        }
        Console.WriteLine(JsonSerializer.Serialize(result, JsonLines.Json));
        return 0;
    }

    private static NativeReadResult Read(string path)
    {
        using var metadata = Open(path, 0x200000 | 0x80000);
        if (FStat(metadata.DangerousGetHandle().ToInt32(), out var stat) != 0 || (stat.Mode & 0xf000) != 0x8000)
            return new(false, "only regular text files are supported");
        if (FStatFs(metadata.DangerousGetHandle().ToInt32(), out var filesystem) != 0
            || filesystem.Type is not (0xef53 or 0x794c7630 or 0x01021994 or 0x58465342 or 0x9123683e))
            return new(false, "native read does not support this filesystem");
        if (stat.Size > MaxBytes)
            return new(false, "file exceeds 128 KiB");
        using var handle = Open($"/proc/self/fd/{metadata.DangerousGetHandle().ToInt32()}", 0x80000 | 0x800);
        using var stream = new FileStream(handle, FileAccess.Read);
        byte[] bytes = new byte[MaxBytes + 1];
        int count = 0;
        while (count < bytes.Length)
        {
            int received = stream.Read(bytes, count, bytes.Length - count);
            if (received == 0) break;
            count += received;
        }
        if (count > MaxBytes) return new(false, "file exceeds 128 KiB");
        string text;
        try { text = new UTF8Encoding(false, true).GetString(bytes, 0, count); }
        catch (DecoderFallbackException) { return new(false, "file is not UTF-8 text"); }
        return text.Contains('\0') ? new(false, "binary files are not supported") : new(true, text);
    }

    private static void RestrictReads(string[] roots)
    {
        if (!OperatingSystem.IsLinux() || RuntimeInformation.ProcessArchitecture != Architecture.X64
            || Syscall(444, 0, 0, 1, 0) < 3)
            throw new BridgeException("native_read_sandbox_unavailable");
        nint attributes = Marshal.AllocHGlobal(16);
        try
        {
            Marshal.WriteInt64(attributes, 0x7fff);
            int ruleset = checked((int)Syscall(444, attributes, 8, 0, 0));
            if (ruleset < 0) throw new BridgeException("native_read_sandbox_unavailable");
            using var rules = new SafeFileHandle(ruleset, ownsHandle: true);
            foreach (string root in roots)
            {
                if (!Path.IsPathFullyQualified(root)) throw new BridgeException("invalid_native_read_root");
                using var directory = Open(root, 0x200000 | 0x80000);
                if (FStat(directory.DangerousGetHandle().ToInt32(), out var stat) != 0)
                    throw new BridgeException("invalid_native_read_root");
                Marshal.WriteInt64(attributes, (stat.Mode & 0xf000) == 0x4000 ? 12 : 4);
                Marshal.WriteInt32(attributes, 8, directory.DangerousGetHandle().ToInt32());
                if (Syscall(445, ruleset, 1, attributes, 0) != 0)
                    throw new BridgeException("native_read_sandbox_unavailable");
            }
            if (Prctl(38, 1, 0, 0, 0) != 0 || Syscall(446, ruleset, 0, 0, 0) != 0)
                throw new BridgeException("native_read_sandbox_unavailable");
        }
        finally { Marshal.FreeHGlobal(attributes); }
    }

    private static SafeFileHandle Open(string path, int flags)
    {
        int descriptor = OpenFile(path, flags);
        if (descriptor < 0) throw new IOException("file read denied or unavailable");
        return new SafeFileHandle(descriptor, ownsHandle: true);
    }

    [StructLayout(LayoutKind.Explicit, Size = 144)]
    private struct FileStat
    {
        [FieldOffset(24)] public uint Mode;
        [FieldOffset(48)] public long Size;
    }

    [StructLayout(LayoutKind.Explicit, Size = 120)]
    private struct FileSystemStat
    {
        [FieldOffset(0)] public long Type;
    }

    [DllImport("libc", EntryPoint = "syscall", SetLastError = true)]
    private static extern long Syscall(long number, long first, long second, long third, long fourth);
    [DllImport("libc", EntryPoint = "prctl", SetLastError = true)]
    private static extern int Prctl(int option, ulong argument, ulong third, ulong fourth, ulong fifth);
    [DllImport("libc", EntryPoint = "open", SetLastError = true)]
    private static extern int OpenFile([MarshalAs(UnmanagedType.LPUTF8Str)] string path, int flags);
    [DllImport("libc", EntryPoint = "fstat", SetLastError = true)]
    private static extern int FStat(int descriptor, out FileStat stat);
    [DllImport("libc", EntryPoint = "fstatfs", SetLastError = true)]
    private static extern int FStatFs(int descriptor, out FileSystemStat stat);
    [DllImport("libc", EntryPoint = "getppid")]
    private static extern int GetParentPid();
}