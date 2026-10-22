//! Writes the man page and the shell completions of `ssj` to the directory given.

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let Some(dir) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: ssj-assets DIR");
        return ExitCode::from(2);
    };
    let written = std::fs::create_dir_all(dir.join("completions"))
        .and_then(|()| ssj_assets::write(&dir))
        .and_then(|()| {
            // the release puts the completions in a directory of their own.
            for file in ["ssj.bash", "_ssj", "ssj.fish"] {
                std::fs::rename(dir.join(file), dir.join("completions").join(file))?;
            }
            Ok(())
        });
    match written {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ssj-assets: {err}");
            ExitCode::from(1)
        }
    }
}
