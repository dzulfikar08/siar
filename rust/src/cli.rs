//! Command-line access to the MCP tools: `siar tools`, `siar help <tool>`,
//! `siar <tool> --arg value`. One line per call for agents that run shell commands, and the API
//! token stays in the environment instead of the model's context.
//!
//! Local: tools run in this process against SIAR_DATA_DIR (no scheduler is started).
//! Remote: with SIAR_URL set, calls go to $SIAR_URL/mcp with SIAR_API_TOKEN as the bearer token.

use base64::Engine;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::Path;

use crate::config::env;
use crate::util::{bail, Error, Result};

const HELP: &str = "siar: schedule and publish social media posts from the command line.

  siar tools                    list every tool
  siar help <tool>              arguments of one tool
  siar <tool> [--arg value ...] call a tool; prints JSON

Arguments: --name value. Values that look like JSON (numbers, true/false, [..], {..})
are parsed as JSON. --name @file.md reads a file, --name - reads stdin, and
--json '{...}' (or --json @args.json) passes all arguments at once.
Tool names take dashes or underscores: list-accounts = list_accounts.

Remote server: set SIAR_URL (e.g. https://siar.example.com) and SIAR_API_TOKEN.
Without SIAR_URL the tools run locally against SIAR_DATA_DIR.
With SIAR_URL, stage-media --path <local file> uploads the file's bytes.

Examples:
  siar list-accounts
  siar validate-post --content @post.md --targets '[\"acc_…\"]'
  siar schedule-post --json @post.json --at 2026-11-20T08:00:00+07:00
  siar stage-media --path ./cover.jpg --alt \"Screenshot of the dashboard\"";

/// `println!` that treats a closed pipe (`siar tools | head`) as the reader being done: exit 0
/// quietly instead of panicking.
macro_rules! say {
    ($($t:tt)*) => {{
        use std::io::Write;
        let mut out = std::io::stdout().lock();
        if let Err(e) = writeln!(out, $($t)*).and_then(|_| out.flush()) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                std::process::exit(0);
            }
            return Err(e.into());
        }
    }};
}

enum Backend {
    Local,
    Remote { endpoint: String, token: String },
}

impl Backend {
    fn from_env() -> Result<Backend> {
        let Some(base) = env("SIAR_URL").filter(|u| !u.is_empty()) else {
            crate::db::init()?;
            return Ok(Backend::Local);
        };
        let Some(token) = env("SIAR_API_TOKEN").filter(|t| !t.is_empty()) else {
            bail!("SIAR_URL is set but SIAR_API_TOKEN is not");
        };
        let base = base.trim_end_matches('/');
        let base = base.strip_suffix("/mcp").unwrap_or(base);
        Ok(Backend::Remote {
            endpoint: format!("{base}/mcp"),
            token,
        })
    }

    fn remote(&self) -> bool {
        matches!(self, Backend::Remote { .. })
    }

