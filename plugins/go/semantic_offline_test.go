package main

// A full index plus semantic pass reaches no network and writes
// nothing under the project root
// (docs/design/GM-512-plugins-no-writes-no-network.md §3, ADR 0033).
//
// Each test indexes a fixture with runBulkIndex and then runs the plugin's
// own whole-project semantic pass, under a user environment that would
// download or rewrite without the prevention:
//
//   - network: GOPROXY points at a local sink that logs every connection
//     and answers 403; GOMODCACHE is empty, so an unpinned `go list` would
//     fetch the required module from it;
//   - writes: a user GOFLAGS=-mod=mod, under which `go list` adds the
//     missing `go` line and requirement to go.mod;
//   - vendor: a vendored module (and a vendored workspace), which only loads
//     offline with -mod=vendor.
//
// Every test first runs plain `go list` the way the user's environment
// would, on a separate copy of the fixture, and checks it really fetches or
// writes (the hostile setup is in force). Then the pass must resolve a
// semantic edge (the loader really ran), leave the root's snapshot - every
// path with its type, size and content hash - unchanged, and leave the sink
// empty.

import (
	"bufio"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"io/fs"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"
)

// networkSink is a local listener that records the request line of every
// connection and refuses it with a 403, so each attempt is a log line and
// fails fast.
type networkSink struct {
	listener net.Listener
	mu       sync.Mutex
	lines    []string
}

func startNetworkSink(t *testing.T) *networkSink {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	sink := &networkSink{listener: listener}
	done := make(chan struct{})
	go func() {
		defer close(done)
		for {
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			_ = conn.SetDeadline(time.Now().Add(5 * time.Second))
			line, _ := bufio.NewReader(conn).ReadString('\n')
			sink.mu.Lock()
			sink.lines = append(sink.lines, strings.TrimSpace(line))
			sink.mu.Unlock()
			_, _ = io.WriteString(conn, "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
			_ = conn.Close()
		}
	}()
	t.Cleanup(func() {
		_ = listener.Close()
		<-done
	})
	return sink
}

func (s *networkSink) url() string { return "http://" + s.listener.Addr().String() }

// take returns the attempts logged so far and forgets them.
func (s *networkSink) take() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	lines := s.lines
	s.lines = nil
	return lines
}

// treeSnapshot maps every path under a root to its type, size and content
// hash: mtime alone is too coarse, and content alone misses a new empty
// directory.
type treeSnapshot map[string]string

func snapshotTree(t *testing.T, root string) treeSnapshot {
	t.Helper()
	snapshot := treeSnapshot{}
	err := filepath.WalkDir(root, func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, _ := filepath.Rel(root, path)
		switch {
		case entry.Type()&fs.ModeSymlink != 0:
			target, _ := os.Readlink(path)
			snapshot[rel] = "link " + target
		case entry.IsDir():
			snapshot[rel] = "dir"
		default:
			content, err := os.ReadFile(path)
			if err != nil {
				return err
			}
			sum := sha256.Sum256(content)
			snapshot[rel] = fmt.Sprintf("file %d %s", len(content), hex.EncodeToString(sum[:]))
		}
		return nil
	})
	if err != nil {
		t.Fatalf("snapshot %s: %v", root, err)
	}
	return snapshot
}

// changes lists every path added, removed or changed between two snapshots.
func (before treeSnapshot) changes(after treeSnapshot) []string {
	var out []string
	for path, was := range before {
		if now, ok := after[path]; !ok {
			out = append(out, "removed "+path)
		} else if now != was {
			out = append(out, "changed "+path)
		}
	}
	for path := range after {
		if _, ok := before[path]; !ok {
			out = append(out, "added "+path)
		}
	}
	sort.Strings(out)
	return out
}

