// What differs between the two shapes this plugin ships in, in one place.
//
// A dev checkout runs the plugin as `node dist/src/index.js`: `process.execPath`
// is a real `node`, and handing it a script path is all it takes to run another
// one. A release archive (task 65) instead ships a **single-executable
// application** - the plugin's whole compiled bundle injected into a copy of the
// Node binary via Node's SEA support, so it needs no Node installed on the
// machine at all (see `scripts/bundle-plugin.sh` for how it is built and why SEA
// rather than `bun build --compile`).
//
// That one change breaks an assumption `semantic.ts` was written on. A SEA
// executable *is* a JS runtime, but it does not behave like the `node` CLI: it
// always runs its own embedded main script, and argv is passed through to that
// script rather than interpreted as "a script to run". So
// `spawn(process.execPath, [tsserverPath])` - the whole mechanism behind the
// semantic layer's tsserver child - would spawn a second copy of *this plugin*
// instead of the compiler. [`RUN_NODE_FLAG`] is the way back out: the bundled
// executable recognizes it as "stop being the plugin, be `node` for one script",
// which is what makes the semantic layer keep working with no Node installed
// (verified end to end against a project's own `lib/tsserver.js` with `node`
// removed from `PATH`).
//
// Nothing here is conditional at runtime in a dev build: [`IS_SELF_CONTAINED`]
// is a compile-time constant that only the bundler defines, so the dev path
// keeps the exact behavior it had before this file existed, and the branches
// that only make sense in a bundle are dead code the bundler drops.

import { createRequire } from "node:module";

/**
 * Defined (as `true`) only by `scripts/bundle-plugin.sh`, through esbuild's
 * `--define`. A `tsc` build never emits a declaration for it, which is why
 * every read below goes through a `typeof` guard rather than naming it
 * directly - an undeclared identifier read bare is a ReferenceError, and this
 * module is imported on the plugin's startup path.
 */
declare const __G_MESH_SELF_CONTAINED__: boolean | undefined;

/**
 * True in the single-executable release build, false when running from
 * `dist/` under a system `node`.
 */
export const IS_SELF_CONTAINED: boolean =
  typeof __G_MESH_SELF_CONTAINED__ !== "undefined" && __G_MESH_SELF_CONTAINED__ === true;

/**
 * First argv entry that makes the bundled executable run `argv[1]` as an
 * ordinary Node script instead of starting the plugin - see this module's doc
 * comment. Handled in `index.ts`'s `main`, ahead of everything else it does.
 *
 * Kept distinct from `index.ts`'s `--bulk-index` for the same reason that flag
 * exists at all: these are two different *modes of the same executable*, and a
 * flag is the only thing core (or this plugin's own semantic layer) can pass a
 * process it spawns before it has said anything.
 */
export const RUN_NODE_FLAG = "--run-node";

/**
 * The argv that runs `script` on *this process's own* JS runtime, whichever
 * shape the plugin is in. Callers pair it with `process.execPath`:
 *
 * ```ts
 * spawn(process.execPath, nodeRuntimeArgv(tsserverPath, flags), { ... })
 * ```
 *
 * In a dev checkout that is exactly the `node <script> <args>` this replaced.
 * In the bundle it prefixes [`RUN_NODE_FLAG`], which the same executable
 * unwraps in [`runNodeScript`].
 */
export function nodeRuntimeArgv(script: string, args: readonly string[] = []): string[] {
  return IS_SELF_CONTAINED ? [RUN_NODE_FLAG, script, ...args] : [script, ...args];
}

/**
 * Runs `script` as though it had been handed to `node` on the command line,
 * and never returns to the plugin's own startup path.
 *
 * `process.argv` is rewritten first because a CLI-shaped script reads its
 * options from it, and tsserver is exactly that (`ts.sys.args` is
 * `process.argv.slice(2)`). Left alone, the bundle's own argv - which carries
 * [`RUN_NODE_FLAG`] and the script path in the slots tsserver expects its
 * flags in - would feed the compiler garbage.
 *
 * `createRequire` rather than the ambient `require`: inside a
 * single-executable application the built-in `require` resolves *built-in
 * modules only*, so loading anything off disk needs a require anchored at a
 * real path. Anchoring it at the script itself also gives the script the
 * module resolution it would have had under `node` - its own `node_modules`
 * lookup starts from its own directory, which is what lets a project's pinned
 * TypeScript find the rest of its package.
 */
export function runNodeScript(script: string, args: readonly string[] = []): void {
  process.argv = [process.execPath, script, ...args];
  createRequire(script)(script);
}
