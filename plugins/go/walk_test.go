package main

import (
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"testing"
	"time"
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

// A directory reached both directly and through a symlink to it yields each
// file once, under its real spelling: the plain walk reaches real/, so its
// spelling wins even though "linked" sorts first (symlinks.go's invariants,
// docs/adr/0025-project-walk-follows-symlinks.md).
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
	want := []string{"real/pkg.go"}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("walkProjectFiles = %v, want %v (the plain walk reaches real/, so its spelling wins over linked/)", got, want)
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

// Symlinks (docs/adr/0025-project-walk-follows-symlinks.md): the same
// behaviours the SDK walk's tests pin (B-numbers from
// docs/architecture/gm-349-sdk-walk-symlinks.md, section 7), named after them
// so a drift between the two walks shows. Each names the production change
// that makes it fail.

// linkAt creates a symlink at root/at whose stored target is target,
// verbatim (a relative target resolves against the link's own directory).
func linkAt(t *testing.T, root, target, at string) {
	t.Helper()
	path := filepath.Join(root, filepath.FromSlash(at))
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("MkdirAll: %v", err)
	}
	if err := os.Symlink(target, path); err != nil {
		t.Fatalf("Symlink: %v", err)
	}
}

func skipWithoutSymlinks(t *testing.T) {
	t.Helper()
	if runtime.GOOS == "windows" {
		t.Skip("symlink creation needs elevated privileges on Windows")
	}
}

func assertWalk(t *testing.T, root string, want []string) {
	t.Helper()
	got := walkProjectFiles(root)
	if len(got) == 0 && len(want) == 0 {
		return
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("walkProjectFiles = %v, want %v", got, want)
	}
}

// B1: a gitignored directory reached only through a link is walked under
// the link's spelling, whichever side of the link it sorts on.
//
// Control: in walkDir, run the guard (admitLink/enterDir) before
// isIgnoredByLayers -> the real-src order returns [].
func TestAGitignoredTargetReachedOnlyThroughALinkIsWalkedUnderTheLinkSpelling(t *testing.T) {
	skipWithoutSymlinks(t)
	for _, target := range []string{"real-src", "z-src"} {
		t.Run(target, func(t *testing.T) {
			root := t.TempDir()
			writeFile(t, root, ".gitignore", target+"/\n")
			writeFile(t, root, target+"/pkg.go", "package p\n")
			linkAt(t, root, "../"+target, "src/linked")
			assertWalk(t, root, []string{"src/linked/pkg.go"})
		})
	}
}

// B2: links sorting before their
// plainly walked target add nothing; the real spelling is listed.
//
// Control: in walkProjectFiles' winner pass keep the first spelling in walk
// order (drop the `current.viaLink && !viaLink` replacement) -> the
// packages/... spellings are listed.
func TestALinkToAWalkedDirectoryAddsNothingAndTheRealSpellingWins(t *testing.T) {
	skipWithoutSymlinks(t)
	root := t.TempDir()
	writeFile(t, root, "lib/real-lib/index.go", "package p\n")
	linkAt(t, root, "../lib/real-lib", "packages/lib")
	writeFile(t, root, "lib/shared/thing.go", "package p\n")
	linkAt(t, root, "../lib/shared", "packages/a-dup")
	linkAt(t, root, "../lib/shared", "packages/dup")
	// "lib" sorts before "packages"; a target sorting after its links:
	writeFile(t, root, "z/x.go", "package p\n")
	linkAt(t, root, "z", "a")
	assertWalk(t, root, []string{"lib/real-lib/index.go", "lib/shared/thing.go", "z/x.go"})
}

// B3: a file link to a walked file is listed once, under
// the real spelling, whether the link's name sorts before or after it.
//
// Control: key the winner pass by spelling (`realPath := absPath`) -> the
// link's spelling is listed too.
func TestAFileLinkToAWalkedFileIsIndexedOnce(t *testing.T) {
	skipWithoutSymlinks(t)
	for _, alias := range []string{"alias.go", "z-alias.go"} {
		t.Run(alias, func(t *testing.T) {
			root := t.TempDir()
			writeFile(t, root, "src/index.go", "package p\n")
			linkAt(t, root, "index.go", "src/"+alias)
			assertWalk(t, root, []string{"src/index.go"})
		})
	}
}

