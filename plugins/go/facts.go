package main

// The resolution facts of a workspace (goFacts) and what changed between two
// of them (resolutionDeltaFor): which files a go.mod/go.work save re-keys, and
// which importers it can make resolve differently.
//
// Design: docs/architecture/gm-509-selective-config-reindex.md, section 3.6
// and its GM-545 paragraph.
//
//   - **What extraction reads from the model** is a directory's import path
//     (workspace.importPath, every container key) and whether an import path
//     lies under one of the modules (workspace.isProjectImportPath). Both are
//     functions of the `[(dir, modulePath)]` list alone, so `require`, `go`,
//     `toolchain`, `exclude` and `retract` edits answer `unchanged`.
//   - **`replace` and go.work `use` are facts too**, although extraction never
//     reads them: the semantic tier's `packages.Load` honours both, so a
//     changed one selects the importers of the module it names, which core's
//     scoped semanticPass then re-answers.
//   - **Every module changed answers `unknown`**: a single-module repository's
//     rename (or its first or last go.mod) moves every container key, which
//     the selection would only send over the threshold to the whole-language
//     reindex anyway.
//   - **Importers are matched by their text.** A Go import specifier is the
//     import path, which is also the placeholder's container scope, so a
//     `specifier` selector finds both a linked and an unlinked import.

import (
	"encoding/json"
	"fmt"
	"sort"
	"strings"
)

// factsFormat is the facts' format. A blob of another version is unreadable,
// which answers `unknown`.
const factsFormat = 1

// goFacts is what a workspace's resolution reads, as core stores it (an
// opaque JSON string).
type goFacts struct {
	Format   int           `json:"format"`
	Modules  []factModule  `json:"modules"`
	Uses     []string      `json:"uses"`
	Replaces []factReplace `json:"replaces"`
}

type factModule struct {
	Dir  string `json:"dir"`
	Path string `json:"path"`
}

type factReplace struct {
	File string `json:"file"`
	From string `json:"from"`
	Rule string `json:"rule"`
}

// factsOf returns the facts of ws, sorted so two loads of one tree encode
// the same.
func factsOf(ws *workspace) goFacts {
	facts := goFacts{
		Format:   factsFormat,
		Modules:  make([]factModule, 0, len(ws.modules)),
		Uses:     append([]string{}, ws.uses...),
		Replaces: make([]factReplace, 0, len(ws.replaces)),
	}
	for _, module := range ws.modules {
		facts.Modules = append(facts.Modules, factModule{Dir: module.dir, Path: module.path})
	}
	sort.Slice(facts.Modules, func(i, j int) bool { return facts.Modules[i].Dir < facts.Modules[j].Dir })
	sort.Strings(facts.Uses)
	for _, rule := range ws.replaces {
		facts.Replaces = append(facts.Replaces, factReplace{File: rule.file, From: rule.from, Rule: rule.rule})
	}
	return facts
}

// encodeFacts is the blob core stores: the bulk trailer and a
// resolutionChanged answer both carry it.
func encodeFacts(ws *workspace) string {
	body, err := json.Marshal(factsOf(ws))
	if err != nil {
		// Unreachable: strings and ints only.
		return ""
	}
	return string(body)
}

// decodeFacts returns the facts a blob holds, or false for one of another
// format or no JSON at all.
func decodeFacts(blob string) (goFacts, bool) {
	var facts goFacts
	if err := json.Unmarshal([]byte(blob), &facts); err != nil || facts.Format != factsFormat {
		return goFacts{}, false
	}
	return facts, true
}

// --- the delta's wire shape (wire/src/lib.rs, ResolutionDelta) -----------

type resolutionDelta struct {
	Kind    string           `json:"kind"`
	Reason  string           `json:"reason,omitempty"`
	Files   []pathScope      `json:"files,omitempty"`
	Imports []importSelector `json:"imports,omitempty"`
}

type pathScope struct {
	Under    string   `json:"under"`
	NotUnder []string `json:"notUnder,omitempty"`
}

type importSelector struct {
	Importers pathScope   `json:"importers"`
	By        importMatch `json:"by"`
}

// importMatch is `{"specifier": <matcher>}`, the only form this plugin sends.
type importMatch struct {
	Specifier underMatcher `json:"specifier"`
}

// underMatcher is `{"under": {"prefix", "separator"}}`.
type underMatcher struct {
	Under underArgs `json:"under"`
}

type underArgs struct {
	Prefix    string `json:"prefix"`
	Separator string `json:"separator"`
}

func unchangedDelta() resolutionDelta { return resolutionDelta{Kind: "unchanged"} }

func unknownDelta(reason string) resolutionDelta {
	return resolutionDelta{Kind: "unknown", Reason: reason}
}

// specifierUnder selects, across the whole project, the importers of the
// module path `path` or of a package under it.
func specifierUnder(path string) importSelector {
	return importSelector{
		Importers: pathScope{Under: ""},
		By:        importMatch{Specifier: underMatcher{Under: underArgs{Prefix: path, Separator: "/"}}},
	}
}

// resolutionDeltaFor answers resolutionChanged: what changed between the
// facts the index was built from (`previous`, nil when core holds none) and
// the reloaded workspace `ws`.
func resolutionDeltaFor(previous *string, ws *workspace) resolutionDelta {
	if previous == nil {
		return unknownDelta("no previous resolution facts")
	}
	old, ok := decodeFacts(*previous)
	if !ok {
		return unknownDelta("the previous resolution facts are unreadable")
	}
	return factsDelta(old, factsOf(ws))
}

