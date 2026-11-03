//! `agentrouter`: console mode on the command line (see `cli.rs`, `console.rs`). `mcp-serve host` is the link's
//! host server, started by the console and by cloud sandboxes inside the sandbox (`host/`, HOSTD-MCP §3.7).

mod cli;
mod cloud;
mod console;
mod events;
mod host;
mod leases;
mod service;
mod start;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mcp-serve")
        && args.get(1).map(String::as_str) == Some("host")
    {
        std::process::exit(host::run(&args[2..]));
    }
    cli::main()
}
