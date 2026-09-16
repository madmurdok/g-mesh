package main

// A hand-rolled subset of .gitignore matching, mirroring
// plugins/typescript/src/ignorePolicy.ts's shape (per-directory layers,
// later/deeper layer overrides earlier, negation) rather than its
// implementation (which delegates the pattern language itself to the npm
// `ignore` package).
//
// Decision: hand-rolled rather than a dependency. `go.mod` for this plugin
// has zero requirements today, which is worth keeping for a scaffold whose
// only job is to prove the shape/stream/id/ownership contract with File
// nodes - and the pattern subset actually needed (literal segments, `*`
// and `?` wildcards within a segment, `**` across segments, a
// directory-only trailing `/`, an anchoring leading `/` or embedded `/`,
// negation with `!`, later-line-wins within one file) is exactly what the
// TS plugin's own fixtures exercise and what real-world .gitignore files
// overwhelmingly use. What is deliberately not supported: character
// classes (`[abc]`, `[!abc]`), backslash-escaped metacharacters, and `**`
// fused with other text in the same segment (`**.log`) - the same tier of
// coverage lightweight Go gitignore libraries (e.g. sabhiram/go-gitignore,
// which also translates patterns to a regexp by hand) offer, chosen over a
// heavier, fuller one (go-git's gitignore sub-package, which pulls in the
// whole go-git module tree for one small piece of it) for a plugin this
// small. If a real project's .gitignore ever needs the unsupported syntax,
// the failure mode is "the walk indexes something it shouldn't" (or the
// reverse), not a crash - and GM-280/GM-281 can revisit this once real Go
// declarations make walk correctness worth a heavier dependency.

import (
	"bufio"
	"os"
	"path"
	"path/filepath"
	"regexp"
	"strings"
)

// hardExcludedDirs mirrors plugin.toml's [plugin.workspace] exclude_dirs
// (vendor, testdata) plus .git, which every plugin excludes unconditionally
// regardless of the manifest. Kept here as a literal, hand-kept-in-sync
// copy because this binary has no access to its own parsed manifest at
// runtime (core resolves plugin.toml; this process only ever receives a
// project root on argv) - the same split ignorePolicy.ts's
// HARD_EXCLUDED_DIRS / plugin.toml's exclude_dirs keeps for the TS plugin,
// down to the comment asking the two to be kept in sync by hand.
var hardExcludedDirs = map[string]bool{
	".git":     true,
	"vendor":   true,
	"testdata": true,
}

// ignorePattern is one compiled line of a .gitignore.
type ignorePattern struct {
	negate bool
	// dirOnly patterns (a trailing "/" in the source line) only ever
	// match a directory candidate, never a file of the same name.
	dirOnly bool
	// basenameOnly is true for a pattern with no "/" anywhere but a
	// possible trailing one already stripped - matched against only the
	// last path segment of a candidate, at any depth, per git's own rule
	// that only a pattern containing an *embedded* slash is anchored to
	// the .gitignore's own directory.
	basenameOnly bool
	re           *regexp.Regexp
}

// gitignoreLayer is one directory's own .gitignore, plus the (absolute)
// directory it applies to - mirrors ignorePolicy.ts's GitignoreLayer.
type gitignoreLayer struct {
	baseDir  string
	patterns []ignorePattern
}

// loadGitignoreLayer reads dir/.gitignore, or returns nil if there is none
// (or it has no patterns worth keeping) - "no .gitignore here" is not an
// error, matching ignorePolicy.ts's loadGitignoreLayer.
func loadGitignoreLayer(dir string) *gitignoreLayer {
	f, err := os.Open(filepath.Join(dir, ".gitignore"))
	if err != nil {
		return nil
	}
	defer f.Close()

	var patterns []ignorePattern
	scanner := bufio.NewScanner(f)
	for scanner.Scan() {
		if p, ok := compilePattern(scanner.Text()); ok {
			patterns = append(patterns, p)
		}
	}
	if len(patterns) == 0 {
		return nil
	}
	return &gitignoreLayer{baseDir: dir, patterns: patterns}
}

