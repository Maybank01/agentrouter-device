//! AgentRouter linked device: an outbound-only agent that lets the person's cloud conversations run
//! commands and read and write files on this computer, within the access level chosen here.
//!
//! Protocol: agentrouter-cloud `docs/product/workspace-v1/DEVICE-PROTOCOL.md`. Design and security
//! rules: `LINKED-DEVICES.md` (§5, §6, §14, §17). The cloud is never trusted: every request is
//! signature-checked and then passes the local gate (access level, folders, local confirmation), the
//! presence gate (pause, the "being controlled" indicator) and is audited. AIs on this computer reach
//! it through the local MCP (`mcp`, `local_ipc`), current user only.

pub mod audit;
pub mod config;
pub mod consent;
pub mod device;
pub mod gate;
pub mod jobs;
pub mod keystore;
pub mod link;
pub mod local_ipc;
pub mod mcp;
pub mod net;
pub mod presence;
pub mod protocol;
pub mod share;
pub mod util;
