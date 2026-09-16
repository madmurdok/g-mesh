package main

// The Go workspace model: how a project-relative *directory* becomes a Go
// *import path*, which is this plugin's container key
// (docs/architecture/multi-language-plugins.md, "Data Model > Logical
// containers": Go's container key is the import path, and Go packages are
// flat, so a container never has a parent).
//
// An import path is `<module path> + "/" + <directory relative to the module
// root>`, so the only thing this file has to work out is which module a
// directory belongs to and what that module is called. Both come out of
// go.mod's `module` line; `go.work`'s `use` directives are what makes a
// repository hold more than one of them.
//
// # Why this is parsed by hand rather than with golang.org/x/mod
//
// `golang.org/x/mod/modfile` is the real parser and would be the obvious
// dependency - but plugins/go/go.mod is deliberately at zero requirements
// (GM-279's decision 3 made the same call for .gitignore matching, with the
// full trade-off in ignore.go's own doc comment), and the two directives
// this plugin actually reads - `module` in go.mod and `use` in go.work - are
// a line-oriented subset with a stable, tiny grammar: an optional block
// form, optionally quoted paths, `//` line comments. What a hand parser
// would get wrong compared to modfile is everything this plugin never asks
// about (`require`, `replace`, `exclude`, retractions, version semantics).
//
// # Why no `go list`
//
// `go list -m` would answer this exactly, and would also mean running the Go
// toolchain on the hot path of every bulk index and, worse, making the
// *structural* tier depend on a toolchain being installed - which the design
// doc rules out directly ("Semantic tiers may depend on a toolchain being
// present. Structural tiers may not."). The toolchain arrives with GM-281's
// go/types pass, where it is allowed to.

import (
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
)

// moduleRoot is one go.mod: the directory holding it and the module path it
// declares.
type moduleRoot struct {
	// Project-relative POSIX directory, "" for the project root itself.
	dir string
	// The `module` line's path, e.g. "github.com/example/app".
	path string
}

// workspace is the whole project's module layout, as one immutable value.
// It is rebuilt from scratch on `workspaceChanged` (control.go) rather than
// patched, because a go.mod edit can rename a module - which moves every
// container key under it - and recomputing is cheaper than reasoning about
// which ones moved.
type workspace struct {
	// Longest `dir` first, so importPath's first match is the *innermost*
	// module containing a directory. A nested module (tools/go.mod inside
	// the root module's tree) owns its own subtree, exactly as the go
	// command treats it.
	modules []moduleRoot
}

// loadWorkspace reads every go.mod in the project, plus go.work's `use`
// directives, and returns the module layout they describe.
//
// Both sources are read, not just one: a `go.work` is not required for a
// multi-module repository (nested go.mod files alone make one), and a
// `go.work` may name a directory this walk would otherwise skip. Reading
// both and taking the union means a repository is understood whether or not
// it has a workspace file. A `use` directive pointing *outside* the project
// root (`use ../sibling`) is kept only if it resolves to a directory under
// the root: nothing outside the root is ever walked or indexed, so a module
// there could not gain members anyway.
func loadWorkspace(root string) *workspace {
	byDir := map[string]string{}

	for _, dir := range goWorkUseDirs(root) {
		if path, ok := readModulePath(filepath.Join(root, filepath.FromSlash(dir))); ok {
			byDir[dir] = path
		}
	}

	// Every go.mod in the tree. `.git`, `vendor` and `testdata` are skipped
	// for the same reasons walk.go skips them - a vendored dependency's own
	// go.mod must never be mistaken for one of this project's modules - and
	// so are dot-directories, which the walk never yields files from either.
	_ = filepath.WalkDir(root, func(abs string, entry fs.DirEntry, err error) error {
		if err != nil {
			return nil //nolint:nilerr // an unreadable subtree yields no modules, it is not fatal
		}
		if entry.IsDir() {
			if abs == root {
				return nil
			}
			if hardExcludedDirs[entry.Name()] || strings.HasPrefix(entry.Name(), ".") {
				return fs.SkipDir
			}
			return nil
		}
		if entry.Name() != "go.mod" {
			return nil
		}
		dir, ok := relativeDir(root, filepath.Dir(abs))
		if !ok {
			return nil
		}
		if path, ok := readModulePath(filepath.Dir(abs)); ok {
			byDir[dir] = path
		}
		return nil
	})

	modules := make([]moduleRoot, 0, len(byDir))
	for dir, path := range byDir {
		modules = append(modules, moduleRoot{dir: dir, path: path})
	}
	// Longest directory first; ties broken by name so the ordering is total
	// and two runs over the same tree agree (the id-stability.bulk-repeat
	// check reaches this through every container key).
	sort.Slice(modules, func(i, j int) bool {
		if len(modules[i].dir) != len(modules[j].dir) {
			return len(modules[i].dir) > len(modules[j].dir)
		}
		return modules[i].dir < modules[j].dir
	})
	return &workspace{modules: modules}
}

// importPath returns the Go import path of a project-relative directory -
// this plugin's container key for every declaration in it.
//
// With no go.mod anywhere above the directory there is no import path to
// compute, and the fallback is the project-relative directory itself ("."
// for the root). That is a container key that is stable, unique within the
// project and still links same-package uses across the files of one
// directory, which is the whole point of the key; what it cannot do is match
// an `import` specifier, so a file outside that directory never links into
// it. That is the right failure - a directory with no module is not
// importable in Go either.
func (w *workspace) importPath(relDir string) string {
	relDir = normalizeDir(relDir)
	for _, module := range w.modules {
		suffix, ok := underDir(module.dir, relDir)
		if !ok {
			continue
		}
		if suffix == "" {
			return module.path
		}
		return module.path + "/" + suffix
	}
	if relDir == "" {
		return "."
	}
	return relDir
}

