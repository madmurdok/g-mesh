# GM-503: answers about a path in an absent or failed language say why they are empty

Design note for ADR 0022, section 1, owed item (c). Status: proposed (GM-503/S1).

## Problem

The instructions are read once per session. An agent that skipped them (a
subagent, deferred tool loading) and asks `get_file_outline` about `tools/gen.py`
in a project with no Python plugin gets
`g-mesh: no file 'tools/gen.py' found in the index`, the same text a typo gets.
Nothing in the answer says that g-mesh cannot see Python files at all, or how
to fix it. `languages::absent_for_path` was written for this and has no caller.

## Facts (with the call that established each)

- `languages::absent_for_path` has no callers: `find_callers(absent_for_path)` -> `results: []`, `hasMore: false`.
- `Coverage::from_outcomes` is called only by `GMeshMcpServer::instructions` (core/src/mcp/mod.rs) and the test helper `instructions::tests::warm_real`: `find_callers(from_outcomes)`.
- `PluginRegistry::missing_languages` is called only by `GMeshMcpServer::instructions`: `find_callers(missing_languages)`.
- `manifest::discover` is called in production by `daemon::run` (core/src/daemon/mod.rs:264, once at startup), `cli::init`, `cli::reindex`, `cli::status::collect` and `daemon::plugin::discovered_fingerprint` (memoized `OnceLock`); the rest are tests: `find_callers(discover)`. The daemon never rediscovers: `PluginRegistry.discovered` is fixed for its lifetime (registry.rs doc of `query_shapes`, `receiver_call_capabilities`).
- `daemon::run` stamps the index with `registry::indexer_version(&discovered)`, which hashes every discovered plugin; `schema::ensure_current` wipes the index when it differs (mod.rs:264-275, read). Removing a plugin changes the digest, so the next daemon starts from an empty index.
- The registry keeps the failed set in memory: `set_failed_languages` (after each walk), `seed_failed_languages` (startup, from `language_outcome`), `is_failed_language` (registry.rs:606-640, read). ADR 0021 section 2: nothing of a failed language is in the index.
- Path-anchored answers, each read: `get_file_outline::handle` errors `no file '{path}' found in the index` on a miss (get_file_outline.rs:107-125); `find_definition::by_position` errors `no symbol found at {path}:{line}:{col}` (find_definition.rs:444-458); `get_dependencies::from_file` falls through container keys and entry-point inference, then errors with `no_file_message` (get_dependencies.rs:405-440, 679-709). All three are tool errors with one text block (`tool_result::error`).
- `file_paths` filters exist on `SymbolQueryParams` (find_references, find_callers, find_callees) and `FindImplementationsParams` (ignored when `transitive`): mod.rs:904-960 (read). `SearchCodeParams` has no path parameter (mod.rs:1005-1018), so `search_code` is not path-anchored.
- Session lifetime: in a single-project session, the end of the daemon's connection ends the session (`shim/router.rs` `Event::Done`, "the session is over"). Only a folder (front) session survives a daemon restart: a later `select_project` connects to a new daemon and replays `initialize` (router.rs module doc; `multi_project_front.rs::reselecting_the_same_project_refreshes_its_guidance`). ADR 0022 section 2's "a daemon restart that the session survives" is therefore the folder case only.
- Existing response-level disclosures are camelCase, `skip_serializing_if`, flattened into `answer::Summary` through the tool's `*Disclosures` struct (answer.rs:17-42; `provenance` in find_callers_callees.rs).

g-mesh note: all calls above answered from the main checkout `g-mesh` (this branch changes no code); no call failed.

## Recommendation

### 1. Shape

One type, `mcp::not_indexed::NotIndexed`, rendered two ways:

```rust
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(super) struct NotIndexed {
    pub language: String,
    pub reason: NotIndexedReason,           // "pluginAbsent" | "pluginFailed"
    pub command: String,                    // `g-mesh plugins install <lang>` | `g-mesh reindex`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,              // failed only: innermost cause, as the instructions show it
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub file_paths: Vec<String>,            // filter answers only: which requested paths it covers
}
```

