package main

// The symlink guard for this plugin's project walk. Links are followed, under
// the same policy as the SDK's walk (plugins/sdk/src/walk.rs); why, and why
// these rules, is docs/adr/0025-project-walk-follows-symlinks.md.
//
// Invariants (walk.go's walkDir and walkProjectFiles hold the other half):
//
//   - A link is judged only after the hard-excluded names and .gitignore have
//     let it through: an ignored or excluded link is never followed, and a
//     gitignored *target* is not a reason to refuse one. Nothing is recorded
//     for an entry the walk skips.
//   - A link is followed only when its target resolves, its real path is
//     inside the root's real path, and no component of that path below the
//     root is a hard-excluded directory name. Anything else is refused and
//     contributes nothing; the walk goes on.
//   - A directory is entered through a link at most once, and never through a
//     link once it has been entered at all; a directory reached without a
//     link is always entered. Cycles therefore end: every ancestor of the
//     current position has been entered.
//   - An entry's identity is its real path: the followed link above it plus
//     the suffix below that link, never the spelling it was reached by.
//   - Every file appears once, keyed by its real path. Its spelling is the one
//     with no followed link above it when the plain walk reaches it, and the
//     first in walk order otherwise - sibling names never decide the identity
//     of a file the plain walk reaches.

import (
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

// symlinkGuard is stateful: one instance belongs to exactly one traversal
// and must not outlive it or be shared with another.
type symlinkGuard struct {
	// projectRootReal is both the walk's root and its real path: the walk
	// starts at the canonicalized root, so a path with no followed link
	// above it is already real.
	projectRootReal string
	// followed maps each followed link, as reached, to its real path.
	followed map[string]string
	// entered holds every directory entered, by real path. It is seeded
	// with the root, which no entry check would otherwise cover, so a link
	// back onto the root is refused like any other link onto an entered
	// directory.
	entered map[string]bool
}

func newSymlinkGuard(projectRootReal string) *symlinkGuard {
	return &symlinkGuard{
		projectRootReal: projectRootReal,
		followed:        map[string]string{},
		entered:         map[string]bool{projectRootReal: true},
	}
}

// realOf returns absPath's real path, and whether a followed link is above
// it (or is it). With no link followed yet it is absPath itself, no lookup.
func (g *symlinkGuard) realOf(absPath string) (string, bool) {
	if len(g.followed) == 0 {
		return absPath, false
	}
	for ancestor := absPath; ancestor != g.projectRootReal; {
		if real, ok := g.followed[ancestor]; ok {
			return real + absPath[len(ancestor):], true
		}
		parent := filepath.Dir(ancestor)
		if parent == ancestor {
			break
		}
		ancestor = parent
	}
	return absPath, false
}

// enterDir records a directory reached without being a link itself. It is
// always entered, even when its real path already was through a link: that
// is what lets the plain spelling of an aliased file be seen and win.
func (g *symlinkGuard) enterDir(absPath string) {
	real, _ := g.realOf(absPath)
	g.entered[real] = true
}

// admitLink answers whether the walk may follow the link at absPath, whose
// followed target is a directory when isDir. It is called only for a link
// the walk would otherwise step onto (not excluded, not ignored).
func (g *symlinkGuard) admitLink(absPath string, isDir bool) bool {
	real, err := filepath.EvalSymlinks(absPath)
	if err != nil {
		return false // dangling, or vanished since the caller's stat
	}
	rel, err := filepath.Rel(g.projectRootReal, real)
	if err != nil || rel == ".." || strings.HasPrefix(rel, ".."+string(filepath.Separator)) {
		return false // outside the project root
	}
	for _, part := range strings.Split(rel, string(filepath.Separator)) {
		if hardExcludedDirs[part] {
			return false // into a directory the walk never enters by name
		}
	}
	if isDir {
		if g.entered[real] {
			return false // an ancestor (a cycle), or a directory already walked
		}
		g.entered[real] = true
	}
	g.followed[absPath] = real
	return true
}
