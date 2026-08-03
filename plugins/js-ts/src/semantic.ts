// Semantic resolution via a `tsserver` child process.
//
// The structural layer (tree-sitter) answers what it can on its own and
// marks the rest unresolved; this layer upgrades those edges point-wise
// using TypeScript's real type checker. The decision to reach the checker
// through a subprocess rather than `ts.createProgram`/`ts.createLanguageService`
// in this process is recorded, with the measurements behind it, in
// `docs/architecture/g-mesh-v1.md` ("TS semantic layer"). The short version,
// measured on Node 20.6.1 / TypeScript 5.9.3:
//
//   - The compiler API is synchronous. In-process, reaching a first answer
//     stalls this plugin's single event loop for 1677ms (46-file project) to
//     2729ms (618-file monorepo), which stops the control plane and the
//     incremental reparse path dead. Through a subprocess the worst observed
//     stall is 26ms. The pass is specified as async; only one of these is.
//   - An OOM in the checker kills this process (SIGABRT, exit 134) and takes
//     the working structural index down with it. In a child it kills the
//     child; the plugin stayed alive at 24.9MB and kept serving.
//   - The child's ~265MB returns to the OS on kill. In-process, dispose plus
//     forced GC settled at 146.4MB against a 30.8MB baseline, and merely
//     `require`ing the compiler costs +61.5MB before any work at all.
//   - The child can be the *project's own* tsserver, so a project pinned to
//     an older TypeScript is analyzed by the compiler it builds with.
//
// Deliberately standalone: nothing here is wired into index.ts's message
// dispatch, per this ticket's scope (same convention as incremental.ts and
// bulkIndex.ts). The `semanticPass` control message and the runtime/lifecycle
// management of the child are separate tickets.

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import * as path from "node:path";

import { FrameReader } from "./jsonrpc";

/**
 * tsserver's wire protocol is asymmetric, which is the one thing the
 * architecture doc's "a tsserver plugin needs almost no adapter" got wrong:
 *
 *  - **Output** is `Content-Length: N\r\n\r\n{body}` - byte-identical to the
 *    framing core already speaks, so `FrameReader` parses it verbatim.
 *  - **Input** is newline-delimited JSON. A `Content-Length` frame on stdin
 *    is rejected outright (`Unexpected token 'C'`), so requests cannot reuse
 *    `writeMessage`.
 *
 * The envelope differs too: `{seq, type, command, arguments}` rather than
 * JSON-RPC 2.0's `{jsonrpc, id, method, params}`.
 */
interface TsServerRequest {
  seq: number;
  type: "request";
  command: string;
  arguments?: unknown;
}

interface TsServerResponse {
  type: "response";
  request_seq: number;
  command: string;
  success: boolean;
  message?: string;
  body?: unknown;
}

interface TsServerEvent {
  type: "event";
  event: string;
  body?: unknown;
}

type TsServerMessage = TsServerResponse | TsServerEvent;

/** A `{line, offset}` pair. Both are **1-based**, unlike tree-sitter's
 * zero-based `SourcePoint` in incremental.ts - conversion is the caller's
 * job and is the only impedance between the two layers. */
export interface TsServerPosition {
  line: number;
  offset: number;
}

export interface SemanticServerOptions {
  /**
   * Where to find `lib/tsserver.js`. Prefer the project's own install
   * (`<projectRoot>/node_modules/typescript`) so the checker matches what the
   * project actually builds with; fall back to this plugin's bundled copy.
   */
  tsserverPath: string;
  /** Extra argv for the child. The defaults below are always applied first. */
  args?: readonly string[];
}

/**
 * Resolution order for the compiler to drive: the project's own TypeScript,
 * then the copy bundled with this plugin. Returns a path to `lib/tsserver.js`;
 * existence is the caller's check (this is pure path arithmetic so it stays
 * testable without a filesystem).
 */
export function tsserverCandidates(projectRoot: string): string[] {
  const bundled = path.join(__dirname, "..", "..", "node_modules", "typescript", "lib", "tsserver.js");
  return [
    path.join(projectRoot, "node_modules", "typescript", "lib", "tsserver.js"),
    bundled,
  ];
}

