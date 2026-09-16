package main

import (
	"sort"
	"strings"
	"testing"
)

// The fixtures below are extracted in memory against a fixed one-module
// workspace, so a test says what it is about (the code) rather than about
// laying out a temporary directory. workspace_test.go covers the module
// layout itself.
const (
	testModule    = "github.com/example/app"
	testContainer = testModule
	serverPkg     = testModule + "/server"
)

func testWorkspace() *workspace {
	return &workspace{modules: []moduleRoot{{dir: "", path: testModule}}}
}

func extract(t *testing.T, relPath, source string) fileGraph {
	t.Helper()
	return extractFile(testWorkspace(), relPath, []byte(source))
}

// --- assertion helpers --------------------------------------------------

func nodeByQName(t *testing.T, graph fileGraph, qualifiedName string) wireNode {
	t.Helper()
	for _, node := range graph.nodes {
		if node.QualifiedName == qualifiedName {
			return node
		}
	}
	t.Fatalf("no node with qualifiedName %q; have %v", qualifiedName, qualifiedNames(graph))
	return wireNode{}
}

func qualifiedNames(graph fileGraph) []string {
	names := make([]string, 0, len(graph.nodes))
	for _, node := range graph.nodes {
		names = append(names, node.Kind+" "+node.NativeKind+" "+node.QualifiedName)
	}
	return names
}

// placeholderTargets renders every pending_symbol placeholder as
// "<container>|<name>|<fromContainer>", which is the whole address core will
// resolve it by - the one thing these tests are actually about.
func placeholderTargets(graph fileGraph) []string {
	var out []string
	for _, node := range graph.nodes {
		if node.NativeKind != pendingSymbolNativeKind {
			continue
		}
		out = append(out, node.Target.Scope.Container+"|"+node.Target.Key.Name+"|"+node.Target.FromContainer)
	}
	sort.Strings(out)
	return out
}

// edgeSummaries renders each edge as "KIND from -> to (resolved)", with ids
// replaced by the qualifiedNames they belong to.
func edgeSummaries(graph fileGraph) []string {
	names := map[string]string{}
	for _, node := range graph.nodes {
		names[node.ID] = node.QualifiedName
	}
	var out []string
	for _, edge := range graph.edges {
		state := "unresolved"
		if edge.Resolved {
			state = "resolved"
		}
		out = append(out, edge.Kind+" "+names[edge.FromID]+" -> "+names[edge.ToID]+" ("+state+")")
	}
	sort.Strings(out)
	return out
}

func assertHasEdge(t *testing.T, graph fileGraph, want string) {
	t.Helper()
	for _, summary := range edgeSummaries(graph) {
		if summary == want {
			return
		}
	}
	t.Fatalf("no edge %q; have:\n  %s", want, strings.Join(edgeSummaries(graph), "\n  "))
}

func assertNoEdgeMentioning(t *testing.T, graph fileGraph, needle string) {
	t.Helper()
	for _, summary := range edgeSummaries(graph) {
		if strings.Contains(summary, needle) {
			t.Fatalf("expected no edge mentioning %q, got %q", needle, summary)
		}
	}
}

func openSiteNames(graph fileGraph) []string {
	var out []string
	for _, site := range graph.openSites {
		out = append(out, site.Name)
	}
	sort.Strings(out)
	return out
}

// --- edge shapes --------------------------------------------------------

// A target declared in the same file needs nothing from core: the edge
// points straight at it and says so.
func TestSameFileCallIsADirectResolvedEdge(t *testing.T) {
	graph := extract(t, "a.go", `package app

func Caller() { callee() }

func callee() {}
`)

	assertHasEdge(t, graph, "CALLS Caller -> callee (resolved)")
	if targets := placeholderTargets(graph); len(targets) != 0 {
		t.Fatalf("a same-file call must not produce a placeholder, got %v", targets)
	}
}

