# `g-mesh deps --json`: format and behaviour

`g-mesh deps --json` prints every file-level import edge g-mesh has indexed
for one project as a single JSON document on stdout. Format version **1**.

## Command

```
g-mesh deps --json [--timeout <SECONDS>] [--project <DIR>]
```

| Flag | Meaning |
|---|---|
| `--json` | Required. JSON is the only format; `g-mesh deps` without it is a usage error (exit 2). |
| `--timeout <SECONDS>` | How long to wait for the daemon and the index. Default `600`; `0` waits without limit. |
| `--project <DIR>` | The project root. Default: the current directory. `$CLAUDE_PROJECT_DIR` is never consulted. |

The root is canonicalised. A folder of projects (a directory g-mesh would
serve as a front, see `g-mesh debug-candidates`) is refused with exit 3
before anything is started.

## How the data is obtained

1. The project's daemon is reached, or started, exactly as `g-mesh mcp-shim`
   does (a daemon from an older build is replaced first). A cold start prints
   one `g-mesh mcp-shim: nothing is serving ... - starting a daemon for it`
   line on stderr.
2. The daemon is asked to build the index, as a session's first tool call
   does, and `deps` waits until the structural index is ready.
3. The index database is read read-only, in one transaction: the document is
   one consistent snapshot even while the daemon keeps writing.

While waiting, a line is printed to stderr every 10 s:
`g-mesh deps: waiting for the index of <root> (<phase or progress>)`.

The `--timeout` deadline covers steps 1 and 2, including a wait for a running
`g-mesh reindex`/`init` (retried every 500 ms). Reading and printing are not
bounded. Before the timeout is honoured, the daemon is reached (started if
need be) and asked to build the index, for up to 15 s past the deadline. A
timeout after that request leaves the daemon running and indexing, so the next
run is faster. If the daemon could not be reached and asked even then, the
message says so: indexing may not have started, and the next run starts it.

stdout carries only the JSON document: one compact line, then a newline. Every
other message goes to stderr.

## Exit codes

| Code | Meaning | stdout | stderr |
|---|---|---|---|
| 0 | Document printed. Some languages may be `plugin_absent` or `failed`: see `coverage`. | the document | one line per language not indexed |
| 1 | Unexpected error (I/O, SQL, malformed state) | empty | `g-mesh: <error chain>` |
| 2 | Usage error (unknown flag, missing `--json`) | empty | clap's message |
| 3 | Not a project: a folder of projects, or not an existing directory | empty | `g-mesh: <root> is a folder of N projects; run deps inside one of them` |
| 4 | Index unavailable: the daemon did not start or accept, the walk failed, the endpoint path is too long, or the daemon kept disappearing | empty | `g-mesh: the index for <root> is unavailable: <reason> (daemon log: <path>)` |
| 5 | Timed out before the index was ready | empty | the request reached the daemon: `g-mesh: the index for <root> is still being built (<progress>); waited <t>. The daemon keeps indexing; run again.` It did not: `g-mesh: the daemon for <root> was not reached and asked to build the index in time (<progress>); waited <t>. Indexing may not have started; run again.` |

Code 2 is not used for "some languages failed" here (unlike `init`/`reindex`):
a partly indexed project still prints a usable document with exit 0.

A project where no installed plugin covers any of its languages has a valid,
empty index: exit 0, `languages: []`, and `coverage` naming the
`plugin_absent` languages.

## Schema (version 1)

```json
{
  "version": 1,
  "project_root": "/Users/me/src/app",
  "languages": ["python", "rust", "typescript"],
  "coverage": {
    "go":         {"status": "plugin_absent", "files": 63},
    "python":     {"status": "indexed", "files": 56, "edges": 140, "unresolved": 2, "external": 31, "extensions": [".py", ".pyi"]},
    "rust":       {"status": "indexed", "files": 40, "edges": 95, "unresolved": 0, "external": 12, "extensions": [".rs"]},
    "typescript": {"status": "indexed", "files": 67, "edges": 210, "unresolved": 3, "external": 44,
                   "extensions": [".cjs", ".cts", ".js", ".jsx", ".mjs", ".mts", ".ts", ".tsx"]}
  },
  "unresolved": {"python": 2, "typescript": 3},
  "edges": [
    {"from": "pkg/cli.py",    "to": "pkg/config.py", "kind": "file", "language": "python",     "module": "pkg.config"},
    {"from": "server/main.go","to": "server/render", "kind": "dir",  "language": "go",         "module": "example.com/app/server/render"},
    {"from": "src/a.ts",      "to": "src/b.ts",      "kind": "file", "language": "typescript", "module": null},
    {"from": "src/main.rs",   "to": "src/config.rs", "kind": "file", "language": "rust",       "module": "app::config"}
  ]
}
```

