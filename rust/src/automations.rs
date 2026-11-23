//! Automations: comment replies, comment-to-DM, DM keywords, story replies and
//! mentions, default replies; human-like delays through the jobs table, follow gate, click tracking,
//! polling and Meta webhooks.

use base64::Engine;
use hmac::{Hmac, Mac};
use rusqlite::params;
use serde_json::{json, Map, Value};
use sha2::Sha256;

use crate::accounts::{ctx_for, get_account};
use crate::config::{config, env};
use crate::db::{self, AccountRow};
use crate::insights::{store_comments, sync_comments};
use crate::platforms;
use crate::providers::{self, Button, DmPayload, RemoteComment};
use crate::util::{bail, is_tag_char, iso, js_string, json_or, new_id, now, random_bytes, to_num, truthy, Error, Obj, Result};

pub const KINDS: [&str; 6] = ["comment_to_dm", "comment_reply", "dm_keyword", "story_reply", "story_mention", "default_reply"];
const COMMENT_KINDS: [&str; 2] = ["comment_to_dm", "comment_reply"];
const DM_KINDS: [&str; 5] = ["comment_to_dm", "dm_keyword", "story_reply", "story_mention", "default_reply"];

fn log(msg: &str) {
    eprintln!("{} [automations] {msg}", iso(now()));
}

#[derive(Clone)]
struct Auto {
    id: String,
    account_id: String,
    name: String,
    kind: String,
    enabled: bool,
    /// The config object as stored (key order kept).
    c: Map<String, Value>,
    stats: Map<String, Value>,
    created_at: i64,
}

impl Auto {
    fn s(&self, k: &str) -> Option<String> {
        self.c.get(k).filter(|v| !v.is_null()).map(js_string)
    }
    fn strs(&self, k: &str) -> Vec<String> {
        self.c
            .get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().map(js_string).collect())
            .unwrap_or_default()
    }
    fn enabled_at(&self) -> i64 {
        to_num(self.c.get("enabled_at")) as i64
    }
    fn follow_gate(&self) -> Option<&Value> {
        self.c.get("follow_gate").filter(|g| truthy(Some(g)))
    }
}

const COLS: &str = "id, account_id, name, kind, enabled, config, stats, created_at";

fn load(r: &rusqlite::Row) -> rusqlite::Result<Auto> {
    let config: String = r.get(5)?;
    let stats: String = r.get(6)?;
    Ok(Auto {
        id: r.get(0)?,
        account_id: r.get(1)?,
        name: r.get(2)?,
        kind: r.get(3)?,
        enabled: r.get::<_, i64>(4)? != 0,
        c: json_or(Some(&config), Value::Object(Map::new())).as_object().cloned().unwrap_or_default(),
        stats: json_or(Some(&stats), Value::Object(Map::new())).as_object().cloned().unwrap_or_default(),
        created_at: r.get(7)?,
    })
}

fn row(id: &str) -> Result<Auto> {
    match db::with(|c| db::opt_row(c, &format!("SELECT {COLS} FROM automations WHERE id = ?"), [id], load))? {
        Some(x) => Ok(x),
        None => bail!("Unknown automation {id}"),
    }
}

fn pub_json(x: &Auto) -> Result<Value> {
    let a = get_account(&x.account_id)?;
    let (n, clicks, people): (i64, i64, i64) = db::with(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) AS n, COALESCE(SUM(clicks), 0) AS clicks, COUNT(first_click_at) AS people FROM links WHERE automation_id = ?",
            [&x.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?)
    })?;
    let mut o = Obj::new()
        .set("id", x.id.clone())
        .set("account", a.label())
        .set("account_id", a.id.clone())
        .set("name", x.name.clone())
        .set("kind", x.kind.clone())
        .set("enabled", x.enabled);
    for (k, v) in &x.c {
        o.0.insert(k.clone(), v.clone());
    }
    o.0.insert("enabled_at".into(), Value::from(iso(x.enabled_at())));
    let mut stats = x.stats.clone();
    stats.insert("links_sent".into(), n.into());
    stats.insert("link_clicks".into(), clicks.into());
    stats.insert("people_clicked".into(), people.into());
    Ok(o.set("stats", Value::Object(stats)).build())
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

fn validate(platform: &str, kind: &str, c: &Map<String, Value>) -> Result<()> {
    let p = platforms::get(platform)?;
    let ig = platform.starts_with("instagram");
    let len = |k: &str| c.get(k).and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
    if !KINDS.contains(&kind) {
        bail!("kind must be one of {}", KINDS.join(", "));
    }
    if kind == "comment_reply" {
        if !p.has("comments") {
            bail!("{} has no comment API", p.name);
        }
        if !(3..=5).contains(&len("public_replies")) {
            bail!("comment_reply needs 3-5 public_replies, so the replies do not look like a bot");
        }
        if c.get("timing").and_then(|v| v.as_str()) == Some("fast") {
            bail!("timing fast is refused for comment_reply: platforms hide fast identical replies as spam");
        }
        if truthy(c.get("dm_text")) || len("buttons") > 0 || truthy(c.get("follow_gate")) || truthy(c.get("react")) {
            bail!("comment_reply sends a public reply only: no dm_text, buttons, follow_gate or react");
        }
        let max = if platform == "tiktok" { 150 } else { 500 };
        if c.get("public_replies")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .any(|r| utf16_len(&js_string(r)) > max)
        {
            bail!("public replies are at most {max} characters here");
        }
    } else {
        if !ig || !p.has("dm") {
            bail!("{kind} needs an Instagram account (DMs are Instagram-only)");
        }
        if !truthy(c.get("dm_text")) {
            bail!("dm_text is required");
        }
        if len("public_replies") > 3 {
            bail!("comment_to_dm takes up to 3 public_replies");
        }
        if kind == "dm_keyword" && (c.get("match").and_then(|v| v.as_str()) == Some("any") || len("keywords") == 0) {
            bail!("dm_keyword needs keywords (match any is not allowed)");
        }
    }
    if len("buttons") > 3 {
        bail!("At most 3 buttons");
    }
    if c.get("scope").and_then(|v| v.as_str()) == Some("post") && len("platform_post_ids") == 0 {
        bail!("scope post needs platform_post_ids");
    }
    Ok(())
}

