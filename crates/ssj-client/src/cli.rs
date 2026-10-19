//! Command line of `ssj`.

use std::path::PathBuf;

use ssj_proto::{EnvFilter, PROTOCOL, SessionName, is_selector};

use crate::config::AutoInstall;
use clap::FromArgMatches;

use crate::options::Options;

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

/// Whether typed characters are shown before the server has echoed them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LocalEcho {
    #[default]
    Off,
    Auto,
}

pub fn parse_local_echo(text: &str) -> Result<LocalEcho, String> {
    match text {
        "off" => Ok(LocalEcho::Off),
        "auto" => Ok(LocalEcho::Auto),
        _ => Err("expected off or auto".to_owned()),
    }
}

pub fn parse_compress(text: &str) -> Result<bool, String> {
    match text {
        "auto" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected auto or off".to_owned()),
    }
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
    /// Edit `client.toml` here, or `server.toml` on the host if there is a destination.
    Edit,
    /// Print the cached session names of a host, or of all hosts, for tab completion.
    Complete(Option<String>),
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
    /// Fetch the server file from the release (`--download`).
    pub download: bool,
    /// Where the release files are; the default comes from the repository of the package.
    pub download_url: Option<String>,
    /// Directory with the files `ssj-server-<build>` for uploads.
    pub server_dir: Option<PathBuf>,
    /// The server file to upload, whatever the system of the host is.
    pub server_binary: Option<PathBuf>,
    /// `None` turns the escape character off.
    pub escape: Option<u8>,
    pub transport: Transport,
    /// Join the only session without a client instead of starting another one.
    pub auto_attach: bool,
    /// Programs on the host may use the ssh agent of this machine.
    pub forward_agent: bool,
    pub local_echo: LocalEcho,
    /// Compression of input and output; `None` leaves it to the files.
    pub compress: Option<bool>,
    /// `None` leaves it to the files and the default.
    pub congestion: Option<ssj_net::Congestion>,
    /// `list` prints JSON.
    pub json: bool,
    /// How much to tell (`-v` counted), 0 to 3.
    pub verbose: u8,
    /// Where the output of `-v` goes instead of the terminal.
    pub log_file: Option<PathBuf>,
    /// Never prompt and give up early: for refreshes that nobody waits for.
    pub batch: bool,
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
    pub forward_agent: bool,
    pub local_echo: bool,
}