- **Error answers** (the three path-anchored tools; every one of them is a
  tool error on a miss): a tool error (`isError: true`) whose single content
  block is a JSON object, built by `not_indexed::refusal`:
  `{"error": "<the tool's miss message> + NotIndexed::sentence()", "notIndexed": {...}}`.
  `notIndexed` is one `NotIndexed` value without `filePaths`:
  - absent: `{"language":"python","reason":"pluginAbsent","command":"g-mesh plugins install python"}`
  - failed: `{"language":"python","reason":"pluginFailed","command":"g-mesh reindex","error":"<innermost cause>"}`
    (`error` is omitted when no outcome row was recorded for the language).
  The `error` text keeps the tool's unchanged miss message and appends the
  human sentence, which reuses ADR 0022 section 3's wording for the two
  states, so an agent that did read the instructions sees the same words:
  absent: `` - python files are not indexed here: no plugin is installed (`g-mesh plugins install python`), so this is not evidence the file is empty or missing.``
  failed: `` - python files are not indexed here: its plugin failed ({error}); fix the plugin, then run `g-mesh reindex`. This is not evidence the file is empty or missing.``
  A path in a covered language, or an unsupported, extensionless or excluded
  path, gets the tool's plain text error unchanged.
- **Success answers** (`file_paths` filter): a response-level field
  `notIndexed: [NotIndexed, ...]`, one entry per language, `filePaths` naming
  the requested paths it covers (request order, duplicates dropped; entries
  ordered by the first requested path of each language); absent when empty.
  The field is last, after `provenance`. It joins each tool's
  `*Disclosures` struct, so `answer: files|count` carry it too.

Alternatives:
- *Plain text sentence appended to the error*: simplest, but not machine-readable
  and unlike the success answers' field. Not chosen; the error is a JSON body.
- *Turn the miss into a success with `results: []` plus `notIndexed`*: makes
  the field literal, but turns "this file is not in the index" into an empty
  outline, which is the false statement this task removes. Rejected.
- *Once-per-session hint* (`session_hints`): wrong channel; the fact is about
  this answer's path, and hints are for how to read a field.

Name: `notIndexed`, after the instructions' "Not indexed, ..." wording.
Alternatives `absentLanguage` (wrong for failed), `coverage` (too vague).

**Conflict risk with GM-502 and GM-527** (both touch core/src/mcp in this
batch): this task edits the handler wrappers in `mcp/mod.rs` (each gains one
line computing `not_indexed`), `get_file_outline::handle`,
`find_definition::{by_position, handle_in}`, `get_dependencies::{handle,
from_file}` signatures, and (slice S3) the page/disclosure structs of
find_references, find_callers_callees, find_implementations. The edits are
additive (a new parameter, a new optional field); the risk is textual
conflicts in the same functions, not semantic ones. Land whichever is first
and rebase; the orchestrator should check whether GM-502/GM-527 change these
signatures.

### 2. Which tools carry it

| Tool | Anchor | Carries |
|---|---|---|
| `get_file_outline` | `file_path` | yes, on the "no file" error |
| `find_definition` | `file_path` + `position` | yes, on the "no symbol at" error; never for `symbol_id`/`symbol_name` |
| `get_dependencies` | `file_path` | yes, on the "no file" error; never for `module_id`/`resume_token` |
| `find_references`, `find_callers`, `find_callees` | symbol; `file_paths` filter | yes, `notIndexed` for filter paths in an uncovered language |
| `find_implementations` | symbol; `file_paths` filter | same, direct mode only (`transitive` ignores the filter) |
| `search_code` | free text | no: no path parameter |

`find_definition`'s `by_file_name` (a `symbol_name` that names a file) is left
out: it is a name lookup, and guessing a language from a name is out of scope.

### 3. How the daemon knows (no rediscovery)