// userGoEnv is a user environment with nothing pinned: the module proxy is
// the sink, the module cache is empty, no go env file, and goflags as given.
// GOTOOLCHAIN=local only so the sanity runs do not try a toolchain switch.
func userGoEnv(t *testing.T, sink *networkSink, goflags string) {
	t.Helper()
	modcache := t.TempDir()
	// The module cache holds read-only files when anything was extracted;
	// make it removable (runs before t.TempDir's own cleanup).
	t.Cleanup(func() {
		_ = filepath.WalkDir(modcache, func(path string, _ fs.DirEntry, _ error) error {
			_ = os.Chmod(path, 0o755)
			return nil
		})
	})
	t.Setenv("GOPROXY", sink.url())
	t.Setenv("GOMODCACHE", modcache)
	t.Setenv("GOFLAGS", goflags)
	t.Setenv("GOENV", "off")
	t.Setenv("GOWORK", "")
	t.Setenv("GO111MODULE", "")
	t.Setenv("GONOPROXY", "")
	t.Setenv("GOPRIVATE", "")
	t.Setenv("GONOSUMDB", "")
	t.Setenv("GOTOOLCHAIN", "local")
	t.Setenv("GOPACKAGESDRIVER", "")
	os.Unsetenv("GOPACKAGESDRIVER")
}

// writeTree writes files under a fresh temp dir and returns it.
func writeTree(t *testing.T, files map[string]string) string {
	t.Helper()
	root := t.TempDir()
	for name, contents := range files {
		writeFixtureFile(t, root, name, contents)
	}
	return root
}

// plainGoList runs `go list -e -deps ./...` in dir with the test's (the
// user's) environment and none of the plugin's settings.
func plainGoList(t *testing.T, dir string) string {
	t.Helper()
	cmd := exec.Command("go", "list", "-e", "-deps", "./...")
	cmd.Dir = dir
	out, _ := cmd.CombinedOutput()
	return string(out)
}

// fullIndexAndSemanticPass is what the daemon does on first open: the bulk
// index, then a whole-project semantic pass.
func fullIndexAndSemanticPass(t *testing.T, root string) []string {
	t.Helper()
	if _, err := runBulkIndex(root, io.Discard); err != nil {
		t.Fatalf("bulk index: %v", err)
	}
	diff, _, _ := newPluginState(root).handleSemanticPass(nil)
	return renderEdges(t, root, diff)
}

func requireEdge(t *testing.T, edges []string, want string) {
	t.Helper()
	for _, edge := range edges {
		if edge == want {
			return
		}
	}
	t.Fatalf("sanity: the semantic pass did not resolve %s, so it proves nothing; edges = %v", want, edges)
}

const plainSource = "type Plain struct{}\n\n" +
	"func (Plain) Seven() int { return 7 }\n\n" +
	"func use() int {\n\tvar p Plain\n\treturn p.Seven()\n}\n"

// A placeholder `h1:` hash: syntactically valid, matches nothing. The fetch
// happens before any verification, and the test asserts the attempt.
const placeholderHash = "h1:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="

func networkFixture() map[string]string {
	return map[string]string{
		"go.mod": "module example.com/gnet\n\ngo 1.22\n\nrequire example.com/gm512missing v1.0.0\n",
		"go.sum": "example.com/gm512missing v1.0.0 " + placeholderHash + "\n" +
			"example.com/gm512missing v1.0.0/go.mod " + placeholderHash + "\n",
		"a.go":      "package gnet\n\n" + plainSource,
		"uses/u.go": "package uses\n\nimport \"example.com/gm512missing\"\n\nvar _ = gm512missing.X\n",
	}
}

// A module missing from GOMODCACHE is not fetched: `go list` runs with
// GOPROXY=off, whatever the user's GOPROXY says. Control C4.
func TestSemanticPassFetchesNoModule(t *testing.T) {
	requireGoToolchain(t)
	sink := startNetworkSink(t)
	userGoEnv(t, sink, "")

	// Sanity: in this environment, plain `go list` asks the proxy.
	out := plainGoList(t, writeTree(t, networkFixture()))
	if attempts := sink.take(); len(attempts) == 0 {
		t.Fatalf("sanity: plain `go list` with GOPROXY=%s and an empty module cache made no request: %s",
			sink.url(), out)
	}

	root := writeTree(t, networkFixture())
	before := snapshotTree(t, root)
	edges := fullIndexAndSemanticPass(t, root)

	requireEdge(t, edges, "a.go:use CALLS example.com/gnet#Plain.Seven")
	if attempts := sink.take(); len(attempts) != 0 {
		t.Fatalf("the semantic pass reached the module proxy: %v", attempts)
	}
	if changes := before.changes(snapshotTree(t, root)); len(changes) != 0 {
		t.Fatalf("the semantic pass wrote under the root: %v", changes)
	}
}

