//! AgentRouter device core: what the console mode keeps from the device side — pairing and the token
//! (`net`, `link`, `keystore`), the protocol primitives, the argument-level audit, pinned MCP packages
//! (`mcp_pkg`), config and helpers. The link is `agentrouter-hostd-link`, the MCP endpoint `agentrouter-aggregator`
//! (docs/HOSTD-MCP.md). `exec`, `shell`, `confine`/`confined`, `consent`, `deploy`, `workspace`, `worktree`,
//! `gate`, `lockfile` and `local_agent` stay only for `agentrouter mcp-serve local` (local Agents, pending the
//! communication design, HOSTD-MCP §4).

pub mod audit;
pub mod config;
pub mod confine;
#[cfg(windows)]
pub mod confined;
pub mod consent;
pub mod deploy;
pub mod exec;
pub mod gate;
pub mod keystore;
pub mod link;
pub mod local_agent;
pub mod lockfile;
pub mod mcp_pkg;
pub mod net;
pub mod protocol;
pub mod shell;
pub mod shell_env;
pub mod util;
pub mod workspace;
pub mod worktree;
