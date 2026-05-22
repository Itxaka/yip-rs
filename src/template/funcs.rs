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
use chrono::Utc;
use md5::Md5;
use rand::distributions::Alphanumeric;
use rand::Rng;
use serde_json::{json, Map, Value};
// `sha2::Digest` is the same `digest::Digest` trait md-5 re-exports; importing
// it once here makes `.update()` / `.finalize()` resolve for all three hashers.
use sha2::{Digest as _, Sha256, Sha512};
use tera::{Error as TeraError, Result as TeraResult, Tera};

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
//   Date:      now [fn], date, dateInZone
//   OS:        env [fn], expandenv [fn + filter], tempDir [fn]
//
// TODO(sprig): not yet ported (add when a kairos config needs them):
//   - regex* family (regexMatch, regexFind, regexFindAll, regexReplaceAll, regexSplit)
//   - semver family (semver, semverCompare)
//   - htpasswd / bcrypt / derivePassword / genPrivateKey
//   - dateModify / mustDateModify / unixEpoch
//   - dig / get / mustGet (deep map access — tera already handles `.`)
//   - uuidv4 (we already inject `Random` in sysdata)
//   - mergeOverwrite / merge / deepCopy
//   - toToml / fromToml
//   - kindOf / typeOf / kindIs / typeIs

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
    #[ignore = "tera's built-in `default` filter shadows our sprig variant — \
                only triggers on undefined values, not empty strings. Revisit \
                when porting a yip config that depends on sprig's behaviour."]
    fn default_filter_empty_string() {
        let data = json!({"name": ""});
        let out = render(r#"{{ .name | default(value="anon") }}"#, &data).unwrap();
        assert_eq!(out, "anon");
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
}
