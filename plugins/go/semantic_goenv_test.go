package main

// ADR 0033: the -mod flag the semantic pass gives `go list` follows
// go's *effective* environment, which includes the go env file (GOENV,
// `go env -w`), not only the process environment. A flag chosen from the
// process environment alone breaks the load when the go env file says
// GO111MODULE=off (go rejects any -mod outside module mode) or GOWORK=off
// (a workspace member must not get the workspace's -mod=vendor).

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

// goEnvFile writes a go env file with the given lines and points GOENV at
// it for the rest of the test.
func goEnvFile(t *testing.T, lines ...string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "go.env")
	if err := os.WriteFile(path, []byte(strings.Join(lines, "\n")+"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	t.Setenv("GOENV", path)
}

// goEnvIn is `go env <name>` from dir under the test's environment, plus
// extra overrides.
func goEnvIn(t *testing.T, dir, name string, extra ...string) string {
	t.Helper()
	cmd := exec.Command("go", "env", name)
	cmd.Dir = dir
	cmd.Env = append(os.Environ(), extra...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("sanity: go env %s in %s: %v: %s", name, dir, err, out)
	}
	return strings.TrimSpace(string(out))
}

// GO111MODULE=off in the go env file only: the package under GOPATH loads
// in GOPATH mode, with no -mod flag. Its go.mod is ignored by go in GOPATH
// mode but is still there for a flag chosen from the process environment to
// find, so that flag would be -mod=readonly, which go rejects outright.
// Control C8.
func TestSemanticPassLoadsGOPATHModeFromTheGoEnvFile(t *testing.T) {
	requireGoToolchain(t)
	sink := startNetworkSink(t)
	userGoEnv(t, sink, "")
	goEnvFile(t, "GO111MODULE=off")
	gopath, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("GOPATH", gopath)
	root := filepath.Join(gopath, "src", "example.com", "gp")
	for name, contents := range map[string]string{
		"go.mod": "module example.com/gp\n\ngo 1.22\n",
		"a.go":   "package gp\n\n" + plainSource,
	} {
		writeFixtureFile(t, root, name, contents)
	}

	// Sanity: the go env file is in force, and go rejects a -mod flag here.
	if got := goEnvIn(t, root, "GO111MODULE"); got != "off" {
		t.Fatalf("sanity: go env GO111MODULE = %q, want off from the go env file", got)
	}
	if got := goEnvIn(t, root, "GOMOD"); got != "" {
		t.Fatalf("sanity: go env GOMOD = %q, want \"\" (GOPATH mode)", got)
	}
	cmd := exec.Command("go", "list", "-mod=readonly", "./...")
	cmd.Dir = root
	if out, err := cmd.CombinedOutput(); err == nil {
		t.Fatalf("sanity: `go list -mod=readonly` succeeded in GOPATH mode: %s", out)
	}

	before := snapshotTree(t, root)
	edges := fullIndexAndSemanticPass(t, root)

	if !containsString(edges, "a.go:use CALLS example.com/gp#Plain.Seven") {
		t.Fatalf("the semantic pass did not load the GOPATH-mode package; edges = %v", edges)
	}
	if changes := before.changes(snapshotTree(t, root)); len(changes) != 0 {
		t.Fatalf("the semantic pass wrote under the root: %v", changes)
	}
	if attempts := sink.take(); len(attempts) != 0 {
		t.Fatalf("the semantic pass reached the module proxy: %v", attempts)
	}
}

// goWorkOffFixture is a workspace whose go.work has vendor/modules.txt
// beside it, using app/, which has no vendor/ of its own and reaches its
// dependency through a directory `replace`. Under GOWORK=off app/ is its own
// main module: it loads with -mod=readonly, while the workspace's
// -mod=vendor fails on app's go.mod ("inconsistent vendoring").
func goWorkOffFixture() map[string]string {
	return map[string]string{
		"go.work": "go 1.22\n\nuse ./app\n",
		"vendor/modules.txt": "## workspace\n# example.com/vdep v1.0.0\n## explicit; go 1.22\n" +
			"example.com/vdep\n",
		"vendor/example.com/vdep/vdep.go": vendoredDep,
		"app/go.mod": "module example.com/gv\n\ngo 1.22\n\nrequire example.com/vdep v1.0.0\n\n" +
			"replace example.com/vdep => ../vdep\n",
		"app/a.go":    vendoredUser,
		"vdep/go.mod": "module example.com/vdep\n\ngo 1.22\n",
		"vdep/d.go":   vendoredDep,
	}
}

// GOWORK=off in the go env file only: a member of a vendored workspace
// loads as its own module, with its edges, and its go.mod and go.sum stay
// as they were. Control C8.
func TestSemanticPassLoadsAWorkspaceMemberUnderGoWorkOffFromTheGoEnvFile(t *testing.T) {
	requireGoToolchain(t)
	sink := startNetworkSink(t)
	userGoEnv(t, sink, "")
	goEnvFile(t, "GOWORK=off")
	root := writeTree(t, goWorkOffFixture())
	app := filepath.Join(root, "app")

	// Sanity: the go env file is in force, and without it app/ is a member
	// of the vendored workspace.
	if got := goEnvIn(t, app, "GOWORK"); got != "off" {
		t.Fatalf("sanity: go env GOWORK = %q, want off from the go env file", got)
	}
	if got := goEnvIn(t, app, "GOWORK", "GOENV=off"); filepath.Base(got) != "go.work" {
		t.Fatalf("sanity: without the go env file, go env GOWORK = %q, want the fixture's go.work", got)
	}

	before := snapshotTree(t, root)
	edges := fullIndexAndSemanticPass(t, root)

	for _, want := range []string{
		"app/a.go:use CALLS example.com/gv#Plain.Seven",
		"app/a.go:useDep CALLS example.com/gv#Plain.Seven",
	} {
		if !containsString(edges, want) {
			t.Fatalf("the semantic pass did not resolve %s under GOWORK=off; edges = %v", want, edges)
		}
	}
	if changes := before.changes(snapshotTree(t, root)); len(changes) != 0 {
		t.Fatalf("the semantic pass wrote under the root: %v", changes)
	}
	if attempts := sink.take(); len(attempts) != 0 {
		t.Fatalf("the semantic pass reached the module proxy: %v", attempts)
	}
}

// modFlagFor's answers for go's resolved GOWORK and GOMOD. Control C9: the
// GOPATH-mode case with a GOWORK still set.
func TestModFlagForResolvedGoEnv(t *testing.T) {
	root := writeTree(t, map[string]string{
		"vmod/go.mod":             "module example.com/v\n",
		"vmod/vendor/modules.txt": "",
		"mod/go.mod":              "module example.com/m\n",
		"ws/go.work":              "go 1.22\n",
		"ws/vendor/modules.txt":   "",
		"bare/go.work":            "go 1.22\n",
	})
	at := func(rel string) string { return filepath.Join(root, filepath.FromSlash(rel)) }
	for _, tc := range []struct {
		name, gowork, gomod, want string
	}{
		{"GOPATH mode", "", "", ""},
		{"GOPATH mode, GOWORK=off", "off", "", ""},
		{"GOPATH mode, GOWORK still set", at("ws/go.work"), "", ""},
		{"module mode, no go.mod", "", os.DevNull, ""},
		{"module mode, no go.mod, GOWORK=off", "off", os.DevNull, ""},
		{"go.mod", "", at("mod/go.mod"), "-mod=readonly"},
		{"vendored go.mod", "", at("vmod/go.mod"), "-mod=vendor"},
		{"GOWORK=off, vendored go.mod", "off", at("vmod/go.mod"), "-mod=vendor"},
		{"vendored go.work governs", at("ws/go.work"), at("mod/go.mod"), "-mod=vendor"},
		{"unvendored go.work governs", at("bare/go.work"), at("vmod/go.mod"), "-mod=readonly"},
		{"go.work with no go.mod", at("ws/go.work"), os.DevNull, "-mod=vendor"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if got := modFlagFor(tc.gowork, tc.gomod); got != tc.want {
				t.Fatalf("modFlagFor(%q, %q) = %q, want %q", tc.gowork, tc.gomod, got, tc.want)
			}
		})
	}
}

// When `go env` cannot answer (fails, or prints something that is not its
// JSON), modFlag says -mod=readonly, the flag that never writes. Control
// C10.
func TestModFlagFallsBackToReadonlyWhenGoEnvFails(t *testing.T) {
	dir := writeTree(t, map[string]string{"go.mod": "module example.com/m\n"})
	for _, tc := range []struct{ name, script string }{
		{"go env fails", "#!/bin/sh\nexit 1\n"},
		{"go env prints no JSON", "#!/bin/sh\necho not-json\n"},
	} {
		t.Run(tc.name, func(t *testing.T) {
			bin := t.TempDir()
			fake := filepath.Join(bin, "go")
			if err := os.WriteFile(fake, []byte(tc.script), 0o755); err != nil {
				t.Fatal(err)
			}
			t.Setenv("PATH", bin)
			// Sanity: the fake is the `go` modFlag runs.
			if found, err := exec.LookPath("go"); err != nil || found != fake {
				t.Fatalf("sanity: LookPath(go) = %q, %v; want %s", found, err, fake)
			}
			if got := modFlag(dir, os.Environ()); got != "-mod=readonly" {
				t.Fatalf("modFlag with a broken `go env` = %q, want -mod=readonly", got)
			}
		})
	}
}
