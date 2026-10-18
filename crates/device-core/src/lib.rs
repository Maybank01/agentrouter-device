//! AgentRouter linked device: an outbound-only link that lets the person's cloud conversations use the local
//! MCP servers on this computer (files, git, terminal, Windows UI, …), within the access level chosen here.
//!
//! Protocol: agentrouter-cloud `docs/product/workspace-v1/DEVICE-PROTOCOL.md`. Design and security
//! rules: `LINKED-DEVICES.md` (§5, §6). The cloud is never trusted: every request is signature-checked
//! and then passes the local gate (access level, folders, local confirmation), and is audited.

pub mod audit;
pub mod checkpoint;
pub mod config;
pub mod confine;
pub mod consent;
pub mod device;
pub mod exec;
pub mod gate;
pub mod keystore;
pub mod link;
pub mod mcp;
pub mod mcp_pkg;
pub mod net;
pub mod protocol;
pub mod screen;
pub mod util;
pub mod workspace;
pub mod worktree;
