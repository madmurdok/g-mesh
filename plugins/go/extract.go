package main

// The structural extractor: one Go file in, one file's worth of graph out
// (GM-280). `go/parser` only - no toolchain, no type information. The
// go/types pass that answers what this one deliberately refuses to guess is
// GM-281; see "What this tier deliberately does not answer" below.
//
// Everything in this file is a pure function of (workspace, path, bytes):
// nothing here touches the filesystem, so bulk indexing and an incremental
// `fileChanged` run identical code and therefore produce identical ids and
// ranges - which is what `id-stability.incremental-matches-bulk` and
// `id-stability.declaration-edit-applies` (core/src/cli/plugin_check/checks.rs)
// actually check.
//
// # The three shapes a use can have, and why they are different
//
// This is the one judgement the whole tier rests on, and the one GM-280 was
// called "high complexity" for. Without a type checker, a bare name at a use
// site is one of four things, and only one of them may become a
// package-scoped address:
//
//  1. **A local** - a parameter, a `:=` variable, a named result, a
//     receiver, a type parameter, a local `var`/`const`/`type`, a label.
//     Emitting a package-scoped placeholder for one produces an edge onto a
//     package symbol the call site cannot even see. scope.go is what rules
//     this out, and its doc comment has the full reasoning.
//  2. **A declaration of this very file** - a direct, `resolved: true` edge.
//     Nothing is left for core to confirm: the target is a node in the same
//     diff.
//  3. **A declaration of a sibling file of the same package** - a
//     `name`-keyed placeholder scoped to this file's own container. The
//     plugin genuinely cannot tell this case from case 4 below, which is
//     exactly why the address is a *container* and not a file: core looks
//     the name up among every member of the package, wherever it lives
//     (core/src/graph/symbol_links.rs).
//  4. **A builtin** (`len`, `error`, `nil`, ...) - nothing. Checked *after*
//     case 2, because a package may legally shadow a universe name.
//
// A qualified use `pkg.F()` is a fifth case and an easy one: the placeholder
// is scoped to the *imported* container instead of the own one. A use
// `x.M()` through a value receiver is the sixth, and it is the one this tier
// answers with nothing at all - see open_sites.go.
//
// # What this tier deliberately does not answer
//
// Each of these is a *missing* edge, never a wrong one, on the project's
// standing rule (core/src/graph/symbol_links.rs, "What stays unresolved"):
//
//   - **Receiver calls** `x.M()`: which type `x` has needs a type checker.
//     Recorded as an open site for GM-281, never guessed.
//   - **Method promotion through struct embedding**: `s.Close()` where
//     `Close` is promoted from an embedded field is not modelled
//     structurally at all - working out a method set is `go/types`' job and
//     belongs to GM-281. This tier does not even emit the *possibility*: an
//     embedded field is walked as an ordinary type reference and nothing
//     more.
//   - **Interface satisfaction** (`SUPERTYPE_OF`): Go's interfaces are
//     structural, so nothing in a type's syntax says which interfaces it
//     implements. `types.Implements` answers it in GM-281. That includes the
//     `var _ I = (*T)(nil)` idiom, which *does* state it syntactically but
//     is one spelling among several and is left to the pass that gets them
//     all.
//   - **Bare names in a file with a dot import**: see collectImports.
//   - **Struct field and map literal keys**: a bare `Key:` in a composite
//     literal is a field name in a struct literal and a constant in a map
//     literal, and the two are indistinguishable without types (uses.go).
//   - **Build-constrained files** (`//go:build`, `_windows.go`): indexed
//     structurally like any other file, every alternative of them. GM-281's
//     go/types pass only ever type-checks the host GOOS/GOARCH, so files
//     excluded by the host's constraints keep their structural graph and
//     receive no semantic upgrade - the design doc's "Go build constraints"
//     failure mode, documented rather than hidden.
//
// # Generated files
//
// A file carrying the `// Code generated ... DO NOT EDIT.` line is indexed
// exactly like a hand-written one. It is real, compiled, callable code -
// dropping it would make `find_callers` on anything a generated file calls
// silently incomplete, which is the failure mode this project treats as
// worst. Whether it is *interesting* is a question for a query, not for the
// index, and the walk already has the two exclusion mechanisms that matter
// (`.gitignore`, which is where a repository states that generated output is
// not source, and `exclude_dirs`).

import (
	"bytes"
	"go/ast"
	"go/parser"
	"go/printer"
	"go/token"
	"path"
	"strconv"
	"strings"
)