// The shape the whole container-scoped address exists for: a call to a
// symbol of the same package that this file does not declare. The plugin
// cannot know which sibling file has it - that is exactly why the address
// names the *package* and not a file.
func TestCrossFileSamePackageCallIsAContainerPlaceholder(t *testing.T) {
	graph := extract(t, "a.go", `package app

func Caller() { helper() }
`)

	if got, want := placeholderTargets(graph), []string{testContainer + "|helper|" + testContainer}; !equalStrings(got, want) {
		t.Fatalf("placeholders = %v, want %v", got, want)
	}
	assertHasEdge(t, graph, "CALLS Caller -> "+testContainer+".helper (unresolved)")

	placeholder := nodeByQName(t, graph, testContainer+".helper")
	if placeholder.Kind != nodeKindModule || placeholder.NativeKind != pendingSymbolNativeKind {
		t.Fatalf("placeholder = %+v, want a Module/pending_symbol node", placeholder)
	}
	if placeholder.Name != "helper" {
		t.Fatalf("placeholder name = %q, want the bare name core matches on", placeholder.Name)
	}
	if placeholder.Container != "" {
		t.Fatalf("a placeholder is an address, not a member; container = %q", placeholder.Container)
	}
}

// The other half of the same story, and the acceptance criterion "an
// unexported symbol not linkable from another package": the declaration says
// it is visible only to `github.com/example/app/server`, and a requester in
// another package says so too. Core's linker refuses on those two facts
// alone (core/src/graph/symbol_links.rs, "Visibility"), and Go packages
// being flat means there is no parent chain that could let the requester in
// by another route.
func TestUnexportedDeclarationAndForeignRequesterCannotMeet(t *testing.T) {
	declaring := extractFile(testWorkspace(), "server/conn.go", []byte(`package server

func hidden() {}
`))
	declaration := nodeByQName(t, declaring, "hidden")
	if declaration.Visibility.kind != "container" || declaration.Visibility.container != serverPkg {
		t.Fatalf("visibility = %+v, want container(%s)", declaration.Visibility, serverPkg)
	}
	if declaration.ContainerParent != "" {
		t.Fatalf("containerParent = %q; Go packages are flat, so an unexported name has exactly one container that can see it", declaration.ContainerParent)
	}

	// A caller in another package, reaching the same name through an import.
	calling := extractFile(testWorkspace(), "cmd/main.go", []byte(`package main

import "github.com/example/app/server"

func main() { server.hidden() }
`))
	placeholder := nodeByQName(t, calling, serverPkg+".hidden")
	if placeholder.Target.Scope.Container != serverPkg {
		t.Fatalf("target container = %q, want %q", placeholder.Target.Scope.Container, serverPkg)
	}
	if placeholder.Target.FromContainer != testModule+"/cmd" {
		t.Fatalf("fromContainer = %q, want the *caller's* package", placeholder.Target.FromContainer)
	}
	if placeholder.Target.FromContainer == declaration.Visibility.container {
		t.Fatal("this test is vacuous unless the two containers differ")
	}
}

// A qualified call through an import names its container exactly, which is
// the one cross-package edge a structural tier can honestly emit.
func TestQualifiedCallIsScopedToTheImportedContainer(t *testing.T) {
	graph := extract(t, "cmd/main.go", `package main

import (
	"fmt"

	"github.com/example/app/server"
)

func main() {
	fmt.Println(server.New())
}
`)

	want := []string{
		"fmt|Println|" + testModule + "/cmd",
		serverPkg + "|New|" + testModule + "/cmd",
	}
	if got := placeholderTargets(graph); !equalStrings(got, want) {
		t.Fatalf("placeholders = %v, want %v", got, want)
	}
	assertHasEdge(t, graph, "CALLS main -> "+serverPkg+".New (unresolved)")
	assertHasEdge(t, graph, "CALLS main -> fmt.Println (unresolved)")
}

