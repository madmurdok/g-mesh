package main

import (
	"sort"
	"strings"
	"testing"
)

// Both receiver forms normalize to `T.M`, and which form it was stays
// visible in the signature - the design doc's own rule. Go forbids declaring
// both, so the normalization can never merge two distinct declarations.
func TestMethodQualifiedNamesNormalizeAndKeepTheReceiverKind(t *testing.T) {
	pointer := extract(t, "a.go", `package app

type Server struct{}

func (s *Server) Close() error { return nil }
`)
	value := extract(t, "b.go", `package app

type Server struct{}

func (s Server) Close() error { return nil }
`)

	for name, graph := range map[string]fileGraph{"pointer": pointer, "value": value} {
		node := nodeByQName(t, graph, "Server.Close")
		if node.Kind != nodeKindFunction || node.NativeKind != "method" {
			t.Fatalf("%s receiver: node = %+v, want a Function/method", name, node)
		}
		if node.Name != "Close" {
			t.Fatalf("%s receiver: name = %q, want the bare method name", name, node.Name)
		}
	}

	if got := nodeByQName(t, pointer, "Server.Close").Signature; got != "func (s *Server) Close() error" {
		t.Fatalf("pointer signature = %q", got)
	}
	if got := nodeByQName(t, value, "Server.Close").Signature; got != "func (s Server) Close() error" {
		t.Fatalf("value signature = %q", got)
	}
}

func TestGenericReceiverNormalizesToTheBareTypeName(t *testing.T) {
	graph := extract(t, "a.go", `package app

type Stack[T any] struct{}

func (s *Stack[T]) Push(v T) {}
`)
	node := nodeByQName(t, graph, "Stack.Push")
	if node.Signature != "func (s *Stack[T]) Push(v T)" {
		t.Fatalf("signature = %q", node.Signature)
	}
	if got := nodeByQName(t, graph, "Stack").Signature; got != "type Stack[T any] struct" {
		t.Fatalf("type signature = %q", got)
	}
}

// An interface method is `I.M` and is a *declaration*. Nothing structural
// claims any type implements the interface: Go's interfaces are structural,
// and only go/types can answer that (GM-281).
func TestInterfaceMethodsAreDeclarationsAndNotImplementations(t *testing.T) {
	graph := extract(t, "a.go", `package app

import "io"

// Closer closes things.
type Closer interface {
	io.Reader

	// Close releases the resource.
	Close() error
}

type Server struct{}

func (s *Server) Close() error { return nil }
`)

	method := nodeByQName(t, graph, "Closer.Close")
	if method.NativeKind != "interface_method" || method.Kind != nodeKindFunction {
		t.Fatalf("node = %+v, want a Function/interface_method", method)
	}
	if method.Signature != "func Close() error" {
		t.Fatalf("signature = %q", method.Signature)
	}
	if method.DocComment != "Close releases the resource." {
		t.Fatalf("docComment = %q", method.DocComment)
	}
	// `Server.Close` and `Closer.Close` are two different declarations with
	// two different qualifiedNames, which is the whole point of `I.M`.
	nodeByQName(t, graph, "Server.Close")

	for _, edge := range graph.edges {
		if edge.Kind == "SUPERTYPE_OF" {
			t.Fatal("a structural Go tier must never claim interface satisfaction")
		}
	}
	// The embedded interface is an ordinary type reference and nothing more:
	// promotion of io.Reader's methods into Closer's method set is go/types'
	// answer, not this tier's.
	assertHasEdge(t, graph, "REFERENCES Closer -> io.Reader (unresolved)")
}

// Several `init`s in one file would collide on qualifiedName and therefore
// on node id. They are kept - an init body is ordinary code that needs a
// caller - and disambiguated in nativeKind, which participates in the id.
func TestSeveralInitsInOneFileGetDistinctStableIDs(t *testing.T) {
	graph := extract(t, "a.go", `package app

func init() { first() }
func init() { second() }
func init() { third() }

func first()  {}
func second() {}
func third()  {}
`)

	var kinds []string
	ids := map[string]bool{}
	for _, node := range graph.nodes {
		if node.QualifiedName == "init" {
			kinds = append(kinds, node.NativeKind)
			ids[node.ID] = true
		}
	}
	sort.Strings(kinds)
	if want := []string{"init", "init#1", "init#2"}; !equalStrings(kinds, want) {
		t.Fatalf("init nativeKinds = %v, want %v", kinds, want)
	}
	if len(ids) != 3 {
		t.Fatalf("three inits collapsed to %d id(s)", len(ids))
	}

	// Each init's own body must hang off its own node, which is the reason
	// they are nodes at all.
	assertHasEdge(t, graph, "CALLS init -> first (resolved)")
	assertHasEdge(t, graph, "CALLS init -> second (resolved)")
	assertHasEdge(t, graph, "CALLS init -> third (resolved)")
	if n := countUses(graph, "CALLS init ->"); n != 3 {
		t.Fatalf("expected each init to make its own call, got %d", n)
	}
}

