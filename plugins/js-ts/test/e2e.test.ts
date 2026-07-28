import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, ChildProcessWithoutNullStreams } from "node:child_process";
import * as fs from "node:fs/promises";
import * as os from "node:os";
import path from "node:path";
import { FrameReader, encodeFrame } from "../src/jsonrpc";
import { PROTOCOL_VERSION } from "../src/protocol";

// End-to-end tests: spawn the actual compiled plugin process and speak the
// wire protocol at it exactly as core would, playing the "core" role.
const ENTRY = path.join(__dirname, "..", "src", "index.js");

function spawnPlugin(): ChildProcessWithoutNullStreams {
  return spawn(process.execPath, [ENTRY], { stdio: ["pipe", "pipe", "pipe"] });
}

interface FrameCollector {
  frames: Buffer[];
  wait(n: number, timeoutMs?: number): Promise<void>;
}

function collectFrames(stream: NodeJS.ReadableStream): FrameCollector {
  const reader = new FrameReader();
  const frames: Buffer[] = [];
  const waiters: Array<{ n: number; resolve: () => void }> = [];

  stream.on("data", (chunk: Buffer) => {
    for (const frame of reader.push(chunk)) frames.push(frame);
    for (let i = waiters.length - 1; i >= 0; i--) {
      if (frames.length >= waiters[i].n) {
        waiters[i].resolve();
        waiters.splice(i, 1);
      }
    }
  });

  function wait(n: number, timeoutMs = 5000): Promise<void> {
    if (frames.length >= n) return Promise.resolve();
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`timed out waiting for frame #${n}`)), timeoutMs);
      waiters.push({ n, resolve: () => { clearTimeout(timer); resolve(); } });
    });
  }

  return { frames, wait };
}

test("plugin completes handshake with the expected protocol version and language", async () => {
  const child = spawnPlugin();
  const out = collectFrames(child.stdout);

  try {
    await out.wait(1);
    const handshake = JSON.parse(out.frames[0].toString("utf8"));
    assert.equal(handshake.protocolVersion, PROTOCOL_VERSION);
    assert.equal(handshake.language, "typescript");
    assert.equal(typeof handshake.pluginVersion, "string");
  } finally {
    child.kill();
  }
});

test("plugin parses a framed reindex request end to end and responds", async () => {
  const child = spawnPlugin();
  const out = collectFrames(child.stdout);

  try {
    await out.wait(1); // handshake

    const request = {
      jsonrpc: "2.0",
      id: 42,
      method: "reindex",
      params: { filePath: "src/lib.ts" },
    };
    child.stdin.write(encodeFrame(Buffer.from(JSON.stringify(request))));

    await out.wait(2); // handshake + response
    const response = JSON.parse(out.frames[1].toString("utf8"));
    assert.equal(response.jsonrpc, "2.0");
    assert.equal(response.id, 42);
    assert.ok(response.result);

    assert.equal(child.exitCode, null, "process must still be alive after handling the message");
  } finally {
    child.kill();
  }
});

test("plugin handles a fileChanged notification without crashing and without responding", async () => {
  const child = spawnPlugin();
  const out = collectFrames(child.stdout);
  const stderrLines: string[] = [];
  child.stderr.on("data", (c: Buffer) => stderrLines.push(c.toString("utf8")));

  try {
    await out.wait(1); // handshake

    const notification = {
      jsonrpc: "2.0",
      method: "fileChanged",
      params: { filePath: "src/main.ts" },
    };
    child.stdin.write(encodeFrame(Buffer.from(JSON.stringify(notification))));

    // Notifications get no response frame; give the process a moment to log
    // and confirm it's still alive rather than waiting on a frame that
    // should never arrive.
    await new Promise((resolve) => setTimeout(resolve, 300));

    assert.equal(out.frames.length, 1, "a notification must not produce a response frame");
    assert.equal(child.exitCode, null, "process must still be alive");
    assert.ok(
      stderrLines.some((line) => line.includes("file changed")),
      "expected a log line acknowledging the notification",
    );
  } finally {
    child.kill();
  }
});

/**
 * The one-shot mode core spawns for a cold start (`daemon::bulk_index`). What
 * matters to core is the *stream contract*, not the extraction: the whole of
 * stdout is NDJSON with no handshake in front of it, and the process ends by
 * itself once everything has been written - that end is core's only
 * end-of-index signal.
 */
test("--bulk-index streams the project as NDJSON on stdout and then exits", async () => {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), "gmesh-e2e-bulk-"));
  await fs.mkdir(path.join(root, "src"));
  await fs.writeFile(
    path.join(root, "src", "a.ts"),
    "export function alpha(): number {\n  return 1;\n}\n",
    "utf8",
  );

  try {
    const child = spawn(process.execPath, [ENTRY, "--bulk-index", root], {
      stdio: ["ignore", "pipe", "pipe"],
    });

    const stdout: Buffer[] = [];
    child.stdout.on("data", (chunk: Buffer) => stdout.push(chunk));

    const exitCode = await new Promise<number | null>((resolve, reject) => {
      child.on("error", reject);
      child.on("close", (code) => resolve(code));
    });

    assert.equal(exitCode, 0, "a completed bulk index must exit cleanly");

    const text = Buffer.concat(stdout).toString("utf8");
    const lines = text.split("\n").filter((line) => line.length > 0);
    assert.ok(lines.length > 0, "expected NDJSON output for a non-empty project");

    const parsed = lines.map((line) => JSON.parse(line));
    // A Content-Length header ahead of the payload would mean the control
    // plane's framing leaked into a stream that must be plain NDJSON.
    assert.ok(!text.includes("Content-Length"), "bulk output must not be framed");
    assert.ok(
      parsed.some((p) => p.kind === "File" && p.filePath === "src/a.ts"),
      "expected the walked file's File node",
    );
    assert.ok(
      parsed.some((p) => p.kind === "Function" && p.name === "alpha"),
      "expected the walked file's symbols",
    );
  } finally {
    await fs.rm(root, { recursive: true, force: true });
  }
});

test("malformed JSON body does not crash the plugin", async () => {
  const child = spawnPlugin();
  const out = collectFrames(child.stdout);
  const stderrLines: string[] = [];
  child.stderr.on("data", (c: Buffer) => stderrLines.push(c.toString("utf8")));

  try {
    await out.wait(1); // handshake
    child.stdin.write(encodeFrame(Buffer.from("not json")));

    await new Promise((resolve) => setTimeout(resolve, 300));

    assert.equal(child.exitCode, null, "process must survive a malformed frame body");
    assert.ok(stderrLines.some((line) => line.includes("malformed control message JSON")));
  } finally {
    child.kill();
  }
});
