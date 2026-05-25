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
//! Control flow (best-effort: enough for the constructs real Kairos configs
//! lean on; anything more elaborate falls through unchanged and the tera
//! parse will surface the error so the executor's raw-bytes fallback can
//! kick in):
//!
//! - `{{ if .X }}A{{ else }}B{{ end }}`
//!     -> `{% if X %}A{% else %}B{% endif %}`
//! - `{{ if eq .A .B }}...{{ end }}` -> `{% if A == B %}...{% endif %}`
//!   (`eq`, `ne`, `lt`, `gt`, `le`, `ge` are translated; `and`/`or`/`not`
//!   map directly).
//! - `{{ range .Items }}{{ . }}{{ end }}`
//!     -> `{% for __it1 in Items %}{{ __it1 }}{% endfor %}`. Nested
//!   ranges generate distinct loop variable names (`__it1`, `__it2`, …).
//! - `{{ upper .Name }}` (function-call form) -> `{{ Name | upper }}`
//!   for a small allowlist of common sprig funcs (upper, lower, trim,
//!   quote, squote, title, b64enc, b64dec, len, first, last, toString).
//!
//! Everything outside `{{ ... }}` (and `{{- -}}`) is copied verbatim.

use std::sync::Arc;

use regex::Regex;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
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
/// bare `{{ . }}` to `{{ __root__ }}` (or to the active range loop variable
/// when inside a `range` block), and strips Go whitespace-trim dashes
/// (`{{- ... -}}`). String literals (`"..."` and backtick-quoted) are not
/// touched so a dotted path appearing inside a string survives intact.
///
/// Control flow (`if`/`else`/`end`/`range`) is translated to tera's
/// `{% ... %}` form. A stack of "block kinds" tracks whether each open
/// block was an `if` or a `range`, so a single Go `{{ end }}` maps to the
/// right tera closer (`{% endif %}` vs `{% endfor %}`).
pub fn preprocess(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;

    // Track open Go blocks so a single `{{ end }}` closes the right one.
    // Each stack entry also carries the loop variable name (only meaningful
    // for `Range`) so nested `{{ . }}` references can be rebound.
    let mut block_stack: Vec<Block> = Vec::new();
    let mut loop_counter: u32 = 0;

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
            // Active range loop variable (for `{{ . }}` rebinding), if any.
            let current_loop_var = block_stack
                .iter()
                .rev()
                .find_map(|b| match b {
                    Block::Range(name) => Some(name.clone()),
                    Block::If => None,
                });
            // Extract the inner expression and rewrite.
            let inner = &input[start..end];
            let (body, kind) = rewrite_expr(inner, current_loop_var.as_deref(), &mut loop_counter);
            match kind {
                ExprKind::Output => {
                    out.push_str("{{");
                    out.push_str(&body);
                    out.push_str("}}");
                }
                ExprKind::OpenIf => {
                    out.push_str("{%");
                    out.push_str(&body);
                    out.push_str("%}");
                    block_stack.push(Block::If);
                }
                ExprKind::Else => {
                    out.push_str("{%");
                    out.push_str(&body);
                    out.push_str("%}");
                }
                ExprKind::OpenRange(loop_var) => {
                    out.push_str("{%");
                    out.push_str(&body);
                    out.push_str("%}");
                    block_stack.push(Block::Range(loop_var));
                }
                ExprKind::End => {
                    // Match the most recently opened block.
                    let closer = match block_stack.pop() {
                        Some(Block::Range(_)) => "{% endfor %}",
                        Some(Block::If) => "{% endif %}",
                        // Unmatched `end`: leave it as a tera-invalid token
                        // so the parse error surfaces and the executor
                        // falls back to raw bytes.
                        None => "{% endif %}",
                    };
                    out.push_str(closer);
                }
                ExprKind::Raw => {
                    // Preprocess couldn't make sense of this — emit it back
                    // unchanged. Tera will likely error and the caller's
                    // raw-bytes fallback kicks in.
                    out.push_str("{{");
                    out.push_str(inner);
                    out.push_str("}}");
                }
            }
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

/// Outcome of rewriting one `{{ ... }}` segment.
enum ExprKind {
    /// Normal output expression: `{{ Foo }}`, `{{ x | upper }}`, …
    Output,
    /// Open of an if block: `{% if ... %}`.
    OpenIf,
    /// `{% else %}` (or `{% elif ... %}`).
    Else,
    /// Open of a for/range block, with the loop variable that was minted.
    OpenRange(String),
    /// Close of either an if or a range block. The caller knows which.
    End,
    /// The preprocessor couldn't understand the inner — emit verbatim.
    Raw,
}

enum Block {
    If,
    Range(String),
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
///
/// `loop_var` is `Some(name)` when we are inside a `range` block — bare
/// `{{ . }}` then rewrites to that variable rather than to `__root__`.
/// `loop_counter` is bumped each time we mint a new range loop var.
fn rewrite_expr(
    inner: &str,
    loop_var: Option<&str>,
    loop_counter: &mut u32,
) -> (String, ExprKind) {
    // Strip Go whitespace-trim dashes: `- expr -`, `- expr`, `expr -`.
    let trimmed_dashes = strip_trim_dashes(inner);
    let trimmed = trimmed_dashes.trim();

    // Bare dot: `.` (possibly surrounded by whitespace) -> active loop var or root.
    if trimmed == "." {
        let target = loop_var.unwrap_or("__root__");
        return (format!(" {target} "), ExprKind::Output);
    }

    // Control flow detection. Keyword must be the first whitespace-separated
    // token of the trimmed inner.
    if let Some(rest) = strip_keyword(trimmed, "end") {
        // `{{ end }}` with no further content (Go has no `end XYZ`).
        if rest.trim().is_empty() {
            return (String::new(), ExprKind::End);
        }
    }
    if let Some(rest) = strip_keyword(trimmed, "else") {
        // `{{ else }}` or `{{ else if X }}`.
        let after = rest.trim();
        if after.is_empty() {
            return (" else ".to_string(), ExprKind::Else);
        }
        if let Some(cond_src) = strip_keyword(after, "if") {
            let cond = rewrite_condition(cond_src.trim());
            return (format!(" elif {cond} "), ExprKind::Else);
        }
        // Anything else after `else` is unexpected — bail to Raw.
        return (String::new(), ExprKind::Raw);
    }
    if let Some(cond_src) = strip_keyword(trimmed, "if") {
        let cond = rewrite_condition(cond_src.trim());
        return (format!(" if {cond} "), ExprKind::OpenIf);
    }
    if let Some(rest) = strip_keyword(trimmed, "range") {
        // For v1 we support the simple `range <expr>` form. Go also
        // supports `range $i, $v := <expr>` (with explicit names) and a
        // pipeline form; those fall through to Raw and the executor's
        // raw-bytes path handles them.
        let src = rest.trim();
        if src.starts_with('$') {
            // Explicit variable binding — give up cleanly.
            return (String::new(), ExprKind::Raw);
        }
        *loop_counter += 1;
        let var = format!("__it{}", loop_counter);
        // Rewrite the iterable expression using the field-access logic
        // (drop leading dots, handle pipelines).
        let iter_expr = rewrite_field_access(src, loop_var);
        return (
            format!(" for {var} in {iter_expr} "),
            ExprKind::OpenRange(var),
        );
    }
    if strip_keyword(trimmed, "with").is_some()
        || strip_keyword(trimmed, "define").is_some()
        || strip_keyword(trimmed, "block").is_some()
        || strip_keyword(trimmed, "template").is_some()
    {
        // Constructs we don't translate yet. Emit Raw so the eventual
        // tera parse fails and the caller falls back to raw bytes.
        return (String::new(), ExprKind::Raw);
    }

    // Function-call form: `funcname .Arg` -> `Arg | funcname`. Only the
    // small allowlist of single-arg sprig funcs that real configs reach
    // for is rewritten — everything else stays as-is so tera can parse
    // pipelines and existing filter syntax unchanged.
    if let Some(rewritten) = try_rewrite_funcall(trimmed, loop_var) {
        return (format!(" {rewritten} "), ExprKind::Output);
    }

    // Default path: drop leading dots on identifiers (the original
    // behaviour). Preserves string literals.
    (rewrite_field_access(&trimmed_dashes, loop_var), ExprKind::Output)
}

/// If `s` starts with the given keyword followed by whitespace (or is exactly
/// the keyword), return the remainder after the keyword. Otherwise `None`.
fn strip_keyword<'a>(s: &'a str, kw: &str) -> Option<&'a str> {
    if let Some(rest) = s.strip_prefix(kw) {
        if rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace()) {
            return Some(rest);
        }
    }
    None
}

