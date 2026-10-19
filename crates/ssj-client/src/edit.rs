//! `ssj edit`: change `client.toml` here, or `server.toml` on a host, in `$EDITOR`.
//!
//! The text is edited in a private temporary file and judged as soon as the editor ends. A file
//! that is not valid is never saved: the error is shown and the editor can be opened again, as
//! `visudo` does. The local file is judged by the client, the remote one by `ssj-server
//! --check-config` on the host, since only the server knows its own keys.

use std::fs;
use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bootstrap::{run_remote, run_remote_command, sh_command};
use crate::cli::Args;
use crate::config::Config;
use crate::vlog;

/// What a new `client.toml` starts with: every key, as a comment with its default.
pub const CLIENT_TEMPLATE: &str = "\
# Settings of ssj, read at every start. Flags on the command line override them.
# auto_install = \"ask\"            # \"ask\", \"always\", or \"never\"
# escape_char = \"~\"               # one character, or \"none\"
# ssh_command = \"ssh -J jump\"     # replaces ssh
# send_env = [\"TERM\", \"LANG\", \"LC_*\"]
# transport = \"auto\"              # \"auto\", \"udp\", \"tcp\", or \"stdio\"
# auto_attach = false             # join the only session without a client
# local_echo = \"off\"              # \"off\" or \"auto\": show typed characters at once
# compress = \"off\"                # \"off\" or \"auto\": compress while the link is the limit
# congestion = \"bbr\"              # \"bbr\" or \"cubic\"
# forward_agent = false           # let programs on the host use your ssh agent
";

/// What a new `server.toml` starts with.
pub const SERVER_TEMPLATE: &str = "\
# Settings of ssj-server, read when a session starts. Flags of the client override them.
# port_range = \"60000:61000\"       # UDP ports the server may use, \"0\" for any
# detached_timeout = \"none\"        # seconds without a client, or \"none\"
# max_detached = \"none\"            # limit even after a deliberate detach
# replay_capacity = 4194304        # bytes of output kept for a client that is away
# slow_client = \"drop\"             # \"drop\" or \"wait\"
# compress = \"off\"                 # \"off\" or \"auto\"
# congestion = \"bbr\"               # \"bbr\" or \"cubic\"
# live_backlog = 1048576           # bytes that may wait for a connected client (drop)
# away_after = 10                  # seconds a silent client may block the output (wait)
# shell = \"/bin/zsh\"               # default: $SHELL
# accept_env = [\"TERM\", \"LANG\", \"LC_*\"]
";

/// The editor to run: `$EDITOR`, else `$VISUAL`, else `vi`.
pub fn editor() -> String {
    ["EDITOR", "VISUAL"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "vi".to_owned())
}

/// Runs the editor on `file` and waits. The editor may have arguments (`code -w`), so the shell
/// splits the command, as it does for `git`.
fn run_editor(editor: &str, file: &Path) -> Result<(), String> {
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("ssj-edit")
        .arg(file)
        .status()
        .map_err(|e| format!("cannot run the editor {editor:?}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("the editor {editor:?} ended with {status}"))
    }
}

/// A private temporary file with the text. The name ends in `.toml`, for the highlighting.
fn temp_file(text: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("ssj-edit-{}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let path = dir.join("edit.toml");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| format!("cannot make {}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

fn remove_temp(path: &Path) {
    let _ = fs::remove_file(path);
    if let Some(dir) = path.parent() {
        let _ = fs::remove_dir(dir);
    }
}

/// Whether to open the editor again after an error. Anything but `d` or `n` means yes.
fn ask_again() -> bool {
    eprint!("Edit again? [Y/n] (n discards the changes) ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() || answer.is_empty() {
        return false;
    }
    !matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "n" | "no" | "d"
    )
}

/// The loop: edit, judge, and ask again if the text is not valid. `Ok(None)` if nothing changed or
/// the user gave up.
async fn edit_text<F, Fut>(initial: &str, mut judge: F) -> Result<Option<String>, String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let editor = editor();
    let path = temp_file(initial)?;
    let result = async {
        loop {
            vlog!(1, "running the editor {editor:?} on {}", path.display());
            run_editor(&editor, &path)?;
            let text = fs::read_to_string(&path)
                .map_err(|e| format!("cannot read the edited file: {e}"))?;
            if text == initial {
                return Ok(None);
            }
            match judge(text.clone()).await {
                Ok(()) => return Ok(Some(text)),
                Err(message) => {
                    eprintln!("ssj: the file is not valid, and not saved: {message}");
                    if !ask_again() {
                        return Ok(None);
                    }
                }
            }
        }
    }
    .await;
    remove_temp(&path);
    result
}

/// Writes `text` to `path` through a file in the same directory (mode 0600), so that a reader
/// never sees half of it.
fn save_local(path: &Path, text: &str) -> Result<(), String> {
    let dir = path.parent().ok_or("the config file has no directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot make {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(".client.toml.{}", std::process::id()));
    let written = (|| -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, path)
    })();
    if let Err(err) = written {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot save {}: {err}", path.display()));
    }
    Ok(())
}

