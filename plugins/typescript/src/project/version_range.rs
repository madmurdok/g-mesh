//! tsc 5.9's semver ranges (`VersionRange` in `semver.ts`), only as far as
//! testing them against tsc's own version: the range of a versioned `types`
//! condition (`"types@>=4.0"`) in a package's `exports`.

use std::cmp::Ordering;

/// The tsc version a range is tested against. Every comparison is against
/// this release, which has no prerelease tag.
const TSC_VERSION: [u64; 3] = [5, 9, 3];

/// Whether `text` parses as a tsc range that tsc 5.9.3 satisfies. A text tsc
/// cannot parse is `false`; an empty one (`""`, `"||"`) matches any version.
pub fn includes_tsc_version(text: &str) -> bool {
    parse_range(text).is_some_and(|alternatives| {
        alternatives.is_empty()
            || alternatives.iter().any(|comparators| comparators.iter().all(Comparator::holds))
    })
}

/// A comparison operand: a version, of which only whether it carries a
/// prerelease tag matters when comparing it with a release.
#[derive(Clone, Copy)]
struct Operand {
    numbers: [u64; 3],
    prerelease: bool,
}

impl Operand {
    /// The next major, minor or patch version (`field` 0, 1, 2), untagged.
    fn increment(self, field: usize) -> Self {
        let mut numbers = self.numbers;
        numbers[field] = numbers[field].saturating_add(1);
        for later in &mut numbers[field + 1..] {
            *later = 0;
        }
        Operand { numbers, prerelease: false }
    }

    /// This version tagged `-0`, the lowest prerelease of it.
    fn lowest_prerelease(self) -> Self {
        Operand { prerelease: true, ..self }
    }

    /// How [`TSC_VERSION`] compares with this operand.
    fn tsc_cmp(self) -> Ordering {
        TSC_VERSION.cmp(&self.numbers).then(if self.prerelease { Ordering::Greater } else { Ordering::Equal })
    }
}

#[derive(Clone, Copy)]
enum Op {
    Less,
    LessEq,
    Greater,
    GreaterEq,
    Eq,
}

#[derive(Clone, Copy)]
struct Comparator {
    op: Op,
    operand: Operand,
}

impl Comparator {
    fn holds(&self) -> bool {
        let cmp = self.operand.tsc_cmp();
        match self.op {
            Op::Less => cmp.is_lt(),
            Op::LessEq => cmp.is_le(),
            Op::Greater => cmp.is_gt(),
            Op::GreaterEq => cmp.is_ge(),
            Op::Eq => cmp.is_eq(),
        }
    }
}

/// A partial version (`1`, `1.x`, `1.2.3-beta`): its version with wildcards
/// as 0, and which of major, minor, patch are wildcards (missing included).
struct Partial {
    operand: Operand,
    wild: [bool; 3],
}

/// `parseRange`: `||`-separated alternatives, each a hyphen range or
/// whitespace-separated comparators. `None` when tsc rejects the text.
fn parse_range(text: &str) -> Option<Vec<Vec<Comparator>>> {
    let mut alternatives = Vec::new();
    for range in text.trim().split("||") {
        if range.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = range.split_whitespace().collect();
        let mut comparators = Vec::new();
        match tokens.as_slice() {
            [] => return None,
            [left, "-", right] if is_range_text(left) && is_range_text(right) => {
                parse_hyphen(left, right, &mut comparators)?
            }
            _ => {
                for token in tokens {
                    let (op, body) = split_operator(token)?;
                    parse_comparator(op, body, &mut comparators)?;
                }
            }
        }
        alternatives.push(comparators);
    }
    Some(alternatives)
}

/// Whether `text` is non-empty and only `[a-z0-9-+.*]` (any case), the
/// characters tsc's range grammar allows in a version.
fn is_range_text(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '+' | '.' | '*'))
}

/// A comparator token's operator (`~ ^ < > = <= >=`, or none) and version.
fn split_operator(token: &str) -> Option<(Option<&str>, &str)> {
    let two = ["<=", ">="].into_iter().find(|op| token.starts_with(op));
    let one = token.get(..1).filter(|first| "~^<>=".contains(*first));
    [two, one]
        .into_iter()
        .flatten()
        .map(|op| (Some(op), &token[op.len()..]))
        .chain(std::iter::once((None, token)))
        .find(|(_, body)| is_range_text(body))
}

fn parse_hyphen(left: &str, right: &str, comparators: &mut Vec<Comparator>) -> Option<()> {
    let left = parse_partial(left)?;
    let right = parse_partial(right)?;
    if !left.wild[0] {
        comparators.push(Comparator { op: Op::GreaterEq, operand: left.operand });
    }
    if !right.wild[0] {
        comparators.push(if right.wild[1] {
            Comparator { op: Op::Less, operand: right.operand.increment(0) }
        } else if right.wild[2] {
            Comparator { op: Op::Less, operand: right.operand.increment(1) }
        } else {
            Comparator { op: Op::LessEq, operand: right.operand }
        });
    }
    Some(())
}