// A go.mod `go list -mod=mod` would rewrite: no `go` line, and a `replace`d
// local module that is imported but not required. Both modules (the root
// and dep/) are loaded.
func rewriteFixture() map[string]string {
	return map[string]string{
		"go.mod":     "module example.com/gw\n\nreplace example.com/dep => ./dep\n",
		"a.go":       "package gw\n\n" + plainSource,
		"b/b.go":     "package b\n\nimport \"example.com/dep\"\n\nvar _ = dep.F\n",
		"dep/go.mod": "module example.com/dep\n",
		"dep/d.go":   "package dep\n\nfunc F() {}\n",
	}
}

// A user GOFLAGS=-mod=mod never rewrites go.mod or go.sum: the pass gives
// `go list` an explicit -mod=readonly, which overrides only GOFLAGS' -mod.
// Control C5.
func TestSemanticPassRewritesNoGoModUnderUserModMod(t *testing.T) {
	requireGoToolchain(t)
	sink := startNetworkSink(t)
	userGoEnv(t, sink, "-mod=mod")

	// Sanity: in this environment, plain `go list` rewrites go.mod.
	copyRoot := writeTree(t, rewriteFixture())
	copyBefore := snapshotTree(t, copyRoot)
	out := plainGoList(t, copyRoot)
	if changes := copyBefore.changes(snapshotTree(t, copyRoot)); !containsString(changes, "changed go.mod") {
		t.Fatalf("sanity: plain `go list` with GOFLAGS=-mod=mod did not rewrite go.mod (changes %v): %s",
			changes, out)
	}
	sink.take()

	root := writeTree(t, rewriteFixture())
	before := snapshotTree(t, root)
	edges := fullIndexAndSemanticPass(t, root)

	requireEdge(t, edges, "a.go:use CALLS example.com/gw#Plain.Seven")
	if changes := before.changes(snapshotTree(t, root)); len(changes) != 0 {
		t.Fatalf("the semantic pass wrote under the root: %v", changes)
	}
	if attempts := sink.take(); len(attempts) != 0 {
		t.Fatalf("the semantic pass reached the module proxy: %v", attempts)
	}
}

func containsString(list []string, want string) bool {
	for _, item := range list {
		if item == want {
			return true
		}
	}
	return false
}

const vendoredDep = "package vdep\n\nfunc Zero[X any]() X {\n\tvar x X\n\treturn x\n}\n"

const vendoredUser = "package gv\n\nimport \"example.com/vdep\"\n\n" +
	"func useDep() int {\n\tp := vdep.Zero[Plain]()\n\treturn p.Seven()\n}\n\n" + plainSource

// vendoredFixture is a module requiring example.com/vdep, which is only in
// vendor/: beside go.mod, or (workspace) beside a go.work using the module.
func vendoredFixture(workspace bool) (files map[string]string, moduleDir string) {
	goMod := "module example.com/gv\n\ngo 1.22\n\nrequire example.com/vdep v1.0.0\n"
	modulesTxt := "# example.com/vdep v1.0.0\n## explicit; go 1.22\nexample.com/vdep\n"
	if !workspace {
		return map[string]string{
			"go.mod":                          goMod,
			"a.go":                            vendoredUser,
			"vendor/modules.txt":              modulesTxt,
			"vendor/example.com/vdep/vdep.go": vendoredDep,
		}, ""
	}
	return map[string]string{
		"go.work":                         "go 1.22\n\nuse ./app\n",
		"app/go.mod":                      goMod,
		"app/a.go":                        vendoredUser,
		"vendor/modules.txt":              "## workspace\n" + modulesTxt,
		"vendor/example.com/vdep/vdep.go": vendoredDep,
	}, "app/"
}

