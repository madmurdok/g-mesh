# GM-519: member visibility and the linker's visibility rules

Research note; no production change. It answers three questions for the owner:
which bad graph shapes do member visibility (TS option A) and core's
`Linker::visible` produce today, how many engine answers does the linker
refuse on the bench corpora and why, and what to change.

Inputs folded in: the S1 fixture draft (deleted with this commit) and
`GM-519-refusal-counts.md` (S2, kept as the raw count record).

## Background

- `Resolver::visible` (`core/src/graph/symbol_links.rs`) admits a candidate by
  its `visibility`: `public` always; `container(c)` when the requester's
  container is `c` or has it on its parent chain; `file` **only** when the
  lookup scope is a `Scope::Container` and the candidate is in the requester's
  own file. The module doc ("Visibility") narrowed `file` this way so that a
  TS self-import (`import { x } from "./self"`, or a barrel round trip) does
  not see a file's non-exported nodes named `x`.
- TS has no containers, so every TS placeholder is in a `Scope::File`, and a
  `file`-visible TS node is never a candidate for any placeholder.
- **Option A** (4.0.0, `plugins/typescript/src/extractor/emit.rs`): a class or
  interface member takes its owner's visibility, so members of exported types
  are `public`. Without it, every TS member would be `file` and unreachable.
- **Option B** (not taken): a semantic (bridge/engine) answer skips the
  visibility check, since the engine already resolved the use.
- After the visibility filter, the linker applies the **kind rule**
  (`LINKABLE_EDGE_KINDS`: `CALLS` needs a `Function`, `SUPERTYPE_OF` a `Type`
  then a `Function`, `REFERENCES` anything), then wants exactly one candidate;
  several go through `sole_non_member` (name keys only) and
  `sole_accessor_getter`, otherwise the edge stays on its placeholder.

## Method

- Binary: installed g-mesh 4.6.0 with its bundled TS/Python/Rust plugins
  (option A shipped), Go plugin 0.5.0 from the main checkout. Semantic tiers:
  vtsls 0.3.0, pyright 1.1.414, rust-analyzer 1.97.1, go/types.
- Fixtures: `docs/results/GM-519-fixtures/<plugin>/<case>/{project/,expect.toml}`
  in the conformance-kit format; `expect.toml` asserts the **correct** graph,
  so FAIL = bug. `GM-519-fixtures/run-all.sh <g-mesh> <main checkout> <out>`
  runs them all through `g-mesh plugins check`. Every result is with the
  semantic tier on (without it, `plugins check` fails its `session` check and
  skips all expectations, so a structural-only arm is not runnable).
- Cause of each FAIL and the corpus counts: `docs/results/GM-519-count-refusals.sql`
  over a finished index (`g-mesh reindex`, semantic pass `completed: true`).
  An edge still on its `pending_symbol` placeholder (`resolved = 0`,
  `linkedFrom IS NULL`) with a `qualifiedName` key whose address holds a
  declaration was refused; the SQL re-applies `visible`/`sees` and splits it
  into `refused_visibility_file`, `refused_visibility_container`,
  `refused_other`. Sanity check (S2): t03/t04 show 1 refused and 0 linked,
  t07 shows 0 refused and 2 linked.
- S3 breakdown of `refused_other` and the unclassified `name`-keyed semantic
  edges: the SQL in the appendix over the same four S2 indexes (no reindex).

## Bad cases per plugin

"Rule" names what in the linker or plugin produces the result.

### TypeScript (13 cases: 1 wrong, 5 missing, 7 correct)

