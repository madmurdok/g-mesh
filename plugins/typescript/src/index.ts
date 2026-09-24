import { FrameReader, writeMessage } from "./jsonrpc";
import { parseControlEnvelope, ControlEnvelope, PROTOCOL_VERSION, JSONRPC_VERSION } from "./protocol";
import { reparseChangedFile, type FileDiff } from "./incremental";
import { bulkIndexProject, toWireNode, type WireEdge, type WireNode } from "./bulkIndex";
import { stopSemanticProjects } from "./semantic";
import { runSemanticPass } from "./semanticPass";
import { RUN_NODE_FLAG, runNodeScript } from "./runtime";
import { PLUGIN_VERSION } from "./version.generated";
import { holdPoint } from "./testHold";

/** Selects one-shot bulk-index mode instead of the control-plane loop; must
 * stay in sync with core's `daemon::bulk_index::BULK_INDEX_FLAG`. */
const BULK_INDEX_FLAG = "--bulk-index";

/** Set to "1" by core on every bulk spawn: stdin is then a pipe core holds
 * open and never writes, and its end means core is gone (GM-397). Must stay
 * in sync with core's `daemon::bulk_index::BULK_STDIN_LIFELINE_ENV`. Without
 * it the bulk walk leaves stdin alone, so an older core or a hand run with
 * `< /dev/null` is not read as "exit before walking". */
const BULK_STDIN_LIFELINE_ENV = "G_MESH_BULK_STDIN_LIFELINE";

function log(message: string): void {
  process.stderr.write(`[g-mesh-js-ts] ${message}\n`);
}

function sendHandshake(): void {
  writeMessage(process.stdout, {
    protocolVersion: PROTOCOL_VERSION,
    language: "typescript",
    pluginVersion: PLUGIN_VERSION,
  });
}

/** Mirrors core's `FileChangeDiff` (core/src/protocol/types.rs): add/remove
 * vocabulary becomes upsert/delete, and removed items are reduced to just
 * their ids - core only needs to know what to delete, not what it looked
 * like. */
interface WireFileChangeDiff {
  upsertNodes: WireNode[];
  deleteNodeIds: string[];
  upsertEdges: WireEdge[];
  deleteEdgeIds: string[];
}

function toWireFileChangeDiff(diff: FileDiff): WireFileChangeDiff {
  return {
    upsertNodes: diff.addedNodes.map(toWireNode),
    deleteNodeIds: diff.removedNodes.map((node) => node.id),
    upsertEdges: diff.addedEdges,
    deleteEdgeIds: diff.removedEdges.map((edge) => edge.id),
  };
}

const EMPTY_WIRE_DIFF: WireFileChangeDiff = {
  upsertNodes: [],
  deleteNodeIds: [],
  upsertEdges: [],
  deleteEdgeIds: [],
};

/**
 * Reparses `filePath` against this plugin's cached state and, if the
 * envelope was a request (had an `id`), answers it with the real diff.
 * A bare notification (no `id`) still updates the cached state - so a later
 * request-style FileChanged for the same file diffs against current
 * content - but core isn't waiting on a response, so failures are logged
 * only, never surfaced.
 *
 * Core always waits for a response to a request it sent (see
 * `watcher::apply::apply_file_change` on the Rust side), so a reparse
 * failure on the request path still must answer with *something* - an
 * empty diff is a safe no-op or, most often, an early sign the file was
 * deleted out from under the plugin, which the next change (if any)
 * self-corrects.
 */
async function handleFileChanged(projectRoot: string, filePath: string, id: ControlEnvelope["id"]): Promise<void> {
  log(`file changed: ${filePath}`);
  try {
    const diff = await reparseChangedFile(projectRoot, filePath);
    if (id !== undefined) {
      writeMessage(process.stdout, { jsonrpc: JSONRPC_VERSION, id, result: toWireFileChangeDiff(diff) });
    }
  } catch (err) {
    log(`failed to reparse changed file ${filePath}: ${(err as Error).message}`);
    if (id !== undefined) {
      writeMessage(process.stdout, { jsonrpc: JSONRPC_VERSION, id, result: EMPTY_WIRE_DIFF });
    }
  }
}

/**
 * Answers a semantic pass: asks semanticPass.ts what TypeScript's own checker
 * can resolve that the structural pass could not, and sends it back as the
 * same diff shape `fileChanged` answers with.
 *
 * An empty `filePaths` means the whole project - the wire's own convention,
 * sent once the cold-start walk lands - and a one-entry list a reparse that
 * just settled. The pass itself distinguishes them; everything else here is
 * the same contract `handleFileChanged` has.
 *
 * A pass that fails answers with an empty diff rather than with an error. Core
 * drops a failing semantic pass on the floor by design (it is an upgrade over a
 * graph that is already committed and serviceable), and an empty diff is the
 * same outcome reached without making core parse a failure first: the index
 * keeps whatever the structural layer already resolved, which is the state it
 * was in anyway. A *partial* answer, which is what a per-question failure
 * inside the pass degrades to, still gets through.
 */