    /// One JSON-RPC request; returns its `result`.
    fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let msg = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let answer = match self {
            Backend::Local => crate::mcp::handle(&msg).unwrap_or(Value::Null),
            Backend::Remote { endpoint, token } => {
                match crate::net::post(endpoint.as_str())
                    .bearer(token)
                    .header("accept", "application/json")
                    .json(msg)
                    .get_json()
                {
                    Ok(v) => v,
                    Err(Error::Http { status: 401, .. }) => bail!("{endpoint} refused SIAR_API_TOKEN (401)"),
                    Err(e) => return Err(e),
                }
            }
        };
        if let Some(err) = answer.get("error") {
            let m = err.get("message").and_then(|m| m.as_str()).map(String::from).unwrap_or_else(|| err.to_string());
            bail!("{m}");
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    fn list(&self) -> Result<Vec<Value>> {
        Ok(self
            .rpc("tools/list", json!({}))?
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default())
    }

    fn call(&self, name: &str, args: Value) -> Result<Value> {
        let r = self.rpc("tools/call", json!({ "name": name, "arguments": args }))?;
        let text = r["content"][0]["text"].as_str().unwrap_or("").to_string();
        if r["isError"] == true {
            bail!("{text}");
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

fn read_stdin() -> Result<String> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    Ok(s)
}

fn read_file(path: &str) -> Result<String> {
    if !Path::new(path).is_file() {
        bail!("File not found: {path}");
    }
    Ok(std::fs::read_to_string(path)?)
}

fn looks_like_json(raw: &str) -> bool {
    if raw.starts_with(['[', '{', '"']) || matches!(raw, "true" | "false" | "null") {
        return true;
    }
    // -?\d+(\.\d+)?
    let digits = raw.strip_prefix('-').unwrap_or(raw);
    let (int, frac) = digits.split_once('.').unwrap_or((digits, "0"));
    !int.is_empty() && !frac.is_empty() && int.bytes().all(|b| b.is_ascii_digit()) && frac.bytes().all(|b| b.is_ascii_digit())
}

fn coerce(raw: &str) -> Result<Value> {
    if raw == "-" {
        return Ok(Value::String(read_stdin()?));
    }
    if let Some(path) = raw.strip_prefix('@').filter(|p| !p.is_empty()) {
        return Ok(Value::String(read_file(path)?));
    }
    if looks_like_json(raw) {
        if let Ok(v) = serde_json::from_str(raw) {
            return Ok(v);
        }
    }
    Ok(Value::String(raw.to_string()))
}

/// `--key value` pairs. Values that parse as JSON are used as JSON; `@file` reads a file; `-` reads stdin.
pub fn parse_args(argv: &[String]) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        i += 1;
        if a == "--json" {
            let Some(raw) = argv.get(i) else {
                bail!("--json needs a value");
            };
            i += 1;
            let text = if raw == "-" {
                read_stdin()?
            } else if let Some(path) = raw.strip_prefix('@') {
                read_file(path)?
            } else {
                raw.clone()
            };
            match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(m)) => out.extend(m),
                _ => bail!("--json must be a JSON object"),
            }
            continue;
        }
        let Some(flag) = a.strip_prefix("--") else {
            bail!("Unexpected argument \"{a}\". Arguments are --name value.");
        };
        let (key, raw) = match flag.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None if argv.get(i).is_some_and(|n| !n.starts_with("--")) => {
                i += 1;
                (flag, Some(argv[i - 1].clone()))
            }
            None => (flag, None),
        };
        let value = match raw {
            Some(r) => coerce(&r)?,
            None => Value::Bool(true),
        };
        out.insert(key.replace('-', "_"), value);
    }
    Ok(out)
}

fn dashed(name: &str) -> String {
    name.replace('_', "-")
}

fn usage_for(t: &Value) -> String {
    let name = t["name"].as_str().unwrap_or("");
    let mut lines = vec![
        format!("siar {}", dashed(name)),
        String::new(),
        t["description"].as_str().unwrap_or("").to_string(),
        String::new(),
    ];
    let required: Vec<&str> = t["inputSchema"]["required"]
        .as_array()
        .map(|r| r.iter().filter_map(|x| x.as_str()).collect())
        .unwrap_or_default();
    let props = t["inputSchema"]["properties"].as_object().cloned().unwrap_or_default();
    if props.is_empty() {
        lines.push("No arguments.".into());
    }
    for (n, p) in &props {
        let ty = match &p["type"] {
            Value::String(s) => s.clone(),
            Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" | "),
            _ if p.get("anyOf").is_some() => "string | object".into(),
            _ => "value".into(),
        };
        let req = if required.contains(&n.as_str()) { ", required" } else { "" };
        let desc = p["description"].as_str().map(|d| format!("  {d}")).unwrap_or_default();
        lines.push(format!("  --{}  ({ty}{req}){desc}", dashed(n)));
    }
    if t["annotations"]["destructiveHint"] == true {
        lines.push(String::new());
        lines.push("Destructive: confirm with the user first.".into());
    }
    lines.join("\n")
}

fn find_tool(backend: &Backend, asked: &str) -> Result<Value> {
    let name = asked.replace('-', "_");
    match backend.list()?.into_iter().find(|t| t["name"] == name.as_str()) {
        Some(t) => Ok(t),
        None => bail!("Unknown tool {asked}. Run `siar tools` for the list."),
    }
}

