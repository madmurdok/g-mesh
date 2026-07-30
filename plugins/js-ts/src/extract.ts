// Syntax-only (tree-sitter) extraction of graph nodes/edges from a single
// JS/TS source file. Field names mirror core's NodeRecord/EdgeRecord
// (core/src/storage/write.rs) in camelCase; the NDJSON wire encoding that
// carries them to the core is a separate ticket, as is any TS-compiler-API
// semantic layer - everything produced here is name-based and therefore
// `resolved: false` / `source: 'tree-sitter'`. The one exception is module
// specifiers, which are literals rather than names: the ones naming a file of
// this project - relative, or a package of its own workspace - are resolved to
// a real path here (see `recordImport`) and core turns that into
// a `resolved: true` edge once it has confirmed the target is in the index.
// Symbols reached *through* such a specifier ride on the same handshake: a
// call or reference to an imported name gets a pending-symbol placeholder
// (see `PENDING_SYMBOL_NATIVE_KIND`) naming the file and the export it is
// waiting for, which core links the same way. Where that file only passes the
// name through - a barrel doing `export * from`/`export { x } from`, which is
// what a workspace specifier usually lands on - a re-export placeholder (see
// `REEXPORT_NATIVE_KIND`) records the next hop, so core can follow the chain
// to wherever the declaration really is.

import { createHash } from "node:crypto";
import * as path from "node:path";
import Parser from "tree-sitter";
import JavaScript from "tree-sitter-javascript";
import TypeScript from "tree-sitter-typescript";

type SyntaxNode = Parser.SyntaxNode;

export type NodeKind = "File" | "Module" | "Type" | "Function" | "Variable";

export type EdgeKind = "DEFINES" | "IMPORTS" | "CALLS" | "SUPERTYPE_OF" | "REFERENCES" | "EXPORTS";

export type EdgeSource = "tree-sitter" | "ts-compiler";

/** Everything this module emits comes from the structural pass only. */
export const EDGE_SOURCE: EdgeSource = "tree-sitter";

/**
 * `nativeKind` of an import placeholder whose specifier was resolved to a
 * file in this project: its `qualifiedName` is that file's project-relative
 * path rather than the raw specifier text, which is core's cue to repoint the
 * IMPORTS edge at the real `File` node (`core/src/graph/imports.rs` - the
 * constant is mirrored there, so the two must be changed together).
 * Unresolved targets keep `external_module`.
 */
export const RESOLVED_MODULE_NATIVE_KIND = "resolved_module";

/**
 * `nativeKind` of a placeholder standing in for a symbol this file *imports*
 * from another file in this project. The extractor only ever sees one file,
 * so `foo()` after `import { foo } from "./x"` has no local declaration to
 * point a CALLS edge at - and dropping the edge, which is what used to
 * happen, is what made find_callers/find_references/find_implementations
 * blind to every cross-file usage.
 *
 * The placeholder is the same handshake `RESOLVED_MODULE_NATIVE_KIND` uses,
 * one level finer: its `qualifiedName` is
 * [`pendingSymbolQualifiedName`]`(target file, imported name)` and its `name`
 * is the imported name, which is everything core needs to find the real
 * symbol once the target file is in the index and repoint the edge at it
 * (`core/src/graph/symbol_links.rs` - the constant and the `#` convention are
 * mirrored there, so the two must be changed together).
 */
export const PENDING_SYMBOL_NATIVE_KIND = "pending_symbol";

/**
 * `nativeKind` of a placeholder recording that this file *publishes* a name it
 * does not declare - `export { a } from "./y"`, `export { a as b } from "./y"`,
 * `export * from "./y"`. A barrel/index file is made of nothing else, and in a
 * monorepo it is what a workspace specifier resolves to, so without this a
 * usage reached through `@scope/pkg` addresses a file where the name is only
 * passed through and core's linking finds nothing declared there.
 *
 * The placeholder is addressed exactly like a pending symbol - its
 * `qualifiedName` is [`pendingSymbolQualifiedName`]`(target file, name in that
 * file)` - while its `name` is the name *this* file publishes, which the two
 * differ on whenever an alias renamed it. That pair is the whole fact core
 * needs to follow the chain one hop further (`core/src/graph/symbol_links.rs` -
 * the constant is mirrored there, so the two must be changed together).
 *
 * Which file the target really is, and whether the chain ends in a declaration
 * at all, stays core's call for the usual reason: the target may not be
 * indexed yet, or ever.
 */
export const REEXPORT_NATIVE_KIND = "reexport";

/**
 * The name a whole-module re-export (`export * from "./y"`) is recorded under,
 * at both ends of its address: it publishes every name the target exports
 * rather than one, so neither end can be spelled out until core knows what
 * that file exports. `*` cannot collide with a real symbol - no identifier is
 * spelled that way.
 */
export const REEXPORT_ALL_NAME = "*";

/**
 * How a pending-symbol placeholder is addressed: the target file's
 * project-relative path and the name that file is expected to export, in one
 * string, because `qualifiedName` is the only identity field a node id is
 * derived from (see [`nodeIdFor`]) and two symbols imported from the same
 * file must not collapse into one placeholder.
 *
 * Core splits it back apart on the *last* `#`, which is exact: a symbol name
 * never contains one, whatever the file path does.
 */
export function pendingSymbolQualifiedName(targetFilePath: string, importedName: string): string {
  return `${targetFilePath}#${importedName}`;
}

/**
 * Mirrors core's `NodeRecord`. Line/column are tree-sitter's native
 * zero-based row/column (same convention as LSP and the TS compiler API);
 * any 1-based presentation happens at the MCP layer.
 */
export interface ExtractedNode {
  id: string;
  kind: NodeKind;
  name: string;
  qualifiedName: string;
  filePath: string;
  startLine: number;
  startCol: number;
  endLine: number;
  endCol: number;
  signature?: string;
  exported: boolean;
  docComment?: string;
  language: string;
  nativeKind?: string;
  hasSyntaxErrors: boolean;
}

/** Mirrors core's `EdgeRecord`. */
export interface ExtractedEdge {
  id: string;
  fromId: string;
  toId: string;
  kind: EdgeKind;
  source: EdgeSource;
  resolved: boolean;
}

export interface ExtractResult {
  nodes: ExtractedNode[];
  edges: ExtractedEdge[];
  /** True when tree-sitter's error-tolerant parse hit ERROR/MISSING nodes. */
  hasSyntaxErrors: boolean;
}

/**
 * Turns a module specifier written in `fromFilePath` into the
 * project-relative POSIX path it names, or `null` when it names nothing in
 * this project (a bare/package specifier, a dangling relative import, a file
 * this plugin does not parse). Implemented by resolve.ts and injected here so
 * the extractor itself stays a pure function of (path, text): the resolution
 * policy is filesystem-dependent, this walk is not.
 */
export type SpecifierResolver = (specifier: string, fromFilePath: string) => string | null;

export interface ExtractOptions {
  /**
   * Omitted (the default) means no specifier resolution at all - every import
   * target stays a raw-specifier placeholder, exactly as before this option
   * existed. Callers that have a project root to resolve against pass
   * `createProjectResolver(projectRoot)` from resolve.ts.
   */
  resolveSpecifier?: SpecifierResolver;
}

