package main

// The semantic tier: `go/types` through golang.org/x/tools/go/packages
// (GM-281). This is the pass that answers what extract.go's structural tier
// deliberately refuses to guess - which type a receiver has, which method an
// embedded field promotes, which interface a type satisfies - and it is the
// only part of this plugin that needs a Go toolchain on PATH.
//
// # What a pass emits, and why a placeholder rather than a node id
//
// Every answer is a `qualifiedName`-keyed placeholder in the *declaring*
// package's container, plus one `semantic` / `go-types` edge onto it:
//
//	target = { scope: { container: "<import path>" },
//	           key:   { qualifiedName: "Server.Close" },
//	           fromContainer: "<the using file's package>" }
//
// That is the address core's linker was generalized for
// (core/src/graph/symbol_links.rs, "The address is a row, not a string"): a
// `name` key would be `Close`, which exists on dozens of types, while a
// `qualifiedName` key names one declaration and nothing else. Computing the
// *node id* of that declaration here and emitting a direct edge was the
// alternative and is worse for a reason that is easy to miss: this process
// knows the declaring file, but it does not know that core has that file
// indexed (it may be gitignored, or the walk may not have reached it yet),
// and an edge onto an id nothing declares is a dangling row no query can see
// past. A placeholder degrades to "unresolved" instead, which is the honest
// answer and the one the whole handshake exists to produce.
//
// The same reasoning is why nothing here is ever emitted for a target
// outside this project (the standard library, a module dependency): those
// packages are not indexed, so an address into them could only ever go
// unresolved.
//
// # Laziness, and the marker that proves it
//
// `packages.Load` shells out to `go list`, which for a large repository
// costs seconds and hundreds of megabytes. Core therefore promises never to
// ask for it during bulk indexing or a `fileChanged`, and a plugin promises
// not to start its engine before the first `semanticPass` - the conformance
// kit's `capabilities.semantic-engine-lazy` check, which reads the marker
// [markSemanticEngineStarted] writes at exactly the moment this file first
// calls `packages.Load`. Nothing else in this process imports
// golang.org/x/tools at all, so "the engine started" and "a semanticPass
// arrived" cannot come apart.
//
// # Degradation
//
// No `go` on PATH, or a `packages.Load` that fails outright: log once and
// answer with an empty diff and `incomplete: true` (wire/src/lib.rs's
// `FileChangeResponse::incomplete`). That field is what makes the rest of
// the sentence true rather than aspirational: core's
// `watcher::apply::apply_semantic_pass` withholds
// `language_state.semanticPassAt` only when a whole-project pass reports
// itself incomplete, so the structural graph stays exactly as it was,
// `semanticPassAt` is never set for Go, and the MCP instructions keep
// listing the receiver-call gap - the design doc's "Semantic engine
// missing" failure mode, which is a *partial* index rather than a broken
// one. A pass that resolved nothing but never says so is, on the wire, a
// pass that finished - see GM-384.
//
// # What this pass does not answer
//
//   - **Files excluded by the host's build constraints.** `go/packages`
//     type-checks for one GOOS/GOARCH pair, so `sys_windows.go` on a Linux
//     host is never loaded and its structural graph receives no upgrade. It
//     keeps every node and edge extract.go gave it; only the receiver calls
//     inside it stay open. Documented in plugins/go/README.md rather than
//     hidden.
//   - **Struct fields.** `x.field` resolves exactly, but this plugin emits
//     no node for a struct field (extract.go models declarations, not
//     members), so there is nothing for an edge to land on. The site is
//     resolved and then dropped.
//   - **Dependents of a changed package, during a per-file pass.** Re-
//     checking one file re-checks its own package and no other, so an edge
//     from a *different* package into a symbol this edit moved is refreshed
//     when that package is next checked - the same staleness the TS plugin
//     accepts today.

import (
	"fmt"
	"go/token"
	"go/types"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"golang.org/x/tools/go/packages"
)

const (
	// engineNameSemantic labels every edge this tier emits. The *tier* is
	// the closed set core branches on (`semantic`); the engine is the free
	// label that says which of this plugin's two passes produced the edge,
	// so an upgrade is distinguishable from the `go-parser` edge it sits
	// beside.
	engineNameSemantic = "go-types"
	sourceTierSemantic = "semantic"

	edgeKindSupertypeOf = "SUPERTYPE_OF"

	// markerDirEnv and semanticEngineMarker mirror
	// core/src/cli/plugin_check/session.rs's MARKER_DIR_ENV /
	// SEMANTIC_ENGINE_MARKER, and the README's plugin-authoring section.
	// Unset in every real run, where writing the marker is a no-op.
	markerDirEnv         = "G_MESH_PLUGIN_CHECK_MARKER_DIR"
	semanticEngineMarker = "semantic-engine-started"

	// loadMode is `packages.LoadSyntax`'s mode: names, files, the import
	// graph, and *for the initial packages only* types, syntax and
	// type-checking results.
	//
	// `NeedDeps` is deliberately absent. With it, every transitive
	// dependency is type-checked from source - the whole standard library
	// included - to produce syntax trees this pass never looks at: the only
	// thing it ever asks about a dependency's symbol is its package path and
	// name, which export data already carries. Without it, `go list`
	// supplies dependencies as compiled export data and the type checker
	// reads them from there, which is both dramatically cheaper and exactly
	// what "reusing loaded dependencies" means for a per-file re-check: the
	// go build cache, not an in-process package graph (go/packages has no
	// incremental API to reuse one through).
	loadMode = packages.NeedName | packages.NeedFiles | packages.NeedCompiledGoFiles |
		packages.NeedImports | packages.NeedTypes | packages.NeedSyntax | packages.NeedTypesInfo
)

