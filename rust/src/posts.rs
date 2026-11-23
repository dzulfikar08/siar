//! Posts, targets, validation, publishing with retries (src/posts.ts).

use rusqlite::params;
use serde_json::{Map, Value};

use crate::accounts::{build_ctx, get_account, mark_account, to_media};
use crate::db::{self, PostRow, TargetRow};
use crate::insights::get_idea;
use crate::platforms;
use crate::providers;
use crate::util::{arg_strs, bail, iso, js_string, json_or, new_id, now, permanent, Error, Obj, Result};
use crate::validate::{check, Check, Media};

const MAX_ATTEMPTS: i64 = 5;

/// One target as given by the agent, with the account id resolved.
pub struct TargetInput {
    pub account_id: String,
    pub content: Option<String>,
    pub media_ids: Option<Vec<String>>,
    pub options: Value,
}

/// The post fields of a tool call (content, media_ids, options, targets, idea_id).
pub struct PostInput<'a>(pub &'a Value);

impl PostInput<'_> {
    fn content(&self) -> String {
        match self.0.get("content") {
            None | Some(Value::Null) => String::new(),
            Some(v) => js_string(v),
        }
    }
    fn media_json(&self) -> String {
        serde_json::to_string(&arg_strs(self.0, "media_ids").unwrap_or_default()).unwrap_or_else(|_| "[]".into())
    }
    fn options_json(&self) -> String {
        match self.0.get("options") {
            None | Some(Value::Null) => "{}".into(),
            Some(v) => v.to_string(),
        }
    }
}

fn empty_obj() -> Value {
    Value::Object(Map::new())
}

