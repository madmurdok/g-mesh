import { FrameReader, writeMessage } from "./jsonrpc";
import { parseControlEnvelope, ControlEnvelope, PROTOCOL_VERSION, JSONRPC_VERSION } from "./protocol";
import { reparseChangedFile, type FileDiff } from "./incremental";
import { bulkIndexProject, toWireNode, type WireEdge, type WireNode } from "./bulkIndex";
import { stopSemanticProjects } from "./semantic";
import { runSemanticPass } from "./semanticPass";

const PLUGIN_VERSION = "0.1.0"; // keep in sync with package.json's "version"

/** Selects one-shot bulk-index mode instead of the control-plane loop; must
 * stay in sync with core's `daemon::bulk_index::BULK_INDEX_FLAG`. */
const BULK_INDEX_FLAG = "--bulk-index";

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
 * Answers a semantic pass: what TypeScript's own checker can resolve that
 * tree-sitter could not (semanticPass.ts).
 *
 * An empty `filePaths` means the whole project - the wire's own convention,
 * sent once the cold-start walk lands - and a one-entry list a reparse that
 * just settled. The pass itself distinguishes them; everything else here is
 * the same contract `handleFileChanged` has.
 *
 * A failure answers with an empty diff rather than propagating. Core blocks on
 * a response to any request it sent, so a request must always be answered with
 * a diff - never with the `{ acknowledged: true }` shape the no-op methods
 * use, which core would fail to deserialize as one - and an empty diff is the
 * honest answer for a pass that could not run: the index keeps whatever the
 * structural layer already resolved, which is the state it was in anyway.
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
      `semantic pass over ${pass.filesScanned} file(s): ${pass.upsertEdges.length} edge(s) resolved, ` +
        `${pass.deleteEdgeIds.length} retracted, ${pass.unresolvedUses} left unresolved`,
    );
  } catch (err) {
    log(`semantic pass failed: ${(err as Error).message}`);
  }

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

function main(): void {
  const args = process.argv.slice(2);

  if (args[0] === BULK_INDEX_FLAG) {
    // No process.exit() on success on purpose: stdout is a pipe, so its last
    // writes can still be in flight, and exiting explicitly would truncate
    // the stream core is reading. Letting the event loop run dry exits only
    // once everything has actually been handed over.
    runBulkIndex(args[1] ?? process.cwd()).catch((err) => {
      log(`bulk index failed: ${(err as Error).message}`);
      process.exitCode = 1;
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
  process.stdin.on("end", () => {
    stopSemanticProjects();
    process.exit(0);
  });
}

main();
