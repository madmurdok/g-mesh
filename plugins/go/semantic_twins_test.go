package main

// Build-tag twins. A package that declares one name in two files
// with complementary build constraints (gin's binding.go / binding_nomsgpack.go)
// gives core two declarations with one container and one name, so an engine
// answer addressed by container was ambiguous and refused. A twinned answer
// is addressed by the file the host build declares it in instead; every other
// answer keeps its container address and its id.
//
// One fixture module carries every shape, judged against one load per pass:
//
//	lib/binding.go       //go:build !tagx  the twin the host builds
//	lib/binding_tagx.go  //go:build tagx   the twin it excludes
//	lib/plain.go                           non-twinned declarations
//	lib/gen.go           //go:build ignore package main: no twins
//	lib/size_tagx_test.go //go:build tagx  a test file: no twins
//	app/app.go                             the answers

import (
	"bufio"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"testing"
)

const twinModule = "example.com/twins"

// twinDecls is what both twins declare: an interface with a method (T1,
// T3), a type used in a conversion (T4), a method on a non-twinned type
// (T5), and the constructor the call site uses.
const twinDecls = `
type Binder interface {
	Bind() string
}

type Kelvin float64

func (Codec) Encode() string { return "%s" }

func New() Binder { return impl{} }

type impl struct{}

func (impl) Bind() string { return "%s" }
`

var twinFiles = map[string]string{
	"go.mod": "module " + twinModule + "\n\ngo 1.22\n",

	// The host-built twin. Its last two declarations sit behind `//line`
	// directives (T6): Legacy's declaring file becomes lib/legacy.tmpl, a
	// project file that is not Go source, and Outside's a path outside the
	// project root.
	"lib/binding.go": "//go:build !tagx\n\npackage lib\n" +
		strings.ReplaceAll(twinDecls, "%s", "json") + `
//line legacy.tmpl:1
type Legacy interface {
	Run() string
}

//line /gm570-outside/outside.go:1
type Outside interface {
	Walk() string
}
`,

	"lib/binding_tagx.go": "//go:build tagx\n\npackage lib\n" +
		strings.ReplaceAll(twinDecls, "%s", "msgpack") + `
type Legacy interface {
	Run() string
}

type Outside interface {
	Walk() string
}
`,

	"lib/plain.go": `package lib

type Codec struct{}

func (Codec) Name() string { return "codec" }

func (Codec) Size() int { return 1 }

type Meters float64
`,

	// An excluded file of another package declares nothing in lib, so
	// Codec.Name is not a twin.
	"lib/gen.go": `//go:build ignore

package main

func (Codec) Name() string { return "generated" }
`,

	// An excluded test file is not production code, so Codec.Size is not a
	// twin either.
	"lib/size_tagx_test.go": `//go:build tagx

package lib

func (Codec) Size() int { return 2 }
`,

	"app/app.go": `package app

import "example.com/twins/lib"

func UseBinder() string {
	b := lib.New()
	return b.Bind()
}

func UseCodec() string {
	var c lib.Codec
	return c.Encode()
}

func UseName() string {
	var c lib.Codec
	return c.Name()
}

func UseSize() int {
	var c lib.Codec
	return c.Size()
}

func UseLegacy(l lib.Legacy) string { return l.Run() }

func UseOutside(o lib.Outside) string { return o.Walk() }

func Convert() interface{} { return lib.Kelvin(3) }

func ConvertMeters() interface{} { return lib.Meters(2) }

type Mine struct{}

func (Mine) Bind() string { return "mine" }
`,
}

