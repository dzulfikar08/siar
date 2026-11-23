//! Staged media: a local copy always, plus Cloudflare R2 (hand-rolled SigV4)
//! when configured. Downloads and R2 uploads stream through disk, never whole-file in memory.

use base64::Engine;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::config::{config, r2_enabled};
use crate::db::{self, MediaRow};
use crate::net::{self, Body};
use crate::util::{bail, hex, iso, new_id, now, uri_component, Obj, Result};

const MIME: [(&str, &str); 9] = [
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".png", "image/png"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".mp4", "video/mp4"),
    (".mov", "video/quicktime"),
    (".webm", "video/webm"),
    (".pdf", "application/pdf"),
];

pub fn mime_for_ext(ext: &str) -> Option<&'static str> {
    MIME.iter().find(|(e, _)| e.eq_ignore_ascii_case(ext)).map(|x| x.1)
}

/// The extension for a MIME type (the last one listed wins, as in the TS object).
fn ext_for_mime(mime: &str) -> &'static str {
    MIME.iter().rev().find(|(_, m)| *m == mime).map(|x| x.0).unwrap_or("")
}

pub fn sniff(b: &[u8]) -> Option<&'static str> {
    let h = |i: usize| b.get(i).copied();
    if h(0) == Some(0xff) && h(1) == Some(0xd8) {
        return Some("image/jpeg");
    }
    if b.starts_with(&[0x89, 0x50, 0x4e, 0x47]) {
        return Some("image/png");
    }
    if b.starts_with(b"GIF") {
        return Some("image/gif");
    }
    if b.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    if b.starts_with(b"%PDF") {
        return Some("application/pdf");
    }
    if b.get(4..8) == Some(b"ftyp") {
        return Some(if b.get(8..12) == Some(b"qt  ") { "video/quicktime" } else { "video/mp4" });
    }
    if b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some("video/webm");
    }
    None
}

pub fn kind_of(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "image"
    } else if mime.starts_with("video/") {
        "video"
    } else if mime == "application/pdf" {
        "pdf"
    } else {
        "other"
    }
}

/// path.extname
fn extname(name: &str) -> &str {
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(0) | None => "",
        Some(i) => &base[i..],
    }
}

fn basename(p: &str) -> String {
    p.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

/// `filename.replace(/[^\w.\-]+/g, "_")`
fn safe_filename(s: &str) -> String {
    let mut out = String::new();
    let mut in_bad = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
            out.push(c);
            in_bad = false;
        } else if !in_bad {
            out.push('_');
            in_bad = true;
        }
    }
    out
}

pub fn public_media(m: &MediaRow) -> Value {
    Obj::new()
        .set("id", m.id.clone())
        .set("filename", m.filename.clone())
        .set("mime", m.mime.clone())
        .set("size", m.size)
        .set("storage", m.storage.clone())
        .set("url", m.url.clone())
        .set("alt", m.alt.clone())
        .build()
}

pub fn local_media_path(key: &str) -> PathBuf {
    crate::tenant::media_dir().join(key)
}

/// Copy a reader to a file, returning (bytes, first 16 bytes, sha256 hex).
fn copy_hashing(r: impl Read, path: &Path) -> Result<(u64, Vec<u8>, String)> {
    copy_limited(r, path, None)
}

/// Like copy_hashing, but stops with `limit.1` as the error once more than `limit.0` bytes arrive.
fn copy_limited(mut r: impl Read, path: &Path, limit: Option<&(u64, String)>) -> Result<(u64, Vec<u8>, String)> {
    let mut f = File::create(path)?;
    let mut hasher = Sha256::new();
    let mut head = Vec::with_capacity(16);
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        let k = r.read(&mut buf)?;
        if k == 0 {
            break;
        }
        if head.len() < 16 {
            head.extend_from_slice(&buf[..k.min(16 - head.len())]);
        }
        hasher.update(&buf[..k]);
        f.write_all(&buf[..k])?;
        n += k as u64;
        if let Some((max, msg)) = limit {
            if n > *max {
                bail!("{msg}");
            }
        }
    }
    f.sync_all()?;
    Ok((n, head, hex(&hasher.finalize())))
}