// B4: two links into one ignored target, one to a subdirectory of the
// other's: each file once, under the first link to reach it.
//
// Control: make symlinkGuard.realOf return its argument -> b/x.go is listed
// beside a/sub/x.go.
func TestNestedLinksIntoOneIgnoredTargetListEachFileOnce(t *testing.T) {
	skipWithoutSymlinks(t)
	root := t.TempDir()
	writeFile(t, root, ".gitignore", "zlib/\n")
	writeFile(t, root, "zlib/sub/x.go", "package p\n")
	linkAt(t, root, "zlib", "a")
	linkAt(t, root, "zlib/sub", "b")
	assertWalk(t, root, []string{"a/sub/x.go"})
}

// B5: links to the containing directory, to the root and
// to an already-walked sibling terminate and add nothing. Under a deadline,
// so a regression fails rather than hangs.
//
// Control: drop the `g.entered[real]` refusal in admitLink -> the walk does
// not end and the deadline fails the test.
func TestALinkToAnAncestorTerminatesAndAddsNothing(t *testing.T) {
	skipWithoutSymlinks(t)
	root := t.TempDir()
	writeFile(t, root, "cycle/a.go", "package p\n")
	linkAt(t, root, ".", "cycle/loop")
	writeFile(t, root, "d/x.go", "package p\n")
	linkAt(t, root, "..", "d/up")
	writeFile(t, root, "e/y.go", "package p\n")
	linkAt(t, root, "../d", "e/tod")
	linkAt(t, root, ".", "self")

	done := make(chan []string, 1)
	go func() { done <- walkProjectFiles(root) }()
	select {
	case got := <-done:
		want := []string{"cycle/a.go", "d/x.go", "e/y.go"}
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("walkProjectFiles = %v, want %v", got, want)
		}
	case <-time.After(30 * time.Second):
		t.Fatal("a walk over link cycles must terminate")
	}
}

// B6: directory and file links outside the root are
// refused.
//
// Control: drop admitLink's Rel/".." check -> out/o.go and o.go are listed.
func TestALinkOutsideTheRootIsRefused(t *testing.T) {
	skipWithoutSymlinks(t)
	outside := t.TempDir()
	writeFile(t, outside, "o.go", "package p\n")
	root := t.TempDir()
	writeFile(t, root, "a.go", "package p\n")
	linkAt(t, root, outside, "out")
	linkAt(t, root, filepath.Join(outside, "o.go"), "o.go")
	assertWalk(t, root, []string{"a.go"})
}

// B7: dangling directory-shaped and file links are skipped
// and the walk continues past them.
//
// Control: ignore walkDir's os.Stat error for a link -> nil-pointer panic.
func TestADanglingLinkIsSkipped(t *testing.T) {
	skipWithoutSymlinks(t)
	root := t.TempDir()
	writeFile(t, root, "a.go", "package p\n")
	linkAt(t, root, "nowhere", "0dangling")
	linkAt(t, root, "nowhere.go", "0dangling.go")
	writeFile(t, root, "z/b.go", "package p\n")
	assertWalk(t, root, []string{"a.go", "z/b.go"})
}

// B8: a link whose target passes through a hard-excluded directory is
// refused (Go's set: .git, vendor, testdata), and a link named like one is
// never resolved.
//
// Control: drop admitLink's hardExcludedDirs component check ->
// linkdep/dep.go and dep.go are listed.
func TestALinkIntoAnExcludedDirectoryIsRefusedAndOneNamedExcludedIsNeverResolved(t *testing.T) {
	skipWithoutSymlinks(t)
	root := t.TempDir()
	writeFile(t, root, "vendor/dep/dep.go", "package p\n")
	writeFile(t, root, "lib/l.go", "package p\n")
	linkAt(t, root, "vendor/dep", "linkdep")
	linkAt(t, root, "vendor/dep/dep.go", "dep.go")
	linkAt(t, root, "../lib", "x/testdata")
	assertWalk(t, root, []string{"lib/l.go"})
}

// B11: a root given through a link keeps B2 and B5, and paths are relative
// to the root as given.
//
// Control: in walkProjectFiles, walk the root as given while the guard keeps
// the canonical one (walkDir(root, ...) beside newSymlinkGuard(real)) -> the
// plain and link spellings get different real paths: [a/x.go z/x.go].
func TestARootGivenThroughALinkKeepsRealWinsAndRelativePaths(t *testing.T) {
	skipWithoutSymlinks(t)
	base := t.TempDir()
	writeFile(t, base, "real-root/z/x.go", "package p\n")
	linkAt(t, base, "z", "real-root/a")
	linkAt(t, base, "..", "real-root/z/up")
	linkAt(t, base, "real-root", "via")
	assertWalk(t, filepath.Join(base, "via"), []string{"z/x.go"})
}