func TestImportsAreResolvedOrExternalByModulePath(t *testing.T) {
	graph := extract(t, "a.go", `package app

import (
	"fmt"

	"github.com/example/app/server"
	"github.com/other/dep"
)

var _ = fmt.Sprint(server.New(), dep.F())
`)

	if node := nodeByQName(t, graph, "fmt"); node.NativeKind != externalModuleNativeKind || node.Target != nil {
		t.Fatalf("fmt = %+v, want an external_module with no target", node)
	}
	if node := nodeByQName(t, graph, "github.com/other/dep"); node.NativeKind != externalModuleNativeKind {
		t.Fatalf("a module this project does not own must be external, got %q", node.NativeKind)
	}
	inProject := nodeByQName(t, graph, serverPkg)
	if inProject.NativeKind != resolvedModuleNativeKind {
		t.Fatalf("nativeKind = %q, want %q", inProject.NativeKind, resolvedModuleNativeKind)
	}
	if inProject.Target.Scope.Container != serverPkg || inProject.Target.Key.Name != reexportAllName {
		t.Fatalf("target = %+v, want the whole container", *inProject.Target)
	}
	assertHasEdge(t, graph, "IMPORTS a.go -> "+serverPkg+" (unresolved)")
}

// An aliased import binds the alias; a blank import binds nothing and still
// records the dependency, because a dependency is the only thing it states.
func TestImportAliasAndBlankImport(t *testing.T) {
	graph := extract(t, "a.go", `package app

import (
	f "fmt"
	_ "net/http/pprof"
)

func F() { f.Println() }
`)

	assertHasEdge(t, graph, "IMPORTS a.go -> net/http/pprof (unresolved)")
	if got, want := placeholderTargets(graph), []string{"fmt|Println|" + testContainer}; !equalStrings(got, want) {
		t.Fatalf("placeholders = %v, want the alias resolved to its import path: %v", got, want)
	}
}

// A dot import makes every bare name in the file ambiguous between this
// package and the dot-imported one, and a structural tier cannot see which
// names the other package exports. So the file emits no own-container
// placeholder at all - while the import itself, the same-file hits and the
// qualified uses through *other* imports all stay exactly as they were.
func TestDotImportSuppressesOwnContainerPlaceholdersOnly(t *testing.T) {
	graph := extract(t, "a.go", `package app

import (
	. "github.com/example/app/server"
	"strings"
)

func local() {}

func F() {
	Ambiguous()
	local()
	strings.TrimSpace("")
}
`)

	assertHasEdge(t, graph, "IMPORTS a.go -> "+serverPkg+" (unresolved)")
	assertHasEdge(t, graph, "CALLS F -> local (resolved)")
	assertHasEdge(t, graph, "CALLS F -> strings.TrimSpace (unresolved)")
	assertNoEdgeMentioning(t, graph, "Ambiguous")

	if got, want := placeholderTargets(graph), []string{"strings|TrimSpace|" + testContainer}; !equalStrings(got, want) {
		t.Fatalf("placeholders = %v, want only the qualified one: %v", got, want)
	}
}

// Without the dot import the same file would have addressed `Ambiguous` at
// its own package - which is what makes the test above a real test and not a
// tautology about a file that had nothing to emit.
func TestWithoutADotImportTheSameBareCallIsAddressed(t *testing.T) {
	graph := extract(t, "a.go", `package app

func F() { Ambiguous() }
`)
	assertHasEdge(t, graph, "CALLS F -> "+testContainer+".Ambiguous (unresolved)")
}

