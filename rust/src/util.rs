use serde_json::{Map, Value};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Errors carry the same messages as the TypeScript version, because agents read them.
#[derive(Debug)]
pub enum Error {
    /// A non-2xx answer from a platform. 429 and 5xx are retryable, 401 means re-auth.
    Http { status: u16, body: String, url: String },
    /// The post itself is invalid: never retried.
    Permanent(String),
    /// Anything else (network errors, bad input). Retried by the scheduler.
    Msg(String),
}

impl Error {
    pub fn retryable(&self) -> bool {
        match self {
            Error::Http { status, .. } => *status == 429 || *status >= 500,
            Error::Permanent(_) => false,
            Error::Msg(_) => true,
        }
    }
    pub fn auth_failed(&self) -> bool {
        matches!(self, Error::Http { status: 401, .. })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Http { status, body, url } => {
                let (host, path) = host_path(url);
                let short: String = body.chars().take(500).collect();
                write!(f, "HTTP {status} from {host}{path}: {short}")
            }
            Error::Permanent(m) | Error::Msg(m) => f.write_str(m),
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Msg(e.to_string())
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Msg(e.to_string())
    }
}
impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        Error::Msg(format!("fetch failed: {e}"))
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Msg(format!("JSON Parse error: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

macro_rules! bail {
    ($($t:tt)*) => { return Err($crate::util::Error::Msg(format!($($t)*))) };
}
macro_rules! permanent {
    ($($t:tt)*) => { return Err($crate::util::Error::Permanent(format!($($t)*))) };
}
pub(crate) use bail;
pub(crate) use permanent;

pub fn host_path(url: &str) -> (&str, &str) {
    let rest = url.split_once("://").map(|x| x.1).unwrap_or(url);
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let path = path.split(['?', '#']).next().unwrap_or("/");
    (host, path)
}

pub fn host_of(url: &str) -> String {
    let h = host_path(url).0;
    let h = h.rsplit_once('@').map(|x| x.1).unwrap_or(h);
    if h.starts_with('[') {
        return h.split(']').next().unwrap_or("").trim_start_matches('[').to_string();
    }
    h.split(':').next().unwrap_or("").to_ascii_lowercase()
}

// ---------- time ----------

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `new Date(ms).toISOString()`
pub fn iso(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// ISO 8601 date-time as platforms and agents send it: `2026-11-17T08:00:00+07:00`, `...Z`,
/// `...+0000`, optional seconds and fraction. Without an offset the time is taken as UTC
/// (callers that need an explicit offset check for it first). Returns epoch ms.
pub fn parse_iso(s: &str) -> Option<i64> {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let part = s.get(from..from + len)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let y = num(0, 4)?;
    if b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let mo = num(5, 2)?;
    let d = num(8, 2)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut i = 10;
    let (mut h, mut mi, mut sec, mut ms) = (0, 0, 0, 0);
    if i < b.len() && (b[i] == b'T' || b[i] == b't' || b[i] == b' ') {
        h = num(i + 1, 2)?;
        if b.get(i + 3) != Some(&b':') {
            return None;
        }
        mi = num(i + 4, 2)?;
        i += 6;
        if b.get(i) == Some(&b':') {
            sec = num(i + 1, 2)?;
            i += 3;
            if b.get(i) == Some(&b'.') || b.get(i) == Some(&b',') {
                let start = i + 1;
                let mut end = start;
                while end < b.len() && b[end].is_ascii_digit() {
                    end += 1;
                }
                if end == start {
                    return None;
                }
                let frac = &s[start..end.min(start + 3)];
                ms = format!("{frac:0<3}").parse().ok()?;
                i = end;
            }
        }
        if h > 24 || mi > 59 || sec > 59 {
            return None;
        }
    }
    let mut offset_min = 0i64;
    if i < b.len() {
        match b[i] {
            b'Z' | b'z' if i + 1 == b.len() => {}
            b'+' | b'-' => {
                let sign = if b[i] == b'-' { -1 } else { 1 };
                let oh = num(i + 1, 2)?;
                let om = if b.get(i + 3) == Some(&b':') { num(i + 4, 2)? } else { num(i + 3, 2)? };
                let used = if b.get(i + 3) == Some(&b':') { 6 } else { 5 };
                if i + used != b.len() {
                    return None;
                }
                offset_min = sign * (oh * 60 + om);
            }
            _ => return None,
        }
    }
    let days = days_from_civil(y, mo, d);
    Some(((days * 24 + h) * 60 + mi - offset_min) * 60_000 + sec * 1000 + ms)
}

/// A time an agent gave: must say which timezone it means.
pub fn parse_time(v: Option<&Value>) -> Result<Option<i64>> {
    let s = match v {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) => s.as_str(),
        Some(other) => bail!("Cannot parse time \"{}\"", js_string(other)),
    };
    if s.is_empty() {
        return Ok(None);
    }
    if s == "now" {
        return Ok(Some(now()));
    }
    if !has_offset(s.trim()) {
        bail!("Time \"{s}\" has no timezone. Use ISO 8601 with an offset, e.g. 2026-11-17T08:00:00+07:00");
    }
    match parse_iso(s) {
        Some(t) => Ok(Some(t)),
        None => bail!("Cannot parse time \"{s}\""),
    }
}

/// `/(Z|[+-]\d{2}:?\d{2})$/i`
fn has_offset(s: &str) -> bool {
    let b = s.as_bytes();
    if b.last().map(|c| c.eq_ignore_ascii_case(&b'z')) == Some(true) {
        return true;
    }
    let digits = |x: &[u8]| x.iter().all(|c| c.is_ascii_digit());
    let n = b.len();
    (n >= 5 && (b[n - 5] == b'+' || b[n - 5] == b'-') && digits(&b[n - 4..]))
        || (n >= 6 && (b[n - 6] == b'+' || b[n - 6] == b'-') && digits(&b[n - 5..n - 3]) && b[n - 3] == b':' && digits(&b[n - 2..]))
}

// ---------- JSON helpers ----------

/// Object builder that, like JSON.stringify, leaves out keys whose value is undefined (None).
#[derive(Default)]
pub struct Obj(pub Map<String, Value>);

impl Obj {
    pub fn new() -> Self {
        Obj(Map::new())
    }
    pub fn set(mut self, k: &str, v: impl Into<Value>) -> Self {
        self.0.insert(k.to_string(), v.into());
        self
    }
    pub fn opt<T: Into<Value>>(mut self, k: &str, v: Option<T>) -> Self {
        if let Some(v) = v {
            self.0.insert(k.to_string(), v.into());
        }
        self
    }
    /// Sets the key unless the value is null (JS `x ?? undefined`).
    pub fn defined(mut self, k: &str, v: Option<&Value>) -> Self {
        if let Some(v) = v {
            if !v.is_null() {
                self.0.insert(k.to_string(), v.clone());
            }
        }
        self
    }
    pub fn build(self) -> Value {
        Value::Object(self.0)
    }
}

/// Parse a JSON column, falling back when it is missing or invalid.
pub fn json_or(s: Option<&str>, fallback: Value) -> Value {
    s.and_then(|s| serde_json::from_str(s).ok()).unwrap_or(fallback)
}

/// JS truthiness.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0 && !f.is_nan()).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// The value unless it is null/undefined (`a ?? b` building block).
pub fn some(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

/// String(v) as JS would print it.
pub fn js_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Number(n) => jnum_str(n),
        Value::Bool(b) => b.to_string(),
        Value::Array(a) => a
            .iter()
            .map(|x| if x.is_null() { String::new() } else { js_string(x) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

fn jnum_str(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    match n.as_f64() {
        Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{}", f as i64),
        Some(f) => f.to_string(),
        None => n.to_string(),
    }
}

/// A number as JSON.stringify prints it (2.0 -> 2).
pub fn jnum(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9.0e15 {
        Value::from(f as i64)
    } else if f.is_finite() {
        Value::from(f)
    } else {
        Value::Null
    }
}

/// `Number(x)` for platform numbers that may come as strings.
pub fn to_num(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse().unwrap_or(f64::NAN)
            }
        }
        Some(Value::Bool(b)) => {
            if *b {
                1.0
            } else {
                0.0
            }
        }
        Some(Value::Null) | None => 0.0,
        _ => f64::NAN,
    }
}

pub fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

pub fn arg_str(args: &Value, k: &str) -> Option<String> {
    match args.get(k) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(v) => Some(js_string(v)),
    }
}

pub fn arg_i64(args: &Value, k: &str) -> Option<i64> {
    match args.get(k) {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
}

pub fn arg_bool(args: &Value, k: &str) -> Option<bool> {
    match args.get(k) {
        None | Some(Value::Null) => None,
        v => Some(truthy(v)),
    }
}

pub fn arg_strs(args: &Value, k: &str) -> Option<Vec<String>> {
    args.get(k).and_then(|v| v.as_array()).map(|a| a.iter().map(js_string).collect())
}

/// `epoch ms` from a platform timestamp (Date.parse); 0 when unparseable.
pub fn date_parse(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::String(s)) => parse_iso(s).unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

// ---------- encoding ----------

/// application/x-www-form-urlencoded, like URLSearchParams.
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => out.push(b as char),
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// encodeURIComponent
pub fn uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub fn query_string(q: &[(&str, String)]) -> String {
    q.iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `Basic base64(user:pass)`
pub fn basic_auth(user: &str, pass: &str) -> String {
    use base64::Engine;
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
}

/// `[\p{L}\p{N}_]` with JavaScript's u flag (letters, numbers, underscore; no combining marks).
pub fn is_tag_char(c: char) -> bool {
    let cp = c as u32;
    if cp < 0x80 {
        return c.is_ascii_alphanumeric() || c == '_';
    }
    crate::unicode_tables::TAG_CHAR
        .binary_search_by(|&(lo, hi)| {
            if hi < cp {
                std::cmp::Ordering::Less
            } else if lo > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// JavaScript's `\s`.
pub fn js_is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// Hashtags as `/(?<=^|\s)#([\p{L}\p{N}_]+)/gu` finds them: (byte start of '#', tag).
pub fn hashtags(s: &str) -> Vec<(usize, String)> {
    let mut out = vec![];
    let mut prev: Option<char> = None;
    for (i, c) in s.char_indices() {
        if c == '#' && prev.map(js_is_space).unwrap_or(true) {
            let tag: String = s[i + 1..].chars().take_while(|c| is_tag_char(*c)).collect();
            if !tag.is_empty() {
                out.push((i, tag));
            }
        }
        prev = Some(c);
    }
    out
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("system random source");
    b
}

/// `${prefix}_` + 16 hex chars of a v4 UUID (the 13th is always "4"), like the TS ids.
pub fn new_id(prefix: &str) -> String {
    let mut h = hex(&random_bytes(8));
    h.replace_range(12..13, "4");
    format!("{prefix}_{h}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_round_trip() {
        let t = parse_iso("2026-11-17T08:00:00+07:00").unwrap();
        assert_eq!(iso(t), "2026-11-17T01:00:00.000Z");
        assert_eq!(parse_iso("2026-11-10T03:12:45+0000"), parse_iso("2026-11-10T03:12:45.000Z"));
        assert_eq!(parse_iso("1970-01-01T00:00:00.5Z"), Some(500));
        assert!(parse_iso("2026-13-01T00:00:00Z").is_none());
        assert!(parse_time(Some(&Value::from("2026-11-17T08:00:00"))).is_err());
        assert!(parse_time(Some(&Value::from("2026-11-17T08:00:00+0700"))).unwrap().is_some());
    }

    #[test]
    fn ids_look_like_ts_ids() {
        let id = new_id("med");
        assert_eq!(id.len(), 20);
        assert_eq!(&id[16..17], "4");
    }

    #[test]
    fn numbers_print_like_js() {
        assert_eq!(serde_json::to_string(&jnum(2.0)).unwrap(), "2");
        assert_eq!(serde_json::to_string(&jnum(1.25)).unwrap(), "1.25");
    }
}