/// GET a URL for staging. Hosted mode checks every hop, so redirects cannot reach local addresses.
fn fetch_for_staging(url: &str) -> Result<net::Resp> {
    if !crate::tenant::hosted() {
        return net::get(url).timeout_secs(300).send();
    }
    let mut url = url.to_string();
    for _ in 0..5 {
        crate::limits::check_public_url(&url)?;
        let res = net::get(&url).no_redirects().timeout_secs(300).send()?;
        if !(300..400).contains(&res.status) {
            return Ok(res);
        }
        let Some(loc) = res.header("location") else {
            bail!("Redirect without a location from {url}")
        };
        url = if loc.starts_with("http://") || loc.starts_with("https://") {
            loc
        } else {
            let (host, _) = crate::util::host_path(&url);
            let scheme = if url.starts_with("https") { "https" } else { "http" };
            format!("{scheme}://{host}{}{loc}", if loc.starts_with('/') { "" } else { "/" })
        };
    }
    bail!("Too many redirects")
}

pub fn stage_media(args: &Value) -> Result<MediaRow> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty()).map(|v| v.to_string());
    let mut filename = s("filename");
    if crate::tenant::hosted() && s("path").is_some() && s("url").is_none() {
        bail!("path is not available on the hosted service (it would read the server's disk); give url or base64");
    }
    let allowance = crate::limits::upload_allowance()?;
    let id = new_id("med");
    let dir = crate::tenant::media_dir().join(&id);
    fs::create_dir_all(&dir)?;
    let tmp = dir.join(".incoming");
    let mut header_mime: Option<String> = None;
    let staged = (|| -> Result<(u64, Vec<u8>, String)> {
        if let Some(url) = s("url") {
            let res = fetch_for_staging(&url)?;
            header_mime = res.header("content-type").map(|c| c.split(';').next().unwrap_or("").trim().to_string());
            if filename.is_none() {
                let path = crate::util::host_path(&url).1.to_string();
                let b = basename(&path);
                filename = Some(if b.is_empty() { "file".into() } else { b });
            }
            copy_limited(res.into_reader(), &tmp, allowance.as_ref())
        } else if let Some(path) = s("path") {
            let f = match File::open(&path) {
                Ok(f) => f,
                Err(_) => bail!("File not found: {path}"),
            };
            if filename.is_none() {
                filename = Some(basename(&path));
            }
            copy_limited(f, &tmp, allowance.as_ref())
        } else if let Some(b64) = s("base64") {
            let cleaned: String = b64.chars().filter(|c| !c.is_whitespace() && *c != '=').collect();
            let bytes = base64::engine::general_purpose::STANDARD_NO_PAD
                .decode(cleaned.replace('-', "+").replace('_', "/"))
                .map_err(|e| crate::util::Error::Msg(format!("Invalid base64: {e}")))?;
            if filename.is_none() {
                filename = Some("upload".into());
            }
            copy_limited(bytes.as_slice(), &tmp, allowance.as_ref())
        } else {
            bail!("Give one of url, path or base64")
        }
    })();
    let (size, head, sha) = match staged {
        Ok(x) => x,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
    };
    let mut filename = filename.unwrap_or_else(|| "file".into());
    let mime = sniff(&head)
        .map(|m| m.to_string())
        .or_else(|| mime_for_ext(&extname(&filename).to_lowercase()).map(|m| m.to_string()))
        .or_else(|| header_mime.filter(|m| !m.is_empty() && m != "application/octet-stream"));
    let Some(mime) = mime else {
        let _ = fs::remove_dir_all(&dir);
        bail!("Unsupported media type for {filename}");
    };
    if extname(&filename).is_empty() {
        filename.push_str(ext_for_mime(&mime));
    }
    let filename = safe_filename(&filename);
    let key = format!("{id}/{filename}");
    let local = crate::tenant::media_dir().join(&key);
    fs::rename(&tmp, &local)?;
    let (storage, url) = if r2_enabled() {
        let r = &config().r2;
        let bucket = r.bucket.as_deref().unwrap_or_default();
        let object = format!("{}{key}", crate::tenant::r2_prefix());
        let (put_url, headers) = sign_s3("PUT", &format!("/{bucket}/{object}"), &sha, &[("content-type", mime.clone())]);
        let put = net::req("PUT", put_url)
            .headers(&headers)
            .body(Body::File {
                path: local.clone(),
                offset: 0,
                len: size,
            })
            .timeout_secs(600)
            .send();
        if let Err(e) = put {
            let _ = fs::remove_dir_all(&dir);
            return Err(e);
        }
        ("r2", format!("{}/{object}", r.public_url.as_deref().unwrap_or_default()))
    } else {
        ("local", format!("{}/{key}", crate::tenant::media_url_prefix()))
    };
    let row = MediaRow {
        id: id.clone(),
        filename,
        mime,
        size: size as i64,
        storage: storage.into(),
        key,
        url,
        alt: s("alt"),
        created_at: now(),
    };
    db::with(|c| {
        c.execute(
            "INSERT INTO media (id, filename, mime, size, storage, key, url, alt, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params![row.id, row.filename, row.mime, row.size, row.storage, row.key, row.url, row.alt, row.created_at],
        )?;
        Ok(())
    })?;
    Ok(row)
}