export class UnsupportedFileError extends Error {
  constructor(filePath: string) {
    super(`unsupported file extension for the js-ts plugin: ${filePath}`);
    this.name = "UnsupportedFileError";
  }
}

type GrammarKey = "typescript" | "tsx" | "javascript";

interface GrammarChoice {
  readonly key: GrammarKey;
  readonly grammar: unknown;
  /** Value written to a node's `language` column. */
  readonly language: "typescript" | "javascript";
}

/**
 * Grammar per extension: tree-sitter-javascript already understands JSX, so
 * `.jsx` uses it; only TypeScript needs the separate `tsx` grammar, whose
 * JSX support costs the `<T>expr` cast syntax that plain `.ts` files use.
 */
function grammarFor(filePath: string): GrammarChoice | null {
  switch (path.extname(filePath).toLowerCase()) {
    case ".ts":
    case ".mts":
    case ".cts":
      return { key: "typescript", grammar: TypeScript.typescript, language: "typescript" };
    case ".tsx":
      return { key: "tsx", grammar: TypeScript.tsx, language: "typescript" };
    case ".js":
    case ".mjs":
    case ".cjs":
    case ".jsx":
      return { key: "javascript", grammar: JavaScript, language: "javascript" };
    default:
      return null;
  }
}

export function isSupportedFile(filePath: string): boolean {
  return grammarFor(filePath) !== null;
}

// Parsers are stateful but reusable, and constructing one loads a native
// grammar - keep one per grammar for the life of the process.
const parsers = new Map<GrammarKey, Parser>();

function parserFor(choice: GrammarChoice): Parser {
  const existing = parsers.get(choice.key);
  if (existing) return existing;
  const parser = new Parser();
  parser.setLanguage(choice.grammar);
  parsers.set(choice.key, parser);
  return parser;
}

// node-tree-sitter hands the whole remaining source to the native side in
// one slice, so the read buffer must fit the entire file - the default
// (32 KiB) makes `parse()` throw "Invalid argument" on any larger input.
function bufferSizeFor(sourceText: string): number {
  return Math.max(32 * 1024, (sourceText.length + 1) * 2);
}

function hash(input: string): string {
  return createHash("sha256").update(input, "utf8").digest("hex").slice(0, 32);
}

/**
 * Ids must survive edits elsewhere in the file (they key incremental
 * diffing), so they are derived from identity - path, kind, qualified name -
 * and never from source positions, which shift on every insertion above a
 * symbol. `nativeKind` participates so that a getter and a setter sharing
 * one qualifiedName stay two distinct nodes.
 */
export function nodeIdFor(
  filePath: string,
  kind: NodeKind,
  qualifiedName: string,
  nativeKind?: string,
): string {
  return hash(`node ${filePath} ${kind} ${qualifiedName} ${nativeKind ?? ""}`);
}

/** Edges carry no position, so identity is the (from, kind, to) triple - repeated calls between the same pair collapse into one edge. */
export function edgeIdFor(fromId: string, kind: EdgeKind, toId: string): string {
  return hash(`edge ${fromId} ${kind} ${toId}`);
}

/** A tree-sitter parse tree. Re-exported so callers that hold one across
 * edits (incremental.ts) need no direct `tree-sitter` import. */
export type ParsedTree = Parser.Tree;

export interface IncrementalExtraction {
  /**
   * The tree this extraction walked. Callers keep it, apply a
   * `Tree.edit(...)` describing the next change, and hand it back as
   * `oldTree` so tree-sitter can reuse the untouched subtrees.
   */
  tree: ParsedTree;
  result: ExtractResult;
}

/**
 * Parses `sourceText` with the grammar matching `filePath`'s extension and
 * walks the tree into graph nodes/edges.
 *
 * `filePath` is used verbatim in node ids, so callers must pass it in a
 * stable form (project-relative is what the core uses) - the same file
 * addressed two different ways yields two disjoint sets of nodes.
 *
 * @throws {UnsupportedFileError} for extensions this plugin does not own.
 */
export function extractFile(
  filePath: string,
  sourceText: string,
  options?: ExtractOptions,
): ExtractResult {
  return extractIncremental(filePath, sourceText, undefined, options).result;
}

/**
 * `extractFile` plus the parse tree, and with the option to seed the parse
 * from the file's previous tree.
 *
 * `oldTree` must already carry the `Tree.edit(...)` describing how
 * `sourceText` differs from the text that tree was parsed from; tree-sitter
 * then re-lexes only the affected ranges and reuses every other subtree.
 * Omitting it is a plain full parse, which is exactly what `extractFile`
 * does - the two share this one code path so an incremental result can never
 * drift from a from-scratch one.
 *
 * The extraction itself is always a full walk of the resulting tree: node ids
 * are content-derived (`nodeIdFor`) and name resolution is file-global, so a
 * partial walk could not produce correct edges. The saving is in the parse,
 * and the caller (incremental.ts) turns the full result into a diff.
 *
 * @throws {UnsupportedFileError} for extensions this plugin does not own.
 */
export function extractIncremental(
  filePath: string,
  sourceText: string,
  oldTree?: ParsedTree,
  options?: ExtractOptions,
): IncrementalExtraction {
  const choice = grammarFor(filePath);
  if (!choice) throw new UnsupportedFileError(filePath);

  const tree = parserFor(choice).parse(sourceText, oldTree, {
    bufferSize: bufferSizeFor(sourceText),
  });

  const result = new Extractor(
    filePath,
    choice.language,
    tree.rootNode.hasError,
    options?.resolveSpecifier,
  ).run(tree.rootNode);
  return { tree, result };
}

// --- walk -------------------------------------------------------------

/**
 * Lexical context threaded through the walk. `prefix` builds qualifiedName;
 * `namespacePrefix` is the subset of it that a bare (unqualified) name can
 * resolve against - class members need `this.`/`Class.` and so are excluded.
 */
interface Scope {
  readonly prefix: string;
  readonly namespacePrefix: string;
  /**
   * The `from` of CALLS edges written here: the nearest enclosing Function
   * node, or - inside a function expression that has no node of its own (a
   * callback argument, an object-literal property value, an object-literal
   * method) - the nearest enclosing declared symbol, which may be a Variable
   * or a Type. See [`Extractor.callerFallback`]. Null only at module top
   * level, where a call is made by nothing and degrades to a usage edge.
   */
  readonly enclosingCallerId: string | null;
  /** Nearest enclosing symbol (or the File) - the `from` of REFERENCES edges. */
  readonly enclosingSymbolId: string;
  /** qualifiedName of the enclosing class/interface, for `this.m()`. */
  readonly enclosingTypeQName: string | null;
  /** Heritage names of the enclosing class, for `super.m()`. */
  readonly supertypeNames: readonly string[];
  /** Inside a function body: locals are deliberately not graph nodes. */
  readonly insideFunction: boolean;
}

type CallReceiver = "none" | "this" | "super" | "qualified";

interface PendingCall {
  readonly name: string;
  readonly receiver: CallReceiver;
  readonly scope: Scope;
  /** Receiver identifier for `qualified` calls (`Base.s()`, `NS.f()`). */
  readonly objectName?: string;
}

