# GM-643: publish the Release, not leave it as a draft

## Today

- `release.yml` `publish` job (`needs: build`, tag-only) runs
  `prepare-release-assets.sh` (asset names/checksums), composes notes, then
  "Create or refresh the draft Release": `gh release create "$tag" ... --draft`
  (re-run: `gh release upload --clobber`, draft state untouched).
- Nothing in the repo ever undrafts. The header (steps 3-4, "The draft step is
  the deliberate cost of this design") and README "Cutting a release" step 4
  ("approve the draft on GitHub") make it a manual click; `cut-release.sh` ends
  by printing "Review and publish the draft release".
- `install.sh` / `install.ps1` resolve the version from `releases/latest`, which
  ignores drafts. `install-latest` (GM-205) runs on `workflow_dispatch` only.

## Cause

Not a regression. `git log -S--draft` shows `--draft` and the manual
"approve the draft" step both arrived in the first commits (4295576, 5e0493e,
2026-08-18, #67's design: "on a repository where this pipeline has yet to run
once"). Every release since was published by a human click; v4.12.0-v4.14.0
were simply never clicked, so `releases/latest` stayed v4.11.0.

State on 2026-10-21 (read-only `gh`): v4.12.0-v4.15.0 were all published at
12:18Z today (by hand), 41 assets each, 0 drafts left, `releases/latest` =
v4.15.0. **The existing-drafts question is already resolved; no owner action.**

## Options

**(a) cut-release.sh waits for release.yml, then `gh release edit --draft=false`.**
Benefit: the publish stays on the owner's machine under the owner's token.
Risk: the script must stay alive for the ~1h build (`gh run watch`); a closed
lid or Ctrl-C leaves the draft as today. It also changes cut-release.sh's
`main()`, which GM-586 edits next. Without `--push` there is nothing to wait for.

**(b) release.yml publishes itself, after assets are verified.**
Benefit: no human step to forget. The guards already exist: tag push is an
explicit human act (`cut-release.sh --push`), every one of the four legs ran
release-smoke.sh + install.ps1 smoke (GM-333) before `publish` can start, and
`prepare-release-assets.sh` refuses a wrong/missing asset. Risk: no human looks
at the notes before they go public (notes can be edited after publishing); a bad
build that passes every smoke leg becomes `latest` at once (the fix is a new
patch release, or `gh release edit --draft=true` by hand).

**(c) Keep the draft, document the click better.**
Benefit: no code change. Risk: this is today's procedure, already documented in
three places, and it failed for three releases running.

## Recommendation

(b), as a two-phase step: keep creating the Release as a draft (so a
half-uploaded Release is never public), check the uploaded asset count, then
undraft it and mark it latest. The #67 premise ("pipeline has yet to run once")
no longer holds: it has cut 4.x releases repeatedly. Optionally (must confirm)
let `install-latest` also run on tag pushes after `publish`, asserting that
`releases/latest` is now this tag: that is AC3 proved on every release.

cut-release.sh: only its final hint text and header comment change (no new
logic), so GM-586's later edit of the same file does not conflict in substance.

## Edit map

| file | where | change |
| --- | --- | --- |
| `.github/workflows/release.yml` | header, lines ~3, ~68-78 (steps 4 and "The draft step is the deliberate cost" paragraph) | describe: draft created, assets verified, then published automatically; drop the "press Publish yourself" text |
| same | `publish` job `name:` (~864) | "Publish Release" |
| same | step "Create or refresh the draft Release" (~1001-1044) | create path unchanged (`--draft`). After create/upload: `n="$(gh release view "$tag" --json assets --jq '.assets\|length')"`; fail unless `n == ${#assets[@]}`. Then, only if `isDraft` is `true`: `gh release edit "$tag" --draft=false --latest`. An already-published release (re-run) is left as is. Summary: replace the "A draft is not public..." lines with draft/published state |
| same | `install-latest` job (~755-768) - only if owner confirms | `needs: publish`; `if: ${{ !cancelled() && (github.event_name == 'workflow_dispatch' \|\| needs.publish.result == 'success') }}`; on a tag run, also throw unless `$latest -eq $env:GITHUB_REF_NAME.TrimStart('v')` (both pwsh and powershell steps) |
| `scripts/cut-release.sh` | header lines 4-5, 84-88; `main()` final hints (~717, ~721) | "approve the draft" -> "release.yml publishes it when all four builds pass"; hint becomes "Check the published release: .../releases/tag/$tag" |
| `README.md` | lines ~162-166 ("Until a release is published...") and "Cutting a release" step 4 (~407) | step 4: the tag run publishes the Release after all four legs pass; if `publish` fails, fix and re-run the job (it publishes a still-draft Release) |
| `scripts/install.sh` | `no_published_release` text (~380-383) and comment ~113-116 | third cause becomes "a release build is still running or failed" (no human Publish step) |
| `scripts/install.ps1` | comment ~133-135, `Get-NoPublishedReleaseMessage` (~272-275) | same wording as install.sh |

New comments follow CLAUDE.md's comment rule: state the invariant ("a Release
is public only after every asset is uploaded"), no GM-643 history.
No Rust code changes. Tests live in `core/tests/scripts/release_packaging_scripts.rs`
(it already pins `release.yml` text, e.g. the Node-free test at ~1159).

## Behaviours the tests slice must pin

1. `publish` still creates a new Release with `--draft` (never public half-uploaded).
2. `publish` runs `gh release edit "$tag" --draft=false` (with `--latest`) after the create/upload.
3. The undraft comes after the asset-count check, and the check compares the uploaded count to `${#assets[@]}`.
4. The undraft is guarded by `isDraft == true`: a re-run never re-flags or re-latests an already-published release.
5. `publish` keeps `if: ... needs.build.result == 'success' && startsWith(github.ref, 'refs/tags/v')`.
6. cut-release.sh's final output no longer tells the user to publish a draft (run with a stub? text check of the script is enough).
7. (if confirmed) `install-latest` runs after `publish` on tag runs and asserts latest == the tag.

Text assertions on the YAML/script are the realistic level; a behavioural test
of step 3/4 would need a `gh` stub on `PATH` running the step's bash, which is
worth it only if the step is extracted into a script (not proposed: footprint).

## Owner actions on existing drafts

None needed: 0 drafts, latest = v4.15.0. To re-check (read-only):

```
gh release list --limit 20 --json tagName,isDraft --jq '.[]|select(.isDraft)'
gh api repos/madmurdok/g-mesh/releases/latest --jq .tag_name
```

Had drafts remained, publishing oldest to newest makes the newest `latest`:
`for t in v4.12.0 v4.13.0 v4.14.0 v4.15.0; do gh release edit "$t" --draft=false; done`
then `gh release edit v4.15.0 --latest`.

## Must confirm

1. Drop the human review gate (option b). Alternative: keep (c) and accept the risk.
2. `--latest` on every publish: correct while releases are linear; a future
   hotfix of an older line (e.g. 4.14.1 after 4.15.0) would wrongly become
   latest. OK to assume linear releases?
3. Extend `install-latest` to tag runs after `publish` (adds ~10 min Windows
   runner per release; a failure shows red after the release is already public).
4. Verifying the new step needs a real tag run (4.16.0 itself): the first proof
   is the 4.16.0 release; a PR/dispatch run skips `publish` entirely.
