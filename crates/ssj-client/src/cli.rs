//! Command line of `ssj`.

use ssj_proto::{EnvFilter, PROTOCOL};

/// Where the server file is, as seen on the remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPath {
    /// `~/.local/share/ssj/bin/ssj-server-p<N>` following `XDG_DATA_HOME`, expanded remotely.
    Default,
    /// Taken literally and quoted for the remote shell.
    Custom(String),
}

#[derive(Clone, Debug)]
pub struct Args {
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
    let mut destination = None;
    let mut ssh_command = vec!["ssh".to_owned()];
    let mut server_path = ServerPath::Default;
    let mut server_args = Vec::new();
    let mut udp_host = None;
    let mut send_env: Option<Vec<String>> = None;

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
            "--udp-host" => udp_host = Some(value("--udp-host")?),
            "--send-env" => send_env
                .get_or_insert_with(Vec::new)
                .push(value("--send-env")?),
            other if other.starts_with('-') => return Err(format!("unknown option {other:?}")),
            other => {
                if destination.replace(other.to_owned()).is_some() {
                    return Err("only one destination is allowed".to_owned());
                }
            }
        }
    }
    let destination = destination.ok_or("missing destination (user@host)")?;
    if destination.is_empty() || destination.contains(char::is_whitespace) {
        return Err("invalid destination".to_owned());
    }
    let send_env = match send_env {
        Some(patterns) => EnvFilter::new(patterns).map_err(|e| format!("--send-env: {e:?}"))?,
        None => EnvFilter::default_patterns(),
    };
    Ok(Args {
        destination,
        ssh_command,
        server_path,
        server_args,
        udp_host,
        send_env,
    })
}

/// Name of the server file for this protocol version.
pub fn default_server_file() -> String {
    format!("ssj-server-p{PROTOCOL}")
}