interface PendingReference {
  readonly name: string;
  readonly scope: Scope;
}

interface PendingSupertype {
  readonly fromId: string;
  readonly name: string;
  readonly scope: Scope;
}

/**
 * One local name introduced by an import whose specifier resolved to a file
 * in this project - i.e. a name this file uses but another file declares.
 */
interface ImportBinding {
  /** Project-relative POSIX path of the file the specifier named. */
  readonly targetPath: string;
  /** The name that file exports, i.e. the pre-alias one (or `default`). */
  readonly importedName: string;
  /** Where the local name is bound; the placeholder's source position. */
  readonly at: SyntaxNode;
}

/** Class/interface members: `.` for static (JSDoc convention), `#` for instance. */
type MemberSeparator = "." | "#";

function qualify(prefix: string, name: string, separator: MemberSeparator = "."): string {
  return prefix.length === 0 ? name : `${prefix}${separator}${name}`;
}

const CLASS_TYPES = new Set(["class_declaration", "abstract_class_declaration", "class"]);

/**
 * Every way of writing a function as an *expression*. A declaration position
 * (a `const`, a class field, `export default`) turns one of these into a
 * Function node; anywhere else it is an unnamed function whose body is still
 * walked, see the `visit` cases mirroring this set.
 */
const FUNCTION_VALUE_TYPES = new Set([
  "arrow_function",
  "function_expression",
  "generator_function",
]);

class Extractor {
  private readonly nodes = new Map<string, ExtractedNode>();
  private readonly edges = new Map<string, ExtractedEdge>();
  /** Declared symbols only - import placeholders must not shadow real names. */
  private readonly byName = new Map<string, ExtractedNode[]>();
  private readonly byQualifiedName = new Map<string, ExtractedNode>();
  /** Local name -> the imported symbol it stands for, for names this file
   * uses but does not declare. Consulted only after the declared-symbol
   * lookup has failed, so a local always shadows an import. */
  private readonly importBindings = new Map<string, ImportBinding>();
  private readonly pendingCalls: PendingCall[] = [];
  private readonly pendingReferences: PendingReference[] = [];
  private readonly pendingSupertypes: PendingSupertype[] = [];
  private readonly pendingExports: string[] = [];
  private fileNode!: ExtractedNode;

  constructor(
    private readonly filePath: string,
    private readonly language: string,
    private readonly hasSyntaxErrors: boolean,
    private readonly resolveSpecifier?: SpecifierResolver,
  ) {}

  run(root: SyntaxNode): ExtractResult {
    this.fileNode = this.addNode({
      kind: "File",
      name: path.basename(this.filePath),
      qualifiedName: this.filePath,
      at: root,
    });

    const scope: Scope = {
      prefix: "",
      namespacePrefix: "",
      enclosingCallerId: null,
      enclosingSymbolId: this.fileNode.id,
      enclosingTypeQName: null,
      supertypeNames: [],
      insideFunction: false,
    };
    this.visitChildren(root, scope);
    this.resolvePending();

    return {
      nodes: [...this.nodes.values()],
      edges: [...this.edges.values()],
      hasSyntaxErrors: this.hasSyntaxErrors,
    };
  }

  // --- node/edge construction ----------------------------------------

  private addNode(params: {
    kind: NodeKind;
    name: string;
    qualifiedName: string;
    at: SyntaxNode;
    nativeKind?: string;
    signature?: string;
    docComment?: string;
    exported?: boolean;
  }): ExtractedNode {
    const id = nodeIdFor(this.filePath, params.kind, params.qualifiedName, params.nativeKind);
    const existing = this.nodes.get(id);
    // Same id twice means the same symbol declared twice - overload
    // signatures, or a namespace/interface merged across statements. First
    // declaration wins; disambiguating them needs the semantic layer.
    if (existing) {
      if (params.exported) existing.exported = true;
      return existing;
    }

    const node: ExtractedNode = {
      id,
      kind: params.kind,
      name: params.name,
      qualifiedName: params.qualifiedName,
      filePath: this.filePath,
      startLine: params.at.startPosition.row,
      startCol: params.at.startPosition.column,
      endLine: params.at.endPosition.row,
      endCol: params.at.endPosition.column,
      exported: params.exported ?? false,
      language: this.language,
      hasSyntaxErrors: this.hasSyntaxErrors,
    };
    if (params.signature !== undefined) node.signature = params.signature;
    if (params.docComment !== undefined) node.docComment = params.docComment;
    if (params.nativeKind !== undefined) node.nativeKind = params.nativeKind;

    this.nodes.set(id, node);
    return node;
  }

  /** Adds a symbol declared in this file: indexed for name resolution, plus DEFINES/EXPORTS edges. */
  private declareSymbol(params: {
    kind: NodeKind;
    name: string;
    qualifiedName: string;
    at: SyntaxNode;
    nativeKind?: string;
    signature?: string;
    docComment?: string;
    exported?: boolean;
  }): ExtractedNode {
    const node = this.addNode(params);

    if (!this.byQualifiedName.has(node.qualifiedName)) {
      this.byQualifiedName.set(node.qualifiedName, node);
    }
    const sameName = this.byName.get(node.name);
    if (sameName) {
      if (!sameName.includes(node)) sameName.push(node);
    } else {
      this.byName.set(node.name, [node]);
    }

    this.addEdge(this.fileNode.id, "DEFINES", node.id);
    if (node.exported) this.addEdge(this.fileNode.id, "EXPORTS", node.id);
    return node;
  }

  private addEdge(fromId: string, kind: EdgeKind, toId: string): void {
    // Edges are FK-constrained in SQLite; never emit a dangling one.
    if (!this.nodes.has(fromId) || !this.nodes.has(toId)) return;
    const id = edgeIdFor(fromId, kind, toId);
    if (this.edges.has(id)) return;
    this.edges.set(id, { id, fromId, toId, kind, source: EDGE_SOURCE, resolved: false });
  }

  private markExported(node: ExtractedNode): void {
    node.exported = true;
    this.addEdge(this.fileNode.id, "EXPORTS", node.id);
  }

  // --- traversal ------------------------------------------------------

  private visitChildren(node: SyntaxNode, scope: Scope): void {
    for (const child of node.namedChildren) this.visit(child, scope);
  }

  private visitField(node: SyntaxNode, field: string, scope: Scope): void {
    const child = node.childForFieldName(field);
    if (child) this.visit(child, scope);
  }

