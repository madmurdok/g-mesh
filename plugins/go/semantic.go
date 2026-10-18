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
// package's container (or, for a build-tag twin, the declaring file - see
// twinnedNames), plus one `semantic` / `go-types` edge onto it:
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
// # What `go list` may run
//
// Nothing the module chooses: `go list` runs with GOTOOLCHAIN=local, so a
// `go`/`toolchain` line newer than the installed Go fails to load (and
// degrades below) instead of downloading and running another toolchain, and
// with GOPACKAGESDRIVER=off, so no driver binary on PATH replaces it. cgo
// stays on: it runs the user's own `cc`/`pkg-config`. See [loadEnv] and
// docs/adr/0032-semantic-tiers-execute-no-project-code.md.
//
// # What `go list` may touch
//
// No network and nothing under the project root: GOPROXY=off with a
// GONOPROXY that matches nothing, so a module missing from GOMODCACHE fails
// to load instead of being downloaded, private or not, GOSUMDB=off, so no
// checksum is looked up, and
// `-mod=readonly` (or `-mod=vendor` for a vendored module or workspace) as a
// build flag, so `go list` never rewrites go.mod or go.sum. One exception go
// itself makes: in workspace mode, when a member's go.sum lacks entries for a
// module in GOMODCACHE, go adds them to go.work.sum beside go.work, under
// `-mod=readonly` too. The build flag
// overrides only `-mod` in the user's GOFLAGS; the rest of it still applies.
// Imports of a module that is not in the cache do not type-check; local code
// still resolves. See [loadEnv], [modFlag] and
// docs/adr/0033-semantic-tiers-reach-no-network-and-write-nothing.md.
//
// # Degradation
//
// No `go` on PATH, or a `packages.Load` that fails outright: log once and
// answer with `incomplete: true` and an `incompleteReason` naming what did
// not load (wire/src/lib.rs's `FileChangeResponse::incomplete`) - with an
// empty diff when nothing loaded, and with what the other modules resolved
// when only some failed (GM-442). That field is what makes the rest of
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
//   - **Which of two build-tag twins a name means, beyond the host's
//     choice.** When a package declares one name in two files that the build
//     constraints keep apart (`binding.go` / `binding_nomsgpack.go`), a
//     container-scoped address matches both and core refuses it as
//     ambiguous. Such a *twinned* name (see twinnedNames) is addressed by
//     the file the host build declares it in instead, so its answer lands on
//     the twin this host compiles - which twin that is depends on the host
//     and on GOFLAGS tags, exactly as the rest of this pass does (GM-570).
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
	"encoding/json"
	"fmt"
	"go/ast"
	"go/parser"
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
		// over, as the other plugins treat their own.
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

	// supertypes is, per file, what each SUPERTYPE_OF edge in emittedEdges
	// was produced from, so a pass that could not re-evaluate the edge's
	// interface (its package was neither folded nor imported this pass)
	// carries the edge forward instead of retracting it (GM-590).
	supertypes map[string]map[string]supertypeRecord

	// recheckedImplementors is the files outside the requested scope the
	// last per-file pass re-checked because an interface one of their
	// SUPERTYPE_OF edges points at changed or went away (GM-620), nil when
	// that pass needed no second load. Read by tests; the daemon only logs it.
	recheckedImplementors []string

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
		supertypes:   map[string]map[string]supertypeRecord{},
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
// three things a resolution needs: the open questions, the uses onto a
// placeholder that go/types may correct (a conversion) or sharpen (a twinned
// name), and the node ids an answer can be written from.
type structuralFile struct {
	container       string
	openSites       []openSite
	placeholderUses []placeholderUse
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
			container:       graph.container,
			openSites:       graph.openSites,
			placeholderUses: graph.placeholderUses,
			typeNodes:       map[string]wireNode{},
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
	// loadOf is the load (the absolute directory `packages.Load` ran from)
	// each object of namedTypes and interfaces was folded by, whether from
	// source or from export data. Every object in either map has an entry.
	// answerImplements pairs only objects of one load: across two loads
	// types.Implements matches only predeclared signatures, and no build
	// sees both declarations.
	loadOf map[*types.TypeName]string
	// views is folded, importedInterfaces and importedPackages per load, so
	// carryForward can tell whether the load holding a subtype evaluated an
	// interface.
	views map[string]*loadView
	// declaredIn is the project-relative file each of those - and each
	// project object an answer in `uses` or `selections` resolves to - was
	// declared in. It is recorded while folding, not derived later, because
	// a `types.Object` carries a `token.Pos` that only means something in
	// the `token.FileSet` it was type-checked with - and this pass creates
	// one per `packages.Load` call.
	declaredIn map[types.Object]string

	// twinned is, per package path, the qualifiedName spellings the package
	// declares in a file the host build loads *and* in one it excludes (see
	// twinnedNames), unioned across every copy of the path (a directory of
	// each module the path can resolve to) and every module view. Only
	// these are addressed by file (GM-570).
	twinned map[string]map[string]bool
	// twinsKnown is every package path that some load of this pass folded or
	// listed, so loadReferencedTwins does not list a path again from the
	// referencing module.
	twinsKnown map[string]bool
	// twinCopies is every copy (copyKey: package path and directory) whose
	// twins are already unioned into twinned and builtFiles, so neither a
	// package's test variants nor a second load or listing re-parse its
	// files.
	twinCopies map[string]bool
	// builtFiles is, per twinned package path, the project-relative files
	// of the package's GoFiles - the ones the host build compiles - unioned
	// across copies and module views like twinned. A twinned answer is addressed by
	// file only when its declaring file is in this set: membership is what
	// "a file of this project the host builds" means, whatever the path's
	// spelling (a `//line` directive can name a template, a phantom path or
	// a Windows path outside the root).
	builtFiles map[string]map[string]bool
	// referenced is every project package path an answer of the load in
	// progress resolved into (loadFor starts a fresh set per module). A
	// package this pass did not fold itself (a per-file pass's imports)
	// still needs its twins, or an edit to the importing file would flip
	// its answers back to the ambiguous container address.
	referenced map[string]bool

	// loadView is the union over every load of the pass:
	// staleImplementorFiles and listUncoveredCopies read it pass-wide.
	loadView
	// importsWalked is every imported *types.Package already walked. Keyed by
	// object, not path: each module's load has its own copy of a package,
	// and a type is checked against the copy its own load imported.
	importsWalked map[*types.Package]bool
	// exhaustive is a whole-project pass in which every module loaded: every
	// project interface was evaluated, so nothing is carried forward, and
	// every referenced project package was folded, so no package's twins had
	// to be listed. Set by [semanticEngine.loadFor].
	exhaustive bool
}

// loadView is what one load (or, embedded in resolutions, the whole pass)
// evaluated of the project's interfaces.
type loadView struct {
	// folded is every project package path whose own source was
	// type-checked: its interfaces are all in `interfaces`, so an edge onto
	// one that is missing describes an interface that is gone.
	folded map[string]bool
	// importedInterfaces is every `<package path>.<name>` interface added
	// from an imported project package's export data (foldImportedInterfaces):
	// re-evaluated exactly, never carried forward.
	importedInterfaces map[string]bool
	// importedPackages is every project package path walked from its own,
	// complete export data. That holds every exported declaration, so an
	// exported interface of one of these that is not in importedInterfaces
	// is gone (an unexported one may simply not have been exported).
	importedPackages map[string]bool
}

func newLoadView() *loadView {
	return &loadView{
		folded:             map[string]bool{},
		importedInterfaces: map[string]bool{},
		importedPackages:   map[string]bool{},
	}
}

func newResolutions() *resolutions {
	return &resolutions{
		selections: map[sitePos]*types.Selection{},
		uses:       map[sitePos]types.Object{},
		namedTypes: map[*types.TypeName]*types.Named{},
		interfaces: map[*types.TypeName]*types.Named{},
		loadOf:     map[*types.TypeName]string{},
		views:      map[string]*loadView{},
		declaredIn: map[types.Object]string{},
		twinned:    map[string]map[string]bool{},
		twinsKnown: map[string]bool{},
		twinCopies: map[string]bool{},
		builtFiles: map[string]map[string]bool{},
		referenced: map[string]bool{},

		loadView:      *newLoadView(),
		importsWalked: map[*types.Package]bool{},
	}
}

