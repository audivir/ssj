//! Putting the server on a host: finding the right file, uploading it, and deciding when to.

use std::io::{BufRead, IsTerminal};
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use ssj_proto::bootstrap::sanitize;

use crate::bootstrap::{run_remote_command, sh_command};
use crate::cli::Args;
use crate::config::AutoInstall;

/// Largest server file the client uploads.
const MAX_BINARY: u64 = 64 * 1024 * 1024;

/// Systems that have a server build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    LinuxX86_64,
    LinuxAarch64,
    MacArm64,
}

impl Target {
    /// Name of the build. Linux builds are static (musl) and run on any distribution.
    pub fn triple(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "x86_64-unknown-linux-musl",
            Self::LinuxAarch64 => "aarch64-unknown-linux-musl",
            Self::MacArm64 => "aarch64-apple-darwin",
        }
    }
}

/// Reads the answer of `uname -sm`, which has to be in a line of its own: `SSJ TARGET <os> <arch>`.
pub fn parse_target(stdout: &[u8]) -> Result<Target, String> {
    let lines: Vec<&[u8]> = stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_prefix(b"SSJ TARGET "))
        .collect();
    let [system] = lines[..] else {
        return Err("the host did not report its system".to_owned());
    };
    match system {
        b"Linux x86_64" => Ok(Target::LinuxX86_64),
        b"Linux aarch64" => Ok(Target::LinuxAarch64),
        b"Darwin arm64" => Ok(Target::MacArm64),
        other => Err(format!(
            "there is no server for the system {:?}",
            sanitize(other, 60)
        )),
    }
}

