//! AgentRouter device core: what the console mode keeps from the device side — pairing and the token
//! (`net`, `link`, `keystore`), the protocol primitives, the argument-level audit, pinned MCP packages
//! (`mcp_pkg`), config and helpers. The link is `agentrouter-hostd-link`, the MCP endpoint `agentrouter-aggregator`
//! (docs/HOSTD-MCP.md). `exec` and `shell` run the host server's commands and terminals.

pub mod audit;
pub mod capacity;
pub mod command_text;
pub mod config;
pub mod errors;
pub mod exec;
pub mod keystore;
pub mod limits;
pub mod link;
pub mod local_data;
pub mod local_secrets;
pub mod lockfile;
pub mod mcp_pkg;
pub mod net;
pub mod ports;
pub mod protocol;
pub mod shell;
pub mod shell_env;
pub mod software;
pub mod util;
