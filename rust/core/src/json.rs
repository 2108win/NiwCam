//! Minimal JSON value, parser and writer for the phone protocol's control messages. Port of
//! `src/engine/Json.h`. Small on purpose: objects keep insertion order, numbers are doubles,
//! strings are UTF-8.
//!
//! Known differences from the C++ (see `rust/DECISIONS.md` ADR-006): strings are `String`, so
//! input bytes that are not UTF-8 become U+FFFD and a lone `\uD800`-`\uDFFF` escape becomes
//! U+FFFD (the C++ wrote an invalid 3-byte sequence); a bad `\u` escape or a number out of range
//! is an error with its own message.

use std::fmt::Write as _;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(Vec<(String, Value)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError(pub String);

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JsonError {}

fn err<T>(msg: &str) -> Result<T, JsonError> {
    Err(JsonError(msg.to_string()))
}

static NULL: Value = Value::Null;

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Value::Number(v as f64)
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Value::Number(v as f64)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Value::Number(v as f64)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Value::Number(v)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::String(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::String(s)
    }
}
impl From<Vec<Value>> for Value {
    fn from(a: Vec<Value>) -> Self {
        Value::Array(a)
    }
}

impl Value {
    pub fn object() -> Self {
        Value::Object(Vec::new())
    }

    pub fn array() -> Self {
        Value::Array(Vec::new())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
    pub fn is_object(&self) -> bool {
        matches!(self, Value::Object(_))
    }
    pub fn is_array(&self) -> bool {
        matches!(self, Value::Array(_))
    }
    pub fn is_string(&self) -> bool {
        matches!(self, Value::String(_))
    }
    pub fn is_number(&self) -> bool {
        matches!(self, Value::Number(_))
    }

    pub fn bool_or(&self, fallback: bool) -> bool {
        match self {
            Value::Bool(b) => *b,
            _ => fallback,
        }
    }

    pub fn num(&self, fallback: f64) -> f64 {
        match self {
            Value::Number(n) => *n,
            _ => fallback,
        }
    }

    /// Rounded half away from zero, like `std::lround`; saturates at the `i32` range.
    pub fn int(&self, fallback: i32) -> i32 {
        match self {
            Value::Number(n) => n.round() as i32,
            _ => fallback,
        }
    }

    /// The string, or `""` when this is not a string.
    pub fn str(&self) -> &str {
        match self {
            Value::String(s) => s,
            _ => "",
        }
    }

    pub fn str_or(&self, fallback: &str) -> String {
        match self {
            Value::String(s) => s.clone(),
            _ => fallback.to_string(),
        }
    }

    pub fn items(&self) -> &[Value] {
        match self {
            Value::Array(a) => a,
            _ => &[],
        }
    }

    pub fn members(&self) -> &[(String, Value)] {
        match self {
            Value::Object(o) => o,
            _ => &[],
        }
    }

    /// Object access: a missing key (or a value that is not an object) reads as null.
    pub fn get(&self, key: &str) -> &Value {
        self.members()
            .iter()
            .find(|(k, _)| k == key)
            .map_or(&NULL, |(_, v)| v)
    }

    /// Sets a member, replacing one with the same key in place; a value that is not an object
    /// becomes one.
    pub fn set(&mut self, key: &str, v: impl Into<Value>) -> &mut Self {
        if !self.is_object() {
            *self = Value::object();
        }
        if let Value::Object(o) = self {
            let v = v.into();
            match o.iter_mut().find(|(k, _)| k == key) {
                Some((_, existing)) => *existing = v,
                None => o.push((key.to_string(), v)),
            }
        }
        self
    }

    /// Appends to an array; a value that is not an array becomes one.
    pub fn push(&mut self, v: impl Into<Value>) -> &mut Self {
        if !self.is_array() {
            *self = Value::array();
        }
        if let Value::Array(a) = self {
            a.push(v.into());
        }
        self
    }

    pub fn dump(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    pub fn parse(text: &str) -> Result<Value, JsonError> {
        let mut p = Parser {
            t: text.as_bytes(),
            pos: 0,
        };
        let v = p.value()?;
        p.skip_ws();
        if p.pos != p.t.len() {
            return err("json: trailing data");
        }
        Ok(v)
    }

    /// Parses wire bytes; bytes that are not UTF-8 become U+FFFD.
    pub fn parse_bytes(bytes: &[u8]) -> Result<Value, JsonError> {
        Self::parse(&String::from_utf8_lossy(bytes))
    }

    fn write(&self, out: &mut String) {
        match self {
            Value::Null => out.push_str("null"),
            Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Value::Number(n) => out.push_str(&format_number(*n)),
            Value::String(s) => write_string(out, s),
            Value::Array(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Value::Object(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_string(out, k);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

impl std::ops::Index<&str> for Value {
    type Output = Value;
    fn index(&self, key: &str) -> &Value {
        self.get(key)
    }
}

/// Whole numbers print as integers (`%lld`), the rest as `%.9g`; infinities and NaN as 0.
fn format_number(n: f64) -> String {
    if n.is_finite() && n == n.floor() && n.abs() < 1e15 {
        return format!("{}", n as i64);
    }
    format_g9(if n.is_finite() { n } else { 0.0 })
}

/// C's `%.9g` for a finite, non-integral (or huge) number.
fn format_g9(v: f64) -> String {
    const P: i32 = 9;
    if v == 0.0 {
        return "0".to_string();
    }
    // The exponent after rounding to P significant digits decides the style, as in C.
    let sci = format!("{:.*e}", (P - 1) as usize, v);
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let x: i32 = exp.parse().unwrap_or(0);
    if !(-4..P).contains(&x) {
        let mant = strip_zeros(mant);
        let sign = if x < 0 { '-' } else { '+' };
        format!("{mant}e{sign}{:02}", x.abs())
    } else {
        let decimals = (P - 1 - x) as usize;
        strip_zeros(&format!("{v:.decimals$}")).to_string()
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                // INFALLIBLE: writing to a String cannot fail.
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    t: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.t.get(self.pos), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.t.get(self.pos).copied()
    }

    fn starts_with(&self, word: &str) -> bool {
        self.t[self.pos..].starts_with(word.as_bytes())
    }

    fn value(&mut self) -> Result<Value, JsonError> {
        self.skip_ws();
        let Some(c) = self.peek() else {
            return err("json: unexpected end");
        };
        match c {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(Value::String(self.string()?)),
            _ if self.starts_with("true") => {
                self.pos += 4;
                Ok(Value::Bool(true))
            }
            _ if self.starts_with("false") => {
                self.pos += 5;
                Ok(Value::Bool(false))
            }
            _ if self.starts_with("null") => {
                self.pos += 4;
                Ok(Value::Null)
            }
            _ => self.number(),
        }
    }

    fn object(&mut self) -> Result<Value, JsonError> {
        let mut v = Value::object();
        self.pos += 1;
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(v);
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return err("json: expected ':'");
            }
            self.pos += 1;
            let member = self.value()?;
            v.set(&key, member);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(v);
                }
                _ => return err("json: expected ',' or '}'"),
            }
        }
    }

    fn array(&mut self) -> Result<Value, JsonError> {
        let mut v = Value::array();
        self.pos += 1;
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(v);
        }
        loop {
            let item = self.value()?;
            v.push(item);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(v);
                }
                _ => return err("json: expected ',' or ']'"),
            }
        }
    }