const (
	// engineName labels every edge this tier emits. The *tier* is the closed
	// set core branches on (`syntactic`); the engine is the free-text label
	// that says which of this plugin's two passes produced the edge, so a
	// GM-281 upgrade is distinguishable from what it replaced.
	engineName          = "go-parser"
	sourceTierSyntactic = "syntactic"

	// Core's placeholder kinds (core/src/protocol/conformance.rs's
	// PLACEHOLDER_NATIVE_KINDS). A node carrying one of these *must* carry a
	// `target`, which the kit's shape check enforces.
	pendingSymbolNativeKind  = "pending_symbol"
	resolvedModuleNativeKind = "resolved_module"
	// externalModuleNativeKind is not one of core's placeholder kinds: core
	// stores such a node as an ordinary `Module` row and never tries to link
	// it, which is exactly right for an import of something outside this
	// project (the standard library, a dependency). It needs no target, and
	// the conformance kit still treats an edge onto it as a placeholder edge
	// for the same-file rule - nothing will ever confirm it, so
	// `resolved: true` would be a false claim. Mirrors the TS plugin's own
	// nativeKind for the same situation.
	externalModuleNativeKind = "external_module"

	// reexportAllName is the key `graph::imports` never reads but the wire
	// shape requires on every placeholder: an import addresses a whole
	// container, so there is no "which name" to state. Core's own legacy
	// derivation (`protocol::types::derive_legacy_target`) puts "*" here for
	// precisely this reason, so this plugin says the same thing.
	reexportAllName = "*"

	nodeKindFile     = "File"
	nodeKindModule   = "Module"
	nodeKindType     = "Type"
	nodeKindFunction = "Function"
	nodeKindVariable = "Variable"

	edgeKindDefines    = "DEFINES"
	edgeKindExports    = "EXPORTS"
	edgeKindImports    = "IMPORTS"
	edgeKindCalls      = "CALLS"
	edgeKindReferences = "REFERENCES"

	// A signature or doc comment is a display string, not data anything
	// keys on, and a generated Go file can carry a several-kilobyte struct
	// literal as one declaration. Capped so one pathological declaration
	// cannot dominate a diff.
	maxSignatureLength  = 400
	maxDocCommentLength = 1000
)

// fileGraph is one file's whole contribution: the nodes and edges that go to
// core, plus the open sites that never do.
type fileGraph struct {
	nodes []wireNode
	edges []wireEdge
	// Use sites this tier could not resolve - receiver calls and field
	// accesses through a value (`x.M()`). Kept in this process's memory and
	// handed to the semantic pass; never sent to core, which has no concept
	// of one. The design doc's `FileGraph.open_sites`, and GM-281's input.
	openSites []openSite
}

// extractFile parses one Go file and returns its graph.
//
// `parser.ParseComments | parser.AllErrors` is the mode the design doc
// specifies, and both halves are load-bearing: `ParseComments` is what makes
// doc comments available at all, and `AllErrors` keeps the parser going past
// the first syntax error so a broken file still yields whatever declarations
// it does have - a *partial* graph plus `hasSyntaxErrors`, rather than
// nothing. An editor's file is mid-edit and therefore broken most of the
// time it is looked at, so "partial" is the normal case, not the exceptional
// one.
func extractFile(ws *workspace, relPath string, content []byte) fileGraph {
	fset := token.NewFileSet()
	astFile, err := parser.ParseFile(fset, relPath, content, parser.ParseComments|parser.AllErrors)

	fileNode := computeFileNode(relPath, content)
	fileNode.HasSyntaxErrors = err != nil

	if astFile == nil {
		// Only reachable when the parser could not produce even an empty
		// file (it returns a partial *ast.File for every input it can read).
		// The File node alone is still an honest answer: the file exists.
		return fileGraph{nodes: []wireNode{fileNode}}
	}

	e := &extractor{
		ws:              ws,
		relPath:         relPath,
		fset:            fset,
		fileNodeID:      fileNode.ID,
		hasSyntaxErrors: fileNode.HasSyntaxErrors,
		container:       containerKeyFor(ws, relPath, packageNameOf(astFile)),
		nodeSeen:        map[string]bool{},
		nodeKind:        map[string]string{},
		placeholder:     map[string]bool{},
		edgeSeen:        map[string]bool{},
		fileDecls:       map[string]string{},
		imports:         map[string]string{},
	}
	e.addNode(fileNode)

	e.collectImports(astFile)
	e.declareAll(astFile)
	e.useAll(astFile)

	return fileGraph{nodes: e.nodes, edges: e.edges, openSites: e.openSites}
}

