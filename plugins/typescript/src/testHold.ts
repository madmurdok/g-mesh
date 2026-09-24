/**
 * A test-only knob that parks the plugin at a named point, neither reading
 * stdin nor writing stdout, for as long as a test wants it parked (GM-397).
 * The contract is identical in all four bundled plugins - see
 * plugins/sdk/src/hold.rs for why it exists:
 *
 * With `G_MESH_PLUGIN_HOLD_DIR` set to a directory, a plugin reaching hold
 * point `P` for language `L` looks for `<dir>/P-L.hold`. If it exists, the
 * plugin writes its own pid to `<dir>/P-L.pid` (through a temporary file and
 * a rename) and then waits while the hold file exists, checking every 10 ms,
 * for at most 60 s. Unset, or with no hold file, it costs one stat at most.
 *
 * Unlike the Go and SDK holds, which block their thread, this one is an async
 * `setTimeout` loop: this plugin's real waits (fs, tsserver) are async too, so
 * an event loop left free to run is the honest model of "busy" here.
 */
import * as fs from "node:fs";
import * as path from "node:path";

export const HOLD_DIR_ENV = "G_MESH_PLUGIN_HOLD_DIR";
const HOLD_LANGUAGE = "typescript";
const POLL_MS = 10;
const MAX_HOLD_MS = 60_000;

function log(message: string): void {
  process.stderr.write(`[g-mesh-js-ts] ${HOLD_DIR_ENV}: ${message}\n`);
}

/** Parks at `point` (`bulk` or `semantic`) if the knob asks for it. */
export async function holdPoint(point: "bulk" | "semantic"): Promise<void> {
  const dir = process.env[HOLD_DIR_ENV];
  if (!dir) return;
  const stem = `${point}-${HOLD_LANGUAGE}`;
  const hold = path.join(dir, `${stem}.hold`);
  if (!fs.existsSync(hold)) return;

  try {
    const tmp = path.join(dir, `${stem}.pid.tmp`);
    fs.writeFileSync(tmp, String(process.pid));
    fs.renameSync(tmp, path.join(dir, `${stem}.pid`));
  } catch (err) {
    log(`failed to record the pid at hold point ${point}: ${(err as Error).message}`);
  }
  log(`holding at ${point} while ${hold} exists`);

  const deadline = Date.now() + MAX_HOLD_MS;
  while (fs.existsSync(hold) && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, POLL_MS));
  }
}