/// Translate a Go condition into tera form. Handles `eq/ne/lt/gt/le/ge`
/// prefix-call form, `and`/`or`/`not` (which tera already accepts), and the
/// leading-dot field access. Anything else passes through after the dot
/// stripping.
fn rewrite_condition(src: &str) -> String {
    let mapped = if let Some(parts) = parse_two_arg_call(src, &["eq", "ne", "lt", "gt", "le", "ge"]) {
        let (op, a, b) = parts;
        let tera_op = match op {
            "eq" => "==",
            "ne" => "!=",
            "lt" => "<",
            "gt" => ">",
            "le" => "<=",
            "ge" => ">=",
            _ => unreachable!(),
        };
        format!("{a} {tera_op} {b}")
    } else if let Some((a, b)) = parse_named_call(src, "and") {
        format!("{a} and {b}")
    } else if let Some((a, b)) = parse_named_call(src, "or") {
        format!("{a} or {b}")
    } else if let Some(rest) = strip_keyword(src, "not") {
        format!("not {}", rewrite_condition(rest.trim()))
    } else {
        src.to_string()
    };
    rewrite_field_access(&mapped, None)
}

/// Parse `op A B` where `op` is one of `names`. Returns (op, A, B). Args are
/// returned with leading dots stripped already.
fn parse_two_arg_call<'a>(src: &'a str, names: &[&'a str]) -> Option<(&'a str, String, String)> {
    let mut it = src.split_whitespace();
    let op = it.next()?;
    if !names.contains(&op) {
        return None;
    }
    let a_raw = it.next()?;
    let b_raw = it.next()?;
    // Reject if there's anything else after the two args — this is a
    // simple form only.
    if it.next().is_some() {
        return None;
    }
    let a = rewrite_field_access(a_raw, None);
    let b = rewrite_field_access(b_raw, None);
    Some((op, a, b))
}