// markSemanticEngineStarted appends this process's pid to the conformance
// kit's semantic-engine marker, if the kit set the directory to write it in.
//
// "At the moment it starts its semantic engine" is taken literally: the one
// call site is immediately before the first `packages.Load` of a run, not at
// the top of the pass and not at process start. A marker written earlier
// would make `capabilities.semantic-engine-lazy` pass for a plugin that in
// fact loads eagerly; a marker written later would make it pass for a plugin
// whose engine had already been running for a while.
func markSemanticEngineStarted() {
	dir := os.Getenv(markerDirEnv)
	if dir == "" {
		return
	}
	file, err := os.OpenFile(filepath.Join(dir, semanticEngineMarker), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		// A conformance-run diagnostic only - never worth failing a pass
		// over, exactly as plugins/typescript/src/semantic.ts treats its own.
		return
	}
	defer file.Close()
	fmt.Fprintf(file, "%d\n", os.Getpid())
}

// semanticEngine is this process's whole semantic state. Constructing one
// loads nothing: see this file's "Laziness" section.
type semanticEngine struct {
	// projectRoot as core passed it, and realRoot with symlinks resolved -
	// the base walkProjectFiles itself makes its relative paths against
	// (walk.go's canonicalizeProjectRoot), and therefore the base every
	// file path on the wire is relative to. `go list` reports absolute
	// paths, and on macOS it reports them under /private/var while the
	// caller's own $TMPDIR says /var, so both spellings are kept and tried.
	projectRoot string
	realRoot    string

	// loggedDegradation keeps the "no toolchain" / "load failed" line to one
	// per process, as the design doc's failure-mode table specifies ("the
	// plugin logs once and answers semanticPass with an empty diff"). A pass
	// runs on every file change, so an unconditional log would fill a user's
	// daemon log with one line per keystroke.
	loggedDegradation bool

	// emittedEdges is the semantic edge ids this process last produced for
	// each file, so a re-pass of that file can retract the ones it no longer
	// produces (a renamed method, a type that stopped satisfying an
	// interface). Only edges: a stale *placeholder node* with no edges left
	// on it is inert, while `deleteNodeIds` is held to "every id was emitted
	// before" by the conformance kit and is not worth the bookkeeping for
	// rows nothing reads.
	emittedEdges map[string][]string

	// ws is the module layout of the pass currently running, set by `run`
	// and cleared when it returns. A pass is strictly sequential within one
	// process (the control loop reads and answers one frame at a time), so
	// there is never more than one in flight, and carrying it on the engine
	// rather than threading it through every helper keeps
	// `isProjectContainer` - which every resolution consults - a one-line
	// call.
	ws *workspace
}

func newSemanticEngine(projectRoot string) *semanticEngine {
	return &semanticEngine{
		projectRoot:  projectRoot,
		realRoot:     canonicalizeProjectRoot(projectRoot),
		emittedEdges: map[string][]string{},
	}
}

func (e *semanticEngine) degrade(format string, args ...interface{}) {
	if e.loggedDegradation {
		return
	}
	e.loggedDegradation = true
	logf(format, args...)
}

// --- the structural view ------------------------------------------------

// structuralFile is what extract.go says about one file, reduced to the
// three things a resolution needs: the open questions, the calls that may
// have been addressed at a type, and the node ids an answer can be written
// from.
type structuralFile struct {
	container        string
	openSites        []openSite
	placeholderCalls []placeholderCall
	// typeNodes are the file's `Type` declarations by name - the `from` of
	// every SUPERTYPE_OF edge.
	typeNodes map[string]wireNode
}