/// Resolve the effective text, media and options of one target.
fn effective(
    post_content: &str,
    post_media: &str,
    post_options: &str,
    t_content: Option<&str>,
    t_media: Option<&str>,
    t_options: &str,
    platform: &str,
) -> (String, Vec<String>, Value) {
    let post_opts = json_or(Some(post_options), empty_obj());
    let mut merged = Map::new();
    if let Some(o) = post_opts.as_object() {
        for (k, v) in o {
            if !v.is_object() {
                merged.insert(k.clone(), v.clone());
            }
        }
        if let Some(per) = o.get(platform).and_then(|v| v.as_object()) {
            for (k, v) in per {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    if let Some(t) = json_or(Some(t_options), empty_obj()).as_object() {
        for (k, v) in t {
            merged.insert(k.clone(), v.clone());
        }
    }
    let post_media_ids = json_or(Some(post_media), Value::Array(vec![]));
    let media = match t_media {
        Some(m) => json_or(Some(m), post_media_ids),
        None => post_media_ids,
    };
    let ids = media.as_array().map(|a| a.iter().map(js_string).collect()).unwrap_or_default();
    (t_content.unwrap_or(post_content).to_string(), ids, Value::Object(merged))
}

/// Validate one target; returns `{account, platform, ok, errors, warnings, chars, maxChars}`.
fn validate_target(
    post: (&str, &str, &str),
    account_id: &str,
    t_content: Option<&str>,
    t_media: Option<&str>,
    t_options: &str,
) -> Result<(String, Check, String)> {
    let a = get_account(account_id)?;
    let p = platforms::get(&a.platform)?;
    let (text, media_ids, options) = effective(post.0, post.1, post.2, t_content, t_media, t_options, &a.platform);
    let rows = to_media(&media_ids)?;
    let media: Vec<Media> = rows.iter().map(Media::new).collect();
    Ok((a.label(), check(&a.platform, p.limits(), &text, &media, &options), a.platform.clone()))
}

fn norm_targets(input: &Value) -> Result<Vec<TargetInput>> {
    let list = input.get("targets").and_then(|v| v.as_array()).filter(|a| !a.is_empty());
    let Some(list) = list else { bail!("Give at least one target account") };
    list.iter()
        .map(|t| {
            let (id, content, media, options) = match t {
                Value::Object(_) => (
                    t.get("account_id").map(js_string).unwrap_or_else(|| "undefined".into()),
                    t.get("content").filter(|v| !v.is_null()).map(js_string),
                    arg_strs(t, "media_ids"),
                    t.get("options").filter(|v| !v.is_null()).cloned().unwrap_or_else(empty_obj),
                ),
                other => (js_string(other), None, None, empty_obj()),
            };
            Ok(TargetInput {
                account_id: get_account(&id)?.id,
                content,
                media_ids: media,
                options,
            })
        })
        .collect()
}

fn checks_json(checks: &[(String, Check, String)]) -> Value {
    Value::Array(checks.iter().map(|(acc, c, p)| c.to_json(acc, p)).collect())
}

fn fails_message(checks: &[(String, Check, String)]) -> Option<String> {
    let bad: Vec<String> = checks
        .iter()
        .filter(|c| !c.1.ok)
        .map(|(acc, c, _)| format!("- {acc}: {}", c.errors.join("; ")))
        .collect();
    if bad.is_empty() {
        None
    } else {
        Some(format!("Post fails validation:\n{}", bad.join("\n")))
    }
}

fn validate_input(input: &Value) -> Result<Vec<(String, Check, String)>> {
    let pi = PostInput(input);
    let (content, media, options) = (pi.content(), pi.media_json(), pi.options_json());
    norm_targets(input)?
        .iter()
        .map(|t| {
            let tm = t.media_ids.as_ref().map(|m| serde_json::to_string(m).unwrap_or_default());
            validate_target(
                (&content, &media, &options),
                &t.account_id,
                t.content.as_deref(),
                tm.as_deref(),
                &t.options.to_string(),
            )
        })
        .collect()
}

/// Check a post against every target's limits without saving anything.
pub fn validate_post(input: &Value) -> Result<Value> {
    Ok(checks_json(&validate_input(input)?))
}

pub fn create_post(input: &Value, status: &str, at: Option<i64>, force: bool) -> Result<Value> {
    let checks = validate_input(input)?;
    if status == "scheduled" && !force {
        if let Some(m) = fails_message(&checks) {
            bail!("{m}");
        }
    }
    let id = new_id("post");
    let ts = now();
    let at = if status == "scheduled" { Some(at.unwrap_or(ts)) } else { None };
    let pi = PostInput(input);
    let idea_id = input.get("idea_id").filter(|v| crate::util::truthy(Some(v))).map(js_string);
    if let Some(i) = &idea_id {
        get_idea(i)?;
    }
    let targets = norm_targets(input)?;
    if status == "scheduled" {
        crate::limits::check_post(None)?;
    }
    db::tx(false, |c| {
        c.execute(
            "INSERT INTO posts (id, status, content, media, options, scheduled_at, created_at, updated_at, idea_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![id, status, pi.content(), pi.media_json(), pi.options_json(), at, ts, ts, idea_id],
        )?;
        insert_targets(c, &id, &targets, at)
    })?;
    if status == "scheduled" {
        crate::limits::record_post(&id)?;
    }
    Ok(Obj::new().set("post", get_post(&id)?).set("checks", checks_json(&checks)).build())
}

fn insert_targets(c: &rusqlite::Connection, post_id: &str, targets: &[TargetInput], at: Option<i64>) -> Result<()> {
    for t in targets {
        c.execute(
            "INSERT INTO targets (id, post_id, account_id, content, media, options, status, next_attempt_at) VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)",
            params![
                new_id("tgt"),
                post_id,
                t.account_id,
                t.content,
                t.media_ids.as_ref().map(|m| serde_json::to_string(m).unwrap_or_default()),
                t.options.to_string(),
                at
            ],
        )?;
    }
    Ok(())
}

fn post_row(id: &str) -> Result<Option<PostRow>> {
    db::with(|c| db::opt_row(c, &format!("SELECT {} FROM posts WHERE id = ?", PostRow::COLS), [id], PostRow::from))
}

type TargetWithAccount = (TargetRow, String, String, Option<String>);

fn target_with_account(r: &rusqlite::Row) -> rusqlite::Result<TargetWithAccount> {
    Ok((TargetRow::from(r)?, r.get(13)?, r.get(14)?, r.get(15)?))
}

/// Posts whose time (scheduled, else created) falls in [from, to), with every target's effective text,
/// media and options: the data behind the calendar page (src/ui.ts calendarData).
pub fn calendar_data(from: i64, to: i64) -> Result<Value> {
    db::with(|c| {
        let posts = db::rows(
            c,
            &format!(
                "SELECT {} FROM posts WHERE COALESCE(scheduled_at, created_at) >= ? AND COALESCE(scheduled_at, created_at) < ? ORDER BY COALESCE(scheduled_at, created_at) LIMIT 2000",
                PostRow::COLS
            ),
            params![from, to],
            PostRow::from,
        )?;
        let accounts = db::rows(c, "SELECT id, platform, name, handle, status FROM accounts ORDER BY platform, name", [], |r| {
            Ok(Obj::new()
                .set("id", r.get::<_, String>(0)?)
                .set("platform", r.get::<_, String>(1)?)
                .set("name", r.get::<_, String>(2)?)
                .set("handle", r.get::<_, Option<String>>(3)?)
                .set("status", r.get::<_, String>(4)?)
                .build())
        })?;
        let mut out = Vec::with_capacity(posts.len());
        for p in &posts {
            let targets = db::rows(
                c,
                &format!(
                    "SELECT {}, a.platform, a.name AS account_name, a.handle FROM targets t JOIN accounts a ON a.id = t.account_id WHERE t.post_id = ?",
                    TargetRow::COLS
                ),
                [&p.id],
                target_with_account,
            )?;
            let mut ts = Vec::with_capacity(targets.len());
            for (t, platform, name, handle) in &targets {
                let (text, ids, options) = effective(&p.content, &p.media, &p.options, t.content.as_deref(), t.media.as_deref(), &t.options, platform);
                ts.push(
                    Obj::new()
                        .set("account_id", t.account_id.clone())
                        .set("platform", platform.clone())
                        .set("name", name.clone())
                        .set("handle", handle.clone())
                        .set("status", t.status.clone())
                        .set("text", text)
                        .set("media", calendar_media(c, &ids)?)
                        .set("options", options)
                        .opt("url", t.remote_url.clone())
                        .opt("error", t.error.clone())
                        .opt("published_at", t.published_at.map(iso))
                        .build(),
                );
            }
            out.push(
                Obj::new()
                    .set("id", p.id.clone())
                    .set("status", p.status.clone())
                    .set("at", iso(p.scheduled_at.unwrap_or(p.created_at)))
                    .set("scheduled", p.scheduled_at.is_some())
                    .set("targets", ts)
                    .build(),
            );
        }
        Ok(Obj::new().set("accounts", accounts).set("posts", out).build())
    })
}

fn calendar_media(c: &rusqlite::Connection, ids: &[String]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for id in ids {
        let row = db::opt_row(c, "SELECT mime, url, alt, filename FROM media WHERE id = ?", [id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        if let Some((mime, url, alt, filename)) = row {
            out.push(
                Obj::new()
                    .set("id", id.clone())
                    .set("url", url)
                    .set("kind", crate::media::kind_of(&mime))
                    .set("mime", mime)
                    .opt("alt", alt)
                    .set("filename", filename)
                    .build(),
            );
        }
    }
    Ok(out)
}

pub fn get_post(id: &str) -> Result<Value> {
    let (p, targets) = db::with(|c| {
        let Some(p) = db::opt_row(c, &format!("SELECT {} FROM posts WHERE id = ?", PostRow::COLS), [id], PostRow::from)? else {
            bail!("Unknown post {id}");
        };
        let targets = db::rows(
            c,
            &format!(
                "SELECT {}, a.platform, a.name AS account_name, a.handle FROM targets t JOIN accounts a ON a.id = t.account_id WHERE post_id = ?",
                TargetRow::COLS
            ),
            [id],
            target_with_account,
        )?;
        Ok((p, targets))
    })?;
    Ok(post_json(&p, &targets))
}

fn post_json(p: &PostRow, targets: &[TargetWithAccount]) -> Value {
    let targets: Vec<Value> = targets
        .iter()
        .map(|(t, platform, name, handle)| {
            Obj::new()
                .set("id", t.id.clone())
                .set("account_id", t.account_id.clone())
                .set("account", format!("{platform}:{}", handle.as_deref().unwrap_or(name)))
                .set("status", t.status.clone())
                .opt("content", t.content.clone())
                .opt(
                    "media_ids",
                    t.media.as_deref().filter(|m| !m.is_empty()).map(|m| json_or(Some(m), Value::Array(vec![]))),
                )
                .set("options", json_or(Some(&t.options), empty_obj()))
                .set("attempts", t.attempts)
                .opt(
                    "next_attempt_at",
                    if t.status == "pending" {
                        t.next_attempt_at.filter(|x| *x != 0).map(iso)
                    } else {
                        None
                    },
                )
                .opt("remote_id", t.remote_id.clone())
                .opt("url", t.remote_url.clone())
                .opt("error", t.error.clone())
                .opt("published_at", t.published_at.filter(|x| *x != 0).map(iso))
                .build()
        })
        .collect();
    Obj::new()
        .set("id", p.id.clone())
        .set("status", p.status.clone())
        .set("content", p.content.clone())
        .set("media_ids", json_or(Some(&p.media), Value::Array(vec![])))
        .set("options", json_or(Some(&p.options), empty_obj()))
        .set("scheduled_at", p.scheduled_at.filter(|x| *x != 0).map(iso))
        .opt("idea_id", p.idea_id.clone())
        .set("created_at", iso(p.created_at))
        .set("targets", targets)
        .build()
}

pub fn list_posts(status: Option<String>, from: Option<i64>, to: Option<i64>, account_id: Option<String>, limit: Option<i64>) -> Result<Value> {
    let mut wh: Vec<&str> = vec![];
    let mut args: Vec<rusqlite::types::Value> = vec![];
    if let Some(s) = status.filter(|s| !s.is_empty()) {
        wh.push("p.status = ?");
        args.push(s.into());
    }
    if let Some(f) = from.filter(|x| *x != 0) {
        wh.push("COALESCE(p.scheduled_at, p.created_at) >= ?");
        args.push(f.into());
    }
    if let Some(t) = to.filter(|x| *x != 0) {
        wh.push("COALESCE(p.scheduled_at, p.created_at) < ?");
        args.push(t.into());
    }
    if let Some(a) = account_id.filter(|s| !s.is_empty()) {
        wh.push("EXISTS (SELECT 1 FROM targets t WHERE t.post_id = p.id AND t.account_id = ?)");
        args.push(get_account(&a)?.id.into());
    }
    args.push(limit.unwrap_or(50).into());
    let w = if wh.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", wh.join(" AND "))
    };
    let sql = format!("SELECT p.id FROM posts p {w} ORDER BY COALESCE(p.scheduled_at, p.created_at) DESC LIMIT ?");
    // The TS version calls getPost per row, and each one scans `targets` (no index on post_id).
    // Here all targets of the page come from one scan; the output is the same.
    let (posts, targets) = db::with(|c| {
        let ids = db::rows(c, &sql, rusqlite::params_from_iter(args.iter()), |r| r.get::<_, String>(0))?;
        if ids.is_empty() {
            return Ok((vec![], vec![]));
        }
        let marks = vec!["?"; ids.len()].join(",");
        let posts = db::rows(
            c,
            &format!("SELECT {} FROM posts WHERE id IN ({marks})", PostRow::COLS),
            rusqlite::params_from_iter(ids.iter()),
            PostRow::from,
        )?;
        let targets = db::rows(
            c,
            &format!(
                "SELECT {}, a.platform, a.name AS account_name, a.handle FROM targets t JOIN accounts a ON a.id = t.account_id WHERE post_id IN ({marks})",
                TargetRow::COLS
            ),
            rusqlite::params_from_iter(ids.iter()),
            target_with_account,
        )?;
        // Keep the page order of `ids`.
        let posts: Vec<PostRow> = ids.iter().filter_map(|id| posts.iter().find(|p| &p.id == id).cloned()).collect();
        Ok((posts, targets))
    })?;
    let mut out = vec![];
    for post in &posts {
        let mine: Vec<_> = targets.iter().filter(|t| t.0.post_id == post.id).cloned().collect();
        let mut p = post_json(post, &mine);
        if let Some(Value::String(content)) = p.get_mut("content") {
            let units: Vec<u16> = content.encode_utf16().collect();
            if units.len() > 200 {
                *content = format!("{}…", String::from_utf16_lossy(&units[..200]));
            }
        }
        out.push(p);
    }
    Ok(Value::Array(out))
}

pub fn update_post(id: &str, patch: &Value, scheduled_at: Option<Option<i64>>) -> Result<Value> {
    let Some(p) = post_row(id)? else { bail!("Unknown post {id}") };
    if !["draft", "scheduled", "failed"].contains(&p.status.as_str()) {
        bail!("Post is {}; only drafts, scheduled and failed posts can be edited", p.status);
    }
    let pi = PostInput(patch);
    let content = match patch.get("content") {
        None | Some(Value::Null) => p.content.clone(),
        Some(_) => pi.content(),
    };
    let media = if patch.get("media_ids").map(|v| crate::util::truthy(Some(v))).unwrap_or(false) {
        pi.media_json()
    } else {
        p.media.clone()
    };
    let options = if patch.get("options").map(|v| crate::util::truthy(Some(v))).unwrap_or(false) {
        pi.options_json()
    } else {
        p.options.clone()
    };
    let new_targets = if patch.get("targets").map(|v| crate::util::truthy(Some(v))).unwrap_or(false) {
        Some(norm_targets(patch)?)
    } else {
        None
    };
    db::tx(false, |c| {
        c.execute(
            "UPDATE posts SET content = ?, media = ?, options = ?, updated_at = ? WHERE id = ?",
            params![content, media, options, now(), id],
        )?;
        if let Some(ts) = &new_targets {
            c.execute("DELETE FROM targets WHERE post_id = ? AND status != 'published'", [id])?;
            let at = if p.status == "scheduled" { p.scheduled_at } else { None };
            insert_targets(c, id, ts, at)?;
        }
        Ok(())
    })?;
    if let Some(at) = scheduled_at {
        return schedule_post(id, at, false);
    }
    Ok(Obj::new().set("post", get_post(id)?).set("checks", checks_json(&checks_for(id)?)).build())
}

fn checks_for(id: &str) -> Result<Vec<(String, Check, String)>> {
    let (p, targets) = db::with(|c| {
        let p = db::opt_row(c, &format!("SELECT {} FROM posts WHERE id = ?", PostRow::COLS), [id], PostRow::from)?;
        let t = db::rows(
            c,
            &format!(
                "SELECT {} FROM targets t WHERE post_id = ? AND status IN ('pending', 'failed')",
                TargetRow::COLS
            ),
            [id],
            TargetRow::from,
        )?;
        Ok((p, t))
    })?;
    let Some(p) = p else { bail!("Unknown post {id}") };
    targets
        .iter()
        .map(|t| {
            validate_target(
                (&p.content, &p.media, &p.options),
                &t.account_id,
                t.content.as_deref(),
                t.media.as_deref(),
                &t.options,
            )
        })
        .collect()
}

/// Schedule (or with at = None, unschedule back to draft). Failed targets are retried.
pub fn schedule_post(id: &str, at: Option<i64>, force: bool) -> Result<Value> {
    let Some(p) = post_row(id)? else { bail!("Unknown post {id}") };
    if p.status == "publishing" || p.status == "published" {
        bail!("Post is already {}", p.status);
    }
    let checks = checks_for(id)?;
    if at.is_some() && !force {
        if let Some(m) = fails_message(&checks) {
            bail!("{m}");
        }
    }
    if at.is_some() {
        crate::limits::check_post(Some(id))?;
    }
    db::tx(false, |c| {
        c.execute(
            "UPDATE posts SET status = ?, scheduled_at = ?, updated_at = ? WHERE id = ?",
            params![if at.is_none() { "draft" } else { "scheduled" }, at, now(), id],
        )?;
        c.execute(
            "UPDATE targets SET status = 'pending', attempts = 0, error = NULL, next_attempt_at = ? WHERE post_id = ? AND status IN ('pending', 'failed', 'cancelled')",
            params![at, id],
        )?;
        Ok(())
    })?;
    if at.is_some() {
        crate::limits::record_post(id)?;
    }
    Ok(Obj::new().set("post", get_post(id)?).set("checks", checks_json(&checks)).build())
}

pub fn cancel_post(id: &str) -> Result<Value> {
    let p = get_post(id)?;
    if p["status"] == "published" {
        bail!("Post is already published; use delete_published to remove it from the platforms");
    }
    db::tx(false, |c| {
        c.execute(
            "UPDATE targets SET status = 'cancelled', next_attempt_at = NULL WHERE post_id = ? AND status = 'pending'",
            [id],
        )?;
        c.execute("UPDATE posts SET status = 'cancelled', updated_at = ? WHERE id = ?", params![now(), id])?;
        Ok(())
    })?;
    get_post(id)
}

pub fn delete_post(id: &str) -> Result<Value> {
    let p = get_post(id)?;
    if p["targets"].as_array().into_iter().flatten().any(|t| t["status"] == "publishing") {
        bail!("Post is being published right now");
    }
    db::with(|c| {
        c.execute("DELETE FROM posts WHERE id = ?", [id])?;
        Ok(())
    })?;
    Ok(Obj::new().set("deleted", id).build())
}

fn rollup(c: &rusqlite::Connection, post_id: &str) -> Result<()> {
    let counts = db::rows(
        c,
        "SELECT status, COUNT(*) AS n FROM targets WHERE post_id = ? GROUP BY status",
        [post_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
    )?;
    let n = |s: &str| counts.iter().find(|x| x.0 == s).map(|x| x.1).unwrap_or(0);
    let status = if n("publishing") > 0 || n("pending") > 0 {
        if n("publishing") > 0 {
            "publishing"
        } else {
            "scheduled"
        }
    } else if n("published") > 0 && (n("failed") > 0 || n("cancelled") > 0) {
        if n("failed") > 0 {
            "partial"
        } else {
            "published"
        }
    } else if n("published") > 0 {
        "published"
    } else if n("failed") > 0 {
        "failed"
    } else {
        "cancelled"
    };
    c.execute(
        "UPDATE posts SET status = ?, updated_at = ? WHERE id = ? AND status != 'draft'",
        params![status, now(), post_id],
    )?;
    Ok(())
}

fn backoff(attempt: i64) -> i64 {
    (30_000i64 * 2i64.pow((attempt - 1).clamp(0, 20) as u32)).min(60 * 60_000)
}

fn load_target(target_id: &str) -> Result<(TargetRow, PostRow)> {
    db::with(|c| {
        let Some(t) = db::opt_row(
            c,
            &format!("SELECT {} FROM targets t WHERE id = ?", TargetRow::COLS),
            [target_id],
            TargetRow::from,
        )?
        else {
            bail!("Unknown target {target_id}");
        };
        let Some(p) = db::opt_row(c, &format!("SELECT {} FROM posts WHERE id = ?", PostRow::COLS), [&t.post_id], PostRow::from)? else {
            bail!("Unknown post {}", t.post_id);
        };
        Ok((t, p))
    })
}

pub fn publish_target(target_id: &str) -> Result<()> {
    let (t, post) = load_target(target_id)?;
    let a = get_account(&t.account_id)?;
    let p = platforms::get(&a.platform)?;
    let (text, media_ids, options) = effective(
        &post.content,
        &post.media,
        &post.options,
        t.content.as_deref(),
        t.media.as_deref(),
        &t.options,
        &a.platform,
    );
    let outcome = (|| -> Result<providers::Published> {
        if a.status == "reauth_required" {
            permanent!("{} needs to be reconnected", a.name);
        }
        let rows = to_media(&media_ids)?;
        let media: Vec<Media> = rows.iter().map(Media::new).collect();
        let v = check(&a.platform, p.limits(), &text, &media, &options);
        if !v.ok {
            return Err(Error::Permanent(v.errors.join("; ")));
        }
        let ctx = build_ctx(&a, &text, &media_ids, options.clone(), false)?;
        match providers::publish(&a.platform, &ctx) {
            Ok(r) => Ok(r),
            // One forced refresh on 401, then retry once.
            Err(e) if e.auth_failed() && p.has("refresh") => {
                let ctx = build_ctx(&get_account(&a.id)?, &text, &media_ids, options.clone(), true)?;
                providers::publish(&a.platform, &ctx)
            }
            Err(e) => Err(e),
        }
    })();
    match outcome {
        Ok(res) => {
            db::with(|c| {
                c.execute(
                    "UPDATE targets SET status = 'published', remote_id = ?, remote_url = ?, error = NULL, published_at = ?, next_attempt_at = NULL WHERE id = ?",
                    params![res.id, res.url, now(), t.id],
                )?;
                Ok(())
            })?;
            crate::automations::bind_next_post(&a.id, &res.id)?;
        }
        Err(err) => {
            let attempts = t.attempts + 1;
            let retry = err.retryable() && attempts < MAX_ATTEMPTS;
            if err.auth_failed() {
                mark_account(&a.id, "reauth_required")?;
            }
            let msg: String = err.to_string().chars().take(2000).collect();
            db::with(|c| {
                c.execute(
                    "UPDATE targets SET status = ?, attempts = ?, error = ?, next_attempt_at = ? WHERE id = ?",
                    params![
                        if retry { "pending" } else { "failed" },
                        attempts,
                        msg,
                        if retry { Some(now() + backoff(attempts)) } else { None },
                        t.id
                    ],
                )?;
                Ok(())
            })?;
        }
    }
    db::with(|c| rollup(c, &t.post_id))
}

/// Claim due targets atomically (BEGIN IMMEDIATE) so two processes on one database never double-post.
pub fn claim_due(limit: i64) -> Result<Vec<String>> {
    db::tx(true, |c| {
        let ts = now();
        let rows = db::rows(
            c,
            "SELECT t.id, t.post_id FROM targets t JOIN posts p ON p.id = t.post_id
             WHERE t.status = 'pending' AND p.status NOT IN ('draft', 'cancelled') AND t.next_attempt_at IS NOT NULL AND t.next_attempt_at <= ?
             ORDER BY t.next_attempt_at LIMIT ?",
            params![ts, limit],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        let mut claimed = vec![];
        for (id, post_id) in rows {
            // next_attempt_at doubles as the claim time while publishing (see recover_interrupted).
            if c.execute(
                "UPDATE targets SET status = 'publishing', next_attempt_at = ? WHERE id = ? AND status = 'pending'",
                params![ts, id],
            )? > 0
            {
                claimed.push(id);
                c.execute("UPDATE posts SET status = 'publishing' WHERE id = ?", [post_id])?;
            }
        }
        Ok(claimed)
    })
}

/// Due targets, without claiming them (SIAR_SCHEDULER=dry).
pub fn count_due() -> Result<i64> {
    db::with(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM targets t JOIN posts p ON p.id = t.post_id
             WHERE t.status = 'pending' AND p.status NOT IN ('draft', 'cancelled') AND t.next_attempt_at IS NOT NULL AND t.next_attempt_at <= ?",
            [now()],
            |r| r.get(0),
        )?)
    })
}

/// Targets stuck in 'publishing' (process killed mid-upload) may or may not be live, so they are
/// marked failed for a human to check instead of being retried. Only claims older than the lease.
pub fn recover_interrupted(lease_ms: i64) -> Result<usize> {
    db::with(|c| {
        let stuck = db::rows(
            c,
            "SELECT id, post_id FROM targets WHERE status = 'publishing' AND COALESCE(next_attempt_at, 0) < ?",
            [now() - lease_ms],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )?;
        for (id, post_id) in &stuck {
            c.execute(
                "UPDATE targets SET status = 'failed', next_attempt_at = NULL, error = 'Interrupted while publishing; check the platform before retrying' WHERE id = ?",
                [id],
            )?;
            rollup(c, post_id)?;
        }
        Ok(stuck.len())
    })
}

/// Publish several targets in parallel threads (Promise.all in TS).
pub fn publish_all(ids: &[String]) {
    let tenant = crate::tenant::current();
    std::thread::scope(|s| {
        for id in ids {
            let tenant = tenant.clone();
            s.spawn(move || {
                let _g = crate::tenant::enter_opt(tenant);
                if let Err(e) = publish_target(id) {
                    eprintln!("{} [scheduler] target {id} {e}", iso(now()));
                }
            });
        }
    });
}

pub fn publish_now(post_id: &str, force: bool) -> Result<Value> {
    schedule_post(post_id, Some(now()), force)?;
    let ids = db::tx(true, |c| {
        let rows = db::rows(c, "SELECT id FROM targets WHERE post_id = ? AND status = 'pending'", [post_id], |r| {
            r.get::<_, String>(0)
        })?;
        let mut out = vec![];
        for id in rows {
            if c.execute(
                "UPDATE targets SET status = 'publishing', next_attempt_at = ? WHERE id = ? AND status = 'pending'",
                params![now(), id],
            )? > 0
            {
                out.push(id);
            }
        }
        Ok(out)
    })?;
    publish_all(&ids);
    get_post(post_id)
}

fn published_targets(post_id: &str) -> Result<(PostRow, Vec<TargetRow>)> {
    let Some(post) = post_row(post_id)? else { bail!("Unknown post {post_id}") };
    let targets = db::with(|c| {
        db::rows(
            c,
            &format!("SELECT {} FROM targets t WHERE post_id = ? AND status = 'published'", TargetRow::COLS),
            [post_id],
            TargetRow::from,
        )
    })?;
    Ok((post, targets))
}

pub fn delete_published(post_id: &str, account_ids: Option<Vec<String>>) -> Result<Value> {
    let (post, targets) = published_targets(post_id)?;
    let only: Option<Vec<String>> = match account_ids {
        Some(ids) => Some(ids.iter().map(|x| get_account(x).map(|a| a.id)).collect::<Result<_>>()?),
        None => None,
    };
    let mut out = vec![];
    for t in targets {
        if let Some(o) = &only {
            if !o.contains(&t.account_id) {
                continue;
            }
        }
        let a = get_account(&t.account_id)?;
        let p = platforms::get(&a.platform)?;
        if !p.has("remove") {
            out.push(
                Obj::new()
                    .set("account", a.name.clone())
                    .set("deleted", false)
                    .set("reason", format!("{} has no delete API", p.name))
                    .build(),
            );
            continue;
        }
        let (text, _, options) = effective(
            &post.content,
            &post.media,
            &post.options,
            t.content.as_deref(),
            t.media.as_deref(),
            &t.options,
            &a.platform,
        );
        let r = (|| {
            let ctx = build_ctx(&a, &text, &[], options, false)?;
            providers::remove(&a.platform, &ctx, t.remote_id.as_deref().unwrap_or_default())
        })();
        match r {
            Ok(()) => {
                db::with(|c| {
                    c.execute("UPDATE targets SET status = 'cancelled', error = 'Deleted from platform' WHERE id = ?", [&t.id])?;
                    Ok(())
                })?;
                out.push(Obj::new().set("account", a.name.clone()).set("deleted", true).build());
            }
            Err(e) => out.push(
                Obj::new()
                    .set("account", a.name.clone())
                    .set("deleted", false)
                    .set("reason", e.to_string())
                    .build(),
            ),
        }
    }
    db::with(|c| rollup(c, post_id))?;
    Ok(Value::Array(out))
}

pub fn store_metric(target_id: &str, m: &Value) -> Result<()> {
    db::with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO metrics (target_id, fetched_at, data) VALUES (?, ?, ?)",
            params![target_id, now(), m.to_string()],
        )?;
        Ok(())
    })
}