// view is the loadView of one load, created on first use.
func (r *resolutions) view(load string) *loadView {
	v := r.views[load]
	if v == nil {
		v = newLoadView()
		r.views[load] = v
	}
	return v
}

// load runs `go list` + the type checker over `patterns` from `dir` and folds
// the result into `into`. Returns an error only for a driver-level failure -
// individual packages that do not compile are kept, because a file with a
// broken sibling still has exact type information for everything the checker
// did get through, and refusing the whole pass over one bad package is how a
// semantic tier becomes useless in exactly the repositories that need it.
func (e *semanticEngine) load(into *resolutions, dir string, patterns []string) (*packages.Config, error) {
	fset := token.NewFileSet()
	cfg := loadConfig(dir, loadMode)
	cfg.Tests = true
	cfg.Fset = fset

	// The engine is starting *now* - see markSemanticEngineStarted.
	markSemanticEngineStarted()

	loaded, err := packages.Load(cfg, patterns...)
	if err != nil {
		e.degrade("semantic pass degraded: packages.Load in %s failed: %v - "+
			"answering incomplete, with whatever the other modules resolved", dir, err)
		return nil, err
	}
	for _, pkg := range loaded {
		e.foldPackage(into, fset, pkg, dir)
	}
	return cfg, nil
}

// loadConfig is the `go list` configuration a load from dir runs with: its
// directory, loadEnv and its `-mod` flag. A listing of a module's packages
// uses it too, so it resolves import paths and evaluates build constraints
// exactly as that module's load does.
func loadConfig(dir string, mode packages.LoadMode) *packages.Config {
	env := loadEnv()
	cfg := &packages.Config{Mode: mode, Dir: dir, Env: env}
	if flag := modFlag(dir, env); flag != "" {
		cfg.BuildFlags = []string{flag}
	}
	return cfg
}

// twinListing is the project packages one load configuration's answers
// resolved into, to be listed for twins with that same configuration.
type twinListing struct {
	cfg   *packages.Config
	paths map[string]bool
}

// twinListingKey identifies a load configuration by what decides how
// `go list` resolves an import path and evaluates build constraints.
func twinListingKey(cfg *packages.Config) string {
	return strings.Join([]string{
		cfg.Dir,
		strings.Join(cfg.Env, "\x01"),
		strings.Join(cfg.BuildFlags, "\x01"),
	}, "\x00")
}

// loadReferencedTwins computes the twinned names of every project package an
// answer resolved into but no load has folded - in a per-file pass, or a
// whole-project pass with a failed module, the imported packages, which the
// type checker read from export data and go/packages therefore reports
// without their IgnoredFiles. It asks `go list`
// for names and files only (no type checking), once per load configuration,
// from the directory and with the flags of the load whose answers referenced
// the package: an import path resolves only in a module that requires it, and
// build constraints are evaluated the same way as in that load.
//
// What is missing is decided for every group before any listing runs, so a
// package one module lists is still listed by every other module that
// referenced it. A name twinned in any module's view is twinned: its
// container address is ambiguous there. A failed listing leaves its packages
// without twins from it, which keeps their answers container-scoped.
//
// Then every other copy of each path the pass depends on is listed from its
// own module (listUncoveredCopies), so the twins are the union over every
// copy of the path, not only the copies the referencing modules resolve to.
// configs is each loaded module's load configuration and failed each module
// that did not load, both keyed by project-relative module directory.
func (e *semanticEngine) loadReferencedTwins(
	ws *workspace,
	into *resolutions,
	listings map[string]*twinListing,
	configs map[string]*packages.Config,
	failed map[string]error,
) {
	missingBy := map[string][]string{}
	for key, l := range listings {
		for path := range l.paths {
			if !into.twinsKnown[path] {
				missingBy[key] = append(missingBy[key], path)
			}
		}
	}
	for _, key := range sortedKeys(missingBy) {
		missing := missingBy[key]
		sort.Strings(missing)
		base := listings[key].cfg
		cfg := &packages.Config{
			Mode:       packages.NeedName | packages.NeedFiles,
			Dir:        base.Dir,
			Env:        base.Env,
			BuildFlags: base.BuildFlags,
		}
		listed, err := packages.Load(cfg, missing...)
		if err != nil {
			logf("semantic pass: listing %d imported package(s) from %s for build-tag twins failed: %v",
				len(missing), base.Dir, err)
			continue
		}
		wanted := map[string]bool{}
		for _, path := range missing {
			wanted[path] = true
		}
		for _, pkg := range listed {
			if wanted[pkg.PkgPath] && !into.twinCopies[e.twinCopyKey(pkg)] {
				e.unionTwins(into, pkg)
			}
		}
	}
	for _, missing := range missingBy {
		for _, path := range missing {
			into.twinsKnown[path] = true
		}
	}
	e.listUncoveredCopies(ws, into, listings, configs, failed)
}

// listUncoveredCopies lists, for their twins, the copies of the paths this
// pass depends on that no fold or listing of the pass has covered. The
// dependent paths are every path an answer resolved into and every path the
// pass folded (an implements answer addresses a folded interface by its
// path even when no call answer did). Each copy is listed from its own
// module, with that module's load configuration, all of one module's copies
// in one `go list`; a listed package counts only when it resolved to the
// expected directory.
//
// Skipped: external test packages (`go list` cannot list them by import
// path, and an answer reaches one only from its own files, which the pass
// folded), copies of a module that failed to load in this pass (the listing
// would fail too), and copy directories without a .go file. A failed listing
// is logged and leaves its copies' names out.
func (e *semanticEngine) listUncoveredCopies(
	ws *workspace,
	into *resolutions,
	listings map[string]*twinListing,
	configs map[string]*packages.Config,
	failed map[string]error,
) {
	if ws == nil {
		return
	}
	dependent := map[string]bool{}
	for _, l := range listings {
		for path := range l.paths {
			dependent[path] = true
		}
	}
	for path := range into.folded {
		dependent[path] = true
	}
	// Module directory -> package path -> the copy's directory.
	uncovered := map[string]map[string]string{}
	for _, path := range sortedKeys(dependent) {
		if strings.HasSuffix(path, "_test") {
			continue
		}
		for _, c := range ws.packageCopies(path) {
			if into.twinCopies[copyKey(path, c.dir)] {
				continue
			}
			if _, bad := failed[c.module]; bad {
				continue
			}
			if !hasGoFile(filepath.Join(e.realRoot, filepath.FromSlash(c.dir))) {
				continue
			}
			if uncovered[c.module] == nil {
				uncovered[c.module] = map[string]string{}
			}
			uncovered[c.module][path] = c.dir
		}
	}
	for _, module := range sortedKeys(uncovered) {
		copies := uncovered[module]
		paths := sortedKeys(copies)
		var cfg *packages.Config
		if base := configs[module]; base != nil {
			cfg = &packages.Config{Dir: base.Dir, Env: base.Env, BuildFlags: base.BuildFlags}
		} else {
			cfg = loadConfig(filepath.Join(e.realRoot, filepath.FromSlash(module)), 0)
		}
		cfg.Mode = packages.NeedName | packages.NeedFiles
		listed, err := packages.Load(cfg, paths...)
		if err != nil {
			logf("semantic pass: listing %d other module cop(ies) from %s for build-tag twins failed: %v",
				len(paths), cfg.Dir, err)
			continue
		}
		for _, pkg := range listed {
			dir, ok := copies[pkg.PkgPath]
			if !ok {
				continue
			}
			key := e.twinCopyKey(pkg)
			if key != copyKey(pkg.PkgPath, dir) || into.twinCopies[key] {
				continue
			}
			e.unionTwins(into, pkg)
		}
	}
}

// hasGoFile reports whether dir holds at least one .go file.
func hasGoFile(dir string) bool {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return false
	}
	for _, entry := range entries {
		if !entry.IsDir() && strings.HasSuffix(entry.Name(), ".go") {
			return true
		}
	}
	return false
}