impl Args {
    /// Empty arguments for commands that need no destination.
    fn blank() -> Self {
        Self {
            command: Command::Connect,
            destination: String::new(),
            ssh_port: None,
            name: None,
            ssh_command: vec!["ssh".to_owned()],
            server_path: ServerPath::Default,
            server_args: Vec::new(),
            udp_host: None,
            send_env: EnvFilter::default_patterns(),
            auto_install: None,
            download: false,
            download_url: None,
            server_dir: None,
            server_binary: None,
            escape: Some(b'~'),
            transport: Transport::Auto,
            auto_attach: false,
            forward_agent: false,
            local_echo: LocalEcho::Off,
            compress: None,
            congestion: None,
            json: false,
            verbose: 0,
            log_file: None,
            batch: false,
            explicit: Explicit::default(),
        }
    }

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
        if !self.explicit.forward_agent {
            self.forward_agent = config.forward_agent;
        }
        if self.congestion.is_none() {
            self.congestion = config.congestion;
        }
        if self.compress.is_none() {
            self.compress = config.compress;
        }
        if !self.explicit.local_echo {
            self.local_echo = config.local_echo;
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
    let parsed = crate::options::command()
        .try_get_matches_from(std::iter::once("ssj".to_owned()).chain(args))
        .and_then(|matches| Options::from_arg_matches(&matches));
    let options = match parsed {
        Ok(options) => options,
        Err(error) if error.use_stderr() => {
            return Err(error.render().to_string().trim_end().to_owned());
        }
        // help and version go to the output and end the program
        Err(error) => error.exit(),
    };
    from_options(options)
}

fn from_options(options: Options) -> Result<Args, String> {
    let Options {
        words: positionals, ..
    } = &options;
    if let Some(host) = &options.complete {
        return Ok(Args {
            command: Command::Complete((!host.is_empty()).then(|| host.clone())),
            ..Args::blank()
        });
    }
    let ssh_command = match &options.ssh_command {
        Some(text) => {
            let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
            if words.is_empty() {
                return Err("--ssh-command is empty".to_owned());
            }
            words
        }
        None => vec!["ssh".to_owned()],
    };
    let mut server_args = options.server_arg.clone();
    if let Some(range) = &options.port_range {
        server_args.extend(["--port-range".to_owned(), range.clone()]);
    }
    if let Some(timeout) = &options.detached_timeout {
        server_args.extend(["--detached-timeout".to_owned(), timeout.clone()]);
    }
    let auto_install = match (options.install, options.no_install) {
        (true, _) => Some(AutoInstall::Always),
        (_, true) => Some(AutoInstall::Never),
        _ => None,
    };
    let escape = match &options.escape_char {
        Some(text) => parse_escape(text).map_err(|e| format!("--escape-char: {e}"))?,
        None => Some(b'~'),
    };
    let transport = options
        .transport
        .as_deref()
        .map(|text| parse_transport(text).map_err(|e| format!("--transport: {e}")))
        .transpose()?;
    let congestion = options
        .congestion
        .as_deref()
        .map(|text| ssj_net::Congestion::parse(text).map_err(|e| format!("--congestion: {e}")))
        .transpose()?;
    let compress = options
        .compress
        .as_deref()
        .map(|text| parse_compress(text).map_err(|e| format!("--compress: {e}")))
        .transpose()?;
    let local_echo = options
        .local_echo
        .as_deref()
        .map(|text| parse_local_echo(text).map_err(|e| format!("--local-echo: {e}")))
        .transpose()?;
    let name = options
        .name
        .as_deref()
        .map(|text| {
            SessionName::parse(text)
                .ok_or("--name: 1 to 32 characters of letters, digits, '.', '_', '-'")
        })
        .transpose()?;
    let auto_attach =
        (options.auto_attach || options.no_auto_attach).then_some(options.auto_attach);
    let forward_agent =
        (options.forward_agent || options.no_forward_agent).then_some(options.forward_agent);
    // `list`, `attach`, and `kill` are commands only when a destination follows them
    let command_word = positionals
        .first()
        .filter(|_| positionals.len() > 1)
        .map(String::as_str);
    // `ssj edit` alone is the editor for the local file; a host that is called `edit` is reached
    // as `user@edit`
    let local_edit = positionals.as_slice() == ["edit"];
    let (command, destination) = match command_word {
        _ if local_edit => (Command::Edit, String::new()),
        Some(word @ ("list" | "attach" | "kill" | "install" | "edit")) => {
            let rest = &positionals[1..];
            let (id, destination) = match rest {
                [destination] => (None, destination.clone()),
                [destination, id] if word == "attach" || word == "kill" => (
                    Some(
                        is_selector(id)
                            .then(|| id.clone())
                            .ok_or("not a valid session id or name")?,
                    ),
                    destination.clone(),
                ),
                _ => {
                    return Err(format!(
                        "usage: ssj {word} user@host{}",
                        if matches!(word, "attach" | "kill") {
                            " [ID]"
                        } else {
                            ""
                        }
                    ));
                }
            };
            let command = match word {
                "list" => Command::List,
                "install" => Command::Install,
                "edit" => Command::Edit,
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
    let (target, ssh_port) = if local_edit {
        (String::new(), None)
    } else {
        let parsed = ssj_proto::destination::parse(&destination)
            .map_err(|e| format!("invalid destination {destination:?}: {e}"))?;
        (parsed.target(), parsed.port)
    };
    let send_env_set = !options.send_env.is_empty();
    let send_env = if send_env_set {
        EnvFilter::new(options.send_env.clone()).map_err(|e| format!("--send-env: {e:?}"))?
    } else {
        EnvFilter::default_patterns()
    };
    Ok(Args {
        command,
        destination: target,
        ssh_port,
        name,
        explicit: Explicit {
            ssh_command: options.ssh_command.is_some(),
            escape: options.escape_char.is_some(),
            send_env: send_env_set,
            transport: transport.is_some(),
            auto_attach: auto_attach.is_some(),
            forward_agent: forward_agent.is_some(),
            local_echo: local_echo.is_some(),
        },
        ssh_command,
        server_path: options
            .server_path
            .map_or(ServerPath::Default, ServerPath::Custom),
        server_args,
        udp_host: options.udp_host,
        send_env,
        auto_install,
        download: options.download,
        download_url: options.download_url,
        server_dir: options.server_dir.map(PathBuf::from),
        server_binary: options.server_binary.map(PathBuf::from),
        escape,
        transport: transport.unwrap_or_default(),
        auto_attach: auto_attach.unwrap_or(false),
        forward_agent: forward_agent.unwrap_or(false),
        local_echo: local_echo.unwrap_or_default(),
        compress,
        congestion,
        json: options.json,
        verbose: options.verbose.min(3),
        log_file: options.log_file.map(PathBuf::from),
        batch: options.batch,
    })
}

/// Name of the server file for this protocol version.
pub fn default_server_file() -> String {
    format!("ssj-server-p{PROTOCOL}")
}
