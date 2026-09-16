package main

import (
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"testing"
)

func TestWalkProjectFilesHonorsGitignoreAndHardExcludedDirs(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "main.go", "package main\n")
	writeFile(t, root, "internal/util.go", "package internal\n")
	writeFile(t, root, "internal/util_test.go", "package internal\n")
	writeFile(t, root, ".gitignore", "generated.go\n")
	writeFile(t, root, "generated.go", "package main\n")
	writeFile(t, root, "vendor/dep/dep.go", "package dep\n")
	writeFile(t, root, "testdata/fixture.go", "package testdata\n")
	writeFile(t, root, ".git/HEAD", "ref: refs/heads/main\n")
	writeFile(t, root, "README.md", "not go\n")

	got := walkProjectFiles(root)
	want := []string{"internal/util.go", "internal/util_test.go", "main.go"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("walkProjectFiles = %v, want %v", got, want)
	}
}

func TestWalkProjectFilesHonorsNestedGitignore(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "a.go", "package root\n")
	writeFile(t, root, "sub/.gitignore", "skip.go\n")
	writeFile(t, root, "sub/skip.go", "package sub\n")
	writeFile(t, root, "sub/keep.go", "package sub\n")

	got := walkProjectFiles(root)
	want := []string{"a.go", "sub/keep.go"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("walkProjectFiles = %v, want %v", got, want)
	}
}

// A directory reached both directly and through a symlink to it is walked
// exactly once, under whichever path sibling order (sorted, so this is
// deterministic) reaches its real location first - see symlinks.go's own
// doc comment on newSymlinkGuard/resolve. "linked" sorts before "real"
// alphabetically, so it claims real/'s content and "real" itself is then
// refused as a second path onto an already-claimed location - the
// documented trade-off, not a bug: the alternative (indexing the same
// file twice, under two ids derived from two different paths) is worse.
func TestWalkProjectFilesFollowsSymlinkedDirectoryOnce(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("symlink creation needs elevated privileges on Windows")
	}
	root := t.TempDir()
	writeFile(t, root, "real/pkg.go", "package real\n")
	if err := os.Symlink(filepath.Join(root, "real"), filepath.Join(root, "linked")); err != nil {
		t.Fatalf("Symlink: %v", err)
	}

	got := walkProjectFiles(root)
	want := []string{"linked/pkg.go"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("walkProjectFiles = %v, want %v (linked/ sorts first and claims real/'s content; real/ is then a duplicate)", got, want)
	}
}

func TestWalkProjectFilesRefusesASymlinkCycle(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("symlink creation needs elevated privileges on Windows")
	}
	root := t.TempDir()
	writeFile(t, root, "a.go", "package a\n")
	if err := os.MkdirAll(filepath.Join(root, "loop"), 0o755); err != nil {
		t.Fatalf("MkdirAll: %v", err)
	}
	// loop/self points back at loop's own parent (the project root),
	// which the guard has already claimed - an attempt at a cycle.
	if err := os.Symlink(root, filepath.Join(root, "loop", "self")); err != nil {
		t.Fatalf("Symlink: %v", err)
	}

	// Must terminate at all (a real cycle would hang a naive walk) and
	// must not index a.go a second time under loop/self/a.go.
	got := walkProjectFiles(root)
	want := []string{"a.go"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("walkProjectFiles = %v, want %v", got, want)
	}
}

func TestWalkProjectFilesSkipsADanglingSymlink(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("symlink creation needs elevated privileges on Windows")
	}
	root := t.TempDir()
	writeFile(t, root, "a.go", "package a\n")
	if err := os.Symlink(filepath.Join(root, "does-not-exist.go"), filepath.Join(root, "dangling.go")); err != nil {
		t.Fatalf("Symlink: %v", err)
	}

	got := walkProjectFiles(root)
	want := []string{"a.go"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("walkProjectFiles = %v, want %v", got, want)
	}
}

func TestIsGoFileIsCaseInsensitiveOnExtension(t *testing.T) {
	if !isGoFile("a.go") || !isGoFile("A.GO") {
		t.Error("expected .go and .GO to both match")
	}
	if isGoFile("a.py") {
		t.Error("a.py must not match")
	}
}