    /// Takes the longest run of digits and `+-.eE`, then reads the longest number at its start,
    /// like `std::stod` on that run.
    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.pos;
        let mut end = start;
        while end < self.t.len()
            && (self.t[end].is_ascii_digit() || b"+-.eE".contains(&self.t[end]))
        {
            end += 1;
        }
        if end == start {
            return err("json: bad token");
        }
        let run = &self.t[start..end];
        let len = number_prefix_len(run);
        if len == 0 {
            return err("json: bad number");
        }
        // INFALLIBLE: the prefix is ASCII digits, signs, dots and exponent letters only.
        let text = std::str::from_utf8(&run[..len]).unwrap_or("0");
        let d: f64 = text
            .parse()
            .map_err(|_| JsonError("json: bad number".into()))?;
        if !d.is_finite() {
            return err("json: number out of range");
        }
        self.pos = end;
        Ok(Value::Number(d))
    }

    fn hex4(&mut self) -> Result<u32, JsonError> {
        let digits = self
            .t
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| JsonError("json: bad \\u escape".into()))?;
        let mut cp = 0u32;
        for &d in digits {
            let n = (d as char)
                .to_digit(16)
                .ok_or_else(|| JsonError("json: bad \\u escape".into()))?;
            cp = cp * 16 + n;
        }
        self.pos += 4;
        Ok(cp)
    }

    fn string(&mut self) -> Result<String, JsonError> {
        if self.peek() != Some(b'"') {
            return err("json: expected string");
        }
        self.pos += 1;
        let mut s: Vec<u8> = Vec::new();
        while let Some(c) = self.peek() {
            if c == b'"' {
                break;
            }
            self.pos += 1;
            if c != b'\\' {
                s.push(c);
                continue;
            }
            let Some(e) = self.peek() else { break };
            self.pos += 1;
            match e {
                b'n' => s.push(b'\n'),
                b'r' => s.push(b'\r'),
                b't' => s.push(b'\t'),
                b'b' => s.push(0x08),
                b'f' => s.push(0x0c),
                b'u' => {
                    let mut cp = self.hex4()?;
                    if (0xD800..=0xDBFF).contains(&cp) && self.starts_with("\\u") {
                        self.pos += 2;
                        let lo = self.hex4()?;
                        // As the C++: no range check on the low half.
                        cp = 0x10000u32
                            .wrapping_add((cp - 0xD800) << 10)
                            .wrapping_add(lo.wrapping_sub(0xDC00));
                    }
                    let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                    let mut buf = [0u8; 4];
                    s.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                other => s.push(other),
            }
        }
        if self.pos >= self.t.len() {
            return err("json: unterminated string");
        }
        self.pos += 1;
        Ok(String::from_utf8_lossy(&s).into_owned())
    }
}