func TestVisibilityIsCapitalizationAndContainerIsTheImportPath(t *testing.T) {
	graph := extractFile(testWorkspace(), "server/server.go", []byte(`package server

type Server struct{}

type conn struct{}

const Timeout = 1

var registry = 2

func New() *Server { return nil }

func helper() {}
`))

	public := []string{"Server", "Timeout", "New"}
	private := []string{"conn", "registry", "helper"}
	for _, name := range public {
		node := nodeByQName(t, graph, name)
		if node.Visibility.kind != "public" {
			t.Errorf("%s visibility = %+v, want public", name, node.Visibility)
		}
		if node.Container != serverPkg {
			t.Errorf("%s container = %q, want %q", name, node.Container, serverPkg)
		}
	}
	for _, name := range private {
		node := nodeByQName(t, graph, name)
		if node.Visibility.kind != "container" || node.Visibility.container != serverPkg {
			t.Errorf("%s visibility = %+v, want container(%s)", name, node.Visibility, serverPkg)
		}
	}

	// Only the public ones get an EXPORTS edge; all of them get DEFINES, and
	// both always run from the File node.
	exports, defines := 0, 0
	for _, edge := range graph.edges {
		switch edge.Kind {
		case edgeKindExports:
			exports++
		case edgeKindDefines:
			defines++
		}
		if (edge.Kind == edgeKindExports || edge.Kind == edgeKindDefines) &&
			edge.FromID != nodeByQName(t, graph, "server/server.go").ID {
			t.Fatalf("%s edge does not start at the File node", edge.Kind)
		}
	}
	if exports != len(public) || defines != len(public)+len(private) {
		t.Fatalf("got %d EXPORTS and %d DEFINES, want %d and %d",
			exports, defines, len(public), len(public)+len(private))
	}
}

// Go's one legal case of two packages in one directory. They cannot see each
// other's unexported names, so they are separate containers.
func TestExternalTestPackageGetsItsOwnContainer(t *testing.T) {
	external := extractFile(testWorkspace(), "app_test.go", []byte(`package app_test

func TestX() {}
`))
	if got := nodeByQName(t, external, "TestX").Container; got != testModule+"_test" {
		t.Fatalf("container = %q, want %q", got, testModule+"_test")
	}

	// An *internal* test file is an ordinary member of the package it tests,
	// which is what lets it reach that package's unexported symbols.
	internal := extractFile(testWorkspace(), "app_internal_test.go", []byte(`package app

func TestY() {}
`))
	if got := nodeByQName(t, internal, "TestY").Container; got != testModule {
		t.Fatalf("internal test container = %q, want %q", got, testModule)
	}
}

// `package main` is an ordinary package with an ordinary import path, which
// is also what `go list` calls it - so two `main` packages in one repository
// get two containers instead of colliding on the name.
func TestPackageMainIsNotSpecial(t *testing.T) {
	one := extractFile(testWorkspace(), "cmd/a/main.go", []byte("package main\n\nfunc main() {}\n"))
	two := extractFile(testWorkspace(), "cmd/b/main.go", []byte("package main\n\nfunc main() {}\n"))

	if got, want := nodeByQName(t, one, "main").Container, testModule+"/cmd/a"; got != want {
		t.Fatalf("container = %q, want %q", got, want)
	}
	if got, want := nodeByQName(t, two, "main").Container, testModule+"/cmd/b"; got != want {
		t.Fatalf("container = %q, want %q", got, want)
	}
}

func TestDocCommentsAndTypeSignatures(t *testing.T) {
	graph := extract(t, "a.go", `package app

// Server serves.
type Server struct {
	addr string
}

// Alias is an alias.
type Alias = Server

// Celsius is a defined type.
type Celsius float64

// Grouped holds two constants.
const (
	// First is the first.
	First = 1
	Second = 2
)

// Only is the single spec of its group, so the group's comment is its own.
var (
	Only = 3
)
`)

	cases := map[string]struct{ nativeKind, signature, doc string }{
		"Server":  {"struct", "type Server struct", "Server serves."},
		"Alias":   {"alias", "type Alias = Server", "Alias is an alias."},
		"Celsius": {"type", "type Celsius float64", "Celsius is a defined type."},
		"First":   {"const", "", "First is the first."},
		"Second":  {"const", "", ""},
		"Only":    {"var", "", "Only is the single spec of its group, so the group's comment is its own."},
	}
	for name, want := range cases {
		node := nodeByQName(t, graph, name)
		if node.NativeKind != want.nativeKind {
			t.Errorf("%s nativeKind = %q, want %q", name, node.NativeKind, want.nativeKind)
		}
		if node.Signature != want.signature {
			t.Errorf("%s signature = %q, want %q", name, node.Signature, want.signature)
		}
		if node.DocComment != want.doc {
			t.Errorf("%s docComment = %q, want %q", name, node.DocComment, want.doc)
		}
	}
}