pub async fn detect_target(args: &Args) -> Result<Target, String> {
    let command = sh_command(r#"printf 'SSJ TARGET %s\n' "$(uname -sm)""#, &[]);
    let remote = run_remote_command(args, &command, Vec::new()).await?;
    if !remote.status.success() {
        return Err(format!(
            "could not ask the host for its system: {}",
            remote.error_text()
        ));
    }
    parse_target(&remote.stdout)
}

/// The server file for `target`: the explicit one, or `ssj-server-<build>` in the server
/// directory, which defaults to the directory of the client.
pub fn find_server_binary(args: &Args, target: Target) -> Result<PathBuf, String> {
    if let Some(file) = &args.server_binary {
        return Ok(file.clone());
    }
    let dir = match &args.server_dir {
        Some(dir) => dir.clone(),
        None => std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(PathBuf::from))
            .ok_or("cannot find the directory of the client")?,
    };
    let path = dir.join(format!("ssj-server-{}", target.triple()));
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "there is no server build for this host at {}; put the file {} from the release there, \
             or point to it with --server-dir or --server-binary",
            path.display(),
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Runs on the host. The first line of stdin is the expected SHA-256, the rest is the file.
///
/// The file is checked, tested, and moved into place only then, so that a broken upload never
/// replaces a working server.
const INSTALL_SCRIPT: &str = r#"set -e
umask 077
dir="${XDG_DATA_HOME:-$HOME/.local/share}/ssj/bin"
mkdir -p "$dir"
tmp="$dir/.ssj-server-p__PROTOCOL__.$$"
trap 'rm -f "$tmp"' EXIT
IFS= read -r want
cat > "$tmp"
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp" | cut -d' ' -f1)
elif command -v shasum >/dev/null 2>&1; then
  got=$(shasum -a 256 "$tmp" | cut -d' ' -f1)
else
  echo 'neither sha256sum nor shasum is available, so the upload cannot be verified' >&2
  exit 3
fi
if [ "$got" != "$want" ]; then
  echo 'the uploaded file does not match its digest' >&2
  exit 4
fi
chmod 700 "$tmp"
if ! "$tmp" --version >/dev/null 2>&1; then
  echo "the uploaded server cannot run in $dir (is it on a noexec mount?)" >&2
  exit 5
fi
mv -f "$tmp" "$dir/ssj-server-p__PROTOCOL__"
trap - EXIT
echo "SSJ INSTALLED $dir/ssj-server-p__PROTOCOL__""#;

/// Compares the digest of the file with the one this client was released with.
///
/// A client from a release has the digests of all server files inside, so a file from another
/// release, a damaged one, or a swapped one is refused. A build that was not patched has none, and
/// an explicit `--server-binary` is the choice of the user, so both go ahead with a note.
fn check_against_manifest(
    args: &Args,
    target: Target,
    digest: &str,
    file: &std::path::Path,
) -> Result<(), String> {
    let manifest = crate::manifest_slot::embedded()?;
    let recorded = manifest
        .lookup(target.triple())
        .map(|d| ssj_manifest::hex(&d));
    if args.server_binary.is_some() {
        if recorded.as_deref() != Some(digest) {
            eprintln!(
                "ssj: note: {} is not checked against the manifest of this client",
                file.display()
            );
        }
        return Ok(());
    }
    if manifest.is_empty() {
        eprintln!(
            "ssj: warning: this client has no manifest of server digests (a development build), \
             so {} is not checked against a release",
            file.display()
        );
        return Ok(());
    }
    match recorded {
        Some(expected) if expected == digest => Ok(()),
        Some(expected) => Err(format!(
            "{} does not match the digest that this client was released with (expected {expected}, \
             found {digest}); it may belong to another release, or it was changed",
            file.display()
        )),
        None => Err(format!(
            "this client has no digest for the build {}",
            target.triple()
        )),
    }
}

/// Uploads the server file for the host and returns the path it was installed at.
pub async fn install(args: &Args) -> Result<String, String> {
    let target = detect_target(args).await?;
    let file = find_server_binary(args, target)?;
    let size = std::fs::metadata(&file)
        .map_err(|e| format!("cannot read {}: {e}", file.display()))?
        .len();
    if size == 0 || size > MAX_BINARY {
        return Err(format!(
            "{} has an implausible size of {size} bytes",
            file.display()
        ));
    }
    let binary =
        std::fs::read(&file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;

    let digest = sha256_hex(&binary);
    check_against_manifest(args, target, &digest, &file)?;

    let mut stdin = format!("{digest}\n").into_bytes();
    stdin.extend_from_slice(&binary);
    let script = INSTALL_SCRIPT.replace("__PROTOCOL__", &ssj_proto::PROTOCOL.to_string());
    let remote = run_remote_command(args, &sh_command(&script, &[]), stdin).await?;
    if !remote.status.success() {
        return Err(format!(
            "the installation failed ({}): {}",
            remote.status,
            remote.error_text()
        ));
    }
    let installed: Vec<&[u8]> = remote
        .stdout
        .split(|b| *b == b'\n')
        .filter_map(|line| line.strip_prefix(b"SSJ INSTALLED "))
        .collect();
    match installed[..] {
        [path] => Ok(sanitize(path, 300)),
        _ => Err("the host did not confirm the installation".to_owned()),
    }
}

/// What to do about a missing server.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Install,
    /// Do not install, and say why and what to do instead.
    Skip(String),
}

/// Applies the policy. `ask` is only called when a person can be asked.
pub fn decide(
    policy: AutoInstall,
    destination: &str,
    interactive: bool,
    ask: impl FnOnce(&str) -> bool,
) -> Decision {
    let hint = format!("run `ssj install {destination}` to install it");
    match policy {
        AutoInstall::Always => Decision::Install,
        AutoInstall::Never => Decision::Skip(format!(
            "ssj-server is not installed on {destination}; {hint}"
        )),
        AutoInstall::Ask if !interactive => Decision::Skip(format!(
            "ssj-server is not installed on {destination}, and there is no terminal to ask; \
             {hint}, or use --install"
        )),
        AutoInstall::Ask => {
            let question =
                format!("ssj-server is not installed on {destination}. Install it now? [y/N] ");
            if ask(&question) {
                Decision::Install
            } else {
                Decision::Skip(format!("not installed; {hint}"))
            }
        }
    }
}

/// Whether a person can answer: both input and output are terminals.
pub fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Asks on the terminal. Only `y` and `yes` count as consent.
pub fn confirm(question: &str) -> bool {
    eprint!("{question}");
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