// loadEnv is the environment `go list` runs with: the inherited one, with
// the Go toolchain pinned to the installed one, no packages driver and no
// module proxy (so nothing is downloaded). Later entries win, so these
// override the user's own settings.
//
// GOPROXY=off covers only modules fetched through the proxy list: a module
// matched by GONOPROXY (which defaults to GOPRIVATE) is fetched directly from
// its host whatever GOPROXY says. GONOPROXY is therefore set to "-", a
// pattern no module path matches (a path never begins with '-').
//
// GOSUMDB=off: go still looks up checksums it records for modules already in
// GOMODCACHE (go.work.sum in workspace mode), and that lookup goes to the
// checksum database directly, not through GOPROXY. With it off, the hashes
// recorded in go.work.sum come from the module cache unchecked against the
// checksum database; nothing is downloaded.
func loadEnv() []string {
	return append(os.Environ(),
		"GOTOOLCHAIN=local", "GOPACKAGESDRIVER=off", "GOPROXY=off", "GONOPROXY=-",
		"GOSUMDB=off")
}

// modFlag is the `-mod` build flag `go list` runs with from dir under env,
// so it never writes go.mod or go.sum: "-mod=vendor" when the governing
// go.work (else go.mod) has a vendor/modules.txt beside it, "-mod=readonly"
// otherwise, and "" when dir is not in module mode or has no go.mod or
// go.work above it, where go rejects the flag and writes neither file anyway.
//
// The governing files are the ones go itself resolves (`go env GOWORK
// GOMOD`, run from dir under the load's own env), so every source of go's
// settings counts: the process environment, the go env file (GOENV,
// `go env -w`) and go's defaults. When go cannot answer, the answer is
// "-mod=readonly", the flag that never writes.
func modFlag(dir string, env []string) string {
	cmd := exec.Command("go", "env", "-json", "GOWORK", "GOMOD")
	cmd.Dir = dir
	cmd.Env = env
	out, err := cmd.Output()
	if err != nil {
		return "-mod=readonly"
	}
	var resolved struct{ GOWORK, GOMOD string }
	if err := json.Unmarshal(out, &resolved); err != nil {
		return "-mod=readonly"
	}
	return modFlagFor(resolved.GOWORK, resolved.GOMOD)
}

// modFlagFor is [modFlag]'s answer for go's resolved GOWORK and GOMOD:
// GOWORK is the workspace file in effect ("" or "off" for none), GOMOD the
// main module's go.mod ("" outside module mode, os.DevNull in module mode
// with no go.mod).
func modFlagFor(gowork, gomod string) string {
	if gomod == "" {
		return ""
	}
	governing := gowork
	if governing == "off" {
		governing = ""
	}
	if governing == "" && gomod != os.DevNull {
		governing = gomod
	}
	if governing == "" {
		return ""
	}
	if info, err := os.Stat(filepath.Join(filepath.Dir(governing), "vendor", "modules.txt")); err == nil && info.Mode().IsRegular() {
		return "-mod=vendor"
	}
	return "-mod=readonly"
}

// foldPackage indexes one loaded package's type information by position and
// collects its named types.
// foldPackage folds one root package of the load `load` into `into`.
func (e *semanticEngine) foldPackage(into *resolutions, fset *token.FileSet, pkg *packages.Package, load string) {
	if pkg.TypesInfo != nil {
		for expr, selection := range pkg.TypesInfo.Selections {
			if expr.Sel == nil {
				continue
			}
			if key, ok := e.positionOf(fset, expr.Sel.Pos()); ok {
				into.selections[key] = selection
				e.recordDeclaration(into, fset, selection.Obj())
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
					e.recordDeclaration(into, fset, obj)
				}
			}
		}
	}
	if !e.isProjectContainer(pkg.PkgPath) {
		return
	}
	e.collectTwins(into, pkg)
	if pkg.Types == nil {
		return
	}
	into.folded[pkg.PkgPath] = true
	into.view(load).folded[pkg.PkgPath] = true
	e.foldImportedInterfaces(into, fset, pkg.Types, load)
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
		into.loadOf[typeName] = load
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

// foldImportedInterfaces adds, from export data, the interfaces and the
// candidate implementors of every project package `from` imports,
// transitively, tagged with the importing load. A per-file pass folds only
// the file's own package; without the interfaces a type would never be
// checked against an imported interface, so the SUPERTYPE_OF edge a
// whole-project pass gave it would be retracted. Without the types, a
// consumer module that imports both an interface's package and an
// implementor's package (neither importing the other) would never pair them,
// although its build can assign one to the other. The export-data objects
// share the importing package's universe, so types.Implements answers
// exactly. The walk stops at non-project packages: a dependency cannot
// import back into the project.
//
// Only exported types are folded: export data holds every exported
// declaration, but an unexported one only when the exported API reaches it,
// so folding those would make the answer depend on the export-data encoding.
// A type with no methods satisfies no folded interface and is skipped.
func (e *semanticEngine) foldImportedInterfaces(into *resolutions, fset *token.FileSet, from *types.Package, load string) {
	view := into.view(load)
	for _, imp := range from.Imports() {
		path := imp.Path()
		if into.importsWalked[imp] || !e.isProjectContainer(path) {
			continue
		}
		into.importsWalked[imp] = true
		// A package reached only through another package's export data is
		// partial: its scope holds just the objects that export data
		// references, so a missing interface of it is not gone.
		if imp.Complete() {
			into.importedPackages[path] = true
			view.importedPackages[path] = true
		}
		// Its twins are listed like any other package an answer resolved
		// into, so targetScopeOf can address a twinned interface by file.
		into.referenced[path] = true
		scope := imp.Scope()
		for _, name := range scope.Names() {
			typeName, ok := scope.Lookup(name).(*types.TypeName)
			if !ok || typeName.IsAlias() {
				continue
			}
			named, ok := typeName.Type().(*types.Named)
			if !ok || named.TypeParams().Len() > 0 {
				continue
			}
			if _, folded := into.namedTypes[typeName]; !folded && typeName.Exported() && len(methodNames(named)) > 0 {
				if at, ok := e.positionOf(fset, typeName.Pos()); ok {
					into.namedTypes[typeName] = named
					into.loadOf[typeName] = load
					into.declaredIn[typeName] = at.file
				}
			}
			iface, ok := named.Underlying().(*types.Interface)
			if !ok || iface.NumMethods() == 0 || !iface.IsMethodSet() {
				continue
			}
			into.interfaces[typeName] = named
			into.loadOf[typeName] = load
			into.importedInterfaces[path+"."+name] = true
			view.importedInterfaces[path+"."+name] = true
			if _, done := into.declaredIn[typeName]; !done {
				if at, ok := e.positionOf(fset, typeName.Pos()); ok {
					into.declaredIn[typeName] = at.file
				}
			}
		}
		e.foldImportedInterfaces(into, fset, imp, load)
	}
}

// recordDeclaration records the project file a project object an answer
// resolved to is declared in, and that its package was referenced. Objects
// outside the project are skipped: addressOf never addresses them.
func (e *semanticEngine) recordDeclaration(into *resolutions, fset *token.FileSet, obj types.Object) {
	if obj == nil || obj.Pkg() == nil {
		return
	}
	path := obj.Pkg().Path()
	if !e.isProjectContainer(path) {
		return
	}
	into.referenced[path] = true
	if _, done := into.declaredIn[obj]; done {
		return
	}
	if at, ok := e.positionOf(fset, obj.Pos()); ok {
		into.declaredIn[obj] = at.file
	}
}

// collectTwins computes (once per copy of a package path) the twinned names
// of one folded project package from its loaded and its build-excluded
// files, and unions them with the other copies' (unionTwins).
func (e *semanticEngine) collectTwins(into *resolutions, pkg *packages.Package) {
	if pkg.PkgPath == "" {
		return
	}
	into.twinsKnown[pkg.PkgPath] = true
	if into.twinCopies[e.twinCopyKey(pkg)] {
		return
	}
	e.unionTwins(into, pkg)
}

