import { FrameReader, writeMessage } from "./jsonrpc";
import { parseControlEnvelope, ControlEnvelope, PROTOCOL_VERSION, JSONRPC_VERSION } from "./protocol";
import { reparseChangedFile, type FileDiff } from "./incremental";
import { toWireNode, type WireEdge, type WireNode } from "./bulkIndex";

const PLUGIN_VERSION = "0.1.0"; // keep in sync with package.json's "version"

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

async function handleEnvelope(envelope: ControlEnvelope, projectRoot: string): Promise<void> {
  switch (envelope.method) {
    case "reindex":
      // No NDJSON bulk-index wiring yet - that would mean streaming
      // bulkIndex.ts's output back over this same control-plane connection,
      // which is a different transport shape than the request/response
      // FileChanged uses and not required by this ticket's acceptance
      // criteria. Left for a later ticket.
      log(`reindex requested: ${envelope.params?.filePath}`);
      break;
    case "fileChanged":
      await handleFileChanged(projectRoot, envelope.params?.filePath ?? "", envelope.id);
      return; // handleFileChanged already sent the (only) response, if any
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

function main(): void {
  // Nothing else passes core's control messages a project root (they carry
  // only a file path), so the plugin has to learn it some other way at
  // startup. A CLI arg is the simplest option here since core already spawns
  // this process itself (daemon::plugin::PluginProcess::spawn) and can pass
  // it directly; falling back to cwd keeps a bare `node dist/src/index.js`
  // (as used by the plugin's own e2e tests) working without an argument.
  const projectRoot = process.argv[2] ?? process.cwd();

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

  process.stdin.on("end", () => process.exit(0));
}

main();