// renderTwinEdges renders a semantic diff as
//
//	"app/app.go:UseBinder CALLS file:lib/binding.go#Binder.Bind"
//	"app/app.go:UseName CALLS container:example.com/twins/lib#Codec.Name"
//
// so a test sees the scope each answer was sent with. It also returns the
// semantic nodes by that same target rendering.
func renderTwinEdges(t *testing.T, root string, diff fileChangeDiff) ([]string, map[string]wireNode) {
	t.Helper()
	ws := loadWorkspace(root)
	labels := map[string]string{}
	for _, relPath := range walkProjectFiles(root) {
		content, err := os.ReadFile(filepath.Join(canonicalizeProjectRoot(root), filepath.FromSlash(relPath)))
		if err != nil {
			t.Fatalf("read %s: %v", relPath, err)
		}
		for _, node := range extractFile(ws, relPath, content).nodes {
			labels[node.ID] = relPath + ":" + node.QualifiedName
		}
	}
	targets := map[string]string{}
	nodes := map[string]wireNode{}
	for _, node := range diff.UpsertNodes {
		if node.Target == nil {
			t.Fatalf("semantic node %s (%s) carries no placeholder target", node.ID, node.QualifiedName)
		}
		scope := node.Target.Scope
		var rendered string
		switch {
		case scope.File != "" && scope.Container == "":
			rendered = "file:" + scope.File
		case scope.Container != "" && scope.File == "":
			rendered = "container:" + scope.Container
		default:
			t.Fatalf("semantic node %s has scope %+v: want exactly one of file and container", node.ID, scope)
		}
		if node.Target.Key.QualifiedName == "" || node.Target.Key.Name != "" {
			t.Fatalf("semantic node %s is not qualifiedName-keyed: %+v", node.ID, node.Target.Key)
		}
		rendered += "#" + node.Target.Key.QualifiedName
		targets[node.ID] = rendered
		nodes[node.FilePath+" "+rendered] = node
	}
	var out []string
	for _, edge := range diff.UpsertEdges {
		from, ok := labels[edge.FromID]
		if !ok {
			from = "<unknown " + edge.FromID + ">"
		}
		to, ok := targets[edge.ToID]
		if !ok {
			to = "<unknown " + edge.ToID + ">"
		}
		out = append(out, from+" "+edge.Kind+" "+to)
	}
	sort.Strings(out)
	return out, nodes
}

func twinWholePass(t *testing.T) (string, []string, map[string]wireNode) {
	t.Helper()
	requireGoToolchain(t)
	root := writeTree(t, twinFiles)
	diff, reason, _ := newPluginState(root).handleSemanticPass(nil)
	if reason != "" {
		t.Fatalf("a whole-project pass answered incomplete: %s", reason)
	}
	edges, nodes := renderTwinEdges(t, root, diff)
	return root, edges, nodes
}

func requireTwinNode(t *testing.T, nodes map[string]wireNode, key string) wireNode {
	t.Helper()
	node, ok := nodes[key]
	if !ok {
		keys := make([]string, 0, len(nodes))
		for k := range nodes {
			keys = append(keys, k)
		}
		sort.Strings(keys)
		t.Fatalf("no semantic node %q; nodes:\n  %s", key, strings.Join(keys, "\n  "))
	}
	return node
}

// T1, T2, T3, T5, T6 and the two filters in one whole-project pass: every
// answer, with the scope it is sent with. Twinned names (Binder, Binder.Bind,
// Kelvin, Codec.Encode) are addressed by the host-built twin; everything else
// keeps its container, including the twins whose declaring file is not a Go
// file of this project (Legacy.Run, Outside.Walk).
func TestSemanticPassAddressesBuildTagTwinsByTheHostBuiltFile(t *testing.T) {
	_, got, _ := twinWholePass(t)

	lib := "container:" + twinModule + "/lib"
	want := []string{
		// T5: a method with a receiver and an interface method are twins.
		"app/app.go:UseBinder CALLS file:lib/binding.go#Binder.Bind",
		"app/app.go:UseCodec CALLS file:lib/binding.go#Codec.Encode",
		// T2: names declared once keep the container address. Codec.Name
		// is also declared by an ignored `package main` file, Codec.Size by
		// an ignored test file: neither makes a twin.
		"app/app.go:UseName CALLS " + lib + "#Codec.Name",
		"app/app.go:UseSize CALLS " + lib + "#Codec.Size",
		// T6: a twin whose declaring file is not a project Go file keeps the
		// container scope rather than losing the edge.
		"app/app.go:UseLegacy CALLS " + lib + "#Legacy.Run",
		"app/app.go:UseOutside CALLS " + lib + "#Outside.Walk",
		// A conversion of a twinned and of a plain type.
		"app/app.go:Convert REFERENCES file:lib/binding.go#Kelvin",
		"app/app.go:ConvertMeters REFERENCES " + lib + "#Meters",
		// T3: SUPERTYPE_OF onto a twinned interface, from another package
		// and from the twin's own package.
		"app/app.go:Mine SUPERTYPE_OF file:lib/binding.go#Binder",
		"lib/binding.go:impl SUPERTYPE_OF file:lib/binding.go#Binder",
	}
	sort.Strings(want)
	if !equalStrings(got, want) {
		t.Fatalf("semantic pass produced\n  %s\nwant\n  %s",
			strings.Join(got, "\n  "), strings.Join(want, "\n  "))
	}
}