pub fn create_automation(input: &Value) -> Result<Value> {
    let a = get_account(&input.get("account_id").map(js_string).unwrap_or_else(|| "undefined".into()))?;
    let kind = input
        .get("kind")
        .filter(|v| !v.is_null())
        .map(js_string)
        .unwrap_or_else(|| "comment_to_dm".into());
    let get = |k: &str, d: Value| input.get(k).filter(|v| !v.is_null()).cloned().unwrap_or(d);
    let mut c = Map::new();
    c.insert("keywords".into(), get("keywords", json!([])));
    c.insert("match".into(), get("match", json!("contains")));
    c.insert("scope".into(), get("scope", json!("any")));
    c.insert("platform_post_ids".into(), get("platform_post_ids", json!([])));
    c.insert("public_replies".into(), get("public_replies", json!([])));
    if let Some(v) = input.get("dm_text").filter(|v| !v.is_null()) {
        c.insert("dm_text".into(), v.clone());
    }
    c.insert("buttons".into(), get("buttons", json!([])));
    if let Some(v) = input.get("follow_gate").filter(|v| !v.is_null()) {
        c.insert("follow_gate".into(), v.clone());
    }
    c.insert("react".into(), get("react", json!(false)));
    c.insert("once_per_person".into(), get("once_per_person", json!(kind == "comment_to_dm")));
    c.insert("timing".into(), get("timing", json!("natural")));
    c.insert("cooldown_hours".into(), get("cooldown_hours", json!(24)));
    c.insert("track_clicks".into(), get("track_clicks", json!(true)));
    c.insert("enabled_at".into(), now().into());
    validate(&a.platform, &kind, &c)?;
    let id = new_id("auto");
    let name = input
        .get("name")
        .filter(|v| !v.is_null())
        .map(js_string)
        .unwrap_or_else(|| format!("{kind} {}", &iso(now())[..10]));
    let enabled = if input.get("enabled") == Some(&Value::Bool(false)) { 0 } else { 1 };
    db::with(|db| {
        db.execute(
            "INSERT INTO automations (id, account_id, name, kind, enabled, config, stats, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, '{}', ?, ?)",
            params![id, a.id, name, kind, enabled, Value::Object(c).to_string(), now(), now()],
        )?;
        Ok(())
    })?;
    pub_json(&row(&id)?)
}

pub fn get_automation(id: &str) -> Result<Value> {
    pub_json(&row(id)?)
}

pub fn list_automations(account_id: Option<&str>) -> Result<Value> {
    let rows = match account_id {
        Some(a) => {
            let acc = get_account(a)?;
            db::with(|c| {
                db::rows(
                    c,
                    &format!("SELECT {COLS} FROM automations WHERE account_id = ? ORDER BY created_at"),
                    [&acc.id],
                    load,
                )
            })?
        }
        None => db::with(|c| db::rows(c, &format!("SELECT {COLS} FROM automations ORDER BY created_at"), [], load))?,
    };
    Ok(Value::Array(rows.iter().map(pub_json).collect::<Result<Vec<_>>>()?))
}

/// `patch` holds the tool arguments (minus automation_id and kind).
pub fn update_automation(id: &str, patch: &Map<String, Value>) -> Result<Value> {
    let x = row(id)?;
    let name = patch.get("name").filter(|v| !v.is_null()).map(js_string);
    let enabled = patch.get("enabled").filter(|v| !v.is_null()).map(|v| truthy(Some(v)));
    let mut c = x.c.clone();
    for (k, v) in patch {
        if k == "name" || k == "enabled" || v.is_null() {
            continue;
        }
        c.insert(k.clone(), v.clone());
    }
    // Turning it back on only answers what arrives from now on.
    if enabled == Some(true) && !x.enabled {
        c.insert("enabled_at".into(), now().into());
    }
    validate(&get_account(&x.account_id)?.platform, &x.kind, &c)?;
    let on = enabled.unwrap_or(x.enabled);
    db::with(|db| {
        db.execute(
            "UPDATE automations SET name = ?, enabled = ?, config = ?, updated_at = ? WHERE id = ?",
            params![name.unwrap_or(x.name.clone()), on as i64, Value::Object(c).to_string(), now(), id],
        )?;
        Ok(())
    })?;
    get_automation(id)
}

