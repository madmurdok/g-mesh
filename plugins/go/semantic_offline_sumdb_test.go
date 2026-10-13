package main

// The two ways around GOPROXY=off that go still has
// (docs/design/GM-512-plugins-no-writes-no-network.md, ADR 0033):
//
//   - a module matched by the user's GOPRIVATE or GONOPROXY is fetched
//     directly from its host, not through the proxy list;
//   - a checksum go records for a module already in GOMODCACHE (go.work.sum
//     in workspace mode) is looked up in the checksum database directly.
//
// Both go straight to a remote host, so the sink here is an HTTPS proxy
// (HTTPS_PROXY/HTTP_PROXY): every direct connection becomes a
// `CONNECT host:443` line in its log. Each test first runs `go list` under
// loadEnv() without the setting that closes the hole, on a separate copy of
// the fixture, and checks it really connects (the hole is open in this
// environment and the sink sees it).

import (
	"archive/zip"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// proxyThroughSink routes every direct HTTP(S) connection go makes through
// the sink. GOPROXY's own sink address is loopback, which go never proxies,
// so proxy requests still reach the sink directly.
func proxyThroughSink(t *testing.T, sink *networkSink) {
	t.Helper()
	for _, name := range []string{"HTTPS_PROXY", "HTTP_PROXY", "https_proxy", "http_proxy"} {
		t.Setenv(name, sink.url())
	}
	for _, name := range []string{"NO_PROXY", "no_proxy", "GOINSECURE", "GOSUMDB", "GOVCS"} {
		t.Setenv(name, "")
	}
	t.Setenv("GIT_TERMINAL_PROMPT", "0")
}

// loadEnvWithout is loadEnv() minus the entries given, so a sanity check can
// show what go does without one of them.
func loadEnvWithout(drop ...string) []string {
	var env []string
	for _, entry := range loadEnv() {
		if !containsString(drop, entry) {
			env = append(env, entry)
		}
	}
	return env
}

// goListUnder runs `go list -mod=readonly -e -deps ./...` in dir under env:
// the semantic pass's own command line for a module without vendor/.
func goListUnder(t *testing.T, dir string, env []string) string {
	t.Helper()
	cmd := exec.Command("go", "list", "-mod=readonly", "-e", "-deps", "./...")
	cmd.Dir = dir
	cmd.Env = env
	out, _ := cmd.CombinedOutput()
	return string(out)
}

// connectedTo reports whether the sink logged a direct connection to host.
func connectedTo(attempts []string, host string) bool {
	for _, line := range attempts {
		if strings.HasPrefix(line, "CONNECT "+host+":") {
			return true
		}
	}
	return false
}

// A module matched by the user's GOPRIVATE, or GONOPROXY, and missing from
// GOMODCACHE is not fetched from its host: the load runs with GONOPROXY=-,
// which matches no module path, so GOPROXY=off covers every module. The
// module's own packages still resolve. Control C12.
func TestSemanticPassFetchesNoPrivateModuleDirectly(t *testing.T) {
	requireGoToolchain(t)
	for _, setting := range []string{"GOPRIVATE", "GONOPROXY"} {
		t.Run(setting, func(t *testing.T) {
			sink := startNetworkSink(t)
			userGoEnv(t, sink, "")
			proxyThroughSink(t, sink)
			t.Setenv(setting, "example.com")

			// Sanity: without GONOPROXY=-, the load's own env fetches the
			// private module from example.com directly.
			out := goListUnder(t, writeTree(t, networkFixture()), loadEnvWithout("GONOPROXY=-"))
			if attempts := sink.take(); !connectedTo(attempts, "example.com") {
				t.Fatalf("sanity: `go list` with %s=example.com, GOPROXY=off and no GONOPROXY=- made no direct "+
					"connection to example.com (sink saw %v): %s", setting, attempts, out)
			}

			root := writeTree(t, networkFixture())
			before := snapshotTree(t, root)
			edges := fullIndexAndSemanticPass(t, root)

			requireEdge(t, edges, "a.go:use CALLS example.com/gnet#Plain.Seven")
			if attempts := sink.take(); len(attempts) != 0 {
				t.Fatalf("the semantic pass connected out with %s=example.com: %v", setting, attempts)
			}
			if changes := before.changes(snapshotTree(t, root)); len(changes) != 0 {
				t.Fatalf("the semantic pass wrote under the root: %v", changes)
			}
		})
	}
}

const cachedDepModule = "example.com/gm512cached"

const cachedDepSource = "package gm512cached\n\nfunc Zero[X any]() X {\n\tvar x X\n\treturn x\n}\n"

// fillModuleCache puts cachedDepModule v1.0.0 in the test's GOMODCACHE from
// a local file proxy, with GOSUMDB=off: no checksum database lookup is
// cached either, so a later lookup would have to go out.
func fillModuleCache(t *testing.T) {
	t.Helper()
	proxy := t.TempDir()
	versions := filepath.Join(proxy, filepath.FromSlash(cachedDepModule), "@v")
	goMod := "module " + cachedDepModule + "\n\ngo 1.22\n"
	writeFixtureFile(t, versions, "list", "v1.0.0\n")
	writeFixtureFile(t, versions, "v1.0.0.info", `{"Version":"v1.0.0","Time":"2024-01-01T00:00:00Z"}`)
	writeFixtureFile(t, versions, "v1.0.0.mod", goMod)

	zipFile, err := os.Create(filepath.Join(versions, "v1.0.0.zip"))
	if err != nil {
		t.Fatal(err)
	}
	archive := zip.NewWriter(zipFile)
	prefix := cachedDepModule + "@v1.0.0/"
	for name, contents := range map[string]string{"go.mod": goMod, "dep.go": cachedDepSource} {
		w, err := archive.Create(prefix + name)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := w.Write([]byte(contents)); err != nil {
			t.Fatal(err)
		}
	}
	if err := archive.Close(); err != nil {
		t.Fatal(err)
	}
	if err := zipFile.Close(); err != nil {
		t.Fatal(err)
	}

	cmd := exec.Command("go", "mod", "download", cachedDepModule+"@v1.0.0")
	cmd.Dir = t.TempDir()
	cmd.Env = append(os.Environ(), "GOPROXY="+fileURL(proxy), "GOSUMDB=off", "GOFLAGS=", "GOWORK=off")
	if out, err := cmd.CombinedOutput(); err != nil {
		t.Fatalf("sanity: filling the module cache from %s: %v: %s", proxy, err, out)
	}
}

// workspaceFixture is a go.work using one member that requires the cached
// module and has no go.sum. `useDep`'s call is typed only through it.
func workspaceFixture() map[string]string {
	return map[string]string{
		"go.work":    "go 1.22\n\nuse ./app\n",
		"app/go.mod": "module example.com/gw\n\ngo 1.22\n\nrequire " + cachedDepModule + " v1.0.0\n",
		"app/a.go": "package gw\n\nimport \"" + cachedDepModule + "\"\n\n" +
			"func useDep() int {\n\tp := gm512cached.Zero[Plain]()\n\treturn p.Seven()\n}\n\n" + plainSource,
	}
}

// The documented exception (ADR 0033): in workspace mode, a member whose
// go.sum lacks the entries for a module already in GOMODCACHE makes go
// record them in go.work.sum under the root, even with -mod=readonly. That
// file is the only change, and the checksums are not looked up in the
// checksum database: the load runs with GOSUMDB=off. Control C13 (the
// lookup); the write itself is go's and has no control.
func TestSemanticPassInAWorkspaceWritesOnlyGoWorkSum(t *testing.T) {
	requireGoToolchain(t)
	sink := startNetworkSink(t)
	userGoEnv(t, sink, "")
	proxyThroughSink(t, sink)
	fillModuleCache(t)

	// Sanity: without GOSUMDB=off, the load's own env looks the cached
	// module's checksums up in sum.golang.org.
	out := goListUnder(t, filepath.Join(writeTree(t, workspaceFixture()), "app"), loadEnvWithout("GOSUMDB=off"))
	if attempts := sink.take(); !connectedTo(attempts, "sum.golang.org") {
		t.Fatalf("sanity: `go list` in the workspace member without GOSUMDB=off made no connection to "+
			"sum.golang.org (sink saw %v): %s", attempts, out)
	}

	root := writeTree(t, workspaceFixture())
	before := snapshotTree(t, root)
	edges := fullIndexAndSemanticPass(t, root)

	requireEdge(t, edges, "app/a.go:use CALLS example.com/gw#Plain.Seven")
	requireEdge(t, edges, "app/a.go:useDep CALLS example.com/gw#Plain.Seven")
	if attempts := sink.take(); len(attempts) != 0 {
		t.Fatalf("the semantic pass connected out in the workspace: %v", attempts)
	}
	changes := before.changes(snapshotTree(t, root))
	switch {
	case len(changes) == 0:
		t.Fatalf("go no longer writes go.work.sum here (the exception in ADR 0033 is gone with this go " +
			"version): the root is unchanged. Drop the exception from ADR 0033 and make this test " +
			"assert no change at all")
	case len(changes) != 1 || changes[0] != "added go.work.sum":
		t.Fatalf("the semantic pass wrote more under the root than the documented go.work.sum: %v", changes)
	}
}

// fileURL is the file:// URL of the absolute path p, as cmd/go parses it on
// every OS: file:///tmp/x on Unix, file:///C:/x on Windows. Gluing
// "file://" to a Windows path puts the drive in the host ("file://C:/x"),
// which GOPROXY rejects as a file URL with non-path elements.
func fileURL(p string) string {
	slashed := filepath.ToSlash(p)
	if !strings.HasPrefix(slashed, "/") {
		slashed = "/" + slashed
	}
	return (&url.URL{Scheme: "file", Path: slashed}).String()
}