// extractStructural re-runs the structural extractor over the files in
// scope.
//
// Re-extracting rather than reading `pluginState`'s cache is not a missed
// optimization. A whole-project pass covers files this process has never
// been asked about (the bulk walk that did see them was a *different*
// process), so the cache is empty for almost all of them; and re-extracting
// guarantees that the byte offsets an open site carries were computed from
// the same file contents `go/packages` is about to parse, which is the one
// thing the position-keyed join below depends on.
func (e *semanticEngine) extractStructural(ws *workspace, relPaths []string) map[string]structuralFile {
	out := make(map[string]structuralFile, len(relPaths))
	for _, relPath := range relPaths {
		content, err := os.ReadFile(filepath.Join(e.realRoot, filepath.FromSlash(relPath)))
		if err != nil {
			continue
		}
		graph := extractFile(ws, relPath, content)
		file := structuralFile{
			container:        graph.container,
			openSites:        graph.openSites,
			placeholderCalls: graph.placeholderCalls,
			typeNodes:        map[string]wireNode{},
		}
		for _, node := range graph.nodes {
			if node.Kind == nodeKindType {
				file.typeNodes[node.QualifiedName] = node
			}
		}
		out[relPath] = file
	}
	return out
}

// --- loading ------------------------------------------------------------

// sitePos is the join key between the two halves of this pass: a
// project-relative file plus a 0-based line/column, which is what an open
// site records and what `go/types`' own position for the same identifier
// converts to.
type sitePos struct {
	file string
	line int
	col  int
}

// resolutions is everything `go/types` said about the positions this pass
// cares about, plus the project's own named types and interfaces.
type resolutions struct {
	// selections answers `x.M()` / `x.field` - a selection through a value,
	// which is the one thing `types.Info.Selections` exists for and the one
	// the structural tier can never guess.
	selections map[sitePos]*types.Selection
	// uses answers every other identifier, `pkg.F` and a dot-imported bare
	// name alike.
	uses map[sitePos]types.Object

	// namedTypes and interfaces are the project's own, for types.Implements.
	// Keyed by the type's own object so the same declaration reached through
	// two package variants (`app` and `app [app.test]`) collapses to one.
	namedTypes map[*types.TypeName]*types.Named
	interfaces map[*types.TypeName]*types.Named
	// declaredIn is the project-relative file each of those was declared in.
	// It is recorded while folding, not derived later, because a
	// `types.Object` carries a `token.Pos` that only means something in the
	// `token.FileSet` it was type-checked with - and this pass creates one
	// per `packages.Load` call.
	declaredIn map[*types.TypeName]string
}

func newResolutions() *resolutions {
	return &resolutions{
		selections: map[sitePos]*types.Selection{},
		uses:       map[sitePos]types.Object{},
		namedTypes: map[*types.TypeName]*types.Named{},
		interfaces: map[*types.TypeName]*types.Named{},
		declaredIn: map[*types.TypeName]string{},
	}
}

// load runs `go list` + the type checker over `patterns` from `dir` and folds
// the result into `into`. Returns false only for a driver-level failure -
// individual packages that do not compile are kept, because a file with a
// broken sibling still has exact type information for everything the checker
// did get through, and refusing the whole pass over one bad package is how a
// semantic tier becomes useless in exactly the repositories that need it.
func (e *semanticEngine) load(into *resolutions, dir string, patterns []string) bool {
	fset := token.NewFileSet()
	cfg := &packages.Config{Mode: loadMode, Dir: dir, Tests: true, Fset: fset}

	// The engine is starting *now* - see markSemanticEngineStarted.
	markSemanticEngineStarted()

	loaded, err := packages.Load(cfg, patterns...)
	if err != nil {
		e.degrade("semantic pass degraded: packages.Load in %s failed: %v - answering with an empty diff", dir, err)
		return false
	}
	for _, pkg := range loaded {
		e.foldPackage(into, fset, pkg)
	}
	return true
}

// foldPackage indexes one loaded package's type information by position and
// collects its named types.
func (e *semanticEngine) foldPackage(into *resolutions, fset *token.FileSet, pkg *packages.Package) {
	if pkg.TypesInfo != nil {
		for expr, selection := range pkg.TypesInfo.Selections {
			if expr.Sel == nil {
				continue
			}
			if key, ok := e.positionOf(fset, expr.Sel.Pos()); ok {
				into.selections[key] = selection
			}
		}
		for ident, obj := range pkg.TypesInfo.Uses {
			if key, ok := e.positionOf(fset, ident.Pos()); ok {
				// Selections win: `types.Info` never records a method or
				// field selection in both maps, so this only ever guards
				// against two packages disagreeing about one position, which
				// they cannot (a file belongs to one package, whatever test
				// variants it is compiled into).
				if _, taken := into.selections[key]; !taken {
					into.uses[key] = obj
				}
			}
		}
	}
	if pkg.Types == nil || !e.isProjectContainer(pkg.PkgPath) {
		return
	}
	scope := pkg.Types.Scope()
	for _, name := range scope.Names() {
		typeName, ok := scope.Lookup(name).(*types.TypeName)
		if !ok || typeName.IsAlias() {
			// An alias declares no new type: `types.Implements(A, I)` is by
			// construction the same answer as for whatever `A` names, so an
			// edge from it would restate a fact already stated from the
			// underlying declaration.
			continue
		}
		named, ok := typeName.Type().(*types.Named)
		if !ok || named.TypeParams().Len() > 0 {
			// A generic type is not one type but a family of them, and
			// whether `Stack[T]` satisfies an interface can depend on `T`.
			// This plugin gives the whole family one node (`Stack`), so
			// there is no honest single answer to attach to it.
			continue
		}
		declaredIn, ok := e.positionOf(fset, typeName.Pos())
		if !ok {
			continue
		}
		into.namedTypes[typeName] = named
		into.declaredIn[typeName] = declaredIn.file
		if iface, ok := named.Underlying().(*types.Interface); ok && iface.NumMethods() > 0 && iface.IsMethodSet() {
			// An empty interface is satisfied by everything, which is true
			// and useless; a constraint interface (one with a type set) is
			// not something a type "implements" in the sense
			// find_implementations answers.
			into.interfaces[typeName] = named
		}
	}
}

