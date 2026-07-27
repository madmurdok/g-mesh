// JSON-compatible mirror of core/src/protocol/types.rs. Field casing here
// must match what serde actually produces on the Rust side, not a naive
// reading of the struct - see that file's `#[serde(rename_all = ...)]`
// attributes and its test module for the ground truth.

// Bumped in lockstep with core's CURRENT_PROTOCOL_VERSION.
export const PROTOCOL_VERSION = 1;
export const JSONRPC_VERSION = "2.0";

export interface Handshake {
  protocolVersion: number;
  language: string;
  pluginVersion: string;
}

export type RequestId = number | string;

export type ControlMethod = "reindex" | "fileChanged" | "status";

// Mirrors ControlMessage: Reindex/FileChanged carry `params: { filePath }`,
// Status carries no `params` field at all (serde's adjacently-tagged
// encoding omits `content` entirely for a unit variant).
export interface ControlEnvelope {
  jsonrpc: "2.0";
  id?: RequestId;
  method: ControlMethod;
  params?: { filePath: string };
}

export type ParseResult<T> = { ok: true; value: T } | { ok: false; error: string };

/**
 * Validates untrusted JSON into a ControlEnvelope. Deliberately manual
 * (no schema library) per the ticket's scope - just enough to never crash
 * on malformed input, matching the Rust side's "never panic on untrusted
 * data" stance.
 */
export function parseControlEnvelope(value: unknown): ParseResult<ControlEnvelope> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    return { ok: false, error: "expected a JSON object" };
  }
  const obj = value as Record<string, unknown>;

  if (obj.jsonrpc !== JSONRPC_VERSION) {
    return { ok: false, error: `expected jsonrpc: "${JSONRPC_VERSION}"` };
  }

  if (obj.id !== undefined && typeof obj.id !== "number" && typeof obj.id !== "string") {
    return { ok: false, error: "id must be a number or string when present" };
  }

  if (obj.method !== "reindex" && obj.method !== "fileChanged" && obj.method !== "status") {
    return { ok: false, error: `unknown method: ${JSON.stringify(obj.method)}` };
  }

  if (obj.method === "reindex" || obj.method === "fileChanged") {
    const params = obj.params as Record<string, unknown> | undefined;
    if (typeof params !== "object" || params === null || typeof params.filePath !== "string") {
      return { ok: false, error: `${obj.method} requires params.filePath: string` };
    }
    return {
      ok: true,
      value: {
        jsonrpc: JSONRPC_VERSION,
        id: obj.id as RequestId | undefined,
        method: obj.method,
        params: { filePath: params.filePath },
      },
    };
  }

  return {
    ok: true,
    value: { jsonrpc: JSONRPC_VERSION, id: obj.id as RequestId | undefined, method: "status" },
  };
}
