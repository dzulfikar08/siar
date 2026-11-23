//! Nostr and Farcaster.
//! BIP-340 signatures come from k256 (RustCrypto); relays are spoken to over a small
//! blocking WebSocket client on rustls.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{opt_string, or_zero, put, Published};
use crate::accounts::{Connected, Ctx};
use crate::media::assert_public_url;
use crate::net;
use crate::util::{bail, hashtags, hex, js_string, now, permanent, random_bytes, str_of, truthy, Error, Result};

// ---------- BIP-340 (k256) and bech32 (NIP-19) ----------

fn unhex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("Invalid hex");
    }
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).map_err(|_| Error::Msg("Invalid hex".into())))
        .collect()
}

pub fn get_public_key(priv_hex: &str) -> Result<String> {
    let sk = k256::schnorr::SigningKey::from_bytes(&unhex(priv_hex)?).map_err(|_| Error::Msg("Invalid private key".into()))?;
    Ok(hex(&sk.verifying_key().to_bytes()))
}

pub fn sign(msg: &[u8], priv_hex: &str) -> Result<String> {
    let sk = k256::schnorr::SigningKey::from_bytes(&unhex(priv_hex)?).map_err(|_| Error::Msg("Invalid private key".into()))?;
    let mut aux = [0u8; 32];
    aux.copy_from_slice(&random_bytes(32));
    let sig = sk.sign_raw(msg, &aux).map_err(|_| Error::Msg("Bad nonce".into()))?;
    Ok(hex(&sig.to_bytes()))
}

pub fn verify(sig_hex: &str, msg: &[u8], pub_hex: &str) -> bool {
    let (Ok(sig), Ok(pk)) = (unhex(sig_hex), unhex(pub_hex)) else { return false };
    let Ok(vk) = k256::schnorr::VerifyingKey::from_bytes(&pk) else { return false };
    let Ok(sig) = k256::schnorr::Signature::try_from(sig.as_slice()) else {
        return false;
    };
    vk.verify_raw(msg, &sig).is_ok()
}

const CHARSET: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";

fn polymod(values: &[u8]) -> u32 {
    const GEN: [u32; 5] = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3];
    let mut chk: u32 = 1;
    for v in values {
        let b = chk >> 25;
        chk = ((chk & 0x1ffffff) << 5) ^ (*v as u32);
        for (i, g) in GEN.iter().enumerate() {
            if (b >> i) & 1 == 1 {
                chk ^= g;
            }
        }
    }
    chk
}

fn hrp_expand(h: &str) -> Vec<u8> {
    let mut v: Vec<u8> = h.bytes().map(|c| c >> 5).collect();
    v.push(0);
    v.extend(h.bytes().map(|c| c & 31));
    v
}

fn convert(data: &[u8], from: u32, to: u32, pad: bool) -> Vec<u8> {
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut out = vec![];
    let maxv = (1u32 << to) - 1;
    for v in data {
        acc = (acc << from) | *v as u32;
        bits += from;
        while bits >= to {
            bits -= to;
            out.push(((acc >> bits) & maxv) as u8);
        }
    }
    if pad && bits > 0 {
        out.push(((acc << (to - bits)) & maxv) as u8);
    }
    out
}

pub fn bech32_decode(s: &str) -> Result<(String, String)> {
    let s = s.to_lowercase();
    let Some(pos) = s.rfind('1') else { bail!("Invalid bech32 string") };
    let hrp = &s[..pos];
    let data: Option<Vec<u8>> = s[pos + 1..].bytes().map(|c| CHARSET.iter().position(|x| *x == c).map(|p| p as u8)).collect();
    let Some(data) = data else { bail!("Invalid bech32 string") };
    let mut chk = hrp_expand(hrp);
    chk.extend(&data);
    if data.len() < 6 || polymod(&chk) != 1 {
        bail!("Invalid bech32 string");
    }
    Ok((hrp.to_string(), hex(&convert(&data[..data.len() - 6], 5, 8, false))))
}