// unionTwins marks pkg's copy as covered and adds its twinned names to
// into.twinned and, when it has any, its built files to into.builtFiles.
func (e *semanticEngine) unionTwins(into *resolutions, pkg *packages.Package) {
	into.twinCopies[e.twinCopyKey(pkg)] = true
	names := twinnedNames(pkg.Name, pkg.GoFiles, pkg.IgnoredFiles)
	if len(names) == 0 {
		return
	}
	if into.twinned[pkg.PkgPath] == nil {
		into.twinned[pkg.PkgPath] = map[string]bool{}
	}
	for name := range names {
		into.twinned[pkg.PkgPath][name] = true
	}
	e.recordBuiltFiles(into, pkg)
}

// copyKey identifies one copy of a package path: the path and the copy's
// directory, project-relative ("" for the root) or, outside the project,
// absolute.
func copyKey(path, dir string) string {
	return path + "\x00" + dir
}

// twinCopyKey is the copyKey of a loaded or listed package: its directory
// is pkg.Dir, else the directory of its first file. The directory is spelled
// project-relative through relPathOf, so /var and /private/var are one key.
func (e *semanticEngine) twinCopyKey(pkg *packages.Package) string {
	dir := pkg.Dir
	if dir == "" {
		for _, files := range [2][]string{pkg.GoFiles, pkg.IgnoredFiles} {
			if len(files) > 0 {
				dir = filepath.Dir(files[0])
				break
			}
		}
	}
	if rel, ok := e.relPathOf(dir); ok && dir != "" {
		dir = normalizeDir(rel)
	}
	return copyKey(pkg.PkgPath, dir)
}

// recordBuiltFiles adds the project-relative GoFiles of a twinned package to
// into.builtFiles; a file outside the project root is skipped.
func (e *semanticEngine) recordBuiltFiles(into *resolutions, pkg *packages.Package) {
	if into.builtFiles[pkg.PkgPath] == nil {
		into.builtFiles[pkg.PkgPath] = map[string]bool{}
	}
	for _, f := range pkg.GoFiles {
		if rel, ok := e.relPathOf(f); ok {
			into.builtFiles[pkg.PkgPath][rel] = true
		}
	}
}

// twinnedNames is the set of qualifiedName spellings (extract.go's: `F`, `T`,
// `T.M` for a method or an interface method, and each var/const name) that
// package `pkgName` declares both in a file the host build loads and in one
// its build constraints exclude - a build-tag twin (GM-570). Core cannot tell
// the two declarations apart by container and name, so an answer onto such a
// name has to say which file it means.
//
// Only twins are reported, not every name of a package with an excluded
// file: a name declared once keeps its container address, and with it the
// placeholder id it has always had.
//
// `_test.go` files are skipped on both sides: with `Tests: true` they are
// loaded rather than ignored, and a test file's names are not what a twin in
// production code collides with. An excluded file of another package (an
// `//go:build ignore` `package main` generator) declares nothing here.
func twinnedNames(pkgName string, loadedFiles, ignoredFiles []string) map[string]bool {
	excluded := spellingsOf(pkgName, ignoredFiles)
	if len(excluded) == 0 {
		return nil
	}
	loaded := spellingsOf(pkgName, loadedFiles)
	twinned := map[string]bool{}
	for name := range excluded {
		if loaded[name] {
			twinned[name] = true
		}
	}
	return twinned
}

// spellingsOf parses each non-test `.go` file of package `pkgName` and
// collects its declarations' qualifiedName spellings. A file that does not
// parse contributes what the parser recovered.
func spellingsOf(pkgName string, files []string) map[string]bool {
	out := map[string]bool{}
	fset := token.NewFileSet()
	for _, path := range files {
		if !strings.HasSuffix(path, ".go") || strings.HasSuffix(path, "_test.go") {
			continue
		}
		file, _ := parser.ParseFile(fset, path, nil, parser.SkipObjectResolution)
		if file == nil || file.Name == nil || file.Name.Name != pkgName {
			continue
		}
		declaredSpellings(file, out)
	}
	return out
}

// declaredSpellings adds a file's package-level declarations to `into`,
// spelled exactly as extract.go's declareFunc / declareType /
// declareInterfaceMethods / declareValues spell their qualifiedNames.
func declaredSpellings(file *ast.File, into map[string]bool) {
	add := func(names ...string) {
		for _, name := range names {
			if name == "" || name == "_" {
				return
			}
		}
		into[joinPath(dotPath(names...))] = true
	}
	for _, decl := range file.Decls {
		switch d := decl.(type) {
		case *ast.FuncDecl:
			if d.Name == nil {
				continue
			}
			if d.Recv != nil && len(d.Recv.List) > 0 {
				if receiver, ok := receiverTypeName(d.Recv.List[0].Type); ok {
					add(receiver, d.Name.Name)
				}
			} else if d.Name.Name != "init" {
				add(d.Name.Name)
			}
		case *ast.GenDecl:
			for _, spec := range d.Specs {
				switch s := spec.(type) {
				case *ast.TypeSpec:
					if s.Name == nil {
						continue
					}
					add(s.Name.Name)
					iface, ok := s.Type.(*ast.InterfaceType)
					if !ok || iface.Methods == nil {
						continue
					}
					for _, field := range iface.Methods.List {
						if _, ok := field.Type.(*ast.FuncType); !ok {
							continue
						}
						for _, method := range field.Names {
							if method != nil {
								add(s.Name.Name, method.Name)
							}
						}
					}
				case *ast.ValueSpec:
					for _, ident := range s.Names {
						if ident != nil {
							add(ident.Name)
						}
					}
				}
			}
		}
	}
}