fn parse_comparator(op: Option<&str>, text: &str, comparators: &mut Vec<Comparator>) -> Option<()> {
    let Partial { operand, wild } = parse_partial(text)?;
    let mut push = |op, operand| comparators.push(Comparator { op, operand });
    if wild[0] {
        if matches!(op, Some("<" | ">")) {
            push(Op::Less, Operand { numbers: [0, 0, 0], prerelease: true });
        }
        return Some(());
    }
    match op {
        Some("~") => {
            push(Op::GreaterEq, operand);
            push(Op::Less, operand.increment(if wild[1] { 0 } else { 1 }));
        }
        Some("^") => {
            push(Op::GreaterEq, operand);
            let field = if operand.numbers[0] > 0 || wild[1] {
                0
            } else if operand.numbers[1] > 0 || wild[2] {
                1
            } else {
                2
            };
            push(Op::Less, operand.increment(field));
        }
        Some(op @ ("<" | ">=")) => {
            let op = if op == "<" { Op::Less } else { Op::GreaterEq };
            push(op, if wild[1] || wild[2] { operand.lowest_prerelease() } else { operand });
        }
        Some(op @ ("<=" | ">")) => {
            let widened = if op == "<=" { Op::Less } else { Op::GreaterEq };
            if wild[1] {
                push(widened, operand.increment(0).lowest_prerelease());
            } else if wild[2] {
                push(widened, operand.increment(1).lowest_prerelease());
            } else {
                push(if op == "<=" { Op::LessEq } else { Op::Greater }, operand);
            }
        }
        _ => {
            if wild[1] || wild[2] {
                push(Op::GreaterEq, operand.lowest_prerelease());
                push(Op::Less, operand.increment(if wild[1] { 0 } else { 1 }).lowest_prerelease());
            } else {
                push(Op::Eq, operand);
            }
        }
    }
    Some(())
}

/// `parsePartial`: `major[.minor[.patch[-prerelease][+build]]]`, each number
/// `0`, a number without a leading zero, or a wildcard `x`/`X`/`*`. A
/// prerelease or build part tsc's `Version` would reject (an empty part, a
/// numeric part with a leading zero) is `None` here; tsc throws on it.
fn parse_partial(text: &str) -> Option<Partial> {
    let (before_build, build) = match text.split_once('+') {
        Some((before, build)) => (before, Some(build)),
        None => (text, None),
    };
    let (core, prerelease) = match before_build.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (before_build, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() > 3 || (parts.len() < 3 && (prerelease.is_some() || build.is_some())) {
        return None;
    }
    if !prerelease.is_none_or(|text| dotted_parts_ok(text, is_prerelease_part))
        || !build.is_none_or(|text| dotted_parts_ok(text, is_build_part))
    {
        return None;
    }
    let mut numbers = [0; 3];
    let mut wild = [true; 3];
    for (index, part) in parts.iter().enumerate() {
        match *part {
            "x" | "X" | "*" => {}
            _ => {
                numbers[index] = parse_number(part)?;
                wild[index] = false;
            }
        }
    }
    for index in 1..3 {
        wild[index] |= wild[index - 1];
        if wild[index] {
            numbers[index] = 0;
        }
    }
    if wild[0] {
        numbers[0] = 0;
    }
    Some(Partial { operand: Operand { numbers, prerelease: prerelease.is_some() }, wild })
}

/// `0` or a decimal without a leading zero; saturates past `u64::MAX`.
fn parse_number(part: &str) -> Option<u64> {
    let digits = !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
    (digits && (part == "0" || !part.starts_with('0'))).then(|| part.parse().unwrap_or(u64::MAX))
}

/// Whether `text` is `.`-separated parts that all pass `part_ok`.
fn dotted_parts_ok(text: &str, part_ok: fn(&str) -> bool) -> bool {
    text.split('.').all(part_ok)
}

/// A prerelease identifier: `0`, a number without a leading zero, or a
/// letter or `-` followed by letters, digits and `-`.
fn is_prerelease_part(part: &str) -> bool {
    let mut chars = part.chars();
    match chars.next() {
        Some(first) if first.is_ascii_digit() => parse_number(part).is_some(),
        Some(first) if first.is_ascii_alphabetic() || first == '-' => {
            chars.all(|c| c.is_ascii_alphanumeric() || c == '-')
        }
        _ => false,
    }
}

/// A build identifier: letters, digits and `-`, at least one.
fn is_build_part(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}