| case (fixture under `docs/results/GM-519-fixtures/typescript/`) | graph shows | rule |
|---|---|---|
| `t01-import-var-vs-member`: `import { handler }` of a const while exported `Router` has a method `handler` | **wrong link** `run -> Router#handler` (CALLS, syntactic) | option A makes `Router#handler` public; the `name` walk finds Variable `handler` and Function `Router#handler`, the kind rule keeps the Function alone, so `sole_non_member` (which would discard the member) never runs. No semantic answer corrects it |
| `t01c-control-nonexported-owner`: t01 with `Router` not exported | correct (no link) | member is `file`, refused. Shows option A causes t01 |
| `t02-barrel-var-vs-member`: t01 through `export * from "./x"` | **missing link**: structural edge retracted, semantic answer (`handler`, a Variable) refused | kind rule (`CALLS` onto a Variable), not visibility |
| `t03-nonexported-class-via-instance`: `export const service = new ServiceImpl()`, `service.start()` elsewhere | **missing link** `boot -> ServiceImpl#start` | `file` rule: member of a non-exported class is `file`, bridge addresses `Scope::File` |
| `t04-same-file-local-class`: `new Counter().bump()` in Counter's own file | **missing link** `tally -> Counter#bump` | `file` rule: needs `Scope::Container` even from the same file, which TS never has |
| `t05-private-member-same-file` | correct | |
| `t06-module-augmentation`: `declare module "./base" { interface Widget { glow() } }` | **missing link** `show -> Widget#glow` (`w.draw()` linked) | `file` rule: the augmenting interface is not `export`ed syntactically |
| `t07-namespace-members` | correct | |
| `t08-protected-subclass` | correct | `protected` follows its owner |
| `t09-same-name-unrelated-classes` | correct, no spurious link | structural receiver calls stay unresolved |
| `t10-overloads` | correct | |
| `t11-static-qualified-call` | correct | |
| `t12-exported-alias-of-local-class`: `export type Motor = Engine` / returned instance | **missing link** `go -> Engine#ignite` | `file` rule, as t03 |

### Rust (7 cases: 1 missing, 6 correct)

| case (under `.../rust/`) | graph shows | rule |
|---|---|---|
| `r01-macro-rules-sibling-module`: `macro_rules!` in `#[macro_use] mod macros`, used in sibling `mod user` | **missing link** `user::doubled -> macros::twice` | `container` rule, but the cause is the plugin's mapping: an unexported macro gets `container(crate::macros)`, while `#[macro_use]` scopes it textually to everything after the `mod` line, siblings included |
| `r02`..`r07` (private method from child, `pub(crate)`, private trait default method, `pub(super)` in child impl, same-name methods, private `const fn` in const initializers) | correct | bridge scope and `container` visibility agree |

### Python (2 cases: 1 wrong, 1 correct)

| case (under `.../python/`) | graph shows | rule |
|---|---|---|
| `p01-import-var-vs-method`: `from pkg.x import handler`, module variable vs method `Router.handler` | **wrong link** `run -> Router.handler` (CALLS, syntactic) | same mechanism as t01: Python marks every member `public` (keys.rs), so this predates option A |
| `p02-same-name-methods-receiver` | correct | |

### Go (1 case: correct)

`go/g01-unexported-method-other-file`: correct. go/types addresses
`container(<import path>)` with `fromContainer`, and Go containers are flat.

**Which plugins hit the `file` rule:** only TypeScript. Python emits only
`public`, Go `public`/`container`, Rust `file` only on re-export rows.

## Refusal counts on the bench corpora

g-mesh 4.6.0, full reindex, semantic pass completed on all four (S2 detail,
timings and machine state in `GM-519-refusal-counts.md`). Semantic answers =
linked + refused; share = refused / answers.

| corpus | semantic linked | `file` rule | `container` rule | other | refused share |
|---|---|---|---|---|---|
| excalidraw (TS) | 2021 | 62 | 0 | 82 | 144 / 2165 = 6.7% |
| gin (Go) | 1297 | 0 | 1 | 114 | 115 / 1412 = 8.1% |
| ripgrep (Rust) | 2063 | 0 | 0 | 51 | 51 / 2114 = 2.4% |
| py-requests (Python) | 47 | 0 | 0 | 0 | 0 / 47 |