/// Length of the longest prefix of `run` that is a decimal float: sign, digits, an optional
/// fraction, an optional exponent. 0 when there is no digit.
fn number_prefix_len(run: &[u8]) -> usize {
    let mut i = 0;
    if matches!(run.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_start = i;
    while run.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let mut digits = i - int_start;
    if run.get(i) == Some(&b'.') {
        let frac_start = i + 1;
        let mut j = frac_start;
        while run.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        digits += j - frac_start;
        if digits > 0 {
            i = j;
        }
    }
    if digits == 0 {
        return 0;
    }
    if matches!(run.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(run.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        let exp_start = j;
        while run.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        if j > exp_start {
            i = j;
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_order_and_types() {
        let text = r#"{"type":"caps","n":3,"f":0.5,"ok":true,"none":null,"a":[1,2,{"x":"y"}],"s":"a\"b\\c\n"}"#;
        let v = Value::parse(text).unwrap();
        assert_eq!(v.dump(), text);
        assert_eq!(v["type"].str(), "caps");
        assert_eq!(v["a"].items().len(), 3);
        assert_eq!(v["a"].items()[2]["x"].str(), "y");
        assert!(v["missing"].is_null());
        assert_eq!(v["n"].int(0), 3);
        assert!(v["ok"].bool_or(false));
    }

    #[test]
    fn numbers_print_like_the_cpp() {
        assert_eq!(Value::from(30).dump(), "30");
        assert_eq!(Value::from(-7).dump(), "-7");
        assert_eq!(Value::from(0.5).dump(), "0.5");
        assert_eq!(Value::from(1.0 / 3.0).dump(), "0.333333333");
        assert_eq!(Value::from(1e20).dump(), "1e+20");
        assert_eq!(Value::from(1.5e-7).dump(), "1.5e-07");
        assert_eq!(Value::from(123456789.5).dump(), "123456790");
        assert_eq!(Value::from(f64::NAN).dump(), "0");
        assert_eq!(Value::from(f64::INFINITY).dump(), "0");
        assert_eq!(Value::from(-0.25).dump(), "-0.25");
    }

    #[test]
    fn unicode_escapes() {
        let v = Value::parse(r#""\u00e9 \ud83d\ude00 \u0041""#).unwrap();
        assert_eq!(v.str(), "é 😀 A");
        assert_eq!(Value::from("\u{1}").dump(), "\"\\u0001\"");
    }

    #[test]
    fn errors() {
        for bad in [
            "",
            "{",
            "[1,",
            "{\"a\" 1}",
            "[1 2]",
            "\"abc",
            "tru",
            "{} x",
            "-",
        ] {
            assert!(Value::parse(bad).is_err(), "{bad:?}");
        }
        assert_eq!(Value::parse("[1] x").unwrap_err().0, "json: trailing data");
    }

    #[test]
    fn set_replaces_in_place() {
        let mut v = Value::object();
        v.set("a", 1).set("b", 2).set("a", 3);
        assert_eq!(v.dump(), r#"{"a":3,"b":2}"#);
    }

    #[test]
    fn number_tokens_like_stod() {
        assert_eq!(Value::parse("-12.5e2").unwrap().num(0.0), -1250.0);
        assert_eq!(Value::parse("1-2").unwrap().num(0.0), 1.0); // stod reads the prefix
        assert!(Value::parse("1e999").is_err());
    }
}