// A receiver call is the shape a structural tier must refuse: `Close` exists
// on many types, and the name alone cannot say which. No edge, one open
// site - and the receiver *expression* is still walked, because a
// package-level variable used as a receiver is a real, resolvable use.
func TestReceiverCallIsAnOpenSiteAndNeverAnEdge(t *testing.T) {
	graph := extract(t, "a.go", `package app

var logger = newLogger()

func F(s *Server) {
	s.Close()
	logger.Printf("x")
	_ = s.field
}
`)

	assertNoEdgeMentioning(t, graph, "Close")
	assertNoEdgeMentioning(t, graph, "Printf")
	assertNoEdgeMentioning(t, graph, "field")
	// `logger` itself is a package-level declaration of this very file.
	assertHasEdge(t, graph, "REFERENCES F -> logger (resolved)")

	if got, want := openSiteNames(graph), []string{"Close", "Printf", "field"}; !equalStrings(got, want) {
		t.Fatalf("open sites = %v, want %v", got, want)
	}
	for _, site := range graph.openSites {
		if site.FilePath != "a.go" {
			t.Fatalf("open site %+v has the wrong file", site)
		}
		if site.Name == "Close" && !site.IsCall {
			t.Fatal("s.Close() is a call")
		}
		if site.Name == "field" && site.IsCall {
			t.Fatal("s.field is not a call")
		}
	}
}

func TestBuiltinsProduceNothing(t *testing.T) {
	graph := extract(t, "a.go", `package app

func F(xs []int) int {
	ys := make([]int, 0, len(xs))
	ys = append(ys, xs...)
	if ys == nil {
		panic("no")
	}
	return len(ys)
}
`)
	if targets := placeholderTargets(graph); len(targets) != 0 {
		t.Fatalf("a universe name is not a package symbol; got %v", targets)
	}
}

// A package may legally shadow a universe name, and then the shadow is what
// a use means - so the file's own declarations are consulted before the
// universe block, not after.
func TestAPackageLevelDeclarationShadowsABuiltin(t *testing.T) {
	graph := extract(t, "a.go", `package app

func len(x string) int { return 0 }

func F() int { return len("x") }
`)
	assertHasEdge(t, graph, "CALLS F -> len (resolved)")
}

// --- local scope: the trap ----------------------------------------------

// Every binding form Go has, in one file, each shadowing a name this package
// also declares at top level. Not one of them may produce a placeholder: a
// package-scoped address for a local is an edge onto a symbol the use site
// cannot even see.
func TestLocalsNeverProduceAPackageScopedPlaceholder(t *testing.T) {
	graph := extract(t, "a.go", `package app

func Every[typeParam any](param int, second typeParam) (named error) {
	shadowed := 1
	var declared int
	const constant = 2
	type localType struct{}

	_ = param
	_ = second
	_ = named
	_ = shadowed
	_ = declared
	_ = constant
	_ = localType{}
	_ = typeParam(second)

	if inIf := 3; inIf > 0 {
		_ = inIf
	}

	for loopVar := 0; loopVar < 1; loopVar++ {
		_ = loopVar
	}

	for rangeKey, rangeValue := range []int{} {
		_ = rangeKey
		_ = rangeValue
	}

	switch switched := any(nil); switched {
	case nil:
		_ = switched
	}

	switch typeSwitched := any(nil).(type) {
	case int:
		_ = typeSwitched
	}

	select {
	case commReceived := <-make(chan int):
		_ = commReceived
	}

	closureParam := func(inner int) { _ = inner }
	closureParam(0)

label:
	for {
		break label
	}

	func(literalParam string) { _ = literalParam }("")

	return nil
}

func (receiverName *Every) Method() { _ = receiverName }
`)

	if targets := placeholderTargets(graph); len(targets) != 0 {
		t.Fatalf("a local must never become a package-scoped address; got %v", targets)
	}
}

// The counterpart: each of those spellings, used *without* being bound, is a
// real package-scope reference. Without this the test above would pass for a
// walker that simply never emitted anything.
func TestTheSameNamesUnboundAreAddressed(t *testing.T) {
	graph := extract(t, "a.go", `package app

func F() {
	_ = param
	_ = shadowed
	_ = constant
	_ = loopVar
	_ = rangeKey
	_ = switched
	_ = typeSwitched
	_ = commReceived
	_ = inner
	_ = literalParam
	_ = receiverName
}
`)

	want := []string{
		"commReceived", "constant", "inner", "literalParam", "loopVar",
		"param", "rangeKey", "receiverName", "shadowed", "switched", "typeSwitched",
	}
	var got []string
	for _, node := range graph.nodes {
		if node.NativeKind == pendingSymbolNativeKind {
			got = append(got, node.Name)
		}
	}
	sort.Strings(got)
	if !equalStrings(got, want) {
		t.Fatalf("placeholders = %v, want %v", got, want)
	}
}

