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
// Two layers live here, and the split is deliberate:
//
//   - `SemanticServer` is the wire adapter - one live child, request/response
//     over its asymmetric protocol, and nothing else. It knows no g-mesh
//     vocabulary.
//   - `SemanticProject` is the lifecycle: one lazily-started child per project
//     root, started on the first question actually asked of it and stopped
//     with this plugin's own process. It is also where "which project is this
//     file in" is handled, because tsserver will not answer a point query
//     until the file's project is loaded (see `ensureOpen`).
//
// No g-mesh vocabulary reaches either of them: which edges are worth asking
// about, and what a `definition` answer means for the graph, live one layer up
// in semanticPass.ts, which is what index.ts's `semanticPass` handler calls.
// index.ts itself only ties the child's lifetime to the plugin's own.

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { existsSync } from "node:fs";
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

export interface TsServerResponse {
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

/**
 * One `definition` answer: the span of the *name* being declared, plus the
 * span of the whole declaration (`contextStart`/`contextEnd`) where tsserver
 * supplies it. Absolute paths, as tsserver reports them.
 */
export interface DefinitionLocation {
  file: string;
  start: TsServerPosition;
  end: TsServerPosition;
  contextStart?: TsServerPosition;
  contextEnd?: TsServerPosition;
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
  /**
   * How long a single request may stay in flight before it is failed. A hung
   * child must not hang the pass - the backlog walk's failure mode is "this
   * edge stays unresolved", never "the plugin stops answering".
   */
  requestTimeoutMs?: number;
  /** Called once when the child goes away, for whatever reason. */
  onExit?: (error: Error) => void;
}

const DEFAULT_REQUEST_TIMEOUT_MS = 30_000;

/** Keep only the tail of the child's stderr: enough to explain an exit,
 * bounded so a chatty child cannot grow this process's heap. */
const STDERR_TAIL_BYTES = 4096;

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
 * First candidate that actually exists, or null if neither does - which is a
 * real outcome (a plugin installed without its dependencies), and one the
 * caller has to be able to degrade from rather than crash on.
 */
export function resolveTsserverPath(projectRoot: string): string | null {
  for (const candidate of tsserverCandidates(projectRoot)) {
    if (existsSync(candidate)) return candidate;
  }
  return null;
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
 * Every child this module has spawned and not yet reaped. A spawned process
 * is *not* killed when its parent exits - it is orphaned - so an exit hook
 * (installed once, on the first spawn) is what actually ties the checker's
 * ~265MB to this plugin's own lifetime rather than leaving it running after
 * core has torn the plugin down.
 *
 * The hook is the *tidy* path, not the only one. It cannot run when the
 * plugin is SIGKILLed, which is exactly what core does to a plugin that
 * ignores its closed stdin (`daemon::plugin::shutdown`) - and there the
 * backstop is tsserver's own: it exits when its stdin closes. Verified by
 * SIGKILLing a parent holding a live child, after which no `tsserver.js`
 * process remained.
 */
const LIVE_SERVERS = new Set<SemanticServer>();
let exitHookInstalled = false;

/**
 * `unref` on a child's pipe, which the type declarations do not admit to:
 * `ChildProcessWithoutNullStreams` types its stdio as plain `Readable`/
 * `Writable`, but a `"pipe"` stdio is a `net.Socket` at runtime, and only the
 * socket handle's `unref` stops it holding this process's event loop open.
 * Optional-called rather than asserted, so a future stdio shape that really is
 * a bare stream degrades to "stays ref'd" instead of throwing.
 */
function unrefStream(stream: NodeJS.ReadableStream | NodeJS.WritableStream): void {
  (stream as { unref?: () => void }).unref?.();
}

function installExitHook(): void {
  if (exitHookInstalled) return;
  exitHookInstalled = true;
  // "exit" fires on a normal drain and on process.exit() alike (index.ts uses
  // the latter when core closes its end of stdin). Only synchronous work is
  // possible here, which kill() is.
  process.on("exit", () => {
    for (const server of LIVE_SERVERS) server.dispose();
  });
}

/**
 * A live `tsserver`, addressed request/response.
 *
 * Deliberately thin: no project-loading state machine, no open-file
 * bookkeeping - that is `SemanticProject`'s job. The child is `unref`'d, so it
 * never by itself keeps this plugin's event loop alive; an in-flight request's
 * timeout timer does that instead, for exactly as long as an answer is
 * actually being awaited.
 */
export class SemanticServer {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly reader = new FrameReader();
  private readonly pending = new Map<
    number,
    { resolve: (r: TsServerResponse) => void; reject: (e: Error) => void; timer: NodeJS.Timeout }
  >();
  private readonly requestTimeoutMs: number;
  private readonly onExit?: (error: Error) => void;
  private stderrTail = "";
  private seq = 0;
  private exited: Error | null = null;

  constructor(projectRoot: string, options: SemanticServerOptions) {
    this.requestTimeoutMs = options.requestTimeoutMs ?? DEFAULT_REQUEST_TIMEOUT_MS;
    this.onExit = options.onExit;

    this.child = spawn(
      process.execPath,
      [options.tsserverPath, ...DEFAULT_ARGS, ...(options.args ?? [])],
      { cwd: projectRoot, stdio: ["pipe", "pipe", "pipe"] },
    ) as ChildProcessWithoutNullStreams;

    LIVE_SERVERS.add(this);
    installExitHook();

    this.child.unref();
    unrefStream(this.child.stdin);
    unrefStream(this.child.stdout);
    unrefStream(this.child.stderr);

    this.child.stdout.on("data", (chunk: Buffer) => {
      let frames: Buffer[];
      try {
        frames = this.reader.push(chunk);
      } catch (err) {
        // Framing is unrecoverable mid-stream: the reader has lost its place
        // in the byte stream and every later frame would be garbage.
        this.fail(new Error(`tsserver framing error: ${(err as Error).message}`));
        this.dispose();
        return;
      }
      for (const frame of frames) this.dispatch(frame);
    });

    // Drained, not ignored: an unread pipe fills its buffer and then blocks
    // the writer - tsserver would stop answering rather than crash, which is
    // the harder failure to diagnose. The tail is kept to explain an exit.
    this.child.stderr.on("data", (chunk: Buffer) => {
      this.stderrTail = (this.stderrTail + chunk.toString("utf8")).slice(-STDERR_TAIL_BYTES);
    });

    // A write to a dead child's stdin raises EPIPE asynchronously; unhandled,
    // that is an uncaught exception in the *plugin*, which is precisely the
    // blast radius the subprocess exists to prevent.
    this.child.stdin.on("error", (err: Error) => {
      this.fail(new Error(`tsserver stdin write failed: ${err.message}`));
    });

    // Spawn itself failing (no such file, EACCES) surfaces here, not as a
    // throw from spawn().
    this.child.on("error", (err: Error) => {
      this.fail(new Error(`tsserver could not be started: ${err.message}`));
    });

    // A checker OOM lands here rather than taking this process with it -
    // which is the whole point of the subprocess. Everything in flight is
    // failed so the pass can degrade to "left unresolved" instead of hanging.
    this.child.on("exit", (code, signal) => {
      const detail = this.stderrTail.trim();
      this.fail(
        new Error(
          `tsserver exited (code=${code}, signal=${signal})${detail === "" ? "" : `: ${detail}`}`,
        ),
      );
    });
  }

  /** False once the child has died or been disposed. The owner uses this to
   * decide whether to spawn a replacement rather than to resurrect this one. */
  get isAlive(): boolean {
    return this.exited === null;
  }

  /** Terminal for this instance: records the cause once, fails everything in
   * flight with it, and stops the exit hook tracking a child that is gone. */
  private fail(error: Error): void {
    if (this.exited === null) {
      this.exited = error;
      LIVE_SERVERS.delete(this);
      this.onExit?.(error);
    }
    for (const { reject, timer } of this.pending.values()) {
      clearTimeout(timer);
      reject(this.exited);
    }
    this.pending.clear();
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
    if (waiter === undefined) return; // Late answer to a timed-out request.
    this.pending.delete(message.request_seq);
    clearTimeout(waiter.timer);
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

  /**
   * Request/response. Rejects if the child died with the request in flight,
   * or if no answer arrived within the timeout. Resolves with the response
   * even when `success` is false - a tsserver-level "no" (`No Project`, an
   * unknown command) is an answer, and its caller is the one that knows
   * whether it is fatal.
   */
  request(command: string, args?: unknown): Promise<TsServerResponse> {
    if (this.exited !== null) return Promise.reject(this.exited);
    this.seq += 1;
    const seq = this.seq;
    return new Promise<TsServerResponse>((resolve, reject) => {
      // Ref'd on purpose (the child and its pipes are not): while an answer
      // is genuinely being awaited, this process must not exit out from under
      // it. It is cleared the moment the answer lands.
      const timer = setTimeout(() => {
        this.pending.delete(seq);
        reject(new Error(`tsserver request "${command}" timed out after ${this.requestTimeoutMs}ms`));
      }, this.requestTimeoutMs);
      this.pending.set(seq, { resolve, reject, timer });
      this.write({ seq, type: "request", command, arguments: args });
    });
  }

  /** Frees the child's whole footprint back to the OS - the reclaim path the
   * in-process shape does not have. Idempotent. */
  dispose(): void {
    const alreadyDead = this.exited !== null;
    this.child.kill();
    if (!alreadyDead) this.fail(new Error("tsserver was disposed"));
  }
}

export interface SemanticProjectOptions {
  /** Override the tsserver resolution order (tests, and a pinned compiler). */
  tsserverPath?: string;
  args?: readonly string[];
  requestTimeoutMs?: number;
  /** Diagnostics sink. The pass must survive the child dying, so this is
   * where that gets reported rather than thrown at whoever asked last. */
  onLog?: (message: string) => void;
}

/**
 * One project root's semantic capability, with the child's lifecycle attached
 * to this plugin's own.
 *
 * **Lazy.** Constructing this spawns nothing - a plain-JS project with no
 * `tsconfig.json`, or a run where no semantic question is ever asked, must not
 * pay the child's ~265MB or its ~1.2s startup. The child appears on the first
 * query and, like core's own handling of a dead plugin, is replaced lazily on
 * the next query if it died.
 *
 * **Stopped with the plugin.** `stop()` kills it; so does the module's
 * process-exit hook, so an abrupt teardown cannot orphan a checker.
 */
export class SemanticProject {
  private server: SemanticServer | null = null;
  /**
   * Files handed to tsserver's `open`. Two reasons this set exists:
   *
   *  - tsserver answers a point query only once the file's *project* is
   *    loaded. Measured against a freshly spawned child: `definition` on an
   *    unopened file fails outright with `No Project`. Once *some* file in
   *    that project has been opened, an unopened sibling answers in ~2ms -
   *    the project, not the file, is what had to be built - so this is the
   *    minimum state that has to be tracked, not a per-file requirement.
   *  - `open` is a notification, so making one per file costs no round trip;
   *    the set exists to keep it to one per file rather than one per query.
   */
  private readonly opened = new Set<string>();

  constructor(
    private readonly projectRoot: string,
    private readonly options: SemanticProjectOptions = {},
  ) {}

  /** Whether a child is currently running. False before the first query. */
  get isRunning(): boolean {
    return this.server !== null && this.server.isAlive;
  }

  /**
   * The running child, started if there is none and replaced if the last one
   * died. Throws only when no tsserver exists to start at all - the one
   * failure a retry cannot fix.
   */
  ensureServer(): SemanticServer {
    if (this.server !== null && !this.server.isAlive) {
      // A replacement child knows nothing of the dead one's open files.
      this.server = null;
      this.opened.clear();
    }
    if (this.server !== null) return this.server;

    const tsserverPath = this.options.tsserverPath ?? resolveTsserverPath(this.projectRoot);
    if (tsserverPath === null) {
      throw new Error(
        `no tsserver found for ${this.projectRoot} (looked in ${tsserverCandidates(this.projectRoot).join(", ")})`,
      );
    }

    this.options.onLog?.(`starting tsserver for ${this.projectRoot} from ${tsserverPath}`);
    this.server = new SemanticServer(this.projectRoot, {
      tsserverPath,
      args: this.options.args,
      requestTimeoutMs: this.options.requestTimeoutMs,
      onExit: (error) => this.options.onLog?.(`tsserver stopped: ${error.message}`),
    });
    return this.server;
  }

  /**
   * Loads the file's project if this is the first time it has been seen.
   *
   * `projectRootPath` is what keeps a file with no `tsconfig.json` above it
   * from landing in an inferred project rooted wherever tsserver felt like;
   * where there *is* a `tsconfig.json`, tsserver finds it by walking up from
   * the file itself and applies it whole - `paths` aliases and all - which is
   * why this layer does not reuse tsconfigPaths.ts. Verified, not assumed:
   * `configuredProjectFor` reports the config actually picked up, and
   * semantic.test.ts pins that an aliased import resolves through it.
   */
  private ensureOpen(server: SemanticServer, file: string): void {
    if (this.opened.has(file)) return;
    // Note the absent `fileContent`: tsserver reads the file off disk, so the
    // index and the checker see the same bytes. Once open, the file's content
    // is client-owned - keeping it in step after an edit is the `change`
    // command's job, and part of the incremental-pass ticket.
    server.notify("open", { file, projectRootPath: this.projectRoot });
    this.opened.add(file);
  }

  /**
   * The one real semantic question this ticket proves end to end: where is
   * the thing at this position declared? An empty array is a real answer
   * ("nothing is declared there"), and stays distinct from a failure, which
   * throws.
   */
  async definition(file: string, position: TsServerPosition): Promise<DefinitionLocation[]> {
    const server = this.ensureServer();
    this.ensureOpen(server, file);
    const response = await server.request("definition", {
      file,
      line: position.line,
      offset: position.offset,
    });
    if (!response.success) {
      // tsserver puts a whole stack in `message`; the first line is the part
      // that says what actually went wrong.
      const reason = (response.message ?? "unknown error").split("\n")[0];
      throw new Error(`tsserver definition failed for ${file}:${position.line}:${position.offset}: ${reason}`);
    }
    return (response.body as DefinitionLocation[] | undefined) ?? [];
  }

  /**
   * Which `tsconfig.json` tsserver actually applied to this file, or null for
   * an inferred project (a plain-JS tree, or a file outside every config).
   * Diagnostic rather than load-bearing - but it is the only way to *check*
   * the project's own config is in force rather than to hope so.
   */
  async configuredProjectFor(file: string): Promise<string | null> {
    const server = this.ensureServer();
    this.ensureOpen(server, file);
    const response = await server.request("projectInfo", { file, needFileNameList: false });
    if (!response.success) return null;
    const name = (response.body as { configFileName?: string } | undefined)?.configFileName;
    // An inferred project's name is a synthetic path (`/dev/null/inferredProject1*`),
    // never a config that exists.
    if (name === undefined || !name.endsWith(".json")) return null;
    return name;
  }

  /** Stops the child, if one is running. Idempotent; a later query starts a
   * fresh one, which is what makes this safe to call on an idle timer as well
   * as at shutdown. */
  stop(): void {
    if (this.server === null) return;
    this.options.onLog?.(`stopping tsserver for ${this.projectRoot}`);
    this.server.dispose();
    this.server = null;
    this.opened.clear();
  }
}

/**
 * One `SemanticProject` per project root, so the plugin cannot end up with
 * two checkers for one tree. index.ts holds no state of its own for this; it
 * only needs `stopSemanticProjects()` at teardown.
 */
const PROJECTS = new Map<string, SemanticProject>();

export function semanticProjectFor(projectRoot: string, options?: SemanticProjectOptions): SemanticProject {
  const key = path.resolve(projectRoot);
  let project = PROJECTS.get(key);
  if (project === undefined) {
    project = new SemanticProject(key, options);
    PROJECTS.set(key, project);
  }
  return project;
}

/** Teardown counterpart, called on the plugin's own shutdown path. */
export function stopSemanticProjects(): void {
  for (const project of PROJECTS.values()) project.stop();
  PROJECTS.clear();
}
