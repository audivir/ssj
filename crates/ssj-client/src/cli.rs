//! Command line of `ssj`.

use std::path::PathBuf;

use ssj_proto::{EnvFilter, PROTOCOL, SessionName, is_selector};

use crate::config::AutoInstall;

/// Where the server file is, as seen on the remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPath {
    /// `~/.local/share/ssj/bin/ssj-server-p<N>` following `XDG_DATA_HOME`, expanded remotely.
    Default,
    /// Taken literally and quoted for the remote shell.
    Custom(String),
}

/// How the client reaches the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Transport {
    /// UDP first, then TCP through `ssh -W`, then the `ssh` channel itself.
    #[default]
    Auto,
    Udp,
    Tcp,
    Stdio,
}

pub fn parse_transport(text: &str) -> Result<Transport, String> {
    match text {
        "auto" => Ok(Transport::Auto),
        "udp" => Ok(Transport::Udp),
        "tcp" => Ok(Transport::Tcp),
        "stdio" => Ok(Transport::Stdio),
        _ => Err("expected auto, udp, tcp, or stdio".to_owned()),
    }
}

/// What the client is asked to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Start a new session.
    Connect,
    /// Join a running session as a new client.
    Attach(Option<String>),
    /// Print the sessions on the host.
    List,
    /// Put the server on the host.
    Install,
    /// End a session on the host.
    Kill(Option<String>),
}

#[derive(Clone, Debug)]
pub struct Args {
    pub command: Command,
    /// `[user@]host` for `ssh`: the destination without the port and without brackets.
    pub destination: String,
    /// Port of the `ssh` server, when the destination names one.
    pub ssh_port: Option<u16>,
    /// Name of the session: it is joined if it runs and started otherwise.
    pub name: Option<SessionName>,
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
    pub transport: Transport,
    /// Join the only session without a client instead of starting another one.
    pub auto_attach: bool,
    /// Which of `ssh_command`, `escape`, and `send_env` the command line set, since the config
    /// file only fills in the others.
    pub explicit: Explicit,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Explicit {
    pub ssh_command: bool,
    pub escape: bool,
    pub send_env: bool,
    pub transport: bool,
    pub auto_attach: bool,
}

impl Args {
    /// Fills in what the command line left open from the config file.
    pub fn apply_config(&mut self, config: &crate::config::Config) {
        if !self.explicit.ssh_command {
            self.ssh_command = config.ssh_command.clone();
        }
        if !self.explicit.escape {
            self.escape = config.escape;
        }
        if !self.explicit.auto_attach {
            self.auto_attach = config.auto_attach;
        }
        if !self.explicit.transport {
            self.transport = config.transport;
        }
        if !self.explicit.send_env {
            self.send_env = config.send_env.clone();
        }
        if self.auto_install.is_none() {
            self.auto_install = Some(config.auto_install);
        }
    }

    /// The options that tell `ssh` the port, to go before the destination.
    pub fn ssh_port_args(&self) -> Vec<String> {
        self.ssh_port
            .map_or_else(Vec::new, |port| vec!["-p".to_owned(), port.to_string()])
    }

    /// Host part of the destination, without the user.
    pub fn host(&self) -> &str {
        self.destination
            .rsplit_once('@')
            .map_or(&self.destination, |(_, host)| host)
    }
}

/// One printable ASCII character, or `none`.
pub fn parse_escape(text: &str) -> Result<Option<u8>, String> {
    match text.as_bytes() {
        b"none" => Ok(None),
        [byte] if byte.is_ascii() && !byte.is_ascii_control() => Ok(Some(*byte)),
        _ => Err("expected one printable ASCII character or none".to_owned()),
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
    let (mut ssh_command_set, mut escape_set) = (false, false);
    let mut transport = None;
    let mut name = None;
    let mut auto_attach = None;

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
                ssh_command_set = true;
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
            "--name" => {
                name = Some(
                    SessionName::parse(&value("--name")?)
                        .ok_or("--name: 1 to 32 characters of letters, digits, '.', '_', '-'")?,
                );
            }
            "--auto-attach" => auto_attach = Some(true),
            "--no-auto-attach" => auto_attach = Some(false),
            "--transport" => {
                transport = Some(
                    parse_transport(&value("--transport")?)
                        .map_err(|e| format!("--transport: {e}"))?,
                );
            }
            "--udp-host" => udp_host = Some(value("--udp-host")?),
            "--escape-char" => {
                escape = parse_escape(&value("--escape-char")?)
                    .map_err(|e| format!("--escape-char: {e}"))?;
                escape_set = true;
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
                    Some(
                        is_selector(id)
                            .then(|| id.clone())
                            .ok_or("not a valid session id or name")?,
                    ),
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
    let parsed = ssj_proto::destination::parse(&destination)
        .map_err(|e| format!("invalid destination {destination:?}: {e}"))?;
    let send_env_set = send_env.is_some();
    let send_env = match send_env {
        Some(patterns) => EnvFilter::new(patterns).map_err(|e| format!("--send-env: {e:?}"))?,
        None => EnvFilter::default_patterns(),
    };
    Ok(Args {
        command,
        destination: parsed.target(),
        ssh_port: parsed.port,
        name,
        ssh_command,
        server_path,
        server_args,
        udp_host,
        send_env,
        auto_install,
        server_dir,
        server_binary,
        escape,
        transport: transport.unwrap_or_default(),
        auto_attach: auto_attach.unwrap_or(false),
        explicit: Explicit {
            ssh_command: ssh_command_set,
            escape: escape_set,
            send_env: send_env_set,
            transport: transport.is_some(),
            auto_attach: auto_attach.is_some(),
        },
    })
}

/// Name of the server file for this protocol version.
pub fn default_server_file() -> String {
    format!("ssj-server-p{PROTOCOL}")
}