async function handleSemanticPass(
  projectRoot: string,
  filePaths: string[],
  id: ControlEnvelope["id"],
): Promise<void> {
  log(
    filePaths.length === 0
      ? "semantic pass requested for the whole project"
      : `semantic pass requested for: ${filePaths.join(", ")}`,
  );

  let result = EMPTY_WIRE_DIFF;
  try {
    const pass = await runSemanticPass(projectRoot, filePaths, { onLog: log });
    result = {
      upsertNodes: pass.upsertNodes.map(toWireNode),
      // Placeholders are never deleted, for the reason core's
      // `graph::symbol_links` spells out: a later edit can hang a new edge on
      // one this pass did not re-send, and a deleted node would leave that
      // edge pointing at nothing.
      deleteNodeIds: [],
      upsertEdges: pass.upsertEdges,
      deleteEdgeIds: pass.deleteEdgeIds,
    };
    log(
      `semantic pass over ${pass.filesScanned} file(s): ${pass.upsertEdges.length} edge(s) answered, ` +
        `${pass.deleteEdgeIds.length} retracted, ${pass.unresolvedUses} left unresolved`,
    );
  } catch (err) {
    log(`semantic pass failed: ${(err as Error).message}`);
  }

  // Same contract as handleFileChanged: core blocks on a response to any
  // request it sent, so a request must always be answered with a diff -
  // never with the `{ acknowledged: true }` shape the no-op methods use,
  // which core would fail to deserialize as one.
  if (id !== undefined) {
    writeMessage(process.stdout, { jsonrpc: JSONRPC_VERSION, id, result });
  }
}

async function handleEnvelope(envelope: ControlEnvelope, projectRoot: string): Promise<void> {
  switch (envelope.method) {
    case "reindex":
      // Still a no-op: a whole-project rebuild is not something this
      // connection can carry - its output is an unbounded stream, not one
      // response frame - so core runs it as a separate `--bulk-index`
      // process instead (see `runBulkIndex` below). What is left for a
      // later ticket is the per-file meaning this message's `filePath`
      // implies, which `fileChanged` already covers in practice.
      log(`reindex requested: ${envelope.params?.filePath}`);
      break;
    case "fileChanged":
      await handleFileChanged(projectRoot, envelope.params?.filePath ?? "", envelope.id);
      return; // handleFileChanged already sent the (only) response, if any
    case "semanticPass":
      // Test-only (GM-397): parks the pass before any work starts. See
      // testHold.ts.
      await holdPoint("semantic");
      // parseControlEnvelope has already established filePaths is a real
      // string[] for this method; the `?? []` is for the type, not a case
      // that can happen.
      await handleSemanticPass(projectRoot, envelope.params?.filePaths ?? [], envelope.id);
      return; // answered with a diff, not the acknowledgement below
    case "status":
      log("status requested");
      break;
  }

  // Presence of `id` means this was a JSON-RPC request expecting a
  // response; its absence means a fire-and-forget notification.
  if (envelope.id !== undefined) {
    writeMessage(process.stdout, {
      jsonrpc: JSONRPC_VERSION,
      id: envelope.id,
      result: { acknowledged: true },
    });
  }
}

function handleFrame(frame: Buffer, projectRoot: string): void {
  let json: unknown;
  try {
    json = JSON.parse(frame.toString("utf8"));
  } catch (err) {
    log(`malformed control message JSON: ${(err as Error).message}`);
    return;
  }

  const parsed = parseControlEnvelope(json);
  if (!parsed.ok) {
    log(`malformed control envelope: ${parsed.error}`);
    return;
  }

  // handleEnvelope is async (reparsing reads the file off disk); errors
  // inside it are already caught where they can occur (handleFileChanged),
  // but catching here too means a mistake in a future branch fails loudly
  // in the log instead of as an unhandled rejection that kills the process.
  handleEnvelope(parsed.value, projectRoot).catch((err) => {
    log(`unexpected error handling control message: ${(err as Error).message}`);
  });
}

/**
 * One-shot cold-start index: walks the whole project and streams it to
 * stdout as NDJSON (bulkIndex.ts), then lets the process end.
 *
 * Its own process rather than a control-plane method, because the two have
 * incompatible shapes: a whole-project walk is an open-ended stream, while
 * the control plane is framed request/response. Running it standalone makes
 * this stdout a self-contained NDJSON stream that simply ends at EOF - which
 * is exactly what core's `NdjsonReader` consumes - with no handshake ahead
 * of it and no framing or termination marker to agree on.
 */
async function runBulkIndex(projectRoot: string): Promise<void> {
  const summary = await bulkIndexProject(projectRoot, process.stdout);
  log(
    `bulk index complete: ${summary.filesProcessed} files, ` +
      `${summary.nodesEmitted} nodes, ${summary.edgesEmitted} edges`,
  );
}