// extractor is one file's extraction state. Not reused across files: every
// map in it is keyed by something only meaningful within one file.
type extractor struct {
	ws              *workspace
	relPath         string
	fset            *token.FileSet
	fileNodeID      string
	hasSyntaxErrors bool
	// container is this file's package as a container key: the directory's
	// import path, or that plus "_test" for an external test package.
	container string

	nodes    []wireNode
	nodeSeen map[string]bool
	// nodeKind is what a node id is, for deciding whether a call site may
	// produce a CALLS edge (core's linker only ever lands CALLS on a
	// Function, so emitting one onto a Type would be an edge core drops).
	nodeKind    map[string]string
	placeholder map[string]bool

	edges    []wireEdge
	edgeSeen map[string]bool

	// fileDecls maps a *package-scope* name declared in this file to its
	// node id. Methods and interface methods are deliberately absent: `M` is
	// not a package-scope name, only `T.M` is a qualifiedName.
	fileDecls map[string]string

	// imports maps the name a file binds an import to (the package name, or
	// an alias) to the import path. Blank and dot imports bind nothing and
	// are absent.
	imports map[string]string
	// hasDotImport suppresses every own-container placeholder in this file -
	// see collectImports.
	hasDotImport bool

	openSites []openSite
	// initOrdinal counts `func init()` declarations within this file, so
	// several of them get distinct node ids - see declareFunc.
	initOrdinal int
	// usedInits is the use pass's own counter over the same declarations,
	// walking them in the same source order so it recovers the id
	// declareFunc gave each one (uses.go's nextInitID).
	usedInits int
}

// containerKeyFor is this plugin's whole container rule: a declaration's
// container is its directory's Go import path, except in an external test
// package, which gets its own.
//
// # `package x_test`
//
// Go's one case of two packages in one directory. They are genuinely
// separate packages - `x_test` may import `x`, and cannot see its unexported
// names - so they are separate containers, and `<import path>_test` is the
// key the design doc specifies. The test is the *package clause*, not the
// file name: Go reserves the `_test` package-name suffix for external test
// packages (go/build rejects a non-test file declaring one), so the suffix
// is exactly the signal, and reading it off the clause also keeps an
// external test helper that is not itself named `*_test.go` in the right
// container.
//
// An *internal* test file (`package x` in `x_test.go`) is not this case: it
// is an ordinary member of `x`, gets `x`'s container, and can therefore link
// to the package's unexported symbols - which is precisely what it is for.
//
// # `package main`
//
// Not special. `main` is an ordinary package with an ordinary import path
// (`github.com/example/app/cmd`), which is what `go list` calls it too, so
// two different `main` packages in one repository get two different
// containers rather than colliding on the name "main". Nothing imports a
// main package, so its container simply never appears as an import target.
//
// # Two packages in one directory that are *not* a test pair
//
// Illegal Go, but reachable on disk (a stale file, a directory of loose
// build-tagged snippets). Both land in the same container key, which is the
// same answer `go list` gives before it reports the error. The consequence
// is bounded: a name declared in both becomes ambiguous and core refuses to
// link it, which is the right failure.
func containerKeyFor(ws *workspace, relPath, packageName string) string {
	base := ws.importPath(normalizeDir(path.Dir(relPath)))
	if strings.HasSuffix(packageName, "_test") {
		return base + "_test"
	}
	return base
}

func packageNameOf(file *ast.File) string {
	if file.Name == nil {
		return ""
	}
	return file.Name.Name
}

// --- nodes and edges ----------------------------------------------------

func (e *extractor) addNode(node wireNode) string {
	node.HasSyntaxErrors = e.hasSyntaxErrors
	if e.nodeSeen[node.ID] {
		return node.ID
	}
	e.nodeSeen[node.ID] = true
	e.nodeKind[node.ID] = node.Kind
	e.nodes = append(e.nodes, node)
	return node.ID
}

// addEdge records one edge, deduplicated by id.
//
// `resolved` is decided by what the edge points at, never by who made it: a
// target that is a real node of this very file is confirmed already, and a
// target that is a placeholder is a claim only core can settle. That is the
// same rule the TS plugin's addEdge states, and it is what the conformance
// kit's `same-file-rule` check enforces on both plugins.
func (e *extractor) addEdge(fromID, kind, toID string) {
	if fromID == "" || toID == "" || !e.nodeSeen[fromID] || !e.nodeSeen[toID] {
		return
	}
	id := edgeIDFor(fromID, kind, toID, nil)
	if e.edgeSeen[id] {
		return
	}
	e.edgeSeen[id] = true
	e.edges = append(e.edges, wireEdge{
		ID:       id,
		FromID:   fromID,
		ToID:     toID,
		Kind:     kind,
		Source:   sourceTierSyntactic,
		Engine:   engineName,
		Resolved: !e.placeholder[toID],
	})
}

