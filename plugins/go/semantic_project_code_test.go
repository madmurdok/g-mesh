package main

// The semantic tier's `go list` runs no toolchain and no packages driver the
// indexed project (or its PATH) names
// (docs/design/GM-513-semantic-servers-no-project-code.md, fixture `gfx`).

import (
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
)

const projectCodeEdge = "a.go:use CALLS example.com/gfx#Plain.M"

// writeHostileGoProject writes a module whose go.mod asks for a toolchain no
// machine has, so an unpinned `go` would try to download it.
func writeHostileGoProject(t *testing.T) string {
	t.Helper()
	root := t.TempDir()
	writeFixtureFile(t, root, "go.mod", "module example.com/gfx\n\ngo 1.22\n\ntoolchain go1.99.1\n")
	writeFixtureFile(t, root, "a.go", "package gfx\n\ntype Plain struct{}\n\n"+
		"func (Plain) M() string { return \"\" }\n\n"+
		"func use() string {\n\tvar p Plain\n\treturn p.M()\n}\n")
	return root
}

// hostileGoEnv is the environment of a user who has not pinned anything:
// toolchain switching on, no proxy to download from, no packages driver
// setting.
func hostileGoEnv(t *testing.T) {
	t.Helper()
	t.Setenv("GOTOOLCHAIN", "auto")
	t.Setenv("GOPROXY", "off")
	t.Setenv("GOFLAGS", "")
	t.Setenv("GOPACKAGESDRIVER", "")
	os.Unsetenv("GOPACKAGESDRIVER")
}

func semanticPassEdges(t *testing.T, root string) (fileChangeDiff, string) {
	t.Helper()
	diff, reason, _ := newPluginState(root).handleSemanticPass(nil)
	return diff, reason
}

func assertPlainResolved(t *testing.T, root string, diff fileChangeDiff, reason string) {
	t.Helper()
	if reason != "" {
		t.Fatalf("the semantic pass did not load the project: %s", reason)
	}
	got := renderEdges(t, root, diff)
	for _, edge := range got {
		if edge == projectCodeEdge {
			return
		}
	}
	t.Fatalf("edges = %v, want %s among them", got, projectCodeEdge)
}

// go.mod's `toolchain go1.99.1` is never switched to: the pass loads with
// the installed toolchain.
func TestSemanticPassRunsNoToolchainTheProjectNames(t *testing.T) {
	requireGoToolchain(t)
	root := writeHostileGoProject(t)
	hostileGoEnv(t)

	// Sanity: an unpinned `go` in this module does try to switch.
	list := exec.Command("go", "list", "./...")
	list.Dir = root
	out, err := list.CombinedOutput()
	if err == nil || !strings.Contains(string(out), "go1.99.1") {
		t.Fatalf("sanity: `go list` with GOTOOLCHAIN=auto did not try go1.99.1 (err %v): %s", err, out)
	}

	diff, reason := semanticPassEdges(t, root)
	assertPlainResolved(t, root, diff, reason)
}

// A `gopackagesdriver` on PATH never replaces `go list`.
func TestSemanticPassRunsNoPackagesDriverFromPath(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("the driver shim is a shell script")
	}
	requireGoToolchain(t)
	root := writeHostileGoProject(t)
	// No toolchain line here: this test is about the driver alone.
	writeFixtureFile(t, root, "go.mod", "module example.com/gfx\n\ngo 1.22\n")
	hostileGoEnv(t)

	shims := t.TempDir()
	marker := filepath.Join(shims, "driver-ran")
	driver := filepath.Join(shims, "gopackagesdriver")
	if err := os.WriteFile(driver, []byte("#!/bin/sh\necho \"$@\" >> '"+marker+"'\nexit 1\n"), 0o755); err != nil {
		t.Fatalf("write %s: %v", driver, err)
	}
	t.Setenv("PATH", shims+string(os.PathListSeparator)+os.Getenv("PATH"))

	// Sanity: the shim is the driver PATH finds, and it marks when run.
	if found, err := exec.LookPath("gopackagesdriver"); err != nil || found != driver {
		t.Fatalf("sanity: gopackagesdriver on PATH is %q (%v), want %s", found, err, driver)
	}
	_ = exec.Command(driver).Run()
	if _, err := os.Stat(marker); err != nil {
		t.Fatalf("sanity: the driver shim wrote no marker: %v", err)
	}
	if err := os.Remove(marker); err != nil {
		t.Fatalf("remove %s: %v", marker, err)
	}

	diff, reason := semanticPassEdges(t, root)
	if log, err := os.ReadFile(marker); err == nil {
		t.Fatalf("the semantic pass ran the gopackagesdriver on PATH: %s", log)
	}
	assertPlainResolved(t, root, diff, reason)
}