// T2 and T4: ids. A container-scoped answer's placeholder has the id it has
// always had - the structural placeholderFor id for the same address in the
// same file (Meters), and the address hash for a member (Codec.Name). A
// file-scoped one must differ from the structural placeholder of the same
// name in the same file (Kelvin), or the two tiers write one node and the
// last writer decides its address (design 1.4).
func TestSemanticPassGivesAFileScopedPlaceholderItsOwnID(t *testing.T) {
	root, _, nodes := twinWholePass(t)

	structural := map[string]string{}
	content, err := os.ReadFile(filepath.Join(canonicalizeProjectRoot(root), "app", "app.go"))
	if err != nil {
		t.Fatal(err)
	}
	for _, node := range extractFile(loadWorkspace(root), "app/app.go", content).nodes {
		if node.Target != nil {
			structural[node.QualifiedName] = node.ID
		}
	}
	lib := twinModule + "/lib"

	kelvinStructural, ok := structural[lib+".Kelvin"]
	if !ok {
		t.Fatalf("sanity: the structural tier emitted no placeholder for %s.Kelvin; placeholders %v", lib, structural)
	}
	kelvin := requireTwinNode(t, nodes, "app/app.go file:lib/binding.go#Kelvin")
	if kelvin.ID == kelvinStructural {
		t.Fatalf("the file-scoped Kelvin placeholder reuses the structural placeholder id %s", kelvin.ID)
	}
	if kelvin.QualifiedName != lib+".Kelvin" {
		t.Fatalf("the file-scoped placeholder's display qualifiedName = %q, want %q", kelvin.QualifiedName, lib+".Kelvin")
	}

	metersStructural, ok := structural[lib+".Meters"]
	if !ok {
		t.Fatalf("sanity: the structural tier emitted no placeholder for %s.Meters", lib)
	}
	meters := requireTwinNode(t, nodes, "app/app.go container:"+lib+"#Meters")
	if meters.ID != metersStructural {
		t.Fatalf("the container-scoped Meters placeholder id %s, want the structural id %s", meters.ID, metersStructural)
	}

	name := requireTwinNode(t, nodes, "app/app.go container:"+lib+"#Codec.Name")
	if want := nodeIDFor("app/app.go", nodeKindModule, lib+".Codec.Name", pendingSymbolNativeKind); name.ID != want {
		t.Fatalf("the container-scoped Codec.Name placeholder id %s, want %s", name.ID, want)
	}
	if name.QualifiedName != lib+".Codec.Name" {
		t.Fatalf("Codec.Name placeholder qualifiedName = %q", name.QualifiedName)
	}
}

