//! Post checks against platform limits (src/validate.ts).

use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

use crate::db::MediaRow;
use crate::media::kind_of;
use crate::util::{truthy, Obj};

/// X's weighted count: URLs are 23, most CJK/emoji count as 2.
fn twitter_length(text: &str) -> usize {
    let mut n = 0;
    let mut rest = String::with_capacity(text.len());
    let mut i = 0;
    let b = text.as_bytes();
    while i < b.len() {
        let tail = &text[i..];
        if tail.starts_with("http://") || tail.starts_with("https://") {
            let scheme = if tail.starts_with("https://") { 8 } else { 7 };
            // \S+ after the scheme: at least one non-space character.
            let body: usize = tail[scheme..].chars().take_while(|c| !c.is_whitespace()).map(|c| c.len_utf8()).sum();
            if body > 0 {
                n += 23;
                i += scheme + body;
                continue;
            }
        }
        let c = tail.chars().next().unwrap_or('\0');
        rest.push(c);
        i += c.len_utf8().max(1);
    }
    for ch in rest.chars() {
        let cp = ch as u32;
        let light = cp <= 0x10ff || (0x2000..=0x200d).contains(&cp) || (0x2010..=0x201f).contains(&cp) || (0x2032..=0x2037).contains(&cp);
        n += if light { 1 } else { 2 };
    }
    n
}

pub fn count_chars(text: &str, how: &str) -> usize {
    match how {
        "graphemes" => text.graphemes(true).count(),
        "twitter" => twitter_length(text),
        "utf16" => text.encode_utf16().count(),
        _ => text.chars().count(),
    }
}

pub struct Media<'a> {
    pub row: &'a MediaRow,
    pub kind: &'static str,
}

impl<'a> Media<'a> {
    pub fn new(row: &'a MediaRow) -> Self {
        Media { row, kind: kind_of(&row.mime) }
    }
}

pub struct Check {
    pub ok: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub chars: usize,
    pub max_chars: i64,
}

impl Check {
    /// Merged into `{account, platform, ok, errors, warnings, chars, maxChars}` like the TS spread.
    pub fn to_json(&self, account: &str, platform: &str) -> Value {
        Obj::new()
            .set("account", account)
            .set("platform", platform)
            .set("ok", self.ok)
            .set("errors", self.errors.clone())
            .set("warnings", self.warnings.clone())
            .set("chars", self.chars)
            .set("maxChars", self.max_chars)
            .build()
    }
}

fn mb(n: f64, digits: usize) -> String {
    format!("{:.*}", digits, n / 1_048_576.0)
}