  private visit(node: SyntaxNode, scope: Scope): void {
    switch (node.type) {
      case "comment":
        return;
      case "import_statement":
        this.handleImport(node);
        return;
      case "export_statement":
        this.handleExport(node, scope);
        return;
      case "class_declaration":
      case "abstract_class_declaration":
      case "class":
      case "interface_declaration":
      case "type_alias_declaration":
      case "enum_declaration":
      case "function_declaration":
      case "generator_function_declaration":
      case "lexical_declaration":
      case "variable_declaration":
      case "internal_module":
      case "module":
        this.visitDeclaration(node, scope, false, node);
        return;
      case "method_definition":
      case "method_signature":
      case "abstract_method_signature":
        this.handleMethod(node, scope);
        return;
      case "public_field_definition":
        this.handleField(node, scope);
        return;
      case "property_signature":
        // Interface properties are not graph symbols, but their type
        // annotations still reference ones.
        this.visitField(node, "type", scope);
        return;
      case "formal_parameters":
        this.visitParameters(node, scope);
        return;
      // A function expression in any position a declaration handler did not
      // already claim: a callback argument (`.map(x => ...)`,
      // `setTimeout(() => ...)`), an object-literal property value
      // (`register({ perform: () => ... })`), an IIFE, an element of an array
      // literal. It gets no node of its own - it has no name to hang one on,
      // and a positional one would not survive an edit above it - but its
      // body is a function body, so its locals stay out of the graph and its
      // calls are attributed to the symbol it was written into rather than
      // dropped. Mirrors `FUNCTION_VALUE_TYPES`.
      case "arrow_function":
      case "function_expression":
      case "generator_function":
        this.visitFunctionParts(node, scope);
        return;
      case "call_expression":
        this.handleCall(node, scope);
        return;
      case "new_expression":
        this.handleNew(node, scope);
        return;
      case "identifier":
      case "type_identifier":
      case "shorthand_property_identifier":
        if (!isBindingPosition(node)) this.pendingReferences.push({ name: node.text, scope });
        return;
      default:
        this.visitChildren(node, scope);
    }
  }

  private visitDeclaration(
    node: SyntaxNode,
    scope: Scope,
    exported: boolean,
    outer: SyntaxNode,
  ): void {
    switch (node.type) {
      case "class_declaration":
      case "abstract_class_declaration":
      case "class":
        this.handleClass(node, scope, exported, outer);
        return;
      case "interface_declaration":
        this.handleInterface(node, scope, exported, outer);
        return;
      case "type_alias_declaration":
        this.handleTypeAlias(node, scope, exported, outer);
        return;
      case "enum_declaration":
        this.handleEnum(node, scope, exported, outer);
        return;
      case "function_declaration":
      case "generator_function_declaration":
        this.handleFunctionDeclaration(node, scope, exported, outer);
        return;
      case "lexical_declaration":
      case "variable_declaration":
        this.handleVariableDeclaration(node, scope, exported, outer);
        return;
      case "internal_module":
      case "module":
        this.handleNamespace(node, scope, exported, outer);
        return;
      default:
        this.visit(node, scope);
    }
  }

  // --- imports / exports ----------------------------------------------

  /**
   * Module specifiers are the one name a syntax-only pass can resolve
   * without guessing (it is a literal, not an identifier), so import targets
   * get a placeholder `Module` node - enough to hang the IMPORTS edge on.
   * Identifier references never get placeholders; they are heuristics, and a
   * wrong node is worse than a missing edge.
   */
  private handleImport(node: SyntaxNode): void {
    const source = node.childForFieldName("source");
    if (!source) return;
    const targetPath = this.recordImport(source);
    if (targetPath !== null) this.recordImportBindings(node, targetPath);
  }

  /**
   * The local names this import statement binds, and the name each one stands
   * for in the target file. Only a specifier that resolved to a project file
   * is worth recording: an off-workspace package's symbols are not in this
   * index, so a placeholder for one could never be linked to anything (which
   * specifiers resolve at all is resolve.ts's call).
   *
   * `import * as NS from "./x"` is deliberately not recorded. The binding
   * names a whole module rather than one symbol, so `NS.f()` would have to
   * resolve `f` against the target file's exports - and this pass cannot tell
   * that apart from an ordinary property access on a value, which is the kind
   * of guess the extractor does not make.
   */
  private recordImportBindings(statement: SyntaxNode, targetPath: string): void {
    const clause = statement.children.find((child) => child.type === "import_clause");
    if (!clause) return; // `import "./side-effect"` binds nothing

    for (const child of clause.namedChildren) {
      // `import Default from "./x"` - the clause's bare identifier.
      if (child.type === "identifier") {
        this.bindImport(child, targetPath, "default");
        continue;
      }
      if (child.type !== "named_imports") continue; // namespace_import: see above
      for (const specifier of child.namedChildren) {
        if (specifier.type !== "import_specifier") continue;
        const name = specifier.childForFieldName("name");
        if (!name) continue;
        const alias = specifier.childForFieldName("alias");
        this.bindImport(alias ?? name, targetPath, name.text);
      }
    }
  }

  private bindImport(local: SyntaxNode, targetPath: string, importedName: string): void {
    // First binding wins, matching how a re-declared symbol is treated.
    if (this.importBindings.has(local.text)) return;
    this.importBindings.set(local.text, { targetPath, importedName, at: local });
  }

  /**
   * The placeholder carries the resolution rather than being replaced by it:
   * when `resolveSpecifier` recognises the specifier, the node's
   * `qualifiedName` becomes the project-relative path of the file it names
   * and its `nativeKind` says so, which is what lets core rewrite the IMPORTS
   * edge onto that file's real `File` node (`graph::imports`).
   *
   * Why the edge is not simply emitted against the target file's node id
   * here - the plugin *can* compute it, ids are a pure function of the path:
   *
   *  - Edges are foreign keys onto nodes, and the target file's `File` node
   *    may not be committed yet (bulk-index walk order) or ever (the target
   *    is gitignored, or excluded from the walk). Core is the only side that
   *    knows what is actually in the index, so it is the only side that can
   *    point an edge at a node without risking a dangling one - see the
   *    batching contract in `daemon::bulk_index`, which depends on a file's
   *    edges never leaving that file.
   *  - Resolution then degrades gracefully instead of silently: an import
   *    whose target is not indexed stays exactly what it is today, an
   *    unresolved placeholder, rather than an edge into nothing.
   *
   * `resolved` stays false on the edge for the same reason: it becomes true
   * only when core has actually repointed it at a real node.
   *
   * Returns the project-relative path the specifier resolved to, or `null`
   * when it named nothing in this project - which is also the test for
   * whether the names it binds are worth tracking (`recordImportBindings`).
   */
  private recordImport(source: SyntaxNode): string | null {
    const specifier = stringLiteralValue(source);
    if (specifier === undefined) return null;

    const resolvedPath = this.resolveSpecifier?.(specifier, this.filePath) ?? null;
    const target = this.addNode({
      kind: "Module",
      name: specifier,
      qualifiedName: resolvedPath ?? specifier,
      at: source,
      nativeKind: resolvedPath === null ? "external_module" : RESOLVED_MODULE_NATIVE_KIND,
    });
    this.addEdge(this.fileNode.id, "IMPORTS", target.id);
    return resolvedPath;
  }