Read the registry the daemon already has, per call, with no I/O:
`registry.discovered` (fixed for the daemon's life) for absent, and
`is_failed_language` (in-memory, replaced by every walk) for failed. Cost: one
extension lookup and one `HashSet` probe per path. The failed `error` is read
from `language_outcome` only on the miss path of a failed language (one small
query under the store read the handler already holds).

Why this is enough for a mid-session removal: a plugin removed under a running
daemon changes nothing that daemon serves (its manifests are loaded, its rows
stay in the index, its answers stay non-empty and true as of the last update).
The removal takes effect at the next daemon start, which wipes the index (the
digest changed) and discovers without the plugin, so that daemon's registry
is exactly right. The session that sees "indexed" in stale instructions and
then gets answers from the new daemon is the folder session that reselects
the project; any agent on that connection that did not read the reselect
guidance is the one the field is for.

Alternatives:
- *Rediscover per call* (`manifest::discover` reads every plugin root and
  parses each `plugin.toml`, a few ms of I/O per call): would report "absent"
  while the running daemon still holds and answers that language's rows, a
  contradiction. Rejected.
- *Cache with invalidation / watch the plugin roots*: same contradiction plus a
  watcher. Rejected.

### 4. A removed plugin whose files are still indexed

The field is computed only on a miss (or, for filters, only for paths in an
uncovered language), so it never sits next to rows from the same file. By
construction it cannot: absent means the plugin was not discovered at startup,
so the index was wiped and re-walked without it; failed means ADR 0021 removed
the language's rows. The one window where a removed plugin's files are still
indexed (plugin directory deleted under a running daemon) is the window where
discovery still lists it, so `absent_for_path` returns `None` and no field
appears: the rows answer. For filters, `notIndexed` can sit beside rows from
other, covered files; that is intended (it says which of the requested files
had no chance of matching).

### 5. Precedence

They cannot both hold: absent = no discovered manifest for the file
(`absent_for_path`), failed = a discovered manifest whose language is in the
failed set. Order of the check: (1) `discovered.language_for(path)` is
`Some(lang)` -> failed if `is_failed_language(lang)`, else covered (no field);
(2) `None` -> `absent_for_path(discovered, path)` -> absent, else no field
(unsupported or extensionless). Discovery wins over any stale recorded
outcome: a recorded `Failed` for a language with no manifest reads as absent,
because installing is the fix.

Exclusions: `absent_for_path` ignores the catalogue entry's `exclude_dirs`, so
`venv/x.py` would be told to install Python, which would not index it either.
The gate applies `under_excluded_dir(entry.exclude_dirs)` after it, matching
`count_absent_files` (decision 5).

## Edit map

New:
- `core/src/mcp/not_indexed.rs`: `NotIndexed`, `NotIndexedReason`, `NotIndexed::absent(entry)`, `NotIndexed::failed(language, error)`, `sentence()`, `group(paths)` (one entry per language with `filePaths`), unit tests' home.

Change:
- `core/src/daemon/registry.rs`: new `PluginRegistry::path_coverage(&self, file_path) -> Option<(String, NotIndexedReason-like)>` beside `is_failed_language` (637-640), using `language_for` (650-652) and `languages::absent_for_path`; no I/O. Read for context: `missing_languages` (778-783), `seed_failed_languages` (612-635).
- `core/src/languages.rs`: `absent_for_path` (160-171) / `absent_for_path_in` (201-214): apply the entry's `exclude_dirs` (or do it in the registry method; decision 5).
- `core/src/mcp/instructions.rs`: `error_cause` (240-256) to `pub(super)` so the field's `error` matches the instructions.
- `core/src/mcp/mod.rs`: `get_file_outline` (800-815), `find_definition` (671-696), `get_dependencies` (817-839): compute `Option<NotIndexed>` kind from `self.registry.path_coverage(path)` and pass it in; `find_references` (698-717), `find_callers` (719-744), `find_callees` (746-774), `find_implementations` (776-798): compute it per `file_paths` entry (S3). `mod not_indexed;`.
- `core/src/mcp/get_file_outline.rs`: `handle` (107-125): on the miss, answer with `not_indexed::miss` (JSON body; reads the failed error under `conn`).
- `core/src/mcp/find_definition.rs`: `handle_in` (1216-1252) passes it to `by_position` (444-458), appended on the miss only.
- `core/src/mcp/get_dependencies.rs`: `handle` (784-808) and `from_file` (405-440): when the path is uncovered and `find_file_node` misses, refuse with the `no file` message plus the sentence before the container-key and entry-point fallbacks (a path with a known language extension is a file path, not a key; must-confirm 3).
- S3: `find_references.rs` `ReferencePage` (81-) / `ReferenceDisclosures` (136-) / `handle_in` (289-); `find_callers_callees.rs` `CallerPage` (171-), `CalleePage` (354-), `CallerDisclosures` (389-), `CalleeDisclosures` (403-), `handle_callers_in` (513-), `handle_callees_in` (687-); `find_implementations.rs` `ImplementationPage` (77-), `handle_in` (173-), `dispatch_in` (571-): add `#[serde(skip_serializing_if = "Vec::is_empty")] not_indexed: Vec<NotIndexed>`.
- `core/src/storage/schema.rs` `language_outcomes` (893-): read only; a one-language accessor is optional.

Context only: `answer.rs` `Summary` (17-42), `provenance.rs` `Provenance` (198-220) for the field conventions; `daemon/mod.rs` 264-275 (startup discovery and wipe).

## Slices proposed

- S2 code (opus): not_indexed.rs, registry gate, the three error paths.
- S3 code (opus): the `file_paths` field on the four find tools (cuttable; decision 2).
- S4 tests (opus), S5 verify (opus).

## Behaviours for the tests slice

1. `get_file_outline` on `x.py` with no Python plugin: tool error whose JSON body has `notIndexed` (`pluginAbsent`) and whose `error` names `python`, "no plugin", and `` `g-mesh plugins install python` `` (`get_file_outline::handle`, `NotIndexed::sentence`).
2. Same for `find_definition` with `file_path: x.py` + `position` (`find_definition::by_position`).
3. Same for `get_dependencies` with `file_path: x.py`, and no entry-point substitution happens (`get_dependencies::from_file`).
4. Failed language (discovered, in the failed set): the same three carry "plugin failed", the innermost cause and `` `g-mesh reindex` ``, not the install command.
5. A file of an indexed language: a hit carries no `notIndexed` key; a miss (`nope.rs`) carries no `notIndexed` and stays plain text (AC2).
6. An unsupported or extensionless path: plain text error, no `notIndexed`.
7. A path under the catalogue entry's `exclude_dirs` (e.g. `venv/x.py`): no `notIndexed` (if decision 5 is accepted).
8. Precedence: a language both recorded `Failed` and with no discovered manifest reads as absent (`PluginRegistry::path_coverage`).
9. `find_references`/`find_callers`/`find_callees`/`find_implementations` with `file_paths: ["src/lib.rs", "x.py"]`: `notIndexed` has one python entry with `filePaths: ["x.py"]`; with only `.rs` paths, no key; `answer: "count"` and `"files"` carry it too; `transitive: true` does not (S3).
10. `NotIndexed` serializes exactly (`language`, `reason`, `command`, optional `error`, optional `filePaths`; camelCase; absent fields omitted).
11. Mid-session removal: the field appears on the same MCP session after the plugin is removed (AC3, harness below).

## Test plan

Unit (in-process, no daemon): `not_indexed.rs` tests for 10 and the sentences;
`registry` tests for the gate (6, 7, 8) with a hand-built `DiscoveredPlugins`
(`languages::tests::discovered_with_excludes` pattern) and
`set_failed_languages`; handler tests for 1-5 and 9 calling
`get_file_outline::handle`, `find_definition::handle_in`,
`get_dependencies::handle` and the find `handle_in`s with an injected
`NotIndexed`.

Integration (real binary, `core/tests/absent_language_field.rs`, patterns from
`one_plugin_binary_missing.rs`: `shim()`, `text()`, `common::*plugin_root`):
- absent: plugin root with `common::add_real_rust_plugin` only; project with
  `src/lib.rs` and `tools/gen.py`; behaviours 1-3 and 5 over the shim.
- failed: `common::rust_and_missing_python_plugin_root()`; behaviour 4.
- **mid-session removal (AC3)**, extending `multi_project_front.rs`'s reselect
  harness (`select`, `guidance_of`, `outline_names`): a folder with project
  `a/` (`src/lib.rs`, `tools/gen.py`) and `b/`; a copied plugin root with
  Rust and a working Python plugin (`rust_and_missing_python_plugin_root` +
  `install_python_binary` from `one_plugin_binary_missing.rs`). One client:
  select `a`, outline `tools/gen.py` -> rows, guidance names python as
  indexed; select `b`; delete the plugin root's `python/` (or
  `g-mesh plugins remove python` with `G_MESH_PLUGIN_ROOTS_OVERRIDE`) and
  `g-mesh stop` in `a`; select `a` again on the same client; outline
  `tools/gen.py` -> tool error with `` `g-mesh plugins install python` ``.
  The single-project shim cannot host this test: its session ends with its
  daemon. Control: drop the sentence in `get_file_outline::handle` -> the
  last assertion fails. No order between processes is asserted beyond the
  test's own sequential calls; it runs 5 times (processes).

Controls (for verify): remove the append in each of the three handlers;
make the gate return `None` for failed; drop the `exclude_dirs` check; drop
the `notIndexed` field from one disclosure struct. Each must fail its test.

## Must confirm (owner)

1. Error answers carry a JSON body (`error` text plus `notIndexed`); decided by the owner.
2. The `file_paths` field on the four find tools is in scope (recommended: yes, as its own slice S3, cuttable).
3. `get_dependencies` refuses an uncovered-language file path before its container-key and entry-point fallbacks (recommended: yes).
4. The failed field includes the innermost error cause (recommended: yes, same text as the instructions).
5. The absent check honours the catalogue's `exclude_dirs` (recommended: yes).
6. AC3's "mid-session" is the folder-session reselect harness above; a single-project session cannot survive the restart a removal needs (recommended: accept, and correct ADR 0022 section 2's wording in S2 or a docs follow-up).
