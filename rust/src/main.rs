//! siar command line: `serve`, `mcp` (stdio), `keygen`, and in hosted mode the operator commands
//! `tenant create`, `key create`, `plan list`, `plan put`, `plan default` and `mail test`.

use siar::{config, control, db, mcp, scheduler, server, tenant, vault};

// musl's allocator is small but slow with large JSON responses (big allocations are mmapped and
// unmapped on every request); mimalloc is the default. Build with --no-default-features to compare.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

const USAGE: &str = "usage: siar [mcp | serve | keygen]
hosted mode (SIAR_MODE=hosted):
  siar tenant create <name>        new workspace with an API key (printed once)
  siar key create <tenant> <name>  another API key for a workspace
  siar plan list                   plans as JSON
  siar plan put '<json>'           add or replace a plan (see rust/HOSTED.md)
  siar plan default <plan id>      plan for workspaces without a subscription
  siar mail test <address>         send a test e-mail through the configured mailer";

fn hosted_only() -> siar::util::Result<()> {
    if !tenant::hosted() {
        return Err(siar::util::Error::Msg("This command needs SIAR_MODE=hosted".into()));
    }
    control::init()
}

fn admin(args: &[String]) -> siar::util::Result<()> {
    hosted_only()?;
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match a.as_slice() {
        ["tenant", "create", name] => {
            let t = control::create_tenant(name, None)?;
            let (_, key) = control::create_api_key(&t, "default")?;
            println!("{}", serde_json::json!({ "tenant_id": t, "api_key": key }));
        }
        ["key", "create", t, name] => {
            if control::tenant(t)?.is_none() {
                return Err(siar::util::Error::Msg("Unknown tenant".into()));
            }
            let (_, key) = control::create_api_key(t, name)?;
            println!("{}", serde_json::json!({ "tenant_id": t, "api_key": key }));
        }
        ["plan", "list"] => {
            for p in control::plans()? {
                let prices = control::plan_prices(&p.id, "doku")?;
                println!(
                    "{}",
                    serde_json::json!({
                        "id": p.id, "name": p.name, "description": p.description, "price_usd_cents": p.price_usd_cents,
                        "polar_product_id": p.polar_product_id, "max_accounts": p.max_accounts, "max_posts_month": p.max_posts_month,
                        "max_storage_mb": p.max_storage_mb, "max_upload_mb": p.max_upload_mb, "api_per_minute": p.api_per_minute,
                        "public": p.public as i64, "sort": p.sort, "interval": p.interval, "tier": p.tier,
                        "features": p.features.split(',').filter(|f| !f.is_empty()).collect::<Vec<_>>(),
                        "polar_price_cents": siar::billing::polar_price(&p).total,
                        "prices": prices.iter().map(|(m, a, c)| serde_json::json!({ "provider": "doku", "months": m, "amount": a, "currency": c })).collect::<Vec<_>>(),
                    })
                );
            }
        }
        ["plan", "put", json] => {
            let v: serde_json::Value = serde_json::from_str(json)?;
            control::put_plan(&v)?;
            println!("saved plan {}", v["id"]);
        }
        ["plan", "default", id] => {
            if control::plan(id)?.is_none() {
                return Err(siar::util::Error::Msg(format!("Unknown plan {id}")));
            }
            control::set_setting("default_plan", id)?;
            println!("default plan: {id}");
        }
        ["mail", "test", to] => {
            let text = "This is a test e-mail from siar. If you can read it, sending works.\n";
            if !siar::mailer::send(to, "siar test e-mail", text, &format!("<p>{}</p>", text.trim()))? {
                return Err(siar::util::Error::Msg("Mail is not configured (SIAR_MAIL_* unset)".into()));
            }
            println!("sent to {to}");
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "mcp".into());
    let scheduler_mode = config::env("SIAR_SCHEDULER").unwrap_or_default();
    let result = match cmd.as_str() {
        "mcp" if tenant::hosted() => Err(siar::util::Error::Msg(
            "stdio MCP is for single-tenant mode; hosted mode serves /mcp over HTTP".into(),
        )),
        "mcp" => db::init().map(|_| {
            // stdio MCP for Claude Code / Cursor. Runs the scheduler too unless another process owns it.
            scheduler::start(&scheduler_mode);
            mcp::run_stdio();
        }),
        "serve" => {
            let init = if tenant::hosted() { control::init() } else { db::init() };
            init.and_then(|_| {
                // Long-running: HTTP MCP endpoint, OAuth callbacks, local media, scheduler (and in
                // hosted mode the web pages, dashboard and payment webhooks).
                server::start()?;
                scheduler::start(&scheduler_mode);
                loop {
                    std::thread::park();
                }
            })
        }
        "keygen" => {
            println!("SIAR_VAULT_KEY={}", vault::generate_key());
            println!(
                "SIAR_API_TOKEN={}",
                vault::generate_key().chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()
            );
            Ok(())
        }
        "tenant" | "key" | "plan" | "mail" => admin(&args),
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(1);
        }
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