  private handleExport(node: SyntaxNode, scope: Scope): void {
    const source = node.childForFieldName("source");
    // `export ... from "x"` also imports x, and the path that resolved to is
    // the far end of every re-export this statement records.
    const targetPath = source === null ? null : this.recordImport(source);

    const declaration = node.childForFieldName("declaration");
    if (declaration) {
      this.visitDeclaration(declaration, scope, true, node);
      return;
    }

    const value = node.childForFieldName("value");
    if (value) {
      this.handleDefaultExportValue(value, scope, node);
      return;
    }

    const clause = node.children.find((child) => child.type === "export_clause");
    if (clause) {
      for (const spec of clause.namedChildren) {
        if (spec.type !== "export_specifier") continue;
        const name = spec.childForFieldName("name");
        if (!name) continue;
        const alias = spec.childForFieldName("alias");
        // Names re-exported from another module are not local symbols, so
        // only a clause without `from` can mark anything exported here; with
        // one, the pair (published name, name over there) is recorded instead.
        if (source === null) {
          this.pendingExports.push(name.text);
        } else if (targetPath !== null) {
          this.recordReexport(alias ?? name, alias?.text ?? name.text, targetPath, name.text);
        }
      }
      return;
    }

    if (isWholeModuleReexport(node)) {
      if (targetPath !== null) {
        this.recordReexport(node, REEXPORT_ALL_NAME, targetPath, REEXPORT_ALL_NAME);
      }
      return;
    }

    // `export = foo` (TS export assignment).
    const identifier = node.namedChildren.find((child) => child.type === "identifier");
    if (identifier) this.pendingExports.push(identifier.text);
  }

  /**
   * Records that this file publishes `publishedName` without declaring it -
   * see [`REEXPORT_NATIVE_KIND`]. Nothing is added to the file's own name
   * lookup (`declareSymbol`'s job): a re-exported name is not a declaration
   * here, so it must not shadow one, and a usage of it in this file is an
   * ordinary import that already has its own placeholder.
   *
   * Two aliases of the same target symbol in one file (`export { a as b, a as
   * c } from "./y"`) collapse into one placeholder, since a node's identity is
   * its `qualifiedName` and both spell the same address. First one wins, as
   * everywhere else here; the loser degrades to an unlinked usage, never to a
   * wrong link.
   */
  private recordReexport(
    at: SyntaxNode,
    publishedName: string,
    targetPath: string,
    exportedName: string,
  ): void {
    this.addNode({
      kind: "Module",
      name: publishedName,
      qualifiedName: pendingSymbolQualifiedName(targetPath, exportedName),
      at,
      nativeKind: REEXPORT_NATIVE_KIND,
    });
  }

  private handleDefaultExportValue(value: SyntaxNode, scope: Scope, outer: SyntaxNode): void {
    if (CLASS_TYPES.has(value.type)) {
      this.handleClass(value, scope, true, outer);
      return;
    }
    if (FUNCTION_VALUE_TYPES.has(value.type)) {
      // `export default () => {}` - anonymous, so the export name is all we have.
      const fn = this.declareSymbol({
        kind: "Function",
        name: "default",
        qualifiedName: qualify(scope.prefix, "default"),
        at: value,
        nativeKind: value.type,
        signature: functionSignature("default", value),
        docComment: docCommentFor(outer),
        exported: true,
      });
      this.visitFunctionBody(value, scope, fn);
      return;
    }
    if (value.type === "identifier") {
      this.pendingExports.push(value.text);
      return;
    }
    this.visit(value, scope);
  }

  // --- declarations ----------------------------------------------------

  private handleClass(
    node: SyntaxNode,
    scope: Scope,
    exported: boolean,
    outer: SyntaxNode,
  ): void {
    const nameNode = node.childForFieldName("name");
    const name = nameNode?.text ?? "default"; // `export default class {}`
    const qualifiedName = qualify(scope.prefix, name);
    const body = node.childForFieldName("body");
    const heritage = node.children.find((child) => child.type === "class_heritage");
    const supertypeNames = heritage ? heritageNames(heritage) : [];

    if (scope.insideFunction) {
      // A class declared inside a function body is a local: no node, but its
      // methods still hold calls/references worth recording.
      const localScope: Scope = { ...scope, enclosingTypeQName: null, supertypeNames };
      if (body) this.visitChildren(body, localScope);
      return;
    }

    const classNode = this.declareSymbol({
      kind: "Type",
      name,
      qualifiedName,
      at: node,
      nativeKind: node.type === "abstract_class_declaration" ? "abstract_class" : "class",
      docComment: docCommentFor(outer),
      exported,
    });

    for (const supertype of supertypeNames) {
      this.pendingSupertypes.push({ fromId: classNode.id, name: supertype, scope });
    }

    const memberScope: Scope = {
      ...scope,
      prefix: qualifiedName,
      enclosingCallerId: null,
      enclosingSymbolId: classNode.id,
      enclosingTypeQName: qualifiedName,
      supertypeNames,
    };
    this.visitField(node, "type_parameters", memberScope);
    if (body) this.visitChildren(body, memberScope);
  }

  private handleInterface(
    node: SyntaxNode,
    scope: Scope,
    exported: boolean,
    outer: SyntaxNode,
  ): void {
    const nameNode = node.childForFieldName("name");
    if (!nameNode) return;
    const qualifiedName = qualify(scope.prefix, nameNode.text);
    const body = node.childForFieldName("body");

    if (scope.insideFunction) {
      if (body) this.visitChildren(body, scope);
      return;
    }

    const typeNode = this.declareSymbol({
      kind: "Type",
      name: nameNode.text,
      qualifiedName,
      at: node,
      nativeKind: "interface",
      docComment: docCommentFor(outer),
      exported,
    });

    const extendsClause = node.children.find((child) => child.type === "extends_type_clause");
    const supertypeNames = extendsClause ? heritageNames(extendsClause) : [];
    for (const supertype of supertypeNames) {
      this.pendingSupertypes.push({ fromId: typeNode.id, name: supertype, scope });
    }

    const memberScope: Scope = {
      ...scope,
      prefix: qualifiedName,
      enclosingCallerId: null,
      enclosingSymbolId: typeNode.id,
      enclosingTypeQName: qualifiedName,
      supertypeNames,
    };
    this.visitField(node, "type_parameters", memberScope);
    if (body) this.visitChildren(body, memberScope);
  }

  private handleTypeAlias(
    node: SyntaxNode,
    scope: Scope,
    exported: boolean,
    outer: SyntaxNode,
  ): void {
    const nameNode = node.childForFieldName("name");
    if (!nameNode) return;
    if (scope.insideFunction) {
      this.visitField(node, "value", scope);
      return;
    }

    const aliasNode = this.declareSymbol({
      kind: "Type",
      name: nameNode.text,
      qualifiedName: qualify(scope.prefix, nameNode.text),
      at: node,
      nativeKind: "type_alias",
      docComment: docCommentFor(outer),
      exported,
    });
    this.visitField(node, "value", { ...scope, enclosingSymbolId: aliasNode.id });
  }

  private handleEnum(
    node: SyntaxNode,
    scope: Scope,
    exported: boolean,
    outer: SyntaxNode,
  ): void {
    const nameNode = node.childForFieldName("name");
    if (!nameNode || scope.insideFunction) return;
    // Enum members are values inside the type, below symbol granularity.
    this.declareSymbol({
      kind: "Type",
      name: nameNode.text,
      qualifiedName: qualify(scope.prefix, nameNode.text),
      at: node,
      nativeKind: "enum",
      docComment: docCommentFor(outer),
      exported,
    });
  }