/**
 * Applied to every spawn:
 *  - `--disableAutomaticTypingAcquisition`: ATA is the one genuinely
 *    networked thing in the `typescript` package - it npm-installs `@types/*`
 *    for the project. g-mesh indexes what is on disk.
 *  - `--suppressDiagnosticEvents`: tsserver otherwise computes and pushes
 *    diagnostics for every open file. Nothing here consumes them and they are
 *    not free.
 *
 * Just as important is what is **not** here. The security model forbids ever
 * loading a project's `compilerOptions.plugins` entries, which execute code at
 * language-server startup; unlike a bare `LanguageService`, tsserver knows how
 * to load them. Measured against the same malicious-plugin fixture
 * security.test.ts uses: with `--allowLocalPluginLoads` tsserver *does*
 * require() a plugin out of the project's own `node_modules`; without it, it
 * does not, and still answers semantic queries normally. So the mitigation
 * survives the subprocess intact, but it now rests on a default rather than on
 * the code simply having no way to do it - hence the regression test in
 * security.test.ts pinning that neither this flag nor `--globalPlugins` /
 * `--pluginProbeLocations` ever appears here.
 */
const DEFAULT_ARGS = ["--disableAutomaticTypingAcquisition", "--suppressDiagnosticEvents"] as const;

/**
 * A live `tsserver`, addressed request/response.
 *
 * Deliberately thin: no project-loading state machine, no open-file
 * bookkeeping. Point queries do not require `open` first - an unopened file
 * answered in 2.2ms in the prototype - so the semantic pass can walk the
 * unresolved-edge backlog by (file, position) alone.
 */
export class SemanticServer {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly reader = new FrameReader();
  private readonly pending = new Map<number, { resolve: (r: TsServerResponse) => void; reject: (e: Error) => void }>();
  private seq = 0;
  private exited: Error | null = null;

  constructor(projectRoot: string, options: SemanticServerOptions) {
    this.child = spawn(
      process.execPath,
      [options.tsserverPath, ...DEFAULT_ARGS, ...(options.args ?? [])],
      { cwd: projectRoot, stdio: ["pipe", "pipe", "pipe"] },
    ) as ChildProcessWithoutNullStreams;

    this.child.stdout.on("data", (chunk: Buffer) => {
      for (const frame of this.reader.push(chunk)) {
        this.dispatch(frame);
      }
    });

    // A checker OOM lands here rather than taking this process with it -
    // which is the whole point of the subprocess. Everything in flight is
    // failed so the pass can degrade to "left unresolved" instead of hanging.
    this.child.on("exit", (code, signal) => {
      this.exited = new Error(`tsserver exited (code=${code}, signal=${signal})`);
      for (const { reject } of this.pending.values()) reject(this.exited);
      this.pending.clear();
    });
  }

  private dispatch(frame: Buffer): void {
    let message: TsServerMessage;
    try {
      message = JSON.parse(frame.toString("utf8")) as TsServerMessage;
    } catch {
      return; // A frame we cannot parse is not a reason to tear the pass down.
    }
    if (message.type !== "response") return;
    const waiter = this.pending.get(message.request_seq);
    if (waiter === undefined) return;
    this.pending.delete(message.request_seq);
    waiter.resolve(message);
  }

  private write(request: TsServerRequest): void {
    // Newline-delimited, *not* Content-Length framed - see TsServerRequest.
    this.child.stdin.write(`${JSON.stringify(request)}\n`);
  }

  /** Fire-and-forget: `open`, `change`, `close` return no response. */
  notify(command: string, args?: unknown): void {
    if (this.exited !== null) throw this.exited;
    this.seq += 1;
    this.write({ seq: this.seq, type: "request", command, arguments: args });
  }

  /** Request/response. Rejects if the child died with the request in flight. */
  request(command: string, args?: unknown): Promise<TsServerResponse> {
    if (this.exited !== null) return Promise.reject(this.exited);
    this.seq += 1;
    const seq = this.seq;
    return new Promise<TsServerResponse>((resolve, reject) => {
      this.pending.set(seq, { resolve, reject });
      this.write({ seq, type: "request", command, arguments: args });
    });
  }

  /** Frees the child's whole footprint back to the OS - the reclaim path the
   * in-process shape does not have. */
  dispose(): void {
    this.child.kill();
  }
}