// declareSymbol adds a declaration node plus its ownership edges. DEFINES
// and EXPORTS always run from the File node: core writes a container's own
// DEFINES edges itself, and the kit's ownership.defines-exports-from-file
// check enforces that a plugin's never do.
func (e *extractor) declareSymbol(node wireNode) string {
	node.FilePath = e.relPath
	node.Language = languageName
	node.Container = e.container
	// containerParent stays unset: Go packages are flat, so a container
	// never has a parent (the design doc's Logical containers table says so
	// for Go, Java and Kotlin alike). Core is fine with that - a container
	// with no parent yields an empty `parent_chain`, which is the complete
	// and correct answer rather than a gap: the gap rule GM-265 documents
	// bites only a language whose containers *do* nest and where an
	// intermediate one can be memberless, which Go has no way to produce.
	// The practical consequence is that Go's `container(pkg)` visibility is
	// exactly "the same package and nothing else", which is exactly Go's
	// own rule for an unexported name.
	id := e.addNode(node)
	e.addEdge(e.fileNodeID, edgeKindDefines, id)
	if node.Visibility.kind == "public" {
		e.addEdge(e.fileNodeID, edgeKindExports, id)
	}
	return id
}

// visibilityOf is Go's export rule, which is entirely a property of the
// spelling: a capitalized name is visible everywhere, anything else only
// within its own package. Core turns the latter into "the requester's
// container is this one", and Go packages having no parents makes that an
// exact match rather than an ancestor walk.
//
// A method's visibility is read off the *method* name, not the receiver
// type's. An exported method on an unexported type is not reachable by name
// from outside the package in practice, but it is genuinely public as a
// name (it satisfies an interface, it is promoted through an embedded
// field), and modelling it as package-private would refuse links that are
// real.
func (e *extractor) visibilityOf(name string) visibility {
	if ast.IsExported(name) {
		return publicVisibility()
	}
	return containerVisibility(e.container)
}

// --- positions ----------------------------------------------------------

// position converts a go/token position (1-based line and byte column) to
// the wire's 0-based pair, which is what tree-sitter reports and therefore
// what every other plugin and every core consumer already expects.
func (e *extractor) position(pos token.Pos) wirePosition {
	if !pos.IsValid() {
		return wirePosition{}
	}
	p := e.fset.Position(pos)
	line := p.Line - 1
	col := p.Column - 1
	if line < 0 {
		line = 0
	}
	if col < 0 {
		col = 0
	}
	return wirePosition{Line: line, Col: col}
}

func (e *extractor) rangeOf(start, end token.Pos) wireRange {
	return wireRange{Start: e.position(start), End: e.position(end)}
}

// --- imports ------------------------------------------------------------

// collectImports emits one placeholder node and one IMPORTS edge per import
// specifier, and records what each one binds.
//
// # The three import forms, each decided and each documented
//
//   - **An alias** (`import f "fmt"`) binds the alias. Nothing else changes:
//     the placeholder still addresses the import path, because that is what
//     the file depends on.
//   - **A blank import** (`import _ "net/http/pprof"`) binds nothing, and
//     still gets its placeholder and its IMPORTS edge. The dependency is
//     completely real - it is the *only* thing such an import expresses -
//     and `get_dependencies` would be wrong to omit it. What it cannot do is
//     make a name available, so no use site can ever reach it.
//   - **A dot import** (`import . "pkg"`) binds every exported name of the
//     imported package into this file, invisibly. That makes every bare name
//     in the file ambiguous between "this package" and "the dot-imported
//     one", and a structural tier has no way to tell them apart: the set of
//     names the other package exports is precisely what it cannot see. So a
//     file containing a dot import emits **no own-container placeholders at
//     all** (useIdent), and keeps only the two kinds of edge that stay
//     exact: direct same-file hits, which are lexical, and qualified
//     `pkg.F()` uses through some *other* import, which name their container
//     explicitly. The import itself still gets its placeholder and its
//     IMPORTS edge. Guessing the other way - emitting the own-container
//     address anyway - would silently produce a wrong edge every time the
//     name really came from the dot import and the own package happened to
//     declare one too, which is the exact failure the project's standing
//     rule exists to prevent. Dot imports are rare outside test helpers, so
//     the cost of the conservative choice is small and confined to the files
//     that opted into the ambiguity.
func (e *extractor) collectImports(file *ast.File) {
	for _, spec := range file.Imports {
		if spec.Path == nil {
			continue
		}
		importPath, err := strconv.Unquote(spec.Path.Value)
		if err != nil || importPath == "" {
			continue
		}

		nativeKind := externalModuleNativeKind
		var target *placeholderTarget
		if e.ws.isProjectImportPath(importPath) {
			nativeKind = resolvedModuleNativeKind
			target = &placeholderTarget{
				Scope:         targetScope{Container: importPath},
				Key:           targetKey{Name: reexportAllName},
				FromContainer: e.container,
			}
		}

		id := nodeIDFor(e.relPath, nodeKindModule, importPath, nativeKind)
		e.placeholder[id] = true
		e.addNode(wireNode{
			ID:            id,
			Kind:          nodeKindModule,
			Name:          path.Base(importPath),
			QualifiedName: importPath,
			FilePath:      e.relPath,
			Range:         e.rangeOf(spec.Pos(), spec.End()),
			Visibility:    fileVisibility(),
			Language:      languageName,
			NativeKind:    nativeKind,
			Target:        target,
		})
		e.addEdge(e.fileNodeID, edgeKindImports, id)

		switch {
		case spec.Name == nil:
			e.imports[packageNameFromPath(importPath)] = importPath
		case spec.Name.Name == "_":
			// Binds nothing.
		case spec.Name.Name == ".":
			e.hasDotImport = true
		default:
			e.imports[spec.Name.Name] = importPath
		}
	}
}