pub fn delete_automation(id: &str) -> Result<Value> {
    row(id)?;
    db::with(|c| {
        c.execute("DELETE FROM automations WHERE id = ?", [id])?;
        c.execute("UPDATE jobs SET status = 'cancelled' WHERE automation_id = ? AND status = 'pending'", [id])?;
        Ok(())
    })?;
    Ok(Obj::new().set("deleted", id).build())
}

fn enabled_for(account_id: &str, kinds: &[&str]) -> Result<Vec<Auto>> {
    if !crate::limits::feature_ok("automations") {
        return Ok(vec![]);
    }
    let marks = vec!["?"; kinds.len()].join(",");
    let mut args: Vec<String> = vec![account_id.to_string()];
    args.extend(kinds.iter().map(|k| k.to_string()));
    db::with(|c| {
        db::rows(
            c,
            &format!("SELECT {COLS} FROM automations WHERE account_id = ? AND enabled = 1 AND kind IN ({marks}) ORDER BY created_at"),
            rusqlite::params_from_iter(args.iter()),
            load,
        )
    })
}

fn bump(id: &str, key: &str) -> Result<()> {
    let mut x = row(id)?;
    let cur = to_num(x.stats.get(key));
    x.stats.insert(key.into(), crate::util::jnum(cur + 1.0));
    save_stats(&x)
}

fn save_stats(x: &Auto) -> Result<()> {
    db::with(|c| {
        c.execute(
            "UPDATE automations SET stats = ? WHERE id = ?",
            params![Value::Object(x.stats.clone()).to_string(), x.id],
        )?;
        Ok(())
    })
}

fn marked(auto_id: &str, key: &str, within_ms: Option<i64>) -> Result<bool> {
    let r: Option<i64> = db::with(|c| {
        db::opt_row(
            c,
            "SELECT created_at FROM automation_marks WHERE automation_id = ? AND key = ?",
            params![auto_id, key],
            |r| r.get(0),
        )
    })?;
    Ok(match (r, within_ms) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(t), Some(w)) => now() - t < w,
    })
}

fn mark(auto_id: &str, key: &str) -> Result<()> {
    db::with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO automation_marks (automation_id, key, created_at) VALUES (?, ?, ?)",
            params![auto_id, key, now()],
        )?;
        Ok(())
    })
}

fn unmark(auto_id: &str, key: &str) -> Result<()> {
    db::with(|c| {
        c.execute("DELETE FROM automation_marks WHERE automation_id = ? AND key = ?", params![auto_id, key])?;
        Ok(())
    })
}