// Go evaluates a `:=`'s right-hand side before it binds anything on the
// left, so `helper := helper()` calls the package-level `helper`. A walker
// that bound the left side first would lose that edge.
func TestShortVariableDeclarationReadsItsRightHandSideFirst(t *testing.T) {
	graph := extract(t, "a.go", `package app

func F() {
	helper := helper()
	_ = helper
}
`)
	assertHasEdge(t, graph, "CALLS F -> "+testContainer+".helper (unresolved)")
}

// A shadow covers the rest of its block and nothing before it.
func TestAShadowDoesNotReachBackwardsOrOutOfItsBlock(t *testing.T) {
	graph := extract(t, "a.go", `package app

func F() {
	before()
	{
		before := 1
		_ = before
	}
	after()
	_ = before
}

func before() {}
func after()  {}
`)
	assertHasEdge(t, graph, "CALLS F -> before (resolved)")
	assertHasEdge(t, graph, "CALLS F -> after (resolved)")
	assertHasEdge(t, graph, "REFERENCES F -> before (resolved)")
}

// A parameter's *type* resolves in the enclosing scope, not in the body the
// parameter name binds in - so `func f(Config Config)` references the
// package-level type `Config`, and only uses inside the body see the
// parameter.
func TestAParameterTypeResolvesOutsideTheBodyItBinds(t *testing.T) {
	graph := extract(t, "a.go", `package app

type Config struct{}

func F(Config Config) { _ = Config }
`)
	assertHasEdge(t, graph, "REFERENCES F -> Config (resolved)")
	if n := countUses(graph, "-> Config"); n != 1 {
		t.Fatalf("expected exactly one reference to the type Config, got %d:\n  %s",
			n, strings.Join(edgeSummaries(graph), "\n  "))
	}
}

// A struct field name and a struct literal's key are not uses of anything -
// and a map literal's bare key is indistinguishable from the latter, so it
// is skipped too. A key that is not a bare identifier is unambiguous and is
// walked.
func TestCompositeLiteralKeys(t *testing.T) {
	graph := extract(t, "a.go", `package app

import "strings"

type Config struct{ Mode int }

const Mode = 1

func F() {
	_ = Config{Mode: 2}
	_ = map[string]int{"a": Mode}
	_ = map[string]int{strings.Title: 3}
}
`)
	// `Mode:` as a key is skipped; `Mode` as a *value* in the map literal is
	// a real use of the package-level constant.
	assertHasEdge(t, graph, "REFERENCES F -> Mode (resolved)")
	if n := countUses(graph, "-> Mode"); n != 1 {
		t.Fatalf("expected exactly one edge onto Mode (the value, not the field key), got %d:\n  %s",
			n, strings.Join(edgeSummaries(graph), "\n  "))
	}
	assertHasEdge(t, graph, "REFERENCES F -> strings.Title (unresolved)")
}

func equalStrings(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

// countUses counts CALLS/REFERENCES edges mentioning a name, ignoring the
// DEFINES/EXPORTS pair every declaration gets from its file - those are
// ownership, not uses, and counting them would drown the thing under test.
func countUses(graph fileGraph, needle string) int {
	count := 0
	for _, summary := range edgeSummaries(graph) {
		if !strings.HasPrefix(summary, edgeKindCalls) && !strings.HasPrefix(summary, edgeKindReferences) {
			continue
		}
		if strings.Contains(summary, needle) {
			count++
		}
	}
	return count
}
