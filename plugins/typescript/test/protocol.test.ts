import { test } from "node:test";
import assert from "node:assert/strict";

import { parseControlEnvelope } from "../src/protocol";

// The control envelope is this plugin's only untrusted input, and
// parseControlEnvelope is the single place that decides what a well-formed
// one looks like. These pin the semanticPass rules specifically - what the
// method needs, and what it must reject rather than quietly accept.

function envelope(extra: Record<string, unknown>): unknown {
  return { jsonrpc: "2.0", id: 1, ...extra };
}

test("a semanticPass naming files parses into a string list", () => {
  const parsed = parseControlEnvelope(
    envelope({ method: "semanticPass", params: { filePaths: ["src/a.ts", "src/b.ts"] } }),
  );

  assert.equal(parsed.ok, true);
  if (!parsed.ok) return;
  assert.equal(parsed.value.method, "semanticPass");
  assert.deepEqual(parsed.value.params?.filePaths, ["src/a.ts", "src/b.ts"]);
});

test("an empty filePaths list is valid - it means the whole project", () => {
  const parsed = parseControlEnvelope(envelope({ method: "semanticPass", params: { filePaths: [] } }));

  assert.equal(parsed.ok, true);
  if (!parsed.ok) return;
  assert.deepEqual(parsed.value.params?.filePaths, []);
});

// Core always spells the list out (serde serializes the Vec either way), so
// an absent one is a malformed request rather than shorthand for "all".
test("semanticPass without filePaths is rejected", () => {
  const parsed = parseControlEnvelope(envelope({ method: "semanticPass", params: {} }));

  assert.equal(parsed.ok, false);
  if (parsed.ok) return;
  assert.match(parsed.error, /filePaths/);
});

test("semanticPass with a non-string entry is rejected", () => {
  const parsed = parseControlEnvelope(
    envelope({ method: "semanticPass", params: { filePaths: ["src/a.ts", 7] } }),
  );

  assert.equal(parsed.ok, false);
});

test("semanticPass with filePath (singular) is rejected", () => {
  const parsed = parseControlEnvelope(
    envelope({ method: "semanticPass", params: { filePath: "src/a.ts" } }),
  );

  assert.equal(parsed.ok, false);
});

test("adding semanticPass did not loosen the other methods", () => {
  assert.equal(parseControlEnvelope(envelope({ method: "fileChanged", params: {} })).ok, false);
  assert.equal(parseControlEnvelope(envelope({ method: "status" })).ok, true);
  assert.equal(parseControlEnvelope(envelope({ method: "semanticPasss", params: {} })).ok, false);
});
