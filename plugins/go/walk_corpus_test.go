package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"testing"
)

// testdata/walk.json is written by plugins/sdk/tests/go_walk_corpus.rs from
// what sdk::walk_project answers for each tree, which also asserts the file is
// current. This test builds the same trees and holds walkProjectFiles to the
// same list. A file path starting with "../" lands beside the project root.
type walkCorpusCase struct {
	Name       string   `json:"name"`
	Files      []string `json:"files"`
	Gitignores []struct {
		Dir  string `json:"dir"`
		Text string `json:"text"`
	} `json:"gitignores"`
	Links []struct {
		Link   string `json:"link"`
		Target string `json:"target"`
	} `json:"links"`
	Expect []string `json:"expect"`
}

func TestWalkProjectFilesMatchesTheSDKWalkerCorpus(t *testing.T) {
	data, err := os.ReadFile("testdata/walk.json")
	if err != nil {
		t.Fatalf("read corpus: %v", err)
	}
	var corpus struct {
		Cases []walkCorpusCase `json:"cases"`
	}
	if err := json.Unmarshal(data, &corpus); err != nil {
		t.Fatalf("parse corpus: %v", err)
	}
	if len(corpus.Cases) == 0 {
		t.Fatal("corpus has no cases")
	}
	for _, c := range corpus.Cases {
		t.Run(c.Name, func(t *testing.T) {
			if len(c.Links) > 0 && runtime.GOOS == "windows" {
				t.Skip("symlinks need privileges on Windows")
			}
			root := filepath.Join(t.TempDir(), "root")
			if err := os.MkdirAll(root, 0o755); err != nil {
				t.Fatal(err)
			}
			for _, file := range c.Files {
				path := filepath.Join(root, filepath.FromSlash(file))
				mustMkdir(t, filepath.Dir(path))
				if err := os.WriteFile(path, []byte("package p\n"), 0o644); err != nil {
					t.Fatal(err)
				}
			}
			for _, g := range c.Gitignores {
				dir := filepath.Join(root, filepath.FromSlash(g.Dir))
				mustMkdir(t, dir)
				if err := os.WriteFile(filepath.Join(dir, ".gitignore"), []byte(g.Text), 0o644); err != nil {
					t.Fatal(err)
				}
			}
			for _, l := range c.Links {
				path := filepath.Join(root, filepath.FromSlash(l.Link))
				mustMkdir(t, filepath.Dir(path))
				if err := os.Symlink(filepath.FromSlash(l.Target), path); err != nil {
					t.Fatal(err)
				}
			}
			got := walkProjectFiles(root)
			if got == nil {
				got = []string{}
			}
			if !reflect.DeepEqual(got, c.Expect) {
				t.Errorf("walkProjectFiles = %v, the SDK walker lists %v", got, c.Expect)
			}
		})
	}
}

func mustMkdir(t *testing.T, dir string) {
	t.Helper()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatal(err)
	}
}
