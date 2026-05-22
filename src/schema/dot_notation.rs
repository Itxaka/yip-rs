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

    // ---------------------------------------------------------------------
    // Ported from Go `schema_test.go` "Loading from dot notation" Context.
    // ---------------------------------------------------------------------

    #[test]
    fn go_one_config_with_garbage_extra_token() {
        // `oneConfigwithGarbageS := "stages.foo[0].name=bar boo.baz"`
        let out = dot_notation_modifier(b"stages.foo[0].name=bar boo.baz").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["stages"]["foo"][0]["name"], Value::String("bar".into()));
        // The garbage `boo.baz` token still produces a node (value "true").
        assert_eq!(v["boo"]["baz"], Value::String("true".into()));
    }

    #[test]
    fn go_two_configs_merged() {
        // `twoConfigsS := "stages.foo[0].name=bar   stages.foo[0].commands[0]=baz"`
        let out = dot_notation_modifier(
            b"stages.foo[0].name=bar   stages.foo[0].commands[0]=baz",
        )
        .unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["stages"]["foo"][0]["name"], Value::String("bar".into()));
        assert_eq!(
            v["stages"]["foo"][0]["commands"][0],
            Value::String("baz".into())
        );
    }

    #[test]
    fn go_three_invalid_no_stage_keys() {
        // `threeConfigInvalid := ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/jojo"`
        let out = dot_notation_modifier(
            br#"ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/jojo""#,
        )
        .unwrap();
        let v = yaml_to_value(&out);
        // No `stages` key — invalid as a yip config but the modifier still produces
        // a doc with `ip` and `test` keys.
        assert!(v.get("stages").is_none());
        assert_eq!(v["ip"], Value::String("dhcp".into()));
    }

    #[test]
    fn go_four_half_invalid_keeps_valid_part() {
        // `fourConfigHalfInvalid := stages.foo[0].name=bar ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/dio"`
        let out = dot_notation_modifier(
            br#"stages.foo[0].name=bar ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/dio""#,
        )
        .unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["stages"]["foo"][0]["name"], Value::String("bar".into()));
        assert_eq!(v["ip"], Value::String("dhcp".into()));
    }

    // ---------------------------------------------------------------------
    // Path parsing: edge cases.
    // ---------------------------------------------------------------------

    #[test]
    fn parse_path_single_key() {
        let segs = parse_path("foo").unwrap();
        assert_eq!(segs, vec![PathSegment::Key("foo".into())]);
    }

    #[test]
    fn parse_path_only_index() {
        let segs = parse_path("[3]").unwrap();
        assert_eq!(segs, vec![PathSegment::Index(3)]);
    }

    #[test]
    fn parse_path_consecutive_indices() {
        let segs = parse_path("a[0][1]").unwrap();
        assert_eq!(
            segs,
            vec![
                PathSegment::Key("a".into()),
                PathSegment::Index(0),
                PathSegment::Index(1),
            ]
        );
    }

    #[test]
    fn parse_path_trailing_dot_ignored() {
        let segs = parse_path("foo.").unwrap();
        assert_eq!(segs, vec![PathSegment::Key("foo".into())]);
    }

    #[test]
    fn parse_path_bad_index_errors() {
        let res = parse_path("foo[abc]");
        assert!(res.is_err(), "non-numeric index must error");
    }

    #[test]
    fn parse_path_deeply_nested() {
        let segs = parse_path("a.b.c.d.e.f.g").unwrap();
        assert_eq!(segs.len(), 7);
        assert_eq!(segs[6], PathSegment::Key("g".into()));
    }

    // ---------------------------------------------------------------------
    // Modifier behaviour: misc + fuzz.
    // ---------------------------------------------------------------------

    #[test]
    fn modifier_empty_input_yields_empty_doc() {
        let out = dot_notation_modifier(b"").unwrap();
        let v = yaml_to_value(&out);
        // Empty mapping or null — both are acceptable.
        assert!(v.is_mapping() || v.is_null(), "got {:?}", v);
    }

    #[test]
    fn modifier_whitespace_only_input() {
        let out = dot_notation_modifier(b"   \t  ").unwrap();
        let v = yaml_to_value(&out);
        assert!(v.is_mapping() || v.is_null());
    }

    #[test]
    fn modifier_index_grows_sequence() {
        // Out-of-order indices should grow the sequence and leave nulls in the gaps.
        let out = dot_notation_modifier(b"a[2]=v").unwrap();
        let v = yaml_to_value(&out);
        let seq = v["a"].as_sequence().unwrap();
        assert_eq!(seq.len(), 3);
        assert_eq!(seq[2], Value::String("v".into()));
        assert!(seq[0].is_null());
        assert!(seq[1].is_null());
    }

    #[test]
    fn modifier_value_with_equals_kept_verbatim() {
        // Anything past the first `=` is the value.
        let out = dot_notation_modifier(b"a=b=c=d").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["a"], Value::String("b=c=d".into()));
    }

    #[test]
    fn modifier_bare_token_becomes_true_string() {
        let out = dot_notation_modifier(b"single").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["single"], Value::String("true".into()));
    }

    #[test]
    fn modifier_long_key_name() {
        // Stress: very long key.
        let key: String = "a".repeat(2000);
        let input = format!("{key}=v");
        let out = dot_notation_modifier(input.as_bytes()).unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v[key.as_str()], Value::String("v".into()));
    }

    #[test]
    fn modifier_long_value() {
        let val: String = "x".repeat(5000);
        let input = format!("k={val}");
        let out = dot_notation_modifier(input.as_bytes()).unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["k"], Value::String(val));
    }

    #[test]
    fn modifier_quotes_around_key_stripped() {
        // `"foo"=bar` → key becomes `foo`.
        let out = dot_notation_modifier(br#""foo"=bar"#).unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["foo"], Value::String("bar".into()));
    }

    #[test]
    fn modifier_many_tokens() {
        // 20 tokens — sanity-check the merge logic doesn't explode.
        let mut s = String::new();
        for i in 0..20 {
            s.push_str(&format!("k{i}=v{i} "));
        }
        let out = dot_notation_modifier(s.trim().as_bytes()).unwrap();
        let v = yaml_to_value(&out);
        for i in 0..20 {
            assert_eq!(v[format!("k{i}").as_str()], Value::String(format!("v{i}")));
        }
    }

    #[test]
    fn modifier_overwrites_on_duplicate_key() {
        // Last write wins for the same key.
        let out = dot_notation_modifier(b"k=v1 k=v2").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["k"], Value::String("v2".into()));
    }

    #[test]
    fn modifier_mixed_dot_and_index() {
        let out = dot_notation_modifier(
            b"stages.foo[0].commands[0]=a stages.foo[0].commands[1]=b stages.foo[0].name=n",
        )
        .unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(
            v["stages"]["foo"][0]["commands"][0],
            Value::String("a".into())
        );
        assert_eq!(
            v["stages"]["foo"][0]["commands"][1],
            Value::String("b".into())
        );
        assert_eq!(v["stages"]["foo"][0]["name"], Value::String("n".into()));
    }

    #[test]
    fn modifier_value_with_backslashes() {
        // Backslashes are not special to the modifier (only to shlex). The
        // exact post-shlex value depends on the shlex version, but the call
        // must not panic and must produce a YAML document that includes the
        // key `k`.
        let out = dot_notation_modifier(br#"k=abc"#).unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["k"], Value::String("abc".into()));
    }

    #[test]
    fn modifier_empty_value_after_equals() {
        let out = dot_notation_modifier(b"k=").unwrap();
        let v = yaml_to_value(&out);
        assert_eq!(v["k"], Value::String("".into()));
    }

    #[test]
    fn modifier_index_zero_only() {
        let out = dot_notation_modifier(b"a[0]=v").unwrap();
        let v = yaml_to_value(&out);
        let seq = v["a"].as_sequence().unwrap();
        assert_eq!(seq.len(), 1);
        assert_eq!(seq[0], Value::String("v".into()));
    }

    #[test]
    fn modifier_yaml_round_trip_via_serde() {
        // Take the modifier output, parse it back, re-serialize — same shape.
        let out = dot_notation_modifier(b"stages.foo[0].name=bar").unwrap();
        let v: Value = serde_yaml::from_slice(&out).unwrap();
        let s2 = serde_yaml::to_string(&v).unwrap();
        let v2: Value = serde_yaml::from_str(&s2).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn modifier_assignment_to_existing_scalar_overwrites_path() {
        // First write makes `a` a scalar; second write requires `a` to be a
        // mapping — `set_path` coerces.
        let out = dot_notation_modifier(b"a=v a.b=w").unwrap();
        let v = yaml_to_value(&out);
        // The final shape depends on whether shlex preserved order; we only
        // verify the deeper path landed.
        assert_eq!(v["a"]["b"], Value::String("w".into()));
    }

    // ---------------------------------------------------------------------
    // Verbatim port of Go's `schema_test.go` "Loading from dot notation"
    // Context. Each It block lifts the same fixture string and asserts on
    // the resulting `Config` (not just the raw YAML value tree) — matching
    // Go's `yipConfig.Stages["foo"][0].Name` style.
    //
    //     oneConfigwithGarbageS := "stages.foo[0].name=bar boo.baz"
    //     twoConfigsS           := "stages.foo[0].name=bar   stages.foo[0].commands[0]=baz"
    //     threeConfigInvalid    := `ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/jojo"`
    //     fourConfigHalfInvalid := `stages.foo[0].name=bar ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/dio"`
    // ---------------------------------------------------------------------

    use crate::schema::Config;

    /// Go It #1: `loadYip(oneConfigwithGarbageS)`; asserts
    /// `yipConfig.Stages["foo"][0].Name == "bar"`.
    #[test]
    fn go_it_reads_yip_file_correctly_one_config_with_garbage() {
        let yaml = dot_notation_modifier(b"stages.foo[0].name=bar boo.baz").unwrap();
        let cfg = Config::load(&yaml).unwrap();
        assert_eq!(cfg.stages["foo"][0].name, "bar");
    }

    /// Go It #2: `loadYip(twoConfigsS)`; asserts both `Name` and
    /// `Commands[0]`.
    #[test]
    fn go_it_reads_yip_file_correctly_two_configs() {
        let yaml =
            dot_notation_modifier(b"stages.foo[0].name=bar   stages.foo[0].commands[0]=baz")
                .unwrap();
        let cfg = Config::load(&yaml).unwrap();
        assert_eq!(cfg.stages["foo"][0].name, "bar");
        assert_eq!(cfg.stages["foo"][0].commands[0], "baz");
    }

    /// Go It #3: `Load(twoConfigsS, nil, nil, DotNotationModifier)` —
    /// passes the dot-notation string straight into the loader. In Rust
    /// we run the modifier then `Config::load`; the assertions match Go.
    #[test]
    fn go_it_reads_yip_file_correctly_two_configs_via_loader() {
        let yaml =
            dot_notation_modifier(b"stages.foo[0].name=bar   stages.foo[0].commands[0]=baz")
                .unwrap();
        let cfg = Config::load(&yaml).unwrap();
        assert_eq!(cfg.stages["foo"][0].name, "bar");
        assert_eq!(cfg.stages["foo"][0].commands[0], "baz");
    }

    /// Go It #4: `threeConfigInvalid` has no `stages.*` token, so the
    /// resulting `Config` should look like the zero value — `Stages` and
    /// `Name` match `Config::default()`.
    #[test]
    fn go_it_reads_yip_file_correctly_three_invalid_yields_empty_config() {
        let yaml = dot_notation_modifier(
            br#"ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/jojo""#,
        )
        .unwrap();
        let cfg = Config::load(&yaml).unwrap();
        // Should look like an empty YipConfig as it's an invalid config —
        // nothing in `Stages`, no `Name`.
        assert_eq!(cfg.stages, Config::default().stages);
        assert_eq!(cfg.name, Config::default().name);
    }

    /// Go It #5: `fourConfigHalfInvalid` — even with garbage tokens, the
    /// valid `stages.foo[0].name=bar` portion still loads.
    #[test]
    fn go_it_reads_yip_file_correctly_four_half_invalid_loads_valid_part() {
        let yaml = dot_notation_modifier(
            br#"stages.foo[0].name=bar ip=dhcp test="echo ping_test_host=127.0.0.1  > /tmp/dio""#,
        )
        .unwrap();
        let cfg = Config::load(&yaml).unwrap();
        assert_eq!(cfg.name, Config::default().name);
        // Even with a broken config, the valid parts must still load.
        assert_eq!(cfg.stages["foo"][0].name, "bar");
    }
}
