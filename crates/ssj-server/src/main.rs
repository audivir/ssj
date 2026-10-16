use std::process::ExitCode;

use ssj_server::{cli, launch};

fn main() -> ExitCode {
    match cli::parse(std::env::args_os().skip(1)) {
        Err(message) => {
            eprintln!("ssj-server: {message}");
            ExitCode::from(2)
        }
        Ok(args) if args.daemon_child => launch::run_child(args),
        Ok(args) => launch::run_parent(&args),
    }
}
