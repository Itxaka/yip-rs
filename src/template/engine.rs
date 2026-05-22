//! Tera-based renderer with a Go `text/template` compatibility preprocessor.
//!
//! The yip Go codebase renders config blobs with `text/template` + sprig.
//! Go's templating syntax uses `{{ .Field }}` for field access and the
//! special `{{ . }}` for the root value. Tera uses `{{ Field }}` without
//! a leading dot. [`preprocess`] rewrites Go-style expressions into the
//! tera dialect so a kairos cloud-init file written for yip Go renders
//! identically here.
//!
//! Rules applied inside every `{{ ... }}` segment (string literals are
//! preserved verbatim):
//!
//! - `{{ . }}`             -> `{{ __root__ }}` (root value, injected from `data`)
//! - `{{ .Foo.Bar }}`      -> `{{ Foo.Bar }}` (strip the leading dot)
//! - `{{ x | foo }}`       -> unchanged (tera supports pipe syntax)
//! - `{{- ... -}}`         -> tera's `{%- ... -%}` whitespace control is
//!   different; we strip the dashes since yip configs rarely use them.
//!
//! Everything outside `{{ ... }}` (and `{{- -}}`) is copied verbatim.

use std::sync::Arc;

use serde_json::{json, Value};
use tera::{Context, Tera};

use crate::error::{Error, Result};

use super::funcs;
use super::sysdata;

/// Render a Go-template-style string against the supplied JSON data blob.
///
/// `data` is exposed as the template root: nested fields are reachable
/// as `{{ Foo.Bar }}` (Go's `{{ .Foo.Bar }}` is accepted via preprocessing).
/// The bare `{{ . }}` form yields the JSON root itself, matching how
/// `utils.TemplatedString("foo-{{.}}", "bar")` renders to `foo-bar` in yip.
pub fn render(template: &str, data: &Value) -> Result<String> {
    let mut tera = Tera::default();
    register_sprig(&mut tera);

    let rewritten = preprocess(template);

    let mut ctx = Context::new();
    // Populate top-level fields from the JSON object so `{{ Foo }}` works
    // for Go-style `{{ .Foo }}` after preprocessing.
    if let Some(obj) = data.as_object() {
        for (k, v) in obj {
            ctx.insert(k, v);
        }
    }
    // Bare `{{ . }}` -> `{{ __root__ }}`; always inject the raw value.
    ctx.insert("__root__", data);

    tera.render_str(&rewritten, &ctx)
        .map_err(|e| Error::Template(format_tera_error(&e)))
}

/// Render with `gather_sysdata()` providing the data. Equivalent to yip's
/// `templateSysData`.
pub fn render_with_sysdata(template: &str) -> Result<String> {
    let data = sysdata::gather_sysdata()?;
    render(template, &data)
}

