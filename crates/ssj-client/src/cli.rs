//! Command line of `ssj`.

use std::path::PathBuf;

use ssj_proto::{EnvFilter, PROTOCOL, SessionId};

use crate::config::AutoInstall;

/// Where the server file is, as seen on the remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPath {
    /// `~/.local/share/ssj/bin/ssj-server-p<N>` following `XDG_DATA_HOME`, expanded remotely.
    Default,
    /// Taken literally and quoted for the remote shell.
    Custom(String),
}

/// What the client is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Start a new session.
    Connect,
    /// Join a running session as a new client.
    Attach(Option<SessionId>),
    /// Print the sessions on the host.
    List,
    /// Put the server on the host.
    Install,
    /// End a session on the host.
    Kill(Option<SessionId>),
}

#[derive(Clone, Debug)]
pub struct Args {
    pub command: Command,
    /// `[user@]host` as given.
    pub destination: String,
    /// Command that replaces `ssh`, split at whitespace.
    pub ssh_command: Vec<String>,
    pub server_path: ServerPath,
    /// Extra arguments for `ssj-server`.
    pub server_args: Vec<String>,
    /// Host name or address for the UDP connection when it differs from the destination.
    pub udp_host: Option<String>,
    pub send_env: EnvFilter,
    /// Overrides the setting of the config file.
    pub auto_install: Option<AutoInstall>,
    /// Directory with the files `ssj-server-<build>` for uploads.
    pub server_dir: Option<PathBuf>,
    /// The server file to upload, whatever the system of the host is.
    pub server_binary: Option<PathBuf>,
    /// `None` turns the escape character off.
    pub escape: Option<u8>,
}

impl Args {
    /// Host part of the destination, without the user.
    pub fn host(&self) -> &str {
        self.destination
            .rsplit_once('@')
            .map_or(&self.destination, |(_, host)| host)
    }
}

pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut positionals: Vec<String> = Vec::new();
    let mut ssh_command = vec!["ssh".to_owned()];
    let mut server_path = ServerPath::Default;
    let mut server_args = Vec::new();
    let mut udp_host = None;
    let mut send_env: Option<Vec<String>> = None;
    let mut auto_install = None;
    let mut server_dir = None;
    let mut server_binary = None;
    let mut escape = Some(b'~');

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--ssh-command" => {
                ssh_command = value("--ssh-command")?
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect();
                if ssh_command.is_empty() {
                    return Err("--ssh-command is empty".to_owned());
                }
            }
            "--server-path" => server_path = ServerPath::Custom(value("--server-path")?),
            "--server-arg" => server_args.push(value("--server-arg")?),
            "--port-range" => {
                server_args.extend(["--port-range".to_owned(), value("--port-range")?]);
            }
            "--detached-timeout" => {
                server_args.extend([
                    "--detached-timeout".to_owned(),
                    value("--detached-timeout")?,
                ]);
            }
            "--install" => auto_install = Some(AutoInstall::Always),
            "--no-install" => auto_install = Some(AutoInstall::Never),
            "--server-dir" => server_dir = Some(PathBuf::from(value("--server-dir")?)),
            "--server-binary" => server_binary = Some(PathBuf::from(value("--server-binary")?)),
            "--udp-host" => udp_host = Some(value("--udp-host")?),
            "--escape-char" => {
                let text = value("--escape-char")?;
                escape = match text.as_bytes() {
                    b"none" => None,
                    [byte] if byte.is_ascii() && !byte.is_ascii_control() => Some(*byte),
                    _ => {
                        return Err(
                            "--escape-char needs one printable ASCII character or none".to_owned()
                        );
                    }
                };
            }
            "--send-env" => send_env
                .get_or_insert_with(Vec::new)
                .push(value("--send-env")?),
            other if other.starts_with('-') => return Err(format!("unknown option {other:?}")),
            other => positionals.push(other.to_owned()),
        }
    }
    // `list`, `attach`, and `kill` are commands only when a destination follows them
    let command_word = positionals
        .first()
        .filter(|_| positionals.len() > 1)
        .map(String::as_str);
    let (command, destination) = match command_word {
        Some(word @ ("list" | "attach" | "kill" | "install")) => {
            let rest = &positionals[1..];
            let (id, destination) = match rest {
                [destination] => (None, destination.clone()),
                [id, destination] if word == "attach" || word == "kill" => (
                    Some(SessionId::parse(id).ok_or("not a valid session id")?),
                    destination.clone(),
                ),
                _ => {
                    return Err(format!(
                        "usage: ssj {word} {}user@host",
                        if matches!(word, "attach" | "kill") {
                            "[ID] "
                        } else {
                            ""
                        }
                    ));
                }
            };
            let command = match word {
                "list" => Command::List,
                "install" => Command::Install,
                "attach" => Command::Attach(id),
                _ => Command::Kill(id),
            };
            (command, destination)
        }
        _ => match positionals.as_slice() {
            [destination] => (Command::Connect, destination.clone()),
            [] => return Err("missing destination (user@host)".to_owned()),
            _ => return Err("only one destination is allowed".to_owned()),
        },
    };
    if destination.is_empty() || destination.contains(char::is_whitespace) {
        return Err("invalid destination".to_owned());
    }
    let send_env = match send_env {
        Some(patterns) => EnvFilter::new(patterns).map_err(|e| format!("--send-env: {e:?}"))?,
        None => EnvFilter::default_patterns(),
    };
    Ok(Args {
        command,
        destination,
        ssh_command,
        server_path,
        server_args,
        udp_host,
        send_env,
        auto_install,
        server_dir,
        server_binary,
        escape,
    })
}

/// Name of the server file for this protocol version.
pub fn default_server_file() -> String {
    format!("ssj-server-p{PROTOCOL}")
}
