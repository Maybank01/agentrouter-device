//! `agentrouter`: console mode on the command line (see `cli.rs`, `console.rs`). The hidden `mcp-serve local`
//! subcommand is the local-Agent stdio MCP entry (`mcp_serve.rs`), kept pending its design (LINKER-MCP §4).

mod cli;
mod console;
mod host;
#[path = "mcp_serve.rs"]
mod mcp_serve;

fn main() {
    // A local Agent starts this binary as a stdio MCP server; everything before the normal CLI.
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The link's own SSH-like server (exec/read/write), started by the console inside the sandbox (host.rs).
    if args.first().map(String::as_str) == Some("mcp-serve")
        && args.get(1).map(String::as_str) == Some("host")
    {
        std::process::exit(host::run(&args[2..]));
    }
    if args.first().map(String::as_str) == Some("mcp-serve") {
        std::process::exit(mcp_serve::run(&args[1..]));
    }
    cli::main()
}
