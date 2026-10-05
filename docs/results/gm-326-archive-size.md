# GM-326/S4: release archive size, x86_64-apple-darwin

Measured 2026-10-06. Sizes are apparent bytes (stat %z), summed over files.

## Baseline substitution
`v3.7.0` is a git tag, but it has **no GitHub release and no assets** (`gh release view v3.7.0`: "release not found").
The GitHub releases with mac assets are v3.18.0 (Latest) and v3.19.0 (Draft). Both are ancestors of the GM-326 branch.
v3.19.0 is the immediate predecessor and the primary baseline. v3.18.0 is listed for reference.

## Sources
- old: `gh release download v3.19.0 / v3.18.0 --pattern '*x86_64-apple-darwin.tar.gz'` (CI-built)
- new: `g-mesh-v4.0.0-x86_64-apple-darwin.tar.gz`, built locally by the verify slice with `scripts/build-targets.sh`, no Node on PATH,
  from feat/GM-326-ship-rust-ts-plugin @ 1486740 (archive mtime 00:26; HEAD later moved to 5820631, a docs-only commit).
  Build log: real 23.43 / user 8.01 / sys 3.02 (incremental). rustc 1.97.1 locally. uptime at measurement: load 7.86 7.08 26.33.
  No rebuild was done for this slice.

## Totals
| metric | v3.18.0 | v3.19.0 | v4.0.0 (GM-326) | delta vs 3.19.0 |
|---|---:|---:|---:|---:|
| packed .tar.gz | 61,490,572 | 61,528,879 | 25,079,765 | -36,449,114 (-59.2%) |
| unpacked total | 181,362,991 | 181,442,229 | 70,399,472 | -111,042,757 (-61.2%) |
| file count | 55 | 55 | 13 | -42 |

## Per top-level entry
| entry | v3.18.0 | v3.19.0 | v4.0.0 | delta vs 3.19.0 |
|---|---:|---:|---:|---:|
| g-mesh (core binary) | 40,812,272 | 40,883,968 | 41,188,012 | +304,044 |
| plugins/typescript | 120,393,859 | 120,393,944 | 8,406,825 | -111,987,119 |
| plugins/go | 8,862,161 | 8,866,695 | 8,871,032 | +4,337 |
| plugins/python | 5,283,670 | 5,284,248 | 5,564,041 | +279,793 |
| plugins/rust | 5,934,558 | 5,935,256 | 6,292,232 | +356,976 |
| README.md | 63,425 | 65,072 | 64,284 | -788 |
| LICENSE + LICENSE-APACHE + LICENSE-MIT | 13,046 | 13,046 | 13,046 | 0 |

There is no `licenses/` dir: the top-level licenses are the three LICENSE* files. LICENSE-nodejs sat inside plugins/typescript.

## plugins/typescript contents
| file | v3.19.0 | v4.0.0 |
|---|---:|---:|
| g-mesh-plugin-typescript (Mach-O x86_64) | 115,427,488 (Node SEA blob) | 8,399,668 (Rust) |
| node_modules/ (41 files) | 4,819,814 | absent |
| -- tree-sitter-typescript | 3,719,332 (its .node prebuild: 2,890,240) | |
| -- tree-sitter-javascript | 649,294 (.node: 382,992) | |
| -- tree-sitter | 440,002 (.node: 404,792) | |
| -- node-gyp-build | 11,186 | |
| LICENSE-nodejs | 145,485 | absent |
| plugin.toml | 1,157 | 7,157 |
| **total** | **120,393,944** | **8,406,825** |

## Non-TS deltas (do not attribute to the TS change)
Core binary +304 KB, python plugin +280 KB, rust plugin +357 KB, go +4 KB vs 3.19.0. Together +0.94 MB.
These are most likely code changes since 3.19.0 plus a local build against a CI build (toolchain/flags may differ).
This run did not isolate them. The TS dir alone accounts for -111.99 MB of the -111.04 MB unpacked delta.