pub fn check(platform: &str, limits: &Value, text: &str, media: &[Media], options: &Value) -> Check {
    let mut errors = vec![];
    let mut warnings = vec![];
    let lim_i = |k: &str| limits.get(k).and_then(|v| v.as_i64());
    let lim_b = |k: &str| limits.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let opt = |k: &str| options.get(k);
    let count = limits.get("count").and_then(|v| v.as_str()).unwrap_or("codepoints");
    let max_chars = if platform == "x" && truthy(opt("long")) {
        25_000
    } else {
        lim_i("maxChars").unwrap_or(0)
    };
    let chars = count_chars(text, count);
    let is_story = |o: &Value| o.get("format").and_then(|v| v.as_str()) == Some("story") || o.get("type").and_then(|v| v.as_str()) == Some("story");
    if chars as i64 > max_chars && !(platform.starts_with("instagram") && is_story(options)) {
        errors.push(format!("Text is {chars} characters; the limit is {max_chars}"));
    }
    if text.trim().is_empty() && media.is_empty() {
        errors.push("Post is empty".into());
    }
    if lim_b("requiresMedia") && media.is_empty() {
        errors.push("This platform needs at least one image or video".into());
    }
    let max_media = lim_i("maxMedia").unwrap_or(0);
    if media.len() as i64 > max_media {
        errors.push(format!("{} media items; the limit is {max_media}", media.len()));
    }
    let allowed: Vec<&str> = limits
        .get("media")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    let mut kinds: Vec<&str> = vec![];
    for m in media {
        if !kinds.contains(&m.kind) {
            kinds.push(m.kind);
        }
        let f = &m.row.filename;
        if !allowed.contains(&m.kind) {
            let list = if allowed.is_empty() { "none".to_string() } else { allowed.join(", ") };
            errors.push(format!("{f}: {} is not supported here (allowed: {list})", m.kind));
        }
        if let Some(max) = lim_i("maxImageBytes").filter(|x| *x != 0) {
            if m.kind == "image" && m.row.size > max {
                errors.push(format!("{f} is {} MB; images max {} MB", mb(m.row.size as f64, 1), mb(max as f64, 1)));
            }
        }
        if let Some(max) = lim_i("maxVideoBytes").filter(|x| *x != 0) {
            if m.kind == "video" && m.row.size > max {
                errors.push(format!("{f} is {} MB; videos max {} MB", mb(m.row.size as f64, 0), mb(max as f64, 0)));
            }
        }
        if m.kind == "image" && m.row.alt.as_deref().unwrap_or("").is_empty() {
            warnings.push(format!("{f} has no alt text"));
        }
    }
    let mixed = lim_b("mixed");
    if kinds.len() > 1 && !mixed {
        errors.push("Mixing images, videos and documents in one post is not supported here".into());
    }
    if kinds.contains(&"video") && media.iter().filter(|m| m.kind == "video").count() > 1 && !mixed {
        errors.push("Only one video per post".into());
    }
    if limits.get("title").and_then(|v| v.as_str()) == Some("required") && !truthy(opt("title")) {
        errors.push("options.title is required".into());
    }
    if let Some(thread) = opt("thread").and_then(|v| v.as_array()).filter(|a| !a.is_empty()) {
        if !lim_b("thread") {
            errors.push("This platform does not support thread replies (options.thread)".into());
        }
        for (i, t) in thread.iter().enumerate() {
            let text = match t {
                Value::String(s) => s.clone(),
                Value::Object(_) => t.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                _ => String::new(),
            };
            let c = count_chars(&text, count);
            if c as i64 > max_chars {
                errors.push(format!("Thread part {} is {c} characters; the limit is {max_chars}", i + 2));
            }
            if t.is_object() && t.get("media_ids").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0) > 1 {
                errors.push(format!("Thread part {}: at most one media item", i + 2));
            }
        }
    }
    if platform == "threads" {
        if truthy(opt("poll")) {
            let opts = opt("poll").and_then(|p| p.get("options")).and_then(|v| v.as_array());
            let n = opts.map(|a| a.len()).unwrap_or(0);
            if !(2..=4).contains(&n) {
                errors.push("A Threads poll takes 2-4 options".into());
            }
            if opts
                .map(|a| {
                    a.iter()
                        .any(|o| !truthy(Some(o)) || o.as_str().map(|s| s.encode_utf16().count() > 25).unwrap_or(false))
                })
                .unwrap_or(false)
            {
                errors.push("Each poll option is 1-25 characters".into());
            }
        }
        if (truthy(opt("poll")) || truthy(opt("link_attachment")) || truthy(opt("text_attachment"))) && !media.is_empty() {
            errors.push("Threads polls, link cards and text attachments need a post without media".into());
        }
        if let Some(tag) = opt("topic_tag").and_then(|v| v.as_str()) {
            if tag.contains('.') || tag.contains('&') {
                errors.push("Threads topic tags cannot contain . or &".into());
            }
        }
    }
    if platform.starts_with("instagram") {
        if is_story(options) && media.len() != 1 {
            errors.push("An Instagram story takes exactly one image or video".into());
        }
        if truthy(opt("trial")) && !(media.len() == 1 && media[0].kind == "video") {
            errors.push("Trial reels need exactly one video".into());
        }
        if opt("collaborators").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0) > 3 {
            errors.push("At most 3 collaborators".into());
        }
    }
    if platform == "youtube" && !opt("made_for_kids").map(|v| v.is_boolean()).unwrap_or(false) {
        errors.push("options.made_for_kids (true/false) is required: ask the user".into());
    }
    if platform == "reddit" && !truthy(opt("subreddit")) {
        errors.push("options.subreddit is required".into());
    }
    if platform == "lemmy" && !truthy(opt("community")) {
        errors.push("options.community is required".into());
    }
    Check {
        ok: errors.is_empty(),
        errors,
        warnings,
        chars,
        max_chars,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_counting() {
        assert_eq!(count_chars("héllo", "codepoints"), 5);
        assert_eq!(count_chars("👨‍👩‍👧", "graphemes"), 1);
        assert_eq!(count_chars("hi https://example.com/a/very/long/path/that/is/long", "twitter"), 3 + 23);
        assert_eq!(count_chars("日本", "twitter"), 4);
        assert_eq!(count_chars("a😀", "utf16"), 3);
        assert_eq!(count_chars("a😀", "codepoints"), 2);
    }
}

#[cfg(test)]
mod ts_fixture {
    use serde_json::Value;

    /// Escaping, keyword matching and counting on tricky inputs, against the TS output
    /// (rust/scripts/dump-meta.ts writes the fixture).
    #[test]
    fn text_handling_matches_the_ts() {
        let f: Value = serde_json::from_str(include_str!("meta/text-fixture.json")).unwrap();
        for case in f.as_array().unwrap() {
            let t = case["text"].as_str().unwrap();
            assert_eq!(crate::providers::linkedin::escape_little(t), case["escape"].as_str().unwrap(), "escape {t:?}");
            for (kw, want) in case["matches"].as_object().unwrap() {
                assert_eq!(
                    crate::automations::matches(std::slice::from_ref(kw), "contains", Some(t)),
                    want.as_bool().unwrap(),
                    "matches {kw:?} in {t:?}"
                );
            }
            for (how, want) in case["counts"].as_object().unwrap() {
                assert_eq!(super::count_chars(t, how) as u64, want.as_u64().unwrap(), "{how} of {t:?}");
            }
        }
    }
}
