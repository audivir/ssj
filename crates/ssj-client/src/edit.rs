//! `ssj edit`: change `client.toml` here, or `server.toml` on a host, in `$EDITOR`.
//!
//! The text is edited in a private temporary file and judged as soon as the editor ends. A file
//! that is not valid is never saved: the error is shown and the editor can be opened again, as
//! `visudo` does. The local file is judged by the client, the remote one by `ssj-server
//! --check-config` on the host, since only the server knows its own keys and defaults.
//!
//! Two editors at the same time must not lose each other's work. The save is a compare and swap: it
//! happens only if the file still has the content that the editor started from (compared by its
//! SHA-256), under a lock (a `flock` here, a lock directory on the host) that makes the comparison
//! and the replacement one step. If the file changed meanwhile, nothing is overwritten, and the
//! edited text is kept in a file whose name is shown.

use std::fs;
use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::bootstrap::{run_remote, run_remote_command, sh_command};
use crate::cli::Args;
use crate::config::Config;
use crate::install::sha256_hex;
use crate::vlog;

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

/// Why a text could not be saved.
enum SaveError {
    /// The file is not what the editor started from.
    Changed,
    Other(String),
}

/// Keeps the edited text in a private file of its own, so that a refused save loses nothing.
fn keep_copy(text: &str) -> Option<PathBuf> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let path =
        std::env::temp_dir().join(format!("ssj-edit-kept-{}-{nanos}.toml", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    file.write_all(text.as_bytes()).ok()?;
    Some(path)
}

/// The message for a save that was refused because the file changed, with the place of the copy.
fn changed_message(what: &str, text: &str) -> String {
    match keep_copy(text) {
        Some(copy) => format!(
            "{what} changed while you were editing, so it was not overwritten. Your text is in {}; \
             run the command again and apply it by hand",
            copy.display()
        ),
        None => format!("{what} changed while you were editing, so it was not overwritten"),
    }
}

/// Writes `text` to `path` if the file still holds `original` (nothing, if it does not exist),
/// through a file in the same directory (mode 0600), so that a reader never sees half of it. A
/// lock on a file next to it makes the comparison and the replacement one step.
fn save_local(path: &Path, original: &str, text: &str) -> Result<(), SaveError> {
    let other = |what: &str, err: std::io::Error| SaveError::Other(format!("{what}: {err}"));
    let dir = path
        .parent()
        .ok_or_else(|| SaveError::Other("the config file has no directory".to_owned()))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| other(&format!("cannot make {}", dir.display()), e))?;
    let lock_path = dir.join(".client.toml.lock");
    let lock = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| other(&format!("cannot open {}", lock_path.display()), e))?;
    // another editor holds it for the blink of a save, so a few tries are enough.
    let mut tries = 0;
    while rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err() {
        tries += 1;
        if tries > 50 {
            return Err(SaveError::Other(format!(
                "{} is locked by another ssj edit",
                lock_path.display()
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let current = match fs::read_to_string(path) {
        Ok(current) => current,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(other(&format!("cannot read {}", path.display()), err)),
    };
    if sha256_hex(current.as_bytes()) != sha256_hex(original.as_bytes()) {
        return Err(SaveError::Changed);
    }
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
        return Err(other(&format!("cannot save {}", path.display()), err));
    }
    // the lock goes with the file handle.
    drop(lock);
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
    let original = ssj_config::read_file("client.toml")?
        .map(|(_, text)| text)
        .unwrap_or_default();
    let current = if original.is_empty() {
        Config::template()
    } else {
        original.clone()
    };
    let edited = edit_text(&current, |text| async move {
        Config::from_text(&text).map(|_| ())
    })
    .await?;
    match edited {
        Some(text) => match save_local(&path, &original, &text) {
            Ok(()) => Ok(format!("saved {}", path.display())),
            Err(SaveError::Changed) => Err(changed_message(&path.display().to_string(), &text)),
            Err(SaveError::Other(message)) => Err(message),
        },
        None => Ok("no changes".to_owned()),
    }
}

/// Reads `server.toml` on the host, in the directory that the server reads it from.
const READ_SCRIPT: &str =
    r#"cat "${XDG_CONFIG_HOME:-$HOME/.config}/ssj/server.toml" 2>/dev/null; exit 0"#;

/// Writes stdin to `server.toml` if the file still has the content that the editor started from.
/// The first line of stdin is its SHA-256, the rest is the new text. A lock directory (`mkdir` is
/// atomic) makes the comparison and the replacement one step. The exit codes: 3 no tool for the
/// digest, 4 the lock is held, 5 the file changed.
const WRITE_SCRIPT: &str = r#"set -e
umask 077
dir="${XDG_CONFIG_HOME:-$HOME/.config}/ssj"
mkdir -p "$dir"
file="$dir/server.toml"
lock="$dir/.server.toml.lock"
IFS= read -r want
tries=0
until mkdir "$lock" 2>/dev/null; do
  tries=$((tries+1))
  if [ "$tries" -gt 5 ]; then
    echo "another ssj edit holds $lock; remove that directory if no editor runs" >&2
    exit 4
  fi
  sleep 1
done
tmp="$dir/.server.toml.$$"
trap 'rm -rf "$tmp" "$lock"' EXIT
trap 'exit 1' HUP INT TERM
cat > "$tmp"
if command -v sha256sum >/dev/null 2>&1; then
  got=$(cat "$file" 2>/dev/null | sha256sum | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  got=$(cat "$file" 2>/dev/null | shasum -a 256 | cut -d' ' -f1)
else
  echo 'neither sha256sum nor shasum is available' >&2
  exit 3
fi
if [ "$got" != "$want" ]; then
  exit 5
fi
mv -f "$tmp" "$file"
echo "SSJ SAVED $file""#;

async fn edit_remote(args: &Args) -> Result<String, String> {
    let current = run_remote_command(args, &sh_command(READ_SCRIPT, &[]), Vec::new()).await?;
    if !current.status.success() {
        return Err(format!(
            "could not read the file on {}: {}",
            args.destination,
            current.error_text()
        ));
    }
    // the digest is of the bytes as they are on the host.
    let original_digest = sha256_hex(&current.stdout);
    let original = String::from_utf8_lossy(&current.stdout).into_owned();
    let initial = if original.trim().is_empty() {
        // the server of the host knows its own keys and defaults.
        let template = run_remote(args, &["--config-template".to_owned()], "").await?;
        if template.server_missing() {
            return Err(format!(
                "ssj-server is not installed on {}; run `ssj install {}` first",
                args.destination, args.destination
            ));
        }
        if !template.status.success() {
            return Err(format!(
                "could not get the template from {}: {}",
                args.destination,
                template.error_text()
            ));
        }
        String::from_utf8_lossy(&template.stdout).into_owned()
    } else {
        original.clone()
    };
    // the server of the host judges the text, before anything is saved.
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
    let mut stdin = format!("{original_digest}\n").into_bytes();
    stdin.extend_from_slice(text.as_bytes());
    let saved = run_remote_command(args, &sh_command(WRITE_SCRIPT, &[]), stdin).await?;
    if saved.status.code() == Some(5) {
        return Err(changed_message(
            &format!("server.toml on {}", args.destination),
            &text,
        ));
    }
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
