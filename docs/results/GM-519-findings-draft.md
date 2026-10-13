# GM-519 findings draft (S1): member visibility and the linker's `file` rule

Scratch notes for S3. Not a recommendation.

## Setup

- Binary: installed g-mesh **4.6.0** (`~/.local/share/g-mesh/bin/g-mesh`) with
  its bundled typescript/python/rust plugins (option A has shipped since 4.0.0);
  plugin manifests from this worktree (`plugins/<lang>`); Go plugin 0.5.0 from
  the main checkout's build (`../g-mesh/plugins/go`).
- Semantic tiers on PATH: vtsls 0.3.0, pyright 1.1.414, rust-analyzer 1.97.1,
  go/types (go on PATH).
- Fixture convention: the conformance kit's (`plugins/<lang>/conformance/project`
  + `expect.toml`). Each case is
  `docs/results/GM-519-fixtures/<plugin>/<case>/{project/,expect.toml}`, and
  `expect.toml` asserts the **correct** graph, so FAIL = bug.
  `GM-519-fixtures/run-all.sh <g-mesh> <main checkout> <out dir>` runs them all
  through `g-mesh plugins check` and writes `summary.txt`.
- Cause of each FAIL: the fixture copied to a scratch dir, `g-mesh reindex`
  (runs the semantic pass), then `GM-519-count-refusals.sql` on its index.
- Structural-only arm: not runnable through the kit. Without a semantic tier on
  PATH, `plugins check` fails its `session` check and skips every expectation.
  Every result below is with the semantic tier on.

## Results

| case | outcome | graph shows | cause |
|---|---|---|---|
| ts/t01 import of a const that an exported class's method shares a name with | **wrong link** | `run -> Router#handler` (CALLS, syntactic) | option A: `handler` is a public member now. The `name` walk finds the Variable `handler` and the Function `Router#handler`, and CALLS keeps only Functions, so the member wins alone. vtsls is never asked about the call (no open site), so nothing corrects it |
| ts/t01c control: t01 with `Router` not exported | correct (no link) | edge unresolved | member is `file`, so the `file` rule refuses it. This is the pre-option-A shape, and it shows option A causes t01 |
| ts/t02 t01 through `export * from "./x"` | no wrong link, but a **missing link** | the structural edge was retracted; the semantic answer (`handler`, a Variable) is `refused_other` | the semantic answer lands on a Variable, and CALLS needs a Function. That is the kind rule, not visibility |
| ts/t03 call through an exported instance of a non-exported class (`export const service = new ServiceImpl()`) | **missing link** | `boot -> ServiceImpl#start` stays on its placeholder | `refused_visibility_file`: the member is `file` and the bridge addresses `Scope::File` |
| ts/t04 receiver call on a local class **in the same file** | **missing link** | `tally -> Counter#bump` unresolved | `refused_visibility_file`: the `file` rule needs `Scope::Container` even from the same file, and TS has no containers, so a `file` member is never reachable by a semantic answer |
| ts/t05 `private` member, `other.raw()` from the same class and file | correct | | passes; which tier linked it was not inspected |
| ts/t06 interface member added by module augmentation (`declare module "./base" { interface Widget { glow() } }`) | **missing link** | `show -> ./base.Widget#glow` unresolved; `w.draw()` linked | `refused_visibility_file`: the augmenting interface is not `export`ed syntactically, so its members are `file` |
| ts/t07 `export namespace Geo { export function area; export class Circle { radius() } }` | correct | | |
| ts/t08 `protected` method called from a subclass in another file | correct | | `protected` follows its owner, so the member is public |
| ts/t09 same method name on two unrelated exported classes | correct, no spurious link | | structural receiver calls are not linked by name (`receiver_calls_structural = "unresolved"`) |
| ts/t10 overloaded method next to a top-level function of the same name | correct | `both -> Formatter#format` | the top-level `format` cannot be anchored by name in `expect.toml` (shares a file with the member), so it is not asserted |
| ts/t11 static `Registry.create()` via an imported class | correct | | |
| ts/t12 method of a non-exported class reached through `export type Motor = Engine` / a returned instance | **missing link** | `go -> Engine#ignite` unresolved | `refused_visibility_file`, same as t03 |
| rust/r01 `macro_rules!` in `#[macro_use] mod macros`, used in sibling `mod user` | **missing link** | `user::doubled -> macros::twice` unresolved | `refused_visibility_container`: an unexported macro gets `container(crate::macros)`, but `#[macro_use]` scopes it textually to everything after the `mod` line, siblings included. The bridge's scope (container) is right; the visibility mapping is wrong for macros |
| rust/r02 private method, receiver call from a child module and the own module | correct | | `container(<own module>)` and the child is on the parent chain |
| rust/r03 `pub(crate)` method from a sibling module | correct | | |
| rust/r04 default method of a private trait, called from a child module | correct | | trait items inherit the trait's visibility |
| rust/r05 `pub(super)` method in an `impl` inside a child module | correct | | |
| rust/r06 same method name on two types | correct | | |
| rust/r07 private `const fn` called from a `const` initializer and a nested module's `const` | correct | | |
| python/p01 `from pkg.x import handler` where `handler` is a module variable and `Router.handler` a method | **wrong link** | `run -> Router.handler` (CALLS, syntactic) | the same shape as t01. Python marks every member `public` (keys.rs), so this is not caused by option A; it predates it |
| python/p02 same method name on two classes | correct | | |
| go/g01 unexported method from another file of the package; external `_test` package calls an exported function | correct | | go/types addresses `container(<import path>)` with `fromContainer`, and Go containers are flat |