/// The CLI entry point for anything that isn't `mcp`, `serve` or `keygen`.
pub fn run(argv: &[String]) -> Result<()> {
    let Some(first) = argv.first().map(String::as_str).filter(|f| !matches!(*f, "--help" | "-h")) else {
        say!("{HELP}");
        return Ok(());
    };
    let rest = &argv[1..];
    if first == "help" && rest.is_empty() {
        say!("{HELP}");
        return Ok(());
    }
    let backend = Backend::from_env()?;

    if first == "tools" {
        let tools = backend.list()?;
        let width = tools.iter().filter_map(|t| t["name"].as_str()).map(str::len).max().unwrap_or(0);
        for t in &tools {
            let name = dashed(t["name"].as_str().unwrap_or(""));
            let desc = t["description"].as_str().unwrap_or("");
            // First sentence only.
            let short = desc.find(". ").map(|i| &desc[..=i]).unwrap_or(desc);
            say!("{name:width$}  {short}");
        }
        return Ok(());
    }

    if first == "help" {
        say!("{}", usage_for(&find_tool(&backend, &rest[0])?));
        return Ok(());
    }

    let name = first.replace('-', "_");
    let mut args = parse_args(rest)?;
    if args.get("help") == Some(&Value::Bool(true)) {
        say!("{}", usage_for(&find_tool(&backend, first)?));
        return Ok(());
    }

    // The server can't read a path on this machine: send the bytes instead.
    if backend.remote() && name == "stage_media" {
        if let Some(Value::String(path)) = args.remove("path") {
            if !Path::new(&path).is_file() {
                bail!("File not found: {path}");
            }
            let bytes = std::fs::read(&path)?;
            args.insert("base64".into(), Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)));
            if !args.contains_key("filename") {
                let file = Path::new(&path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                args.insert("filename".into(), Value::String(file));
            }
        }
    }

    let result = backend.call(&name, Value::Object(args))?;
    match result {
        Value::String(s) => say!("{s}"),
        v => say!("{}", serde_json::to_string_pretty(&v)?),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn json_values_flags_dashes_and_equals() {
        let a = parse_args(&argv(&[
            "--targets",
            "[\"acc_1\"]",
            "--force",
            "--limit",
            "5",
            "--post-id=p_1",
            "--content",
            "plain text",
            "--options",
            "{\"x\":{\"thread\":[\"2/\"]}}",
        ]))
        .unwrap();
        assert_eq!(
            Value::Object(a),
            json!({ "targets": ["acc_1"], "force": true, "limit": 5, "post_id": "p_1", "content": "plain text", "options": { "x": { "thread": ["2/"] } } })
        );
    }

    #[test]
    fn files_and_json_object() {
        let dir = std::env::temp_dir().join(format!("siar-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let post = dir.join("post.md");
        let args = dir.join("args.json");
        std::fs::write(&post, "Line one\nLine two\n").unwrap();
        std::fs::write(&args, r#"{"at":"2026-11-20T08:00:00+07:00","targets":["acc_2"]}"#).unwrap();
        let a = parse_args(&argv(&[
            "--json",
            &format!("@{}", args.display()),
            "--content",
            &format!("@{}", post.display()),
        ]))
        .unwrap();
        assert_eq!(
            Value::Object(a),
            json!({ "at": "2026-11-20T08:00:00+07:00", "targets": ["acc_2"], "content": "Line one\nLine two\n" })
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn strings_that_only_look_like_json_stay_strings() {
        assert_eq!(
            Value::Object(parse_args(&argv(&["--content", "[not json"])).unwrap()),
            json!({ "content": "[not json" })
        );
        assert_eq!(
            Value::Object(parse_args(&argv(&["--content", "1.2.3"])).unwrap()),
            json!({ "content": "1.2.3" })
        );
        assert!(parse_args(&argv(&["stray"])).unwrap_err().to_string().contains("Unexpected argument"));
    }
}
