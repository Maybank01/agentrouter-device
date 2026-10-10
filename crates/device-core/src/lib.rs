//! AgentRouter linked device: an outbound-only agent that lets the person's cloud conversations run
//! commands and read and write files on this computer, within the access level chosen here.
//!
//! Protocol: agentrouter-cloud `docs/product/workspace-v1/DEVICE-PROTOCOL.md`. Design and security
//! rules: `LINKED-DEVICES.md` (§5, §6). The cloud is never trusted: every request is signature-checked
//! and then passes the local gate (access level, folders, local confirmation), and is audited.

pub mod allowlist;
pub mod approvals;
pub mod audit;
pub mod checkpoint;
pub mod config;
pub mod consent;
pub mod device;
pub mod edit;
pub mod gate;
pub mod jobs;
pub mod keystore;
pub mod link;
pub mod net;
pub mod protocol;
pub mod scope_guard;
pub mod search;
pub mod util;