**Counts:** typescript 13 cases: 1 wrong (t01), 5 missing (t02 by the kind
rule; t03, t04, t06, t12 by the `file` rule), 7 correct (t01c control
included). rust 7: 1 missing (r01), 6 correct. python 2: 1 wrong (p01, not
option A), 1 correct. go 1: correct.

## What this answers so far

- **Option A side effects:** one spurious-link shape is reproduced (t01). An
  import's `name` walk now sees same-named public members, and the CALLS
  kind filter turns "Variable + member" into "member only". Not reproduced:
  unrelated same-named methods (t09, receiver calls are not structural),
  overloads (t10), namespaces (t07), protected (t08). Declaration merging
  across exported/non-exported halves is a TS compile error, so it was not
  built. Module augmentation (t06) is a missing link, not a spurious one.
- **Which plugins hit the `file` refusal today:** only TypeScript. Python
  never emits `file` (everything is public), Go emits `public`/`container`,
  and Rust emits `file` only on re-export rows. TypeScript's remaining `file`
  members (non-exported owners, private members, augmentations) are
  unreachable by **any** semantic answer, the same file included (t04),
  because `visible` admits `file` only under `Scope::Container` and the bridge
  addresses TS (no containers) with `Scope::File`.
- **Rust bridge scope vs `container` visibility:** consistent in r02-r07. The
  one mismatch found is the visibility mapping, not the scope: `macro_rules!`
  textual scoping (r01).
- **Other silently dropped semantic answers:** t02 (`refused_other`). A
  semantic answer onto a Variable for a CALLS edge is dropped by the kind
  rule, and the structural edge it contradicted is retracted, so the call has
  no edge at all.

## S2 counting method (no instrumentation)

`docs/results/GM-519-count-refusals.sql` reads a finished index. An edge left
on its `pending_symbol` placeholder (`resolved = 0`, `linkedFrom IS NULL`)
whose `qualifiedName` address has a declaration in the index was refused. The
query re-applies `Resolver::visible`/`sees` (parent chains via a recursive CTE
over `containers`) and splits these edges into `refused_visibility_file`,
`refused_visibility_container`, `refused_other` (ambiguity or kind) and
`no_declaration_at_address`, per language and tier, with the `linked` count
as denominator. A second statement counts `name`-keyed links that landed on a
type member (the t01/p01 shape, an upper bound to read by hand).

Dry run (the t03 copy plus t01's `x.ts` and a caller, reindexed with 4.6.0):
`typescript | semantic | refused_visibility_file | 1`,
`typescript | syntactic | linked | 2`, and the second statement
`typescript | CALLS | 1`. Both match the fixture's one refusal and one wrong
link. The same query classified every FAIL in the table above.

Commands for S2, per corpus (warm checkouts in
`$(node -e 'console.log(require("os").tmpdir())')/gmesh-bench-corpora/{excalidraw,gin,ripgrep,py-requests}`,
registry `../g-mesh-bench/corpora/registry.json`). Put each plugin's language
server on PATH first: vtsls and pyright from `../g-mesh/plugins/{typescript,python}/node_modules/.bin`,
rust-analyzer from `~/.cargo/bin`, and `go`.

```
cd <corpus>
g-mesh reindex                       # wipes, rebuilds, runs the semantic pass to completion
g-mesh status --json | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["stateDir"], d["index"]["semanticPass"])'
sqlite3 -header -column <stateDir>/index.db < <g-mesh worktree>/docs/results/GM-519-count-refusals.sql
g-mesh stop
```

Do not pass `-readonly`: after `reindex` the WAL side files are gone, and a
read-only open then fails with "unable to open database file". The semantic
pass must report `completed: true`; otherwise answers are missing, not
refused. Record `uptime` and `/usr/bin/time -p` with the reindex.
