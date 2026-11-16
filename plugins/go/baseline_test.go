package main

import (
	"encoding/json"
	"os"
	"regexp"
	"slices"
	"strings"
	"testing"
)

var rustBaselineConst = regexp.MustCompile(`pub const BASELINE_EXCLUDED_DIRS: \[&str; \d+\] = (\[[^\]]*\]);`)

// rustBaseline reads BASELINE_EXCLUDED_DIRS out of a checked-in Rust source
// file as text, the way manifest_test.go reads plugin.toml.
func rustBaseline(t *testing.T, path string) []string {
	t.Helper()
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	match := rustBaselineConst.FindSubmatch(content)
	if match == nil {
		t.Fatalf("%s declares no BASELINE_EXCLUDED_DIRS", path)
	}
	var dirs []string
	if err := json.Unmarshal(match[1], &dirs); err != nil {
		t.Fatalf("%s's BASELINE_EXCLUDED_DIRS %s is not a string array: %v", path, match[1], err)
	}
	return dirs
}

// Control: drop ".claude" from baselineExcludedDirs -> both subtests fail.
func TestBaselineExcludedDirsEqualRustBaseline(t *testing.T) {
	for _, path := range []string{"../../walk/src/lib.rs"} {
		t.Run(strings.ReplaceAll(path, "/", "_"), func(t *testing.T) {
			want := rustBaseline(t, path)
			if !slices.Equal(baselineExcludedDirs, want) {
				t.Fatalf("baselineExcludedDirs %q must equal %s's BASELINE_EXCLUDED_DIRS %q", baselineExcludedDirs, path, want)
			}
		})
	}
}

// Control: drop ".claude" from baselineExcludedDirs -> the worktree's
// .go file is walked.
func TestWalkProjectFilesSkipsClaudeDir(t *testing.T) {
	root := t.TempDir()
	writeFile(t, root, "main.go", "package main\n")
	writeFile(t, root, ".claude/worktrees/agent/main.go", "package main\n")
	writeFile(t, root, "sub/.claude/x.go", "package x\n")

	assertWalk(t, root, []string{"main.go"})
}