// The extra behaviour loadReferencedTwins adds: a per-file pass folds only
// the edited file's package, and the imported twinned package comes from
// export data without its ignored files. Its twins are listed separately,
// so an edit to app.go does not flip its answers back to the ambiguous
// container address.
func TestSemanticPassPerFileKeepsTwinsOfAnImportedPackage(t *testing.T) {
	requireGoToolchain(t)
	root := writeTree(t, twinFiles)

	diff, reason, _ := newPluginState(root).handleSemanticPass([]string{"app/app.go"})
	if reason != "" {
		t.Fatalf("a per-file pass answered incomplete: %s", reason)
	}
	got, _ := renderTwinEdges(t, root, diff)
	lib := "container:" + twinModule + "/lib"
	for _, want := range []string{
		"app/app.go:UseBinder CALLS file:lib/binding.go#Binder.Bind",
		"app/app.go:UseCodec CALLS file:lib/binding.go#Codec.Encode",
		"app/app.go:Convert REFERENCES file:lib/binding.go#Kelvin",
		"app/app.go:UseName CALLS " + lib + "#Codec.Name",
	} {
		if !containsString(got, want) {
			t.Fatalf("per-file pass of app/app.go did not produce %s; got\n  %s", want, strings.Join(got, "\n  "))
		}
	}
}

// The `go list` loadReferencedTwins adds runs under the same offline
// settings as the main load (ADR 0033 D3/D4): GOPROXY=off, GOSUMDB=off and
// the main load's `-mod` flag, whatever the user's environment says. A
// recording `go` on PATH logs every invocation's environment and arguments
// and then runs the real one.
func TestLoadReferencedTwinsListsUnderTheOfflineEnv(t *testing.T) {
	requireGoToolchain(t)
	realGo, err := exec.LookPath("go")
	if err != nil {
		t.Skip("no go on PATH")
	}
	root := writeTree(t, twinFiles)
	log := filepath.Join(t.TempDir(), "go.log")
	binDir := t.TempDir()
	buildRecordingGo(t, binDir, realGo, log)

	t.Setenv("PATH", binDir+string(os.PathListSeparator)+os.Getenv("PATH"))
	t.Setenv("GOPROXY", "https://gm570-proxy.invalid")
	t.Setenv("GOSUMDB", "sum.golang.org")
	t.Setenv("GOFLAGS", "-mod=mod")
	t.Setenv("GOENV", "off")
	t.Setenv("GOWORK", "")

	diff, reason, _ := newPluginState(root).handleSemanticPass([]string{"app/app.go"})
	if reason != "" {
		t.Fatalf("a per-file pass answered incomplete: %s", reason)
	}

	var twinLists []string
	for _, line := range readLines(t, log) {
		args := strings.Split(line, "|")[3]
		if strings.HasPrefix(args, "list ") && containsString(strings.Fields(args), twinModule+"/lib") {
			twinLists = append(twinLists, line)
		}
	}
	if len(twinLists) == 0 {
		t.Fatalf("sanity: no `go list` of %s/lib ran, so the twin listing was not observed; log:\n%s",
			twinModule, strings.Join(readLines(t, log), "\n"))
	}
	for _, line := range twinLists {
		fields := strings.Split(line, "|")
		if fields[0] != "GOPROXY=off" || fields[1] != "GOSUMDB=off" {
			t.Errorf("the twin listing ran with %s %s, want GOPROXY=off GOSUMDB=off: %s", fields[0], fields[1], line)
		}
		if !containsString(strings.Fields(fields[3]), "-mod=readonly") {
			t.Errorf("the twin listing ran without -mod=readonly over the user's %s: %s", fields[2], line)
		}
	}

	got, _ := renderTwinEdges(t, root, diff)
	want := "app/app.go:UseBinder CALLS file:lib/binding.go#Binder.Bind"
	if !containsString(got, want) {
		t.Fatalf("sanity: the per-file pass did not produce %s; got\n  %s", want, strings.Join(got, "\n  "))
	}
}