// AllErrors keeps the parser going past the first syntax error, so a broken
// file still yields whatever declarations it does have. An editor's file is
// mid-edit most of the time it is looked at, so this is the normal case.
func TestASyntaxErrorYieldsAPartialGraphWithHasSyntaxErrors(t *testing.T) {
	graph := extract(t, "a.go", `package app

func Good() { helper() }

func Broken( {

func AlsoGood() {}
`)

	if len(graph.nodes) == 0 {
		t.Fatal("a broken file still has a File node")
	}
	for _, node := range graph.nodes {
		if !node.HasSyntaxErrors {
			t.Fatalf("every node of a file that did not parse must carry hasSyntaxErrors: %+v", node)
		}
	}
	// The declaration before the error survives, along with its edge.
	nodeByQName(t, graph, "Good")
	assertHasEdge(t, graph, "CALLS Good -> "+testContainer+".helper (unresolved)")
}

func TestAFileWithNoPackageClauseStillYieldsItsFileNode(t *testing.T) {
	for name, source := range map[string]string{"empty": "", "garbage": "this is not go\n"} {
		t.Run(name, func(t *testing.T) {
			graph := extract(t, "a.go", source)
			if len(graph.nodes) != 1 || graph.nodes[0].Kind != nodeKindFile {
				t.Fatalf("nodes = %v, want exactly the File node", qualifiedNames(graph))
			}
			if !graph.nodes[0].HasSyntaxErrors {
				t.Fatal("hasSyntaxErrors must be set")
			}
		})
	}
}

// A build-constrained file is indexed structurally like any other, every
// alternative of it. go/types in GM-281 only type-checks the host
// GOOS/GOARCH, so the excluded ones keep this graph and get no semantic
// upgrade - the design doc's documented failure mode.
func TestBuildConstrainedFilesAreIndexedStructurally(t *testing.T) {
	windows := extract(t, "sys_windows.go", `//go:build windows

package app

func Platform() string { return "windows" }
`)
	linux := extract(t, "sys_linux.go", `//go:build linux

package app

func Platform() string { return "linux" }
`)

	for _, graph := range []fileGraph{windows, linux} {
		node := nodeByQName(t, graph, "Platform")
		if node.Container != testContainer {
			t.Fatalf("container = %q", node.Container)
		}
	}
	// Two files, two nodes, two different ids - a node id carries the file
	// path, so the alternatives never collapse into one another.
	if nodeByQName(t, windows, "Platform").ID == nodeByQName(t, linux, "Platform").ID {
		t.Fatal("two build-constrained alternatives must stay distinct nodes")
	}
}

// A generated file is real, compiled, callable code. Dropping it would make
// find_callers silently incomplete for everything it calls; whether it is
// *interesting* is a question for a query, and a repository that considers
// it not-source says so in .gitignore, which the walk already honours.
func TestGeneratedFilesAreIndexedLikeAnyOther(t *testing.T) {
	graph := extract(t, "api.pb.go", `// Code generated by protoc-gen-go. DO NOT EDIT.

package app

func Marshal() { helper() }
`)
	nodeByQName(t, graph, "Marshal")
	assertHasEdge(t, graph, "CALLS Marshal -> "+testContainer+".helper (unresolved)")
}

// A node id must not depend on anything that moves when the file is edited
// elsewhere - that is what makes an incremental diff able to say "this same
// symbol, at a new range" rather than "a different symbol".
func TestNodeIDsSurviveAnEditElsewhereInTheFile(t *testing.T) {
	before := extract(t, "a.go", "package app\n\nfunc F() {}\n\nfunc G() {}\n")
	after := extract(t, "a.go", "package app\n\n// a new comment\n\nfunc F() {}\n\nfunc G() {}\n")

	for _, name := range []string{"a.go", "F", "G"} {
		if nodeByQName(t, before, name).ID != nodeByQName(t, after, name).ID {
			t.Fatalf("%s changed id across an edit elsewhere in the file", name)
		}
	}
	if nodeByQName(t, before, "F").Range == nodeByQName(t, after, "F").Range {
		t.Fatal("this test is vacuous unless the edit actually moved F")
	}
}

func TestTruncateCutsOnARuneBoundary(t *testing.T) {
	long := strings.Repeat("é", maxSignatureLength)
	got := truncate(long, maxSignatureLength)
	if !strings.HasSuffix(got, "...") {
		t.Fatalf("expected a truncation marker, got %q", got)
	}
	for _, r := range got {
		if r == '�' {
			t.Fatalf("truncation split a rune: %q", got)
		}
	}
}
