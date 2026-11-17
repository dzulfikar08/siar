//! Outgoing HTTP (src/lib/http.ts): blocking ureq with rustls. Non-2xx answers become
//! Error::Http with the body, like HttpError. Request bodies can stream from a file.

use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use ureq::http;

use crate::util::{query_string, Error, Result};

fn agent() -> &'static ureq::Agent {
    static A: OnceLock<ureq::Agent> = OnceLock::new();
    A.get_or_init(|| {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .user_agent("siar/0.1")
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into()
    })
}

pub enum Body {
    None,
    Json(Value),
    Form(Vec<(String, String)>),
    Bytes(Vec<u8>),
    /// `len` bytes of a file starting at `offset`, streamed.
    File {
        path: PathBuf,
        offset: u64,
        len: u64,
    },
}

pub struct Req {
    method: &'static str,
    url: String,
    headers: Vec<(String, String)>,
    body: Body,
    timeout: Duration,
}

pub fn req(method: &'static str, url: impl Into<String>) -> Req {
    Req {
        method,
        url: url.into(),
        headers: vec![],
        body: Body::None,
        timeout: Duration::from_secs(120),
    }
}
pub fn get(url: impl Into<String>) -> Req {
    req("GET", url)
}
pub fn post(url: impl Into<String>) -> Req {
    req("POST", url)
}

impl Req {
    pub fn header(mut self, k: &str, v: impl Into<String>) -> Self {
        self.headers.push((k.to_string(), v.into()));
        self
    }
    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }
    pub fn headers(mut self, h: &[(&str, String)]) -> Self {
        for (k, v) in h {
            self.headers.push((k.to_string(), v.clone()));
        }
        self
    }
    /// Query parameters; values that are None are skipped (like undefined in TS).
    pub fn query(mut self, q: &[(&str, Option<String>)]) -> Self {
        let q: Vec<(&str, String)> = q.iter().filter_map(|(k, v)| v.clone().map(|v| (*k, v))).collect();
        if !q.is_empty() {
            let sep = if self.url.contains('?') { '&' } else { '?' };
            self.url = format!("{}{sep}{}", self.url, query_string(&q));
        }
        self
    }
    pub fn json(mut self, v: Value) -> Self {
        self.body = Body::Json(v);
        self
    }
    pub fn form(mut self, f: &[(&str, Option<String>)]) -> Self {
        self.body = Body::Form(f.iter().filter_map(|(k, v)| v.clone().map(|v| (k.to_string(), v))).collect());
        self
    }
    pub fn body(mut self, b: Body) -> Self {
        self.body = b;
        self
    }
    pub fn timeout_secs(mut self, s: u64) -> Self {
        self.timeout = Duration::from_secs(s);
        self
    }

    pub fn send(self) -> Result<Resp> {
        let mut b = http::Request::builder().method(self.method).uri(&self.url);
        let has = |k: &str| self.headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(k));
        let mut extra: Vec<(String, String)> = vec![];
        let send = match self.body {
            Body::None => {
                if self.method == "POST" || self.method == "PUT" {
                    extra.push(("content-length".into(), "0".into()));
                }
                ureq::SendBody::none()
            }
            Body::Json(v) => {
                if !has("content-type") {
                    extra.push(("content-type".into(), "application/json".into()));
                }
                let bytes = serde_json::to_vec(&v)?;
                extra.push(("content-length".into(), bytes.len().to_string()));
                ureq::SendBody::from_owned_reader(std::io::Cursor::new(bytes))
            }
            Body::Form(f) => {
                extra.push(("content-type".into(), "application/x-www-form-urlencoded;charset=UTF-8".into()));
                let pairs: Vec<(&str, String)> = f.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
                let bytes = query_string(&pairs).into_bytes();
                extra.push(("content-length".into(), bytes.len().to_string()));
                ureq::SendBody::from_owned_reader(std::io::Cursor::new(bytes))
            }
            Body::Bytes(bytes) => {
                extra.push(("content-length".into(), bytes.len().to_string()));
                ureq::SendBody::from_owned_reader(std::io::Cursor::new(bytes))
            }
            Body::File { path, offset, len } => {
                let mut f = File::open(&path)?;
                f.seek(SeekFrom::Start(offset))?;
                extra.push(("content-length".into(), len.to_string()));
                ureq::SendBody::from_owned_reader(f.take(len))
            }
        };
        for (k, v) in self.headers.iter().chain(extra.iter()) {
            b = b.header(k.as_str(), v.as_str());
        }
        let request = b.body(send).map_err(|e| Error::Msg(format!("Invalid request: {e}")))?;
        let a = agent();
        let request = a.configure_request(request).timeout_global(Some(self.timeout)).build();
        let res = a.run(request)?;
        let status = res.status().as_u16();
        let (parts, body) = res.into_parts();
        if !(200..300).contains(&status) {
            let mut body = body;
            let text = body.with_config().limit(1 << 20).read_to_string().unwrap_or_default();
            return Err(Error::Http {
                status,
                body: text,
                url: self.url,
            });
        }
        Ok(Resp { headers: parts.headers, body })
    }

    /// Send and parse JSON (`{}` for an empty body).
    pub fn get_json(self) -> Result<Value> {
        self.send()?.json()
    }
}

pub struct Resp {
    pub headers: http::HeaderMap,
    pub body: ureq::Body,
}

impl Resp {
    pub fn header(&self, k: &str) -> Option<String> {
        self.headers.get(k).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
    }
    pub fn text(mut self) -> Result<String> {
        Ok(self.body.with_config().limit(64 << 20).read_to_string()?)
    }
    pub fn json(self) -> Result<Value> {
        let t = self.text()?;
        if t.is_empty() {
            return Ok(Value::Object(Default::default()));
        }
        Ok(serde_json::from_str(&t)?)
    }
    pub fn into_reader(self) -> impl Read {
        self.body.into_reader()
    }
}