// positionOf turns a `go/token` position into this plugin's own
// (project-relative path, 0-based line, 0-based column) key, or reports that
// the file is not one this project indexes.
//
// Files outside the project are the normal case here, not an error: with
// `Tests: true` the loader synthesizes a `<pkg>.test` main package under the
// build cache, and every dependency's source lives in the module cache.
func (e *semanticEngine) positionOf(fset *token.FileSet, pos token.Pos) (sitePos, bool) {
	if !pos.IsValid() {
		return sitePos{}, false
	}
	position := fset.Position(pos)
	rel, ok := e.relPathOf(position.Filename)
	if !ok {
		return sitePos{}, false
	}
	return sitePos{file: rel, line: position.Line - 1, col: position.Column - 1}, true
}

// relPathOf expresses an absolute path from `go list` the way the wire
// spells it. Both roots are tried because the two can legitimately differ by
// a symlink: macOS's $TMPDIR is /var/folders/..., a symlink to
// /private/var/folders/..., and the go command reports the resolved form.
func (e *semanticEngine) relPathOf(abs string) (string, bool) {
	for _, root := range [2]string{e.realRoot, e.projectRoot} {
		if root == "" {
			continue
		}
		rel, err := filepath.Rel(root, abs)
		if err != nil {
			continue
		}
		rel = filepath.ToSlash(rel)
		if rel == ".." || strings.HasPrefix(rel, "../") || filepath.IsAbs(rel) {
			continue
		}
		return rel, true
	}
	return "", false
}

// isProjectContainer reports whether an import path is one of this project's
// own packages - the only kind of address this pass ever emits.
//
// The `_test` suffix is handled explicitly because an external test package
// is a container of this project whose *import path* is not under any module
// path: `github.com/example/app_test` is not `github.com/example/app` and
// does not lie under it, but it is exactly the container key extract.go
// gives the files of `package app_test`.
func (e *semanticEngine) isProjectContainer(path string) bool {
	if path == "" || e.ws == nil {
		return false
	}
	if e.ws.isProjectImportPath(path) {
		return true
	}
	if trimmed, cut := strings.CutSuffix(path, "_test"); cut {
		return e.ws.isProjectImportPath(trimmed)
	}
	return false
}

// --- the pass -----------------------------------------------------------

// run answers one `semanticPass` request. `filePaths` empty means the whole
// project, core's own convention (protocol::types::ControlMessage::
// SemanticPass).
//
// The second return is wire.go's `fileChangeResponse.Incomplete`: `true`
// when this pass could not even try to resolve what it was asked about (no
// toolchain, or every module's `go list` failed), `false` when it ran to
// completion - including the trivial completion of "there was nothing in
// scope to resolve." That distinction is why an empty `scope` and a failed
// `loadFor` are not the same return: both answer an empty diff, but only
// the second one is a pass that owed an answer and did not give one.
func (e *semanticEngine) run(ws *workspace, filePaths []string) (fileChangeDiff, bool) {
	started := time.Now()
	e.ws = ws
	defer func() { e.ws = nil }()

	if _, err := exec.LookPath("go"); err != nil {
		e.degrade("semantic pass degraded: no `go` binary on PATH (%v) - answering every "+
			"semanticPass with an empty diff, incomplete=true; the structural graph is unaffected", err)
		return emptyDiff(), true
	}

	wholeProject := len(filePaths) == 0
	var scope []string
	if wholeProject {
		scope = walkProjectFiles(e.projectRoot)
	} else {
		scope = e.claimedFiles(filePaths)
	}
	if len(scope) == 0 {
		return emptyDiff(), false
	}

	structural := e.extractStructural(ws, scope)
	resolved := newResolutions()
	if !e.loadFor(ws, resolved, wholeProject, scope) {
		return emptyDiff(), true
	}

	builder := newSemanticDiff()
	sites := 0
	for _, relPath := range sortedKeys(structural) {
		sites += e.answerFile(builder, resolved, relPath, structural[relPath])
	}
	implementsEdges := e.answerImplements(builder, resolved, structural)

	diff := builder.finish(e, scope)
	logf("semantic pass: %d file(s), %d resolved site(s), %d implements edge(s), "+
		"%d node(s)/%d edge(s) upserted, %d edge(s) retracted, in %s",
		len(scope), sites, implementsEdges, len(diff.UpsertNodes), len(diff.UpsertEdges),
		len(diff.DeleteEdgeIds), time.Since(started).Round(time.Millisecond))
	return diff, false
}

