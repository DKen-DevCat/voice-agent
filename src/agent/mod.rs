//! Claude Code and what it is given: the long-lived session and its
//! stream, the subagents and usage it reports, the MCP server with the
//! tools in place of Bash, and the policy that decides which calls run.

pub mod agents;
pub mod claude;
pub mod mcp;
pub mod policy;
pub mod prompt;
pub mod stream;
pub mod tools;
pub mod usage;
