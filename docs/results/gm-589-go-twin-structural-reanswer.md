# GM-589: Go twin structural re-answer, before and after

Base: release-4.10.0 `d8fcc50` (Go plugin 0.5.0). After: `fix/GM-589-go-twin-structural-reanswer` at `69a7667`.
Release builds, corpus gin `73726dc` (clean), full reindex with the semantic pass completed in both arms.
Plugin root overridden per arm (`G_MESH_PLUGIN_ROOTS_OVERRIDE`, go only). Edges compared by address
(file, qualified name, native kind of both ends, kind, source, resolved), not id.

| count | expected before -> after | measured before | measured after |
|---|---|---|---|
| twin query, syntactic `name` | 241 -> 5 | 241 | 5 |
| twin query, semantic | 0 -> 0 | 0 | 0 |
| semantic linked (Go) | 1412 -> 1648 | 1412 | 1648 |
| semantic Go edge rows | 1413 -> 1649 | 1413 | 1649 |
| syntactic structural name-keyed (Go) | 4146 -> 3910 | 4146 | 3910 |
| syntactic linked (Go) | 1604 -> 1604 | 7698 (1) | 7698 |
| resolved edges onto `binding/binding.go` | +235 | 284 | 519 |
| semantic onto `codec/json/json.go` | +1 | 0 | 1 |
| semantic onto binding_nomsgpack / jsoniter / sonic / go_json | 0 | 0 | 0 |
| resolved edges onto the `JSON` var node | +37 | 4 | 41 |
| total edge rows | unchanged | 13740 | 13740 |

(1) The design note's 1604 uses a different definition; my query counts all resolved syntactic edges from Go nodes.
Before equals after, which is what the check needs.

## Diff by address

Rows only before: 236, all unresolved syntactic edges onto `pending_symbol` placeholders.
Rows only after: 236, all resolved semantic edges: 235 onto `binding/binding.go` (134 var, 60 const, 21 interface
REFERENCES, 20 function CALLS) and 1 REFERENCES onto `codec/json/json.go`.
Resolved edges present before and missing/retargeted after: 0. No other delta.

## Timing

Base reindex real 19.00 s (user 3.49, sys 2.88); after 18.14 s (user 3.51, sys 2.21). Load averages (1/5/15) at start:
base 652.83 533.28 367.44, after 664.84 545.92 376.82; machine heavily loaded, wall times not comparable.
Plugin cores differ (core sha256 d331c1d872c3 vs 88cd4bc62b5e; go plugin d4cb1a51d341 vs 7531fc0586b9).

## Verdict

All expected counts hit exactly; the 5 `json` rows from `codec/json/go_json.go` stay refused. No findings.
