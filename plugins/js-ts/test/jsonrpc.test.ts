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

test("a header line without Content-Length throws instead of hanging", () => {
  const reader = new FrameReader();
  assert.throws(() => reader.push(Buffer.from("\r\n{}")));
});

test("a malformed header line (no colon) throws", () => {
  const reader = new FrameReader();
  assert.throws(() => reader.push(Buffer.from("Content-Length 2\r\n\r\n{}")));
});

test("an unparsable Content-Length value throws", () => {
  const reader = new FrameReader();
  assert.throws(() => reader.push(Buffer.from("Content-Length: nope\r\n\r\n{}")));
});