// packageNameFromPath guesses the name an unaliased import binds.
//
// Go's answer is the imported package's own `package` clause, which is in
// another file this per-file tier never reads, so this is a guess - the same
// last-segment guess every editor makes before its type checker answers. Two
// refinements cover the conventions that break the naive rule: a major
// version suffix (`.../v2` binds the package, not "v2") and a
// `gopkg.in`-style `.vN` suffix on the segment itself.
//
// When the guess is wrong (`gopkg.in/yaml.v2` is `yaml`, which this gets
// right; a package whose clause simply disagrees with its directory, which
// nothing can get right without reading it), the consequence is bounded and
// is the safe direction: the selector base is not recognized as a package,
// so `that.F()` is treated as a receiver call and becomes an *open site*
// rather than a wrong edge. GM-281 resolves it exactly.
func packageNameFromPath(importPath string) string {
	segments := strings.Split(importPath, "/")
	last := segments[len(segments)-1]
	if isMajorVersionSegment(last) && len(segments) > 1 {
		last = segments[len(segments)-2]
	}
	if at := strings.LastIndex(last, "."); at > 0 && isMajorVersionSegment(last[at+1:]) {
		last = last[:at]
	}
	return last
}

func isMajorVersionSegment(segment string) bool {
	if len(segment) < 2 || segment[0] != 'v' {
		return false
	}
	for _, r := range segment[1:] {
		if r < '0' || r > '9' {
			return false
		}
	}
	return true
}

// --- declarations -------------------------------------------------------

// declareAll creates every node this file declares, before any use site is
// walked. Two passes are needed because Go's package block has no
// ordering: a function may call one declared below it, and a `resolved:
// true` same-file edge needs that target to already be a node.
func (e *extractor) declareAll(file *ast.File) {
	for _, decl := range file.Decls {
		switch d := decl.(type) {
		case *ast.FuncDecl:
			e.declareFunc(d)
		case *ast.GenDecl:
			e.declareGen(d)
		}
	}
}

