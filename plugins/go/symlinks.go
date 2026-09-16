package main

// The symlink policy for this plugin's project walk, mirroring
// plugins/typescript/src/symlinks.ts's own guard: symlinks are *followed*
// (a symlinked package - yarn/lerna-style linked packages, vendored shared
// code linked into a workspace - is an ordinary part of how monorepos are
// laid out, and skipping them would make a real, imported, in-tree package
// invisible with nothing saying why), but only under a guard that refuses
// three shapes: a link onto one of its own ancestors (infinite descent), a
// second path onto a real location some other path already claimed (the
// same file indexed twice), and a link resolving outside the project root
// (a path this index must never hold). A dangling link is skipped, not an
// error.

import (
	"os"
	"path/filepath"
	"strings"
)

// canonicalizeProjectRoot resolves root the same way symlinks.ts's
// canonicalizeProjectRoot does: absolute, then real (symlinks resolved).
// Every guard comparison below is made against this value. A root that
// does not exist yet is returned resolved-but-not-real - there is nothing
// to canonicalize, and the caller's own os.ReadDir/os.ReadFile already
// treats an unreadable root as an empty walk.
func canonicalizeProjectRoot(root string) string {
	abs, err := filepath.Abs(root)
	if err != nil {
		return root
	}
	real, err := filepath.EvalSymlinks(abs)
	if err != nil {
		return abs
	}
	return real
}

// resolvedEntry is one directory entry a traversal was allowed to step
// onto, with what it turned out to be once followed - see symlinkGuard's
// own doc comment for `absPath`'s "as-reached, not canonical" rule.
type resolvedEntry struct {
	absPath string
	isDir   bool
	isFile  bool
}

// symlinkGuard is stateful: one instance belongs to exactly one traversal
// and must not outlive it or be shared with another, the same contract
// symlinks.ts's SymlinkGuard documents.
type symlinkGuard struct {
	projectRootReal string
	claimed         map[string]bool
}

// newSymlinkGuard seeds `claimed` with projectRootReal itself - the one
// location no entry check would otherwise cover, since the root is never
// some parent directory's os.DirEntry. Without this, a link pointing back
// at the root (`sub/self -> ../..`) would be followed and the whole
// project walked a second time underneath it.
func newSymlinkGuard(projectRootReal string) *symlinkGuard {
	return &symlinkGuard{
		projectRootReal: projectRootReal,
		claimed:         map[string]bool{projectRootReal: true},
	}
}

// resolve answers "may this traversal step onto entry of parentAbsDir?" -
// the resolved entry, or nil when it must be skipped (a cycle, a second
// path onto an already-claimed target, an escape from the project root, or
// a dangling link).
func (g *symlinkGuard) resolve(parentAbsDir string, entry os.DirEntry) *resolvedEntry {
	absPath := filepath.Join(parentAbsDir, entry.Name())

	if entry.Type()&os.ModeSymlink == 0 {
		if g.claimed[absPath] {
			return nil
		}
		g.claimed[absPath] = true
		info, err := entry.Info()
		if err != nil {
			return nil // vanished between ReadDir and Info
		}
		return &resolvedEntry{absPath: absPath, isDir: info.IsDir(), isFile: info.Mode().IsRegular()}
	}

	real, err := filepath.EvalSymlinks(absPath)
	if err != nil {
		return nil // dangling symlink
	}

	rel, err := filepath.Rel(g.projectRootReal, real)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return nil // escapes the project root
	}
	if g.claimed[real] {
		return nil // cycle, or a second path onto an already-claimed target
	}

	// os.Stat follows the link - the "followed" answer, so a caller never
	// has to know it was looking at one.
	stat, err := os.Stat(absPath)
	if err != nil {
		return nil // vanished between EvalSymlinks and Stat
	}
	g.claimed[real] = true
	return &resolvedEntry{absPath: absPath, isDir: stat.IsDir(), isFile: stat.Mode().IsRegular()}
}
