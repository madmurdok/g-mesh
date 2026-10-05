//! JSON as the project model reads it: objects keep their key order, and
//! tsconfig files may carry comments and trailing commas (JSONC).
//!
//! Key order is part of the resolution rules: `exports`, `imports` and
//! `paths` maps are walked in declaration order. `serde_json::Value` sorts
//! keys, and its `preserve_order` feature must stay off (it would reach every
//! crate linked with this one, see `plugins/rust/Cargo.toml`), so this module
//! has its own value type.

use std::fmt;

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};

/// A parsed JSON value. Object keys are kept in source order; a repeated key
/// keeps its first position and its last value, as `JSON.parse` does.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// Any number. Nothing here reads a number's value.
    Number,
    /// A string.
    String(String),
    /// An array.
    Array(Vec<Json>),
    /// An object, in source key order.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// The value under `key`, when `self` is an object holding it.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(entries) => entries.iter().find(|(name, _)| name == key).map(|(_, value)| value),
            _ => None,
        }
    }

    /// The string, when `self` is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    /// The entries, when `self` is an object.
    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(entries) => Some(entries),
            _ => None,
        }
    }

    /// The items, when `self` is an array.
    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any JSON value")
    }

    fn visit_unit<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_none<E>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Json, E> {
        Ok(Json::Bool(value))
    }

    fn visit_i64<E>(self, _: i64) -> Result<Json, E> {
        Ok(Json::Number)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Json, E> {
        Ok(Json::Number)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Json, E> {
        Ok(Json::Number)
    }

    fn visit_str<E>(self, value: &str) -> Result<Json, E> {
        Ok(Json::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Json, E> {
        Ok(Json::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut entries: Vec<(String, Json)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            match entries.iter_mut().find(|(name, _)| *name == key) {
                Some(slot) => slot.1 = value,
                None => entries.push((key, value)),
            }
        }
        Ok(Json::Object(entries))
    }
}

/// Strict JSON (`package.json`). `None` when it does not parse.
pub fn parse_json(text: &str) -> Option<Json> {
    serde_json::from_str(text).ok()
}

/// JSON with comments and trailing commas (tsconfig/jsconfig). `None` when
/// it does not parse even after those are removed.
pub fn parse_jsonc(text: &str) -> Option<Json> {
    parse_json(&strip_jsonc(text))
}

/// `text` with comments and trailing commas replaced by whitespace. String
/// literals are left alone, so `"http://x"` and `"/* x */"` survive; a
/// comment becomes whitespace rather than nothing, so two tokens are never
/// glued together.
pub fn strip_jsonc(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut commas: Vec<usize> = Vec::new();
    let mut in_string = false;
    let mut index = 0;

    while index < chars.len() {
        let char = chars[index];

        if in_string {
            out.push(char);
            if char == '\\' {
                if let Some(&escaped) = chars.get(index + 1) {
                    out.push(escaped);
                    index += 1;
                }
            } else if char == '"' {
                in_string = false;
            }
            index += 1;
            continue;
        }

        let next = chars.get(index + 1).copied();
        if char == '"' {
            in_string = true;
            out.push(char);
        } else if char == '/' && next == Some('/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            out.push('\n');
        } else if char == '/' && next == Some('*') {
            index += 2;
            while index < chars.len() && !(chars[index] == '*' && chars.get(index + 1) == Some(&'/')) {
                index += 1;
            }
            // Past the `*`; the step below moves past the closing `/`.
            index += 1;
            out.push(' ');
        } else {
            if char == ',' {
                commas.push(out.len());
            }
            out.push(char);
        }
        index += 1;
    }

    // With comments gone, "what follows this comma" is a whitespace skip.
    for comma in commas {
        let mut next = comma + 1;
        while next < out.len() && out[next].is_whitespace() {
            next += 1;
        }
        if next < out.len() && (out[next] == '}' || out[next] == ']') {
            out[comma] = ' ';
        }
    }
    out.into_iter().collect()
}
