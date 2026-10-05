//! Project-relative POSIX path arithmetic, the spelling every lookup in the
//! project model uses. The root directory is `""`.

/// `path` with `.` segments, empty segments and trailing slashes removed and
/// each `..` folded into its parent. A path that climbs above its start keeps
/// its leading `..` segments; an empty result is `"."`.
pub fn normalize(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if segments.last().is_some_and(|last| *last != "..") {
                    segments.pop();
                } else {
                    segments.push("..");
                }
            }
            _ => segments.push(segment),
        }
    }
    if segments.is_empty() {
        ".".to_string()
    } else {
        segments.join("/")
    }
}

/// `dir` and `name` joined, without normalizing.
pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The directory holding `path` (`""` for a file at the root).
pub fn dirname(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The last segment of `path`.
pub fn basename(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}

/// Whether a normalized path names nothing inside the project: the root's
/// own `.`, or anything climbing out of it.
pub fn escapes(normalized: &str) -> bool {
    normalized.is_empty() || normalized == "." || normalized == ".." || normalized.starts_with("../")
}

/// `target` resolved against the project-relative directory `dir`, or `None`
/// when that leaves the project, names the directory's root itself, or
/// `target` is absolute (a POSIX root or a drive letter). Used for every
/// target a manifest or config declares: a package's entries, an `imports`
/// key's targets, a tsconfig `paths` target. As in tsc, `\\` in `target`
/// is a separator on every host.
pub fn inside(dir: &str, target: &str) -> Option<String> {
    let target = with_forward_slashes(target);
    if target.is_empty() || is_rooted(&target) {
        return None;
    }
    let joined = normalize(&join(dir, &target));
    (!escapes(&joined)).then_some(joined)
}

/// `dir` and every ancestor of it, nearest first, ending with the root `""`.
pub fn ancestors(dir: &str) -> impl Iterator<Item = &str> {
    let mut next = Some(dir);
    std::iter::from_fn(move || {
        let current = next?;
        next = if current.is_empty() { None } else { Some(dirname(current)) };
        Some(current)
    })
}

/// `path` with every `\\` turned into `/`: tsc's `normalizeSlashes`, which
/// it applies to every path a config declares, on every host.
pub fn with_forward_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

/// Whether `target` is absolute on some host: a POSIX root or a drive
/// letter. Decided by spelling, not by the host's `Path`, so a config reads
/// the same on every OS.
pub fn is_rooted(target: &str) -> bool {
    target.starts_with('/') || has_drive_letter(target)
}

/// A Windows absolute path's `C:` prefix.
fn has_drive_letter(target: &str) -> bool {
    let bytes = target.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}