/// Keyword match: exact, or the keyword as a word (`(^|[^\p{L}\p{N}_])kw($|[^\p{L}\p{N}_])`, u flag).
pub fn matches(keywords: &[String], how: &str, text: Option<&str>) -> bool {
    if how == "any" || keywords.is_empty() {
        return true;
    }
    let t = text.unwrap_or("").trim().to_lowercase();
    keywords.iter().any(|k| {
        let kw = k.trim().to_lowercase();
        if how == "exact" {
            return t == kw;
        }
        let mut from = 0;
        while from <= t.len() {
            let Some(pos) = t[from..].find(&kw).map(|p| p + from) else { return false };
            let before = t[..pos].chars().next_back();
            let after = t[pos + kw.len()..].chars().next();
            if before.map(|c| !is_tag_char(c)).unwrap_or(true) && after.map(|c| !is_tag_char(c)).unwrap_or(true) {
                return true;
            }
            // Next candidate: one character further.
            from = pos + t[pos..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
        false
    })
}

fn random() -> f64 {
    let b = random_bytes(8);
    (u64::from_le_bytes(b.try_into().unwrap_or([0; 8])) >> 11) as f64 / (1u64 << 53) as f64
}

/// Math.round(min + Math.random() * (max - min))
fn rand(min: i64, max: i64) -> i64 {
    (min as f64 + random() * (max - min) as f64).round() as i64
}

/// (first action delay, DM after a public reply), ms.
fn delays(timing: &str) -> (i64, i64) {
    match timing {
        "fast" => (rand(5, 30) * 1000, rand(1, 3) * 1000),
        "relaxed" => (rand(120, 480) * 1000, rand(60, 180) * 1000),
        _ => (rand(30, 120) * 1000, rand(20, 60) * 1000),
    }
}

fn pick_reply(x: &mut Auto) -> Result<Option<String>> {
    let replies = x.strs("public_replies");
    let n = replies.len();
    if n == 0 {
        return Ok(None);
    }
    let last = x
        .stats
        .get("last_reply_index")
        .filter(|v| !v.is_null())
        .map(|v| to_num(Some(v)) as i64)
        .unwrap_or(-1);
    let mut i = (random() * n as f64).floor() as usize;
    if n > 1 && i as i64 == last {
        i = (i + 1 + (random() * (n - 1) as f64).floor() as usize) % n;
    }
    x.stats.insert("last_reply_index".into(), i.into());
    save_stats(x)?;
    Ok(Some(replies[i].clone()))
}

fn tracked_buttons(x: &Auto, recipient: &str) -> Result<Vec<Button>> {
    let base = &config().public_url;
    let local = base.starts_with("http://localhost") || base.starts_with("https://localhost") || {
        let rest = base.split_once("://").map(|r| r.1).unwrap_or("");
        base.starts_with("http") && (rest.starts_with("127.") || rest.starts_with("[::1]"))
    };
    let track = truthy(x.c.get("track_clicks"));
    let mut out = vec![];
    for b in x.c.get("buttons").and_then(|v| v.as_array()).into_iter().flatten() {
        let title = b.get("title").map(js_string).unwrap_or_default();
        let url = b.get("url").map(js_string).unwrap_or_default();
        if !track || local {
            out.push(Button {
                title,
                url: Some(url),
                payload: None,
            });
            continue;
        }
        let code = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(6));
        db::with(|c| {
            c.execute(
                "INSERT INTO links (code, url, automation_id, recipient, created_at) VALUES (?, ?, ?, ?, ?)",
                params![code, url, x.id, recipient, now()],
            )?;
            Ok(())
        })?;
        out.push(Button {
            title,
            url: Some(match crate::tenant::current_id() {
                Some(t) => format!("{base}/l/t/{t}/{code}"),
                None => format!("{base}/l/{code}"),
            }),
            payload: None,
        });
    }
    Ok(out)
}

pub fn click_link(code: &str) -> Result<Option<String>> {
    db::with(|c| {
        let url: Option<String> = db::opt_row(c, "SELECT url FROM links WHERE code = ?", [code], |r| r.get(0))?;
        if url.is_some() {
            c.execute(
                "UPDATE links SET clicks = clicks + 1, first_click_at = COALESCE(first_click_at, ?) WHERE code = ?",
                params![now(), code],
            )?;
        }
        Ok(url)
    })
}

fn enqueue(kind: &str, account_id: &str, automation_id: Option<&str>, payload: Value, run_at: i64) -> Result<()> {
    db::with(|c| {
        c.execute(
            "INSERT INTO jobs (id, run_at, type, account_id, automation_id, payload) VALUES (?, ?, ?, ?, ?, ?)",
            params![new_id("job"), run_at, kind, account_id, automation_id, payload.to_string()],
        )?;
        Ok(())
    })
}

fn gate_payload(x: &Auto) -> DmPayload {
    let g = x.follow_gate().cloned().unwrap_or_default();
    DmPayload {
        text: g.get("ask_text").map(js_string).unwrap_or_default(),
        buttons: vec![Button {
            title: g.get("button_title").map(js_string).unwrap_or_default(),
            url: None,
            payload: Some(format!("fg:{}", x.id)),
        }],
    }
}

fn main_payload(x: &Auto, recipient: &str) -> Result<DmPayload> {
    Ok(DmPayload {
        text: x.s("dm_text").unwrap_or_default(),
        buttons: tracked_buttons(x, recipient)?,
    })
}

/// Public replies an account sent (or will send) in the last hour, across automations.
fn public_replies_last_hour(account_id: &str) -> Result<i64> {
    db::with(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) AS n FROM jobs WHERE account_id = ? AND type = 'public_reply' AND status IN ('pending', 'done') AND run_at > ?",
            params![account_id, now() - 3_600_000],
            |r| r.get(0),
        )?)
    })
}

// ---------- events ----------

/// Bind "next post" automations to a post the account just published.
pub fn bind_next_post(account_id: &str, remote_post_id: &str) -> Result<()> {
    for mut x in enabled_for(account_id, &COMMENT_KINDS)? {
        if x.s("scope").as_deref() == Some("next") && x.strs("platform_post_ids").is_empty() {
            x.c.insert("platform_post_ids".into(), json!([remote_post_id]));
            db::with(|c| {
                c.execute(
                    "UPDATE automations SET config = ?, updated_at = ? WHERE id = ?",
                    params![Value::Object(x.c.clone()).to_string(), now(), x.id],
                )?;
                Ok(())
            })?;
            log(&format!("{} bound to post {remote_post_id}", x.id));
        }
    }
    Ok(())
}

fn in_scope(x: &Auto, account_id: &str, post_id: &str) -> Result<bool> {
    let scope = x.s("scope").unwrap_or_default();
    if scope == "any" {
        return Ok(true);
    }
    if scope == "next" && x.strs("platform_post_ids").is_empty() {
        // A post made in the app after the automation was created counts as "the next post".
        let created: Option<i64> = db::with(|c| {
            db::opt_row(
                c,
                "SELECT created_at FROM account_posts WHERE account_id = ? AND remote_id = ?",
                params![account_id, post_id],
                |r| r.get(0),
            )
        })?;
        match created {
            Some(t) if t > x.created_at => bind_next_post(account_id, post_id)?,
            _ => return Ok(false),
        }
        return Ok(row(&x.id)?.strs("platform_post_ids").iter().any(|p| p == post_id));
    }
    Ok(x.strs("platform_post_ids").iter().any(|p| p == post_id))
}

