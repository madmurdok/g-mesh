// The build-shape switch (src/runtime.ts) seen from a `tsc` build - which is
// the shape these tests, core's `cargo test`, and every dev checkout run in.
//
// What is worth pinning here is the *dev* half, because it is the half that
// has to keep behaving exactly as it did before the release bundle existed: a
// regression there breaks the plugin for everyone working on the repo, and
// would do it silently, since the bundled half is only exercised when an
// archive is built. The bundled half cannot be tested from here at all -
// `IS_SELF_CONTAINED` is a constant esbuild substitutes at bundle time (see
// scripts/bundle-plugin.sh) - so it is covered where it actually exists: that
// script's own smoke test runs the built executable with `node` removed from
// `PATH` and requires a handshake back.

import test from "node:test";
import assert from "node:assert/strict";

import { IS_SELF_CONTAINED, RUN_NODE_FLAG, nodeRuntimeArgv } from "../src/runtime";

test("a tsc build is not the self-contained one", () => {
  // If this ever flips, every assertion below is testing the wrong branch.
  assert.equal(IS_SELF_CONTAINED, false);
});

test("a dev build spawns a script the way `node <script>` always did", () => {
  assert.deepEqual(nodeRuntimeArgv("/path/to/tsserver.js"), ["/path/to/tsserver.js"]);
  assert.deepEqual(nodeRuntimeArgv("/path/to/tsserver.js", ["--suppressDiagnosticEvents"]), [
    "/path/to/tsserver.js",
    "--suppressDiagnosticEvents",
  ]);
});

test("the dev build passes no interpreter flag of its own", () => {
  // The bundled executable needs `--run-node` to know it is standing in for
  // node; a real `node` would reject it as an unknown option, so it must never
  // appear on this path.
  assert.ok(!nodeRuntimeArgv("/x.js", ["--flag"]).includes(RUN_NODE_FLAG));
});
