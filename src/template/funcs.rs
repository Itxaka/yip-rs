//! Sprig-subset function/filter implementations for the tera engine.
//!
//! Each function is exposed as either a tera filter (`{{ x | foo }}`) or
//! a tera function (`{{ foo(arg=...) }}`). Most sprig funcs are filters
//! because that is how yip configs invoke them.
//!
//! Mapping notes:
//!
//! - tera already ships several builtins under the `builtins` feature
//!   flag, but yip-rs depends on tera with `default-features = false`,
//!   so we register our own versions of even the "obvious" ones to
//!   ensure they exist regardless of feature flags.
//! - Where sprig and tera disagree on filter name (`uniq` vs `unique`,
//!   `len` vs `length`, `hasPrefix` vs `starting_with`), we register
//!   *both* the sprig name and the tera name to maximise compatibility.

use std::collections::HashMap;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::{Duration, Utc};
use md5::Md5;
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde_json::{json, Map, Value};
// `sha2::Digest` is the same `digest::Digest` trait md-5 re-exports; importing
// it once here makes `.update()` / `.finalize()` resolve for all three hashers.
use sha2::{Digest as _, Sha256, Sha512};
use tera::{Error as TeraError, Result as TeraResult, Tera};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Argument helpers

fn arg_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn arg_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        Value::Bool(b) => Some(if *b { 1 } else { 0 }),
        _ => None,
    }
}

fn arg_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

fn err<S: Into<String>>(msg: S) -> TeraError {
    TeraError::msg(msg.into())
}

