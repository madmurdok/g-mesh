//! Hover text, normalised and compared.

use serde_json::Value;

/// The signature a `hover` result shows, normalised for comparison; `None`
/// when it shows nothing.
///
/// `contents` may be a string, a `MarkupContent`, a `MarkedString` object, or
/// an array of either. The signature is the first fenced code block when
/// there is one, and the first paragraph otherwise: what follows is
/// documentation, which a call and a declaration need not share.
pub(super) fn hover_text(result: &Value) -> Option<String> {
    fn collect(value: &Value, into: &mut Vec<String>) {
        match value {
            Value::String(text) => into.push(text.clone()),
            Value::Array(values) => values.iter().for_each(|value| collect(value, into)),
            Value::Object(object) => {
                if let Some(text) = object.get("value").and_then(Value::as_str) {
                    into.push(text.to_string());
                }
            }
            _ => {}
        }
    }
    let mut parts = Vec::new();
    collect(result.get("contents")?, &mut parts);
    let text = parts.join("\n");
    let signature = match text.find("```") {
        Some(fence) => {
            let body = &text[fence + 3..];
            let body = body.find('\n').map_or("", |newline| &body[newline + 1..]);
            body.find("```").map_or(body, |end| &body[..end])
        }
        None => text.trim().split("\n\n").next().unwrap_or_default(),
    };
    let normalised = normalise_hover(signature);
    (!normalised.is_empty()).then_some(normalised)
}

/// Whitespace collapsed to single spaces, and none just inside brackets or
/// before a comma - so a signature a printer wraps over several lines equals
/// the same signature on one.
pub(super) fn normalise_hover(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::with_capacity(collapsed.len());
    let chars: Vec<char> = collapsed.chars().collect();
    for (at, &c) in chars.iter().enumerate() {
        if c == ' ' {
            let before = out.chars().last();
            let after = chars.get(at + 1).copied();
            if matches!(before, Some('(') | Some('[')) || matches!(after, Some(')') | Some(']') | Some(',')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Step 3's match: a candidate declaration's hover equals the call's, or does
/// once its first parameter is dropped (a method called through a bound
/// receiver). Two empty hovers never match.
pub(super) fn hover_matches(declared: &str, call: &str) -> bool {
    if declared.is_empty() || call.is_empty() {
        return false;
    }
    declared == call || without_first_parameter(declared).is_some_and(|dropped| dropped == call)
}

/// `text` with the first parameter of its first parameter list removed - the
/// first `(` that directly follows an identifier, so a `(method)` prefix is
/// not taken for one. `None` when there is no list or it is empty.
pub(super) fn without_first_parameter(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let open = (1..chars.len())
        .find(|&at| chars[at] == '(' && (chars[at - 1].is_alphanumeric() || chars[at - 1] == '_'))?;
    let mut depth = 0usize;
    let mut comma = None;
    let mut close = None;
    for (at, &c) in chars.iter().enumerate().skip(open + 1) {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => depth -= 1,
            ')' => {
                close = Some(at);
                break;
            }
            ',' if depth == 0 && comma.is_none() => comma = Some(at),
            _ => {}
        }
    }
    let close = close?;
    if close == open + 1 {
        return None;
    }
    let rest_from = comma.map_or(close, |comma| comma + 1);
    let head: String = chars[..=open].iter().collect();
    let rest: String = chars[rest_from..].iter().collect();
    Some(format!("{head}{}", rest.trim_start()))
}
