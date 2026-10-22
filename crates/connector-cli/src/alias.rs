//! `agentrouter-device`: the earlier name of `agentrouter`, kept so existing scripts keep working.

#[path = "cli.rs"]
mod cli;
mod cloud;
mod console;
#[path = "mcp_serve.rs"]
mod mcp_serve;
mod service;
mod start;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mcp-serve") {
        std::process::exit(mcp_serve::run(&args[1..]));
    }
    cli::main()
}
