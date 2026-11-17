//! siar, Rust port of the core: MCP server (HTTP and stdio), scheduler, and the Instagram,
//! Threads, LinkedIn and YouTube providers. Same database, vault and env vars as the Bun version.

mod accounts;
mod config;
mod db;
mod insights;
mod mcp;
mod media;
mod net;
mod oauth;
mod platforms;
mod posts;
mod providers;
mod scheduler;
mod server;
mod util;
mod validate;
mod vault;

fn main() {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "mcp".into());
    let scheduler_mode = config::env("SIAR_SCHEDULER").unwrap_or_default();
    let result = match cmd.as_str() {
        "mcp" => db::init().map(|_| {
            // stdio MCP for Claude Code / Cursor. Runs the scheduler too unless another process owns it.
            scheduler::start(&scheduler_mode);
            mcp::run_stdio();
        }),
        "serve" => db::init().and_then(|_| {
            // Long-running: HTTP MCP endpoint, OAuth callbacks, local media, scheduler.
            server::start()?;
            scheduler::start(&scheduler_mode);
            loop {
                std::thread::park();
            }
        }),
        "keygen" => {
            println!("SIAR_VAULT_KEY={}", vault::generate_key());
            println!(
                "SIAR_API_TOKEN={}",
                vault::generate_key().chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()
            );
            Ok(())
        }
        _ => {
            eprintln!("usage: siar [mcp | serve | keygen]");
            std::process::exit(1);
        }
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
