package main

// The project walk: which .go files this plugin ever produces a node for.
// Mirrors plugins/typescript/src/bulkIndex.ts's walkProjectFiles - gitignore
// layering plus the hard-excluded-dirs set (ignore.go) and the symlink
// guard (symlinks.go), combined the same way.

import (
	"os"
	"path/filepath"
	"sort"
	"strings"
)

// isGoFile reports whether absOrRelPath's extension is claimed by this
// plugin - the one extension plugin.toml's [plugin.languages] declares.
func isGoFile(p string) bool {
	return strings.EqualFold(filepath.Ext(p), ".go")
}

// walkProjectFiles walks root depth-first and returns the project-relative
// POSIX paths of every .go file that is not gitignored, sorted for
// deterministic output (matching bulkIndex.ts's own sorted-directory-entries
// guarantee, which is what id-stability.bulk-repeat depends on holding
// across two runs, and what makes a diff between two runs of this plugin
// meaningful to a human reading its output).
func walkProjectFiles(root string) []string {
	real := canonicalizeProjectRoot(root)
	guard := newSymlinkGuard(real)
	var out []string
	walkDir(real, real, nil, guard, &out)
	sort.Strings(out)
	return out
}

func walkDir(root, dir string, layers []*gitignoreLayer, guard *symlinkGuard, out *[]string) {
	if layer := loadGitignoreLayer(dir); layer != nil {
		// A fresh slice per directory level, never appending onto a
		// parent's backing array: two sibling subdirectories must not
		// see each other's own .gitignore layer through aliasing.
		extended := make([]*gitignoreLayer, len(layers), len(layers)+1)
		copy(extended, layers)
		layers = append(extended, layer)
	}

	entries, err := os.ReadDir(dir)
	if err != nil {
		return // vanished/unreadable - nothing to yield, matches bulkIndex.ts
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })

	for _, entry := range entries {
		// By name, before anything is resolved: a hard-excluded name is
		// excluded whatever it turns out to be, and settling that first
		// also saves the guard's realpath call for it.
		if hardExcludedDirs[entry.Name()] {
			continue
		}

		resolved := guard.resolve(dir, entry)
		if resolved == nil {
			continue // cycle, duplicate real path, escapes root, or dangling
		}

		if resolved.isDir {
			if isIgnoredByLayers(layers, resolved.absPath, true) {
				continue
			}
			walkDir(root, resolved.absPath, layers, guard, out)
			continue
		}

		if !resolved.isFile {
			continue // neither file nor directory - a socket, device, fifo
		}
		if !isGoFile(resolved.absPath) {
			continue
		}
		if isIgnoredByLayers(layers, resolved.absPath, false) {
			continue
		}

		rel, err := filepath.Rel(root, resolved.absPath)
		if err != nil {
			continue
		}
		*out = append(*out, filepath.ToSlash(rel))
	}
}