/**
 * The bulk walk's lifeline watcher (GM-397): reads and discards stdin, and
 * ends the process the moment it ends. The walk never touches stdin and can
 * spend a long time between writes, so without this a killed core goes
 * unnoticed until the walk is over. A read error counts as the end: with the
 * variable set core promised a pipe, and one that cannot be read is not one
 * core still holds. Exits 1, not 0: whatever is left to read this stream did
 * not get a complete one.
 */
function onBulkLifelineEnd(): void {
  log("core closed the bulk stream's lifeline - exiting");
  process.exit(1);
}

function watchBulkLifeline(): void {
  process.stdin.on("end", onBulkLifelineEnd);
  process.stdin.on("error", onBulkLifelineEnd);
  process.stdin.resume();
}

/**
 * Lets go of stdin once the walk has settled. Required, not tidy-up: a
 * flowing stdin is an open handle, and an open handle keeps the event loop -
 * and so this process - alive. The process would never exit, its stdout
 * would never reach EOF, and core, reading that stdout to its end before it
 * ever gets to close the lifeline, would wait forever. This is the one
 * deadlock the lifeline can introduce; `bulk_walk_still_completes_with_its_
 * lifeline_open` in core/tests/plugins_die_with_daemon.rs guards it.
 */
function releaseBulkLifeline(): void {
  process.stdin.removeListener("end", onBulkLifelineEnd);
  process.stdin.removeListener("error", onBulkLifelineEnd);
  process.stdin.destroy();
}

function main(): void {
  const args = process.argv.slice(2);

  // Checked before anything else, including the handshake: in this mode the
  // process is not a plugin at all, it is standing in for the `node` binary
  // that a release archive deliberately does not require the machine to have
  // (see runtime.ts). Anything written to stdout here would land in the middle
  // of the protocol stream of whatever spawned us.
  if (args[0] === RUN_NODE_FLAG) {
    const script = args[1];
    if (script === undefined) {
      log(`${RUN_NODE_FLAG} needs a script to run`);
      process.exitCode = 1;
      return;
    }
    runNodeScript(script, args.slice(2));
    return;
  }

  if (args[0] === BULK_INDEX_FLAG) {
    // No process.exit() on success on purpose: stdout is a pipe, so its last
    // writes can still be in flight, and exiting explicitly would truncate
    // the stream core is reading. Letting the event loop run dry exits only
    // once everything has actually been handed over.
    //
    // A write error on stdout (EPIPE: core stopped reading) ends the walk
    // (GM-397). `waitForDrain` only resumes on it, and later writes to the
    // destroyed stream fail silently, so without this a walk whose reader is
    // gone runs to its end anyway.
    process.stdout.on("error", (err) => {
      log(`failed to write the bulk stream: ${err.message} - exiting`);
      process.exit(1);
    });
    const lifeline = process.env[BULK_STDIN_LIFELINE_ENV] === "1";
    if (lifeline) watchBulkLifeline();
    runBulkIndex(args[1] ?? process.cwd())
      .catch((err) => {
        log(`bulk index failed: ${(err as Error).message}`);
        process.exitCode = 1;
      })
      .finally(() => {
        if (lifeline) releaseBulkLifeline();
      });
    return;
  }

  // Nothing else passes core's control messages a project root (they carry
  // only a file path), so the plugin has to learn it some other way at
  // startup. A CLI arg is the simplest option here since core already spawns
  // this process itself (daemon::plugin::PluginProcess::spawn) and can pass
  // it directly; falling back to cwd keeps a bare `node dist/src/index.js`
  // (as used by the plugin's own e2e tests) working without an argument.
  const projectRoot = args[0] ?? process.cwd();

  sendHandshake();

  const reader = new FrameReader();
  process.stdin.on("data", (chunk: Buffer) => {
    let frames: Buffer[];
    try {
      frames = reader.push(chunk);
    } catch (err) {
      log(`framing error: ${(err as Error).message}`);
      return;
    }
    for (const frame of frames) {
      handleFrame(frame, projectRoot);
    }
  });

  // Core ends a plugin by closing its stdin (daemon::plugin::shutdown), so
  // this is the plugin's whole shutdown path - and the only place a semantic
  // child, which is many times this process's own size, gets to be released
  // deliberately rather than by a backstop. Nothing here starts one: the
  // checker is spawned by the first semantic question asked of it and this
  // is a no-op for the (common) run where none ever is.
  //
  // Awaited (GM-321): `stopSemanticProjects` only resolves once every child
  // has actually exited, not merely been asked to. Calling `process.exit(0)`
  // straight after signaling it, as this used to, raced the child's own
  // teardown - the plugin process could be gone before tsserver was, which
  // is the same shape of bug as GM-320 (a daemon whose caller believed a
  // SIGTERM was a completed shutdown). Nothing here changes what got killed;
  // it changes when this process is allowed to say the killing is done.
  process.stdin.on("end", () => {
    void stopSemanticProjects().then(() => process.exit(0));
  });
}

main();