pub fn on_comment(account_id: &str, c: &RemoteComment) -> Result<()> {
    if c.mine || (c.author_id.is_none() && c.username.is_none()) {
        return Ok(());
    }
    let who = c.author_id.clone().or(c.username.clone()).unwrap_or_default();
    for mut x in enabled_for(account_id, &COMMENT_KINDS)? {
        if c.created_at < x.enabled_at() {
            continue;
        }
        if !in_scope(&x, account_id, &c.post_id)? || !matches(&x.strs("keywords"), &x.s("match").unwrap_or_default(), Some(&c.text)) {
            continue;
        }
        let person_key = if x.kind == "comment_reply" {
            format!("p:{}:u:{who}", c.post_id)
        } else {
            format!("u:{who}")
        };
        if (x.kind == "comment_reply" || truthy(x.c.get("once_per_person"))) && marked(&x.id, &person_key, None)? {
            continue;
        }
        if marked(&x.id, &format!("c:{}", c.id), None)? {
            continue;
        }
        mark(&x.id, &format!("c:{}", c.id))?;
        mark(&x.id, &person_key)?;
        bump(&x.id, "triggered")?;
        // Like the TS, pick_reply saves the stats loaded before this bump (so "triggered" is
        // overwritten when a public reply is picked); kept for identical output.
        let (first, dm_after) = delays(&x.s("timing").unwrap_or_default());
        let mut t = now() + first;
        if let Some(reply) = pick_reply(&mut x)? {
            if public_replies_last_hour(account_id)? >= 30 {
                bump(&x.id, "skipped_rate_limit")?;
            } else {
                enqueue(
                    "public_reply",
                    account_id,
                    Some(&x.id),
                    json!({ "commentId": c.id, "postId": c.post_id, "text": reply }),
                    t,
                )?;
            }
            t += dm_after;
        }
        if x.kind == "comment_to_dm" {
            let msg = if x.follow_gate().is_some() {
                gate_payload(&x)
            } else {
                main_payload(&x, &who)?
            };
            if x.follow_gate().is_some() {
                mark(&x.id, &format!("gate:{who}"))?;
            }
            enqueue(
                "dm",
                account_id,
                Some(&x.id),
                json!({ "to": { "commentId": c.id }, "msg": msg.to_json(), "userKey": who }),
                t,
            )?;
        }
        return Ok(()); // one automation per comment
    }
    Ok(())
}

pub struct IncomingDm {
    pub from_id: String,
    pub message_id: String,
    pub text: Option<String>,
    pub created_at: i64,
    /// "reply" | "mention"
    pub story: Option<String>,
    pub postback: Option<String>,
}

pub fn on_message(account_id: &str, m: &IncomingDm) -> Result<()> {
    if now() - m.created_at > 24 * 3_600_000 {
        return Ok(()); // outside the reply window
    }
    let autos = enabled_for(account_id, &DM_KINDS)?;

    // Follow gate: a tap on the button (postback, or the button title typed back when only polling).
    let gate = match m.postback.as_deref().and_then(|p| p.strip_prefix("fg:")) {
        Some(id) => autos.iter().find(|x| x.id == id).cloned(),
        None => {
            let mut found = None;
            for x in &autos {
                if let Some(g) = x.follow_gate() {
                    let title = g.get("button_title").map(js_string).unwrap_or_default().to_lowercase();
                    if marked(&x.id, &format!("gate:{}", m.from_id), None)? && m.text.as_deref().map(|t| t.trim().to_lowercase()) == Some(title) {
                        found = Some(x.clone());
                        break;
                    }
                }
            }
            found
        }
    };
    if let Some(g) = gate {
        let (a, _, ctx) = ctx_for(account_id)?;
        let follows = providers::dm_follows(&a.platform, &ctx, &m.from_id).ok().flatten();
        let fg = g.follow_gate().cloned().unwrap_or_default();
        if follows == Some(true) {
            unmark(&g.id, &format!("gate:{}", m.from_id))?;
            bump(&g.id, "gate_passed")?;
            let msg = main_payload(&g, &m.from_id)?;
            enqueue(
                "dm",
                account_id,
                Some(&g.id),
                json!({ "to": { "id": m.from_id }, "msg": msg.to_json() }),
                now() + rand(2, 6) * 1000,
            )?;
        } else {
            bump(&g.id, "gate_not_following")?;
            let text = fg
                .get("retry_text")
                .filter(|v| !v.is_null())
                .or(fg.get("ask_text"))
                .map(js_string)
                .unwrap_or_default();
            let msg = DmPayload {
                text,
                buttons: vec![Button {
                    title: fg.get("button_title").map(js_string).unwrap_or_default(),
                    url: None,
                    payload: Some(format!("fg:{}", g.id)),
                }],
            };
            enqueue(
                "dm",
                account_id,
                Some(&g.id),
                json!({ "to": { "id": m.from_id }, "msg": msg.to_json() }),
                now() + rand(2, 6) * 1000,
            )?;
        }
        return Ok(());
    }

    for kind in ["story_mention", "story_reply", "dm_keyword", "default_reply"] {
        for x in autos.iter().filter(|a| a.kind == kind) {
            if m.created_at < x.enabled_at() {
                continue;
            }
            let kws = x.strs("keywords");
            let how = x.s("match").unwrap_or_default();
            if kind == "story_mention" && m.story.as_deref() != Some("mention") {
                continue;
            }
            if kind == "story_reply" && (m.story.as_deref() != Some("reply") || !matches(&kws, &how, m.text.as_deref())) {
                continue;
            }
            if kind == "dm_keyword" && (m.story.is_some() || !matches(&kws, &how, m.text.as_deref())) {
                continue;
            }
            let cooldown = (to_num(x.c.get("cooldown_hours")) * 3_600_000.0) as i64;
            if kind == "default_reply" && marked(&x.id, &format!("cd:{}", m.from_id), Some(cooldown))? {
                continue;
            }
            if truthy(x.c.get("once_per_person")) && marked(&x.id, &format!("u:{}", m.from_id), None)? {
                continue;
            }
            if marked(&x.id, &format!("m:{}", m.message_id), None)? {
                return Ok(());
            }
            mark(&x.id, &format!("m:{}", m.message_id))?;
            mark(
                &x.id,
                &if kind == "default_reply" {
                    format!("cd:{}", m.from_id)
                } else {
                    format!("u:{}", m.from_id)
                },
            )?;
            bump(&x.id, "triggered")?;
            let (first, _) = delays(&x.s("timing").unwrap_or_default());
            if truthy(x.c.get("react")) {
                enqueue(
                    "react",
                    account_id,
                    Some(&x.id),
                    json!({ "userId": m.from_id, "messageId": m.message_id }),
                    now() + first.min(20_000),
                )?;
            }
            let msg = if x.follow_gate().is_some() {
                gate_payload(x)
            } else {
                main_payload(x, &m.from_id)?
            };
            if x.follow_gate().is_some() {
                mark(&x.id, &format!("gate:{}", m.from_id))?;
            }
            enqueue(
                "dm",
                account_id,
                Some(&x.id),
                json!({ "to": { "id": m.from_id }, "msg": msg.to_json() }),
                now() + first,
            )?;
            return Ok(());
        }
    }
    Ok(())
}