// factsDelta is resolutionDeltaFor between two decoded facts:
//
//   - every module dir whose path changed, appeared or disappeared: the files
//     under it and not under a deeper module dir (old or new), whose container
//     keys move; and the importers of its old and new import path, whose
//     imports change between a project package and an external one, or
//     name a key that appeared or disappeared;
//   - every such dir being a module dir of the project: `unknown` (see the
//     file doc);
//   - a `use` added or removed: the importers of that dir's module path;
//   - a module path whose `replace` directives changed: its importers.
//
// Equal facts answer `unchanged`.
func factsDelta(old, new goFacts) resolutionDelta {
	oldByDir := modulesByDir(old.Modules)
	newByDir := modulesByDir(new.Modules)

	allDirs := map[string]bool{}
	for dir := range oldByDir {
		allDirs[dir] = true
	}
	for dir := range newByDir {
		allDirs[dir] = true
	}
	var changed []string
	for dir := range allDirs {
		before, hadBefore := oldByDir[dir]
		after, hasAfter := newByDir[dir]
		if hadBefore != hasAfter || before != after {
			changed = append(changed, dir)
		}
	}
	sort.Strings(changed)

	if len(changed) > 0 && len(changed) == len(allDirs) {
		return unknownDelta(fmt.Sprintf(
			"every module of the project changed (%s): every container key moves", strings.Join(quoteDirs(changed), ", ")))
	}

	var files []pathScope
	prefixes := map[string]bool{}
	for _, dir := range changed {
		var nested []string
		for other := range allDirs {
			if other != dir {
				if _, under := underDir(dir, other); under {
					nested = append(nested, other)
				}
			}
		}
		sort.Strings(nested)
		files = append(files, pathScope{Under: dir, NotUnder: nested})
		// The dir's key before and after: its own module's path when it has
		// one, else the enclosing module's path plus the dir. A dir under no
		// module keys by its own relative path, which no import can name.
		for _, byDir := range []map[string]string{oldByDir, newByDir} {
			if key, ok := importPathIn(byDir, dir); ok {
				prefixes[key] = true
			}
		}
	}

	oldUses := setOf(old.Uses)
	newUses := setOf(new.Uses)
	for dir := range symmetricDifference(oldUses, newUses) {
		if path, ok := oldByDir[dir]; ok {
			prefixes[path] = true
		}
		if path, ok := newByDir[dir]; ok {
			prefixes[path] = true
		}
	}

	oldReplaces := replacesByModule(old.Replaces)
	newReplaces := replacesByModule(new.Replaces)
	for from := range unionKeys(oldReplaces, newReplaces) {
		if oldReplaces[from] != newReplaces[from] {
			prefixes[from] = true
		}
	}

	if len(files) == 0 && len(prefixes) == 0 {
		return unchangedDelta()
	}
	imports := make([]importSelector, 0, len(prefixes))
	for _, prefix := range sortedKeys(prefixes) {
		imports = append(imports, specifierUnder(prefix))
	}
	return resolutionDelta{Kind: "affected", Files: files, Imports: imports}
}

func modulesByDir(modules []factModule) map[string]string {
	byDir := make(map[string]string, len(modules))
	for _, module := range modules {
		byDir[module.Dir] = module.Path
	}
	return byDir
}

// importPathIn is workspace.importPath over a dir->path map: the innermost
// module containing dir, and false when none does.
func importPathIn(byDir map[string]string, dir string) (string, bool) {
	best, bestPath, found := "", "", false
	for moduleDir, path := range byDir {
		if _, under := underDir(moduleDir, dir); !under {
			continue
		}
		if !found || len(moduleDir) > len(best) {
			best, bestPath, found = moduleDir, path, true
		}
	}
	if !found {
		return "", false
	}
	suffix, _ := underDir(best, dir)
	if suffix == "" {
		return bestPath, true
	}
	return bestPath + "/" + suffix, true
}

// replacesByModule groups the replace directives by the module they replace,
// as one comparable string per module.
func replacesByModule(rules []factReplace) map[string]string {
	grouped := map[string][]string{}
	for _, rule := range rules {
		grouped[rule.From] = append(grouped[rule.From], rule.File+"\x00"+rule.Rule)
	}
	byModule := make(map[string]string, len(grouped))
	for from, entries := range grouped {
		sort.Strings(entries)
		byModule[from] = strings.Join(entries, "\n")
	}
	return byModule
}

func setOf(values []string) map[string]bool {
	set := make(map[string]bool, len(values))
	for _, value := range values {
		set[value] = true
	}
	return set
}

func symmetricDifference(a, b map[string]bool) map[string]bool {
	diff := map[string]bool{}
	for value := range a {
		if !b[value] {
			diff[value] = true
		}
	}
	for value := range b {
		if !a[value] {
			diff[value] = true
		}
	}
	return diff
}

func unionKeys(a, b map[string]string) map[string]bool {
	keys := map[string]bool{}
	for key := range a {
		keys[key] = true
	}
	for key := range b {
		keys[key] = true
	}
	return keys
}

// quoteDirs names module dirs for a log line, "" as the project root.
func quoteDirs(dirs []string) []string {
	named := make([]string, len(dirs))
	for i, dir := range dirs {
		if dir == "" {
			named[i] = "<root>"
		} else {
			named[i] = dir
		}
	}
	return named
}
