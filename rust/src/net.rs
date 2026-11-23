//! Outgoing HTTP: blocking ureq with rustls. Non-2xx answers become
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
    /// multipart/form-data like FormData; file parts stream from disk.
    Multipart(Vec<Part>),
}

pub enum PartData {
    Text(String),
    Bytes(Vec<u8>),
    /// A staged media file (kept alive until the request is sent).
    Local(crate::media::LocalFile),
    /// `len` bytes of a file from `offset`.
    Slice {
        path: PathBuf,
        offset: u64,
        len: u64,
    },
}

pub struct Part {
    pub name: String,
    pub filename: Option<String>,
    pub mime: Option<String>,
    pub data: PartData,
}

impl Part {
    pub fn text(name: &str, value: impl Into<String>) -> Part {
        Part {
            name: name.into(),
            filename: None,
            mime: None,
            data: PartData::Text(value.into()),
        }
    }
    /// A file part like `fd.append(name, new Blob([...], { type }), filename)`.
    pub fn file(name: &str, filename: &str, mime: Option<&str>, data: PartData) -> Part {
        Part {
            name: name.into(),
            filename: Some(filename.into()),
            mime: Some(mime.unwrap_or("application/octet-stream").into()),
            data,
        }
    }
}

/// Reads several readers one after the other.
struct Chain(std::collections::VecDeque<Box<dyn Read + Send>>);

impl Read for Chain {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while let Some(r) = self.0.front_mut() {
            let n = r.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            self.0.pop_front();
        }
        Ok(0)
    }
}

fn quote_header(s: &str) -> String {
    s.replace('"', "%22").replace('\r', "%0D").replace('\n', "%0A")
}

/// Builds a streaming multipart body: (content type, length, reader).
fn multipart(parts: Vec<Part>) -> Result<(String, u64, Chain)> {
    let boundary = format!("----siarFormBoundary{}", crate::util::hex(&crate::util::random_bytes(12)));
    let mut readers: std::collections::VecDeque<Box<dyn Read + Send>> = Default::default();
    let mut len = 0u64;
    let push_bytes = |readers: &mut std::collections::VecDeque<Box<dyn Read + Send>>, b: Vec<u8>, len: &mut u64| {
        *len += b.len() as u64;
        readers.push_back(Box::new(std::io::Cursor::new(b)));
    };
    for p in parts {
        let mut head = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{}\"", quote_header(&p.name));
        if let Some(f) = &p.filename {
            head.push_str(&format!("; filename=\"{}\"", quote_header(f)));
        }
        head.push_str("\r\n");
        if let Some(m) = &p.mime {
            head.push_str(&format!("Content-Type: {m}\r\n"));
        }
        head.push_str("\r\n");
        push_bytes(&mut readers, head.into_bytes(), &mut len);
        match p.data {
            PartData::Text(t) => push_bytes(&mut readers, t.into_bytes(), &mut len),
            PartData::Bytes(b) => push_bytes(&mut readers, b, &mut len),
            PartData::Local(f) => {
                let file = File::open(&f.path)?;
                len += f.len;
                // The LocalFile guard rides along so a temporary download outlives the upload.
                readers.push_back(Box::new(Guarded(file.take(f.len), f)));
            }
            PartData::Slice { path, offset, len: n } => {
                let mut file = File::open(&path)?;
                file.seek(SeekFrom::Start(offset))?;
                len += n;
                readers.push_back(Box::new(file.take(n)));
            }
        }
        push_bytes(&mut readers, b"\r\n".to_vec(), &mut len);
    }
    push_bytes(&mut readers, format!("--{boundary}--\r\n").into_bytes(), &mut len);
    Ok((format!("multipart/form-data; boundary={boundary}"), len, Chain(readers)))
}

struct Guarded<R>(R, crate::media::LocalFile);

impl<R: Read> Read for Guarded<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let _ = &self.1;
        self.0.read(buf)
    }
}

pub struct Req {
    method: &'static str,
    url: String,
    headers: Vec<(String, String)>,
    body: Body,
    timeout: Duration,
    /// Some(0): do not follow redirects; a 3xx answer is returned as is.
    redirects: Option<u32>,
}

pub fn req(method: &'static str, url: impl Into<String>) -> Req {
    Req {
        method,
        url: url.into(),
        headers: vec![],
        body: Body::None,
        timeout: Duration::from_secs(120),
        redirects: None,
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
    pub fn basic(self, user: &str, pass: &str) -> Self {
        self.header("authorization", crate::util::basic_auth(user, pass))
    }
    pub fn multipart(mut self, parts: Vec<Part>) -> Self {
        self.body = Body::Multipart(parts);
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
    /// Return 3xx answers instead of following them (the caller checks each hop).
    pub fn no_redirects(mut self) -> Self {
        self.redirects = Some(0);
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
            Body::Multipart(parts) => {
                let (ct, len, reader) = multipart(parts)?;
                extra.push(("content-type".into(), ct));
                extra.push(("content-length".into(), len.to_string()));
                ureq::SendBody::from_owned_reader(reader)
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
        let mut cfg = a.configure_request(request).timeout_global(Some(self.timeout));
        if let Some(n) = self.redirects {
            cfg = cfg.max_redirects(n).max_redirects_will_error(false);
        }
        let request = cfg.build();
        let res = a.run(request)?;
        let status = res.status().as_u16();
        let (parts, body) = res.into_parts();
        let redirect = self.redirects == Some(0) && (300..400).contains(&status);
        if !(200..300).contains(&status) && !redirect {
            let mut body = body;
            let text = body.with_config().limit(1 << 20).read_to_string().unwrap_or_default();
            return Err(Error::Http {
                status,
                body: text,
                url: self.url,
            });
        }
        Ok(Resp {
            status,
            headers: parts.headers,
            body,
        })
    }

    /// Send and parse JSON (`{}` for an empty body).
    pub fn get_json(self) -> Result<Value> {
        self.send()?.json()
    }
}

pub struct Resp {
    pub status: u16,
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
