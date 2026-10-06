# GM-353: per-plugin release assets

Status: **draft for owner review** (GM-353/S1). Nothing here is decided until the
owner answers the open questions at the end.

## Problem

`g-mesh plugins install <language>` (GM-331, specified in
`docs/architecture/plugin-distribution.md` § CLI) fetches a plugin from a GitHub
release with `install.sh`'s checksum discipline. A release has nothing for it to
fetch: `scripts/build-targets.sh` writes one archive per triple
(`g-mesh-v<version>-<target>.tar.gz`/`.zip`) and `.github/workflows/release.yml`
publishes nine assets (4 archives, 4 `.sha256`, `SHA256SUMS`). And
`scripts/install.sh:682` (`install.ps1:506` the same) refuses an archive with no
`plugins/typescript/plugin.toml`, which contradicts "an archive can arrive
without a given plugin".

## Facts this note relies on

| fact | source |
|---|---|
| `discover` scans `<root>/*/plugin.toml`, roots in order `~/.g-mesh/plugins/`, then `<exe dir>/plugins/`, then the checkout; first root wins per language | g-mesh `get_file_outline core/src/daemon/manifest.rs` (`default_roots`, `bundled_roots`, `discover`), then `sed -n` on those lines |
| a protocol mismatch in any manifest is an error out of `read_manifest`, which `discover` propagates with `?` - one stale plugin fails discovery as a whole | `manifest.rs:357` (`read_manifest`), `discover` body |
| every staged manifest's `command` is rewritten to `./<exe>`, resolved against the manifest's own dir (`resolve_command` -> `resolve_path_entry(value, dir)`), so a staged `plugins/<lang>/` is relocatable as is | `grep 'command = ' scripts/bundle-*.sh`; g-mesh outline + `sed` of `resolve_command` |
| the staged TS dir holds exactly `plugin.toml` + the binary (test `the_typescript_stage_holds_only_the_manifest_and_the_binary`) | `grep -n "fn " core/tests/release_packaging_scripts.rs` |
| every asset name consumer derives from `build-targets.sh --asset-names`; `prepare-release-assets.sh` checks presence, non-empty, declared name, digest, no unexpected file, and `SHA256SUMS` line count; the publish step uploads exactly what `SHA256SUMS` lists | `prepare-release-assets.sh`; `release.yml` steps "Verify the assets", "Create or refresh the draft Release" |
| two consumers index `--asset-names <target>` positionally: release notes (`${names%%$'\n'*}`) and the install.ps1 smoke step (`$names[0]`, `$names[1]`) | `release.yml:651`, `release.yml:382` |
| core already has a streaming sha256 + `.partial` + rename download (`cli/model.rs` `download`, `download_to_partial`, ureq + `sha2`) and depends on `flate2`; `tar 0.4.46` is already in `Cargo.lock` (transitively); `zip` is not | `grep` on `core/src/cli/model.rs`, `core/Cargo.toml`, `Cargo.lock` |
| core's language catalogue (`languages::CATALOGUE`: typescript, python, rust, go) is what `install_command()` names | g-mesh `get_file_outline core/src/languages.rs` |
| sizes, x86_64-apple-darwin v4.0.0: archive 25.08 MB packed / 70.40 MB unpacked; core 41.19 MB; plugins ts 8.41, go 8.87, rust 6.29, python 5.56 = 29.13 MB unpacked (41%) | `docs/results/gm-326-archive-size.md` |

g-mesh calls made: `select_project`, `get_file_outline` on `manifest.rs` and
`languages.rs`. The rest was `grep -n` + `sed -n` on scripts and YAML (non-code
for g-mesh) and on single known functions.

## D1. What a plugin asset contains

**Proposal:** the staged `plugins/<lang>/` directory, byte for byte, with
`<lang>/` as the archive's only top-level entry:

```
typescript/plugin.toml                 (command = "./g-mesh-plugin-typescript[.exe]")
typescript/g-mesh-plugin-typescript[.exe]
```