// claimedFiles narrows a per-file request to the `.go` files this plugin
// actually indexes. Core routes by extension already, so this is a guard
// against a stray path rather than a filter that normally removes anything.
func (e *semanticEngine) claimedFiles(filePaths []string) []string {
	out := make([]string, 0, len(filePaths))
	for _, relPath := range filePaths {
		if isGoFile(relPath) {
			out = append(out, relPath)
		}
	}
	sort.Strings(out)
	return out
}

// loadFor type-checks what the request needs.
//
// A whole-project pass loads `./...` **once per module root**, with the
// loader's working directory set to that module. One invocation from the
// project root is not enough and one from each module is not redundant: a
// nested `go.mod` is outside the root module, so `go list ./nested/...` from
// the root fails there, and a `go.work` only makes it succeed when the
// workspace happens to name it. Running per module is the one form that
// works with and without a workspace file.
//
// A per-file pass asks for `file=<abs>` per file, grouped by the module each
// file belongs to - `go list`'s own way of saying "the package containing
// this file", which is exactly "re-check that file's package".
func (e *semanticEngine) loadFor(ws *workspace, into *resolutions, wholeProject bool, scope []string) bool {
	if wholeProject {
		dirs := moduleDirs(ws)
		ok := false
		for _, dir := range dirs {
			if e.load(into, filepath.Join(e.realRoot, filepath.FromSlash(dir)), []string{"./..."}) {
				ok = true
			}
		}
		return ok
	}

	byModule := map[string][]string{}
	for _, relPath := range scope {
		dir := moduleDirFor(ws, normalizeDir(filepath.ToSlash(filepath.Dir(relPath))))
		abs := filepath.Join(e.realRoot, filepath.FromSlash(relPath))
		byModule[dir] = append(byModule[dir], "file="+abs)
	}
	ok := false
	for _, dir := range sortedKeys(byModule) {
		patterns := byModule[dir]
		sort.Strings(patterns)
		if e.load(into, filepath.Join(e.realRoot, filepath.FromSlash(dir)), patterns) {
			ok = true
		}
	}
	return ok
}

// moduleDirs is every module root of the project, project-relative, sorted.
// A project with no go.mod at all still gets one entry - the root itself -
// so the loader is given a chance to say what is wrong rather than being
// skipped silently.
func moduleDirs(ws *workspace) []string {
	if len(ws.modules) == 0 {
		return []string{""}
	}
	dirs := make([]string, 0, len(ws.modules))
	for _, module := range ws.modules {
		dirs = append(dirs, module.dir)
	}
	sort.Strings(dirs)
	return dirs
}

// moduleDirFor is the innermost module containing a directory - the same
// rule workspace.importPath applies, reused here to pick `go list`'s working
// directory.
func moduleDirFor(ws *workspace, relDir string) string {
	for _, module := range ws.modules {
		if _, ok := underDir(module.dir, relDir); ok {
			return module.dir
		}
	}
	return ""
}

// --- answering one file -------------------------------------------------

// answerFile resolves one file's open sites and placeholder calls, returning
// how many sites produced an edge.
func (e *semanticEngine) answerFile(
	builder *semanticDiff,
	resolved *resolutions,
	relPath string,
	file structuralFile,
) int {
	answered := 0
	for _, site := range file.openSites {
		obj := resolved.objectAt(sitePos{file: relPath, line: site.Line, col: site.Col}, site.Kind)
		container, qualifiedName, isFunc, ok := e.addressOf(obj)
		if !ok {
			continue
		}
		toID := builder.placeholder(relPath, file.container, container, qualifiedName,
			wirePosition{Line: site.Line, Col: site.Col}, site.Name)
		if site.IsCall && isFunc && site.EnclosingCallerID != "" {
			builder.edge(relPath, site.EnclosingCallerID, edgeKindCalls, toID)
		} else if site.EnclosingSymbolID != "" {
			builder.edge(relPath, site.EnclosingSymbolID, edgeKindReferences, toID)
		} else {
			continue
		}
		answered++
	}

	for _, call := range file.placeholderCalls {
		// `Uses`, never `Selections`: a placeholder call is always either a
		// bare name (`T(x)`) or a package-qualified one (`pkg.T(x)`), and
		// both are plain identifier uses. A selection through a value never
		// produced a structural edge in the first place - it is an open
		// site, handled above.
		obj := resolved.uses[sitePos{file: relPath, line: call.Line, col: call.Col}]
		container, qualifiedName, isFunc, ok := e.addressOf(obj)
		if !ok || isFunc {
			// Either nothing to say, or the structural tier was right and
			// core has already linked its edge. Leaving a correct edge alone
			// is what keeps this pass's diff small.
			continue
		}
		// A conversion, not a call. Retract the CALLS edge core's kind filter
		// will never land, and say the true thing instead.
		builder.retract(relPath, call.EdgeID)
		if call.EnclosingSymbolID == "" {
			continue
		}
		toID := builder.placeholder(relPath, file.container, container, qualifiedName,
			wirePosition{Line: call.Line, Col: call.Col}, call.Name)
		builder.edge(relPath, call.EnclosingSymbolID, edgeKindReferences, toID)
		answered++
	}
	return answered
}

