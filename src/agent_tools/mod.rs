//! The gateway's operations for autonomous agents: `gwctl`'s command
//! planner and client, and the MCP tools served over stdio (`gwctl mcp`)
//! and over streamable HTTP (`/api/mcp`). Each operation is an admin API
//! call, so permissions, the role ceiling, idempotency and audit apply as
//! they do to any other caller.

pub mod args;
pub mod client;
pub mod commands;
pub mod mcp;