// ---------- job runner ----------

struct Job {
    id: String,
    kind: String,
    account_id: String,
    automation_id: Option<String>,
    payload: String,
    attempts: i64,
}

pub fn run_jobs(limit: i64) -> Result<usize> {
    if !crate::limits::feature_ok("automations") {
        // The plan no longer includes automations: queued replies and DMs are dropped (the
        // automations themselves stay, and run again after an upgrade).
        db::with(|c| Ok(c.execute("UPDATE jobs SET status = 'cancelled' WHERE status = 'pending'", [])?))?;
        return Ok(0);
    }
    let due = db::tx(true, |c| {
        let rows = db::rows(
            c,
            "SELECT id, type, account_id, automation_id, payload, attempts FROM jobs WHERE status = 'pending' AND run_at <= ? ORDER BY run_at LIMIT ?",
            params![now(), limit],
            |r| {
                Ok(Job {
                    id: r.get(0)?,
                    kind: r.get(1)?,
                    account_id: r.get(2)?,
                    automation_id: r.get(3)?,
                    payload: r.get(4)?,
                    attempts: r.get(5)?,
                })
            },
        )?;
        let mut out = vec![];
        for j in rows {
            if c.execute("UPDATE jobs SET status = 'running' WHERE id = ? AND status = 'pending'", [&j.id])? > 0 {
                out.push(j);
            }
        }
        Ok(out)
    })?;
    let n = due.len();
    for j in due {
        let payload = json_or(Some(&j.payload), Value::Null);
        let r = (|| -> Result<()> {
            let (a, _, ctx) = ctx_for(&j.account_id)?;
            match j.kind.as_str() {
                "public_reply" => {
                    let comment = payload.get("commentId").map(js_string).unwrap_or_default();
                    let text = payload.get("text").map(js_string).unwrap_or_default();
                    providers::comment_reply(&a.platform, &ctx, &comment, &text)?;
                    db::with(|c| {
                        c.execute(
                            "UPDATE comments SET status = 'replied', reply_text = ?, replied_at = ? WHERE account_id = ? AND remote_id = ?",
                            params![text, now(), j.account_id, comment],
                        )?;
                        Ok(())
                    })?;
                    if let Some(aid) = &j.automation_id {
                        bump(aid, "public_replies")?;
                    }
                }
                "dm" => {
                    let to = &payload["to"];
                    let msg = DmPayload::from_json(&payload["msg"]);
                    let id = to.get("id").filter(|v| !v.is_null()).map(js_string);
                    let comment = to.get("commentId").filter(|v| !v.is_null()).map(js_string);
                    providers::dm_send(&a.platform, &ctx, id.as_deref(), comment.as_deref(), &msg)?;
                    if let Some(aid) = &j.automation_id {
                        bump(aid, "dms")?;
                    }
                }
                "react" => {
                    providers::dm_react(
                        &a.platform,
                        &ctx,
                        &payload.get("userId").map(js_string).unwrap_or_default(),
                        &payload.get("messageId").map(js_string).unwrap_or_default(),
                    )?;
                }
                _ => {}
            }
            Ok(())
        })();
        match r {
            Ok(()) => db::with(|c| {
                c.execute("UPDATE jobs SET status = 'done', done_at = ?, error = NULL WHERE id = ?", params![now(), j.id])?;
                Ok(())
            })?,
            Err(e) => {
                let attempts = j.attempts + 1;
                let retryable = match &e {
                    Error::Http { .. } => e.retryable(),
                    _ => true,
                };
                let retry = attempts < 3 && retryable;
                let msg: String = e.to_string().chars().take(1000).collect();
                db::with(|c| {
                    c.execute(
                        "UPDATE jobs SET status = ?, attempts = ?, error = ?, run_at = ? WHERE id = ?",
                        params![if retry { "pending" } else { "failed" }, attempts, msg, now() + 60_000 * attempts, j.id],
                    )?;
                    Ok(())
                })?;
                if !retry {
                    if let Some(aid) = &j.automation_id {
                        let _ = bump(aid, "errors");
                    }
                }
                log(&format!("job {} {} failed: {e}", j.kind, j.id));
            }
        }
    }
    Ok(n)
}