pub fn bech32_encode(hrp: &str, hex_str: &str) -> Result<String> {
    let data = convert(&unhex(hex_str)?, 8, 5, true);
    let mut v = hrp_expand(hrp);
    v.extend(&data);
    v.extend([0u8; 6]);
    let m = polymod(&v) ^ 1;
    let chk: Vec<u8> = (0..6).map(|i| ((m >> (5 * (5 - i))) & 31) as u8).collect();
    let body: String = data.iter().chain(chk.iter()).map(|d| CHARSET[*d as usize] as char).collect();
    Ok(format!("{hrp}1{body}"))
}

// ---------- minimal WebSocket client (relays) ----------

trait Stream: Read + Write + Send {}
impl<T: Read + Write + Send> Stream for T {}

fn ws_connect(url: &str, deadline: Instant) -> Result<Box<dyn Stream>> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("ws://") {
        (false, r)
    } else {
        bail!("connection error");
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse::<u16>().unwrap_or(443)),
        _ => (authority, if tls { 443 } else { 80 }),
    };
    let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))?
        .next()
        .ok_or_else(|| Error::Msg("connection error".into()))?;
    let tcp = TcpStream::connect_timeout(&addr, left)?;
    tcp.set_read_timeout(Some(left))?;
    tcp.set_write_timeout(Some(left))?;
    let mut stream: Box<dyn Stream> = if tls {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::Msg(e.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|e| Error::Msg(e.to_string()))?;
        let conn = rustls::ClientConnection::new(Arc::new(config), name).map_err(|e| Error::Msg(e.to_string()))?;
        Box::new(rustls::StreamOwned::new(conn, tcp))
    } else {
        Box::new(tcp)
    };
    use base64::Engine;
    let key = base64::engine::general_purpose::STANDARD.encode(random_bytes(16));
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")?;
    stream.flush()?;
    let mut head = vec![];
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut b)? == 0 || head.len() > 16_384 {
            bail!("connection error");
        }
        head.push(b[0]);
    }
    let status = String::from_utf8_lossy(&head);
    if !status.starts_with("HTTP/1.1 101") && !status.starts_with("HTTP/1.0 101") {
        bail!("connection error");
    }
    Ok(stream)
}

fn ws_send(s: &mut dyn Stream, opcode: u8, payload: &[u8]) -> Result<()> {
    let mut frame = vec![0x80 | opcode];
    let n = payload.len();
    if n < 126 {
        frame.push(0x80 | n as u8);
    } else if n < 65536 {
        frame.push(0x80 | 126);
        frame.extend((n as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend((n as u64).to_be_bytes());
    }
    let mask = random_bytes(4);
    frame.extend(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    s.write_all(&frame)?;
    s.flush()?;
    Ok(())
}

/// One frame: (opcode, payload).
fn ws_read(s: &mut dyn Stream) -> Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    s.read_exact(&mut h)?;
    let mut len = (h[1] & 0x7f) as u64;
    if len == 126 {
        let mut b = [0u8; 2];
        s.read_exact(&mut b)?;
        len = u16::from_be_bytes(b) as u64;
    } else if len == 127 {
        let mut b = [0u8; 8];
        s.read_exact(&mut b)?;
        len = u64::from_be_bytes(b);
    }
    if len > 4 * 1024 * 1024 {
        bail!("frame too large");
    }
    let mask = if h[1] & 0x80 != 0 {
        let mut m = [0u8; 4];
        s.read_exact(&mut m)?;
        Some(m)
    } else {
        None
    };
    let mut payload = vec![0u8; len as usize];
    s.read_exact(&mut payload)?;
    if let Some(m) = mask {
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= m[i % 4];
        }
    }
    Ok((h[0] & 0x0f, payload))
}

/// None when the relay accepted the event, else the reason (like the TS sendToRelay).
fn send_to_relay(url: &str, ev: &Value) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let id = ev.get("id").cloned().unwrap_or_default();
    let run = || -> std::result::Result<Option<String>, &'static str> {
        let mut s = ws_connect(url, deadline).map_err(|_| "connection error")?;
        ws_send(s.as_mut(), 1, json!(["EVENT", ev]).to_string().as_bytes()).map_err(|_| "connection error")?;
        loop {
            if Instant::now() > deadline {
                return Err("timeout");
            }
            let (op, payload) = ws_read(s.as_mut()).map_err(|e| match e {
                Error::Msg(m) if m.contains("timed out") || m.contains("Resource temporarily unavailable") || m.contains("would block") => "timeout",
                _ => "connection error",
            })?;
            match op {
                0x8 => return Err("connection error"),
                0x9 => {
                    let _ = ws_send(s.as_mut(), 0xA, &payload);
                }
                0x1 => {
                    if let Ok(Value::Array(a)) = serde_json::from_slice::<Value>(&payload) {
                        if a.first().and_then(|t| t.as_str()) == Some("OK") && a.get(1) == Some(&id) {
                            let _ = ws_send(s.as_mut(), 0x8, &[]);
                            return Ok(if truthy(a.get(2)) {
                                None
                            } else {
                                Some(a.get(3).map(js_string).unwrap_or_else(|| "undefined".into()))
                            });
                        }
                    }
                }
                _ => {}
            }
        }
    };
    match run() {
        Ok(r) => r,
        Err(e) => Some(e.to_string()),
    }
}

