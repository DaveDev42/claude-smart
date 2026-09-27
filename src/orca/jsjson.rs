//! `JSON.stringify`, byte for byte, over a `serde_json::Value` parsed with
//! `preserve_order` + `arbitrary_precision`.
//!
//! Orca writes every JSON file csm also writes (`.claude.json`,
//! `oauth-account.json`, the system-default snapshot, a refreshed credential)
//! as `JSON.stringify(value)` or `JSON.stringify(value, null, 2) + "\n"` of a
//! value it got from `JSON.parse`. serde_json differs from that in two
//! places, both handled here:
//! - key order: a JS object lists integer-like keys (canonical array indices
//!   below 2^32 - 1) first, ascending, then the other keys in insertion
//!   order;
//! - numbers: JS holds a double and prints it with `Number.prototype.toString`
//!   (`1.0` is `1`, `1e21` stays exponent form as `1e+21`, `1e-7` is
//!   `1e-7`, `-0` is `0`, and a literal too big for a double is `null`),
//!   while `arbitrary_precision` keeps the source text.
//!
//! String escaping is the same in both: `"` and `\` escaped, control
//! characters as `\b \f \n \r \t` or `\u00xx` (lowercase hex), everything
//! else verbatim.

use serde_json::{Number, Value};

/// `JSON.stringify(v)`.
pub fn stringify(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, None, 0, &mut out);
    out
}

/// `JSON.stringify(v, null, 2)`.
pub fn stringify_pretty(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, Some(2), 0, &mut out);
    out
}

/// Orca's writeJson text: `JSON.stringify(v, null, 2) + "\n"`.
pub fn write_json_text(v: &Value) -> String {
    let mut s = stringify_pretty(v);
    s.push('\n');
    s
}

/// A canonical array index (`"0"`, `"17"`, never `"01"`), below 2^32 - 1.
pub fn is_array_index(k: &str) -> bool {
    if k.is_empty() || (k.len() > 1 && k.starts_with('0')) {
        return false;
    }
    if !k.bytes().all(|b| b.is_ascii_digit()) || k.len() > 10 {
        return false;
    }
    k.parse::<u64>().is_ok_and(|n| n < u32::MAX as u64)
}

/// The keys of `m` in JS property order.
fn js_key_order(m: &serde_json::Map<String, Value>) -> Vec<&String> {
    let mut ints: Vec<&String> = m.keys().filter(|k| is_array_index(k)).collect();
    ints.sort_by_key(|k| k.parse::<u64>().unwrap_or(0));
    ints.extend(m.keys().filter(|k| !is_array_index(k)));
    ints
}

fn newline(indent: Option<usize>, depth: usize, out: &mut String) {
    if let Some(n) = indent {
        out.push('\n');
        for _ in 0..n * depth {
            out.push(' ');
        }
    }
}

fn write_value(v: &Value, indent: Option<usize>, depth: usize, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&number(n)),
        Value::String(s) => quote(s, out),
        Value::Array(a) => {
            if a.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(indent, depth + 1, out);
                write_value(item, indent, depth + 1, out);
            }
            newline(indent, depth, out);
            out.push(']');
        }
        Value::Object(m) => {
            if m.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, k) in js_key_order(m).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(indent, depth + 1, out);
                quote(k, out);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(&m[k.as_str()], indent, depth + 1, out);
            }
            newline(indent, depth, out);
            out.push('}');
        }
    }
}

/// JSON.stringify's QuoteJSONString.
pub fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A JSON number as JS prints it after `JSON.parse`.
pub fn number(n: &Number) -> String {
    let text = n.to_string();
    match text.parse::<f64>() {
        Ok(f) if f.is_finite() => js_number(f),
        _ => "null".to_owned(),
    }
}

/// `Number.prototype.toString()` for a finite double (radix 10). Pure.
pub fn js_number(f: f64) -> String {
    if f == 0.0 {
        return "0".to_owned();
    }
    let neg = f < 0.0;
    // Rust's `{:e}` prints the shortest round-tripping digits.
    let e = format!("{:e}", f.abs());
    let (mant, exp) = e.split_once('e').unwrap_or((&e, "0"));
    let digits: String = mant.chars().filter(|c| *c != '.').collect();
    let exp: i64 = exp.parse().unwrap_or(0);
    let k = digits.len() as i64;
    let n = exp + 1;
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    if k <= n && n <= 21 {
        s.push_str(&digits);
        for _ in 0..(n - k) {
            s.push('0');
        }
    } else if 0 < n && n <= 21 {
        s.push_str(&digits[..n as usize]);
        s.push('.');
        s.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        s.push_str("0.");
        for _ in 0..(-n) {
            s.push('0');
        }
        s.push_str(&digits);
    } else {
        let e = n - 1;
        let sign = if e < 0 { '-' } else { '+' };
        s.push_str(&digits[..1]);
        if k > 1 {
            s.push('.');
            s.push_str(&digits[1..]);
        }
        s.push('e');
        s.push(sign);
        s.push_str(&e.abs().to_string());
    }
    s
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn numbers_print_like_js() {
        for (src, js) in [
            ("0", "0"),
            ("-0", "0"),
            ("-0.0", "0"),
            ("1.0", "1"),
            ("1.50", "1.5"),
            ("100", "100"),
            ("1e21", "1e+21"),
            ("1e20", "100000000000000000000"),
            ("123456789012345678901", "123456789012345680000"),
            ("12345678901234567890123", "1.2345678901234568e+22"),
            ("0.000001", "0.000001"),
            ("0.0000001", "1e-7"),
            ("1.5e-7", "1.5e-7"),
            ("-2.5E+3", "-2500"),
            ("1700000000000", "1700000000000"),
            ("0.1", "0.1"),
            ("1e400", "null"),
            ("9007199254740993", "9007199254740992"),
        ] {
            let v = parse(src);
            assert_eq!(stringify(&v), js, "{src}");
        }
    }

    #[test]
    fn integer_keys_come_first_ascending() {
        let v = parse(r#"{"b":1,"10":2,"a":3,"2":4,"01":5,"4294967295":6,"4294967294":7}"#);
        assert_eq!(
            stringify(&v),
            r#"{"2":4,"10":2,"4294967294":7,"b":1,"a":3,"01":5,"4294967295":6}"#
        );
        assert!(is_array_index("0") && !is_array_index("-1") && !is_array_index(""));
    }

    #[test]
    fn pretty_matches_js_layout() {
        let v = parse(r#"{"a":[1,{"b":null}],"e":{},"f":[],"s":"x"}"#);
        assert_eq!(
            write_json_text(&v),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": null\n    }\n  ],\n  \"e\": {},\n  \"f\": [],\n  \"s\": \"x\"\n}\n"
        );
    }

    #[test]
    fn strings_escape_like_js() {
        let v = Value::String("q\" b\\ \u{8}\u{c}\n\r\t \u{1f} \u{7f} \u{2028} / é 😀".into());
        assert_eq!(
            stringify(&v),
            "\"q\\\" b\\\\ \\b\\f\\n\\r\\t \\u001f \u{7f} \u{2028} / é 😀\""
        );
    }

    #[test]
    fn agrees_with_serde_on_ordinary_js_output() {
        let src = r#"{"oauthAccount":{"accountUuid":"u-1","emailAddress":"alice@example.com","organizationUuid":null},"n":3,"list":["a",true,false,null,-1.25]}"#;
        let v = parse(src);
        assert_eq!(stringify(&v), src);
        assert_eq!(
            stringify_pretty(&v),
            serde_json::to_string_pretty(&v).unwrap()
        );
    }
}