// buildRecordingGo compiles a `go` into dir that appends
// "GOPROXY=..|GOSUMDB=..|GOFLAGS=..|<args>" to log and then runs realGo
// with the same arguments, environment and stdio.
func buildRecordingGo(t *testing.T, dir, realGo, log string) {
	t.Helper()
	src := filepath.Join(t.TempDir(), "main.go")
	program := `package main

import (
	"os"
	"os/exec"
	"strings"
)

func main() {
	f, err := os.OpenFile(` + "`" + log + "`" + `, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err == nil {
		f.WriteString("GOPROXY=" + os.Getenv("GOPROXY") + "|GOSUMDB=" + os.Getenv("GOSUMDB") +
			"|GOFLAGS=" + os.Getenv("GOFLAGS") + "|" + strings.Join(os.Args[1:], " ") + "\n")
		f.Close()
	}
	cmd := exec.Command(` + "`" + realGo + "`" + `, os.Args[1:]...)
	cmd.Stdin, cmd.Stdout, cmd.Stderr = os.Stdin, os.Stdout, os.Stderr
	if err := cmd.Run(); err != nil {
		if exit, ok := err.(*exec.ExitError); ok {
			os.Exit(exit.ExitCode())
		}
		os.Exit(1)
	}
}
`
	if err := os.WriteFile(src, []byte(program), 0o644); err != nil {
		t.Fatal(err)
	}
	out := filepath.Join(dir, "go")
	if runtime.GOOS == "windows" {
		out += ".exe"
	}
	cmd := exec.Command(realGo, "build", "-o", out, src)
	cmd.Dir = filepath.Dir(src)
	cmd.Env = append(os.Environ(), "GOFLAGS=", "GOWORK=off", "GO111MODULE=on")
	if output, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("sanity: building the recording go: %v: %s", err, output)
	}
}

func readLines(t *testing.T, path string) []string {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatalf("sanity: the recording go logged nothing: %v", err)
	}
	defer f.Close()
	var lines []string
	scanner := bufio.NewScanner(f)
	scanner.Buffer(make([]byte, 0, 64*1024), 1<<20)
	for scanner.Scan() {
		lines = append(lines, scanner.Text())
	}
	return lines
}

// T5 and the two filters, on twinnedNames alone: spellings are extract.go's
// (`F`, `T`, `T.M` for a method and an interface method, each var/const),
// only names declared on both sides are twins, and neither an excluded
// file of another package nor an excluded test file contributes.
func TestTwinnedNamesSpellsMembersAndSkipsOtherPackagesAndTests(t *testing.T) {
	dir := t.TempDir()
	write := func(name, contents string) string {
		path := filepath.Join(dir, name)
		if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
			t.Fatal(err)
		}
		return path
	}
	shared := `
type Codec struct{}
type Binder interface {
	Bind() string
	Unbind()
}
func (*Codec) Encode() string { return "" }
func New() Binder { return nil }
var Default, other = 1, 2
const Mode = "x"
func init() {}
`
	loaded := []string{
		write("binding.go", "//go:build !tagx\n\npackage lib\n"+shared),
		write("plain.go", "package lib\n\nfunc Only() {}\n\nfunc (Codec) Name() string { return \"\" }\n\nfunc (Codec) Size() int { return 0 }\n"),
		write("plain_test.go", "package lib\n\nfunc helper() {}\n"),
	}
	ignored := []string{
		write("binding_tagx.go", "//go:build tagx\n\npackage lib\n"+shared),
		write("gen.go", "//go:build ignore\n\npackage main\n\nfunc (Codec) Name() string { return \"\" }\n"),
		write("size_tagx_test.go", "//go:build tagx\n\npackage lib\n\nfunc (Codec) Size() int { return 0 }\n\nfunc helper() {}\n"),
	}

	got := twinnedNames("lib", loaded, ignored)
	var names []string
	for name := range got {
		names = append(names, name)
	}
	sort.Strings(names)
	want := []string{"Binder", "Binder.Bind", "Binder.Unbind", "Codec", "Codec.Encode", "Default", "Mode", "New", "other"}
	if !equalStrings(names, want) {
		t.Fatalf("twinnedNames = %v, want %v", names, want)
	}

	if twins := twinnedNames("lib", loaded, nil); len(twins) != 0 {
		t.Fatalf("a package with no excluded file has twins: %v", twins)
	}
}
