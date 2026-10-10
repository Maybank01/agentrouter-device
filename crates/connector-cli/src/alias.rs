//! `agentrouter-device`: the earlier name of `agentrouter`, kept so existing scripts keep working.

#[path = "cli.rs"]
mod cli;
#[path = "prompt.rs"]
mod prompt;

fn main() {
    cli::main()
}
