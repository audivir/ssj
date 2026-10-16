use std::process::ExitCode;

use ssj_client::{cli, run};

fn main() -> ExitCode {
    match cli::parse(std::env::args().skip(1)) {
        Err(message) => {
            eprintln!("ssj: {message}");
            ExitCode::from(2)
        }
        Ok(args) => run::run(args),
    }
}
