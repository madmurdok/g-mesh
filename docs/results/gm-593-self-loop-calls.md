# GM-593: semantic CALLS self-loops, before and after

Base: release-4.10.0 `d8fcc50`. After: `fix/GM-593-bridge-self-loop-calls` at `88006c7`.
Release builds, corpora excalidraw and ripgrep (clean checkouts), full reindex
with semantic pass completed. Same method as GM-569/S8 (edge key
`fromId,kind,source,linkedFrom|toId`). Plugin roots overridden per arm.

| arm | corpus | total CALLS | semantic CALLS | semantic self-loops | total edges | real s | user s | sys s | load avg (1/5/15) |
|---|---|---|---|---|---|---|---|---|---|
| base | excalidraw | 7387 | 1835 | 8 | 28678 | 87.48 | 26.01 | 7.19 | 59.05 185.60 278.75 |
| after | excalidraw | 7379 | 1827 | 0 | 28670 | 120.20 | 30.64 | 8.38 | 44.82 149.78 255.68 |
| base | ripgrep | 4424 | 1782 | 15 | 22257 | 197.29 | 7.09 | 2.79 | 57.26 120.04 229.38 |
| after | ripgrep | 4415 | 1773 | 6 | 22248 | 301.95 | 6.59 | 2.34 | 408.08 210.05 239.97 |

Machine was heavily loaded by other agents; real >> user+sys on ripgrep, so
wall times are not comparable between arms. A dry run of ripgrep alone gave
the same counts (15 -> 6) at real 65.86 s / 41.36 s.

## Diff vs base (classify.py)

| corpus | edges only in base | only in after | lost semantic CALLS | retargeted | newly linked |
|---|---|---|---|---|---|
| excalidraw | 8 | 0 | 8 (all self-loops) | 0 | 0 |
| ripgrep | 9 | 0 | 9 (all self-loops) | 0 | 0 |

Matches the design note: excalidraw 8 -> 0, ripgrep 15 -> 6, no other semantic
CALLS lost or retargeted; total edges drop by exactly the dropped answers.

## Cheap observation (ripgrep self-loop callers)

- Base: 15, including `decompress::try_resolve_binary` and
  `Error::is_partial`.
- After: 6, `<Error as Display>::fmt`, `Error::{depth, into_io_error,
  io_error, is_io, is_partial}`. `try_resolve_binary`'s self-edge is gone,
  `Error::is_partial`'s is kept (real recursion on a boxed inner `Error`).
