# fenecdb

fenecdb's rows in Grafana. Each query is FenecQL, run by the plugin's backend
over fenec-server's HTTP endpoint, with the dashboard's time range, interval
and variables bound as parameters -- never written into the statement -- so
alert rules run the same queries as the panels.

```
get ticks select bucket(at, $__interval_ms) as time, sym, sum(qty) as volume
  where sym in $syms and at >= $__from and at < $__to
  group time, sym order time
```

- `$__from`, `$__to`, `$__interval_ms`, `$__max_data_points`, and dashboard
  variables as `$name` or `${name:int}`; a multi-value variable after `in`.
- Columns in the select list's order, typed: numbers, times, text, JSON,
  points as longitude and latitude.
- A time series when the statement groups by its first column, a time; text
  columns are labels, a series each.
- Alerting, annotations, query variables, facets, and the server's plan on
  demand.
- An answer asked again unchanged is answered from the one kept (ETag, 304).

Configure the server's URL and a token: the server's, or a JSON Web Token
whose policy reads what the dashboards show and writes nothing.

Documentation: https://fenecdb.com/docs/grafana
