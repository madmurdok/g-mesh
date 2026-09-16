package main

import (
	"os"
	"path/filepath"
	"testing"
)

func writeFile(t *testing.T, dir, rel, content string) string {
	t.Helper()
	path := filepath.Join(dir, filepath.FromSlash(rel))
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("MkdirAll: %v", err)
	}
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatalf("WriteFile: %v", err)
	}
	return path
}

func TestIsIgnoredByLayersLiteralAndWildcardPatterns(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, ".gitignore", "*.log\n/config.local.go\nbuild/\n")
	layer := loadGitignoreLayer(root)
	if layer == nil {
		t.Fatal("expected a layer to load")
	}
	layers := []*gitignoreLayer{layer}

	cases := []struct {
		rel   string
		isDir bool
		want  bool
	}{
		{"debug.log", false, true},               // *.log, basename anywhere
		{"nested/debug.log", false, true},        // *.log at depth
		{"config.local.go", false, true},         // anchored, root only
		{"nested/config.local.go", false, false}, // anchored pattern does not match nested copy
		{"build", true, true},                    // dirOnly, matches the directory
		{"build", false, false},                  // dirOnly must not match a file of the same name
		{"main.go", false, false},                // not matched by anything
	}
	for _, c := range cases {
		got := isIgnoredByLayers(layers, filepath.Join(root, filepath.FromSlash(c.rel)), c.isDir)
		if got != c.want {
			t.Errorf("isIgnoredByLayers(%q, isDir=%v) = %v, want %v", c.rel, c.isDir, got, c.want)
		}
	}
}

func TestIsIgnoredByLayersNegationOverridesAnEarlierMatch(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, ".gitignore", "*.log\n!keep.log\n")
	layer := loadGitignoreLayer(root)
	layers := []*gitignoreLayer{layer}

	if !isIgnoredByLayers(layers, filepath.Join(root, "debug.log"), false) {
		t.Error("debug.log should be ignored")
	}
	if isIgnoredByLayers(layers, filepath.Join(root, "keep.log"), false) {
		t.Error("keep.log should be un-ignored by the negated pattern")
	}
}

// A deeper .gitignore's pattern overrides an ancestor's for paths under
// it - the same "later/deeper layer wins" rule ignorePolicy.ts's
// isIgnoredByLayers documents, exercised here across two real layers
// (both loaded from disk) rather than one.
func TestIsIgnoredByLayersDeeperLayerOverridesAncestor(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, ".gitignore", "*.go\n")
	writeFile(t, root, "keep/.gitignore", "!important.go\n")

	rootLayer := loadGitignoreLayer(root)
	keepLayer := loadGitignoreLayer(filepath.Join(root, "keep"))
	layers := []*gitignoreLayer{rootLayer, keepLayer}

	if !isIgnoredByLayers(layers, filepath.Join(root, "other.go"), false) {
		t.Error("other.go should still be ignored by the root layer")
	}
	if isIgnoredByLayers(layers, filepath.Join(root, "keep", "important.go"), false) {
		t.Error("keep/important.go should be un-ignored by the deeper layer's negation")
	}
}

func TestIsIgnoredByLayersDoubleStarMatchesAnyDepth(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, ".gitignore", "**/tmp/**\n")
	layer := loadGitignoreLayer(root)
	layers := []*gitignoreLayer{layer}

	if !isIgnoredByLayers(layers, filepath.Join(root, "a", "tmp", "b", "c.go"), false) {
		t.Error("a/tmp/b/c.go should be ignored by **/tmp/**")
	}
	if isIgnoredByLayers(layers, filepath.Join(root, "a", "tmp2", "c.go"), false) {
		t.Error("a/tmp2/c.go should not be ignored - tmp2 is not tmp")
	}
}

func TestLoadGitignoreLayerReturnsNilWhenAbsent(t *testing.T) {
	root := t.TempDir()
	if loadGitignoreLayer(root) != nil {
		t.Error("expected nil for a directory with no .gitignore")
	}
}

func TestCompilePatternSkipsBlankLinesAndComments(t *testing.T) {
	for _, line := range []string{"", "   ", "# a comment", "  # indented comment"} {
		if _, ok := compilePattern(line); ok {
			t.Errorf("compilePattern(%q) should not produce a pattern", line)
		}
	}
}