/// `ssj edit`, `ssj edit user@host`. Returns the exit code.
pub async fn run(args: &Args) -> u8 {
    let result = if args.destination.is_empty() {
        edit_local().await
    } else {
        edit_remote(args).await
    };
    match result {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            1
        }
    }
}

async fn edit_local() -> Result<String, String> {
    let path = ssj_config::path("client.toml")
        .ok_or("neither XDG_CONFIG_HOME nor HOME is set, so there is no config file")?;
    let current = match ssj_config::read_file("client.toml")? {
        Some((_, text)) => text,
        None => CLIENT_TEMPLATE.to_owned(),
    };
    let edited = edit_text(&current, |text| async move {
        Config::from_text(&text).map(|_| ())
    })
    .await?;
    match edited {
        Some(text) => {
            save_local(&path, &text)?;
            Ok(format!("saved {}", path.display()))
        }
        None => Ok("no changes".to_owned()),
    }
}

/// Reads `server.toml` on the host, in the directory that the server reads it from.
const READ_SCRIPT: &str =
    r#"cat "${XDG_CONFIG_HOME:-$HOME/.config}/ssj/server.toml" 2>/dev/null; exit 0"#;

/// Writes stdin to `server.toml` through a file in the same directory.
const WRITE_SCRIPT: &str = r#"set -e
umask 077
dir="${XDG_CONFIG_HOME:-$HOME/.config}/ssj"
mkdir -p "$dir"
tmp="$dir/.server.toml.$$"
trap 'rm -f "$tmp"' EXIT
cat > "$tmp"
mv -f "$tmp" "$dir/server.toml"
trap - EXIT
echo "SSJ SAVED $dir/server.toml""#;

async fn edit_remote(args: &Args) -> Result<String, String> {
    let current = run_remote_command(args, &sh_command(READ_SCRIPT, &[]), Vec::new()).await?;
    if !current.status.success() {
        return Err(format!(
            "could not read the file on {}: {}",
            args.destination,
            current.error_text()
        ));
    }
    let text = String::from_utf8_lossy(&current.stdout).into_owned();
    let initial = if text.trim().is_empty() {
        SERVER_TEMPLATE.to_owned()
    } else {
        text
    };
    // the server of the host judges the text, before anything is saved
    let edited = edit_text(&initial, |text| {
        let args = args.clone();
        async move {
            let checked = run_remote(&args, &["--check-config".to_owned()], &text).await?;
            if checked.server_missing() {
                return Err(format!(
                    "ssj-server is not installed on {}; run `ssj install {}` first",
                    args.destination, args.destination
                ));
            }
            if checked.status.success() {
                Ok(())
            } else {
                Err(checked.error_text())
            }
        }
    })
    .await?;
    let Some(text) = edited else {
        return Ok("no changes".to_owned());
    };
    let saved = run_remote_command(args, &sh_command(WRITE_SCRIPT, &[]), text.into_bytes()).await?;
    if !saved.status.success() {
        return Err(format!(
            "could not save the file on {}: {}",
            args.destination,
            saved.error_text()
        ));
    }
    Ok(format!(
        "saved server.toml on {} (running sessions keep their settings; new ones use the file)",
        args.destination
    ))
}