pub fn automation_activity(id: &str, limit: i64) -> Result<Value> {
    row(id)?;
    let rows = db::with(|c| {
        db::rows(
            c,
            "SELECT type, status, run_at, done_at, error, payload FROM jobs WHERE automation_id = ? ORDER BY run_at DESC LIMIT ?",
            params![id, limit],
            |r| {
                let run_at: i64 = r.get(2)?;
                let done_at: Option<i64> = r.get(3)?;
                let payload: String = r.get(5)?;
                Ok(Obj::new()
                    .set("type", r.get::<_, String>(0)?)
                    .set("status", r.get::<_, String>(1)?)
                    .set("at", iso(done_at.unwrap_or(run_at)))
                    .opt("error", r.get::<_, Option<String>>(4)?)
                    .defined("text", json_or(Some(&payload), Value::Null).get("text"))
                    .build())
            },
        )
    })?;
    Ok(Value::Array(rows))
}

// ---------- polling (works without webhooks) ----------

fn kv_get(k: &str) -> Result<Option<String>> {
    db::with(|c| db::opt_row(c, "SELECT value FROM kv WHERE key = ?", [k], |r| r.get(0)))
}

fn kv_set(k: &str, v: &str) -> Result<()> {
    db::with(|c| {
        c.execute("INSERT OR REPLACE INTO kv (key, value) VALUES (?, ?)", params![k, v])?;
        Ok(())
    })
}

pub fn poll_account(account_id: &str, comments: bool, dms: bool) -> Result<()> {
    if comments {
        for c in sync_comments(account_id, None)? {
            on_comment(account_id, &c)?;
        }
    }
    if dms {
        let key = format!("dmpoll:{account_id}");
        let since = kv_get(&key)?.map(|v| v.trim().parse::<f64>().unwrap_or(f64::NAN)).unwrap_or(0.0);
        let started = now();
        if since != 0.0 && !since.is_nan() {
            let since = since as i64;
            let (a, _, ctx) = ctx_for(account_id)?;
            for conv in providers::dm_conversations(&a.platform, &ctx, 25)? {
                if conv.updated_at <= since {
                    continue;
                }
                for m in providers::dm_messages(&a.platform, &ctx, &conv.id, 10)? {
                    if m.mine || m.created_at <= since {
                        continue;
                    }
                    let seen = format!("seen:{account_id}:{}", m.id);
                    if kv_get(&seen)?.is_some() {
                        continue;
                    }
                    kv_set(&seen, "1")?;
                    let story = m.story.as_ref().and_then(|s| s.get("kind")).map(js_string);
                    on_message(
                        account_id,
                        &IncomingDm {
                            from_id: m.from_id.clone().unwrap_or_default(),
                            message_id: m.id.clone(),
                            text: m.text.clone(),
                            created_at: m.created_at,
                            story,
                            postback: None,
                        },
                    )?;
                }
            }
        }
        kv_set(&key, &(started - 5_000).to_string())?;
    }
    Ok(())
}