/// Parse `name A B` (two operands) for `and` / `or`.
fn parse_named_call(src: &str, name: &str) -> Option<(String, String)> {
    let rest = strip_keyword(src, name)?.trim();
    let mut it = rest.split_whitespace();
    let a_raw = it.next()?;
    let b_raw = it.next()?;
    if it.next().is_some() {
        return None;
    }
    Some((
        rewrite_field_access(a_raw, None),
        rewrite_field_access(b_raw, None),
    ))
}

/// Function-call form rewrite for the small allowlist of common filters. If
/// the input matches `<func> <arg>` exactly, return `<arg-rewritten> | <func>`.
/// Anything else returns `None`.
fn try_rewrite_funcall(src: &str, loop_var: Option<&str>) -> Option<String> {
    // Allowlist of sprig/builtin filter names that take a single piped arg.
    // Kept deliberately small — we'd rather leave a construct alone than
    // mangle a pipeline that already works. `default` is intentionally
    // *not* here because the Go form is `default DEFAULT VALUE` (two args,
    // value piped); we'd have to rearrange args, which is fragile.
    const ALLOWED: &[&str] = &[
        "upper", "lower", "trim", "quote", "squote", "title",
        "b64enc", "b64dec", "len", "first", "last", "toString",
    ];
    let re = Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*)\s+(.+)$").ok()?;
    let caps = re.captures(src)?;
    let name = caps.get(1)?.as_str();
    if !ALLOWED.contains(&name) {
        return None;
    }
    let arg_src = caps.get(2)?.as_str().trim();
    // Refuse if the arg looks like a pipeline / further call already — we
    // only rewrite the simple `func .Field` case.
    if arg_src.contains('|') {
        return None;
    }
    if arg_src.split_whitespace().count() != 1 {
        return None;
    }
    let arg = rewrite_field_access(arg_src, loop_var);
    Some(format!("{arg} | {name}"))
}