// compilePattern parses one .gitignore line into an ignorePattern, or
// reports ok=false for a blank line, a comment (`#...`), or a line this
// hand-rolled subset cannot compile at all.
func compilePattern(raw string) (ignorePattern, bool) {
	// Leading/trailing whitespace is stripped unconditionally - real git
	// only strips it when not backslash-escaped, but this subset does not
	// support escaping at all (see this file's own doc comment), so there
	// is no escaped-space case to preserve here.
	line := strings.TrimSpace(strings.TrimRight(raw, "\r"))
	if line == "" || strings.HasPrefix(line, "#") {
		return ignorePattern{}, false
	}

	negate := false
	if strings.HasPrefix(line, "!") {
		negate = true
		line = line[1:]
	}

	dirOnly := strings.HasSuffix(line, "/")
	if dirOnly {
		line = strings.TrimSuffix(line, "/")
	}
	if line == "" {
		return ignorePattern{}, false
	}

	anchored := strings.HasPrefix(line, "/")
	body := strings.TrimPrefix(line, "/")
	if body == "" {
		return ignorePattern{}, false
	}
	// An embedded (non-trailing) slash anchors the pattern too, per git's
	// own rule - only a pattern with no slash at all (besides the
	// trailing one already stripped above) is a basename-anywhere match.
	basenameOnly := !anchored && !strings.Contains(body, "/")

	re, err := regexp.Compile("^" + translateGlob(body) + "$")
	if err != nil {
		return ignorePattern{}, false
	}
	return ignorePattern{negate: negate, dirOnly: dirOnly, basenameOnly: basenameOnly, re: re}, true
}

// translateGlob turns a single gitignore pattern body (no leading/trailing
// slash, no leading "!") into a regexp matching exactly what it would
// match as a whole string - the caller anchors it with ^...$.
func translateGlob(body string) string {
	if body == "**" {
		return ".*"
	}

	segments := strings.Split(body, "/")
	parts := make([]string, len(segments))
	for i, seg := range segments {
		if seg == "**" {
			parts[i] = "**"
		} else {
			parts[i] = translateSegment(seg)
		}
	}

	var out strings.Builder
	for i, part := range parts {
		if part == "**" {
			switch {
			case i == 0:
				out.WriteString("(?:.*/)?")
			case i == len(parts)-1:
				out.WriteString("(?:/.*)?")
			default:
				out.WriteString("(?:/.*)?/")
			}
			continue
		}
		if i > 0 && parts[i-1] != "**" {
			out.WriteString("/")
		}
		out.WriteString(part)
	}
	return out.String()
}

// translateSegment converts one non-"**" path segment (may contain "*"
// and "?" wildcards) into a regexp fragment: "*" is any run of non-"/"
// bytes, "?" is exactly one, and everything else is matched literally.
func translateSegment(segment string) string {
	var out strings.Builder
	for _, r := range segment {
		switch r {
		case '*':
			out.WriteString("[^/]*")
		case '?':
			out.WriteString("[^/]")
		default:
			out.WriteString(regexp.QuoteMeta(string(r)))
		}
	}
	return out.String()
}

// isIgnoredByLayers combines every ancestor .gitignore layer the way git
// does: each layer's patterns apply to paths relative to that layer's own
// directory, and a later (deeper, or later-declared within one file) match
// - including a negation - overrides an earlier one. Mirrors
// ignorePolicy.ts's isIgnoredByLayers, plus an explicit isDir so a
// dirOnly pattern is only ever asked about a directory candidate.
func isIgnoredByLayers(layers []*gitignoreLayer, absPath string, isDir bool) bool {
	ignored := false
	for _, layer := range layers {
		rel, err := filepath.Rel(layer.baseDir, absPath)
		if err != nil || rel == "." || strings.HasPrefix(rel, "..") {
			continue // not under this layer
		}
		relPosix := filepath.ToSlash(rel)
		for _, p := range layer.patterns {
			if p.dirOnly && !isDir {
				continue
			}
			candidate := relPosix
			if p.basenameOnly {
				candidate = path.Base(relPosix)
			}
			if !p.re.MatchString(candidate) {
				continue
			}
			ignored = !p.negate
		}
	}
	return ignored
}
