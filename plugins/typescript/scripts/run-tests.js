#!/usr/bin/env node
"use strict";

// Runs the compiled test suite as an explicit file list, not `node --test
// dist/test`.
//
// `node --test <path>` behaves differently depending on which Node runs it:
// on Node 20, a directory argument is walked and every test file under it is
// run; on Node 22, the same argument is instead resolved as a module
// specifier, and since `dist/test` has no index file that resolution fails
// with MODULE_NOT_FOUND before a single test runs. That is exactly the gap
// between the Node this plugin is developed against (20.x) and the Node CI
// pins (22.x) -- the directory form was never portable across the two, it
// just never got exercised on both until CI actually ran it.
//
// A shell glob (`node --test dist/test/*.test.js`) would dodge the directory
// form, but npm on Windows runs scripts through cmd.exe, which does not
// expand globs -- the matrix in ci.yml includes windows-2022, so that fix
// would pass on macOS/Linux runners and fail silently (0 tests collected,
// not an error) on Windows. Enumerating the files in Node itself, with
// `fs.readdirSync`, works identically on every OS and every supported Node
// version, because it never depends on the shell that launched it.
//
// Keep this file's `--test` invocation as `node --test <explicit files>`; do
// not go back to a directory or a glob argument.

const fs = require("node:fs");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const testDir = path.resolve(__dirname, "..", "dist", "test");

let entries;
try {
  entries = fs.readdirSync(testDir);
} catch (err) {
  console.error(`run-tests: cannot read ${testDir}: ${err.message}`);
  process.exit(1);
}

const files = entries
  .filter((name) => name.endsWith(".test.js"))
  .sort()
  .map((name) => path.join(testDir, name));

if (files.length === 0) {
  console.error(`run-tests: no *.test.js files found under ${testDir}`);
  process.exit(1);
}

const result = spawnSync(process.execPath, ["--test", ...files], {
  stdio: "inherit",
});

if (result.error) {
  throw result.error;
}

process.exit(result.status === null ? 1 : result.status);
