using System.Diagnostics;
using Xunit;

namespace FenecDb.Tests;

/// <summary>
/// fenec-server processes, started from the binary `cargo build -p fenec-server` makes (FENEC_SERVER names
/// another): a primary with a token, a policy for JSON Web Tokens and the change stream, made once for every
/// test, and as a test needs them a replica of it and a node of tenants.
/// </summary>
public sealed class Servers : IDisposable
{
    public const string RootToken = "dotnet-tests";
    public const string ReplToken = "dotnet-repl";
    public const string JwtSecret = "dotnet-tests-jwt-secret-of-32-bytes-or-more";

    public string Binary { get; }
    public string Scratch { get; }
    public string Primary { get; }
    readonly List<Process> _started = [];

    public Servers()
    {
        Binary = Environment.GetEnvironmentVariable("FENEC_SERVER") ?? Path.Combine(RepoRoot(), "target", "debug", "fenec-server");
        if (!File.Exists(Binary))
            throw new FileNotFoundException($"no fenec-server at {Binary}: cargo build -p fenec-server");
        Scratch = Directory.CreateTempSubdirectory("fenecdb-dotnet").FullName;
        var policy = Path.Combine(Scratch, "policy.txt");
        File.WriteAllText(policy, "notes read,write where owner = $jwt.sub\n");
        Primary = Serve("--file", Path.Combine(Scratch, "primary.fenec"), "--http-token", RootToken,
            "--replication-token", ReplToken, "--jwt-secret", JwtSecret, "--policy", policy, "--sync", "50");
    }

    internal static string RepoRoot()
    {
        for (var dir = new DirectoryInfo(AppContext.BaseDirectory); dir is not null; dir = dir.Parent)
            if (File.Exists(Path.Combine(dir.FullName, "Cargo.toml")) && Directory.Exists(Path.Combine(dir.FullName, "crates")))
                return dir.FullName;
        throw new DirectoryNotFoundException("the repository holding these tests");
    }

    /// <summary>Starts a fenec-server on a port of its own and waits for it to say where it listens.</summary>
    public string Serve(params string[] args)
    {
        var info = new ProcessStartInfo(Binary) { RedirectStandardError = true, UseShellExecute = false };
        foreach (var a in new[] { "--http", "127.0.0.1:0" }.Concat(args)) info.ArgumentList.Add(a);
        var p = Process.Start(info)!;
        lock (_started) _started.Add(p);
        var log = new System.Text.StringBuilder();
        var found = new TaskCompletionSource<string?>();
        _ = Task.Run(async () =>
        {
            while (await p.StandardError.ReadLineAsync() is { } line)
            {
                log.AppendLine(line);
                var at = line.IndexOf("listening on: http://", StringComparison.Ordinal);
                if (at >= 0)
                    found.TrySetResult("http://" + line[(at + "listening on: http://".Length)..].Split(' ')[0]);
            }
            found.TrySetResult(null);
        });
        if (!found.Task.Wait(TimeSpan.FromSeconds(30)) || found.Task.Result is not { } url)
            throw new InvalidOperationException("fenec-server did not start:\n" + log);
        return url;
    }

    /// <summary>A token signed with the primary's secret for these claims.</summary>
    public string Mint(string claims)
    {
        var info = new ProcessStartInfo(Binary) { RedirectStandardOutput = true, UseShellExecute = false };
        foreach (var a in new[] { "--jwt-secret", JwtSecret, "--mint-token", claims }) info.ArgumentList.Add(a);
        using var p = Process.Start(info)!;
        var token = p.StandardOutput.ReadToEnd().Trim();
        p.WaitForExit();
        return token;
    }

    public void Dispose()
    {
        foreach (var p in _started)
        {
            try { p.Kill(); p.WaitForExit(); } catch (InvalidOperationException) { }
            p.Dispose();
        }
        try { Directory.Delete(Scratch, recursive: true); } catch (IOException) { }
    }
}

/// <summary>
/// A TCP proxy to a server: cut, it closes what it carries and every connection it takes until healed -- a
/// replica's link to its primary, held while a write is made, so the replica cannot hold that write however soon it
/// would have caught up.
/// </summary>
public sealed class Link : IDisposable
{
    readonly System.Net.Sockets.TcpListener _listener = new(System.Net.IPAddress.Loopback, 0);
    readonly string _host;
    readonly int _port;
    readonly List<System.Net.Sockets.TcpClient> _open = [];
    bool _cut;

    public Link(string url)
    {
        var target = new Uri(url);
        (_host, _port) = (target.Host, target.Port);
        _listener.Start();
        _ = Task.Run(AcceptAll);
    }

    public string Url => $"http://127.0.0.1:{((System.Net.IPEndPoint)_listener.LocalEndpoint).Port}";

    async Task AcceptAll()
    {
        while (true)
        {
            System.Net.Sockets.TcpClient client;
            try { client = await _listener.AcceptTcpClientAsync(); }
            catch (Exception) { return; }
            var upstream = new System.Net.Sockets.TcpClient();
            try { await upstream.ConnectAsync(_host, _port); }
            catch (System.Net.Sockets.SocketException)
            {
                client.Dispose();
                upstream.Dispose();
                continue;
            }
            lock (_open)
            {
                if (_cut)
                {
                    client.Dispose();
                    upstream.Dispose();
                    continue;
                }
                _open.Add(client);
                _open.Add(upstream);
            }
            _ = Pipe(client, upstream);
            _ = Pipe(upstream, client);
        }
    }

    static async Task Pipe(System.Net.Sockets.TcpClient from, System.Net.Sockets.TcpClient to)
    {
        try { await from.GetStream().CopyToAsync(to.GetStream()); }
        catch (Exception) { }
        from.Dispose();
        to.Dispose();
    }

    /// <summary>Cuts the link, closing every connection it carries, or heals it.</summary>
    public void Cut(bool cut)
    {
        lock (_open)
        {
            _cut = cut;
            if (!cut) return;
            foreach (var c in _open) c.Dispose();
            _open.Clear();
        }
    }

    public void Dispose()
    {
        _listener.Stop();
        Cut(true);
    }
}

[CollectionDefinition("servers")]
public sealed class ServersCollection : ICollectionFixture<Servers>;