// isProjectImportPath reports whether an import specifier names a package of
// this project - i.e. whether it lies under one of the modules found above.
//
// It deliberately does *not* check that the directory exists: whether an
// address is in the index is core's question, not a plugin's
// (core/src/graph/imports.rs's module doc, "Why here and not in the
// plugin"). An import of a package that was never walked simply leaves its
// placeholder unlinked, which is the documented degradation, while a
// filesystem check here would make extraction depend on the tree still
// being on disk in the shape the walk saw it.
func (w *workspace) isProjectImportPath(path string) bool {
	for _, module := range w.modules {
		if path == module.path || strings.HasPrefix(path, module.path+"/") {
			return true
		}
	}
	return false
}

// underDir reports whether `child` is `dir` or lies under it, and returns
// the remainder. `dir == ""` is the project root and contains everything.
func underDir(dir, child string) (string, bool) {
	if dir == "" {
		return child, true
	}
	if child == dir {
		return "", true
	}
	if strings.HasPrefix(child, dir+"/") {
		return child[len(dir)+1:], true
	}
	return "", false
}

// normalizeDir turns filepath.Dir's answers ("." for the root, native
// separators) into this file's convention: POSIX separators, "" for the
// project root.
func normalizeDir(dir string) string {
	dir = filepath.ToSlash(dir)
	if dir == "." || dir == "/" {
		return ""
	}
	return strings.TrimSuffix(dir, "/")
}

// relativeDir expresses an absolute directory relative to the project root,
// in this file's convention. Returns false for a directory outside the root.
func relativeDir(root, abs string) (string, bool) {
	rel, err := filepath.Rel(root, abs)
	if err != nil {
		return "", false
	}
	rel = filepath.ToSlash(rel)
	if rel == ".." || strings.HasPrefix(rel, "../") {
		return "", false
	}
	return normalizeDir(rel), true
}

// readModulePath reads `<dir>/go.mod` and returns its `module` path.
//
// Both spellings the go.mod grammar allows are accepted: the one-line form
// (`module github.com/x/y`) and the parenthesized block form
// (`module (\n\tgithub.com/x/y\n)`), with the path optionally quoted.
func readModulePath(dir string) (string, bool) {
	content, err := os.ReadFile(filepath.Join(dir, "go.mod"))
	if err != nil {
		return "", false
	}

	inBlock := false
	for _, raw := range strings.Split(string(content), "\n") {
		line := stripLineComment(raw)
		if line == "" {
			continue
		}
		if inBlock {
			if line == ")" {
				return "", false
			}
			return unquotePath(firstField(line)), true
		}
		rest, ok := directiveArgs(line, "module")
		if !ok {
			continue
		}
		if rest == "(" {
			inBlock = true
			continue
		}
		if rest == "" {
			continue
		}
		return unquotePath(firstField(rest)), true
	}
	return "", false
}

// goWorkUseDirs reads `<root>/go.work`'s `use` directives and returns the
// project-relative directories they name - the modules a multi-module
// repository is built from.
//
// A repository with no go.work returns nothing, which is not an error: the
// go.mod walk in loadWorkspace finds every module either way, and go.work is
// only consulted so that a workspace naming a module the walk would skip is
// still understood.
func goWorkUseDirs(root string) []string {
	content, err := os.ReadFile(filepath.Join(root, "go.work"))
	if err != nil {
		return nil
	}

	var dirs []string
	add := func(raw string) {
		path := unquotePath(firstField(raw))
		if path == "" {
			return
		}
		abs := filepath.Join(root, filepath.FromSlash(path))
		if dir, ok := relativeDir(root, abs); ok {
			dirs = append(dirs, dir)
		}
	}

	inBlock := false
	for _, raw := range strings.Split(string(content), "\n") {
		line := stripLineComment(raw)
		if line == "" {
			continue
		}
		if inBlock {
			if line == ")" {
				inBlock = false
				continue
			}
			add(line)
			continue
		}
		rest, ok := directiveArgs(line, "use")
		if !ok {
			continue
		}
		if rest == "(" {
			inBlock = true
			continue
		}
		add(rest)
	}
	sort.Strings(dirs)
	return dirs
}

// directiveArgs matches a go.mod/go.work directive keyword at the start of a
// line and returns whatever follows it. The keyword must be a whole word: a
// `moduleFoo` line is not a `module` directive.
func directiveArgs(line, keyword string) (string, bool) {
	if !strings.HasPrefix(line, keyword) {
		return "", false
	}
	rest := line[len(keyword):]
	if rest == "" {
		return "", true
	}
	if rest[0] != ' ' && rest[0] != '\t' && rest[0] != '(' {
		return "", false
	}
	return strings.TrimSpace(rest), true
}

// stripLineComment removes a `//` comment and surrounding whitespace. go.mod
// has no block comments, so this is the whole comment grammar.
func stripLineComment(line string) string {
	if at := strings.Index(line, "//"); at >= 0 {
		line = line[:at]
	}
	return strings.TrimSpace(line)
}

func firstField(line string) string {
	fields := strings.Fields(line)
	if len(fields) == 0 {
		return ""
	}
	return fields[0]
}

// unquotePath accepts both the bare and the quoted spelling of a path in
// go.mod/go.work. A quoted path that will not unquote is returned as it was
// written rather than dropped: a path this plugin cannot read is still a
// better container key than none.
func unquotePath(field string) string {
	if len(field) >= 2 && (field[0] == '"' || field[0] == '`') {
		if unquoted, err := strconv.Unquote(field); err == nil {
			return unquoted
		}
	}
	return field
}