// objectAt reads the declaration `go/types` resolved a site to.
func (r *resolutions) objectAt(key sitePos, kind openSiteKind) types.Object {
	if kind == openSiteSelection {
		if selection, ok := r.selections[key]; ok {
			// Selection.Obj() is the *declared* method or field - the one on
			// the embedded type for a promoted method, the interface's own
			// for a call through an interface value. That is precisely the
			// declaration a caller list should point at, and precisely what
			// no amount of syntax could have told us.
			return selection.Obj()
		}
		// A selector whose base is a package name (`pkg.F`) is a *qualified
		// identifier*, not a selection, and `go/types` records it in `Uses`
		// instead. The structural tier reaches this shape only when it
		// guessed the import's binding name wrong (extract.go's
		// packageNameFromPath), which is exactly the case this fallback
		// repairs.
		return r.uses[key]
	}
	return r.uses[key]
}

// addressOf turns a resolved object into the container/qualifiedName pair
// that addresses its declaration, or reports that there is nothing to
// address.
//
// The qualifiedName spelling has to agree, character for character, with
// what extract.go gives the declaration node, because that is the key core
// matches on: `F` for a function, `T` for a type, `T.M` for a method (value
// and pointer receivers alike), `I.M` for an interface method.
func (e *semanticEngine) addressOf(obj types.Object) (container, qualifiedName string, isFunc, ok bool) {
	if obj == nil || obj.Pkg() == nil {
		// A builtin (`len`), a universe type (`error`), `nil`: no package,
		// and nothing this index declares.
		return "", "", false, false
	}
	name := obj.Name()
	if name == "" || name == "_" {
		return "", "", false, false
	}
	container = obj.Pkg().Path()
	if !e.isProjectContainer(container) {
		return "", "", false, false
	}

	switch object := obj.(type) {
	case *types.Func:
		signature, _ := object.Type().(*types.Signature)
		if signature != nil && signature.Recv() != nil {
			receiver := receiverName(signature.Recv().Type())
			if receiver == "" {
				return "", "", false, false
			}
			return container, receiver + "." + name, true, true
		}
		if name == "init" {
			// Not addressable in Go at all, so nothing can be referring to
			// one here; extract.go gives each its own node precisely because
			// nothing can name it.
			return "", "", false, false
		}
		return container, name, true, true

	case *types.TypeName:
		if !isPackageLevel(object) {
			return "", "", false, false
		}
		return container, name, false, true

	case *types.Var:
		if object.IsField() || !isPackageLevel(object) {
			// A struct field has no node in this graph (extract.go models
			// declarations, not members), and a local has none by design.
			return "", "", false, false
		}
		return container, name, false, true

	case *types.Const:
		if !isPackageLevel(object) {
			return "", "", false, false
		}
		return container, name, false, true
	}
	return "", "", false, false
}

// isPackageLevel reports whether an object is declared in its package's own
// scope rather than inside a function body.
func isPackageLevel(obj types.Object) bool {
	return obj.Pkg() != nil && obj.Parent() == obj.Pkg().Scope()
}

// receiverName is the type name a method's qualifiedName is built from:
// pointer-ness dropped (`*Server` and `Server` are one method set as far as
// any caller's question goes, and Go forbids declaring both), and a generic
// instantiation reduced to its origin (`Stack[int]` -> `Stack`), matching
// extract.go's receiverTypeName exactly.
func receiverName(t types.Type) string {
	if pointer, ok := t.(*types.Pointer); ok {
		t = pointer.Elem()
	}
	named, ok := t.(*types.Named)
	if !ok {
		return ""
	}
	if origin := named.Origin(); origin != nil {
		named = origin
	}
	if named.Obj() == nil {
		return ""
	}
	return named.Obj().Name()
}