// declareFunc adds the node for a function, a method, or an `init`.
//
// # Methods: `T.M`, with the receiver kind in the signature
//
// `func (s *Server) Close()` and `func (s Server) Close()` both become
// `Server.Close`, because they are the same method as far as any question a
// caller asks is concerned - and because Go forbids declaring both, so the
// normalization can never merge two distinct declarations. Which of the two
// it is stays visible where it belongs, in the printed signature.
//
// # `init`: several per package, and several per file
//
// `func init()` is the one Go declaration whose name is not unique - a
// package may have any number, and so may one file. They would collide on
// qualifiedName and therefore on node id, which would silently drop all but
// one of them and, worse, make the surviving node's range depend on which
// one came last.
//
// Skipping them was the alternative and was rejected: an `init` body is
// ordinary code making ordinary calls, and with no node to hang them on
// every one of those calls would degrade to a `REFERENCES` edge from the
// File node, losing the caller. So they are kept and disambiguated in
// `nativeKind`, which participates in the node id: the first `init` of a
// file is `init`, the second `init#1`, and so on in source order. Two
// consequences, both accepted deliberately: the ids are stable as long as
// the *order* of a file's inits is (adding one at the end changes nothing
// about the ones before it), and two `init`s in one file share a
// qualifiedName, which makes them ambiguous to any name lookup - correctly,
// since nothing in Go can name an `init` to call it anyway.
func (e *extractor) declareFunc(d *ast.FuncDecl) {
	if d.Name == nil || d.Name.Name == "" || d.Name.Name == "_" {
		return
	}
	name := d.Name.Name

	nativeKind := "function"
	qualifiedName := name

	if d.Recv != nil && len(d.Recv.List) > 0 {
		receiver, ok := receiverTypeName(d.Recv.List[0].Type)
		if !ok {
			// A receiver this tier cannot read a type name out of - only
			// reachable from a partial AST, since every legal receiver is
			// `T`, `*T` or a generic instantiation of one. No node: a
			// qualifiedName of just `M` would collide with a package-level
			// function `M`, and a wrong identity is worse than a missing
			// declaration.
			return
		}
		nativeKind = "method"
		qualifiedName = receiver + "." + name
	} else if name == "init" {
		nativeKind = "init"
		if e.initOrdinal > 0 {
			nativeKind = "init#" + strconv.Itoa(e.initOrdinal)
		}
		e.initOrdinal++
	}

	id := e.declareSymbol(wireNode{
		ID:            nodeIDFor(e.relPath, nodeKindFunction, qualifiedName, nativeKind),
		Kind:          nodeKindFunction,
		Name:          name,
		QualifiedName: qualifiedName,
		Range:         e.rangeOf(d.Pos(), d.End()),
		Signature:     e.funcSignature(d),
		Visibility:    e.visibilityOf(name),
		DocComment:    docText(d.Doc),
		NativeKind:    nativeKind,
	})

	// Only a plain function is a package-scope *name*. A method is reached
	// through a receiver, and `init` cannot be named at all.
	if d.Recv == nil && nativeKind == "function" {
		e.rememberFileDecl(name, id)
	}
}

func (e *extractor) declareGen(d *ast.GenDecl) {
	switch d.Tok {
	case token.TYPE:
		for _, spec := range d.Specs {
			if typeSpec, ok := spec.(*ast.TypeSpec); ok {
				e.declareType(d, typeSpec)
			}
		}
	case token.VAR, token.CONST:
		for _, spec := range d.Specs {
			if valueSpec, ok := spec.(*ast.ValueSpec); ok {
				e.declareValues(d, valueSpec)
			}
		}
	}
}

func (e *extractor) declareType(gen *ast.GenDecl, spec *ast.TypeSpec) {
	if spec.Name == nil || spec.Name.Name == "" || spec.Name.Name == "_" {
		return
	}
	name := spec.Name.Name
	nativeKind := typeNativeKind(spec)

	id := e.declareSymbol(wireNode{
		ID:            nodeIDFor(e.relPath, nodeKindType, name, nativeKind),
		Kind:          nodeKindType,
		Name:          name,
		QualifiedName: name,
		Range:         e.specRange(gen, spec),
		Signature:     e.typeSignature(spec, nativeKind),
		Visibility:    e.visibilityOf(name),
		DocComment:    docText(firstDoc(spec.Doc, gen)),
		NativeKind:    nativeKind,
	})
	e.rememberFileDecl(name, id)

	// An interface's methods are declarations in their own right - `I.M`,
	// the design doc's spelling. They are exactly that and nothing more: a
	// *declaration*, never an implementation. Nothing here claims that any
	// type satisfies the interface, because Go's interfaces are structural
	// and nothing in a type's syntax says so; `types.Implements` answers it
	// in GM-281 and emits the SUPERTYPE_OF edges.
	if iface, ok := spec.Type.(*ast.InterfaceType); ok {
		e.declareInterfaceMethods(name, iface)
	}
}

func (e *extractor) declareInterfaceMethods(interfaceName string, iface *ast.InterfaceType) {
	if iface.Methods == nil {
		return
	}
	for _, field := range iface.Methods.List {
		funcType, ok := field.Type.(*ast.FuncType)
		if !ok {
			continue // an embedded interface or a type-set element, not a method
		}
		for _, methodName := range field.Names {
			if methodName == nil || methodName.Name == "" || methodName.Name == "_" {
				continue
			}
			qualifiedName := interfaceName + "." + methodName.Name
			e.declareSymbol(wireNode{
				ID:            nodeIDFor(e.relPath, nodeKindFunction, qualifiedName, "interface_method"),
				Kind:          nodeKindFunction,
				Name:          methodName.Name,
				QualifiedName: qualifiedName,
				Range:         e.rangeOf(field.Pos(), field.End()),
				Signature:     e.interfaceMethodSignature(methodName, funcType),
				Visibility:    e.visibilityOf(methodName.Name),
				DocComment:    docText(field.Doc),
				NativeKind:    "interface_method",
			})
		}
	}
}