  private handleNamespace(
    node: SyntaxNode,
    scope: Scope,
    exported: boolean,
    outer: SyntaxNode,
  ): void {
    const nameNode = node.childForFieldName("name");
    const body = node.childForFieldName("body");
    if (!nameNode) return;
    const name = stringLiteralValue(nameNode) ?? nameNode.text;

    if (scope.insideFunction) {
      if (body) this.visitChildren(body, scope);
      return;
    }

    const qualifiedName = qualify(scope.prefix, name);
    const moduleNode = this.declareSymbol({
      kind: "Module",
      name,
      qualifiedName,
      at: node,
      nativeKind: node.type === "module" ? "ambient_module" : "namespace",
      docComment: docCommentFor(outer),
      exported,
    });

    if (!body) return;
    this.visitChildren(body, {
      ...scope,
      prefix: qualifiedName,
      namespacePrefix: qualifiedName,
      enclosingSymbolId: moduleNode.id,
      enclosingTypeQName: null,
      supertypeNames: [],
    });
  }

  private handleFunctionDeclaration(
    node: SyntaxNode,
    scope: Scope,
    exported: boolean,
    outer: SyntaxNode,
  ): void {
    const nameNode = node.childForFieldName("name");
    if (!nameNode || scope.insideFunction) {
      // Nested function declarations are locals: their calls are attributed
      // to the enclosing function instead of getting their own node.
      this.visitFunctionParts(node, scope);
      return;
    }

    const fn = this.declareSymbol({
      kind: "Function",
      name: nameNode.text,
      qualifiedName: qualify(scope.prefix, nameNode.text),
      at: node,
      nativeKind: node.type === "generator_function_declaration" ? "generator_function" : "function",
      signature: functionSignature(nameNode.text, node),
      docComment: docCommentFor(outer),
      exported,
    });
    this.visitFunctionBody(node, scope, fn);
  }

  private handleMethod(node: SyntaxNode, scope: Scope): void {
    const nameNode = node.childForFieldName("name");
    // Object-literal methods have no enclosing type to qualify against.
    if (!nameNode || scope.enclosingTypeQName === null || scope.insideFunction) {
      this.visitFunctionParts(node, scope);
      return;
    }

    const name = nameNode.text;
    const isStatic = node.children.some((child) => child.type === "static");
    const method = this.declareSymbol({
      kind: "Function",
      name,
      qualifiedName: qualify(scope.prefix, name, isStatic ? "." : "#"),
      at: node,
      nativeKind: methodNativeKind(node, name, isStatic),
      signature: functionSignature(name, node),
      docComment: docCommentFor(node),
    });
    this.visitFunctionBody(node, scope, method);
  }

  private handleField(node: SyntaxNode, scope: Scope): void {
    const nameNode = node.childForFieldName("name");
    const value = node.childForFieldName("value");
    // Class data properties are members below symbol granularity; only
    // function-valued fields (the arrow-method idiom) become Function nodes.
    if (
      !nameNode ||
      !value ||
      !FUNCTION_VALUE_TYPES.has(value.type) ||
      scope.enclosingTypeQName === null ||
      scope.insideFunction
    ) {
      this.visitField(node, "type", scope);
      if (value) this.visit(value, scope);
      return;
    }

    const isStatic = node.children.some((child) => child.type === "static");
    const fn = this.declareSymbol({
      kind: "Function",
      name: nameNode.text,
      qualifiedName: qualify(scope.prefix, nameNode.text, isStatic ? "." : "#"),
      at: node,
      nativeKind: value.type,
      signature: functionSignature(nameNode.text, value),
      docComment: docCommentFor(node),
    });
    this.visitFunctionBody(value, scope, fn);
  }

  private handleVariableDeclaration(
    node: SyntaxNode,
    scope: Scope,
    exported: boolean,
    outer: SyntaxNode,
  ): void {
    const keyword = node.children.find(
      (child) => child.type === "const" || child.type === "let" || child.type === "var",
    );

    for (const declarator of node.namedChildren) {
      if (declarator.type !== "variable_declarator") continue;
      const nameNode = declarator.childForFieldName("name");
      const value = declarator.childForFieldName("value");

      // Locals and destructuring patterns produce no node - but their
      // initializers can still call or reference file-level symbols.
      if (scope.insideFunction || !nameNode || nameNode.type !== "identifier") {
        this.visitField(declarator, "type", scope);
        if (value) this.visit(value, scope);
        continue;
      }

      if (value && FUNCTION_VALUE_TYPES.has(value.type)) {
        const fn = this.declareSymbol({
          kind: "Function",
          name: nameNode.text,
          qualifiedName: qualify(scope.prefix, nameNode.text),
          at: declarator,
          nativeKind: value.type,
          signature: functionSignature(nameNode.text, value),
          docComment: docCommentFor(outer),
          exported,
        });
        this.visitFunctionBody(value, scope, fn);
        continue;
      }

      const variable = this.declareSymbol({
        kind: "Variable",
        name: nameNode.text,
        qualifiedName: qualify(scope.prefix, nameNode.text),
        at: declarator,
        nativeKind: keyword?.type ?? "var",
        docComment: docCommentFor(outer),
        exported,
      });
      const valueScope: Scope = { ...scope, enclosingSymbolId: variable.id };
      this.visitField(declarator, "type", valueScope);
      if (value) this.visit(value, valueScope);
    }
  }

  /** Walks a function's parameters and body with `fn` as the enclosing symbol. */
  private visitFunctionBody(node: SyntaxNode, scope: Scope, fn: ExtractedNode): void {
    this.visitFunctionParts(node, {
      ...scope,
      prefix: fn.qualifiedName,
      enclosingCallerId: fn.id,
      enclosingSymbolId: fn.id,
      insideFunction: true,
    });
  }

  /**
   * Walks the parts of any function - one with a node of its own or an
   * unnamed expression - as a function body: its locals stay out of the
   * graph, and a call written in it is attributed to `enclosingCallerId`,
   * which [`callerFallback`] supplies when the function has no node.
   */
  private visitFunctionParts(node: SyntaxNode, scope: Scope): void {
    const bodyScope: Scope = {
      ...scope,
      insideFunction: true,
      enclosingCallerId: scope.enclosingCallerId ?? this.callerFallback(scope),
    };
    this.visitField(node, "type_parameters", bodyScope);
    const parameters = node.childForFieldName("parameters");
    if (parameters) this.visitParameters(parameters, bodyScope);
    this.visitField(node, "return_type", bodyScope);
    this.visitField(node, "body", bodyScope);
  }