pub fn get_media(id: &str) -> Result<MediaRow> {
    let r = db::with(|c| db::opt_row(c, &format!("SELECT {} FROM media WHERE id = ?", MediaRow::COLS), [id], MediaRow::from))?;
    match r {
        Some(m) => Ok(m),
        None => bail!("Unknown media id {id}"),
    }
}

pub fn list_media(limit: i64) -> Result<Vec<Value>> {
    let rows = db::with(|c| {
        db::rows(
            c,
            &format!("SELECT {} FROM media ORDER BY created_at DESC LIMIT ?", MediaRow::COLS),
            [limit],
            MediaRow::from,
        )
    })?;
    Ok(rows.iter().map(public_media).collect())
}

pub fn delete_media(id: &str) -> Result<()> {
    let m = get_media(id)?;
    if m.storage == "r2" && r2_enabled() {
        let bucket = config().r2.bucket.clone().unwrap_or_default();
        let empty = hex(&Sha256::digest(b""));
        let (url, headers) = sign_s3("DELETE", &format!("/{bucket}/{}{}", crate::tenant::r2_prefix(), m.key), &empty, &[]);
        let _ = net::req("DELETE", url).headers(&headers).send();
    }
    let _ = fs::remove_dir_all(crate::tenant::media_dir().join(&m.id));
    db::with(|c| {
        c.execute("DELETE FROM media WHERE id = ?", [id])?;
        Ok(())
    })
}

/// A staged file on disk for uploading. Falls back to downloading the public URL into a
/// temporary file (deleted on drop) when the local copy is gone.
pub struct LocalFile {
    pub path: PathBuf,
    pub len: u64,
    temp: bool,
}