pub fn post_metrics(post_id: &str) -> Result<Value> {
    let (post, targets) = published_targets(post_id)?;
    let mut out = vec![];
    for t in targets {
        let a = get_account(&t.account_id)?;
        let p = platforms::get(&a.platform)?;
        let base = Obj::new().set("account", a.label()).set("url", t.remote_url.clone());
        if !p.has("metrics") {
            out.push(base.set("metrics", Value::Null).set("note", format!("{} has no analytics API", p.name)).build());
            continue;
        }
        let (text, _, options) = effective(
            &post.content,
            &post.media,
            &post.options,
            t.content.as_deref(),
            t.media.as_deref(),
            &t.options,
            &a.platform,
        );
        let r = (|| -> Result<Value> {
            let ctx = build_ctx(&a, &text, &[], options, false)?;
            let m = providers::metrics(&a.platform, &ctx, t.remote_id.as_deref().unwrap_or_default())?;
            store_metric(&t.id, &m)?;
            Ok(m)
        })();
        out.push(match r {
            Ok(m) => base.set("metrics", m).build(),
            Err(e) => base.set("metrics", Value::Null).set("error", e.to_string()).build(),
        });
    }
    Ok(Value::Array(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_merge_like_ts() {
        let (text, media, opts) = effective(
            "hello",
            "[\"med_a\"]",
            r#"{"made_for_kids":false,"youtube":{"title":"T"},"x":{"thread":["2/"]},"tags":["a"]}"#,
            None,
            None,
            r#"{"title":"Override"}"#,
            "youtube",
        );
        assert_eq!(text, "hello");
        assert_eq!(media, vec!["med_a".to_string()]);
        assert_eq!(opts.to_string(), r#"{"made_for_kids":false,"tags":["a"],"title":"Override"}"#);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff(1), 30_000);
        assert_eq!(backoff(2), 60_000);
        assert_eq!(backoff(10), 3_600_000);
    }
}