func (e *extractor) declareValues(gen *ast.GenDecl, spec *ast.ValueSpec) {
	nativeKind := "var"
	if gen.Tok == token.CONST {
		nativeKind = "const"
	}
	for _, ident := range spec.Names {
		if ident == nil || ident.Name == "" || ident.Name == "_" {
			// `var _ = ...` and `var _ I = (*T)(nil)` declare no name.
			// The node is skipped; the initializer is still walked for its
			// own uses (useValueSpec), from the File node.
			continue
		}
		name := ident.Name
		id := e.declareSymbol(wireNode{
			ID:            nodeIDFor(e.relPath, nodeKindVariable, name, nativeKind),
			Kind:          nodeKindVariable,
			Name:          name,
			QualifiedName: name,
			Range:         e.specRange(gen, spec),
			Visibility:    e.visibilityOf(name),
			DocComment:    docText(firstDoc(spec.Doc, gen)),
			NativeKind:    nativeKind,
		})
		e.rememberFileDecl(name, id)
	}
}

// rememberFileDecl records a package-scope name of this file. First writer
// wins: two package-scope declarations of one name is illegal Go, so this
// only ever happens on a partial AST, where picking the first one keeps the
// result deterministic rather than dependent on how far the parser got.
func (e *extractor) rememberFileDecl(name, id string) {
	if _, exists := e.fileDecls[name]; !exists {
		e.fileDecls[name] = id
	}
}

// specRange is the range a `type`/`var`/`const` declaration reports.
//
// For the ordinary one-spec form (`type Server struct { ... }`) it spans the
// whole declaration including the keyword, which is what a reader means by
// "the declaration". Inside a parenthesized block each spec reports only
// itself, since the keyword belongs to all of them.
func (e *extractor) specRange(gen *ast.GenDecl, spec ast.Node) wireRange {
	if gen.Lparen.IsValid() {
		return e.rangeOf(spec.Pos(), spec.End())
	}
	return e.rangeOf(gen.Pos(), spec.End())
}

// firstDoc applies Go's own convention for a doc comment on a spec inside a
// declaration group: the spec's own comment, or - only when the group holds
// exactly one spec, i.e. when the comment above the keyword is
// unambiguously about that spec - the group's.
func firstDoc(own *ast.CommentGroup, gen *ast.GenDecl) *ast.CommentGroup {
	if own != nil {
		return own
	}
	if len(gen.Specs) == 1 {
		return gen.Doc
	}
	return nil
}

func typeNativeKind(spec *ast.TypeSpec) string {
	if spec.Assign.IsValid() {
		return "alias"
	}
	switch spec.Type.(type) {
	case *ast.StructType:
		return "struct"
	case *ast.InterfaceType:
		return "interface"
	default:
		return "type"
	}
}

// receiverTypeName reads the type name out of a method receiver: `T`, `*T`,
// and their generic forms `T[P]` / `*T[P, Q]`, all of which normalize to
// `T`. The pointer-ness is deliberately dropped here and preserved in the
// signature instead - see declareFunc.
func receiverTypeName(expr ast.Expr) (string, bool) {
	for {
		switch n := expr.(type) {
		case *ast.ParenExpr:
			expr = n.X
		case *ast.StarExpr:
			expr = n.X
		case *ast.IndexExpr:
			expr = n.X
		case *ast.IndexListExpr:
			expr = n.X
		case *ast.Ident:
			if n.Name == "" || n.Name == "_" {
				return "", false
			}
			return n.Name, true
		default:
			return "", false
		}
	}
}

// --- signatures and doc comments ----------------------------------------

// printExpr renders an AST node back to source with go/printer, then
// collapses every whitespace run to one space. A signature is a single-line
// display string, and a multi-line parameter list printed verbatim would
// carry the source's own line breaks into a field nothing renders as source.
func (e *extractor) printExpr(node ast.Node) string {
	if node == nil {
		return ""
	}
	var buf bytes.Buffer
	if err := (&printer.Config{Mode: printer.RawFormat, Tabwidth: 4}).Fprint(&buf, e.fset, node); err != nil {
		return ""
	}
	return truncate(strings.Join(strings.Fields(buf.String()), " "), maxSignatureLength)
}

// funcSignature prints the declaration without its body, which is exactly
// `func (s *Server) Close() error` - receiver, receiver kind, type
// parameters, parameters and results, and nothing else.
func (e *extractor) funcSignature(d *ast.FuncDecl) string {
	if d.Type == nil {
		return ""
	}
	return e.printExpr(&ast.FuncDecl{Recv: d.Recv, Name: d.Name, Type: d.Type})
}

