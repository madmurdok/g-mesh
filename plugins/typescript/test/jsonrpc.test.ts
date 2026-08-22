import { test } from "node:test";
import assert from "node:assert/strict";
import { encodeFrame, writeMessage, FrameReader } from "../src/jsonrpc";
import type { ControlEnvelope } from "../src/protocol";

function frameOf(message: unknown): Buffer {
  const chunks: Buffer[] = [];
  const fakeStream = {
    write(chunk: Buffer) {
      chunks.push(chunk);
      return true;
    },
  } as unknown as NodeJS.WritableStream;
  writeMessage(fakeStream, message);
  return Buffer.concat(chunks);
}

test("encodeFrame produces the exact LSP wire format", () => {
  const buf = encodeFrame(Buffer.from('{"ok":true}'));
  assert.equal(buf.toString("utf8"), 'Content-Length: 11\r\n\r\n{"ok":true}');
});

test("FrameReader parses a single frame delivered whole", () => {
  const envelope: ControlEnvelope = {
    jsonrpc: "2.0",
    id: 7,
    method: "reindex",
    params: { filePath: "src/lib.ts" },
  };
  const reader = new FrameReader();
  const frames = reader.push(frameOf(envelope));
  assert.equal(frames.length, 1);
  assert.deepEqual(JSON.parse(frames[0].toString("utf8")), envelope);
});

test("FrameReader reassembles a frame split across many small chunks", () => {
  const envelope: ControlEnvelope = {
    jsonrpc: "2.0",
    method: "fileChanged",
    params: { filePath: "src/main.ts" },
  };
  const whole = frameOf(envelope);
  const reader = new FrameReader();
  const collected: Buffer[] = [];

  // Feed 3 bytes at a time - splits both the header and the body.
  for (let i = 0; i < whole.length; i += 3) {
    const chunk = whole.subarray(i, Math.min(i + 3, whole.length));
    for (const frame of reader.push(chunk)) collected.push(frame);
  }

  assert.equal(collected.length, 1);
  assert.deepEqual(JSON.parse(collected[0].toString("utf8")), envelope);
});

test("FrameReader reads consecutive frames in order", () => {
  const first: ControlEnvelope = { jsonrpc: "2.0", id: 1, method: "status" };
  const second: ControlEnvelope = {
    jsonrpc: "2.0",
    method: "reindex",
    params: { filePath: "a.ts" },
  };
  const stream = Buffer.concat([frameOf(first), frameOf(second)]);

  const reader = new FrameReader();
  const frames = reader.push(stream);
  assert.equal(frames.length, 2);
  assert.deepEqual(JSON.parse(frames[0].toString("utf8")), first);
  assert.deepEqual(JSON.parse(frames[1].toString("utf8")), second);
});

test("headers other than Content-Length are ignored", () => {
  const raw = Buffer.from(
    "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\n{}",
  );
  const reader = new FrameReader();
  const frames = reader.push(raw);
  assert.equal(frames.length, 1);
  assert.equal(frames[0].toString("utf8"), "{}");
});

test("a header block without Content-Length throws instead of hanging", () => {
  const reader = new FrameReader();
  // A real header line, then the blank line that ends the block - which is
  // what makes this malformed. The input used to be a bare "\r\n{}", but that
  // has no header line in it at all, so it exercised the padding case rather
  // than the one the name describes.
  assert.throws(() => reader.push(Buffer.from("X-Whatever: 1\r\n\r\n{}")));
});

// tsserver announces `1 + len` and then writes `${json}${os.EOL}`
// (TypeScript 5.9's `formatMessage`). On Unix those agree; on Windows EOL is
// two bytes, so every frame overruns its own length by one and leaves a
// stray "\n" in front of the next header block. That one byte is what took
// down every semantic-pass test on Windows (GM-248).
function windowsTsserverFrame(message: unknown): Buffer {
  const json = JSON.stringify(message);
  const len = Buffer.byteLength(json, "utf8");
  return Buffer.from(`Content-Length: ${1 + len}\r\n\r\n${json}\r\n`, "utf8");
}

test("consecutive tsserver frames parse despite Windows' off-by-one length", () => {
  const first = { seq: 1, type: "response", command: "open", success: true };
  const second = { seq: 2, type: "response", command: "quickinfo", success: true };
  const reader = new FrameReader();

  const frames = reader.push(
    Buffer.concat([windowsTsserverFrame(first), windowsTsserverFrame(second)]),
  );

  assert.equal(frames.length, 2);
  // The body carries the leading "\r" of the EOL tsserver miscounted, which
  // JSON.parse tolerates as trailing whitespace - the point is that the
  // *second* frame is still found.
  assert.deepEqual(JSON.parse(frames[0].toString("utf8")), first);
  assert.deepEqual(JSON.parse(frames[1].toString("utf8")), second);
});

test("padding between frames is tolerated even when split across chunks", () => {
  const body = '{"ok":true}';
  const reader = new FrameReader();

  assert.deepEqual(reader.push(Buffer.from(`Content-Length: ${body.length}\r\n\r\n${body}`)), [
    Buffer.from(body),
  ]);
  assert.deepEqual(reader.push(Buffer.from("\r")), []);
  assert.deepEqual(reader.push(Buffer.from("\n\r\n")), []);
  assert.deepEqual(reader.push(Buffer.from(`Content-Length: ${body.length}\r\n\r\n${body}`)), [
    Buffer.from(body),
  ]);
});

test("a malformed header line (no colon) throws", () => {
  const reader = new FrameReader();
  assert.throws(() => reader.push(Buffer.from("Content-Length 2\r\n\r\n{}")));
});

test("an unparsable Content-Length value throws", () => {
  const reader = new FrameReader();
  assert.throws(() => reader.push(Buffer.from("Content-Length: nope\r\n\r\n{}")));
});
