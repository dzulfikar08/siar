//! siar: MCP server (HTTP and stdio), CLI, scheduler, automations, every platform, and the
//! read-only calendar page. It opens databases written by the former Bun version as they are.

pub mod accounts;
pub mod authz;
pub mod automations;
pub mod billing;
pub mod cli;
pub mod config;
pub mod control;
pub mod db;
pub mod engage;
pub mod insights;
pub mod limits;
pub mod mailer;
pub mod mcp;
pub mod media;
pub mod net;
pub mod oauth;
pub mod platforms;
pub mod posts;
pub mod providers;
pub mod scheduler;
pub mod server;
pub mod tenant;
pub mod ui;
pub mod unicode_tables;
pub mod util;
pub mod validate;
pub mod vault;
pub mod web;
