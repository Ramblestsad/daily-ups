use clap::Parser;
use daily_ups::{Cli, run};
use std::process;

fn main() {
    let cli = Cli::parse();

    match run(cli) {
        Ok(code) => process::exit(code),
        Err(error) => {
            eprintln!("daily-ups: {error}");
            process::exit(1);
        }
    }
}
