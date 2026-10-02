# Releasing fenecdb

A release is a tag. Its binaries, browser bundle and image are built from the
tag and left in a draft; the packages follow once the draft is published, so
nothing reaches a registry before the notes have had a read.

## A release

1. `make version V=X.Y.Z`: the one version goes into the workspace, the
   Python package, the npm packages, the NuGet package, the Kotlin and Dart
   packages, the Flutter plugin, `Package.swift`'s release and the image the
   README pulls. Then
   `cargo check` moves `Cargo.lock`, and the change goes through a pull
   request like any other.
2. Build the Swift package's binary: `gh workflow run swift-binary.yml -f
   version=X.Y.Z`. SwiftPM fetches the package by its tag, and the
   `Package.swift` at that tag must already name the XCFramework's zip by
   its URL and checksum, so the zip is built before the tag: the run builds
   it from main, tests it on macOS and an iOS simulator, keeps it as its
   artifact and opens a pull request writing its checksum into
   `Package.swift` (`integrations/swift/set-binary.sh`). Merge it.
3. Tag what was merged and push the tag:
   `git tag vX.Y.Z origin/main && git push origin vX.Y.Z`.
   `release.yml` builds the binaries for Linux and macOS, the `fenec-web`
   bundle, the Android AAR (`fenecdb-android-X.Y.Z.aar`, the native library
   for each ABI inside) and the multi-arch image on ghcr.io, takes the
   XCFramework zip step 2's run kept -- held to the checksum the tag's
   `Package.swift` names, and stopping the release where none matches --
   and drafts the release with all of them and their checksums. Beside `vX.Y.Z` it pushes `integrations/go/vX.Y.Z` at
   the same commit: a Go module in a subdirectory is fetched by a tag that
   names it, so that tag is the Go SDK's release (`go get
   github.com/fenecdb/fenec/integrations/go@vX.Y.Z`), and there is no
   registry to publish to.
4. Read the draft, then publish it: `gh release edit vX.Y.Z --draft=false`.
   `packages.yml` builds the three packages again, installs them where a user
   would and uses them (`integrations/packages.sh`), and only then publishes
   `fenecdb` to PyPI, `@fenecdb/web`, `@fenecdb/react`,
   `@fenecdb/cloudflare` and `@fenecdb/langchain` to npm, and `FenecDb` to
   NuGet -- packed, installed into a fresh console app and used against a
   server (`integrations/dotnet/package.sh`) before it is pushed, and
   skipped with a notice where `NUGET_API_KEY` is not set. With their
   secrets it also publishes `io.github.fenecdb:fenecdb` and
   `fenecdb-android` to Maven Central and `fenecdb` and `fenecdb_flutter`
   to pub.dev -- the plugin with the release's XCFramework and AAR libraries
   in it -- each skipped with a notice where its secrets are not set. A
   package whose version is not the tag's stops it before anything goes out.
   SwiftPM has no registry: the tag and the zip on the release are the
   Swift package.

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
| `FenecDB` for SwiftPM | `Package.swift`, `integrations/swift` | the tag, and `FenecFFI.xcframework.zip` on the release: the library embedded in a macOS or iOS app |
| `io.github.fenecdb:fenecdb` on Maven Central | `integrations/kotlin/fenecdb` | the Kotlin library for the JVM; kotlinx-coroutines alone, the native library brought by the app |
| `io.github.fenecdb:fenecdb-android` on Maven Central | `integrations/kotlin/android` | the AAR: the same library and `libfenec_ffi.so` for arm64-v8a, armeabi-v7a, x86_64 |
| `fenecdb` on pub.dev | `integrations/dart/fenecdb` | the Dart package over `dart:ffi`; `package:ffi` alone |
| `fenecdb_flutter` on pub.dev | `integrations/dart/fenecdb_flutter` | the Flutter plugin: the package with the library for iOS, Android and macOS |

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

**Swift.** Nothing to set up either: SwiftPM fetches the package by the
tag and the zip by the URL `Package.swift` names. `swift-binary.yml` opens
its pull request with the workflow's token, which the repository has to
allow (*Settings*, *Actions*, *General*, "Allow GitHub Actions to create
and approve pull requests"); without it the run pushes the branch
`swift-binary-vX.Y.Z` and the pull request is opened by hand.

**Maven Central.** The Central Portal (central.sonatype.com) takes the
bundle `packages.yml` makes, with no Gradle plugin. Once:

1. Sign in to the Portal and add the namespace `io.github.fenecdb`; it is
   verified by a public repository named as the Portal says, made under the
   `fenecdb` organization, which can be deleted after.
2. Under *View Account*, *Generate User Token*: its two halves are the
   secrets `MAVEN_CENTRAL_USERNAME` and `MAVEN_CENTRAL_PASSWORD`.
3. Every file is signed: make a GPG key for the releases
   (`gpg --quick-gen-key "fenecdb releases" rsa4096 sign never`), send its
   public half to a key server the Portal reads
   (`gpg --keyserver keyserver.ubuntu.com --send-keys <id>`), and store the
   private half, armored, and its passphrase:

       gpg --armor --export-secret-keys <id> | gh secret set SIGNING_KEY -R fenecdb/fenec
       gh secret set SIGNING_PASSWORD -R fenecdb/fenec
       gh secret set MAVEN_CENTRAL_USERNAME -R fenecdb/fenec
       gh secret set MAVEN_CENTRAL_PASSWORD -R fenecdb/fenec

The upload is `publishingType=AUTOMATIC`: the Portal releases the
deployment once it validates, and a version is there for good.

**pub.dev.** pub.dev's automated publishing trusts only a workflow started
by a tag's push, and `packages.yml` follows a published release, so it
publishes with an account's credentials. Signed in to pub.dev as the account
that will own `fenecdb` and `fenecdb_flutter` (a verified publisher,
`fenecdb.dev` or the like, can own them after): `dart pub login` on any
machine writes `pub-credentials.json` (`~/Library/Application
Support/dart/` on macOS, `~/.config/dart/` on Linux); then

    gh secret set PUB_CREDENTIALS -R fenecdb/fenec < <that file>

The first publish creates both packages. The credentials hold a refresh
token: logging out of that machine's pub (`dart pub logout`) revokes it,
and a new login goes into the same secret.

**GitHub.** The environments `pypi`, `npm`, `nuget`, `maven` and `pub` are made the first
time the workflow runs. A required reviewer on them makes a publish wait for a second
yes.

**After the first publish.** The docs install what a registry holds: PyPI's
`fenecdb` since 0.1.5 (`pip install "fenecdb[langchain]"`). Until
`@fenecdb/web` is on npm the React example imports `sync` from
`./fenec.js`; then from `@fenecdb/web`, with `npm install @fenecdb/web
@fenecdb/react` beside it.