  /**
   * Who a call made inside an unnamed function is attributed to, i.e. the
   * `from` of its CALLS edge when the function itself is not a node: the
   * nearest enclosing declared symbol - the `const` the callback was written
   * into, the class whose field holds it - which is the symbol a reader would
   * name as the caller, and the only stable identity available (an anonymous
   * function has no name, and a positional one would break the rule that node
   * ids survive edits elsewhere in the file, see [`nodeIdFor`]).
   *
   * Widening the `from` of a CALLS edge past `Function` is deliberate: the
   * alternative - keeping the edge kind pure and dropping it - is what made
   * `find_callers` silently omit every caller whose call site sits in a
   * callback or an object-literal property value, which in idiomatic JS/TS is
   * most of them. Nothing downstream constrains that end: core's linking pass
   * only ever demands a kind of the *target* (`graph::symbol_links`), and
   * `find_callers` reports whatever node it finds there, kind included.
   *
   * The File itself is the one symbol that does not count: a call written at
   * module top level is made by nothing, so it keeps degrading to a usage
   * edge (see [`resolveCall`]).
   */
  private callerFallback(scope: Scope): string | null {
    return scope.enclosingSymbolId === this.fileNode.id ? null : scope.enclosingSymbolId;
  }

  /** Parameter names are bindings, not references; only their types and default values are. */
  private visitParameters(node: SyntaxNode, scope: Scope): void {
    for (const parameter of node.namedChildren) {
      switch (parameter.type) {
        case "required_parameter":
        case "optional_parameter":
          this.visitField(parameter, "type", scope);
          this.visitField(parameter, "value", scope);
          break;
        case "assignment_pattern":
          this.visitField(parameter, "right", scope);
          break;
        default:
          break;
      }
    }
  }

  // --- calls -----------------------------------------------------------

  private handleCall(node: SyntaxNode, scope: Scope): void {
    const callee = node.childForFieldName("function");
    const args = node.childForFieldName("arguments");

    if (callee) {
      if (callee.type === "identifier" && callee.text === "require") {
        if (!this.recordCallImport(node)) {
          this.pendingCalls.push({ name: callee.text, receiver: "none", scope });
        }
      } else if (callee.type === "import") {
        this.recordCallImport(node);
      } else if (callee.type === "identifier") {
        this.pendingCalls.push({ name: callee.text, receiver: "none", scope });
      } else if (callee.type === "super") {
        this.pendingCalls.push({ name: "constructor", receiver: "super", scope });
      } else if (callee.type === "member_expression") {
        const object = callee.childForFieldName("object");
        const property = callee.childForFieldName("property");
        if (object && property && (object.type === "this" || object.type === "super")) {
          this.pendingCalls.push({
            name: property.text,
            receiver: object.type === "this" ? "this" : "super",
            scope,
          });
        } else if (object && property && object.type === "identifier") {
          // `Base.s()` / `NS.f()`: a bare identifier receiver can name a
          // type or namespace declared in this file, which pins the target
          // down exactly. Anything else (a variable, a longer chain) needs a
          // checker to type, so it stays a plain reference.
          this.pendingCalls.push({
            name: property.text,
            receiver: "qualified",
            objectName: object.text,
            scope,
          });
        } else if (object) {
          this.visit(object, scope);
        }
      } else {
        this.visit(callee, scope);
      }
    }

    if (args) this.visitChildren(args, scope);
  }

  /** `new Foo()` is a call of `Foo`'s constructor when the class is in this file. */
  private handleNew(node: SyntaxNode, scope: Scope): void {
    const constructor = node.childForFieldName("constructor");
    const args = node.childForFieldName("arguments");
    if (constructor?.type === "identifier") {
      this.pendingCalls.push({
        name: "constructor",
        receiver: "qualified",
        objectName: constructor.text,
        scope,
      });
    } else if (constructor) {
      this.visit(constructor, scope);
    }
    if (args) this.visitChildren(args, scope);
  }

  /** `require("x")` / `import("x")` with a literal specifier, same as a static import. */
  private recordCallImport(node: SyntaxNode): boolean {
    const args = node.childForFieldName("arguments");
    const first = args?.namedChildren[0];
    if (!first || stringLiteralValue(first) === undefined) return false;
    this.recordImport(first);
    return true;
  }

  // --- name resolution --------------------------------------------------

  private resolvePending(): void {
    for (const name of this.pendingExports) {
      const target = this.lookupByName(name, "");
      if (target) this.markExported(target);
    }
    for (const supertype of this.pendingSupertypes) {
      const target =
        this.lookupType(supertype.name, supertype.scope) ?? this.importedSymbol(supertype.name);
      // Direction: subtype -> supertype, so `find_implementations` on a type
      // is its inbound SUPERTYPE_OF edges (docs/architecture/g-mesh-v1.md).
      if (target) this.addEdge(supertype.fromId, "SUPERTYPE_OF", target.id);
    }
    for (const call of this.pendingCalls) this.resolveCall(call);
    for (const reference of this.pendingReferences) {
      this.resolveReference(reference.name, reference.scope);
    }
  }

  private resolveCall(call: PendingCall): void {
    const target = this.lookupCallTarget(call);

    if (target?.kind === "Function" && call.scope.enclosingCallerId !== null) {
      this.addEdge(call.scope.enclosingCallerId, "CALLS", target.id);
      return;
    }
    // What a CALLS edge points *at* is always a Function, and it is always
    // made from inside some function ([`callerFallback`] names which symbol
    // that is when the function has no node). A call at module top level is
    // neither, so it degrades to a usage edge from the File node.
    if (target) {
      this.addUsage(target, call.scope);
      return;
    }
    // `this.x()` / `super.x()` resolve against a known type or not at all;
    // for the others the name (or the receiver) can still be a usage of a
    // file-level symbol, or of one this file imports.
    if (call.receiver === "none") {
      const imported = this.importedSymbol(call.name);
      if (imported === undefined) {
        this.resolveReference(call.name, call.scope);
      } else if (call.scope.enclosingCallerId !== null) {
        this.addEdge(call.scope.enclosingCallerId, "CALLS", imported.id);
      } else {
        // Same rule as above: a call written at module top level is made by
        // nothing, so it degrades to a usage edge.
        this.addUsage(imported, call.scope);
      }
    }
    if (call.receiver === "qualified" && call.objectName !== undefined) {
      // `Imported.member()`: which member is a question for the semantic
      // layer, but the receiver itself is a usage of the imported symbol.
      this.resolveReference(call.objectName, call.scope);
    }
  }

  private lookupCallTarget(call: PendingCall): ExtractedNode | undefined {
    switch (call.receiver) {
      case "this": {
        const owner = call.scope.enclosingTypeQName;
        return owner === null ? undefined : this.lookupMember(owner, call.name);
      }
      case "super": {
        for (const supertypeName of call.scope.supertypeNames) {
          const supertype = this.lookupType(supertypeName, call.scope);
          if (!supertype) continue;
          const member = this.lookupMember(supertype.qualifiedName, call.name);
          if (member) return member;
        }
        return undefined;
      }
      case "qualified": {
        if (call.objectName === undefined) return undefined;
        const member = this.byQualifiedName.get(`${call.objectName}.${call.name}`);
        if (member) return member;
        const owner = this.lookupByName(call.objectName, call.scope.namespacePrefix);
        return owner === undefined || owner.kind === "Variable"
          ? undefined
          : this.lookupMember(owner.qualifiedName, call.name);
      }
      case "none":
        return this.lookupByName(call.name, call.scope.namespacePrefix, "Function");
    }
  }

  private resolveReference(name: string, scope: Scope): void {
    const target = this.lookupByName(name, scope.namespacePrefix) ?? this.importedSymbol(name);
    if (target) this.addUsage(target, scope);
  }

