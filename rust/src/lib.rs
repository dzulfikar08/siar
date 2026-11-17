//! siar, Rust port: MCP server (HTTP and stdio), scheduler, automations and every platform of the
//! Bun version. Same database, vault and env vars, so either binary can run on the same data.

pub mod accounts;
pub mod automations;
pub mod cli;
pub mod config;
pub mod db;
pub mod engage;
pub mod insights;
pub mod mcp;
pub mod media;
pub mod net;
pub mod oauth;
pub mod platforms;
pub mod posts;
pub mod providers;
pub mod scheduler;
pub mod server;
pub mod unicode_tables;
pub mod util;
pub mod validate;
pub mod vault;
