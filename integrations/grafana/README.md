# fenecdb-datasource

fenecdb's rows in Grafana: a data source whose queries are FenecQL, with the
dashboard's time range, interval and variables bound as `$n` parameters --
never written into the statement -- run by the plugin's Go backend over
fenec-server's HTTP endpoint, so alert rules run them too.

```fenecql
get ticks select bucket(at, $__interval_ms) as time, sym, sum(qty) as volume
  where sym in $syms and at >= $__from and at < $__to
  group time, sym order time
```

The backend sends that as

```json
{"query": "get ticks select bucket(at, $1) as time, sym, sum(qty) as volume where sym in [$2, $3] and at >= $4 and at < $5 group time, sym order time",
 "params": [900000, "S19", "S04", 1791072000000, 1791683333856]}
```

and answers a series a symbol. `make grafana-check` runs it in Grafana 13.2.2
beside the Infinity data source, every panel held to fenec-server's own
answer; the docs are at https://fenecdb.com/docs/grafana.

## Queries

- **Built-ins**, worked out by the backend from the request, a dashboard's or
  an alert rule's alike: `$__from` and `$__to` (milliseconds, which a
  `timestamp` field compares with as it is), `$__interval_ms`,
  `$__max_data_points`.
- **A dashboard variable** is `$sym` or `${sym}`; `${n:int}` reads it as a type:
  `text` (the default), `number`, `int`, `float`, `bool`, `timestamp`
  (milliseconds, or a time as text), `json`. A name used twice is one
  parameter. The browser sends each variable's value beside the query; the
  backend gives the name a slot. A value is a value whatever it holds: a
  quote, a comment, a statement.
- **A multi-value variable** goes after `in`: `where sym in $syms` becomes
  `in [$2, $3, $4]`, one parameter a value -- FenecQL's `in` takes a list of
  values and a parameter binds one. Nothing picked is `in []`, no rows.
- **The parameters' list** is `$1`, `$2`, ...: each a literal of its type, or
  one variable whole. Names in the text are numbered after it.
- **Reads only**: a statement starting with anything but `get`, `select` or
  `explain` is refused before it is sent. The token's policy is what holds
  the data source to reads on the server; this keeps a write from leaving
  Grafana at all.
- **After** (optional): a change the server must hold before it answers
  (`Fenec-After`), for a dashboard reading a replica; the data source's Wait
  bounds it (`Fenec-Wait`).
- **Explain** in the editor shows the statement as bound, its parameters, and
  the server's plan.

## Frames

- **Columns in the select list's order**, typed by the collection's schema
  where a column is a field (`collections`, kept 30 s) and by the values where
  it is computed: `int`/`float` a number (an int past 2^53 kept exact),
  `timestamp` a time, `bool`, `text` a string, `json`, lists and bytes a JSON
  field, a `geo` point two numbers (`loc.lon`, `loc.lat`, Geomap's
  coordinates), a vector or sparse vector left out with a notice.
- **A time series** when the format is Time series, or Auto and the statement
  groups by its first column, a time. Text columns beside it are labels: a
  group by time and symbol becomes a series a symbol (long to wide), which an
  alert rule evaluates an instance a symbol. Rows with no time are left out
  and said.
- **Facets** are frames of their own (`facet <field>`: value, count).
- **`lookup`** levels are flattened as every flat transport flattens them:
  one row per path, `<collection>.<field>` columns, nulls where a level ran
  out.
- **Annotations** come from a query's `time`, `title`, `text` and `tags`
  columns; **query variables** from its first column, or `__text` and
  `__value`.

## Caching

The answer to each statement and its parameters is kept with its `ETag`, and
the same request is sent again with `If-None-Match`: a 304 is answered from
the kept answer. With the server's token fenec-server answers that 304
without running the statement; with a scoped JSON Web Token it runs it and
compares a digest of the answer, which saves the transfer and the plugin's
parsing only -- a 304 by the change counter would tell a token when anyone
wrote to what it reads. A range ending at `now` asks a new request each
refresh; a variable's query, an absolute range or `now/h` ask the same one.
Grafana's own query caching is Grafana Enterprise's and Cloud's, not the
open-source build's. `monitoring/grafana/bench.mjs` measures each way.

## Security

- The token is in the data source's secure settings only: Grafana keeps it
  encrypted and hands it to the backend alone; no frame, resource answer or
  page carries it.
- Every request goes to the configured URL and a path the plugin names
  (`/query`, `/batch`, `/_health`), under `/t/<tenant>/` when a tenant is set;
  nothing a query holds becomes part of a URL. The client follows no
  redirect, sends no basic auth, no headers of the settings' own, and nothing
  forwarded from the viewer.
- No FenecQL is made from user input: a variable's value is a parameter, and
  the only text the plugin writes is `$n`, a bracket of them, and `explain`
  before a `get` for the editor's plan.
- Any viewer of a Grafana can send a data source any query
  (`POST /api/ds/query`), so give it a JSON Web Token whose policy reads what
  the dashboards show and writes nothing (`monitoring/policy.txt`).

## Configure

```yaml
apiVersion: 1
datasources:
  - name: fenecdb
    type: fenecdb-datasource
    uid: fenecdb
    url: http://fenecdb:8080
    jsonData:
      tenant: acme          # optional: /t/acme/
      waitMs: 5000          # optional: Fenec-Wait
      timeoutSeconds: 30    # optional
    secureJsonData:
      token: $__file{/run/fenec/grafana.jwt}
```

## Build and test

The layout is `@grafana/create-plugin` 7.12.2's (`.config/` is its own:
`npx @grafana/create-plugin@7.12.2 update` moves it), the versions pinned by
`package-lock.json` and `go.mod`; the backend takes the Go SDK beside it
(`replace ../go`).

```sh
npm ci && npm run typecheck && npm run lint && npm run test:ci && npm run build   # dist/
go run github.com/magefile/mage@v1.17.2 -v build:linux build:linuxARM64             # dist/gpx_fenecdb_linux_*
FENEC_SERVER=../../target/debug/fenec-server go test ./pkg/...                       # the live tests need a fenec-server
make grafana-check                                                                   # from the repository's root
```

`Dockerfile` builds Grafana 13.2.2 with the plugin from this checkout, which
`monitoring/docker-compose.yml` runs. Into a Grafana of your own: copy `dist/`
to `<plugins>/fenecdb-datasource/` and let it load unsigned,
`GF_PLUGINS_ALLOW_LOADING_UNSIGNED_PLUGINS=fenecdb-datasource`.

## Signing

Grafana loads an unsigned plugin only when told to by name. Signing needs a
Grafana Cloud account, which is the publisher's step:

1. Create an access policy token with the `plugins:write` scope in Grafana
   Cloud (My Account, Security, Access Policies).
2. Private signature, for your own Grafanas, named by their root URLs:
   `GRAFANA_ACCESS_POLICY_TOKEN=... npx @grafana/sign-plugin@latest --rootUrls https://grafana.example.com`
   after `npm run build` and the backend build; it writes `dist/MANIFEST.txt`.
3. Community signature, for the catalog: submit the zipped `dist/` at
   grafana.com for review. The catalog's validator asks for an id of the form
   `<org>-<name>-<type>`, so this id (`fenecdb-datasource`) would become one
   such as `fenecdb-fenecdb-datasource` in `src/plugin.json` and `pkg/plugin/settings.go`.

Then `GF_PLUGINS_ALLOW_LOADING_UNSIGNED_PLUGINS` is no longer needed.
