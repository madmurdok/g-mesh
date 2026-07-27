import { FrameReader, writeMessage } from "./jsonrpc";
import { parseControlEnvelope, ControlEnvelope, PROTOCOL_VERSION, JSONRPC_VERSION } from "./protocol";

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

function handleEnvelope(envelope: ControlEnvelope): void {
  switch (envelope.method) {
    case "reindex":
      // No tree-sitter parsing / NDJSON bulk emission yet - that's the
      // "tree-sitter parse -> node/edge extraction" and "Initial bulk
      // index" tickets. This ticket only wires up the control plane.
      log(`reindex requested: ${envelope.params?.filePath}`);
      break;
    case "fileChanged":
      log(`file changed: ${envelope.params?.filePath}`);
      break;
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

function handleFrame(frame: Buffer): void {
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

  handleEnvelope(parsed.value);
}

function main(): void {
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
      handleFrame(frame);
    }
  });

  process.stdin.on("end", () => process.exit(0));
}

main();
