//! siar command line: `serve`, `mcp` (stdio), `keygen`, and every MCP tool as a command (cli.rs).

use siar::{cli, config, db, mcp, scheduler, server, vault};

// musl's allocator is small but slow with large JSON responses (big allocations are mmapped and
// unmapped on every request); mimalloc is the default. Build with --no-default-features to compare.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    config::load_dotenv();
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
        // Anything else is the CLI: `siar tools`, `siar help <tool>`, `siar <tool> --arg value`.
        _ => cli::run(&std::env::args().skip(1).collect::<Vec<_>>()),
    };
    if let Err(e) = result {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