(Pretty-printed here; the command prints it on one line.)

| Field | Type | Meaning |
|---|---|---|
| `version` | integer | The format version, `1`. |
| `project_root` | string | Absolute canonical path of the root every path is relative to. |
| `languages` | array of strings | Sorted ids of the languages whose outcome is `indexed` (g-mesh plugin ids: `typescript`, `rust`, `python`, `go`). Trust the edges for these languages and no others. |
| `coverage` | object | One entry per language the last walk recorded an outcome for, keyed by language id (keys sorted). |
| `coverage.<lang>.status` | string | `indexed`, `plugin_absent` or `failed`. |
| `coverage.<lang>.files` | integer | `indexed`: files in the index. `plugin_absent`: files found, when counted (else absent). |
| `coverage.<lang>.edges` | integer | `indexed`: rows in `edges` whose `language` is this one. |
| `coverage.<lang>.unresolved` | integer | `indexed`: imports that look in-project but name nothing the index holds; distinct `(from, specifier)` pairs. |
| `coverage.<lang>.external` | integer | `indexed`: imports of packages outside the project (`react`, `std::fs`, `os`); distinct `(from, specifier)` pairs. Not unresolved. |
| `coverage.<lang>.extensions` | array of strings | `indexed`: the file extensions the installed plugin claims, sorted. `typescript` covers JavaScript files too. Absent when the plugin's manifest could not be read. |
| `coverage.<lang>.error` | string | `failed`: why, on one line. |
| `unresolved` | object | `{lang: n}` for indexed languages with `n > 0`; the same numbers as `coverage`. |
| `edges` | array | One row per distinct `(from, to)` pair, sorted by `(from, to, kind)` in byte order. |
| `edges[].from` | string | The importing file. |
| `edges[].to` | string | The imported file, or a directory when `kind` is `dir`. |
| `edges[].kind` | string | What `to` names: `file` or `dir`. Every row is an import in version 1. |
| `edges[].language` | string | The importing file's language. |
| `edges[].module` | string or null | The module/package key the import resolved to (Rust module path starting with the crate's name, e.g. `app::config` for `use crate::config` in crate `app`; Python module; Go package import path); `null` for a file-to-file import (TypeScript/JavaScript). It equals the `qualifiedName` of the row `get_dependencies` returns for that import. |

`line` (the 1-based line of the import in `from`) is **not emitted** in
version 1: the index does not store an import's position. The name is
reserved for that meaning.

### How a target becomes `to`

For every import of a file:

1. Resolved to a file -> `kind: file`, `to` = that file.
2. Resolved to a module or package -> the file(s) that define it. One file ->
   `kind: file`. Several files in one directory (a Go package) -> `kind: dir`,
   `to` = that directory. Files in several directories -> one `kind: file` row
   per file. A module no file defines (it declares nothing of its own) leads to
   the file whose own module declaration it is; else to the files holding its
   members; else, when its only members are sub-modules (a Python package
   with an empty `__init__.py`), to the sub-modules' common directory's
   `__init__.py` when indexed, or to that directory (`kind: dir`, a namespace
   package). No file -> counted as unresolved.
3. An external package -> counted in `external`, no row. A plugin marks
   only a non-relative specifier external: a relative one (`./missing`,
   `.x` in Python) that names no file is a target core could not link, step 4.
4. Anything else -> counted in `unresolved`, no row.

Rows with `from == to` are dropped, and so are `dir` rows whose `to` is the
directory `from` sits in (an import of the importer's own package). Two imports of the same target from one
file collapse into one row.

### Paths

Project-relative, `/`-separated on every OS, no leading `./`, no trailing `/`
on directories, `.` for the root directory.

### Stability

Two runs over an unchanged index print byte-identical output: arrays and
object keys are sorted as stated above.

## Versioning

- Adding a key, or a new value of `kind` or `status`, does **not** change
  `version`. Consumers must ignore unknown keys and treat an unknown `kind`
  or `status` as unknown.
- Removing or renaming a key, or changing a field's meaning or type, bumps
  `version`.

| Version | g-mesh | Change |
|---|---|---|
| 1 | 4.17.0 | Initial. |