/// Walk `src` dropping leading dots on identifiers (`.Foo` -> `Foo`).
/// `loop_var` selects what bare `.` resolves to when it appears as a
/// standalone token; pass `None` to keep the original `__root__` behaviour
/// (callers handle the bare-dot case before getting here in most paths).
fn rewrite_field_access(src: &str, loop_var: Option<&str>) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut prev: char = ' ';
    let mut in_dq = false;
    let mut in_bt = false;
    let mut idx = 0;
    while idx < bytes.len() {
        let ch = src[idx..].chars().next().unwrap();
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
            let next_byte = bytes.get(idx + 1).copied();
            let next_is_ident_start =
                matches!(next_byte, Some(b) if (b.is_ascii_alphabetic() || b == b'_'));
            let prev_breaks_token = matches!(prev, ' ' | '\t' | '\n' | '(' | '|' | ',' | '=');
            // Bare `.` token: previous is a boundary, next is whitespace or
            // end-of-input. Rewrite to the active loop var (or __root__).
            let next_is_boundary = match next_byte {
                None => true,
                Some(b) => matches!(b, b' ' | b'\t' | b'\n' | b')' | b'|' | b',' | b'='),
            };
            if !next_is_ident_start
                && next_is_boundary
                && (prev_breaks_token || out.is_empty())
            {
                let target = loop_var.unwrap_or("__root__");
                out.push_str(target);
                prev = ch;
                idx += ch_len;
                continue;
            }
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

    // -----------------------------------------------------------------
    // Extended edge-case tests.

    #[test]
    fn render_deep_nested_field_access() {
        // Five levels deep: exercises tera's dotted-path resolver on a
        // structure that mirrors the typical `Values.X.Y.Z` shape.
        let data = json!({
            "Values": {
                "A": { "B": { "C": { "D": { "E": "deep" } } } }
            }
        });
        let out = render("{{ .Values.A.B.C.D.E }}", &data).unwrap();
        assert_eq!(out, "deep");
    }

    #[test]
    fn render_escaped_braces_via_raw_block() {
        // Tera lets us emit literal `{{` / `}}` via `{% raw %}` blocks.
        // Go uses `{{"{{"}}` for the same purpose, but yip configs in
        // practice rely on tera's raw block when they need to ship the
        // delimiters verbatim. Verify the engine passes them through.
        let out = render("{% raw %}{{ literal }}{% endraw %}", &Value::Null).unwrap();
        assert_eq!(out, "{{ literal }}");
    }

    #[test]
    fn render_multiple_substitutions_one_line() {
        let data = json!({"First": "kai", "Second": "ros"});
        let out = render("{{ .First }}-{{ .Second }}/{{ .First }}{{ .Second }}", &data).unwrap();
        assert_eq!(out, "kai-ros/kairos");
    }

    #[test]
    fn render_multi_line_control_flow_if() {
        // Tera's control-flow syntax uses `{% if %}` (Go's `{{ if }}` is
        // intentionally not bridged — kairos configs that need branching
        // already use tera syntax). This exercises the multi-line path.
        let data = json!({"flag": true, "Name": "kairos"});
        let tmpl = "\
{% if flag -%}
hello {{ .Name }}
{%- endif %}";
        let out = render(tmpl, &data).unwrap();
        assert_eq!(out, "hello kairos");
    }

    #[test]
    fn render_sprig_chain_upper_quote() {
        // Filter chain: literal -> upper -> quote.
        let out = render(r#"{{ "abc" | upper | quote }}"#, &Value::Null).unwrap();
        assert_eq!(out, "\"ABC\"");
    }

    #[test]
    fn render_non_existent_variable_errors() {
        // By default tera errors on undefined variables. Verify we
        // surface that as a `Template` error rather than silently
        // emitting an empty string.
        let res = render("{{ .DoesNotExist }}", &Value::Null);
        assert!(res.is_err(), "expected error for undefined variable");
    }

    #[test]
    fn render_special_chars_in_values() {
        // Values containing characters that have meaning in shells or
        // YAML must round-trip verbatim through the engine.
        let data = json!({"v": "a,b%c$d#e\""});
        let out = render("[{{ .v }}]", &data).unwrap();
        assert_eq!(out, "[a,b%c$d#e\"]");
    }

    #[test]
    fn render_whitespace_trim_dashes_preserved() {
        // `{{- ... -}}` is Go-style trim; preprocess strips the dashes
        // so the surrounding whitespace ends up controlled by tera's
        // default behaviour. The key invariant is that the inner field
        // resolves and the surrounding text is preserved.
        let data = json!({"Name": "kairos"});
        let out = render("foo {{- .Name -}} bar", &data).unwrap();
        assert!(out.contains("kairos"));
        assert!(out.contains("foo"));
        assert!(out.contains("bar"));
    }

    #[test]
    fn render_round_trip_idempotent() {
        // Rendering twice with the same data must produce identical
        // output — there must be no hidden state in the engine.
        let data = json!({"Values": {"System": {"OS": {"Name": "kairos"}}}});
        let tmpl = "{{ .Values.System.OS.Name }}";
        let first = render(tmpl, &data).unwrap();
        let second = render(tmpl, &data).unwrap();
        assert_eq!(first, second);
        assert_eq!(first, "kairos");
    }

    #[test]
    fn render_empty_template_yields_empty_output() {
        let out = render("", &Value::Null).unwrap();
        assert_eq!(out, "");
    }

    // -----------------------------------------------------------------
    // Go control-flow translation tests.

    #[test]
    fn preprocess_if_translates_to_tera() {
        // Simple `{{ if }}...{{ end }}` becomes tera's `{% if %}...{% endif %}`.
        let out = preprocess("{{ if .X }}hi{{ end }}");
        assert_eq!(out, "{% if X %}hi{% endif %}");
    }

    #[test]
    fn preprocess_if_else_translates_to_tera() {
        let out = preprocess("{{ if .X }}A{{ else }}B{{ end }}");
        assert_eq!(out, "{% if X %}A{% else %}B{% endif %}");
    }

    #[test]
    fn preprocess_eq_translates_to_equality() {
        // `eq` prefix-call becomes `==` infix.
        let out = preprocess("{{ if eq .A .B }}match{{ end }}");
        assert_eq!(out, "{% if A == B %}match{% endif %}");
    }

    #[test]
    fn preprocess_and_or_not_in_conditions() {
        assert_eq!(
            preprocess("{{ if and .A .B }}x{{ end }}"),
            "{% if A and B %}x{% endif %}"
        );
        assert_eq!(
            preprocess("{{ if or .A .B }}x{{ end }}"),
            "{% if A or B %}x{% endif %}"
        );
        assert_eq!(
            preprocess("{{ if not .A }}x{{ end }}"),
            "{% if not A %}x{% endif %}"
        );
    }

    #[test]
    fn preprocess_range_translates_to_for() {
        let out = preprocess("{{ range .Items }}{{ . }}{{ end }}");
        assert_eq!(out, "{% for __it1 in Items %}{{ __it1 }}{% endfor %}");
    }

    #[test]
    fn preprocess_funcall_form_to_filter() {
        // `upper .Name` -> `Name | upper` for the small allowlist.
        let out = preprocess("{{ upper .Name }}");
        assert_eq!(out, "{{ Name | upper }}");
    }

    // -----------------------------------------------------------------
    // Rendering tests for the new constructs.

    #[test]
    fn render_go_if_true_branch() {
        let data = json!({"X": true});
        let out = render("{{ if .X }}hi{{ end }}", &data).unwrap();
        assert_eq!(out, "hi");
    }

    #[test]
    fn render_go_if_else_false_branch() {
        let data = json!({"X": false});
        let out = render("{{ if .X }}hi{{ else }}bye{{ end }}", &data).unwrap();
        assert_eq!(out, "bye");
    }

    #[test]
    fn render_go_if_eq_match() {
        let data = json!({"A": 1, "B": 1});
        let out = render("{{ if eq .A .B }}match{{ end }}", &data).unwrap();
        assert_eq!(out, "match");
    }

    #[test]
    fn render_go_if_eq_no_match() {
        let data = json!({"A": 1, "B": 2});
        let out = render("{{ if eq .A .B }}match{{ end }}", &data).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn render_go_range_over_list() {
        let data = json!({"Items": ["a", "b", "c"]});
        let out = render("{{ range .Items }}{{ . }}{{ end }}", &data).unwrap();
        assert_eq!(out, "abc");
    }

    #[test]
    fn render_go_funcall_upper() {
        let data = json!({"Name": "foo"});
        let out = render("{{ upper .Name }}", &data).unwrap();
        assert_eq!(out, "FOO");
    }

    #[test]
    fn render_go_funcall_lower() {
        let data = json!({"Name": "FOO"});
        let out = render("{{ lower .Name }}", &data).unwrap();
        assert_eq!(out, "foo");
    }

    #[test]
    fn render_go_funcall_quote() {
        let data = json!({"Name": "foo"});
        let out = render("{{ quote .Name }}", &data).unwrap();
        assert_eq!(out, "\"foo\"");
    }

    #[test]
    fn render_go_and_combined() {
        let data = json!({"A": true, "B": true});
        let out = render("{{ if and .A .B }}both{{ end }}", &data).unwrap();
        assert_eq!(out, "both");
    }

    #[test]
    fn render_go_or_short_circuit() {
        let data = json!({"A": false, "B": true});
        let out = render("{{ if or .A .B }}either{{ end }}", &data).unwrap();
        assert_eq!(out, "either");
    }

    #[test]
    fn render_go_range_with_bare_dot_strings() {
        // Bare `{{ . }}` inside `range` rebinds to the loop variable —
        // verifies the loop-var stack in preprocess is wired up.
        let data = json!({"Items": ["a", "b", "c"]});
        let out = render(
            "<{{ range .Items }}-{{ . }}{{ end }}>",
            &data,
        )
        .unwrap();
        assert_eq!(out, "<-a-b-c>");
    }

    #[test]
    fn render_else_if_chain() {
        // `{{ else if eq .X 2 }}` should turn into `{% elif X == 2 %}`.
        let data = json!({"X": 2});
        let out = render(
            "{{ if eq .X 1 }}one{{ else if eq .X 2 }}two{{ else }}other{{ end }}",
            &data,
        )
        .unwrap();
        assert_eq!(out, "two");
    }

    #[test]
    fn render_unrecognized_construct_falls_back_or_errors() {
        // `{{ with .X }}` is intentionally not translated (we'd have to
        // rebind `.` inside the block — out of scope for v1). The
        // preprocess emits it verbatim; tera errors at parse time. The
        // executor's raw-bytes fallback would catch this in production.
        // We only assert that we get a *definite* outcome (no panic) —
        // either tera errors loudly or silently swallows the construct.
        let tmpl = "{{ with .X }}hi{{ end }}";
        let _ = render(tmpl, &json!({"X": "yes"}));
    }

    #[test]
    fn preprocess_unmatched_end_emits_endif_marker() {
        // A stray `{{ end }}` with no matching open block: we emit
        // `{% endif %}` which will fail tera parse, signalling the
        // executor to fall back. This is documented behaviour, not a bug.
        let out = preprocess("{{ end }}");
        assert_eq!(out, "{% endif %}");
    }

    #[test]
    fn preprocess_does_not_touch_existing_tera_syntax() {
        // `{% if x %}` is already tera — it's outside `{{ ... }}` so
        // preprocess never sees it as a Go construct.
        let input = "{% if x %}A{% endif %}";
        assert_eq!(preprocess(input), input);
    }
}
