# Releasing fenecdb

A release is a tag. Its binaries, browser bundle and image are built from the
tag and left in a draft; the packages follow once the draft is published, so
nothing reaches a registry before the notes have had a read.

## A release

1. `make version V=X.Y.Z`: the one version goes into the workspace, the
   Python package, both npm packages and the image the README pulls. Then
   `cargo check` moves `Cargo.lock`, and the change goes through a pull
   request like any other.
2. Tag what was merged and push the tag:
   `git tag vX.Y.Z origin/main && git push origin vX.Y.Z`.
   `release.yml` builds the binaries for Linux and macOS, the `fenec-web`
   bundle and the multi-arch image on ghcr.io, and drafts the release with
   their checksums.
3. Read the draft, then publish it: `gh release edit vX.Y.Z --draft=false`.
   `packages.yml` builds the three packages again, installs them where a user
   would and uses them (`integrations/packages.sh`), and only then publishes
   `fenecdb` to PyPI and `@fenecdb/web` and `@fenecdb/react` to npm. A
   package whose version is not the tag's stops it before anything goes out.

`make packages` runs the same check locally at any time.

| Package | From | What it holds |
| --- | --- | --- |
| `fenecdb` on PyPI | `integrations/python` | the HTTP client and the LangChain and LlamaIndex stores; the standard library alone |
| `@fenecdb/web` on npm | `web/` | `fenec.js` and its types, `fenec.wasm`, `fenec-lite.wasm` and `collate/` |
| `@fenecdb/react` on npm | `integrations/react` | `FenecProvider`, `useFenec`, `useLiveQuery` |

## Once: the registries' side

The repository keeps no key a registry takes: PyPI and npm are told to trust
`packages.yml` itself (trusted publishing, the workflow's OIDC identity).
Setting that up is done once, by whoever owns the names.

**PyPI.** Signed in as the account that will own `fenecdb`, under
*Publishing*, add a pending publisher: project `fenecdb`, owner `fenecdb`,
repository `fenec`, workflow `packages.yml`, environment `pypi`. The first
published release creates the project.

**npm.** Create the organization `fenecdb`, which is the `@fenecdb` scope. npm
trusts a workflow only for a package that already exists, so each package's
first version goes out with a token:

1. Make a granular access token with read and write on `@fenecdb` and store it
   as the repository secret `NPM_TOKEN`.
2. Publish the release: `@fenecdb/web` and `@fenecdb/react` go out with it.
3. On npmjs.com, for each of the two, *Settings*, *Trusted publishing*,
   GitHub Actions: `fenecdb/fenec`, workflow `packages.yml`, environment
   `npm`.
4. Delete the secret and revoke the token. Later releases publish with the
   workflow's identity alone.

**GitHub.** The environments `pypi` and `npm` are made the first time the
workflow runs. A required reviewer on them makes a publish wait for a second
yes.

**After the first publish.** The docs install from the repository until the
packages exist: `site/content/docs/integrations.html` and
`integrations/python/README.md` then take `pip install "fenecdb[langchain]"`,
and the React example imports `sync` from `@fenecdb/web`.