  /**
   * The placeholder standing in for `name` when this file imports it from
   * another project file, created on first use so an import nothing uses
   * costs nothing. Only ever consulted after the declared-symbol lookup has
   * come up empty, which is what keeps a local shadowing an import from being
   * mistaken for it.
   *
   * The edge core will eventually repoint stays `resolved: false` until it
   * does, exactly as with an import placeholder: whether the target file
   * really exports this name is a fact about the index, which only core has.
   */
  private importedSymbol(name: string): ExtractedNode | undefined {
    const binding = this.importBindings.get(name);
    if (binding === undefined) return undefined;
    return this.addNode({
      kind: "Module",
      name: binding.importedName,
      qualifiedName: pendingSymbolQualifiedName(binding.targetPath, binding.importedName),
      at: binding.at,
      nativeKind: PENDING_SYMBOL_NATIVE_KIND,
    });
  }

  private addUsage(target: ExtractedNode, scope: Scope): void {
    const from = scope.enclosingSymbolId;
    if (from === target.id) return; // self-reference carries no information
    if (this.edges.has(edgeIdFor(from, "CALLS", target.id))) return; // already a call
    this.addEdge(from, "REFERENCES", target.id);
  }

  private lookupMember(typeQualifiedName: string, name: string): ExtractedNode | undefined {
    return (
      this.byQualifiedName.get(`${typeQualifiedName}#${name}`) ??
      this.byQualifiedName.get(`${typeQualifiedName}.${name}`)
    );
  }

  private lookupType(name: string, scope: Scope): ExtractedNode | undefined {
    return this.lookupByName(name, scope.namespacePrefix, "Type");
  }

  /**
   * Name-based lookup: an exact qualifiedName match walking out through the
   * enclosing namespaces first, then a unique file-level symbol of that
   * name. Ambiguity (several same-named candidates) yields nothing - a
   * missing edge beats a wrong one, and disambiguating overloads or
   * shadowing needs the semantic layer.
   */
  private lookupByName(name: string, namespacePrefix: string, kind?: NodeKind): ExtractedNode | undefined {
    // Innermost namespace outwards, ending at the module root ("").
    for (let prefix = namespacePrefix; ; prefix = prefix.slice(0, Math.max(0, prefix.lastIndexOf(".")))) {
      const candidate = this.byQualifiedName.get(qualify(prefix, name));
      if (candidate && (kind === undefined || candidate.kind === kind)) return candidate;
      if (prefix.length === 0) break;
    }

    const candidates = (this.byName.get(name) ?? []).filter(
      (node) => kind === undefined || node.kind === kind,
    );
    return candidates.length === 1 ? candidates[0] : undefined;
  }
}

// --- syntax helpers -----------------------------------------------------

/**
 * `export * from "./y"`, which republishes every name `./y` exports.
 *
 * Told apart from `export * as NS from "./y"` by where the `*` sits: a
 * namespace re-export tucks it inside a `namespace_export` child and publishes
 * a single name bound to the whole module, which is the same thing
 * `import * as NS` binds and is left alone for the same reason (see
 * `recordImportBindings`) - resolving `NS.f` needs to tell a module's export
 * from an ordinary property access, which this pass cannot do.
 */
function isWholeModuleReexport(statement: SyntaxNode): boolean {
  return statement.children.some((child) => child.type === "*");
}

/** Identifiers introducing a binding are declarations, not usages. */
function isBindingPosition(node: SyntaxNode): boolean {
  const parent = node.parent;
  if (!parent) return false;
  // A JSX element's `name` is a usage of the component, not a binding.
  if (parent.type.startsWith("jsx_")) return false;
  for (const field of ["name", "pattern", "alias"]) {
    if (parent.childForFieldName(field)?.id === node.id) return true;
  }
  if (parent.type === "for_in_statement" && parent.childForFieldName("left")?.id === node.id) {
    return true;
  }
  return false;
}

/** Text of a string literal node, without quotes; undefined if it isn't one. */
function stringLiteralValue(node: SyntaxNode): string | undefined {
  if (node.type !== "string" && node.type !== "template_string") return undefined;
  const fragment = node.namedChildren.find((child) => child.type === "string_fragment");
  if (fragment) return fragment.text;
  // Empty literal (`""`) has no fragment child.
  return node.text.length >= 2 ? node.text.slice(1, -1) : undefined;
}

/**
 * Supertype names from a `class_heritage` / `extends_type_clause`. Generic
 * arguments are dropped (`Base<T>` -> `Base`); qualified names are kept
 * whole (`NS.Base`) so they can match a namespaced declaration.
 */
function heritageNames(clause: SyntaxNode): string[] {
  const names: string[] = [];
  const visit = (node: SyntaxNode): void => {
    switch (node.type) {
      case "extends_clause":
      case "implements_clause":
        for (const child of node.namedChildren) visit(child);
        return;
      case "generic_type": {
        const name = node.childForFieldName("name");
        if (name) names.push(name.text);
        return;
      }
      case "identifier":
      case "type_identifier":
      case "nested_type_identifier":
      case "member_expression":
        names.push(node.text);
        return;
      default:
        return;
    }
  };
  for (const child of clause.namedChildren) visit(child);
  return names;
}

function methodNativeKind(node: SyntaxNode, name: string, isStatic: boolean): string {
  if (node.type === "abstract_method_signature") return "abstract_method";
  if (node.type === "method_signature") return "method_signature";
  if (node.children.some((child) => child.type === "get")) return "getter";
  if (node.children.some((child) => child.type === "set")) return "setter";
  if (!isStatic && name === "constructor") return "constructor";
  return "method";
}

/**
 * `name<T>(params): Return` rebuilt from the declaration's own fields, so it
 * stays whatever the source said - no type inference is involved.
 */
function functionSignature(name: string, node: SyntaxNode): string {
  const isAsync = node.children.some((child) => child.type === "async");
  const typeParameters = node.childForFieldName("type_parameters")?.text ?? "";
  // Arrow functions with one unparenthesized parameter use `parameter`.
  const parameters =
    node.childForFieldName("parameters")?.text ??
    (node.childForFieldName("parameter") ? `(${node.childForFieldName("parameter")!.text})` : "()");
  const returnType = node.childForFieldName("return_type")?.text ?? "";
  const signature = `${isAsync ? "async " : ""}${name}${typeParameters}${parameters}${returnType}`;
  return signature.replace(/\s+/g, " ").trim();
}

/**
 * JSDoc block immediately preceding a declaration, with the comment
 * delimiters and `*` gutter stripped so the text is usable as embedding
 * input. Line comments are ignored - only `/** ... *\/` counts as a doc.
 */
function docCommentFor(node: SyntaxNode): string | undefined {
  const previous = node.previousNamedSibling;
  if (!previous || previous.type !== "comment") return undefined;
  const text = previous.text;
  if (!text.startsWith("/**")) return undefined;

  const body = text
    .slice(3, text.endsWith("*/") ? -2 : undefined)
    .split("\n")
    .map((line) => line.replace(/^\s*\*+ ?/, "").trim())
    .join("\n")
    .trim();
  return body.length > 0 ? body : undefined;
}
