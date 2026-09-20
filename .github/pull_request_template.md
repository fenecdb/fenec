<!--
CONTRIBUTING.md has the full list. The short version: one concern per diff,
and comments that explain *why*.
-->

## What this changes

<!-- And why. If it fixes an issue, link it. -->

## Invariants

<!--
Delete the ones that do not apply; keep and answer the ones that do.

- [ ] Adds no external crate outside fenec-bench
- [ ] Keeps the dependency direction (core -> ql -> http -> pg -> import -> cli)
- [ ] New limits error rather than truncate
- [ ] Does not make the HNSW graph load-bearing (it stays derived data)
- [ ] File-format records stay self-delimiting; the reader consumes the length
- [ ] No threads and no `now()` on the wasm32 path
- [ ] Filtered `near` keeps its full-scan fallback
-->

None of the invariants in CONTRIBUTING.md are affected.

## Testing

<!--
`make test` is the baseline. Say what you added, and if the change touches
performance, recall or memory, say what `make bench` / `make sweep` /
`make memory` reported before and after, and on what hardware.
-->

- [ ] `make test` passes
