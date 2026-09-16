package main

import (
	"path/filepath"
	"reflect"
	"testing"
)

func TestLoadWorkspaceSingleModule(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "go.mod", "module github.com/example/app\n\ngo 1.22\n")
	writeFile(t, root, "server/server.go", "package server\n")

	ws := loadWorkspace(root)
	if got := ws.importPath(""); got != "github.com/example/app" {
		t.Fatalf("root import path = %q", got)
	}
	if got := ws.importPath("server"); got != "github.com/example/app/server" {
		t.Fatalf("server import path = %q", got)
	}
	if got := ws.importPath("a/b/c"); got != "github.com/example/app/a/b/c" {
		t.Fatalf("nested import path = %q", got)
	}
}

// The multi-module case the design doc leaves as an open question, and the
// reason go.work exists at all: a nested module owns its own subtree, so a
// directory inside it is named by *its* module path, not by the enclosing
// one's. This is the discriminating test for the module boundary - the
// conformance fixture cannot be one, because a consistently wrong boundary
// still links a package to itself.
func TestLoadWorkspaceNestedModuleOwnsItsOwnSubtree(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "go.mod", "module github.com/example/app\n\ngo 1.22\n")
	writeFile(t, root, "go.work", "go 1.22\n\nuse (\n\t.\n\t./tools\n)\n")
	writeFile(t, root, "tools/go.mod", "module github.com/example/tools\n\ngo 1.22\n")
	writeFile(t, root, "tools/gen.go", "package tools\n")
	writeFile(t, root, "tools/sub/x.go", "package sub\n")

	ws := loadWorkspace(root)
	cases := map[string]string{
		"":          "github.com/example/app",
		"server":    "github.com/example/app/server",
		"tools":     "github.com/example/tools",
		"tools/sub": "github.com/example/tools/sub",
	}
	for dir, want := range cases {
		if got := ws.importPath(dir); got != want {
			t.Errorf("importPath(%q) = %q, want %q", dir, got, want)
		}
	}

	for _, path := range []string{"github.com/example/app", "github.com/example/app/server", "github.com/example/tools/sub"} {
		if !ws.isProjectImportPath(path) {
			t.Errorf("isProjectImportPath(%q) = false, want true", path)
		}
	}
	for _, path := range []string{"fmt", "github.com/example/apples", "github.com/other/app"} {
		if ws.isProjectImportPath(path) {
			t.Errorf("isProjectImportPath(%q) = true, want false", path)
		}
	}
}

// A nested module is found even without a go.work naming it: the go.mod walk
// is what makes multi-module repositories work, and go.work only adds to it.
func TestLoadWorkspaceFindsANestedModuleWithNoGoWork(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "go.mod", "module github.com/example/app\n")
	writeFile(t, root, "tools/go.mod", "module github.com/example/tools\n")

	if got := loadWorkspace(root).importPath("tools"); got != "github.com/example/tools" {
		t.Fatalf("importPath(tools) = %q", got)
	}
}

// A vendored dependency ships its own go.mod, and mistaking it for one of
// this project's modules would put every vendored package's import path into
// the graph under a module this project does not own.
func TestLoadWorkspaceIgnoresGoModUnderAnExcludedDirectory(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "go.mod", "module github.com/example/app\n")
	writeFile(t, root, "vendor/github.com/other/dep/go.mod", "module github.com/other/dep\n")
	writeFile(t, root, "testdata/fixture/go.mod", "module github.com/example/fixture\n")

	ws := loadWorkspace(root)
	if len(ws.modules) != 1 {
		t.Fatalf("modules = %+v, want only the root module", ws.modules)
	}
	if ws.isProjectImportPath("github.com/other/dep") {
		t.Fatal("a vendored module must not be read as one of this project's")
	}
}

// With no go.mod anywhere there is no import path to compute. The fallback
// is the directory itself, which still links same-package uses across the
// files of one directory - the property the container key exists for.
func TestImportPathFallsBackToTheDirectoryWithNoModule(t *testing.T) {
	ws := loadWorkspace(t.TempDir())
	if got := ws.importPath(""); got != "." {
		t.Fatalf("root fallback = %q, want \".\"", got)
	}
	if got := ws.importPath("internal/util"); got != "internal/util" {
		t.Fatalf("nested fallback = %q", got)
	}
	if ws.isProjectImportPath("internal/util") {
		t.Fatal("with no module, no import specifier can name this project")
	}
}

func TestReadModulePathAcceptsEverySpellingGoModAllows(t *testing.T) {
	cases := map[string]struct {
		content string
		want    string
		ok      bool
	}{
		"plain":            {"module github.com/example/app\n\ngo 1.22\n", "github.com/example/app", true},
		"quoted":           {"module \"github.com/example/app\"\n", "github.com/example/app", true},
		"block":            {"module (\n\tgithub.com/example/app\n)\n", "github.com/example/app", true},
		"leading comment":  {"// a comment\nmodule github.com/example/app\n", "github.com/example/app", true},
		"trailing comment": {"module github.com/example/app // why\n", "github.com/example/app", true},
		"indented":         {"\tmodule github.com/example/app\n", "github.com/example/app", true},
		"no directive":     {"go 1.22\n", "", false},
		"lookalike":        {"moduleFoo bar\n", "", false},
	}
	for name, c := range cases {
		t.Run(name, func(t *testing.T) {
			dir := t.TempDir()
			writeFile(t, dir, "go.mod", c.content)
			got, ok := readModulePath(dir)
			if ok != c.ok || got != c.want {
				t.Fatalf("readModulePath = (%q, %v), want (%q, %v)", got, ok, c.want, c.ok)
			}
		})
	}
}

func TestGoWorkUseDirs(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "go.work", "go 1.22\n\nuse ./one\n\nuse (\n\t.\n\t./two // with a comment\n\t\"./three\"\n)\n\nuse ../outside\n")

	got := goWorkUseDirs(root)
	want := []string{"", "one", "three", "two"} // sorted; "../outside" is dropped
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("goWorkUseDirs = %v, want %v", got, want)
	}
}

func TestGoWorkUseDirsWithNoGoWork(t *testing.T) {
	if got := goWorkUseDirs(t.TempDir()); got != nil {
		t.Fatalf("goWorkUseDirs = %v, want nil for a project with no go.work", got)
	}
}

func TestPackageNameFromPath(t *testing.T) {
	cases := map[string]string{
		"fmt":                           "fmt",
		"net/http":                      "http",
		"github.com/example/app/server": "server",
		"github.com/example/app/v2":     "app",
		"gopkg.in/yaml.v2":              "yaml",
		"github.com/x/v":                "v",
		"github.com/x/vx":               "vx",
	}
	for path, want := range cases {
		if got := packageNameFromPath(path); got != want {
			t.Errorf("packageNameFromPath(%q) = %q, want %q", path, got, want)
		}
	}
}

func TestRelativeDirRefusesAPathOutsideTheRoot(t *testing.T) {
	root := t.TempDir()
	if _, ok := relativeDir(root, filepath.Join(root, "..", "elsewhere")); ok {
		t.Fatal("a directory outside the project root must be refused")
	}
	if dir, ok := relativeDir(root, root); !ok || dir != "" {
		t.Fatalf("relativeDir(root, root) = (%q, %v), want (\"\", true)", dir, ok)
	}
}