/// Accounts to poll: those with enabled automations, plus all comment-capable ones if SIAR_COMMENT_SYNC=all.
/// (account id, comments, dms), in first-seen order.
pub fn poll_targets() -> Result<Vec<(String, bool, bool)>> {
    let rows: Vec<(String, String)> = if crate::limits::feature_ok("automations") {
        db::with(|c| {
            db::rows(c, "SELECT DISTINCT account_id, kind FROM automations WHERE enabled = 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
        })?
    } else {
        vec![]
    };
    let mut out: Vec<(String, bool, bool)> = vec![];
    for (acc, kind) in rows {
        let i = match out.iter().position(|o| o.0 == acc) {
            Some(i) => i,
            None => {
                out.push((acc.clone(), false, false));
                out.len() - 1
            }
        };
        if COMMENT_KINDS.contains(&kind.as_str()) {
            out[i].1 = true;
        }
        if kind != "comment_reply" {
            out[i].2 = true;
        }
    }
    if env("SIAR_COMMENT_SYNC").as_deref() == Some("all") {
        let accounts: Vec<(String, String)> = db::with(|c| {
            db::rows(c, "SELECT id, platform FROM accounts WHERE status = 'active'", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
        })?;
        for (id, platform) in accounts {
            if platforms::get(&platform).map(|p| p.has("comments")).unwrap_or(false) {
                match out.iter_mut().find(|o| o.0 == id) {
                    Some(o) => o.1 = true,
                    None => out.push((id, true, false)),
                }
            }
        }
    }
    Ok(out)
}

// ---------- Meta webhooks (Instagram), optional and faster than polling ----------

pub fn verify_meta_signature(raw: &[u8], header: Option<&str>) -> bool {
    verify_meta_signature_with(
        raw,
        header,
        &[
            env("INSTAGRAM_CLIENT_SECRET").unwrap_or_default(),
            env("FACEBOOK_CLIENT_SECRET").unwrap_or_default(),
        ],
    )
}

/// X-Hub-Signature-256 against each of `secrets` (app secrets of the Meta apps in use).
pub fn verify_meta_signature_with(raw: &[u8], header: Option<&str>, secrets: &[String]) -> bool {
    let Some(h) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let got: Vec<u8> = (0..h.len() / 2)
        .filter_map(|i| h.get(i * 2..i * 2 + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
        .collect();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
            continue;
        };
        mac.update(raw);
        // verify_slice is constant time and checks the length.
        if mac.verify_slice(&got).is_ok() {
            return true;
        }
    }
    false
}

pub fn handle_meta_webhook(body: &Value) -> Result<()> {
    let object = body.get("object").and_then(|o| o.as_str());
    if object != Some("instagram") && object != Some("page") {
        return Ok(());
    }
    for entry in body.get("entry").and_then(|e| e.as_array()).into_iter().flatten() {
        let entry_id = entry.get("id").map(js_string).unwrap_or_else(|| "undefined".into());
        let acc: Option<AccountRow> = db::with(|c| {
            db::opt_row(
                c,
                &format!(
                    "SELECT {} FROM accounts WHERE external_id = ? AND platform IN ('instagram', 'instagram-login', 'instagram-facebook')",
                    AccountRow::COLS
                ),
                [&entry_id],
                AccountRow::from,
            )
        })?;
        let Some(acc) = acc else { continue };
        for ch in entry.get("changes").and_then(|e| e.as_array()).into_iter().flatten() {
            if ch.get("field").and_then(|f| f.as_str()) != Some("comments") {
                continue;
            }
            let v = &ch["value"];
            let Some(post_id) = v.pointer("/media/id").filter(|p| truthy(Some(p))).map(js_string) else {
                continue;
            };
            let time = entry.get("time").filter(|t| truthy(Some(t))).map(|t| to_num(Some(t)));
            let c = RemoteComment {
                id: v.get("id").map(js_string).unwrap_or_else(|| "undefined".into()),
                post_id,
                parent_id: providers::opt_string(v.get("parent_id")),
                text: providers::opt_string(v.get("text")).unwrap_or_default(),
                author_id: providers::opt_string(v.pointer("/from/id")),
                username: providers::opt_string(v.pointer("/from/username")),
                created_at: time.map(|t| (t * if t < 1e12 { 1000.0 } else { 1.0 }) as i64).unwrap_or_else(now),
                hidden: false,
                mine: providers::opt_string(v.pointer("/from/id")).as_deref() == Some(entry_id.as_str()),
            };
            for f in store_comments(&acc.id, &[c])? {
                on_comment(&acc.id, &f)?;
            }
        }
        for ev in entry.get("messaging").and_then(|e| e.as_array()).into_iter().flatten() {
            let sender = ev.pointer("/sender/id").map(js_string);
            if truthy(ev.pointer("/message/is_echo")) || sender.as_deref() == Some(entry_id.as_str()) {
                continue;
            }
            let msg = ev.get("message");
            let story = if msg.map(|m| truthy(m.pointer("/reply_to/story"))).unwrap_or(false) {
                Some("reply".to_string())
            } else if msg
                .and_then(|m| m.get("attachments"))
                .and_then(|a| a.as_array())
                .map(|a| a.iter().any(|x| x.get("type").and_then(|t| t.as_str()) == Some("story_mention")))
                .unwrap_or(false)
            {
                Some("mention".to_string())
            } else {
                None
            };
            let from = sender.clone().unwrap_or_else(|| "undefined".into());
            let id = msg
                .and_then(|m| m.get("mid"))
                .filter(|v| !v.is_null())
                .or(ev.pointer("/postback/mid").filter(|v| !v.is_null()))
                .map(js_string)
                .unwrap_or_else(|| format!("{from}:{}", ev.get("timestamp").map(js_string).unwrap_or_else(|| "undefined".into())));
            let seen = format!("seen:{}:{id}", acc.id);
            if kv_get(&seen)?.is_some() {
                continue;
            }
            kv_set(&seen, "1")?;
            let ts = to_num(ev.get("timestamp"));
            let created_at = if ts.is_nan() || ts == 0.0 { now() } else { ts as i64 };
            let incoming = IncomingDm {
                from_id: from,
                message_id: id,
                text: msg.and_then(|m| providers::opt_string(m.get("text"))),
                created_at,
                story,
                postback: providers::opt_string(ev.pointer("/postback/payload")),
            };
            if let Err(e) = on_message(&acc.id, &incoming) {
                log(&format!("webhook message {e}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(kw: &[&str], how: &str, t: &str) -> bool {
        matches(&kw.iter().map(|s| s.to_string()).collect::<Vec<_>>(), how, Some(t))
    }

    #[test]
    fn keyword_matching() {
        assert!(m(&["info"], "contains", "Info dong kak"));
        assert!(!m(&["info"], "contains", "informasi"));
        assert!(m(&["harga"], "exact", " Harga "));
        assert!(m(&[], "contains", "anything"));
        assert!(m(&["info"], "contains", "minta info!"));
        assert!(!m(&["info"], "contains", "info_lagi"));
        assert!(m(&["info"], "contains", "infox info"));
    }
}
