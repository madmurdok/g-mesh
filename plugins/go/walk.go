package main

// The project walk: which .go files this plugin ever produces a node for.
// Gitignore layering plus the hard-excluded-dirs set (ignore.go) and the
// symlink guard (symlinks.go), with the same link policy as the SDK's walk
// (plugins/sdk/src/walk.rs).

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
	var walked []string
	walkDir(real, nil, guard, &walked)

	// The winner pass: one spelling per real file. Walk order is
	// depth-first over sorted entries, so "first" is deterministic; a
	// spelling with no followed link above it replaces any earlier one.
	type winner struct {
		absPath string
		viaLink bool
	}
	winners := map[string]winner{}
	var order []string
	for _, absPath := range walked {
		realPath, viaLink := guard.realOf(absPath)
		current, seen := winners[realPath]
		if !seen {
			order = append(order, realPath)
		}
		if !seen || (current.viaLink && !viaLink) {
			winners[realPath] = winner{absPath: absPath, viaLink: viaLink}
		}
	}

	out := make([]string, 0, len(order))
	for _, realPath := range order {
		rel, err := filepath.Rel(real, winners[realPath].absPath)
		if err != nil {
			continue
		}
		out = append(out, filepath.ToSlash(rel))
	}
	sort.Strings(out)
	return out
}

// walkDir appends to walked the absolute, as-reached path of every .go file
// under dir the walk may yield, in walk order. A file reached through more
// than one spelling is appended once per spelling; walkProjectFiles picks
// the winner.
func walkDir(dir string, layers []*gitignoreLayer, guard *symlinkGuard, walked *[]string) {
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
		return // vanished/unreadable - nothing to yield
	}
	sort.Slice(entries, func(i, j int) bool { return entries[i].Name() < entries[j].Name() })

	for _, entry := range entries {
		// By name, before anything is resolved: a hard-excluded name is
		// excluded whatever it turns out to be, and settling that first
		// also saves resolving it.
		if hardExcludedDirs[entry.Name()] {
			continue
		}

		absPath := filepath.Join(dir, entry.Name())
		isLink := entry.Type()&os.ModeSymlink != 0
		var info os.FileInfo
		if isLink {
			// os.Stat follows the link: what it points at decides whether
			// it is walked as a directory or read as a file.
			info, err = os.Stat(absPath)
		} else {
			info, err = entry.Info()
		}
		if err != nil {
			continue // a dangling link, or vanished since ReadDir
		}
		isDir := info.IsDir()

		// Before the guard records anything: an ignored entry claims no
		// real path, so an ignored directory can still be reached through
		// a link to it whatever the two names sort as.
		if isIgnoredByLayers(layers, absPath, isDir) {
			continue
		}
		if isLink {
			if !guard.admitLink(absPath, isDir) {
				continue // cycle, entered directory, outside the root, excluded target
			}
		} else if isDir {
			guard.enterDir(absPath)
		}

		if isDir {
			walkDir(absPath, layers, guard, walked)
			continue
		}
		if !info.Mode().IsRegular() {
			continue // neither file nor directory - a socket, device, fifo
		}
		if !isGoFile(absPath) {
			continue
		}
		*walked = append(*walked, absPath)
	}
}

// plainWalkReaches reports whether walkDir, started at rootReal, reaches the
// .go file at rel (a root-relative path with no link in it) without following
// a link: no component is a hard-excluded name or gitignored. The real
// spelling of such a file is the one walkProjectFiles yields.
func plainWalkReaches(rootReal, rel string) bool {
	if !isGoFile(rel) {
		return false
	}
	parts := strings.Split(rel, string(filepath.Separator))
	var layers []*gitignoreLayer
	dir := rootReal
	for i, part := range parts {
		if layer := loadGitignoreLayer(dir); layer != nil {
			layers = append(layers, layer)
		}
		if hardExcludedDirs[part] {
			return false
		}
		dir = filepath.Join(dir, part)
		if isIgnoredByLayers(layers, dir, i < len(parts)-1) {
			return false
		}
	}
	return true
}
