//! Port of Go's `schema.DotNotationModifier`.
//!
//! Input is a single string containing `key=value` tokens separated by
//! whitespace (shlex-split). Keys may use dot + bracket notation like
//! `stages.foo[0].name`. Tokens with no `=` get the literal value `"true"`.
//!
//! Output is YAML bytes representing the assembled document. Values are
//! always serialized as YAML strings (mirrors Go's `.%s="%s"` jq construction
//! which forces every value into a quoted string).

use serde_yaml::{Mapping, Value};

use crate::error::{Error, Result};

/// Parse a kernel-cmdline-style dot-notation byte slice into a YAML document.
pub fn dot_notation_modifier(input: &[u8]) -> Result<Vec<u8>> {
    let s = std::str::from_utf8(input)
        .map_err(|e| Error::Schema(format!("dot-notation input not utf8: {}", e)))?;

    let tokens = shlex::split(s).unwrap_or_default();

    let mut root = Value::Mapping(Mapping::new());

    for token in tokens {
        let (raw_key, raw_value) = match token.split_once('=') {
            Some((k, v)) => (k.trim_matches('"').to_string(), v.trim_matches('"').to_string()),
            None => (token.trim_matches('"').to_string(), "true".to_string()),
        };
        if raw_key.is_empty() {
            continue;
        }
        let path = parse_path(&raw_key)?;
        set_path(&mut root, &path, Value::String(raw_value));
    }

    let out = serde_yaml::to_string(&root)?;
    Ok(out.into_bytes())
}

#[derive(Debug, PartialEq, Eq)]
enum PathSegment {
    Key(String),
    Index(usize),
}

/// Parse `stages.foo[0].name` into `[Key("stages"), Key("foo"), Index(0), Key("name")]`.
fn parse_path(input: &str) -> Result<Vec<PathSegment>> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if !buf.is_empty() {
                    out.push(PathSegment::Key(std::mem::take(&mut buf)));
                }
            }
            '[' => {
                if !buf.is_empty() {
                    out.push(PathSegment::Key(std::mem::take(&mut buf)));
                }
                let mut idx = String::new();
                for ic in chars.by_ref() {
                    if ic == ']' {
                        break;
                    }
                    idx.push(ic);
                }
                let n: usize = idx
                    .parse()
                    .map_err(|e| Error::Schema(format!("bad index `{}` in `{}`: {}", idx, input, e)))?;
                out.push(PathSegment::Index(n));
            }
            _ => buf.push(c),
        }
    }
    if !buf.is_empty() {
        out.push(PathSegment::Key(buf));
    }
    Ok(out)
}

fn set_path(root: &mut Value, path: &[PathSegment], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }

    // Recursive descent — coerce intermediate nodes to the right container
    // shape as we go. Mirrors how jq creates missing nodes on assignment.
    let (head, tail) = path.split_first().unwrap();
    match head {
        PathSegment::Key(k) => {
            if !matches!(root, Value::Mapping(_)) {
                *root = Value::Mapping(Mapping::new());
            }
            let map = root.as_mapping_mut().unwrap();
            let key = Value::String(k.clone());
            if !map.contains_key(&key) {
                map.insert(key.clone(), Value::Null);
            }
            let child = map.get_mut(&key).unwrap();
            set_path(child, tail, value);
        }
        PathSegment::Index(i) => {
            if !matches!(root, Value::Sequence(_)) {
                *root = Value::Sequence(Vec::new());
            }
            let seq = root.as_sequence_mut().unwrap();
            while seq.len() <= *i {
                seq.push(Value::Null);
            }
            set_path(&mut seq[*i], tail, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml_to_value(b: &[u8]) -> Value {
        serde_yaml::from_slice(b).unwrap()
    }

    #[test]
    fn single_dot_key() {
        let out = dot_notation_modifier(b"foo.bar=baz").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["foo"]["bar"], Value::String("baz".into()));
    }

    #[test]
    fn array_index() {
        let out = dot_notation_modifier(b"stages.foo[0].name=bar").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["stages"]["foo"][0]["name"], Value::String("bar".into()));
    }

    #[test]
    fn multiple_tokens() {
        let out =
            dot_notation_modifier(b"stages.foo[0].name=bar stages.foo[0].commands[0]=baz").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["stages"]["foo"][0]["name"], Value::String("bar".into()));
        assert_eq!(
            v["stages"]["foo"][0]["commands"][0],
            Value::String("baz".into())
        );
    }

    #[test]
    fn bare_token_defaults_true() {
        // Matches Go: "boo.baz" with no `=` becomes value "true".
        let out = dot_notation_modifier(b"boo.baz").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["boo"]["baz"], Value::String("true".into()));
    }

    #[test]
    fn quoted_value_with_spaces() {
        // shlex strips outer quotes; the embedded `=` stays in the value.
        let out =
            dot_notation_modifier(br#"test="echo ping_test_host=127.0.0.1  > /tmp/jojo""#).unwrap();
        let v = yaml_to_value(&out);
        // Verbatim string after the first `=`. We re-strip surrounding quotes
        // (same as Go's `strings.Trim(parts[1], `"`)`).
        assert_eq!(
            v["test"],
            Value::String("echo ping_test_host=127.0.0.1  > /tmp/jojo".into())
        );
    }

    #[test]
    fn parse_path_basic() {
        let segs = parse_path("stages.foo[0].name").unwrap();
        assert_eq!(
            segs,
            vec![
                PathSegment::Key("stages".into()),
                PathSegment::Key("foo".into()),
                PathSegment::Index(0),
                PathSegment::Key("name".into()),
            ]
        );
    }
}
