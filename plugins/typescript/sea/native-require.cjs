// How the single-executable build reaches its native dependencies.
//
// tree-sitter and its two grammars are C addons: what `require("tree-sitter")`
// ultimately loads is a `.node` shared library that `node-gyp-build` picks out
// of the package's own `prebuilds/<platform>-<arch>/` at runtime. A shared
// library cannot be bundled into a JS blob and cannot be dlopen'd out of one,
// so - unlike every other dependency, which esbuild inlines - these three
// packages stay real directories on disk, shipped next to the executable in
// the release archive (see scripts/bundle-plugin.sh).
//
// Reaching them needs a require this bundle does not otherwise have. Inside a
// single-executable application the ambient `require` resolves **built-in
// modules only**; anything on disk needs a require anchored at a real path.
// The anchor here is a file name that need not exist - `createRequire` only
// uses it to decide where module resolution starts - which is deliberately the
// executable's own directory, so `node_modules/` is looked up beside the
// executable exactly where the archive puts it.
//
// The three `<package>.cjs` files beside this one are what esbuild's `--alias`
// points the real package names at; they exist because an alias has to name a
// module, and all three want this same one line of behavior.

const { createRequire } = require("node:module");
const path = require("node:path");

const requireBesideExecutable = createRequire(
  path.join(path.dirname(process.execPath), "g-mesh-plugin-sea-anchor.cjs"),
);

/**
 * Loads `name` from the `node_modules/` shipped beside the executable.
 * Errors are left to propagate: a missing native grammar means this plugin
 * cannot parse anything at all, and failing loudly at startup is far easier to
 * diagnose than an index that silently comes back empty.
 */
module.exports = function loadNativeModule(name) {
  return requireBesideExecutable(name);
};