func (e *extractor) interfaceMethodSignature(name *ast.Ident, funcType *ast.FuncType) string {
	return e.printExpr(&ast.FuncDecl{Name: name, Type: funcType})
}

// typeSignature summarizes a type declaration rather than reprinting it. A
// struct or interface body is the type's *contents*, not its signature, and
// reprinting a hundred-field struct into a display string helps nobody; the
// body is already in the source the range points at.
func (e *extractor) typeSignature(spec *ast.TypeSpec, nativeKind string) string {
	signature := "type " + spec.Name.Name + e.typeParamsText(spec.TypeParams)
	switch nativeKind {
	case "struct":
		return signature + " struct"
	case "interface":
		return signature + " interface"
	case "alias":
		return signature + " = " + e.printExpr(spec.Type)
	default:
		return truncate(signature+" "+e.printExpr(spec.Type), maxSignatureLength)
	}
}

// typeParamsText renders `[T any, U comparable]`. Built by hand rather than
// printed, because go/printer has no context to tell a type-parameter list
// from any other ast.FieldList and renders it as a struct body.
func (e *extractor) typeParamsText(list *ast.FieldList) string {
	if list == nil || len(list.List) == 0 {
		return ""
	}
	parts := make([]string, 0, len(list.List))
	for _, field := range list.List {
		names := make([]string, 0, len(field.Names))
		for _, name := range field.Names {
			names = append(names, name.Name)
		}
		part := strings.Join(names, ", ")
		if constraint := e.printExpr(field.Type); constraint != "" {
			if part != "" {
				part += " "
			}
			part += constraint
		}
		parts = append(parts, part)
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

func docText(group *ast.CommentGroup) string {
	if group == nil {
		return ""
	}
	return truncate(strings.TrimSpace(group.Text()), maxDocCommentLength)
}

func truncate(text string, limit int) string {
	if len(text) <= limit {
		return text
	}
	// Cut on a rune boundary: a signature is UTF-8 and a half-encoded rune
	// would be invalid JSON input for encoding/json to escape.
	cut := limit
	for cut > 0 && text[cut]&0xC0 == 0x80 {
		cut--
	}
	return text[:cut] + "..."
}

// --- the File node ------------------------------------------------------

// computeFileNode builds the one node every file gets, whether or not it
// parses.
//
// visibility is deliberately "file", not "public": this repo's one concrete
// example of a Go plugin's File node (core/tests/fixtures/valid_v2.ndjson)
// uses "file", matching the TS plugin's own convention (extract.ts's
// addNode: a node is "file"-visible unless something marks it exported, and
// nothing ever marks the File node itself exported - only what it *defines*
// can be public). See docs/architecture/multi-language-plugins.md, Go plugin
// section, "Implementation notes (GM-279)".
//
// The File node also carries no `container`. It is not a member of the
// package - its declarations are - and counting it as one would make a
// container's memberCount depend on how many files it is spread across.
func computeFileNode(relPath string, content []byte) wireNode {
	endLine, endCol := textEndPosition(content)
	return wireNode{
		ID:            nodeIDFor(relPath, nodeKindFile, relPath, ""),
		Kind:          nodeKindFile,
		Name:          path.Base(relPath),
		QualifiedName: relPath,
		FilePath:      relPath,
		Range: wireRange{
			Start: wirePosition{Line: 0, Col: 0},
			End:   wirePosition{Line: endLine, Col: endCol},
		},
		Visibility:      fileVisibility(),
		Language:        languageName,
		HasSyntaxErrors: false,
	}
}

// textEndPosition is the File node's end position, computed from the bytes
// rather than from the AST - and it must agree with what
// core/src/cli/plugin_check/session.rs's whitespace_edit actually does for
// that check to pass: endLine is the number of '\n' bytes in the file,
// endCol is the byte length of whatever text follows the last one (0 when
// the file ends in a newline). This is exactly tree-sitter's own root-node
// end position for the TS plugin - see that doc comment's own measurement,
// an 8-line file ending at (8, 0) - so a whitespace-only edit before the
// file's *last* newline (never after it) changes neither the newline count
// nor what follows the final one, and this function's answer is unchanged by
// it, which is what lets control.go's handleFileChanged answer that edit
// with an empty diff.
//
// Reading it from the bytes rather than from `ast.File.End()` is also what
// makes it defined for a file that does not parse at all: an empty file and
// a file whose first token is broken both still have a length.
func textEndPosition(content []byte) (line, col int) {
	line = bytes.Count(content, []byte{'\n'})
	lastNewline := bytes.LastIndexByte(content, '\n')
	col = len(content) - (lastNewline + 1)
	return line, col
}