The t01/p01 wrong-link shape (a `name`-keyed link landing on a type member,
the SQL's second statement) counts **0 on all four corpora**.

### Breakdown: all 310 refusals classified, plus the 38 unclassified

Every refused semantic edge (62 + 1 + 247) and the 38 `name`-keyed TS semantic
edges were classified; **63 are visibility, 285 are not.**

| corpus | group | edges | what it is | visibility problem? |
|---|---|---|---|---|
| excalidraw | `file` rule, target in the requester's own file | 37 | non-exported local types (11 type aliases, 1 interface), consts (9), arrow functions (9), methods (5), functions (2), used in their own file | **yes**, t04 shape |
| excalidraw | `file` rule, ambient/augmentation in `global.d.ts` or `declare global` | 21 | `Window` (9), `CanvasRenderingContext2D` (8), `Clipboard#write`, `image-blob-reduce ... #toBlob`, `png-chunk-text.encode/decode` | **yes**, t06 shape |
| excalidraw | `file` rule, non-exported declaration reached through an exported value | 4 | `App#clearSelection`, `App#handleCanvasDoubleClick` (`class App` + `export default App`, t12), `LocalFileManager#clearObsoleteFiles` (instance in an exported static, t03), `subset` (function inside an `export default {}`, t03) | **yes**, t03/t12 shape |
| excalidraw | other: kind rule, `CALLS` onto a Variable | 40 | const-bound functions (`isSomeElementSelected = (function () {...})()` ×19, `selectGroupsForSelectedElements` ×15, ...) | no: kind rule (t02 shape) |
| excalidraw | other: kind rule, `CALLS` onto a Type | 42 | class (29: `App`, `Collab`, `ActionManager`, ...), type alias (7), interface (6) | no: kind rule |
| excalidraw | unclassified `name`-keyed semantic `CALLS` (file scope) | 38 | keys are interfaces/type aliases (29: `ExcalidrawImperativeAPI` ×9, `Action` ×7, ...) and consts (9: `charWidth`, `actionZoomIn`, ...) | no: same kind-rule family (declaration presence at the address not checked) |
| gin | `container` rule | 1 | go/types `SUPERTYPE_OF errorMsgs -> binding.testInterface`: an unexported interface declared in a `_test.go` file of another package | no: the refusal is right (gin cannot name `binding.testInterface`) |
| gin | other: two visible declarations at one address | 114 | build-tag twins `binding/binding.go` / `binding/binding_nomsgpack.go`: `Binding.Bind` ×51, `Binding.Name` ×32, `Binding` ×14, ... (91 CALLS onto interface methods, 23 SUPERTYPE_OF onto interfaces) | no: ambiguity from build constraints |
| ripgrep | other: kind rule, `CALLS` onto a struct/enum | 51 | tuple-struct constructors (`IgnoreMatch(IgnoreMatchInner::Hidden)`, `ByteSet(*self)`, ...): 48 struct, 3 enum | no: kind rule |

So the visibility rules cost 62 edges, all on TypeScript, all the `file` rule;
the kind rule costs 171 (TS 120, Rust 51) and build-tag ambiguity 114 (Go).

**py-requests open item from S2:** the pass log's "1089 linked edges" vs the
SQL's 47 + 1080. The index holds 1127 edges with `linkedFrom` set (47 semantic
+ 258 + 818 + 4 syntactic), so the log line is the count of one link run's
repoints (`LinkSummary.linked_edges`), not the index total. Not reconciled
further; py-requests has 0 refusals either way.

## What each option fixes and risks

Two facts shape the options. On excalidraw every `qualifiedName`-keyed TS
placeholder is semantic (2147 semantic, 0 syntactic; syntactic TS keys are all
`name`). And placeholders are shared between tiers: 26 (TS), 9 (Go), 28 (Rust)
placeholders carry edges of both sources, so a per-tier rule cannot be decided
per placeholder.

