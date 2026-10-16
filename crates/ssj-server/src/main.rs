use std::process::ExitCode;

use ssj_server::cli::{self, Mode};
use ssj_server::{control_cli, launch};

fn main() -> ExitCode {
    match cli::parse(std::env::args_os().skip(1)) {
        Ok(args) if args.mode == Mode::Version => {
            println!(
                "ssj-server {} protocol {}",
                env!("CARGO_PKG_VERSION"),
                ssj_proto::PROTOCOL
            );
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("ssj-server: {message}");
            ExitCode::from(2)
        }
        Ok(args) if args.mode != Mode::Serve => control_cli::run(&args),
        Ok(args) if args.daemon_child => launch::run_child(args),
        Ok(args) => launch::run_parent(&args),
    }
}
