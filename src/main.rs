mod audit;
mod bridge;
mod cli;
mod ear;
mod eye;
mod hand;
mod indicator;
mod mcp;
mod safety;
mod setup;
mod state;
mod types;
mod watch;

use clap::Parser;
use cli::Cli;

fn main() {
    let cli = Cli::parse();
    if let Err(err) = cli::run(cli) {
        eprintln!("eyra error: {:#}", err);
        std::process::exit(1);
    }
}