// --- interfaces ---------------------------------------------------------

// answerImplements emits the `SUPERTYPE_OF type -> interface` edges nothing
// structural can: Go's interfaces are satisfied implicitly, so a type's
// syntax never says which ones it implements. `types.Implements` does.
//
// Both the value and the pointer method set are tried, because
// `func (s *Server) Close()` makes `*Server` satisfy `Closer` while `Server`
// does not - and a caller asking "what implements Closer" means `Server`,
// the declaration, which is the only thing this index has a node for.
//
// The pairing is quadratic in principle (every project type against every
// project interface). In practice the method-name pre-check below rejects
// almost every pair before `types.Implements` has to build a method set,
// which is what keeps it from dominating the pass - see the measurement in
// this task's report.
func (e *semanticEngine) answerImplements(
	builder *semanticDiff,
	resolved *resolutions,
	structural map[string]structuralFile,
) int {
	if len(resolved.interfaces) == 0 || len(resolved.namedTypes) == 0 {
		return 0
	}

	type candidate struct {
		obj     *types.TypeName
		named   *types.Named
		methods map[string]bool
	}
	types_ := make([]candidate, 0, len(resolved.namedTypes))
	for obj, named := range resolved.namedTypes {
		types_ = append(types_, candidate{obj: obj, named: named, methods: methodNames(named)})
	}
	sort.Slice(types_, func(i, j int) bool { return objectLess(types_[i].obj, types_[j].obj) })

	interfaces := make([]*types.TypeName, 0, len(resolved.interfaces))
	for obj := range resolved.interfaces {
		interfaces = append(interfaces, obj)
	}
	sort.Slice(interfaces, func(i, j int) bool { return objectLess(interfaces[i], interfaces[j]) })

	emitted := 0
	for _, subtype := range types_ {
		file, node, ok := declarationNode(resolved, structural, subtype.obj)
		if !ok {
			continue
		}
		for _, ifaceObj := range interfaces {
			// Pointer identity is not enough: `Tests: true` type-checks a
			// package's test variant (`render [render.test]`) separately
			// from its production variant, so the same `type Render
			// interface { ... }` declaration surfaces as two distinct
			// *types.TypeName objects - one reached via namedTypes, the
			// other via interfaces. Both trivially satisfy
			// types.Implements against each other (same method set), and
			// the placeholder edge target resolves by package path + name
			// back to the one real node, so a pointer-only guard here lets
			// a self edge through. Package path + name is what a Go
			// declaration actually is; two TypeName objects that agree on
			// both name the same declaration no matter which type-checking
			// pass produced them.
			if ifaceObj.Pkg().Path() == subtype.obj.Pkg().Path() && ifaceObj.Name() == subtype.obj.Name() {
				continue
			}
			iface, _ := resolved.interfaces[ifaceObj].Underlying().(*types.Interface)
			if iface == nil || !hasAllMethods(subtype.methods, iface) {
				continue
			}
			if !types.Implements(subtype.named, iface) &&
				!types.Implements(types.NewPointer(subtype.named), iface) {
				continue
			}
			toID := builder.placeholder(file, node.Container, ifaceObj.Pkg().Path(), ifaceObj.Name(),
				node.Range.Start, ifaceObj.Name())
			builder.edge(file, node.ID, edgeKindSupertypeOf, toID)
			emitted++
		}
	}
	return emitted
}

// declarationNode finds the structural node a named type's declaration
// produced, which is what a SUPERTYPE_OF edge is written from. A type whose
// file this plugin never walked (gitignored, or under an excluded directory)
// has no node, and an edge from an id nothing declares is worse than no edge.
func declarationNode(
	resolved *resolutions,
	structural map[string]structuralFile,
	obj *types.TypeName,
) (string, wireNode, bool) {
	relPath, ok := resolved.declaredIn[obj]
	if !ok {
		return "", wireNode{}, false
	}
	file, ok := structural[relPath]
	if !ok {
		return "", wireNode{}, false
	}
	node, ok := file.typeNodes[obj.Name()]
	if !ok {
		return "", wireNode{}, false
	}
	return relPath, node, true
}

// methodNames is every method reachable on a type or on a pointer to it,
// promotions included - the cheap half of the implements check.
func methodNames(named *types.Named) map[string]bool {
	out := map[string]bool{}
	if iface, ok := named.Underlying().(*types.Interface); ok {
		for i := 0; i < iface.NumMethods(); i++ {
			out[iface.Method(i).Name()] = true
		}
		return out
	}
	set := types.NewMethodSet(types.NewPointer(named))
	for i := 0; i < set.Len(); i++ {
		out[set.At(i).Obj().Name()] = true
	}
	return out
}

func hasAllMethods(have map[string]bool, iface *types.Interface) bool {
	for i := 0; i < iface.NumMethods(); i++ {
		if !have[iface.Method(i).Name()] {
			return false
		}
	}
	return true
}

