# GM-594: engine CALLS onto enum types, before and after

Base: cached release-4.10.0 index (`d8fcc50`, ripgrep). After: `fix/GM-594-engine-calls-on-enums`
at `072bc53`, release build, cold reindex of ripgrep `e89fff8` (clean checkout), semantic pass
completed, rust-analyzer 1.97.1. Plugin roots overridden. Edges compared by address
(file, qualified name, kind, source, target), not by id.

Note: the branch also contains the GM-593 merge (`aad0515`), which `d8fcc50` lacks, so the GM-593
self-loop drops show up in the diff.

## Against the design note (section 6)

| measure | expected | before | after |
|---|---|---|---|
| semantic CALLS -> Type/enum | 3 -> 0 | 3 | 0 |
| semantic REFERENCES -> Type/enum | 6 -> 0 | 6 | 0 |
| qualifier landings (semantic edge onto `X` from a pending placeholder named `X`, target an enum) | 9 -> 0 | 9 | 0 |
| semantic CALLS -> Type/struct | 48 unchanged | 48 | 48 |
| placeholder `ContextMode::Limited` | present | 1 | 1 |
| `set_before` -> `ContextMode::Limited` edge | present | 0 | 1 (syntactic, pending) |

## Other deltas (22257 -> 22251 edges)

| edge kind / source | before | after | delta |
|---|---|---|---|
| CALLS semantic | 1782 | 1770 | -12 |
| REFERENCES semantic | 204 | 197 | -7 |
| CALLS syntactic unresolved | 446 | 449 | +3 |
| REFERENCES syntactic unresolved | 417 | 426 | +9 |
| REFERENCES syntactic resolved | 5366 | 5367 | +1 |
| all others | | | 0 |

19 edges only in base, 13 only in after.

- Semantic CALLS -12: 3 are the enum landings; 9 are caller-to-itself edges (GM-593 fix, ripgrep
  self-loops 15 -> 6, as in `gm-593-self-loop-calls.md`).
- Semantic REFERENCES -7: 6 are the enum landings; 1 is `hyperlink::<HyperlinkFormat as
  FromStr>::from_str` onto itself (not part of GM-593's CALLS scope; most likely its `State::*`
  path patterns no longer bind, see the 3 new placeholders below; not checked further).
- Syntactic unresolved +12: the 9 sites now keep their own placeholder (`ContextMode::Limited` x3
  from `set_after/set_before/set_both`, `SearchMode::Count`, `SearchMode::CountMatches`,
  `ColorChoice::Never`, `Error::InvalidDefinition`, `Match::None` x2) plus 3 more in `from_str`
  (`State::InVariable`, `State::OpenVariable`, `State::VerbatimCloseVariable`).
- Syntactic resolved REFERENCES +1: `color::<UserColorSpec as FromStr>::from_str` -> `color::Style`,
  the expected extra structural edge from a path pattern that no longer binds a local.
- Nothing retargeted; no other semantic edge lost.

## Machine state

Reindex: real 32.51 s, user 5.38 s, sys 2.03 s. Load averages 125.11 86.17 66.44 before,
78.19 78.52 64.35 after (heavily loaded; wall time is not a comparison).
Counts are deterministic and do not depend on it.
