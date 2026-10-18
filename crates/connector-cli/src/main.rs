//! `agentrouter`: the AgentRouter helper on the command line (see `cli.rs`). The hidden
//! `mcp-serve workspace` subcommand is the device's workspace MCP service (S1; `mcp_serve.rs`).

mod cli;
#[path = "mcp_serve.rs"]
mod mcp_serve;

fn main() {
    // The cloud starts this binary as a stdio MCP server; everything before the normal CLI.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mcp-serve") {
        std::process::exit(mcp_serve::run(&args[1..]));
    }
    cli::main()
}
