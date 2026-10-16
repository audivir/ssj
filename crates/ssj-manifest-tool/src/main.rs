//! Fills the manifest slot of a client binary with the digests of server files.
//!
//! `ssj-manifest patch CLIENT SERVER...` names each entry after the build in the file name
//! `ssj-server-<build>`. `check` verifies a patched client, `show` prints its entries, and
//! `digest` prints the SHA-256 of the client with the slot content ignored.

use std::path::Path;
use std::process::ExitCode;

use ssj_manifest::{Entry, Manifest, digest, hex};
use ssj_manifest_tool::{digest_ignoring_slot, patch, read};

fn build_name(path: &Path) -> Result<String, String> {
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("a path without a file name")?;
    file.strip_prefix("ssj-server-")
        .map(str::to_owned)
        .ok_or_else(|| format!("{file}: the file name must start with ssj-server-"))
}

fn manifest_of(servers: &[String]) -> Result<Manifest, String> {
    let mut entries = Vec::new();
    for server in servers {
        let bytes = std::fs::read(server).map_err(|e| format!("{server}: {e}"))?;
        entries.push(Entry {
            name: build_name(Path::new(server))?,
            digest: digest(&bytes),
        });
    }
    Manifest::new(entries).map_err(|e| e.to_string())
}

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [command, client, servers @ ..] if command == "patch" && !servers.is_empty() => {
            let manifest = manifest_of(servers)?;
            let mut bytes = std::fs::read(client).map_err(|e| format!("{client}: {e}"))?;
            patch(&mut bytes, &manifest).map_err(|e| format!("{client}: {e}"))?;
            std::fs::write(client, bytes).map_err(|e| format!("{client}: {e}"))?;
            for entry in manifest.entries() {
                println!("{}  {}", hex(&entry.digest), entry.name);
            }
            Ok(())
        }
        [command, client, servers @ ..] if command == "check" && !servers.is_empty() => {
            let expected = manifest_of(servers)?;
            let bytes = std::fs::read(client).map_err(|e| format!("{client}: {e}"))?;
            let found = read(&bytes).map_err(|e| format!("{client}: {e}"))?;
            if found == expected {
                Ok(())
            } else {
                Err(format!(
                    "{client}: the slot does not hold the digests of the given servers"
                ))
            }
        }
        [command, client] if command == "show" => {
            let bytes = std::fs::read(client).map_err(|e| format!("{client}: {e}"))?;
            let manifest = read(&bytes).map_err(|e| format!("{client}: {e}"))?;
            if manifest.is_empty() {
                println!("(empty: this client has not been patched)");
            }
            for entry in manifest.entries() {
                println!("{}  {}", hex(&entry.digest), entry.name);
            }
            Ok(())
        }
        [command, client] if command == "digest" => {
            let bytes = std::fs::read(client).map_err(|e| format!("{client}: {e}"))?;
            println!(
                "{}",
                hex(&digest_ignoring_slot(&bytes).map_err(|e| format!("{client}: {e}"))?)
            );
            Ok(())
        }
        _ => Err(
            "usage: ssj-manifest patch|check CLIENT SERVER... | show CLIENT | digest CLIENT"
                .to_owned(),
        ),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("ssj-manifest: {message}");
            ExitCode::from(1)
        }
    }
}