func objectLess(a, b *types.TypeName) bool {
	if a.Pkg().Path() != b.Pkg().Path() {
		return a.Pkg().Path() < b.Pkg().Path()
	}
	return a.Name() < b.Name()
}

// --- the diff -----------------------------------------------------------

// semanticDiff accumulates one pass's answer, deduplicated by id.
type semanticDiff struct {
	nodes map[string]wireNode
	edges map[string]wireEdge
	// edgesByFile is what a later pass of the same file retracts from - see
	// semanticEngine.emittedEdges.
	edgesByFile map[string][]string
	retracted   map[string]bool
}

func newSemanticDiff() *semanticDiff {
	return &semanticDiff{
		nodes:       map[string]wireNode{},
		edges:       map[string]wireEdge{},
		edgesByFile: map[string][]string{},
		retracted:   map[string]bool{},
	}
}

// placeholder adds (once) the qualifiedName-keyed placeholder that addresses
// one declaration, and returns its node id.
//
// The node's own `qualifiedName` is `<container>.<declaration>`, which is a
// display string and nothing more: core reads the address off
// `placeholder_targets`, never off this field (symbol_links.rs, "The address
// is a row, not a string"). It is spelled that way because it is the one
// rendering a human reading a diff can act on.
func (d *semanticDiff) placeholder(
	relPath, fromContainer, container, qualifiedName string,
	at wirePosition,
	displayName string,
) string {
	address := container + "." + qualifiedName
	id := nodeIDFor(relPath, nodeKindModule, address, pendingSymbolNativeKind)
	if _, exists := d.nodes[id]; exists {
		return id
	}
	d.nodes[id] = wireNode{
		ID:            id,
		Kind:          nodeKindModule,
		Name:          displayName,
		QualifiedName: address,
		FilePath:      relPath,
		Range: wireRange{
			Start: at,
			End:   wirePosition{Line: at.Line, Col: at.Col + len(displayName)},
		},
		Visibility: fileVisibility(),
		Language:   languageName,
		NativeKind: pendingSymbolNativeKind,
		Target: &placeholderTarget{
			Scope:         targetScope{Container: container},
			Key:           targetKey{QualifiedName: qualifiedName},
			FromContainer: fromContainer,
		},
	}
	return id
}

func (d *semanticDiff) edge(relPath, fromID, kind, toID string) {
	if fromID == "" || toID == "" {
		return
	}
	id := edgeIDFor(fromID, kind, toID, nil)
	if _, exists := d.edges[id]; exists {
		return
	}
	d.edges[id] = wireEdge{
		ID:     id,
		FromID: fromID,
		ToID:   toID,
		Kind:   kind,
		Source: sourceTierSemantic,
		Engine: engineNameSemantic,
		// Every edge this pass emits lands on a placeholder, so nothing is
		// confirmed until core links it - the same rule the structural tier
		// applies (extract.go's addEdge): `resolved` describes what the edge
		// points at, never who produced it.
		Resolved: false,
	}
	d.edgesByFile[relPath] = append(d.edgesByFile[relPath], id)
}

func (d *semanticDiff) retract(relPath, edgeID string) {
	_ = relPath
	if edgeID != "" {
		d.retracted[edgeID] = true
	}
}

// finish renders the accumulated answer as a wire diff and records what was
// emitted per file, so the next pass over the same file can retract whatever
// it no longer produces.
func (d *semanticDiff) finish(e *semanticEngine, scope []string) fileChangeDiff {
	diff := emptyDiff()

	for _, id := range sortedKeys(d.nodes) {
		diff.UpsertNodes = append(diff.UpsertNodes, d.nodes[id])
	}
	for _, id := range sortedKeys(d.edges) {
		diff.UpsertEdges = append(diff.UpsertEdges, d.edges[id])
	}

	// A file this pass covered keeps only the semantic edges it produced
	// this time; anything this process emitted for it before and did not
	// produce again describes code that is no longer there.
	for _, relPath := range scope {
		still := map[string]bool{}
		for _, id := range d.edgesByFile[relPath] {
			still[id] = true
		}
		for _, id := range e.emittedEdges[relPath] {
			if !still[id] {
				d.retracted[id] = true
			}
		}
		e.emittedEdges[relPath] = d.edgesByFile[relPath]
	}
	for _, id := range sortedKeys(d.retracted) {
		if _, kept := d.edges[id]; !kept {
			diff.DeleteEdgeIds = append(diff.DeleteEdgeIds, id)
		}
	}
	return diff
}

// sortedKeys is the determinism guard every map in this file needs: Go
// randomizes map iteration, and a diff whose lines shuffle between two
// otherwise identical runs is one no one can compare by eye - and one whose
// tests would depend on the runtime's hash seed.
func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for key := range m {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	return keys
}