`build-targets.sh` archives it from the same stage the main archive is made from,
after bundling and before the main archive. Install = unpack into a plugin root
(`tar -xzf <asset> -C <root>`); removal = `rm -rf <root>/<lang>`. No registry,
no extra metadata file: `plugin.toml` already carries `language`,
`plugin_version` and `protocol_version`, which is what an installer must check.

- Benefit: the asset is not a second build - it is the same bytes the main
  archive carries, so every existing check on the stage (build-targets smoke,
  GM-333 reindex) covers it. Relocatable because of the `./<exe>` rewrite.
- Risk: a `<lang>/` top dir unpacked into the wrong directory spills one dir,
  not loose files - acceptable. No version in the top dir, so an installer must
  read `plugin.toml` to know what it got (it has to anyway, for the protocol).

**Where `plugins install` unpacks it** is GM-331's call but decides whether the
asset works: see Q1. Recommended: beside the executable (`<exe dir>/plugins/`),
not `~/.g-mesh/plugins/`, because the user root outranks the bundled one and
survives a core upgrade, so an old-protocol plugin left there fails discovery for
every language after the next `install.sh` run (manifest.rs:357 + `discover`'s `?`).

## D2. Naming and checksums

**Proposal:** `g-mesh-plugin-<language>-v<version>-<target>.tar.gz` plus sibling
`<asset>.sha256` in the existing `<hex>  <basename>` format, written by the same
`sha256_of` line `build-targets.sh` uses for the main archive, and listed in
`SHA256SUMS`. `.tar.gz` on Windows too (Q2). `<version>` is core's: a plugin
asset of v4.0.0 is for core v4.0.0, so the installer computes the URL from
`CARGO_PKG_VERSION` and its own target, nothing to resolve.

The bundled languages become one array in `build-targets.sh`
(`BUNDLED_PLUGINS=(typescript go rust python)`) used by both the bundling step
and `--asset-names`, so names stay generated, not typed. A test asserts it
equals `languages::CATALOGUE`'s languages, so every `install_command()` core
prints has an asset behind it.

**Checksum code: a third implementation, in Rust, and not a new one.** The fetch
lives in `g-mesh plugins install`, a Rust binary on every platform including
Windows, where neither `install.sh` (POSIX sh) nor `install.ps1` (PowerShell)
can be called from or shared with. The two scripts must stay standalone because
they run before any g-mesh binary exists. So the plugin fetch reuses core's
existing `cli/model.rs` download (`download_to_partial`: streams to `.partial`,
hashes while writing, renames only on success) - extracted to a shared helper by
GM-331 - with one difference: the expected digest comes from the sibling
`.sha256` fetched from the same release, as `install.sh` does, not from a
constant. The discipline is the one stated in `install.sh`'s CHECKSUMS header:
per-asset `.sha256`, compare before unpacking, refuse and delete on mismatch,
print both digests.

- Benefit: one Rust download path for model and plugin, already tested; no
  shell dependency on Windows.
- Risk: three implementations of the same rule (sh, ps1, Rust) can drift. Mitigation:
  the decision and its reason go to an ADR (`docs/adr/0027-...`, per CLAUDE.md:
  decisions live in ADRs, not comments), linked once from the place each copy
  is embodied - `install.sh`'s and `install.ps1`'s CHECKSUMS sections (GM-353's
  code slice) and the Rust helper's module doc (GM-331, when it adds the fetch).
  The comment itself keeps only the invariant: "the same rule is implemented in
  <the other two>; change all three".
- Rejected: `install.sh --plugin <lang>` / `install.ps1 -Plugin`. It shares code
  with install.sh literally, but adds two more front doors to the one GM-331
  specifies, and Windows still needs the ps1 copy.

## D3. Asset count and what Verify assets checks

T = 4 targets, L = 4 languages: per target 2 (main + sha) + 2L (plugin + sha) = 10;
total **4 x 10 + 1 = 41 assets** (was 9). `SHA256SUMS` has T x (1 + L) = **20 lines**.

`--asset-names <target>` keeps the main archive and its `.sha256` as its first two
lines (the two positional consumers stay correct), then plugin pairs in
`BUNDLED_PLUGINS` order; the header comment says so.

`Verify assets (dry run)` (= `prepare-release-assets.sh`) then checks, with no code
change for the first four because they iterate `--asset-names`:

1. every expected name present and non-empty (now 40 files + SHA256SUMS);
2. no unexpected `*.tar.gz`/`*.zip`/`*.sha256` in `dist/`;
3. each `.sha256` names its own file and matches the bytes;
4. `SHA256SUMS` line count = archives (20);
5. **new:** the count itself, computed as `T*(1+L)` from `--list` and
   `BUNDLED_PLUGINS`, logged and asserted - so dropping a language from the array
   and from the build at the same time is still a visible number change;
6. **new:** each plugin asset lists exactly `<lang>/plugin.toml` + `<lang>/<exe>`
   (`tar -tzf`), its `plugin.toml` has `language = "<lang>"`, and both files are
   byte-identical to `plugins/<lang>/` inside the same target's main archive
   (extract-to-stdout + sha256). Linux runner, nothing executed.

Also: `G_MESH_SKIP_PLUGIN_BUNDLE=1` writes no plugin assets, so
`prepare-release-assets.sh` fails on such a dist - correct, that archive must not
be published. Workflow header "nine assets" and the contract block are updated;
release notes gain a plugin-assets line per target.

- Benefit: almost all of it falls out of `--asset-names`; check 6 is what makes
  "the asset is the bundled plugin" a checked statement, not an intention.
- Risk: 41 assets make the draft-release review list long; check 6 needs the
  main archive's internal path, i.e. `artifact_stem_for` - exposed through
  `--stage-dir`'s formula, not retyped.

## D4. What the main archive carries, and install.sh's refusal

**Proposal: keep all four plugins in the main archive; the design moves, the
refusal stays (reworded).**

| option | packed per download (darwin x86_64) | first-run experience |
|---|---|---|
| all four (today) | 25.1 MB measured | indexes all four languages |
| none | ~15 MB (estimate: 25.1 x 41.19/70.40; not measured) | indexes nothing until a second command per language |
| default set (e.g. ts only) | ~18 MB (estimate) | arbitrary: we do not know the audience (the architecture doc's own argument against a TS special case) |

GM-326 already did the size work (-59% packed). Dropping every plugin saves an
estimated ~10 MB per download at the cost of a default install that indexes
nothing and an `install.sh` that would have to fetch plugins too (more code in
two scripts). Not worth it. The per-plugin assets then serve: reinstalling after
`plugins remove`, repairing a broken plugin dir, installs that did not come from
an archive (source build, `cargo install`), and language N+1 if it is ever not
bundled.

Consequently `install.sh:682` / `install.ps1:506` keep refusing a main archive
without `plugins/typescript/plugin.toml`, but as a **layout sentinel**, and the
message stops claiming a law it no longer states ("Core cannot index a
TypeScript project without it"): it says a g-mesh release archive always carries
its bundled plugins, so this is not a release archive (wrong or truncated
asset). The post-unpack `plugins list | grep typescript` stays for the same
reason. The premise "an archive can arrive without a given plugin" applies to
*plugin roots*, not to the main archive; the refusal is not where it bites.

- Benefit: zero behaviour change for anyone installing today; both installers
  keep a cheap check against a wrong asset; the archive shape the GM-333 smoke
  and install.ps1 smoke rely on is unchanged.
- Risk: the architecture doc's "projected base archive" is fixed at four
  plugins - adding a fifth grows every download. Revisit then (Q3), not now.

## D5. Keeping GM-333's smoke check meaningful

`release-smoke.sh` stays as is on the main stage (all four languages, per-language
node counts). **Added:** a second leg on each runner (all four are native, so each
can execute its own binaries): copy the stage, `rm -rf plugins/*`, unpack the four
`g-mesh-plugin-*` assets from `dist/` into `plugins/`, reindex the same fixture,
and require the same per-language counts > 0. That proves the assets reproduce the
bundled layout and each works, on every platform, from the published bytes.
Control: skip unpacking one asset - its language must report 0 and fail.

Caveat to state, not fix here: the per-language check is skipped when `sqlite3` is
not on PATH (likely on the Windows runner); the aggregate check then cannot tell
the legs apart. The verify slice reports whether it ran on each platform.

- Benefit: the acceptance criterion "unpacking a plugin asset into an existing
  install makes that language work" is checked on four platforms in CI, not one.
- Risk: one more reindex per runner (small fixture, seconds); a second copy of
  the stage (~70 MB) on the runner.

## Open questions for the owner

**Q1. Where does an installed plugin go?** (decides whether D1's asset is safe;
GM-331's command implements it)
- a. `<exe dir>/plugins/` - version-locked to the core that fetched it; wiped and
  replaced by the next `install.sh`. **Recommended.** Risk: the exe dir may be
  unwritable (system install) - the command must say so, not fall back.
- b. `~/.g-mesh/plugins/` - survives upgrades. Risk: outranks the bundled copy and an
  old protocol there fails discovery for every language after an upgrade.
- c. b plus a protocol/version check in `discover` that skips (not fails) a stale
  user plugin. Risk: changes discovery semantics; separate task.

**Q2. Windows plugin asset format?**
- a. `.tar.gz` on every target. **Recommended:** one unpack path in Rust (`tar` already
  in the lock, `flate2` direct), a constant name formula; Windows 10+ ships `tar.exe`.
  Risk: departs from the main archive's zip-on-Windows convention.
- b. `.zip` on Windows, as the main archive. Benefit: convention. Risk: adds the `zip`
  crate to core for GM-331, two unpack paths to test.

**Q3. Main archive contents?**
- a. All four plugins (D4). **Recommended.**
- b. None. Benefit: ~10 MB (estimated) less per download. Risk: default install indexes
  nothing; both installers must fetch plugins.
- c. A default set. Risk: requires knowing the audience, which the architecture doc says
  we do not.

**Q4. GM-353 without GM-331's command?** GM-353 delivers the assets, the checks and a
manual install path; `g-mesh plugins install` itself is GM-331.
- a. GM-353's end-to-end proof is the manual path (curl + `shasum -c` + `tar -xzf` into
  an existing install on darwin, plus the CI smoke leg); GM-331 adds the command.
  **Recommended.** Risk: the Rust checksum record lands only with GM-331.
- b. Fold the Rust fetch into GM-353. Risk: doubles the task and overlaps GM-331.

## Proposed slicing (after approval)

| slice | kind | model | files |
|---|---|---|---|
| S2 | code | opus | `scripts/build-targets.sh` (`BUNDLED_PLUGINS`, plugin archive + sha per language, `--asset-names` order), `scripts/prepare-release-assets.sh` (count, layout + identity check), `scripts/release-smoke.sh` (assets leg), `.github/workflows/release.yml` (header/contract text, release notes, smoke invocation), `scripts/install.sh` + `scripts/install.ps1` (reworded refusal, CHECKSUMS cross-reference) |
| S3 | tests | opus | `core/tests/release_packaging_scripts.rs` (asset names and order; plugin archive layout; prepare-release rejects a missing/extra/tampered/non-identical plugin asset; release-smoke assets leg fails with one asset missing; `BUNDLED_PLUGINS` == `CATALOGUE`), `scripts/test-install.sh` if the refusal message is asserted there; each with a described control |
| S4 | verify | opus | none edited: builds controls in a throwaway worktree, runs `build-targets.sh` for the host, `prepare-release-assets.sh` on the result, the manual end-to-end install on darwin (remove `plugins/python`, install its asset, `reindex` shows `.py` nodes), nextest `-p g-mesh` (tests crate) |
| S5 | docs | sonnet | new ADR `docs/adr/0027-plugin-fetch-checksums-in-rust.md` + `docs/adr/README.md` index (S2 links to it, so S5 runs before or with S2), `docs/architecture/plugin-distribution.md` (CLI section: asset name, install location per Q1), `README.md` if it lists release assets |