impl Drop for LocalFile {
    fn drop(&mut self) {
        if self.temp {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub fn local_file(m: &MediaRow) -> Result<LocalFile> {
    let p = crate::tenant::media_dir().join(&m.key);
    if let Ok(md) = fs::metadata(&p) {
        if md.is_file() {
            return Ok(LocalFile {
                path: p,
                len: md.len(),
                temp: false,
            });
        }
    }
    let tmp = config().data_dir.join(format!(".fetch-{}", new_id("tmp")));
    let res = net::get(&m.url).timeout_secs(600).send()?;
    let (len, _, _) = copy_hashing(res.into_reader(), &tmp)?;
    Ok(LocalFile { path: tmp, len, temp: true })
}

/// Small files (thumbnails) as bytes.
pub fn media_bytes(m: &MediaRow) -> Result<Vec<u8>> {
    let f = local_file(m)?;
    Ok(fs::read(&f.path)?)
}

/// Platforms that fetch media themselves need a URL they can reach.
pub fn assert_public_url(m: &MediaRow, platform: &str) -> Result<()> {
    let host = crate::util::host_of(&m.url);
    if host == "localhost" || host == "127.0.0.1" || host.ends_with(".local") {
        bail!(
            "{platform} downloads media from a public URL, but {} is local. Configure R2 or set SIAR_PUBLIC_URL to a public address.",
            m.url
        );
    }
    Ok(())
}

// ---------- SigV4 for R2 ----------

type HmacSha256 = Hmac<Sha256>;

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac takes any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// Sign an S3 request for the R2 endpoint. Returns the URL and headers to send.
pub fn sign_s3(method: &str, path: &str, payload_sha256: &str, extra: &[(&str, String)]) -> (String, Vec<(&'static str, String)>) {
    let r = &config().r2;
    let host = format!("{}.r2.cloudflarestorage.com", r.account_id.as_deref().unwrap_or_default());
    sign_s3_with(
        &host,
        r.access_key_id.as_deref().unwrap_or_default(),
        r.secret_access_key.as_deref().unwrap_or_default(),
        "auto",
        &iso(now()),
        method,
        path,
        payload_sha256,
        extra,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn sign_s3_with(
    host: &str,
    access_key: &str,
    secret: &str,
    region: &str,
    iso_now: &str,
    method: &str,
    path: &str,
    payload_sha256: &str,
    extra: &[(&str, String)],
) -> (String, Vec<(&'static str, String)>) {
    // 2026-11-17T01:02:03.456Z -> 20261117T010203Z
    let amz_date: String = iso_now.chars().filter(|c| *c != '-' && *c != ':').collect::<String>();
    let amz_date = format!("{}Z", &amz_date[..amz_date.find('.').unwrap_or(amz_date.len() - 1)]);
    let date = &amz_date[..8];
    let mut headers: Vec<(String, String)> = vec![
        ("host".into(), host.to_string()),
        ("x-amz-date".into(), amz_date.clone()),
        ("x-amz-content-sha256".into(), payload_sha256.to_string()),
    ];
    for (k, v) in extra {
        headers.push((k.to_lowercase(), v.clone()));
    }
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: String = headers.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical_path = path.split('/').map(|s| uri_component(&percent_decode(s))).collect::<Vec<_>>().join("/");
    let canonical = [method, &canonical_path, "", &canonical_headers, &signed, payload_sha256].join("\n");
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = ["AWS4-HMAC-SHA256", &amz_date, &scope, &hex(&Sha256::digest(canonical.as_bytes()))].join("\n");
    let k_date = hmac(format!("AWS4{secret}").as_bytes(), date);
    let k_signing = hmac(&hmac(&hmac(&k_date, region), "s3"), "aws4_request");
    let signature = hex(&hmac(&k_signing, &to_sign));
    let mut out: Vec<(&'static str, String)> = vec![
        ("x-amz-date", amz_date),
        ("x-amz-content-sha256", payload_sha256.to_string()),
        (
            "authorization",
            format!("AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={signature}"),
        ),
    ];
    for (k, v) in extra {
        if k.eq_ignore_ascii_case("content-type") {
            out.push(("content-type", v.clone()));
        }
    }
    (format!("https://{host}{}", canonical_path), out)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_types() {
        assert_eq!(sniff(&[0xff, 0xd8, 0xff]), Some("image/jpeg"));
        assert_eq!(sniff(b"\0\0\0\x18ftypqt  "), Some("video/quicktime"));
        assert_eq!(sniff(b"\0\0\0\x18ftypisom"), Some("video/mp4"));
        assert_eq!(safe_filename("my photo (1).jpg"), "my_photo_1_.jpg");
        assert_eq!(extname("a/b.tar.gz"), ".gz");
        assert_eq!(extname(".hidden"), "");
    }

    #[test]
    fn sigv4_matches_the_ts_signer() {
        let f: Value = serde_json::from_str(include_str!("meta/sigv4-fixture.json")).unwrap();
        let (url, h) = sign_s3_with(
            "acc.r2.cloudflarestorage.com",
            "AKID",
            "SECRET",
            "auto",
            "2026-11-17T01:02:03.456Z",
            "PUT",
            "/bucket/med_x/a b.jpg",
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            &[("content-type", "image/jpeg".into())],
        );
        assert_eq!(url, f["url"].as_str().unwrap());
        for (k, v) in h {
            assert_eq!(v, f["headers"][k].as_str().unwrap(), "{k}");
        }
    }
}