/// Bridge Go `text/template` syntax to tera. Public for testing.
///
/// Walks the input, copying everything outside `{{ ... }}` verbatim. Inside
/// a delimited expression, drops leading dots on identifiers, rewrites the
/// bare `{{ . }}` to `{{ __root__ }}`, and strips Go whitespace-trim dashes
/// (`{{- ... -}}`). String literals (`"..."` and backtick-quoted) are not
/// touched so a dotted path appearing inside a string survives intact.
pub fn preprocess(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    while i < bytes.len() {
        // Look for the next `{{` delimiter.
        if i + 1 < bytes.len() && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            // Find matching `}}`. If none, copy the rest and bail — tera
            // will surface the parse error.
            let start = i + 2;
            let Some(end) = find_close(bytes, start) else {
                out.push_str(&input[i..]);
                return out;
            };
            // Extract the inner expression and rewrite.
            let inner = &input[start..end];
            let rewritten = rewrite_expr(inner);
            out.push_str("{{");
            out.push_str(&rewritten);
            out.push_str("}}");
            i = end + 2;
            continue;
        }
        // Pass other bytes through. UTF-8 safe: we only branch on ASCII `{`.
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn find_close(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    let mut in_dq = false;
    let mut in_bt = false;
    while i + 1 < bytes.len() {
        let b = bytes[i];
        if !in_bt && b == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            in_dq = !in_dq;
        } else if !in_dq && b == b'`' {
            in_bt = !in_bt;
        } else if !in_dq && !in_bt && b == b'}' && bytes[i + 1] == b'}' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Rewrite a single `{{ ... }}` inner expression from Go style to tera.
fn rewrite_expr(inner: &str) -> String {
    // Strip Go whitespace-trim dashes: `- expr -`, `- expr`, `expr -`.
    let trimmed_dashes = strip_trim_dashes(inner);
    let trimmed = trimmed_dashes.trim();

    // Bare dot: `.` (possibly surrounded by whitespace) -> `__root__`.
    if trimmed == "." {
        return " __root__ ".to_string();
    }

    // Walk and drop `.` only when it begins an identifier (preceded by
    // whitespace, `(`, `|`, `,`, or start-of-expr, and followed by an
    // ASCII letter or underscore). Preserve string literals.
    let bytes = trimmed_dashes.as_bytes();
    let mut out = String::with_capacity(trimmed_dashes.len());
    let mut prev: char = ' ';
    let mut in_dq = false;
    let mut in_bt = false;
    let mut idx = 0;
    while idx < bytes.len() {
        let ch = trimmed_dashes[idx..].chars().next().unwrap();
        let ch_len = ch.len_utf8();

        if in_dq {
            out.push(ch);
            if ch == '"' && (idx == 0 || bytes[idx - 1] != b'\\') {
                in_dq = false;
            }
            prev = ch;
            idx += ch_len;
            continue;
        }
        if in_bt {
            out.push(ch);
            if ch == '`' {
                in_bt = false;
            }
            prev = ch;
            idx += ch_len;
            continue;
        }

        if ch == '"' {
            in_dq = true;
            out.push(ch);
            prev = ch;
            idx += ch_len;
            continue;
        }
        if ch == '`' {
            in_bt = true;
            out.push(ch);
            prev = ch;
            idx += ch_len;
            continue;
        }

        if ch == '.' {
            // Look ahead: is this a leading dot before an identifier?
            let next_byte = bytes.get(idx + 1).copied();
            let next_is_ident_start = matches!(next_byte, Some(b) if (b.is_ascii_alphabetic() || b == b'_'));
            let prev_breaks_token = matches!(prev, ' ' | '\t' | '\n' | '(' | '|' | ',' | '=');
            if next_is_ident_start && (prev_breaks_token || out.is_empty()) {
                // Skip the dot — `.Foo` becomes `Foo`.
                prev = ch;
                idx += ch_len;
                continue;
            }
        }

        out.push(ch);
        prev = ch;
        idx += ch_len;
    }

    out
}

fn strip_trim_dashes(inner: &str) -> String {
    let mut s = inner.to_string();
    // Leading `- ` -> ` `.
    if let Some(rest) = s.strip_prefix('-') {
        s = rest.to_string();
    }
    // Trailing ` -` -> ` `.
    if let Some(rest) = s.strip_suffix('-') {
        s = rest.to_string();
    }
    s
}

/// Format a tera::Error preserving the chain of `source` messages, since
/// the top-level Display often elides the actual cause.
fn format_tera_error(e: &tera::Error) -> String {
    use std::error::Error as _;
    let mut s = format!("{e}");
    let mut src = e.source();
    while let Some(cause) = src {
        s.push_str(": ");
        s.push_str(&format!("{cause}"));
        src = cause.source();
    }
    s
}

fn register_sprig(tera: &mut Tera) {
    funcs::register_all(tera);
}

/// `Arc` wrapper so embedding callers can share a configured engine.
/// Currently unused but matches the architecture used by other modules.
#[allow(dead_code)]
pub fn shared_engine() -> Arc<Tera> {
    let mut t = Tera::default();
    register_sprig(&mut t);
    Arc::new(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preprocess_strips_leading_dots() {
        assert_eq!(preprocess("{{ .Foo }}"), "{{ Foo }}");
        assert_eq!(preprocess("{{ .Foo.Bar }}"), "{{ Foo.Bar }}");
        assert_eq!(
            preprocess("{{ .Values.System.OS.Name }}"),
            "{{ Values.System.OS.Name }}"
        );
    }

    #[test]
    fn preprocess_bare_dot_becomes_root() {
        assert_eq!(preprocess("foo-{{.}}"), "foo-{{ __root__ }}");
        assert_eq!(preprocess("{{ . }}"), "{{ __root__ }}");
    }

    #[test]
    fn preprocess_preserves_literal_strings() {
        // The dot inside the quoted literal must NOT be stripped.
        let out = preprocess(r#"{{ ".Foo" | upper }}"#);
        assert!(out.contains(r#"".Foo""#), "got {out}");
    }

    #[test]
    fn preprocess_pipeline_filter() {
        assert_eq!(preprocess(r#"{{ "x" | upper }}"#), r#"{{ "x" | upper }}"#);
    }

    #[test]
    fn preprocess_outside_braces_unchanged() {
        assert_eq!(preprocess("hello .world"), "hello .world");
    }

    #[test]
    fn preprocess_strips_trim_dashes() {
        assert_eq!(preprocess("{{- .Foo -}}"), "{{ Foo }}");
    }

    #[test]
    fn render_bare_dot_root() {
        let out = render("foo-{{.}}", &Value::String("bar".to_string())).unwrap();
        assert_eq!(out, "foo-bar");
    }

    #[test]
    fn render_field_access_go_style() {
        let data = json!({
            "Values": {
                "System": {
                    "OS": { "Name": "kairos" }
                }
            }
        });
        let out = render("{{ .Values.System.OS.Name }}", &data).unwrap();
        assert_eq!(out, "kairos");
    }

    #[test]
    fn render_string_upper_filter() {
        let out = render(r#"{{ "x" | upper }}"#, &Value::Null).unwrap();
        assert_eq!(out, "X");
    }

    #[test]
    fn render_empty_template() {
        let out = render("plain text", &Value::Null).unwrap();
        assert_eq!(out, "plain text");
    }

    #[test]
    fn render_field_with_filter() {
        let data = json!({"Name": "Kairos"});
        let out = render("{{ .Name | lower }}", &data).unwrap();
        assert_eq!(out, "kairos");
    }
}
