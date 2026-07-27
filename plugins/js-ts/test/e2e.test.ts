import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn, ChildProcessWithoutNullStreams } from "node:child_process";
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