| option | fixes (fixtures; corpus edges) | risks (fixture that would turn wrong) | size |
|---|---|---|---|
| **Keep A only** (today) | nothing new | t01 stays wrong (0 on corpora); t03 t04 t06 t12 r01 stay missing; 62 TS edges stay refused | 0 |
| **Revert A** | t01 | 927 cross-file semantic links onto TS members of exported types (excalidraw) lose their target, since members go back to `file`; t08 and t11 (members of exported classes used from another file) turn missing | small, rejected |
| **Add B**: a semantic answer skips `visible` | t03 t04 t06 t12 r01; 62 TS edges, plus 1 Go edge | none of the 23 fixtures turns wrong (t01c is structural). The linker stops second-checking the engine: gin's `errorMsgs -> binding.testInterface` (`SUPERTYPE_OF` onto another package's test-only interface) gets linked. Shared placeholders force the linker loop to split edges by source, or structural edges ride along without a check | medium: per-source split of the link loop, tests, module doc |
| **C: `file` admitted in a file scope for `qualifiedName` keys** (keep the narrowing for `name` keys) | t03 t04 t06 t12; all 62 TS edges | none of the 23 fixtures turns wrong. The module doc's self-import worry is about `name` keys, which keep the narrowed rule. A future structural `qualifiedName` key in a TS file scope would also see `file` nodes (none exists on excalidraw today); a TS `private` member gets linked from another file if the engine answers it (a TS compile error anyway) | small: `visible` gets the key kind, module doc "Visibility" rewritten, 2-3 linker tests, the four fixtures become conformance cases |
| **C1: `name` keys drop type members before the kind rule** | t01, p01 | none of the 23 turns wrong; t01/t02 then show no edge for a call through a const (missing beats wrong). 0 corpus edges move (the shape counts 0) | small: move `sole_non_member`'s member test ahead of the kind filter for `name` keys, 2 tests |
| **R: Rust plugin maps `#[macro_use]` `macro_rules!` to the parent module's container** | r01 | over-admits modules declared before the `mod macros` line (textual scope); r02-r07 untouched | small, Rust plugin only |
| **Both A and B** | as B (A stays; A is a prerequisite for the 927 links) | as B | medium |

Not visibility, so outside A/B/C: the kind rule (t02; 171 corpus edges: TS
const-bound functions and `new`/class calls, Rust tuple-struct constructors)
and Go build-tag ambiguity (114 edges). Each is larger than the whole
visibility loss.

## Recommendation

Keep A; add **C** (TS `file` members reachable by exact engine answers), **C1**
(the t01/p01 wrong link) and **R** (r01); do not take B. C fixes every
visibility refusal on the corpora (62/62) and four of the five visibility
fixtures with a one-predicate change, while keeping both the self-import
guard and the linker's check of engine answers that B gives up. B's only
extra is r01, which R fixes at its cause. Open the kind-rule and build-tag
findings as separate tasks.

## Owner decisions

### D1. How should an engine answer reach a TypeScript `file`-visible declaration?

- **Today:** a non-exported TS class/interface's members, local functions and
  ambient/augmented declarations are `file`-visible, and `file` is only ever
  admitted in a container scope, which TS never uses. So no engine answer can
  link to them, even from the same file.
- **Example:** `class Counter { bump() {} }` and `new Counter().bump()` in the
  same file: vtsls answers `Counter#bump`, the linker refuses it, the call has
  no edge (t04). On excalidraw 62 such answers are dropped (37 same-file, 21
  `Window`/`CanvasRenderingContext2D` augmentations, 4 through exported values).
- **Change / consequence per option:**
  - **C: admit `file` in a file scope for `qualifiedName` keys only (Recommended).**
    Benefit: all 62 edges and t03 t04 t06 t12 link; small core change; `name`
    keys keep the self-import guard. Risk: any future structural
    `qualifiedName` key in TS would see `file` nodes too.
  - **B: semantic answers skip visibility entirely.** Benefit: also fixes the
    Rust macro case (r01) without a plugin change. Risk: medium change (split
    shared placeholders by source); the linker no longer catches engine
    answers like gin's `SUPERTYPE_OF` onto another package's test-only
    interface.
  - **Keep as is.** Benefit: no change. Risk: 2.9% of TS engine answers stay
    dropped, and `find_callers` misses every caller of a non-exported class's
    methods.

### D2. Fix the `import` of a name that a type member shares (t01/p01)?

- **Today:** `import { handler } from "./x"` where `x` exports a const
  `handler` and a class with a method `handler`: the kind rule keeps only the
  method (a Function) for the CALLS edge, so the call links to
  `Router#handler`, which is wrong. Same in Python (`from pkg.x import handler`).
- **Example:** `docs/results/GM-519-fixtures/typescript/t01-import-var-vs-member`,
  `.../python/p01-import-var-vs-method`.
