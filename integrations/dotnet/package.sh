#!/bin/sh
# FenecDb as NuGet would take it: packed, installed from the .nupkg into a
# console app of its own, and used against a fenec-server started here --
# a write, a vector search and a refusal. Run by `make packages`, by CI, and
# by packages.yml before the package is pushed. The package lands in
# <out>/FenecDb.<version>.nupkg.
#
#   integrations/dotnet/package.sh [out]
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out=${1:-$root/target/nuget}
rm -rf "$out"
mkdir -p "$out"
out=$(cd "$out" && pwd)

# The workspace's version, or a release would push one under another's
# number.
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
nuget=$(sed -n 's:.*<Version>\(.*\)</Version>.*:\1:p' "$here/FenecDb/FenecDb.csproj")
if [ "$nuget" != "$version" ]; then
    echo "FenecDb says $nuget where the workspace says $version (make version V=...)" >&2
    exit 1
fi
if [ -n "${RELEASE_TAG:-}" ] && [ "${RELEASE_TAG#v}" != "$version" ]; then
    echo "the release is $RELEASE_TAG, the package $version" >&2
    exit 1
fi

dotnet pack "$here/FenecDb" -c Release -o "$out" --nologo -v quiet >/dev/null
[ -f "$out/FenecDb.$version.nupkg" ] || { echo "no FenecDb.$version.nupkg in $out" >&2; exit 1; }

cargo=${CARGO:-cargo}
[ -x "$HOME/.cargo/bin/cargo" ] && cargo="$HOME/.cargo/bin/cargo"
"$cargo" build -q -p fenec-server --manifest-path "$root/Cargo.toml"
work=$(mktemp -d)
"$root/target/debug/fenec-server" --http 127.0.0.1:0 2>"$work/server.log" &
server=$!
trap 'kill $server 2>/dev/null; rm -rf "$work"' EXIT INT TERM
i=0
until grep -q "listening on: http://" "$work/server.log"; do
    i=$((i + 1))
    [ $i -gt 100 ] && { cat "$work/server.log"; exit 1; }
    sleep 0.1
done
url=$(sed -n 's/.*listening on: \(http:[^ ]*\).*/\1/p' "$work/server.log" | head -1)

# A console app as a user makes one, for the SDK's own framework, with the
# .nupkg's directory as its one package source beside nuget.org: a restore
# that found FenecDb anywhere else would test that one. The NuGet cache
# keeps a package by its id and version, so the app gets a cache of its
# own -- an earlier pack of the same version would be taken from the
# shared one.
cd "$work"
dotnet new console -o app >/dev/null
cd app
export NUGET_PACKAGES="$work/packages"
cat > nuget.config <<EOF
<?xml version="1.0" encoding="utf-8"?>
<configuration>
  <packageSources>
    <clear />
    <add key="packed" value="$out" />
    <add key="nuget.org" value="https://api.nuget.org/v3/index.json" />
  </packageSources>
</configuration>
EOF
dotnet add package FenecDb --version "$version" >/dev/null
cat > Program.cs <<'EOF'
using FenecDb;

using var db = new FenecClient(args[0]);
await db.ExecAsync("create collection docs (title text, embed vector<3> @hnsw(cosine))");
await db.ExecAsync("put docs {title: $1, embed: $2}", ["Dunes", new[] { 0.9f, 0.1f, 0f }]);
var rows = await db.QueryAsync("get docs select title near embed $1 limit 1", [new[] { 0.9f, 0.1f, 0f }]);
if (rows[0].GetProperty("title").GetString() != "Dunes") throw new Exception("near missed the row");
try { await db.QueryAsync("get nowhere"); throw new Exception("a missing collection was answered"); }
catch (FenecException e) when (e.Status == 404) { }
Console.WriteLine($"FenecDb {typeof(FenecClient).Assembly.GetName().Version}: a write, a search and a refusal");
EOF
dotnet run -c Release -- "$url"
ls -1 "$out"