fn broadcast(relays: &[String], ev: &Value) -> Result<Vec<String>> {
    let results: Vec<(String, Option<String>)> = std::thread::scope(|s| {
        let hs: Vec<_> = relays.iter().map(|r| s.spawn(move || (r.clone(), send_to_relay(r, ev)))).collect();
        hs.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let ok: Vec<String> = results.iter().filter(|(_, e)| e.is_none()).map(|(r, _)| r.clone()).collect();
    if ok.is_empty() {
        let why: Vec<String> = results.iter().map(|(r, e)| format!("{r}: {}", e.clone().unwrap_or_default())).collect();
        bail!("No relay accepted the event: {}", why.join("; "));
    }
    Ok(ok)
}

// ---------- Nostr ----------

const DEFAULT_RELAYS: [&str; 4] = ["wss://relay.damus.io", "wss://nos.lol", "wss://relay.primal.net", "wss://relay.nostr.band"];

fn priv_hex(key: &str) -> Result<String> {
    let key = key.trim();
    if key.starts_with("nsec1") {
        return Ok(bech32_decode(key)?.1);
    }
    if key.len() == 64 && key.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Ok(key.to_lowercase());
    }
    bail!("Give the private key as nsec1… or 64 hex characters")
}

pub fn sign_event(priv_key: &str, kind: i64, content: &str, tags: &Value) -> Result<Value> {
    let pubkey = get_public_key(priv_key)?;
    let created_at = now() / 1000;
    let id = hex(&Sha256::digest(json!([0, pubkey, created_at, kind, tags, content]).to_string().as_bytes()));
    let sig = sign(&unhex(&id)?, priv_key)?;
    Ok(json!({ "id": id, "pubkey": pubkey, "created_at": created_at, "kind": kind, "tags": tags, "content": content, "sig": sig }))
}

pub fn nostr_verify(f: &Map<String, Value>) -> Result<Vec<Connected>> {
    let priv_key = priv_hex(&f.get("private_key").map(js_string).unwrap_or_default())?;
    let public = get_public_key(&priv_key)?;
    let relays_raw = f
        .get("relays")
        .map(js_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RELAYS.join(","));
    let relays: Vec<String> = relays_raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let npub = bech32_encode("npub", &public)?;
    let short: String = npub.chars().take(16).collect();
    Ok(vec![Connected {
        external_id: public,
        name: format!("{short}…"),
        handle: Some(npub),
        secret: json!({ "private_key": priv_key, "relays": relays }),
        meta: None,
        expires_at: None,
    }])
}

fn relays_of(ctx: &Ctx) -> Vec<String> {
    ctx.secret
        .get("relays")
        .and_then(|r| r.as_array())
        .map(|a| a.iter().map(js_string).collect())
        .unwrap_or_default()
}

pub fn nostr_publish(ctx: &Ctx) -> Result<Published> {
    let priv_key = str_of(&ctx.secret, "private_key").unwrap_or_default().to_string();
    let relays = relays_of(ctx);
    for m in &ctx.media {
        assert_public_url(m, "Nostr")?;
    }
    let mut tags: Vec<Value> = ctx
        .opt("hashtags")
        .and_then(|h| h.as_array())
        .into_iter()
        .flatten()
        .map(|t| {
            let t = js_string(t);
            json!(["t", t.strip_prefix('#').unwrap_or(&t).to_lowercase()])
        })
        .collect();
    for (_, tag) in hashtags(&ctx.text) {
        tags.push(json!(["t", tag.to_lowercase()]));
    }
    for m in &ctx.media {
        let mut im = vec![
            Value::from("imeta"),
            Value::from(format!("url {}", m.url)),
            Value::from(format!("m {}", m.mime)),
        ];
        if let Some(alt) = m.alt.as_ref().filter(|a| !a.is_empty()) {
            im.push(Value::from(format!("alt {alt}")));
        }
        tags.push(Value::Array(im));
    }
    let content: Vec<String> = std::iter::once(ctx.text.clone())
        .chain(ctx.media.iter().map(|m| m.url.clone()))
        .filter(|s| !s.is_empty())
        .collect();
    let root = sign_event(&priv_key, 1, &content.join("\n\n"), &Value::Array(tags))?;
    broadcast(&relays, &root)?;
    let root_id = root.get("id").map(js_string).unwrap_or_default();
    let mut parent = root_id.clone();
    let first_relay = relays.first().cloned().unwrap_or_else(|| "undefined".into());
    for part in ctx.opt("thread").and_then(|t| t.as_array()).into_iter().flatten() {
        let tags = json!([
            ["e", root_id, first_relay, "root"],
            ["e", parent, first_relay, "reply"],
            ["p", root.get("pubkey")]
        ]);
        let reply = sign_event(&priv_key, 1, &js_string(part), &tags)?;
        broadcast(&relays, &reply)?;
        parent = reply.get("id").map(js_string).unwrap_or_default();
    }
    Ok(Published {
        url: Some(format!("https://njump.me/{}", bech32_encode("note", &root_id)?)),
        id: root_id,
    })
}

pub fn nostr_remove(ctx: &Ctx, id: &str) -> Result<()> {
    let ev = sign_event(str_of(&ctx.secret, "private_key").unwrap_or_default(), 5, "", &json!([["e", id]]))?;
    broadcast(&relays_of(ctx), &ev)?;
    Ok(())
}

// ---------- Farcaster (Neynar managed signers) ----------

const NEYNAR: &str = "https://api.neynar.com/v2/farcaster";

pub fn farcaster_verify(f: &Map<String, Value>) -> Result<Vec<Connected>> {
    let key = f.get("api_key").map(js_string).unwrap_or_default();
    let signer = net::get(format!("{NEYNAR}/signer"))
        .header("x-api-key", key.clone())
        .query(&[("signer_uuid", f.get("signer_uuid").map(js_string))])
        .get_json()?;
    if str_of(&signer, "status") != Some("approved") {
        bail!(
            "Signer status is {}, it must be approved",
            signer.get("status").map(js_string).unwrap_or_else(|| "undefined".into())
        );
    }
    let fid = signer.get("fid").map(js_string).unwrap_or_default();
    let users = net::get(format!("{NEYNAR}/user/bulk"))
        .header("x-api-key", key)
        .query(&[("fids", Some(fid.clone()))])
        .get_json()?;
    let u = users.pointer("/users/0").cloned().unwrap_or_default();
    Ok(vec![Connected {
        external_id: fid,
        name: opt_string(u.get("display_name")).or_else(|| opt_string(u.get("username"))).unwrap_or_default(),
        handle: opt_string(u.get("username")),
        secret: json!({ "api_key": f.get("api_key"), "signer_uuid": f.get("signer_uuid") }),
        meta: None,
        expires_at: None,
    }])
}

pub fn farcaster_publish(ctx: &Ctx) -> Result<Published> {
    let key = str_of(&ctx.secret, "api_key").unwrap_or_default().to_string();
    let signer = ctx.secret.get("signer_uuid").cloned().unwrap_or_default();
    for m in &ctx.media {
        assert_public_url(m, "Farcaster")?;
    }
    let mut urls: Vec<Value> = ctx.media.iter().map(|m| Value::from(m.url.clone())).collect();
    urls.extend(ctx.opt("embeds").and_then(|e| e.as_array()).cloned().unwrap_or_default());
    let embeds: Vec<Value> = urls.into_iter().take(2).map(|u| json!({ "url": u })).collect();
    let cast = |text: &Value, extra: Map<String, Value>| -> Result<Value> {
        let mut body = Map::new();
        body.insert("signer_uuid".into(), signer.clone());
        body.insert("text".into(), text.clone());
        for (k, v) in extra {
            put(&mut body, &k, Some(v));
        }
        Ok(net::post(format!("{NEYNAR}/cast"))
            .header("x-api-key", key.clone())
            .json(Value::Object(body))
            .get_json()?
            .get("cast")
            .cloned()
            .unwrap_or(Value::Null))
    };
    let mut extra = Map::new();
    extra.insert("embeds".into(), Value::Array(embeds));
    extra.insert("channel_id".into(), ctx.opt("channel_id").cloned().unwrap_or(Value::Null));
    let first = cast(&Value::from(ctx.text.clone()), extra)?;
    if !truthy(first.get("hash")) {
        permanent!("Neynar returned no cast hash");
    }
    let hash = js_string(&first["hash"]);
    let mut parent = hash.clone();
    for part in ctx.opt("thread").and_then(|t| t.as_array()).into_iter().flatten() {
        let mut extra = Map::new();
        extra.insert("parent".into(), Value::from(parent.clone()));
        parent = cast(part, extra)?.get("hash").map(js_string).unwrap_or_else(|| "undefined".into());
    }
    let short: String = hash.chars().take(10).collect();
    Ok(Published {
        url: Some(format!("https://farcaster.xyz/{}/{short}", ctx.handle.clone().unwrap_or_else(|| "null".into()))),
        id: hash,
    })
}

pub fn farcaster_remove(ctx: &Ctx, id: &str) -> Result<()> {
    net::req("DELETE", format!("{NEYNAR}/cast"))
        .header("x-api-key", str_of(&ctx.secret, "api_key").unwrap_or_default())
        .json(json!({ "signer_uuid": ctx.secret.get("signer_uuid"), "target_hash": id }))
        .get_json()?;
    Ok(())
}

pub fn farcaster_metrics(ctx: &Ctx, id: &str) -> Result<Value> {
    let r = net::get(format!("{NEYNAR}/cast"))
        .header("x-api-key", str_of(&ctx.secret, "api_key").unwrap_or_default())
        .query(&[("identifier", Some(id.into())), ("type", Some("hash".into()))])
        .get_json()?;
    let c = r.get("cast").cloned().unwrap_or_default();
    Ok(json!({
        "likes": or_zero(c.pointer("/reactions/likes_count")),
        "recasts": or_zero(c.pointer("/reactions/recasts_count")),
        "replies": or_zero(c.pointer("/replies/count")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bip340_vector_0_and_own_signatures() {
        let sk = "0000000000000000000000000000000000000000000000000000000000000003";
        let pk = get_public_key(sk).unwrap();
        assert_eq!(pk.to_uppercase(), "F9308A019258C31049344F85F89D5229B531C845836F99B08601F113BCE036F9");
        let msg = [0u8; 32];
        let vec0 =
            "E907831F80848D1069A5371B402410364BDF1C5F8307B0084C55F1CE2DCA821525F66A4A85EA8B71E482A74F382D2CE5EBEEE8FDB2172F477DF4900D310536C0".to_lowercase();
        assert!(verify(&vec0, &msg, &pk));
        let sig = sign(&msg, sk).unwrap();
        assert!(verify(&sig, &msg, &pk));
        let mut other = msg;
        other[0] = 1;
        assert!(!verify(&sig, &other, &pk));
    }

    #[test]
    fn bech32_npub_round_trip() {
        let hex_key = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e";
        let npub = bech32_encode("npub", hex_key).unwrap();
        assert_eq!(npub, "npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg");
        assert_eq!(bech32_decode(&npub).unwrap().1, hex_key);
    }

    #[test]
    fn signed_event_verifies() {
        let sk = "0000000000000000000000000000000000000000000000000000000000000003";
        let ev = sign_event(sk, 1, "hello \"nostr\"", &json!([["t", "rust"]])).unwrap();
        let id = ev["id"].as_str().unwrap();
        let expect = hex(&Sha256::digest(
            json!([0, ev["pubkey"], ev["created_at"], 1, [["t", "rust"]], "hello \"nostr\""])
                .to_string()
                .as_bytes(),
        ));
        assert_eq!(id, expect);
        assert!(verify(ev["sig"].as_str().unwrap(), &unhex(id).unwrap(), ev["pubkey"].as_str().unwrap()));
    }
}