// targetScopeOf is the scope an answer onto `obj`, addressed as
// `container`/`key`, is sent with: the declaring file when the name is a
// build-tag twin in its package and that file is one the host build compiles
// for that package, the package's container otherwise.
//
// The file is the one the host build declares the name in, so a twinned
// answer means "the declaration this host compiles" (GM-570). When it is not
// known - a declaration outside the project root, or a `//line` directive
// naming a non-Go source or a path the build does not compile - the
// container scope is kept: the edge stays
// ambiguous, as before, rather than being dropped.
func (r *resolutions) targetScopeOf(obj types.Object, container string, key []pathSegment) targetScope {
	if r.twinned[container][joinPath(key)] {
		if file := r.declaredIn[obj]; r.builtFiles[container][file] {
			return targetScope{File: file}
		}
	}
	return targetScope{Container: container}
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
// The second return is wire.go's `fileChangeResponse.IncompleteReason`, and
// its being non-empty is `Incomplete`: set when this pass did not resolve
// everything it was asked about (no toolchain, or any module's `go list`
// failed - GM-442), "" when it ran to completion - including the trivial
// completion of "there was nothing in scope to resolve." That distinction is
// why an empty `scope` and a failed `loadFor` are not the same return: both
// can answer an empty diff, but only the second is a pass that owed an
// answer and did not give one.
//
// When some modules load and others do not, the diff still carries what the
// loaded ones resolved; the failed modules' files are left out of the
// answered scope, so this pass neither answers nor retracts anything in them.
//
// The third return is wire.go's `fileChangeResponse.UnfinishedFiles`
// (GM-521): once at least one module loaded, the files in scope this pass
// did not answer - those of the modules that failed, possibly none - so core
// settles every other file it sent and owes only these to a later pass. nil
// (the field omitted) when nothing loaded, `go` is missing or the scope was
// empty: core then keeps its whole-pass behaviour, retrying the whole scope.
func (e *semanticEngine) run(ws *workspace, filePaths []string) (fileChangeDiff, string, []string) {
	started := time.Now()
	e.ws = ws
	defer func() { e.ws = nil }()

	if _, err := exec.LookPath("go"); err != nil {
		e.degrade("semantic pass degraded: no `go` binary on PATH (%v) - answering every "+
			"semanticPass with an empty diff, incomplete=true; the structural graph is unaffected", err)
		return emptyDiff(), "the Go semantic pass did not run: no `go` binary on PATH", nil
	}

	wholeProject := len(filePaths) == 0
	var scope []string
	if wholeProject {
		scope = walkProjectFiles(e.projectRoot)
	} else {
		scope = e.claimedFiles(filePaths)
	}
	if len(scope) == 0 {
		return emptyDiff(), "", nil
	}

	resolved := newResolutions()
	outcome := e.loadFor(ws, resolved, wholeProject, scope)
	reason := outcome.incompleteReason()
	if outcome.loaded == 0 {
		return emptyDiff(), reason, nil
	}
	// An edit that changed or removed an interface leaves the SUPERTYPE_OF
	// edges of its implementors in other files stale, and only a pass over
	// those files can retract them (GM-620). Load again with them as roots
	// too, so they are checked against the interface's new source. Gated on
	// an actual interface change: a body edit costs no second load.
	requested := scope
	e.recheckedImplementors = nil
	if !wholeProject {
		if extra := e.staleImplementorFiles(resolved, scope); len(extra) > 0 {
			e.recheckedImplementors = extra
			scope = unionSorted(scope, extra)
			resolved = newResolutions()
			outcome = e.loadFor(ws, resolved, false, scope)
			reason = outcome.incompleteReason()
			if outcome.loaded == 0 {
				return emptyDiff(), reason, nil
			}
			logf("semantic pass: an interface changed - re-checking %d implementor file(s) in %d package(s)",
				len(extra), countDirs(extra))
		}
	}
	unfinished := []string{}
	if len(outcome.failed) > 0 {
		scope, unfinished = e.withoutFailedModules(ws, scope, outcome.failed)
		// Core owes a later pass only the files it sent; an implementor file
		// added here whose module failed keeps its edges as before.
		unfinished = keepPathsIn(unfinished, requested)
	}
	structural := e.extractStructural(ws, scope)

	builder := newSemanticDiff()
	sites := 0
	for _, relPath := range sortedKeys(structural) {
		sites += e.answerFile(builder, resolved, relPath, structural[relPath])
	}
	implementsEdges := e.answerImplements(builder, resolved, structural)

	diff := builder.finish(e, scope, resolved, structural)
	logf("semantic pass: %d file(s), %d resolved site(s), %d implements edge(s), "+
		"%d node(s)/%d edge(s) upserted, %d edge(s) retracted, in %s",
		len(scope), sites, implementsEdges, len(diff.UpsertNodes), len(diff.UpsertEdges),
		len(diff.DeleteEdgeIds), time.Since(started).Round(time.Millisecond))
	if reason != "" {
		logf("semantic pass incomplete: %s (%d file(s) unfinished)", reason, len(unfinished))
	}
	return diff, reason, unfinished
}

// withoutFailedModules splits `scope` into the files whose module loaded and
// those whose module failed to. A failed module's file has no type
// information this pass could answer from, so answering it would only
// retract the semantic edges an earlier pass gave it; it is unfinished
// instead.
func (e *semanticEngine) withoutFailedModules(ws *workspace, scope []string, failed map[string]error) (kept, dropped []string) {
	kept = make([]string, 0, len(scope))
	dropped = []string{}
	for _, relPath := range scope {
		if _, bad := failed[moduleDirFor(ws, normalizeDir(filepath.ToSlash(filepath.Dir(relPath))))]; bad {
			dropped = append(dropped, relPath)
		} else {
			kept = append(kept, relPath)
		}
	}
	return kept, dropped
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
//
// The result says which module roots failed to load (GM-442). *Any* failure
// makes the pass incomplete, not only the one where every module failed: a
// whole-project pass that skipped one module and still called itself
// complete let core sweep that module's semantic edges away as stale.
func (e *semanticEngine) loadFor(ws *workspace, into *resolutions, wholeProject bool, scope []string) loadOutcome {
	byModule := map[string][]string{}
	if wholeProject {
		for _, dir := range moduleDirs(ws) {
			byModule[dir] = []string{"./..."}
		}
	} else {
		for _, relPath := range scope {
			dir := moduleDirFor(ws, normalizeDir(filepath.ToSlash(filepath.Dir(relPath))))
			abs := filepath.Join(e.realRoot, filepath.FromSlash(relPath))
			byModule[dir] = append(byModule[dir], "file="+abs)
		}
	}

	outcome := loadOutcome{failed: map[string]error{}}
	listings := map[string]*twinListing{}
	configs := map[string]*packages.Config{}
	for _, dir := range sortedKeys(byModule) {
		patterns := byModule[dir]
		sort.Strings(patterns)
		into.referenced = map[string]bool{}
		cfg, err := e.load(into, filepath.Join(e.realRoot, filepath.FromSlash(dir)), patterns)
		if err != nil {
			outcome.failed[dir] = err
			continue
		}
		outcome.loaded++
		configs[dir] = cfg
		if len(into.referenced) == 0 {
			continue
		}
		key := twinListingKey(cfg)
		l := listings[key]
		if l == nil {
			l = &twinListing{cfg: cfg, paths: map[string]bool{}}
			listings[key] = l
		}
		for path := range into.referenced {
			l.paths[path] = true
		}
	}
	// Only an exhaustive pass folds every project package itself. A per-file
	// pass, and a whole-project pass with a failed module (whose packages
	// root's answers reach through export data), list the referenced packages
	// no load folded (they come without IgnoredFiles). After all modules
	// loaded, so a listing cannot mark a later module's folded twins as
	// already known. Every other copy of a path the pass depends on is
	// listed too, from its own module, so the twins match an exhaustive
	// pass's.
	into.exhaustive = wholeProject && len(outcome.failed) == 0
	if !into.exhaustive {
		e.loadReferencedTwins(ws, into, listings, configs, outcome.failed)
	}
	return outcome
}

// loadOutcome is what [semanticEngine.loadFor] managed: how many module
// roots loaded, and the error of each one that did not, keyed by its
// project-relative directory.
type loadOutcome struct {
	loaded int
	failed map[string]error
}

// incompleteReason is the wire's `incompleteReason` for a pass with failed
// modules, or "" when every module loaded. It names every failed module -
// which is what `g-mesh status` needs to point at - but carries only the
// first one's error, so the reason does not grow with the failure.
func (o loadOutcome) incompleteReason() string {
	if len(o.failed) == 0 {
		return ""
	}
	dirs := sortedKeys(o.failed)
	names := make([]string, len(dirs))
	for i, dir := range dirs {
		names[i] = moduleLabel(dir)
	}
	return fmt.Sprintf("the Go semantic pass could not load %d module(s): %s (first: %s: %v)",
		len(dirs), strings.Join(names, ", "), moduleLabel(dirs[0]), o.failed[dirs[0]])
}

// moduleLabel is how a reason names a module root: its project-relative
// directory, with the project root itself spelled ".".
func moduleLabel(dir string) string {
	if dir == "" {
		return "."
	}
	return dir
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

// answerFile resolves one file's open sites and placeholder uses, returning
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
		container, key, isFunc, ok := e.addressOf(obj)
		if !ok {
			continue
		}
		toID := builder.placeholder(relPath, file.container, container,
			resolved.targetScopeOf(obj, container, key), key,
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

	for _, use := range file.placeholderUses {
		// `Uses`, never `Selections`: a placeholder use is always either a
		// bare name (`T(x)`) or a package-qualified one (`pkg.T(x)`), and
		// both are plain identifier uses. A selection through a value never
		// produced a structural edge in the first place - it is an open
		// site, handled above.
		obj := resolved.uses[sitePos{file: relPath, line: use.Line, col: use.Col}]
		container, key, isFunc, ok := e.addressOf(obj)
		if !ok {
			continue
		}
		scope := resolved.targetScopeOf(obj, container, key)
		at := wirePosition{Line: use.Line, Col: use.Col}
		if use.Kind == edgeKindCalls && !isFunc {
			// A conversion, not a call. Retract the CALLS edge core's kind
			// filter will never land, and say the true thing instead.
			builder.retract(relPath, use.EdgeID)
			if use.EnclosingSymbolID == "" {
				continue
			}
			toID := builder.placeholder(relPath, file.container, container, scope, key, at, use.Name)
			builder.edge(relPath, use.EnclosingSymbolID, edgeKindReferences, toID)
			answered++
			continue
		}
		if scope.File == "" {
			// The structural tier was right, and core has already linked its
			// edge (or refuses it for a reason a container-scoped answer would
			// not change). Leaving it alone is what keeps this pass's diff
			// small.
			continue
		}
		// A twinned name answered by the twin the host build compiles
		// (GM-589). The structural edge has two candidates and core refuses
		// it; the file-scoped edge links.
		//
		// Known gap, accepted (GM-589 design D2 a): if the other twin is
		// later deleted, the name stops being twinned and this branch no
		// longer fires. A whole-project pass in this same process then
		// retracts this semantic edge through `emittedEdges` (in `finish`)
		// without re-sending the structural edge retracted here, so the use
		// has no edge until this file is next re-extracted by core, which
		// restores the structural edge and now links it. The edge is
		// missing for that window, never wrong.
		builder.retract(relPath, use.EdgeID)
		toID := builder.placeholder(relPath, file.container, container, scope, key, at, use.Name)
		builder.edge(relPath, use.FromID, use.Kind, toID)
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
// and pointer receivers alike), `I.M` for an interface method. It is
// returned as the path whose joinPath is that spelling.
func (e *semanticEngine) addressOf(obj types.Object) (container string, key []pathSegment, isFunc, ok bool) {
	if obj == nil || obj.Pkg() == nil {
		// A builtin (`len`), a universe type (`error`), `nil`: no package,
		// and nothing this index declares.
		return "", nil, false, false
	}
	name := obj.Name()
	if name == "" || name == "_" {
		return "", nil, false, false
	}
	container = obj.Pkg().Path()
	if !e.isProjectContainer(container) {
		return "", nil, false, false
	}

	switch object := obj.(type) {
	case *types.Func:
		signature, _ := object.Type().(*types.Signature)
		if signature != nil && signature.Recv() != nil {
			receiver := receiverName(signature.Recv().Type())
			if receiver == "" {
				return "", nil, false, false
			}
			return container, dotPath(receiver, name), true, true
		}
		if name == "init" {
			// Not addressable in Go at all, so nothing can be referring to
			// one here; extract.go gives each its own node precisely because
			// nothing can name it.
			return "", nil, false, false
		}
		return container, dotPath(name), true, true

	case *types.TypeName:
		if !isPackageLevel(object) {
			return "", nil, false, false
		}
		return container, dotPath(name), false, true

	case *types.Var:
		if object.IsField() || !isPackageLevel(object) {
			// A struct field has no node in this graph (extract.go models
			// declarations, not members), and a local has none by design.
			return "", nil, false, false
		}
		return container, dotPath(name), false, true

	case *types.Const:
		if !isPackageLevel(object) {
			return "", nil, false, false
		}
		return container, dotPath(name), false, true
	}
	return "", nil, false, false
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
	// Equal-name objects of different loads are visited in a fixed order, so
	// the record a pair reached from several loads keeps is deterministic.
	sort.Slice(types_, func(i, j int) bool { return resolved.loadedObjectLess(types_[i].obj, types_[j].obj) })

	// A pair from two loads is never tested: no build sees both
	// declarations, and types.Implements across two loads matches only
	// predeclared signatures. So interfaces are bucketed by load and a type
	// is checked against its own load's bucket only. A pair several loads
	// see gives the same edge id from each, so the edge holds if any build
	// that sees both assigns one to the other.
	interfacesOf := map[string][]*types.TypeName{}
	for obj := range resolved.interfaces {
		load := resolved.loadOf[obj]
		interfacesOf[load] = append(interfacesOf[load], obj)
	}
	for _, bucket := range interfacesOf {
		sort.Slice(bucket, func(i, j int) bool { return objectLess(bucket[i], bucket[j]) })
	}

	emitted := 0
	for _, subtype := range types_ {
		file, node, ok := declarationNode(resolved, structural, subtype.obj)
		if !ok {
			continue
		}
		for _, ifaceObj := range interfacesOf[resolved.loadOf[subtype.obj]] {
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
			ifaceContainer, ifaceKey := ifaceObj.Pkg().Path(), dotPath(ifaceObj.Name())
			record := supertypeRecord{
				subtype:       subtype.obj.Name(),
				subtypePkg:    subtype.obj.Pkg().Path(),
				ifacePkg:      ifaceContainer,
				ifaceName:     ifaceObj.Name(),
				ifaceKey:      ifaceKey,
				ifaceScope:    resolved.targetScopeOf(ifaceObj, ifaceContainer, ifaceKey),
				ifaceMethods:  interfaceMethodNames(iface),
				ifaceSig:      interfaceSignature(iface),
				subtypeNodeID: node.ID,
				subtypeOwnPkg: node.Container,
			}
			builder.supertype(file, record, node.Range.Start)
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

func interfaceMethodNames(iface *types.Interface) []string {
	out := make([]string, 0, iface.NumMethods())
	for i := 0; i < iface.NumMethods(); i++ {
		out = append(out, iface.Method(i).Name())
	}
	return out
}

// interfaceSignature renders an interface's complete method set - embedded
// interfaces flattened, each method with its signature - as a canonical
// string that compares across loads (source or export data): two passes
// produce the same string exactly when the interface requires the same
// methods. The rendering (writeCanonicalType) resolves aliases at every depth,
// so retargeting an alias the interface uses changes the string, and drops
// parameter and result names, so renaming one does not.
func interfaceSignature(iface *types.Interface) string {
	var b strings.Builder
	newCanonicalWriter(&b).methods(iface)
	return b.String()
}

// writeCanonicalType writes t with every alias resolved, named types
// qualified by package path (and, for a function-local type, by its ordinal
// among the package's local types of that name), unexported method and field
// names qualified by their package, type parameters by index with their
// constraint, and no parameter names. Recursion stops at named types and at a
// type parameter already written, so it terminates on recursive types.
func writeCanonicalType(b *strings.Builder, t types.Type) {
	newCanonicalWriter(b).typ(t)
}

// canonicalWriter carries one rendering's state: the type parameters whose
// constraint is already written, so a constraint that mentions its own
// parameter (`T interface{ M() T }`) does not recurse forever.
type canonicalWriter struct {
	b      *strings.Builder
	params map[*types.TypeParam]bool
}

func newCanonicalWriter(b *strings.Builder) *canonicalWriter {
	return &canonicalWriter{b: b, params: map[*types.TypeParam]bool{}}
}

// methods writes an interface's method set in types.Interface order, which
// sorts by Id, and an unexported name's Id carries its package, so the order
// is the same in every load.
func (w *canonicalWriter) methods(iface *types.Interface) {
	for i := 0; i < iface.NumMethods(); i++ {
		m := iface.Method(i)
		if i > 0 {
			w.b.WriteString("; ")
		}
		w.name(m)
		w.b.WriteString(" ")
		w.typ(m.Type())
	}
}

// name writes a method or field name. An unexported one is qualified by its
// package: the spec makes non-exported names from different packages always
// different, so `interface{ m() }` from two packages are two types, and
// retargeting an embedded interface between them must change the string.
// "/" and "." cannot appear in an identifier, so no exported name renders the
// same.
func (w *canonicalWriter) name(obj types.Object) {
	if !obj.Exported() && obj.Pkg() != nil {
		w.b.WriteString(obj.Pkg().Path())
		w.b.WriteString(".")
	}
	w.b.WriteString(obj.Name())
}

func (w *canonicalWriter) typ(t types.Type) {
	b := w.b
	switch t := types.Unalias(t).(type) {
	case *types.Named:
		obj := t.Obj()
		if obj.Pkg() != nil {
			b.WriteString(obj.Pkg().Path())
			b.WriteString(".")
		}
		b.WriteString(obj.Name())
		if n := localTypeOrdinal(obj); n > 0 {
			// U+00B7 is not a letter, so no declared name contains it.
			fmt.Fprintf(b, "\u00b7%d", n)
		}
		if args := t.TypeArgs(); args.Len() > 0 {
			b.WriteString("[")
			for i := 0; i < args.Len(); i++ {
				if i > 0 {
					b.WriteString(", ")
				}
				w.typ(args.At(i))
			}
			b.WriteString("]")
		}
	case *types.TypeParam:
		fmt.Fprintf(b, "$%d", t.Index())
		if !w.params[t] {
			// The constraint is part of the parameter's meaning; written once,
			// at the first mention, in brackets ("$" never starts a type, so
			// "$0[" is never a type argument list).
			w.params[t] = true
			b.WriteString("[")
			w.typ(t.Constraint())
			b.WriteString("]")
		}
	case *types.Pointer:
		b.WriteString("*")
		w.typ(t.Elem())
	case *types.Slice:
		b.WriteString("[]")
		w.typ(t.Elem())
	case *types.Array:
		fmt.Fprintf(b, "[%d]", t.Len())
		w.typ(t.Elem())
	case *types.Map:
		b.WriteString("map[")
		w.typ(t.Key())
		b.WriteString("]")
		w.typ(t.Elem())
	case *types.Chan:
		switch t.Dir() {
		case types.SendOnly:
			b.WriteString("chan<- ")
		case types.RecvOnly:
			b.WriteString("<-chan ")
		default:
			b.WriteString("chan (")
			w.typ(t.Elem())
			b.WriteString(")")
			return
		}
		w.typ(t.Elem())
	case *types.Signature:
		b.WriteString("func(")
		w.tuple(t.Params(), t.Variadic())
		b.WriteString(") (")
		w.tuple(t.Results(), false)
		b.WriteString(")")
	case *types.Struct:
		b.WriteString("struct{")
		for i := 0; i < t.NumFields(); i++ {
			if i > 0 {
				b.WriteString("; ")
			}
			f := t.Field(i)
			if f.Embedded() {
				// "*" cannot start a field name, so no named field renders the same.
				// The name is written too: embedding an alias `A = T` names the
				// field A, and struct{ A } is not identical to struct{ T }.
				b.WriteString("*embedded ")
				w.name(f)
				b.WriteString(" ")
			} else {
				w.name(f)
				b.WriteString(" ")
			}
			w.typ(f.Type())
			if tag := t.Tag(i); tag != "" {
				fmt.Fprintf(b, " %q", tag)
			}
		}
		b.WriteString("}")
	case *types.Interface:
		b.WriteString("interface{")
		w.methods(t)
		if !t.IsMethodSet() {
			for i := 0; i < t.NumEmbeddeds(); i++ {
				b.WriteString("; ")
				w.typ(t.EmbeddedType(i))
			}
		}
		b.WriteString("}")
	case *types.Union:
		for i := 0; i < t.Len(); i++ {
			if i > 0 {
				b.WriteString(" | ")
			}
			if t.Term(i).Tilde() {
				b.WriteString("~")
			}
			w.typ(t.Term(i).Type())
		}
	default:
		// *types.Basic (and anything future): its name is canonical.
		b.WriteString(types.TypeString(t, func(p *types.Package) string { return p.Path() }))
	}
}

// tuple writes a parameter or result list as types only, keeping arity and,
// for parameters, variadic-ness.
func (w *canonicalWriter) tuple(tup *types.Tuple, variadic bool) {
	for i := 0; i < tup.Len(); i++ {
		if i > 0 {
			w.b.WriteString(", ")
		}
		t := tup.At(i).Type()
		if variadic && i == tup.Len()-1 {
			if s, ok := types.Unalias(t).(*types.Slice); ok {
				w.b.WriteString("...")
				t = s.Elem()
			}
		}
		w.typ(t)
	}
}

// localTypeOrdinal is 0 for a package-level type, and for a type declared
// inside a function its 1-based position among the package's function-local
// types of the same name, in scope-tree order (files in load order, then
// source order), so two local `type T` in one package render differently.
// The ordinal shifts only when a same-named local type is added before it,
// which costs one extra re-check. A package's test variant is deliberately
// not told apart: it compiles the same declarations, and its package-level
// names are unique across the variant's files, so path.Name already names
// one declaration - and an export-data load (always the production variant)
// must render the same as a source load of the test variant.
func localTypeOrdinal(obj *types.TypeName) int {
	pkg, parent := obj.Pkg(), obj.Parent()
	if pkg == nil || parent == nil || parent == pkg.Scope() || parent == types.Universe {
		return 0
	}
	n := 0
	var walk func(s *types.Scope) bool
	walk = func(s *types.Scope) bool {
		for i := 0; i < s.NumChildren(); i++ {
			child := s.Child(i)
			// A file scope (a child of the package scope) holds only
			// imports, a dot-imported type among them: never a local type.
			if found, ok := child.Lookup(obj.Name()).(*types.TypeName); ok && s != pkg.Scope() {
				n++
				if found == obj {
					return true
				}
			}
			if walk(child) {
				return true
			}
		}
		return false
	}
	if walk(pkg.Scope()) {
		return n
	}
	// Not reachable from the package scope (an object the importer made
	// without a scope tree): still tell it from the package-level name.
	return 1
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

// loadedObjectLess is objectLess, ties broken by the objects' loads.
func (r *resolutions) loadedObjectLess(a, b *types.TypeName) bool {
	if objectLess(a, b) || objectLess(b, a) {
		return objectLess(a, b)
	}
	return r.loadOf[a] < r.loadOf[b]
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
	// supertypes is this pass's SUPERTYPE_OF records per file and edge id -
	// see semanticEngine.supertypes.
	supertypes map[string]map[string]supertypeRecord
}

// supertypeRecord is what one SUPERTYPE_OF edge was produced from: enough to
// re-emit the edge and its placeholder in a later pass that cannot see the
// interface (GM-590), and to check the subtype can still satisfy it.
type supertypeRecord struct {
	subtype       string // the subtype's name
	subtypePkg    string // the subtype's package path
	subtypeNodeID string // the subtype's structural node, the edge's `from`
	subtypeOwnPkg string // that node's container, the placeholder's fromContainer
	ifacePkg      string
	ifaceName     string
	ifaceKey      []pathSegment
	ifaceScope    targetScope
	ifaceMethods  []string
	// ifaceSig is the interface's method set when the edge was produced
	// (interfaceSignature), so a later pass that folds the interface's
	// package can tell the interface changed and re-check this edge's file
	// (GM-620).
	ifaceSig string
}

func newSemanticDiff() *semanticDiff {
	return &semanticDiff{
		nodes:       map[string]wireNode{},
		edges:       map[string]wireEdge{},
		edgesByFile: map[string][]string{},
		retracted:   map[string]bool{},
		supertypes:  map[string]map[string]supertypeRecord{},
	}
}

// placeholder adds (once) the qualifiedName-keyed placeholder that addresses
// one declaration, and returns its node id. `key` is the declaration's path;
// the key string is its joinPath and the path itself goes out as keyPath.
//
// The node's own `qualifiedName` is `<container>.<declaration>`, which is a
// display string and nothing more: core reads the address off
// `placeholder_targets`, never off this field (symbol_links.rs, "The address
// is a row, not a string"). It is spelled that way because it is the one
// rendering a human reading a diff can act on.
//
// `scope` is where core looks the key up: the container, or - for a
// build-tag twin - the declaring file (targetScopeOf). A file-scoped
// placeholder hashes that file into its id. Without it, the id would be the
// one extract.go's placeholderFor gives a structural placeholder for the
// same name in the same file, the two tiers would write one node, and
// whichever wrote last would decide its address (GM-570). A container-scoped
// id is unchanged, so no existing placeholder or edge id moves.
func (d *semanticDiff) placeholder(
	relPath, fromContainer, container string,
	scope targetScope,
	key []pathSegment,
	at wirePosition,
	displayName string,
) string {
	qualifiedName := joinPath(key)
	address := container + "." + qualifiedName
	hashed := address
	if scope.File != "" {
		hashed = address + "@" + scope.File
	}
	id := nodeIDFor(relPath, nodeKindModule, hashed, pendingSymbolNativeKind)
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
			Scope:         scope,
			Key:           targetKey{QualifiedName: qualifiedName},
			FromContainer: fromContainer,
			KeyPath:       key,
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

// supertype emits one SUPERTYPE_OF edge (and its placeholder) from a record,
// placing the placeholder at the subtype node's current position, and records
// it for the next pass of the file. Returns the edge id.
func (d *semanticDiff) supertype(relPath string, r supertypeRecord, at wirePosition) string {
	toID := d.placeholder(relPath, r.subtypeOwnPkg, r.ifacePkg, r.ifaceScope, r.ifaceKey, at, r.ifaceName)
	d.edge(relPath, r.subtypeNodeID, edgeKindSupertypeOf, toID)
	id := edgeIDFor(r.subtypeNodeID, edgeKindSupertypeOf, toID, nil)
	if d.supertypes[relPath] == nil {
		d.supertypes[relPath] = map[string]supertypeRecord{}
	}
	d.supertypes[relPath][id] = r
	return id
}

// carryForward re-emits the prior SUPERTYPE_OF edges of the files in scope
// that this pass did not produce and could not have: no load holding the
// subtype evaluated the interface (its package was neither folded from
// source nor imported in that load), so "not produced" says nothing about
// whether the type still satisfies it. A load that holds the subtype and
// evaluated the interface answered exactly, and an edge it did not produce is
// retracted. An edge is carried only while its subtype is still a type of
// this file, with the same node, and still has every method the interface
// has: its current methods when some load of the pass evaluated it (the
// edge is then retracted if the interface is gone), else the ones recorded.
// A carried record takes the current interface's signature, so a later pass
// of the interface's file does not re-check it again. The edge is re-upserted
// rather than merely not retracted: its placeholder lives in this file, and
// core retires a placeholder a Complete reparse does not upsert.
func (d *semanticDiff) carryForward(
	e *semanticEngine,
	scope []string,
	resolved *resolutions,
	structural map[string]structuralFile,
) {
	if resolved.exhaustive {
		return
	}
	type subtypeAt struct{ pkg, name, file string }
	// subtypeLoads is every load holding one subtype, and the object whose
	// method names are checked: one folded from source when there is one,
	// the smallest load first, so the answer does not depend on map order.
	type subtypeLoads struct {
		named  *types.Named
		load   string
		source bool
		loads  []string
	}
	current := map[subtypeAt]*subtypeLoads{}
	for obj, named := range resolved.namedTypes {
		key := subtypeAt{obj.Pkg().Path(), obj.Name(), resolved.declaredIn[obj]}
		load := resolved.loadOf[obj]
		source := resolved.view(load).folded[obj.Pkg().Path()]
		at := current[key]
		if at == nil {
			at = &subtypeLoads{named: named, load: load, source: source}
			current[key] = at
		} else if (source && !at.source) || (source == at.source && load < at.load) {
			at.named, at.load, at.source = named, load, source
		}
		at.loads = append(at.loads, load)
	}
	currentIfaces := resolved.currentInterfaces()
	for _, relPath := range scope {
		still := map[string]bool{}
		for _, id := range d.edgesByFile[relPath] {
			still[id] = true
		}
		prior := e.supertypes[relPath]
		for _, id := range sortedKeys(prior) {
			r := prior[id]
			if still[id] {
				continue
			}
			node, ok := structural[relPath].typeNodes[r.subtype]
			if !ok || node.ID != r.subtypeNodeID {
				continue
			}
			at, ok := current[subtypeAt{r.subtypePkg, r.subtype, relPath}]
			if !ok {
				continue
			}
			answered := false
			for _, load := range at.loads {
				if !resolved.view(load).unevaluated(r.ifacePkg, r.ifaceName) {
					answered = true
					break
				}
			}
			if answered {
				continue
			}
			methods := r.ifaceMethods
			if cur, ok := currentIfaces[r.ifacePkg+"."+r.ifaceName]; ok {
				iface := cur.Type().Underlying().(*types.Interface)
				methods = interfaceMethodNames(iface)
				r.ifaceMethods = methods
				r.ifaceSig = interfaceSignature(iface)
				r.ifaceScope = resolved.targetScopeOf(cur, r.ifacePkg, r.ifaceKey)
			} else if !resolved.unevaluated(r.ifacePkg, r.ifaceName) {
				continue
			}
			have := methodNames(at.named)
			satisfied := true
			for _, method := range methods {
				if !have[method] {
					satisfied = false
					break
				}
			}
			if satisfied {
				d.supertype(relPath, r, node.Range.Start)
			}
		}
	}
}

// currentInterfaces is, per `<package path>.<name>`, the interface object
// this pass holds for it: one folded from source when there is one, the
// smallest load first.
func (r *resolutions) currentInterfaces() map[string]*types.TypeName {
	out := map[string]*types.TypeName{}
	sourceOf := func(obj *types.TypeName) bool {
		return r.view(r.loadOf[obj]).folded[obj.Pkg().Path()]
	}
	for obj := range r.interfaces {
		key := obj.Pkg().Path() + "." + obj.Name()
		prev, ok := out[key]
		if !ok {
			out[key] = obj
			continue
		}
		if s, ps := sourceOf(obj), sourceOf(prev); s != ps {
			if s {
				out[key] = obj
			}
			continue
		}
		if r.loadOf[obj] < r.loadOf[prev] {
			out[key] = obj
		}
	}
	return out
}

// staleImplementorFiles is the files outside `scope` holding a SUPERTYPE_OF
// edge onto an interface whose package this pass folded from source, where
// that interface is now gone or its method set differs from the one the edge
// was produced against (GM-620). Their edges may no longer hold, and only a
// pass over their own file can retract them. A recorded file no longer on
// disk is forgotten instead: core already removed its rows. Sorted.
func (e *semanticEngine) staleImplementorFiles(resolved *resolutions, scope []string) []string {
	if len(e.supertypes) == 0 || len(resolved.folded) == 0 {
		return nil
	}
	inScope := map[string]bool{}
	for _, relPath := range scope {
		inScope[relPath] = true
	}
	var sigs map[string]string
	var stale []string
	for _, relPath := range sortedKeys(e.supertypes) {
		if inScope[relPath] {
			continue
		}
		for _, r := range e.supertypes[relPath] {
			if !resolved.folded[r.ifacePkg] {
				continue
			}
			if sigs == nil {
				sigs = map[string]string{}
				for obj, named := range resolved.interfaces {
					if !resolved.folded[obj.Pkg().Path()] {
						continue
					}
					if iface, ok := named.Underlying().(*types.Interface); ok {
						sigs[obj.Pkg().Path()+"."+obj.Name()] = interfaceSignature(iface)
					}
				}
			}
			if sig, ok := sigs[r.ifacePkg+"."+r.ifaceName]; ok && sig == r.ifaceSig {
				continue
			}
			stale = append(stale, relPath)
			break
		}
	}
	kept := stale[:0]
	for _, relPath := range stale {
		if _, err := os.Stat(filepath.Join(e.realRoot, filepath.FromSlash(relPath))); err != nil && os.IsNotExist(err) {
			delete(e.emittedEdges, relPath)
			delete(e.supertypes, relPath)
			continue
		}
		kept = append(kept, relPath)
	}
	if len(kept) == 0 {
		return nil
	}
	return kept
}

// unionSorted is the sorted, de-duplicated union of two path lists.
func unionSorted(a, b []string) []string {
	seen := map[string]bool{}
	for _, p := range a {
		seen[p] = true
	}
	for _, p := range b {
		seen[p] = true
	}
	return sortedKeys(seen)
}

// keepPathsIn keeps the paths of `a` that are also in `b`, in a's order.
func keepPathsIn(a, b []string) []string {
	in := map[string]bool{}
	for _, p := range b {
		in[p] = true
	}
	out := []string{}
	for _, p := range a {
		if in[p] {
			out = append(out, p)
		}
	}
	return out
}

// countDirs is how many distinct directories - Go packages - the paths are in.
func countDirs(paths []string) int {
	dirs := map[string]bool{}
	for _, p := range paths {
		dirs[filepath.ToSlash(filepath.Dir(p))] = true
	}
	return len(dirs)
}

// unevaluated reports whether this load (or, through resolutions, any load
// of the pass) could not have checked a type against the interface
// `pkg.name`: its package was not folded from source, it did not come in
// from export data, and it is not an exported name of a package that did
// (export data would have carried it, so it is gone).
func (v *loadView) unevaluated(pkg, name string) bool {
	if v.folded[pkg] || v.importedInterfaces[pkg+"."+name] {
		return false
	}
	return !(v.importedPackages[pkg] && token.IsExported(name))
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
func (d *semanticDiff) finish(
	e *semanticEngine,
	scope []string,
	resolved *resolutions,
	structural map[string]structuralFile,
) fileChangeDiff {
	d.carryForward(e, scope, resolved, structural)
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
		e.supertypes[relPath] = d.supertypes[relPath]
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