fn first_positional(args: &HashMap<String, Value>, names: &[&str]) -> Option<Value> {
    for n in names {
        if let Some(v) = args.get(*n) {
            return Some(v.clone());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// String filters

fn f_lower(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(arg_string(value).to_lowercase()))
}

fn f_upper(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(arg_string(value).to_uppercase()))
}

fn f_title(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    // Sprig's `title` matches Go's `strings.Title` — title-case each word.
    let s = arg_string(value);
    let mut out = String::with_capacity(s.len());
    let mut prev_is_boundary = true;
    for ch in s.chars() {
        if prev_is_boundary && ch.is_alphabetic() {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
        prev_is_boundary = ch.is_whitespace();
    }
    Ok(Value::String(out))
}

fn f_trim(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(arg_string(value).trim().to_string()))
}

fn f_trim_all(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // Sprig signature: `trimAll "$" "$var$"` (cutset, input). When used as
    // a filter the input is the piped value; the cutset arrives as a named
    // arg or positional.
    let cutset = first_positional(args, &["cutset", "chars", "0"])
        .map(|v| arg_string(&v))
        .unwrap_or_default();
    let s = arg_string(value);
    Ok(Value::String(s.trim_matches(|c: char| cutset.contains(c)).to_string()))
}

fn f_trim_prefix(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let prefix = first_positional(args, &["prefix", "0"])
        .map(|v| arg_string(&v))
        .unwrap_or_default();
    let s = arg_string(value);
    Ok(Value::String(s.strip_prefix(&prefix).map(|r| r.to_string()).unwrap_or(s)))
}

fn f_trim_suffix(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let suffix = first_positional(args, &["suffix", "0"])
        .map(|v| arg_string(&v))
        .unwrap_or_default();
    let s = arg_string(value);
    Ok(Value::String(s.strip_suffix(&suffix).map(|r| r.to_string()).unwrap_or(s)))
}

fn f_replace(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // Sprig: `replace OLD NEW STRING`. As a filter: `STRING | replace OLD NEW`.
    let from = first_positional(args, &["from", "old", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("replace: missing `from`/`old`"))?;
    let to = first_positional(args, &["to", "new", "1"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("replace: missing `to`/`new`"))?;
    Ok(Value::String(arg_string(value).replace(&from, &to)))
}

fn f_repeat(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let n = first_positional(args, &["count", "n", "0"])
        .and_then(|v| arg_i64(&v))
        .ok_or_else(|| err("repeat: missing count"))?;
    if n < 0 {
        return Err(err("repeat: negative count"));
    }
    Ok(Value::String(arg_string(value).repeat(n as usize)))
}

fn f_contains(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let needle = first_positional(args, &["substr", "needle", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("contains: missing substring"))?;
    Ok(Value::Bool(arg_string(value).contains(&needle)))
}

fn f_has_prefix(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let p = first_positional(args, &["prefix", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("hasPrefix: missing prefix"))?;
    Ok(Value::Bool(arg_string(value).starts_with(&p)))
}

fn f_has_suffix(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let p = first_positional(args, &["suffix", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("hasSuffix: missing suffix"))?;
    Ok(Value::Bool(arg_string(value).ends_with(&p)))
}

fn f_split(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // sprig `split SEP STR` returns a map `{_0: "a", _1: "b"}`; we follow
    // tera convention and return a JSON array — that matches what `range`
    // expects, which is the common consumption pattern in yip configs.
    let sep = first_positional(args, &["sep", "pat", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("split: missing separator"))?;
    let s = arg_string(value);
    let parts: Vec<Value> = s
        .split(&sep)
        .map(|p| Value::String(p.to_string()))
        .collect();
    Ok(Value::Array(parts))
}

fn f_join(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let sep = first_positional(args, &["sep", "0"])
        .map(|v| arg_string(&v))
        .unwrap_or_default();
    let arr = value
        .as_array()
        .ok_or_else(|| err("join: input is not an array"))?;
    let s = arr
        .iter()
        .map(arg_string)
        .collect::<Vec<_>>()
        .join(&sep);
    Ok(Value::String(s))
}

fn f_quote(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(format!("\"{}\"", arg_string(value))))
}

fn f_squote(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(format!("'{}'", arg_string(value))))
}

fn f_cat(_: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // sprig cat takes multiple positionals, joined with spaces. When used
    // as a filter the piped value goes first.
    let mut parts: Vec<String> = Vec::new();
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    for k in keys {
        parts.push(arg_string(&args[k]));
    }
    Ok(Value::String(parts.join(" ")))
}

fn f_indent(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let n = first_positional(args, &["count", "width", "n", "0"])
        .and_then(|v| arg_i64(&v))
        .ok_or_else(|| err("indent: missing width"))?;
    if n < 0 {
        return Err(err("indent: negative width"));
    }
    let pad: String = std::iter::repeat(' ').take(n as usize).collect();
    let s = arg_string(value);
    let mut out = String::with_capacity(s.len() + (n as usize));
    out.push_str(&pad);
    for ch in s.chars() {
        out.push(ch);
        if ch == '\n' {
            out.push_str(&pad);
        }
    }
    Ok(Value::String(out))
}

fn f_nindent(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let indented = f_indent(value, args)?;
    Ok(Value::String(format!("\n{}", arg_string(&indented))))
}

// ---------------------------------------------------------------------------
// Default / null

fn f_default(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // Sprig `default DEFAULT VALUE`; as a filter: `VALUE | default DEFAULT`.
    // Falls back to default if value is empty/null/false/0.
    let def = first_positional(args, &["value", "default", "0"])
        .ok_or_else(|| err("default: missing default value"))?;
    if is_empty(value) {
        Ok(def)
    } else {
        Ok(value.clone())
    }
}

fn is_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !*b,
        Value::Number(n) => n.as_f64().map(|f| f == 0.0).unwrap_or(false),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

fn f_empty(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::Bool(is_empty(value)))
}

fn fn_coalesce(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    for k in keys {
        let v = &args[k];
        if !is_empty(v) {
            return Ok(v.clone());
        }
    }
    Ok(Value::Null)
}

// ---------------------------------------------------------------------------
// Lists

fn f_first(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(value.as_array().and_then(|a| a.first().cloned()).unwrap_or(Value::Null))
}

fn f_last(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(value.as_array().and_then(|a| a.last().cloned()).unwrap_or(Value::Null))
}

fn f_len(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let n = match value {
        Value::String(s) => s.chars().count(),
        Value::Array(a) => a.len(),
        Value::Object(o) => o.len(),
        Value::Null => 0,
        _ => 1,
    };
    Ok(Value::Number((n as i64).into()))
}

fn f_append(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut arr = value
        .as_array()
        .cloned()
        .ok_or_else(|| err("append: input is not an array"))?;
    let item = first_positional(args, &["item", "0"])
        .ok_or_else(|| err("append: missing item"))?;
    arr.push(item);
    Ok(Value::Array(arr))
}

fn f_prepend(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut arr = value
        .as_array()
        .cloned()
        .ok_or_else(|| err("prepend: input is not an array"))?;
    let item = first_positional(args, &["item", "0"])
        .ok_or_else(|| err("prepend: missing item"))?;
    arr.insert(0, item);
    Ok(Value::Array(arr))
}

fn fn_concat(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    let mut out = Vec::new();
    for k in keys {
        if let Some(arr) = args[k].as_array() {
            out.extend(arr.iter().cloned());
        } else {
            out.push(args[k].clone());
        }
    }
    Ok(Value::Array(out))
}

fn f_uniq(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let arr = value
        .as_array()
        .ok_or_else(|| err("uniq: input is not an array"))?;
    let mut seen: Vec<&Value> = Vec::new();
    for v in arr {
        if !seen.iter().any(|s| *s == v) {
            seen.push(v);
        }
    }
    Ok(Value::Array(seen.into_iter().cloned().collect()))
}

// ---------------------------------------------------------------------------
// Maps

fn f_keys(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let m = value
        .as_object()
        .ok_or_else(|| err("keys: input is not an object"))?;
    Ok(Value::Array(m.keys().map(|k| Value::String(k.clone())).collect()))
}

fn f_values(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let m = value
        .as_object()
        .ok_or_else(|| err("values: input is not an object"))?;
    Ok(Value::Array(m.values().cloned().collect()))
}

fn f_has_key(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let key = first_positional(args, &["key", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("hasKey: missing key"))?;
    let m = value
        .as_object()
        .ok_or_else(|| err("hasKey: input is not an object"))?;
    Ok(Value::Bool(m.contains_key(&key)))
}

fn f_pluck(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // sprig `pluck KEY MAP1 MAP2 …` — as filter `[MAP1, MAP2] | pluck "KEY"`.
    let key = first_positional(args, &["key", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("pluck: missing key"))?;
    let arr = value
        .as_array()
        .ok_or_else(|| err("pluck: input is not an array of maps"))?;
    let mut out = Vec::new();
    for m in arr {
        if let Some(obj) = m.as_object() {
            if let Some(v) = obj.get(&key) {
                out.push(v.clone());
            }
        }
    }
    Ok(Value::Array(out))
}

fn f_pick(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let m = value
        .as_object()
        .ok_or_else(|| err("pick: input is not an object"))?;
    let mut out = Map::new();
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    for k in keys {
        let name = arg_string(&args[k]);
        if let Some(v) = m.get(&name) {
            out.insert(name, v.clone());
        }
    }
    Ok(Value::Object(out))
}

fn f_omit(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let m = value
        .as_object()
        .ok_or_else(|| err("omit: input is not an object"))?;
    let drop: Vec<String> = args.values().map(arg_string).collect();
    let mut out = Map::new();
    for (k, v) in m {
        if !drop.contains(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

fn fn_dict(args: &HashMap<String, Value>) -> TeraResult<Value> {
    // tera invokes functions with named args, so `dict` already maps onto
    // a kv-pair sequence cleanly: every named arg becomes a key.
    let mut m = Map::new();
    for (k, v) in args {
        m.insert(k.clone(), v.clone());
    }
    Ok(Value::Object(m))
}

fn f_set(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut m = value
        .as_object()
        .cloned()
        .ok_or_else(|| err("set: input is not an object"))?;
    let key = first_positional(args, &["key", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("set: missing key"))?;
    let val = first_positional(args, &["value", "1"])
        .ok_or_else(|| err("set: missing value"))?;
    m.insert(key, val);
    Ok(Value::Object(m))
}

fn f_unset(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut m = value
        .as_object()
        .cloned()
        .ok_or_else(|| err("unset: input is not an object"))?;
    let key = first_positional(args, &["key", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("unset: missing key"))?;
    m.remove(&key);
    Ok(Value::Object(m))
}

// ---------------------------------------------------------------------------
// Math (functions, because sprig uses prefix form `add 1 2 3`)

fn fn_add(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let (i, f, any_float) = sum_numbers(args)?;
    if any_float {
        Ok(json!(f))
    } else {
        Ok(json!(i))
    }
}

fn fn_sub(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let (a, b) = pair(args, "sub")?;
    if let (Some(ai), Some(bi)) = (arg_i64(&a), arg_i64(&b)) {
        Ok(json!(ai - bi))
    } else {
        let af = arg_f64(&a).ok_or_else(|| err("sub: non-numeric a"))?;
        let bf = arg_f64(&b).ok_or_else(|| err("sub: non-numeric b"))?;
        Ok(json!(af - bf))
    }
}

fn fn_mul(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let (mut i, mut f, mut any_float) = (1_i64, 1.0_f64, false);
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    for k in keys {
        let v = &args[k];
        if let Some(n) = arg_i64(v) {
            i = i.saturating_mul(n);
            f *= n as f64;
        } else if let Some(g) = arg_f64(v) {
            f *= g;
            any_float = true;
        } else {
            return Err(err("mul: non-numeric arg"));
        }
    }
    if any_float {
        Ok(json!(f))
    } else {
        Ok(json!(i))
    }
}

fn fn_div(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let (a, b) = pair(args, "div")?;
    if let (Some(ai), Some(bi)) = (arg_i64(&a), arg_i64(&b)) {
        if bi == 0 {
            return Err(err("div: divide by zero"));
        }
        Ok(json!(ai / bi))
    } else {
        let af = arg_f64(&a).ok_or_else(|| err("div: non-numeric a"))?;
        let bf = arg_f64(&b).ok_or_else(|| err("div: non-numeric b"))?;
        Ok(json!(af / bf))
    }
}

fn fn_mod(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let (a, b) = pair(args, "mod")?;
    let ai = arg_i64(&a).ok_or_else(|| err("mod: non-integer a"))?;
    let bi = arg_i64(&b).ok_or_else(|| err("mod: non-integer b"))?;
    if bi == 0 {
        return Err(err("mod: divide by zero"));
    }
    Ok(json!(ai % bi))
}

fn fn_min(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    let mut acc: Option<f64> = None;
    let mut all_int = true;
    let mut acc_i: Option<i64> = None;
    for k in keys {
        let v = &args[k];
        let f = arg_f64(v).ok_or_else(|| err("min: non-numeric arg"))?;
        if let Some(i) = arg_i64(v) {
            acc_i = Some(acc_i.map_or(i, |a| a.min(i)));
        } else {
            all_int = false;
        }
        acc = Some(acc.map_or(f, |a| a.min(f)));
    }
    let acc = acc.ok_or_else(|| err("min: no args"))?;
    if all_int {
        Ok(json!(acc_i.unwrap_or(acc as i64)))
    } else {
        Ok(json!(acc))
    }
}

fn fn_max(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    let mut acc: Option<f64> = None;
    let mut all_int = true;
    let mut acc_i: Option<i64> = None;
    for k in keys {
        let v = &args[k];
        let f = arg_f64(v).ok_or_else(|| err("max: non-numeric arg"))?;
        if let Some(i) = arg_i64(v) {
            acc_i = Some(acc_i.map_or(i, |a| a.max(i)));
        } else {
            all_int = false;
        }
        acc = Some(acc.map_or(f, |a| a.max(f)));
    }
    let acc = acc.ok_or_else(|| err("max: no args"))?;
    if all_int {
        Ok(json!(acc_i.unwrap_or(acc as i64)))
    } else {
        Ok(json!(acc))
    }
}

fn sum_numbers(args: &HashMap<String, Value>) -> TeraResult<(i64, f64, bool)> {
    let mut i: i64 = 0;
    let mut f: f64 = 0.0;
    let mut any_float = false;
    let mut keys: Vec<&String> = args.keys().collect();
    keys.sort();
    for k in keys {
        let v = &args[k];
        if let Some(n) = arg_i64(v) {
            i = i.saturating_add(n);
            f += n as f64;
        } else if let Some(g) = arg_f64(v) {
            f += g;
            any_float = true;
        } else {
            return Err(err("non-numeric arg"));
        }
    }
    Ok((i, f, any_float))
}

fn pair(args: &HashMap<String, Value>, name: &str) -> TeraResult<(Value, Value)> {
    let a = first_positional(args, &["a", "0"])
        .ok_or_else(|| err(format!("{name}: missing first arg")))?;
    let b = first_positional(args, &["b", "1"])
        .ok_or_else(|| err(format!("{name}: missing second arg")))?;
    Ok((a, b))
}

// ---------------------------------------------------------------------------
// Conversion

fn f_int(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let n = arg_i64(value).or_else(|| arg_f64(value).map(|f| f as i64))
        .ok_or_else(|| err("int: not convertible"))?;
    Ok(json!(n))
}

fn f_int64(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    f_int(value, args)
}

fn f_float(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let n = arg_f64(value).ok_or_else(|| err("float: not convertible"))?;
    Ok(json!(n))
}

fn f_to_string(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(arg_string(value)))
}

fn f_to_bool(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let b = match value {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => matches!(s.to_lowercase().as_str(), "true" | "1" | "yes" | "y" | "t" | "on"),
        Value::Null => false,
        _ => true,
    };
    Ok(Value::Bool(b))
}

fn f_to_json(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(serde_json::to_string(value).map_err(|e| err(e.to_string()))?))
}

fn f_to_yaml(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(
        serde_yaml::to_string(value).map_err(|e| err(e.to_string()))?,
    ))
}

fn f_from_json(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let s = arg_string(value);
    serde_json::from_str::<Value>(&s).map_err(|e| err(format!("fromJson: {e}")))
}

fn f_from_yaml(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let s = arg_string(value);
    serde_yaml::from_str::<Value>(&s).map_err(|e| err(format!("fromYaml: {e}")))
}

// ---------------------------------------------------------------------------
// Encoding / hashing

fn f_b64enc(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(B64.encode(arg_string(value).as_bytes())))
}

fn f_b64dec(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let raw = B64
        .decode(arg_string(value).as_bytes())
        .map_err(|e| err(format!("b64dec: {e}")))?;
    Ok(Value::String(
        String::from_utf8(raw).map_err(|e| err(format!("b64dec: {e}")))?,
    ))
}

fn f_sha256(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut h = Sha256::new();
    h.update(arg_string(value).as_bytes());
    Ok(Value::String(hex::encode(h.finalize())))
}

fn f_sha512(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut h = Sha512::new();
    h.update(arg_string(value).as_bytes());
    Ok(Value::String(hex::encode(h.finalize())))
}

fn f_md5(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let mut h = Md5::new();
    h.update(arg_string(value).as_bytes());
    Ok(Value::String(hex::encode(h.finalize())))
}

fn fn_rand_alpha_num(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let n = first_positional(args, &["count", "n", "0"])
        .and_then(|v| arg_i64(&v))
        .ok_or_else(|| err("randAlphaNum: missing count"))? as usize;
    let s: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .map(char::from)
        .collect();
    Ok(Value::String(s))
}

fn fn_rand_alpha(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let n = first_positional(args, &["count", "n", "0"])
        .and_then(|v| arg_i64(&v))
        .ok_or_else(|| err("randAlpha: missing count"))? as usize;
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::thread_rng();
    let s: String = (0..n)
        .map(|_| ALPHA[rng.gen_range(0..ALPHA.len())] as char)
        .collect();
    Ok(Value::String(s))
}

fn fn_rand_numeric(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let n = first_positional(args, &["count", "n", "0"])
        .and_then(|v| arg_i64(&v))
        .ok_or_else(|| err("randNumeric: missing count"))? as usize;
    let mut rng = rand::thread_rng();
    let s: String = (0..n).map(|_| char::from(b'0' + rng.gen_range(0..10))).collect();
    Ok(Value::String(s))
}

// ---------------------------------------------------------------------------
// Date

fn fn_now(_: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(Utc::now().to_rfc3339()))
}

fn f_date(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // sprig `date "2006-01-02" .Time`. As filter: `.Time | date "2006-01-02"`.
    // We accept the Go time format string and translate the most common
    // tokens to chrono's strftime syntax. Less-common Go tokens fall through
    // to chrono unchanged; configs that need exotic formats can use the
    // chrono `%` syntax directly.
    let fmt = first_positional(args, &["format", "fmt", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("date: missing format"))?;
    let chrono_fmt = go_time_layout_to_chrono(&fmt);
    let when = match value {
        Value::Null => Utc::now(),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .map_err(|e| err(format!("date: bad input time: {e}")))?,
        _ => Utc::now(),
    };
    Ok(Value::String(when.format(&chrono_fmt).to_string()))
}

fn f_date_in_zone(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // TODO(sprig): proper `dateInZone` needs chrono-tz to look up arbitrary
    // zone names. yip-rs pulls chrono with `default-features = false` and
    // no chrono-tz, so we approximate by ignoring the zone and rendering UTC.
    f_date(value, args)
}

fn go_time_layout_to_chrono(layout: &str) -> String {
    // Translate the most common Go reference-time tokens. The Go reference
    // time is "Mon Jan 2 15:04:05 MST 2006" — each component is a literal
    // token. This is not a full implementation but covers the typical
    // configs we see in kairos.
    layout
        .replace("2006", "%Y")
        .replace("01", "%m")
        .replace("02", "%d")
        .replace("15", "%H")
        .replace("04", "%M")
        .replace("05", "%S")
        .replace("Jan", "%b")
        .replace("Mon", "%a")
        .replace("MST", "%Z")
}

// ---------------------------------------------------------------------------
// OS / env

fn fn_env(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let name = first_positional(args, &["name", "key", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("env: missing name"))?;
    Ok(Value::String(std::env::var(name).unwrap_or_default()))
}

fn fn_expandenv(args: &HashMap<String, Value>) -> TeraResult<Value> {
    // `expandenv "Hello $USER"` -> substitute $VAR / ${VAR}.
    let s = first_positional(args, &["str", "s", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("expandenv: missing string"))?;
    Ok(Value::String(expand_env_vars(&s)))
}

fn f_expandenv_filter(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(expand_env_vars(&arg_string(value))))
}

fn expand_env_vars(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            // ${VAR}
            if bytes[i + 1] == b'{' {
                if let Some(end) = bytes[i + 2..].iter().position(|&b| b == b'}') {
                    let name = &input[i + 2..i + 2 + end];
                    out.push_str(&std::env::var(name).unwrap_or_default());
                    i = i + 2 + end + 1;
                    continue;
                }
            }
            // $VAR
            let mut j = i + 1;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j > i + 1 {
                let name = &input[i + 1..j];
                out.push_str(&std::env::var(name).unwrap_or_default());
                i = j;
                continue;
            }
        }
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn fn_temp_dir(_: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(
        std::env::temp_dir().to_string_lossy().into_owned(),
    ))
}

// ---------------------------------------------------------------------------
// Deep map access (sprig: dig, get, mustGet)

/// Collect positional args (`"0"`, `"1"`, …) in numeric order, falling back to
/// any extra named args appended after them in sorted order. Used by the
/// variadic sprig funcs (`dig`, `mergeOverwrite`, …) that take an arbitrary
/// number of keys / maps as input.
fn ordered_positionals(args: &HashMap<String, Value>) -> Vec<Value> {
    let mut numeric: Vec<(usize, &String)> = args
        .keys()
        .filter_map(|k| k.parse::<usize>().ok().map(|n| (n, k)))
        .collect();
    numeric.sort_by_key(|(n, _)| *n);
    let mut other: Vec<&String> = args
        .keys()
        .filter(|k| k.parse::<usize>().is_err())
        .collect();
    other.sort();
    let mut out: Vec<Value> = numeric.into_iter().map(|(_, k)| args[k].clone()).collect();
    out.extend(other.into_iter().map(|k| args[k].clone()));
    out
}

fn fn_dig(args: &HashMap<String, Value>) -> TeraResult<Value> {
    // tera invokes functions with named args, so we expose two call shapes:
    //
    //   `dig(keys=["a","b"], map=m)`                    -> walk keys, return null if missing
    //   `dig(keys=["a","b"], default="x", map=m)`       -> walk keys, return default if missing
    //
    // For sprig-style variadic positionals (`dig "a" "b" "default" m`) we
    // also accept the `"0"`, `"1"`, …, `"N"` ordered form: last positional is
    // the map, second-to-last is the default, the rest are keys. This keeps
    // raw `text/template` configs that lean on the variadic form working
    // through the preprocessor.
    let map = args.get("map").cloned();
    let default = args.get("default").cloned();
    let keys_arg = args.get("keys").cloned();

    let (keys, default, map) = if let (Some(keys), Some(m)) = (keys_arg, map.clone()) {
        let ks: Vec<String> = keys
            .as_array()
            .map(|a| a.iter().map(arg_string).collect())
            .unwrap_or_else(|| vec![arg_string(&keys)]);
        (ks, default.unwrap_or(Value::Null), m)
    } else {
        let pos = ordered_positionals(args);
        if pos.len() < 2 {
            return Err(err("dig: need at least a map and one key"));
        }
        let m = pos.last().cloned().unwrap();
        if pos.len() < 3 {
            return Ok(m);
        }
        let def = pos[pos.len() - 2].clone();
        let ks: Vec<String> = pos[..pos.len() - 2].iter().map(arg_string).collect();
        (ks, def, m)
    };

    let mut cur = map;
    for k in keys {
        match cur.as_object().and_then(|o| o.get(&k)) {
            Some(v) => cur = v.clone(),
            None => return Ok(default),
        }
    }
    Ok(cur)
}

fn f_get(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // sprig: `get MAP KEY` — returns the value or "" if missing. As a filter
    // we pipe the map: `MAP | get(key=...)`.
    let key = first_positional(args, &["key", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("get: missing key"))?;
    let m = value
        .as_object()
        .ok_or_else(|| err("get: input is not an object"))?;
    Ok(m.get(&key).cloned().unwrap_or(Value::String(String::new())))
}

fn f_must_get(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let key = first_positional(args, &["key", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("mustGet: missing key"))?;
    let m = value
        .as_object()
        .ok_or_else(|| err("mustGet: input is not an object"))?;
    m.get(&key)
        .cloned()
        .ok_or_else(|| err(format!("mustGet: key {key:?} not found")))
}

// ---------------------------------------------------------------------------
// Merge / deep copy

/// Recursive merge: values from `src` win over (or extend) `dst` when both are
/// maps; otherwise `src` replaces `dst` entirely. Used as the worker for both
/// `merge` and `mergeOverwrite` — the two only differ in which side is the
/// "src" and which is the "dst".
fn merge_into(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        // Two-step lookup to avoid holding a mutable borrow of `dst` across
        // the fallback `insert` call.
        let recurse = matches!(
            (dst.get(k), v),
            (Some(Value::Object(_)), Value::Object(_))
        );
        if recurse {
            if let (Some(Value::Object(dm)), Value::Object(sm)) = (dst.get_mut(k), v) {
                merge_into(dm, sm);
            }
        } else {
            dst.insert(k.clone(), v.clone());
        }
    }
}

fn merge_maps_input(args: &HashMap<String, Value>) -> TeraResult<Vec<Value>> {
    // Two call shapes:
    //   - tera-friendly:   merge(a=m1, b=m2)            — named args, sorted by key
    //   - sprig-variadic:  merge(0=m1, 1=m2, ...)       — numeric positionals
    // Plus a convenience `dst=` + `src=` pair for the 2-arg case so callers
    // can be explicit about which side wins.
    if let (Some(dst), Some(src)) = (args.get("dst"), args.get("src")) {
        return Ok(vec![dst.clone(), src.clone()]);
    }
    let pos = ordered_positionals(args);
    if pos.is_empty() {
        return Err(err("merge: need at least one map"));
    }
    Ok(pos)
}

fn fn_merge(args: &HashMap<String, Value>) -> TeraResult<Value> {
    // sprig `merge DST SRC1 SRC2 ...` — DST takes priority, later args only
    // fill in missing keys.
    let pos = merge_maps_input(args)?;
    let mut acc = pos[0]
        .as_object()
        .cloned()
        .ok_or_else(|| err("merge: first arg is not an object"))?;
    for src in &pos[1..] {
        let sm = src
            .as_object()
            .ok_or_else(|| err("merge: arg is not an object"))?;
        merge_missing(&mut acc, sm);
    }
    Ok(Value::Object(acc))
}

fn merge_missing(dst: &mut Map<String, Value>, src: &Map<String, Value>) {
    for (k, v) in src {
        // Two-step lookup pattern (see `merge_into` above).
        match (dst.get(k), v) {
            (Some(Value::Object(_)), Value::Object(_)) => {
                if let (Some(Value::Object(dm)), Value::Object(sm)) = (dst.get_mut(k), v) {
                    merge_missing(dm, sm);
                }
            }
            (None, _) => {
                dst.insert(k.clone(), v.clone());
            }
            _ => { /* existing non-map value wins */ }
        }
    }
}

fn fn_merge_overwrite(args: &HashMap<String, Value>) -> TeraResult<Value> {
    // sprig `mergeOverwrite DST SRC1 SRC2 ...` — later args overwrite earlier.
    let pos = merge_maps_input(args)?;
    let mut acc = pos[0]
        .as_object()
        .cloned()
        .ok_or_else(|| err("mergeOverwrite: first arg is not an object"))?;
    for src in &pos[1..] {
        let sm = src
            .as_object()
            .ok_or_else(|| err("mergeOverwrite: arg is not an object"))?;
        merge_into(&mut acc, sm);
    }
    Ok(Value::Object(acc))
}

fn f_deep_copy(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    // serde_json::Value is `Clone` — a clone is structurally deep.
    Ok(value.clone())
}

// ---------------------------------------------------------------------------
// Reflection (kindOf / typeOf / kindIs / typeIs)

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "invalid",
        Value::Bool(_) => "bool",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "int"
            } else {
                "float64"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "slice",
        Value::Object(_) => "map",
    }
}

fn f_kind_of(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(json_kind(value).to_string()))
}

fn f_kind_is(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // Sprig parity: `kindIs` uses `kind=`, `typeIs` uses `typ=` (Go-ism). Same impl, both kwargs accepted.
    let want = first_positional(args, &["kind", "typ", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("kindIs: missing kind"))?;
    Ok(Value::Bool(json_kind(value) == want))
}

// ---------------------------------------------------------------------------
// Date math

fn parse_go_duration(s: &str) -> Option<Duration> {
    // Subset of Go's time.ParseDuration: signed number + unit, possibly
    // chained ("1h30m"). Units: ns, us/µs, ms, s, m, h. Anything more exotic
    // returns None (caller handles the error). This is enough for the
    // sprig idiom `dateModify "-1h"` / `"24h"`.
    let mut bytes = s.as_bytes();
    let mut sign: i64 = 1;
    if let Some(&b) = bytes.first() {
        if b == b'-' {
            sign = -1;
            bytes = &bytes[1..];
        } else if b == b'+' {
            bytes = &bytes[1..];
        }
    }
    if bytes.is_empty() {
        return None;
    }
    let mut total = Duration::zero();
    let mut i = 0;
    while i < bytes.len() {
        // number portion (integer or fractional)
        let num_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
            i += 1;
        }
        if i == num_start {
            return None;
        }
        let num_str = std::str::from_utf8(&bytes[num_start..i]).ok()?;
        let n: f64 = num_str.parse().ok()?;
        // unit portion
        let unit_start = i;
        // multi-byte unit (µs) — handled below via the raw `s` slice for safety.
        while i < bytes.len() && !bytes[i].is_ascii_digit() && bytes[i] != b'.' {
            i += 1;
        }
        let unit = std::str::from_utf8(&bytes[unit_start..i]).ok()?;
        let nanos = match unit {
            "ns" => n,
            "us" | "µs" => n * 1_000.0,
            "ms" => n * 1_000_000.0,
            "s" => n * 1_000_000_000.0,
            "m" => n * 60.0 * 1_000_000_000.0,
            "h" => n * 3600.0 * 1_000_000_000.0,
            _ => return None,
        };
        total = total.checked_add(&Duration::nanoseconds(nanos as i64))?;
    }
    Some(if sign < 0 { -total } else { total })
}

fn date_modify_impl(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let dur_str = first_positional(args, &["duration", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("dateModify: missing duration"))?;
    let dur = parse_go_duration(&dur_str)
        .ok_or_else(|| err(format!("dateModify: bad duration {dur_str:?}")))?;
    let when = match value {
        Value::Null => Utc::now(),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .map_err(|e| err(format!("dateModify: bad input time: {e}")))?,
        _ => return Err(err("dateModify: input is not a date string")),
    };
    let shifted = when
        .checked_add_signed(dur)
        .ok_or_else(|| err("dateModify: duration overflow"))?;
    Ok(Value::String(shifted.to_rfc3339()))
}

fn f_date_modify(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // Non-`must` form: sprig returns the input on parse failure rather than
    // erroring. Best-effort: if the duration is malformed we return the
    // original value unchanged.
    match date_modify_impl(value, args) {
        Ok(v) => Ok(v),
        Err(_) => Ok(value.clone()),
    }
}

fn f_must_date_modify(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    date_modify_impl(value, args)
}

fn f_unix_epoch(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let when = match value {
        Value::Null => Utc::now(),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|t| t.with_timezone(&Utc))
            .map_err(|e| err(format!("unixEpoch: bad input time: {e}")))?,
        _ => return Err(err("unixEpoch: input is not a date string")),
    };
    Ok(json!(when.timestamp()))
}

// ---------------------------------------------------------------------------
// UUID

fn fn_uuidv4(_: &HashMap<String, Value>) -> TeraResult<Value> {
    Ok(Value::String(Uuid::new_v4().to_string()))
}

// ---------------------------------------------------------------------------
// TOML

fn f_to_toml(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    // serde_json::Value implements Serialize, but TOML has no null type and
    // requires the top-level to be a table — round-tripping through
    // `toml::Value` gives clearer errors than letting the serializer panic
    // mid-way. For non-object inputs we wrap into a single-key table named
    // `value`, matching sprig's "make it work" stance.
    let normalised = match value {
        Value::Object(_) => value.clone(),
        other => json!({ "value": other }),
    };
    let tv: toml::Value =
        serde_json::from_value(normalised).map_err(|e| err(format!("toToml: {e}")))?;
    Ok(Value::String(
        toml::to_string(&tv).map_err(|e| err(format!("toToml: {e}")))?,
    ))
}

fn f_from_toml(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let s = arg_string(value);
    let tv: toml::Value = toml::from_str(&s).map_err(|e| err(format!("fromToml: {e}")))?;
    serde_json::to_value(tv).map_err(|e| err(format!("fromToml: {e}")))
}

// ---------------------------------------------------------------------------
// Regex
//
// Sprig signatures (`pattern` first, then `subject`):
//   regexMatch PATTERN STRING                        -> bool
//   regexFind PATTERN STRING                         -> first match or ""
//   regexFindAll PATTERN STRING N                    -> [matches], N<0 = all
//   regexReplaceAll PATTERN STRING REPLACEMENT       -> string
//   regexSplit PATTERN STRING N                      -> [pieces], N<0 = all
//
// All five are exposed as both filters (subject piped in) and functions
// (`regexMatch(pattern=..., str=...)`). The filter form treats the piped
// value as the subject string and the named/positional `pattern` arg as
// the regex.

fn compile_re(pattern: &str, fname: &str) -> TeraResult<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| err(format!("{fname}: bad pattern: {e}")))
}

fn regex_match_inner(pattern: &str, subject: &str) -> TeraResult<Value> {
    let re = compile_re(pattern, "regexMatch")?;
    Ok(Value::Bool(re.is_match(subject)))
}

fn regex_find_inner(pattern: &str, subject: &str) -> TeraResult<Value> {
    let re = compile_re(pattern, "regexFind")?;
    Ok(Value::String(
        re.find(subject).map(|m| m.as_str().to_string()).unwrap_or_default(),
    ))
}

fn regex_find_all_inner(pattern: &str, subject: &str, n: i64) -> TeraResult<Value> {
    let re = compile_re(pattern, "regexFindAll")?;
    let iter = re.find_iter(subject).map(|m| Value::String(m.as_str().to_string()));
    let collected: Vec<Value> = if n < 0 {
        iter.collect()
    } else {
        iter.take(n as usize).collect()
    };
    Ok(Value::Array(collected))
}

fn regex_replace_all_inner(pattern: &str, subject: &str, repl: &str) -> TeraResult<Value> {
    let re = compile_re(pattern, "regexReplaceAll")?;
    Ok(Value::String(re.replace_all(subject, repl).into_owned()))
}

fn regex_split_inner(pattern: &str, subject: &str, n: i64) -> TeraResult<Value> {
    let re = compile_re(pattern, "regexSplit")?;
    let parts: Vec<Value> = if n < 0 {
        re.split(subject).map(|p| Value::String(p.to_string())).collect()
    } else {
        re.splitn(subject, n as usize)
            .map(|p| Value::String(p.to_string()))
            .collect()
    };
    Ok(Value::Array(parts))
}

fn pick_pattern(args: &HashMap<String, Value>, fname: &str) -> TeraResult<String> {
    first_positional(args, &["pattern", "regex", "re", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err(format!("{fname}: missing pattern")))
}

fn pick_subject(args: &HashMap<String, Value>, fname: &str) -> TeraResult<String> {
    first_positional(args, &["str", "string", "subject", "input", "1"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err(format!("{fname}: missing subject string")))
}

fn pick_n(args: &HashMap<String, Value>, default: i64) -> i64 {
    first_positional(args, &["n", "count", "limit", "2"])
        .and_then(|v| arg_i64(&v))
        .unwrap_or(default)
}

fn f_regex_match(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexMatch")?;
    regex_match_inner(&pattern, &arg_string(value))
}

fn fn_regex_match(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexMatch")?;
    let subject = pick_subject(args, "regexMatch")?;
    regex_match_inner(&pattern, &subject)
}

fn f_regex_find(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexFind")?;
    regex_find_inner(&pattern, &arg_string(value))
}

fn fn_regex_find(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexFind")?;
    let subject = pick_subject(args, "regexFind")?;
    regex_find_inner(&pattern, &subject)
}

fn f_regex_find_all(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexFindAll")?;
    let n = pick_n(args, -1);
    regex_find_all_inner(&pattern, &arg_string(value), n)
}

fn fn_regex_find_all(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexFindAll")?;
    let subject = pick_subject(args, "regexFindAll")?;
    let n = pick_n(args, -1);
    regex_find_all_inner(&pattern, &subject, n)
}

fn f_regex_replace_all(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexReplaceAll")?;
    let repl = first_positional(args, &["repl", "replacement", "with", "to", "1"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("regexReplaceAll: missing replacement"))?;
    regex_replace_all_inner(&pattern, &arg_string(value), &repl)
}

fn fn_regex_replace_all(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexReplaceAll")?;
    let subject = pick_subject(args, "regexReplaceAll")?;
    let repl = first_positional(args, &["repl", "replacement", "with", "to", "2"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("regexReplaceAll: missing replacement"))?;
    regex_replace_all_inner(&pattern, &subject, &repl)
}

fn f_regex_split(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexSplit")?;
    let n = pick_n(args, -1);
    regex_split_inner(&pattern, &arg_string(value), n)
}

fn fn_regex_split(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let pattern = pick_pattern(args, "regexSplit")?;
    let subject = pick_subject(args, "regexSplit")?;
    let n = pick_n(args, -1);
    regex_split_inner(&pattern, &subject, n)
}

// ---------------------------------------------------------------------------
// Semver
//
// Minimal inline parser — we deliberately do not pull in the `semver` crate
// because the sprig surface area we need is small. Supports:
//   - "MAJOR.MINOR.PATCH" with optional "-PRERELEASE" and "+BUILD"
//   - leading "v" or "V" prefix is stripped, matching Go semver/sprig.
//   - comparison ops: ==, =, !=, >, <, >=, <=, ^MAJOR.MINOR.PATCH,
//     ~MAJOR.MINOR.PATCH
//   - constraint strings may chain with whitespace as AND
//     (e.g. ">=1.2.0 <2.0.0")
//
// Build metadata is ignored in comparisons (per semver spec). Prerelease
// ordering compares dot-separated identifiers, with numeric identifiers
// compared numerically — enough for "^1.2.3" style checks against
// real-world kairos versions.

#[derive(Clone, Debug, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Vec<String>,
    // build metadata intentionally dropped after parsing — never compared.
}

fn parse_semver(input: &str) -> Option<SemVer> {
    let s = input.trim();
    let s = s.strip_prefix('v').or_else(|| s.strip_prefix('V')).unwrap_or(s);
    // Split off build metadata first (the "+..." suffix).
    let core_and_pre = match s.split_once('+') {
        Some((a, _b)) => a,
        None => s,
    };
    let (core, pre) = match core_and_pre.split_once('-') {
        Some((a, b)) => (a, Some(b)),
        None => (core_and_pre, None),
    };
    let mut parts = core.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let pre_parts = pre
        .map(|p| p.split('.').map(|x| x.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    Some(SemVer {
        major,
        minor,
        patch,
        pre: pre_parts,
    })
}

fn cmp_pre(a: &[String], b: &[String]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // Per semver: a version *with* prerelease has lower precedence than the
    // same version *without*.
    match (a.is_empty(), b.is_empty()) {
        (true, true) => return Ordering::Equal,
        (true, false) => return Ordering::Greater,
        (false, true) => return Ordering::Less,
        _ => {}
    }
    for (x, y) in a.iter().zip(b.iter()) {
        let xn = x.parse::<u64>().ok();
        let yn = y.parse::<u64>().ok();
        let ord = match (xn, yn) {
            (Some(xi), Some(yi)) => xi.cmp(&yi),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

fn cmp_semver(a: &SemVer, b: &SemVer) -> std::cmp::Ordering {
    a.major
        .cmp(&b.major)
        .then(a.minor.cmp(&b.minor))
        .then(a.patch.cmp(&b.patch))
        .then(cmp_pre(&a.pre, &b.pre))
}

fn semver_to_value(v: &SemVer) -> Value {
    let mut m = Map::new();
    m.insert("major".into(), json!(v.major));
    m.insert("minor".into(), json!(v.minor));
    m.insert("patch".into(), json!(v.patch));
    m.insert("prerelease".into(), Value::String(v.pre.join(".")));
    let mut s = format!("{}.{}.{}", v.major, v.minor, v.patch);
    if !v.pre.is_empty() {
        s.push('-');
        s.push_str(&v.pre.join("."));
    }
    m.insert("original".into(), Value::String(s));
    Value::Object(m)
}

/// Apply a single constraint clause against a version.
fn check_single_constraint(clause: &str, version: &SemVer) -> TeraResult<bool> {
    use std::cmp::Ordering;
    let clause = clause.trim();
    if clause.is_empty() {
        return Ok(true);
    }
    // Order matters — "<=" must be tested before "<".
    let (op, rest) = if let Some(r) = clause.strip_prefix(">=") {
        (">=", r)
    } else if let Some(r) = clause.strip_prefix("<=") {
        ("<=", r)
    } else if let Some(r) = clause.strip_prefix("==") {
        ("==", r)
    } else if let Some(r) = clause.strip_prefix("!=") {
        ("!=", r)
    } else if let Some(r) = clause.strip_prefix('>') {
        (">", r)
    } else if let Some(r) = clause.strip_prefix('<') {
        ("<", r)
    } else if let Some(r) = clause.strip_prefix('=') {
        ("==", r)
    } else if let Some(r) = clause.strip_prefix('^') {
        ("^", r)
    } else if let Some(r) = clause.strip_prefix('~') {
        ("~", r)
    } else {
        // Bare version implies equality.
        ("==", clause)
    };
    let target = parse_semver(rest.trim())
        .ok_or_else(|| err(format!("semverCompare: bad version `{rest}`")))?;
    let ord = cmp_semver(version, &target);
    let ok = match op {
        "==" => ord == Ordering::Equal,
        "!=" => ord != Ordering::Equal,
        ">" => ord == Ordering::Greater,
        "<" => ord == Ordering::Less,
        ">=" => ord != Ordering::Less,
        "<=" => ord != Ordering::Greater,
        "^" => {
            // Caret: compatible within the same leading non-zero component.
            // ^1.2.3 -> >=1.2.3, <2.0.0
            // ^0.2.3 -> >=0.2.3, <0.3.0
            // ^0.0.3 -> >=0.0.3, <0.0.4
            if ord == Ordering::Less {
                false
            } else if target.major > 0 {
                version.major == target.major
            } else if target.minor > 0 {
                version.major == 0 && version.minor == target.minor
            } else {
                version.major == 0 && version.minor == 0 && version.patch == target.patch
            }
        }
        "~" => {
            // Tilde: allow patch-level changes within MAJOR.MINOR.
            // ~1.2.3 -> >=1.2.3, <1.3.0
            if ord == Ordering::Less {
                false
            } else {
                version.major == target.major && version.minor == target.minor
            }
        }
        _ => return Err(err(format!("semverCompare: unknown op `{op}`"))),
    };
    Ok(ok)
}

fn semver_compare_inner(constraint: &str, version: &str) -> TeraResult<Value> {
    let v = parse_semver(version)
        .ok_or_else(|| err(format!("semverCompare: bad version `{version}`")))?;
    // Whitespace splits clauses; all must hold (AND).
    for clause in constraint.split_whitespace() {
        if !check_single_constraint(clause, &v)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn f_semver(value: &Value, _: &HashMap<String, Value>) -> TeraResult<Value> {
    let s = arg_string(value);
    match parse_semver(&s) {
        Some(v) => Ok(semver_to_value(&v)),
        None => Ok(Value::Null),
    }
}

fn fn_semver(args: &HashMap<String, Value>) -> TeraResult<Value> {
    let s = first_positional(args, &["version", "v", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("semver: missing version"))?;
    match parse_semver(&s) {
        Some(v) => Ok(semver_to_value(&v)),
        None => Ok(Value::Null),
    }
}

fn f_semver_compare(value: &Value, args: &HashMap<String, Value>) -> TeraResult<Value> {
    // Filter form: `VERSION | semverCompare(constraint="^1.2.3")`.
    let constraint = first_positional(args, &["constraint", "range", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("semverCompare: missing constraint"))?;
    semver_compare_inner(&constraint, &arg_string(value))
}

fn fn_semver_compare(args: &HashMap<String, Value>) -> TeraResult<Value> {
    // Function form mirrors sprig: `semverCompare CONSTRAINT VERSION`.
    let constraint = first_positional(args, &["constraint", "range", "0"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("semverCompare: missing constraint"))?;
    let version = first_positional(args, &["version", "v", "1"])
        .map(|v| arg_string(&v))
        .ok_or_else(|| err("semverCompare: missing version"))?;
    semver_compare_inner(&constraint, &version)
}

// ---------------------------------------------------------------------------
// Registration
//
// Sprig funcs ported (tera filter unless noted [fn=function]):
//
//   String:    lower, upper, title, trim, trimAll, trimPrefix, trimSuffix,
//              replace, repeat, contains, hasPrefix, hasSuffix, split, join,
//              quote, squote, cat, indent, nindent
//   Default:   default, empty, coalesce [fn]
//   Lists:     first, last, len, append, prepend, concat [fn], uniq
//   Maps:      keys, values, hasKey, pluck, pick, omit, dict [fn], set, unset
//   Math:      add [fn], sub [fn], mul [fn], div [fn], mod [fn],
//              min [fn], max [fn]
//   Conv:      int, int64, float, toString, toBool, toJson, toYaml,
//              fromJson, fromYaml
//   Encoding:  b64enc, b64dec, sha256sum, sha512sum, md5sum,
//              randAlphaNum [fn], randAlpha [fn], randNumeric [fn]
//   Date:      now [fn], date, dateInZone, dateModify, mustDateModify,
//              unixEpoch
//   OS:        env [fn], expandenv [fn + filter], tempDir [fn]
//   Map deep:  dig [fn], get, mustGet, merge [fn], mergeOverwrite [fn],
//              deepCopy
//   Reflect:   kindOf, typeOf, kindIs, typeIs
//   UUID:      uuidv4 [fn]
//   TOML:      toToml, fromToml
//   Regex:     regexMatch, regexFind, regexFindAll, regexReplaceAll,
//              regexSplit (all both [filter + fn])
//   Semver:    semver, semverCompare (both [filter + fn]); minimal inline
//              parser — no `semver` crate dep added.
//
// TODO(sprig): not yet ported (add when a kairos config needs them):
//   - htpasswd / bcrypt / derivePassword / genPrivateKey
//     (bcrypt is not in deps — htpasswd requires hand-rolling or a new crate
//     dependency; punted per scoping guidance)

pub fn register_all(t: &mut Tera) {
    // Strings
    t.register_filter("lower", f_lower);
    t.register_filter("upper", f_upper);
    t.register_filter("title", f_title);
    t.register_filter("trim", f_trim);
    t.register_filter("trimAll", f_trim_all);
    t.register_filter("trim_all", f_trim_all);
    t.register_filter("trimPrefix", f_trim_prefix);
    t.register_filter("trim_prefix", f_trim_prefix);
    t.register_filter("trimSuffix", f_trim_suffix);
    t.register_filter("trim_suffix", f_trim_suffix);
    t.register_filter("replace", f_replace);
    t.register_filter("repeat", f_repeat);
    t.register_filter("contains", f_contains);
    t.register_filter("hasPrefix", f_has_prefix);
    t.register_filter("has_prefix", f_has_prefix);
    t.register_filter("hasSuffix", f_has_suffix);
    t.register_filter("has_suffix", f_has_suffix);
    t.register_filter("split", f_split);
    t.register_filter("join", f_join);
    t.register_filter("quote", f_quote);
    t.register_filter("squote", f_squote);
    t.register_filter("cat", f_cat);
    t.register_filter("indent", f_indent);
    t.register_filter("nindent", f_nindent);

    // Default / null
    t.register_filter("default", f_default);
    t.register_filter("empty", f_empty);
    t.register_function("coalesce", fn_coalesce);

    // Lists
    t.register_filter("first", f_first);
    t.register_filter("last", f_last);
    t.register_filter("len", f_len);
    t.register_filter("length", f_len);
    t.register_filter("append", f_append);
    t.register_filter("prepend", f_prepend);
    t.register_function("concat", fn_concat);
    t.register_filter("uniq", f_uniq);
    t.register_filter("unique", f_uniq);

    // Maps
    t.register_filter("keys", f_keys);
    t.register_filter("values", f_values);
    t.register_filter("hasKey", f_has_key);
    t.register_filter("has_key", f_has_key);
    t.register_filter("pluck", f_pluck);
    t.register_filter("pick", f_pick);
    t.register_filter("omit", f_omit);
    t.register_function("dict", fn_dict);
    t.register_filter("set", f_set);
    t.register_filter("unset", f_unset);

    // Math
    t.register_function("add", fn_add);
    t.register_function("sub", fn_sub);
    t.register_function("mul", fn_mul);
    t.register_function("div", fn_div);
    t.register_function("mod", fn_mod);
    t.register_function("min", fn_min);
    t.register_function("max", fn_max);

    // Conversion
    t.register_filter("int", f_int);
    t.register_filter("int64", f_int64);
    t.register_filter("float", f_float);
    t.register_filter("toString", f_to_string);
    t.register_filter("to_string", f_to_string);
    t.register_filter("toBool", f_to_bool);
    t.register_filter("to_bool", f_to_bool);
    t.register_filter("toJson", f_to_json);
    t.register_filter("to_json", f_to_json);
    t.register_filter("toYaml", f_to_yaml);
    t.register_filter("to_yaml", f_to_yaml);
    t.register_filter("fromJson", f_from_json);
    t.register_filter("from_json", f_from_json);
    t.register_filter("fromYaml", f_from_yaml);
    t.register_filter("from_yaml", f_from_yaml);

    // Encoding / hashing
    t.register_filter("b64enc", f_b64enc);
    t.register_filter("b64dec", f_b64dec);
    t.register_filter("sha256sum", f_sha256);
    t.register_filter("sha512sum", f_sha512);
    t.register_filter("md5sum", f_md5);
    t.register_function("randAlphaNum", fn_rand_alpha_num);
    t.register_function("rand_alpha_num", fn_rand_alpha_num);
    t.register_function("randAlpha", fn_rand_alpha);
    t.register_function("rand_alpha", fn_rand_alpha);
    t.register_function("randNumeric", fn_rand_numeric);
    t.register_function("rand_numeric", fn_rand_numeric);

    // Date
    t.register_function("now", fn_now);
    t.register_filter("date", f_date);
    t.register_filter("dateInZone", f_date_in_zone);
    t.register_filter("date_in_zone", f_date_in_zone);

    // OS / env
    t.register_function("env", fn_env);
    t.register_function("expandenv", fn_expandenv);
    t.register_filter("expandenv", f_expandenv_filter);
    t.register_function("tempDir", fn_temp_dir);
    t.register_function("temp_dir", fn_temp_dir);

    // Deep map access
    t.register_function("dig", fn_dig);
    t.register_filter("get", f_get);
    t.register_filter("mustGet", f_must_get);
    t.register_filter("must_get", f_must_get);

    // Merge / deep copy
    t.register_function("merge", fn_merge);
    t.register_function("mergeOverwrite", fn_merge_overwrite);
    t.register_function("merge_overwrite", fn_merge_overwrite);
    t.register_filter("deepCopy", f_deep_copy);
    t.register_filter("deep_copy", f_deep_copy);

    // Reflection
    t.register_filter("kindOf", f_kind_of);
    t.register_filter("kind_of", f_kind_of);
    t.register_filter("typeOf", f_kind_of);
    t.register_filter("type_of", f_kind_of);
    t.register_filter("kindIs", f_kind_is);
    t.register_filter("kind_is", f_kind_is);
    t.register_filter("typeIs", f_kind_is);
    t.register_filter("type_is", f_kind_is);

    // Date math
    t.register_filter("dateModify", f_date_modify);
    t.register_filter("date_modify", f_date_modify);
    t.register_filter("mustDateModify", f_must_date_modify);
    t.register_filter("must_date_modify", f_must_date_modify);
    t.register_filter("unixEpoch", f_unix_epoch);
    t.register_filter("unix_epoch", f_unix_epoch);

    // UUID
    t.register_function("uuidv4", fn_uuidv4);

    // TOML
    t.register_filter("toToml", f_to_toml);
    t.register_filter("to_toml", f_to_toml);
    t.register_filter("fromToml", f_from_toml);
    t.register_filter("from_toml", f_from_toml);

    // Regex (sprig: regexMatch / regexFind / regexFindAll / regexReplaceAll /
    // regexSplit). Both filter form (`STR | regexMatch(pattern="…")`) and
    // function form (`regexMatch(pattern="…", str="…")`) are registered, with
    // snake_case aliases for tera idiomatic style.
    t.register_filter("regexMatch", f_regex_match);
    t.register_filter("regex_match", f_regex_match);
    t.register_function("regexMatch", fn_regex_match);
    t.register_function("regex_match", fn_regex_match);
    t.register_filter("regexFind", f_regex_find);
    t.register_filter("regex_find", f_regex_find);
    t.register_function("regexFind", fn_regex_find);
    t.register_function("regex_find", fn_regex_find);
    t.register_filter("regexFindAll", f_regex_find_all);
    t.register_filter("regex_find_all", f_regex_find_all);
    t.register_function("regexFindAll", fn_regex_find_all);
    t.register_function("regex_find_all", fn_regex_find_all);
    t.register_filter("regexReplaceAll", f_regex_replace_all);
    t.register_filter("regex_replace_all", f_regex_replace_all);
    t.register_function("regexReplaceAll", fn_regex_replace_all);
    t.register_function("regex_replace_all", fn_regex_replace_all);
    t.register_filter("regexSplit", f_regex_split);
    t.register_filter("regex_split", f_regex_split);
    t.register_function("regexSplit", fn_regex_split);
    t.register_function("regex_split", fn_regex_split);

    // Semver
    t.register_filter("semver", f_semver);
    t.register_function("semver", fn_semver);
    t.register_filter("semverCompare", f_semver_compare);
    t.register_filter("semver_compare", f_semver_compare);
    t.register_function("semverCompare", fn_semver_compare);
    t.register_function("semver_compare", fn_semver_compare);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::engine::render;
    use serde_json::{json, Value};

    fn r(tmpl: &str) -> String {
        render(tmpl, &Value::Null).unwrap()
    }

    #[test]
    fn upper_filter() {
        assert_eq!(r(r#"{{ "abc" | upper }}"#), "ABC");
    }

    #[test]
    fn lower_filter() {
        assert_eq!(r(r#"{{ "ABC" | lower }}"#), "abc");
    }

    #[test]
    fn sha256_filter() {
        // SHA-256("abc") = ba7816bf8f01cfea4141 40de5dae2223 b00361a39617 7a9cb410ff61f20015ad
        let out = r(r#"{{ "abc" | sha256sum }}"#);
        assert_eq!(
            out,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn md5_filter() {
        // md5("abc") = 900150983cd24fb0d6963f7d28e17f72
        assert_eq!(r(r#"{{ "abc" | md5sum }}"#), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn b64_roundtrip() {
        let enc = r(r#"{{ "hello" | b64enc }}"#);
        assert_eq!(enc, "aGVsbG8=");
        let data = json!({"v": "aGVsbG8="});
        let dec = render(r#"{{ .v | b64dec }}"#, &data).unwrap();
        assert_eq!(dec, "hello");
    }

    #[test]
    fn default_filter_present_value() {
        let data = json!({"name": "kairos"});
        let out = render(r#"{{ .name | default(value="anon") }}"#, &data).unwrap();
        assert_eq!(out, "kairos");
    }

    #[test]
    fn int_filter() {
        assert_eq!(r(r#"{{ "42" | int }}"#), "42");
    }

    #[test]
    fn add_function() {
        // tera uses named args; sprig is variadic. We test the named-arg form.
        assert_eq!(r(r#"{{ add(a=2, b=3) }}"#), "5");
    }

    #[test]
    fn mul_function() {
        assert_eq!(r(r#"{{ mul(a=4, b=5) }}"#), "20");
    }

    #[test]
    fn indent_filter() {
        // Pass the newline through the context — tera string literals do
        // NOT process escape sequences (see replace_string_markers in the
        // tera parser), so a literal "\n" inside `"..."` is the two chars
        // `\` and `n`, not a newline.
        let data = json!({"s": "hello\nworld"});
        let out = render(r#"{{ .s | indent(count=2) }}"#, &data).unwrap();
        assert_eq!(out, "  hello\n  world");
    }

    #[test]
    fn trim_prefix_filter() {
        assert_eq!(
            r(r#"{{ "kairos-foo" | trimPrefix(prefix="kairos-") }}"#),
            "foo"
        );
    }

    #[test]
    fn has_prefix_filter() {
        assert_eq!(r(r#"{{ "kairos-foo" | hasPrefix(prefix="kairos") }}"#), "true");
    }

    #[test]
    fn quote_filter() {
        assert_eq!(r(r#"{{ "x" | quote }}"#), "\"x\"");
    }

    #[test]
    fn join_filter() {
        let data = json!({"xs": ["a", "b", "c"]});
        let out = render(r#"{{ .xs | join(sep=",") }}"#, &data).unwrap();
        assert_eq!(out, "a,b,c");
    }

    #[test]
    fn first_last_len() {
        let data = json!({"xs": [10, 20, 30]});
        assert_eq!(render(r#"{{ .xs | first }}"#, &data).unwrap(), "10");
        assert_eq!(render(r#"{{ .xs | last }}"#, &data).unwrap(), "30");
        assert_eq!(render(r#"{{ .xs | len }}"#, &data).unwrap(), "3");
    }

    #[test]
    fn keys_values() {
        let data = json!({"m": {"a": 1}});
        let out = render(r#"{{ .m | keys | first }}"#, &data).unwrap();
        assert_eq!(out, "a");
    }

    #[test]
    fn empty_filter() {
        let data = json!({"a": "", "b": "x"});
        assert_eq!(render(r#"{{ .a | empty }}"#, &data).unwrap(), "true");
        assert_eq!(render(r#"{{ .b | empty }}"#, &data).unwrap(), "false");
    }

    #[test]
    fn to_json_filter() {
        let data = json!({"a": {"k": "v"}});
        let out = render(r#"{{ .a | toJson }}"#, &data).unwrap();
        assert_eq!(out, r#"{"k":"v"}"#);
    }

    #[test]
    fn env_function() {
        std::env::set_var("YIP_RS_TEMPLATE_TEST", "hello");
        let out = r(r#"{{ env(name="YIP_RS_TEMPLATE_TEST") }}"#);
        assert_eq!(out, "hello");
    }

    #[test]
    fn expandenv_filter() {
        std::env::set_var("YIP_RS_TEMPLATE_EXP", "world");
        let out = r(r#"{{ "hi $YIP_RS_TEMPLATE_EXP" | expandenv }}"#);
        assert_eq!(out, "hi world");
    }

    // -----------------------------------------------------------------------
    // dig / get / mustGet

    #[test]
    fn dig_walks_nested_map() {
        let data = json!({"m": {"a": {"b": {"c": "deep"}}}});
        let out = render(
            r#"{{ dig(keys=["a","b","c"], default="x", map=m) }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "deep");
    }

    #[test]
    fn dig_missing_returns_default() {
        let data = json!({"m": {"a": {"b": 1}}});
        let out = render(
            r#"{{ dig(keys=["a","b","c"], default="fallback", map=m) }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "fallback");
    }

    #[test]
    fn get_filter_present_and_missing() {
        let data = json!({"m": {"k": "v"}});
        assert_eq!(
            render(r#"{{ m | get(key="k") }}"#, &data).unwrap(),
            "v"
        );
        assert_eq!(
            render(r#"{{ m | get(key="absent") }}"#, &data).unwrap(),
            ""
        );
    }

    #[test]
    fn must_get_present() {
        let data = json!({"m": {"k": "v"}});
        assert_eq!(
            render(r#"{{ m | mustGet(key="k") }}"#, &data).unwrap(),
            "v"
        );
    }

    #[test]
    fn must_get_missing_errors() {
        let data = json!({"m": {"k": "v"}});
        let res = render(r#"{{ m | mustGet(key="nope") }}"#, &data);
        assert!(res.is_err(), "expected mustGet to error on missing key");
    }

    // -----------------------------------------------------------------------
    // merge / mergeOverwrite / deepCopy

    #[test]
    fn merge_dst_wins() {
        // merge: first arg (dst) wins; src only fills missing keys.
        let data = json!({"d": {"a": 1, "b": 2}, "s": {"b": 99, "c": 3}});
        let out = render(
            r#"{{ merge(dst=d, src=s) | toJson }}"#,
            &data,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"a": 1, "b": 2, "c": 3}));
    }

    #[test]
    fn merge_overwrite_src_wins() {
        let data = json!({"d": {"a": 1, "b": 2}, "s": {"b": 99, "c": 3}});
        let out = render(
            r#"{{ mergeOverwrite(dst=d, src=s) | toJson }}"#,
            &data,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"a": 1, "b": 99, "c": 3}));
    }

    #[test]
    fn merge_overwrite_nested() {
        let data = json!({
            "d": {"k": {"x": 1, "y": 2}},
            "s": {"k": {"y": 99, "z": 3}},
        });
        let out = render(
            r#"{{ mergeOverwrite(dst=d, src=s) | toJson }}"#,
            &data,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"k": {"x": 1, "y": 99, "z": 3}}));
    }

    #[test]
    fn deep_copy_clones_value() {
        let data = json!({"m": {"a": [1, 2, 3]}});
        let out = render(r#"{{ m | deepCopy | toJson }}"#, &data).unwrap();
        assert_eq!(out, r#"{"a":[1,2,3]}"#);
    }

    // -----------------------------------------------------------------------
    // kindOf / typeOf / kindIs / typeIs

    #[test]
    fn kind_of_basics() {
        let data = json!({
            "s": "hi",
            "i": 7,
            "f": 1.5,
            "b": true,
            "a": [1, 2],
            "m": {"k": "v"},
        });
        assert_eq!(render(r#"{{ s | kindOf }}"#, &data).unwrap(), "string");
        assert_eq!(render(r#"{{ i | kindOf }}"#, &data).unwrap(), "int");
        assert_eq!(render(r#"{{ f | kindOf }}"#, &data).unwrap(), "float64");
        assert_eq!(render(r#"{{ b | kindOf }}"#, &data).unwrap(), "bool");
        assert_eq!(render(r#"{{ a | kindOf }}"#, &data).unwrap(), "slice");
        assert_eq!(render(r#"{{ m | kindOf }}"#, &data).unwrap(), "map");
    }

    #[test]
    fn type_of_is_alias_of_kind_of() {
        let data = json!({"s": "hi"});
        assert_eq!(render(r#"{{ s | typeOf }}"#, &data).unwrap(), "string");
    }

    #[test]
    fn kind_is_matches() {
        let data = json!({"s": "hi", "i": 7});
        assert_eq!(
            render(r#"{{ s | kindIs(kind="string") }}"#, &data).unwrap(),
            "true"
        );
        assert_eq!(
            render(r#"{{ i | kindIs(kind="string") }}"#, &data).unwrap(),
            "false"
        );
    }

    #[test]
    fn type_is_matches() {
        let data = json!({"a": [1]});
        assert_eq!(
            render(r#"{{ a | typeIs(typ="slice") }}"#, &data).unwrap(),
            "true"
        );
    }

    // -----------------------------------------------------------------------
    // dateModify / mustDateModify / unixEpoch

    #[test]
    fn date_modify_adds_duration() {
        let data = json!({"t": "2024-01-01T00:00:00Z"});
        let out = render(r#"{{ t | dateModify(duration="1h") }}"#, &data).unwrap();
        // chrono's rfc3339 for UTC uses "+00:00" suffix.
        assert!(
            out.starts_with("2024-01-01T01:00:00"),
            "got {out:?}"
        );
    }

    #[test]
    fn date_modify_subtracts_duration() {
        let data = json!({"t": "2024-01-01T12:00:00Z"});
        let out = render(r#"{{ t | dateModify(duration="-30m") }}"#, &data).unwrap();
        assert!(
            out.starts_with("2024-01-01T11:30:00"),
            "got {out:?}"
        );
    }

    #[test]
    fn date_modify_chained_units() {
        let data = json!({"t": "2024-01-01T00:00:00Z"});
        let out = render(r#"{{ t | dateModify(duration="1h30m") }}"#, &data).unwrap();
        assert!(
            out.starts_with("2024-01-01T01:30:00"),
            "got {out:?}"
        );
    }

    #[test]
    fn must_date_modify_errors_on_bad_duration() {
        let data = json!({"t": "2024-01-01T00:00:00Z"});
        let res = render(
            r#"{{ t | mustDateModify(duration="garbage") }}"#,
            &data,
        );
        assert!(res.is_err(), "expected mustDateModify to error");
    }

    #[test]
    fn date_modify_lenient_on_bad_duration() {
        // Non-`must` form returns input on parse failure.
        let data = json!({"t": "2024-01-01T00:00:00Z"});
        let out = render(r#"{{ t | dateModify(duration="garbage") }}"#, &data).unwrap();
        assert_eq!(out, "2024-01-01T00:00:00Z");
    }

    #[test]
    fn unix_epoch_basic() {
        let data = json!({"t": "1970-01-01T00:00:42Z"});
        let out = render(r#"{{ t | unixEpoch }}"#, &data).unwrap();
        assert_eq!(out, "42");
    }

    // -----------------------------------------------------------------------
    // uuidv4

    #[test]
    fn uuidv4_function_shape() {
        let out = r(r#"{{ uuidv4() }}"#);
        // Standard UUID string is 36 chars: 8-4-4-4-12 with version 4 nibble.
        assert_eq!(out.len(), 36, "got {out:?}");
        let bytes = out.as_bytes();
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[14], b'4'); // v4 version marker
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
    }

    #[test]
    fn uuidv4_is_unique() {
        let a = r(r#"{{ uuidv4() }}"#);
        let b = r(r#"{{ uuidv4() }}"#);
        assert_ne!(a, b);
    }

    // -----------------------------------------------------------------------
    // toToml / fromToml

    #[test]
    fn to_toml_emits_kv() {
        let data = json!({"m": {"name": "kairos", "n": 7}});
        let out = render(r#"{{ m | toToml }}"#, &data).unwrap();
        // toml::to_string is stable per field but field order is map iteration.
        assert!(out.contains("name = \"kairos\""), "got {out:?}");
        assert!(out.contains("n = 7"), "got {out:?}");
    }

    #[test]
    fn from_toml_roundtrip() {
        let data = json!({"s": "name = \"kairos\"\nn = 7\n"});
        let out = render(r#"{{ s | fromToml | toJson }}"#, &data).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, json!({"name": "kairos", "n": 7}));
    }

    // -----------------------------------------------------------------
    // Extended filter coverage: edge inputs, known vectors, round-trips.

    #[test]
    fn pure_text_filters_edge_inputs() {
        // Empty string: each text filter must return an empty string
        // rather than panicking or producing whitespace.
        let data = json!({"s": ""});
        assert_eq!(render(r#"{{ .s | upper }}"#, &data).unwrap(), "");
        assert_eq!(render(r#"{{ .s | lower }}"#, &data).unwrap(), "");
        assert_eq!(render(r#"{{ .s | trim }}"#, &data).unwrap(), "");
        assert_eq!(render(r#"{{ .s | title }}"#, &data).unwrap(), "");

        // Very long input: 10k chars, a stress test for the per-char
        // walk in `title` and the buffer growth in `upper`/`lower`.
        let long: String = "a".repeat(10_000);
        let data = json!({"s": long});
        let up = render(r#"{{ .s | upper }}"#, &data).unwrap();
        assert_eq!(up.len(), 10_000);
        assert!(up.chars().all(|c| c == 'A'));

        // Unicode: text filters must operate on the underlying UTF-8
        // string without splitting code points.
        let data = json!({"s": "café 日本語"});
        let up = render(r#"{{ .s | upper }}"#, &data).unwrap();
        assert_eq!(up, "CAFÉ 日本語");
        let lo = render(r#"{{ .s | lower }}"#, &data).unwrap();
        assert_eq!(lo, "café 日本語");
    }

    #[test]
    fn to_json_from_json_roundtrip_nested() {
        // Round-trip a nested object via toJson -> fromJson. The
        // intermediate string is what a yip config would persist, e.g.
        // when stashing structured data into an env var.
        let data = json!({
            "obj": {
                "a": 1,
                "b": {"c": [1, 2, 3], "d": "x"}
            }
        });
        let out = render(
            r#"{{ .obj | toJson | fromJson | toJson }}"#,
            &data,
        )
        .unwrap();
        // Field ordering may differ across the round-trip; parse both
        // ends and compare structurally.
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, data["obj"]);
    }

    #[test]
    fn to_yaml_from_yaml_roundtrip() {
        let data = json!({"obj": {"a": 1, "b": "two", "c": [true, false]}});
        // toYaml emits a YAML string; fromYaml parses it back. We
        // then re-emit toJson so the output is a deterministic shape.
        let out = render(
            r#"{{ .obj | toYaml | fromYaml | toJson }}"#,
            &data,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed, data["obj"]);
    }

    #[test]
    fn date_with_specific_input_time() {
        // Render a fixed RFC3339 input through Go-style layout tokens.
        // The Go tokens "2006-01-02" map to chrono's "%Y-%m-%d".
        let data = json!({"t": "2024-03-15T12:34:56Z"});
        let out = render(r#"{{ .t | date(format="2006-01-02") }}"#, &data).unwrap();
        assert_eq!(out, "2024-03-15");
    }

    #[test]
    fn date_in_zone_with_specific_input_time() {
        // dateInZone currently approximates by ignoring the zone and
        // rendering UTC (see f_date_in_zone). Verify the format pass.
        let data = json!({"t": "2024-03-15T12:34:56Z"});
        let out = render(
            r#"{{ .t | dateInZone(format="2006-01-02", zone="UTC") }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "2024-03-15");
    }

    #[test]
    fn sha_known_vectors() {
        // NIST / RFC 1321 / RFC 6234 known vectors for the empty string.
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        // sha512("") = cf83e1357eefb8bd...3a538327af927da3e
        // md5("")    = d41d8cd98f00b204e9800998ecf8427e
        let data = json!({"empty": ""});
        let sha256_empty = render(r#"{{ .empty | sha256sum }}"#, &data).unwrap();
        assert_eq!(
            sha256_empty,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let sha512_empty = render(r#"{{ .empty | sha512sum }}"#, &data).unwrap();
        assert_eq!(
            sha512_empty,
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        let md5_empty = render(r#"{{ .empty | md5sum }}"#, &data).unwrap();
        assert_eq!(md5_empty, "d41d8cd98f00b204e9800998ecf8427e");

        // RFC 6234 test vector for "abc".
        let sha512_abc = r(r#"{{ "abc" | sha512sum }}"#);
        assert_eq!(
            sha512_abc,
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn b64_roundtrip_binary_like_data() {
        // Round-trip a payload containing non-ASCII bytes (here as
        // UTF-8 multibyte sequences plus control chars). b64dec returns
        // a String, so we constrain the input to be valid UTF-8.
        let payload = "\u{0001}\u{0002}café \u{007f}\nline2";
        let data = json!({"v": payload});
        let enc = render(r#"{{ .v | b64enc }}"#, &data).unwrap();
        let dec_data = json!({"enc": enc});
        let dec = render(r#"{{ .enc | b64dec }}"#, &dec_data).unwrap();
        assert_eq!(dec, payload);
    }

    #[test]
    fn rand_alpha_length_and_charset() {
        let out = r(r#"{{ randAlpha(count=20) }}"#);
        assert_eq!(out.len(), 20);
        assert!(
            out.chars().all(|c| c.is_ascii_alphabetic()),
            "randAlpha output contains non-alpha: {out:?}"
        );
    }

    #[test]
    fn rand_numeric_length_and_charset() {
        let out = r(r#"{{ randNumeric(count=12) }}"#);
        assert_eq!(out.len(), 12);
        assert!(
            out.chars().all(|c| c.is_ascii_digit()),
            "randNumeric output contains non-digit: {out:?}"
        );
    }

    #[test]
    fn rand_alpha_num_length_and_charset() {
        let out = r(r#"{{ randAlphaNum(count=16) }}"#);
        assert_eq!(out.len(), 16);
        assert!(
            out.chars().all(|c| c.is_ascii_alphanumeric()),
            "randAlphaNum output contains non-alphanumeric: {out:?}"
        );
    }

    #[test]
    fn env_set_and_unset() {
        // Set var path.
        std::env::set_var("YIP_RS_FN_SET", "value-here");
        let out = r(r#"{{ env(name="YIP_RS_FN_SET") }}"#);
        assert_eq!(out, "value-here");

        // Unset var path: env() returns an empty string rather than
        // erroring, matching sprig semantics.
        std::env::remove_var("YIP_RS_FN_UNSET");
        let out = r(r#"{{ env(name="YIP_RS_FN_UNSET") }}"#);
        assert_eq!(out, "");
    }

    #[test]
    fn indent_and_nindent_multiline() {
        let data = json!({"s": "line1\nline2\nline3"});
        // indent prepends pad to the first line and after every \n.
        let ind = render(r#"{{ .s | indent(count=4) }}"#, &data).unwrap();
        assert_eq!(ind, "    line1\n    line2\n    line3");

        // nindent additionally prepends a leading newline.
        let nind = render(r#"{{ .s | nindent(count=2) }}"#, &data).unwrap();
        assert_eq!(nind, "\n  line1\n  line2\n  line3");
    }

    #[test]
    fn repeat_count_zero_one_large() {
        // count = 0 -> empty string.
        assert_eq!(r(r#"{{ "ab" | repeat(count=0) }}"#), "");
        // count = 1 -> unchanged.
        assert_eq!(r(r#"{{ "ab" | repeat(count=1) }}"#), "ab");
        // Larger count: verify length matches and content is correct.
        let out = r(r#"{{ "ab" | repeat(count=100) }}"#);
        assert_eq!(out.len(), 200);
        assert!(out.starts_with("ab"));
        assert!(out.ends_with("ab"));
    }

    #[test]
    fn join_split_roundtrip() {
        // Take an array, join by `,`, split by `,`, and the result
        // should re-join to the same string.
        let data = json!({"xs": ["a", "b", "c", "d"]});
        let joined = render(r#"{{ .xs | join(sep=",") }}"#, &data).unwrap();
        assert_eq!(joined, "a,b,c,d");

        // Now feed the joined string back into split and re-join.
        let data2 = json!({"s": joined});
        let again = render(
            r#"{{ .s | split(sep=",") | join(sep=",") }}"#,
            &data2,
        )
        .unwrap();
        assert_eq!(again, "a,b,c,d");
    }

    // -----------------------------------------------------------------------
    // Regex

    #[test]
    fn regex_match_ipv4() {
        // Simple IPv4 dotted-quad pattern. We don't bother with range checking
        // (0–255) — sprig users get whatever the regex says.
        let pat = r"^\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}$";
        let data = json!({"ip": "192.168.1.42", "pat": pat});
        let ok = render(
            r#"{{ .ip | regexMatch(pattern=.pat) }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(ok, "true");

        let data_bad = json!({"ip": "not-an-ip", "pat": pat});
        let bad = render(
            r#"{{ .ip | regexMatch(pattern=.pat) }}"#,
            &data_bad,
        )
        .unwrap();
        assert_eq!(bad, "false");
    }

    #[test]
    fn regex_match_function_form() {
        // Function form: `regexMatch(pattern=..., str=...)`.
        let data = json!({
            "pat": r"^[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}$",
            "addr": "alice@example.com",
        });
        let out = render(
            r#"{{ regexMatch(pattern=.pat, str=.addr) }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "true");
    }

    #[test]
    fn regex_find_first_email() {
        // regexFind returns the first match (a substring), not a bool.
        let data = json!({
            "pat": r"[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}",
            "doc": "contact me: alice@example.com or bob@x.io",
        });
        let out = render(
            r#"{{ .doc | regexFind(pattern=.pat) }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "alice@example.com");
    }

    #[test]
    fn regex_find_all_versions_with_limit() {
        // Capture all "vMAJOR.MINOR.PATCH" tokens, respecting n.
        let data = json!({
            "pat": r"v\d+\.\d+\.\d+",
            "doc": "released v1.2.3 then v1.2.4 hotfix and v2.0.0 major",
        });
        // n = -1 -> all matches.
        let all = render(
            r#"{{ .doc | regexFindAll(pattern=.pat, n=-1) | join(sep="|") }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(all, "v1.2.3|v1.2.4|v2.0.0");
        // n = 2 -> first two only.
        let two = render(
            r#"{{ .doc | regexFindAll(pattern=.pat, n=2) | join(sep="|") }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(two, "v1.2.3|v1.2.4");
    }

    #[test]
    fn regex_replace_all_strips_digits() {
        let data = json!({"s": "abc123def456", "pat": r"\d+"});
        let out = render(
            r#"{{ .s | regexReplaceAll(pattern=.pat, repl="X") }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "abcXdefX");
    }

    #[test]
    fn regex_split_on_whitespace() {
        let data = json!({"s": "a  b   c    d", "pat": r"\s+"});
        let out = render(
            r#"{{ .s | regexSplit(pattern=.pat, n=-1) | join(sep="|") }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "a|b|c|d");
    }

    #[test]
    fn regex_bad_pattern_errors() {
        // Unbalanced group is invalid — should bubble up as a tera render
        // error rather than panicking or silently matching nothing.
        let data = json!({"s": "anything", "pat": "(unclosed"});
        let res = render(
            r#"{{ .s | regexMatch(pattern=.pat) }}"#,
            &data,
        );
        assert!(res.is_err(), "expected error, got {res:?}");
    }

    // -----------------------------------------------------------------------
    // Semver

    #[test]
    fn semver_parse_basic() {
        // tera quirks:
        //   - No `.field` postfix on function-call results — needs `{% set %}` first.
        //   - Our preprocess strips leading dots only inside `{{ }}`, not inside
        //     `{% set %}`. Pass the version as a literal here; the integration
        //     tests cover the data-driven path.
        let out = render(
            r#"{% set s = semver(version="1.2.3") %}{{ s.major }}"#,
            &json!({}),
        )
        .unwrap();
        assert_eq!(out, "1");
    }

    #[test]
    fn semver_compare_caret() {
        // ^1.2.3 should accept 1.2.5 (same major, >=patch) but reject 2.0.0.
        let data = json!({"v": "1.2.5"});
        let out = render(
            r#"{{ semverCompare(constraint="^1.2.3", version=.v) }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "true");

        let data2 = json!({"v": "2.0.0"});
        let bad = render(
            r#"{{ semverCompare(constraint="^1.2.3", version=.v) }}"#,
            &data2,
        )
        .unwrap();
        assert_eq!(bad, "false");
    }

    #[test]
    fn semver_compare_filter_form() {
        // Filter form: VERSION piped in, constraint as named arg.
        let data = json!({"v": "1.2.5"});
        let out = render(
            r#"{{ .v | semverCompare(constraint=">=1.2.0 <2.0.0") }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "true");
    }

    #[test]
    fn semver_compare_relational_ops() {
        // Run each op against the same anchor version 1.2.3.
        let data = json!({"v": "1.2.3"});
        for (constraint, expected) in [
            (">=1.2.0", "true"),
            (">1.2.3", "false"),
            ("<=1.2.3", "true"),
            ("<1.2.3", "false"),
            ("==1.2.3", "true"),
            ("!=1.2.3", "false"),
            ("~1.2.0", "true"),  // tilde allows patch range
            ("~1.1.0", "false"), // wrong minor
        ] {
            let tmpl = format!(
                r#"{{{{ .v | semverCompare(constraint="{constraint}") }}}}"#
            );
            let got = render(&tmpl, &data).unwrap();
            assert_eq!(got, expected, "constraint `{constraint}`");
        }
    }

    #[test]
    fn semver_compare_v_prefix() {
        // Leading "v" should be accepted on both sides per sprig/Go convention.
        let data = json!({"v": "v1.2.5"});
        let out = render(
            r#"{{ semverCompare(constraint="^v1.2.3", version=.v) }}"#,
            &data,
        )
        .unwrap();
        assert_eq!(out, "true");
    }

    #[test]
    fn semver_parse_invalid_returns_null() {
        // sprig returns nil/null for unparseable versions; we surface
        // Value::Null which tera renders as an empty string.
        let data = json!({"v": "not.a.version"});
        let out = render(r#"{{ semver(version=.v) }}"#, &data).unwrap();
        assert_eq!(out, "");
    }
}
