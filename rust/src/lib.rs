//! siar: MCP server (HTTP and stdio), CLI, scheduler, automations, every platform, and the
//! read-only calendar page. It opens databases written by the former Bun version as they are.

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
pub mod ui;
pub mod unicode_tables;
pub mod util;
pub mod validate;
pub mod vault;