// A vendored module or workspace loads its dependency from vendor/ with
// -mod=vendor: -mod=readonly would ignore vendor/ and need the module from
// the cache or the network. The user's GOFLAGS does the same and fetches:
// -mod=mod for the module, -mod=readonly for the workspace (where go
// rejects -mod=mod). `useDep`'s call is typed only through the vendored
// package. Control C6.
func TestSemanticPassLoadsAVendoredDependencyFromVendor(t *testing.T) {
	requireGoToolchain(t)
	for _, workspace := range []bool{false, true} {
		name := "module"
		if workspace {
			name = "workspace"
		}
		t.Run(name, func(t *testing.T) {
			goflags := "-mod=mod"
			if workspace {
				goflags = "-mod=readonly"
			}
			sink := startNetworkSink(t)
			userGoEnv(t, sink, goflags)
			files, moduleDir := vendoredFixture(workspace)

			// Sanity: in this environment, plain `go list` ignores vendor/
			// and asks the proxy.
			copyRoot := writeTree(t, files)
			out := plainGoList(t, filepath.Join(copyRoot, filepath.FromSlash(moduleDir)))
			if attempts := sink.take(); len(attempts) == 0 {
				t.Fatalf("sanity: plain `go list` with GOFLAGS=%s made no request: %s", goflags, out)
			}

			root := writeTree(t, files)
			before := snapshotTree(t, root)
			edges := fullIndexAndSemanticPass(t, root)

			requireEdge(t, edges, moduleDir+"a.go:use CALLS example.com/gv#Plain.Seven")
			requireEdge(t, edges, moduleDir+"a.go:useDep CALLS example.com/gv#Plain.Seven")
			if attempts := sink.take(); len(attempts) != 0 {
				t.Fatalf("the semantic pass reached the module proxy: %v", attempts)
			}
			if changes := before.changes(snapshotTree(t, root)); len(changes) != 0 {
				t.Fatalf("the semantic pass wrote under the root: %v", changes)
			}
		})
	}
}

// modFlag's answers outside the plain module case. Control C7: the ""
// cases.
func TestModFlagFollowsTheGoverningFile(t *testing.T) {
	root := writeTree(t, map[string]string{
		"mod/go.mod":                        "module example.com/m\n",
		"mod/nested/x.go":                   "package nested\n",
		"vmod/go.mod":                       "module example.com/v\n",
		"vmod/vendor/modules.txt":           "",
		"ws/go.work":                        "go 1.22\n\nuse ./a\n",
		"ws/vendor/modules.txt":             "",
		"ws/a/go.mod":                       "module example.com/a\n",
		"ws/a/vendor/modules.txt":           "",
		"ws/b/go.mod":                       "module example.com/b\n",
		"other/go.work":                     "go 1.22\n",
		"other/vendor/modules.txt":          "",
		"none/x.go":                         "package none\n",
		"dirvendor/go.mod":                  "module example.com/d\n",
		"dirvendor/vendor/modules.txt/keep": "",
	})
	// An empty go env file, so the user's `go env -w` settings stay out.
	goenv := filepath.Join(t.TempDir(), "go.env")
	if err := os.WriteFile(goenv, nil, 0o644); err != nil {
		t.Fatal(err)
	}
	env := func(vars map[string]string) []string {
		out := append(loadEnv(), "GOENV="+goenv, "GO111MODULE=", "GOWORK=")
		for name, value := range vars {
			out = append(out, name+"="+value)
		}
		return out
	}
	at := func(rel string) string { return filepath.Join(root, filepath.FromSlash(rel)) }
	for _, tc := range []struct {
		name string
		dir  string
		env  map[string]string
		want string
	}{
		{"plain module", "mod", nil, "-mod=readonly"},
		{"nested dir finds go.mod upward", "mod/nested", nil, "-mod=readonly"},
		{"vendored module", "vmod", nil, "-mod=vendor"},
		{"GO111MODULE=off", "mod", map[string]string{"GO111MODULE": "off"}, ""},
		{"GO111MODULE=off, vendored", "vmod", map[string]string{"GO111MODULE": "off"}, ""},
		{"no go.mod or go.work above", "none", nil, ""},
		{"vendor/modules.txt that is a dir", "dirvendor", nil, "-mod=readonly"},
		{"workspace vendor beside go.work", "ws/b", nil, "-mod=vendor"},
		{"GOWORK=off uses go.mod's vendor", "ws/a", map[string]string{"GOWORK": "off"}, "-mod=vendor"},
		{"GOWORK=off, unvendored module", "ws/b", map[string]string{"GOWORK": "off"}, "-mod=readonly"},
		{"GOWORK=<path> uses that file's dir", "mod", map[string]string{"GOWORK": at("other/go.work")}, "-mod=vendor"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := modFlag(at(tc.dir), env(tc.env)); got != tc.want {
				t.Fatalf("modFlag(%s) = %q, want %q", tc.dir, got, tc.want)
			}
		})
	}
}
