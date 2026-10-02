# Releasing fenecdb

A release is a tag. Its binaries, browser bundle and image are built from the
tag and left in a draft; the packages follow once the draft is published, so
nothing reaches a registry before the notes have had a read.

## A release

1. `make version V=X.Y.Z`: the one version goes into the workspace, the
   Python package, the npm packages, the NuGet package and the image the
   README pulls. Then
   `cargo check` moves `Cargo.lock`, and the change goes through a pull
   request like any other.
2. Tag what was merged and push the tag:
   `git tag vX.Y.Z origin/main && git push origin vX.Y.Z`.
   `release.yml` builds the binaries for Linux and macOS, the `fenec-web`
   bundle and the multi-arch image on ghcr.io, and drafts the release with
   their checksums. Beside `vX.Y.Z` it pushes `integrations/go/vX.Y.Z` at
   the same commit: a Go module in a subdirectory is fetched by a tag that
   names it, so that tag is the Go SDK's release (`go get
   github.com/fenecdb/fenec/integrations/go@vX.Y.Z`), and there is no
   registry to publish to.
3. Read the draft, then publish it: `gh release edit vX.Y.Z --draft=false`.
   `packages.yml` builds the three packages again, installs them where a user
   would and uses them (`integrations/packages.sh`), and only then publishes
   `fenecdb` to PyPI, `@fenecdb/web`, `@fenecdb/react`,
   `@fenecdb/cloudflare` and `@fenecdb/langchain` to npm, and `FenecDb` to
   NuGet -- packed, installed into a fresh console app and used against a
   server (`integrations/dotnet/package.sh`) before it is pushed, and
   skipped with a notice where `NUGET_API_KEY` is not set. A package whose
   version is not the tag's stops it before anything goes out.

`make packages` runs the same checks locally at any time, NuGet's where
the .NET SDK is installed.

| Package | From | What it holds |
| --- | --- | --- |
| `fenecdb` on PyPI | `integrations/python` | the HTTP client and the LangChain and LlamaIndex stores; the standard library alone |
| `@fenecdb/web` on npm | `web/` | `fenec.js` and its types, `fenec.wasm`, `fenec-lite.wasm` and `collate/` |
| `@fenecdb/react` on npm | `integrations/react` | `FenecProvider`, `useFenec`, `useLiveQuery` |
| `@fenecdb/cloudflare` on npm | `integrations/cloudflare` | `persist`, `restore`, `checkpoint`: a database kept in a Durable Object's storage |
| `@fenecdb/langchain` on npm | `integrations/langchain` | `FenecVectorStore` for LangChain.js, over a `Fenec` or a `FenecHttp` |
| `FenecDb` on NuGet | `integrations/dotnet/FenecDb` | `FenecClient`, the .NET SDK over HTTP; `HttpClient` and `System.Text.Json` alone |
| `github.com/fenecdb/fenec/integrations/go` | the tag `integrations/go/vX.Y.Z` | package `fenecdb`, the Go SDK over HTTP; the standard library alone |

## Once: the registries' side

`packages.yml` publishes with a token where a registry's secret holds one,
and with its own OIDC identity (trusted publishing) where it does not.
Either is set up once, by whoever owns the names.

**With tokens.** On npm, create the organization `fenecdb` (the `@fenecdb`
scope) and an automation token with publish access to it. npm answers a
publish into a scope nobody has made `404 Not Found - PUT`, as it answers a
token without access -- 0.1.5's dispatch stopped there -- and
`https://registry.npmjs.org/-/org/fenecdb/package` says `Scope not found`
until the organization exists. On PyPI, an API token for the whole account,
since a token scoped to a project cannot make the project on its first
upload. Then, pasting each when asked:

    gh secret set NPM_TOKEN -R fenecdb/fenec
    gh secret set PYPI_API_TOKEN -R fenecdb/fenec

After the first upload the PyPI token can be replaced by one scoped to
`fenecdb`. A release published before the secrets were set gets its
packages from a dispatch: `gh workflow run packages.yml -f tag=vX.Y.Z`.

**Without tokens,** the registries trust the workflow itself:

**PyPI.** Signed in as the account that will own `fenecdb`, under
*Publishing*, add a pending publisher: project `fenecdb`, owner `fenecdb`,
repository `fenec`, workflow `packages.yml`, environment `pypi`. The first
published release creates the project.

**npm.** Create the organization `fenecdb`, which is the `@fenecdb` scope. npm
trusts a workflow only for a package that already exists, so each package's
first version goes out with a token:

1. Make a granular access token with read and write on `@fenecdb` and store it
   as the repository secret `NPM_TOKEN`.
2. Publish the release: the four `@fenecdb` packages go out with it.
3. On npmjs.com, for each of the four, *Settings*, *Trusted publishing*,
   GitHub Actions: `fenecdb/fenec`, workflow `packages.yml`, environment
   `npm`.
4. Delete the secret and revoke the token. Later releases publish with the
   workflow's identity alone.

**NuGet.** NuGet takes an API key here, with no OIDC fallback: without the
secret the `NuGet` job logs a notice and stops, and the other registries go
on. Signed in to nuget.org as the account that will own `FenecDb`, under
*API Keys*, create a key with *Push new packages and package versions* for
the glob pattern `FenecDb` -- the first push creates the package -- then:

    gh secret set NUGET_API_KEY -R fenecdb/fenec

A key expires (365 days at most); a new one goes into the same secret. A
release published before the secret was set gets its package from a
dispatch: `gh workflow run packages.yml -f tag=vX.Y.Z`.

**Go.** Nothing to set up: the module is fetched from the repository by its
tag, and `proxy.golang.org` keeps a version once someone has fetched it.
A tag pushed by mistake is not moved; the next version replaces it.

**GitHub.** The environments `pypi`, `npm` and `nuget` are made the first
time the workflow runs. A required reviewer on them makes a publish wait for a second
yes.

**After the first publish.** The docs install what a registry holds: PyPI's
`fenecdb` since 0.1.5 (`pip install "fenecdb[langchain]"`). Until
`@fenecdb/web` is on npm the React example imports `sync` from
`./fenec.js`; then from `@fenecdb/web`, with `npm install @fenecdb/web
@fenecdb/react` beside it.