- **Change / consequence per option:**
  - **C1: for `name` keys, drop type members before the kind rule (Recommended).**
    Benefit: removes a wrong link in two languages; small. Risk: the call then
    has no edge (calling a const is not a `Function` target); 0 corpus edges
    change, so the gain is only on code shaped like the fixtures.
  - **Leave it.** Benefit: no change. Risk: the wrong link stays possible; it
    counts 0 on the four corpora.

### D3. Fix `macro_rules!` used from a sibling module via `#[macro_use]` (r01)?

- **Today:** the Rust plugin gives an unexported macro the visibility of its
  own module (`container(crate::macros)`), but `#[macro_use] mod macros;`
  makes it usable in the parent and every later sibling, so rust-analyzer's
  answer for `twice!` in `mod user` is refused.
- **Example:** `docs/results/GM-519-fixtures/rust/r01-macro-rules-sibling-module`;
  0 such refusals on ripgrep.
- **Change / consequence per option:**
  - **R: map `#[macro_use]` macros to the parent module's container (Recommended).**
    Benefit: fixes r01 at its cause, Rust plugin only. Risk: over-admits
    modules declared before the `mod` line.
  - **Rely on B (if chosen in D1).** Benefit: no plugin work. Risk: only
    engine answers are fixed; carries B's risks.
  - **Leave it.** Benefit: no change. Risk: macro calls across sibling modules
    keep no edge.

### D4. What to do with the non-visibility refusals found here?

- **Today:** the linker drops 171 engine answers on the kind rule (TS calls of
  const-bound functions and of classes/types, Rust tuple-struct constructors
  `IgnoreMatch(...)`) and 114 on gin's build-tag twin declarations
  (`binding.go` / `binding_nomsgpack.go`). That is more than the 62 visibility
  refusals. t02 is the fixture for the kind rule.
- **Change / consequence per option:**
  - **Open two follow-up tasks: kind rule for engine answers, and build-tag twins (Recommended).**
    Benefit: each gets its own design (e.g. accept `Variable`/`Type` targets
    for semantic `CALLS`, or link to all twins). Risk: tracker growth; neither
    is in 4.7.0.
  - **Record only, no tasks.** Benefit: no scope added. Risk: the larger loss
    stays invisible outside this note.

## Appendix: S3 breakdown query

Run after the CTEs of `GM-519-count-refusals.sql` (everything before its first
final `SELECT`), against the same `index.db`:

```sql
, other AS (
  SELECT c.edge_id, c.language, c.source, c.edge_kind
  FROM classified c WHERE c.outcome = 'refused_other'
),
vis AS (
  SELECT o.edge_id, o.language, o.source, o.edge_kind, n.kind AS dkind, n.nativeKind AS dnk
  FROM other o
  JOIN candidates cd ON cd.edge_id = o.edge_id AND cd.visible_rule
  JOIN nodes n ON n.id = cd.decl_id
),
per_edge AS (
  SELECT edge_id, language, source, edge_kind, COUNT(*) AS n_vis,
    SUM(CASE WHEN edge_kind = 'CALLS' AND dkind = 'Function' THEN 1
             WHEN edge_kind = 'SUPERTYPE_OF' AND dkind IN ('Type', 'Function') THEN 1
             WHEN edge_kind = 'REFERENCES' THEN 1 ELSE 0 END) AS n_fit,
    GROUP_CONCAT(DISTINCT dkind || '/' || COALESCE(dnk, '-')) AS kinds
  FROM vis GROUP BY edge_id
)
SELECT language, source, edge_kind,
  CASE WHEN n_fit = 0 THEN 'kind_mismatch'
       WHEN n_fit >= 2 THEN 'ambiguous_at_address'
       ELSE 'one_fitting_visible_decl' END AS sub,
  kinds, COUNT(*) AS edges
FROM per_edge GROUP BY 1, 2, 3, 4, 5 ORDER BY 1, 2, 4, 6 DESC;
```

Result: TS 82 `kind_mismatch`, Go 114 `ambiguous_at_address`, Rust 51
`kind_mismatch`, 0 `one_fitting_visible_decl` anywhere. The `file`/`container`
rows were listed with their candidate's file, kind and visibility, and the
same-file split compares `placeholder_targets.fromFile` with the candidate's
`filePath`.
