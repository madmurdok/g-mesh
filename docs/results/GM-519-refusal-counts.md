# GM-519 S2: linker-refused semantic answers on the bench corpora

Counts only; no interpretation (S3).

## Setup

- g-mesh 4.6.0 (`~/.local/share/g-mesh/bin/g-mesh`), `g-mesh reindex` per corpus, then `docs/results/GM-519-count-refusals.sql` over `<stateDir>/index.db` (method: `GM-519-findings-draft.md`, "S2 counting method").
- Every reindex ended with `semanticPass.completed = true`, no failures, leftovers, owed or pending.
- Language servers: vtsls 0.3.0, pyright 1.1.414, rust-analyzer 1.97.1, go/types via the Go plugin.
- Machine: `uptime` at the start of the run: up 7 days, load averages 8.7 / 18.1 / 18.6 (3:54); the load stayed 9-15 (1-min) across the corpora, so the machine was busy.

| corpus | commit | warm checkout | reindex `/usr/bin/time -p` real / user / sys (s) | uptime 1-min load at start |
|---|---|---|---|---|
| excalidraw (TypeScript) | 1acf66edabc2ac5bbd4aed0714aed7dca7cc2aab | `$TMPDIR/gmesh-bench-corpora/excalidraw` | 70.59 / 21.21 / 5.69 | 14.62 |
| gin v1.12.0 (Go) | 73726dc606796a025971fe451f0aa6f1b9b847f6 | `.../gin` | 8.74 / 8.13 / 3.03 | 10.75 |
| ripgrep 15.2.0 (Rust) | e89fff89ac9af12e8d4ce9d5fd07beb408ca730f | `.../ripgrep` | 43.87 / 48.14 / 12.71 | 9.79 |
| py-requests (Python) | 6e83187b8feb273ed4c6cdab5efd8d54901dfab3 | `.../py-requests` | 23.92 / 27.69 / 1.68 | 9.14 |

`git status --short` of each of the four corpus checkouts after the run: 0 changed files (clean).

## Counts (edges; `source` = tier that produced the edge)

Semantic rows are the subject. `linked` = settled by the linker; `refused_*` = the linker kept the edge on its placeholder although a declaration exists at the address (`refused_visibility_file` = file rule, `refused_visibility_container` = container rule, `refused_other` = a visible declaration exists but the edge stayed: ambiguity or kind rule); `structural_name_keyed` = `name`-keyed placeholders, not classified by the SQL. Semantic answers = linked + refused (+ unclassified name-keyed shown separately). Share = refused / (linked + refused).

### TypeScript (excalidraw)

| reason | semantic | syntactic |
|---|---|---|
| linked | 2021 | 10878 |
| refused_visibility_file | 62 | 0 |
| refused_visibility_container | 0 | 0 |
| refused_other | 82 | 0 |
| no_declaration_at_address | 0 | 0 |
| structural_name_keyed (unclassified) | 38 | 279 |

Semantic: 2021 linked + 144 refused = 2165 classified answers; refused 144 (6.7%): file rule 62 (2.9%), other 82 (3.8%).

### Go (gin)

| reason | semantic | syntactic |
|---|---|---|
| linked | 1297 | 1604 |
| refused_visibility_file | 0 | 0 |
| refused_visibility_container | 1 | 0 |
| refused_other | 114 | 0 |
| no_declaration_at_address | 0 | 0 |
| structural_name_keyed (unclassified) | 1 | 4146 |

Semantic: 1297 linked + 115 refused = 1412 classified answers; refused 115 (8.1%): container rule 1, other 114 (8.1%).

### Rust (ripgrep)

| reason | semantic | syntactic |
|---|---|---|
| linked | 2063 | 3594 |
| refused_visibility_file | 0 | 0 |
| refused_visibility_container | 0 | 0 |
| refused_other | 51 | 0 |
| no_declaration_at_address | 0 | 764 |
| structural_name_keyed (unclassified) | 0 | 99 |

Semantic: 2063 linked + 51 refused = 2114 classified answers; refused 51 (2.4%), all `refused_other`.

### Python (py-requests)

| reason | semantic | syntactic |
|---|---|---|
| linked | 47 | 1080 |
| refused_* (all three) | 0 | 0 |
| no_declaration_at_address | 0 | 3 |
| structural_name_keyed (unclassified) | 0 | 132 |

Semantic: 47 linked + 0 refused = 47 answers; refused 0.

## Sanity check: the count tells refused from accepted

Fixtures copied to a scratch dir, `g-mesh reindex`, same SQL (semantic pass completed in each).

| fixture | known result (findings draft) | SQL output |
|---|---|---|
| ts/t04-same-file-local-class | missing link (file rule) | `typescript semantic refused_visibility_file 1`; no `linked` rows |
| ts/t03-nonexported-class-via-instance | missing link (file rule) | `typescript semantic refused_visibility_file 1`, `typescript syntactic linked 1` |
| ts/t07-namespace-members | correct | `typescript semantic linked 2`, `typescript syntactic linked 1`; no refused rows |

The refused cases show 1 refused and 0 semantic linked; the correct one shows 0 refused and 2 semantic linked.

## Notes

- The pass log for py-requests reports "whole-project semantic pass carries 1089 linked edge(s)"; the SQL shows 47 semantic and 1080 syntactic linked. Not reconciled here.
- `g-mesh stop` after each run printed "no daemon is running" (the daemon had already exited after reindex); harmless.
- Raw outputs were kept in the session scratchpad, not committed.
