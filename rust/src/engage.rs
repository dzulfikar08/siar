//! Comment actions and Instagram DMs (src/engage.ts, the rest is in insights.rs). Text written by
//! other people is returned wrapped as `{"untrusted": "..."}`.

use rusqlite::params;
use serde_json::Value;

use crate::accounts::{ctx_for, get_account};
use crate::db;
use crate::insights::untrusted;
use crate::providers::{self, Button, DmPayload, RemotePost};
use crate::util::{bail, iso, now, Obj, Result};

/// (account id, remote post id) of a stored comment.
fn comment_row(account_id: &str, comment_id: &str) -> Result<(String, String)> {
    let a = get_account(account_id)?;
    let row = db::with(|c| {
        db::opt_row(
            c,
            "SELECT account_id, remote_post_id FROM comments WHERE account_id = ? AND remote_id = ?",
            params![a.id, comment_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
    })?;
    match row {
        Some(r) => Ok(r),
        None => bail!("Unknown comment {comment_id}; call list_comments with refresh first"),
    }
}

pub fn reply_to_comment(account_id: &str, comment_id: &str, text: &str) -> Result<Value> {
    let (acc, _) = comment_row(account_id, comment_id)?;
    let (a, p, ctx) = ctx_for(account_id)?;
    p.need("comments")?;
    let r = providers::comment_reply(&a.platform, &ctx, comment_id, text)?;
    db::with(|c| {
        c.execute(
            "UPDATE comments SET status = 'replied', reply_text = ?, replied_at = ? WHERE account_id = ? AND remote_id = ?",
            params![text, now(), acc, comment_id],
        )?;
        Ok(())
    })?;
    Ok(Obj::new().set("replied", comment_id).defined("reply_id", r.get("id")).build())
}

pub fn hide_comment(account_id: &str, comment_id: &str, hidden: bool) -> Result<Value> {
    let (acc, _) = comment_row(account_id, comment_id)?;
    let (a, p, ctx) = ctx_for(account_id)?;
    p.need("comments")?;
    if !providers::comment_can(&a.platform, "hide") {
        bail!("{} cannot hide comments through its API", p.name);
    }
    providers::comment_hide(&a.platform, &ctx, comment_id, hidden)?;
    db::with(|c| {
        c.execute(
            "UPDATE comments SET hidden = ? WHERE account_id = ? AND remote_id = ?",
            params![hidden as i64, acc, comment_id],
        )?;
        Ok(())
    })?;
    Ok(Obj::new().set("comment_id", comment_id).set("hidden", hidden).build())
}

pub fn like_comment(account_id: &str, comment_id: &str) -> Result<Value> {
    comment_row(account_id, comment_id)?;
    let (a, p, ctx) = ctx_for(account_id)?;
    p.need("comments")?;
    if !providers::comment_can(&a.platform, "like") {
        bail!("{} does not allow liking comments through its API", p.name);
    }
    providers::comment_like(&a.platform, &ctx, comment_id)?;
    Ok(Obj::new().set("liked", comment_id).build())
}

pub fn delete_comment(account_id: &str, comment_id: &str) -> Result<Value> {
    let (acc, _) = comment_row(account_id, comment_id)?;
    let (a, p, ctx) = ctx_for(account_id)?;
    p.need("comments")?;
    if !providers::comment_can(&a.platform, "remove") {
        bail!("{} cannot delete comments through its API", p.name);
    }
    providers::comment_remove(&a.platform, &ctx, comment_id)?;
    db::with(|c| {
        c.execute("DELETE FROM comments WHERE account_id = ? AND remote_id = ?", params![acc, comment_id])?;
        Ok(())
    })?;
    Ok(Obj::new().set("deleted", comment_id).build())
}

// ---------- DMs ----------

pub fn list_conversations(account_id: &str, limit: i64) -> Result<Value> {
    let (a, p, ctx) = ctx_for(account_id)?;
    p.need("dm")?;
    let list = providers::dm_conversations(&a.platform, &ctx, limit)?;
    Ok(Value::Array(
        list.iter()
            .map(|c| {
                Obj::new()
                    .set("conversation_id", c.id.clone())
                    .opt("user_id", c.user_id.clone())
                    .opt("username", untrusted(c.username.as_deref()))
                    .set("updated_at", iso(c.updated_at))
                    .opt("last_message", untrusted(c.last_message.as_deref()))
                    .build()
            })
            .collect(),
    ))
}

pub fn get_conversation(account_id: &str, conversation_id: &str, limit: i64) -> Result<Value> {
    let (a, p, ctx) = ctx_for(account_id)?;
    p.need("dm")?;
    let msgs = providers::dm_messages(&a.platform, &ctx, conversation_id, limit)?;
    let messages: Vec<Value> = msgs
        .iter()
        .map(|m| {
            let from = if m.mine {
                Value::from("me")
            } else {
                Obj::new()
                    .opt("user_id", m.from_id.clone())
                    .opt("username", untrusted(m.username.as_deref()))
                    .build()
            };
            let text = if m.mine {
                m.text.clone().map(Value::from)
            } else {
                untrusted(m.text.as_deref())
            };
            Obj::new()
                .set("id", m.id.clone())
                .set("from", from)
                .opt("text", text)
                .set("at", iso(m.created_at))
                .opt("story", m.story.clone())
                .opt("attachments", if m.attachments.is_empty() { None } else { Some(m.attachments.clone()) })
                .build()
        })
        .collect();
    Ok(Obj::new()
        .set("messages", messages)
        .set("note", "Instagram only allows replies within 24 hours of the person's last message.")
        .build())
}

/// Buttons as the tools take them: `[{title, url}]`.
pub fn buttons_from(v: Option<&Value>) -> Vec<Button> {
    v.and_then(|b| b.as_array())
        .into_iter()
        .flatten()
        .map(|b| Button {
            title: b.get("title").map(crate::util::js_string).unwrap_or_default(),
            url: crate::providers::opt_string(b.get("url")),
            payload: crate::providers::opt_string(b.get("payload")),
        })
        .collect()
}

pub fn send_dm(account_id: &str, user_id: Option<String>, comment_id: Option<String>, msg: &DmPayload) -> Result<Value> {
    let (a, p, ctx) = ctx_for(account_id)?;
    if user_id.is_none() && comment_id.is_none() {
        bail!("Give user_id, or comment_id for a private reply to a comment");
    }
    p.need("dm")?;
    providers::dm_send(&a.platform, &ctx, user_id.as_deref(), comment_id.as_deref(), msg)
}

pub fn get_contact(account_id: &str, user_id: &str) -> Result<Value> {
    let (a, p, ctx) = ctx_for(account_id)?;
    p.need("dm")?;
    let (username, name, follows) = providers::dm_profile(&a.platform, &ctx, user_id)?;
    Ok(Obj::new()
        .set("user_id", user_id)
        .opt("username", untrusted(username.as_deref()))
        .opt("name", untrusted(name.as_deref()))
        .set("follows_you", follows)
        .build())
}

/// A remote post for search results and post context: text and username are untrusted.
pub fn untrusted_post(p: &RemotePost) -> Value {
    Obj::new()
        .set("id", p.id.clone())
        .opt("url", p.url.clone())
        .opt("text", untrusted(p.text.as_deref()))
        .opt("type", p.kind.clone())
        .opt("username", untrusted(p.username.as_deref()))
        .set("createdAt", iso(p.created_at))
        .opt("metrics", p.metrics.clone())
        .build()
}
