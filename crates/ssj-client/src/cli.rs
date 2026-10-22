//! Command line of `ssj`.

use std::path::PathBuf;

use clap::FromArgMatches;
use clap::error::ErrorKind;
use clap::parser::ValueSource;
use ssj_net::Compress;
use ssj_proto::{EnvFilter, PROTOCOL, SessionName};

use crate::config::AutoInstall;
use crate::options::{COMMANDS, Options};

/// Where the server file is, as seen on the remote host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerPath {
    /// `~/.local/share/ssj/bin/ssj-server-p<N>` following `XDG_DATA_HOME`, expanded remotely.
    Default,
    /// Taken literally and quoted for the remote shell.
    Custom(String),
}

/// How the client reaches the server.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// UDP first, then TCP through `ssh -W`, then the `ssh` channel itself.
    #[default]
    Auto,
    Udp,
    Tcp,
    Stdio,
}

/// Whether typed characters are shown before the server has echoed them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalEcho {
    /// Typed characters appear when the server echoes them.
    #[default]
    Off,
    /// Typed characters appear at once, faint, until the server echoes them.
    Auto,
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
    /// The ids or names to end; none means the only session, or a choice from a menu.
    Kill(Vec<String>),
    /// Make the client of a session on the host leave; the session keeps running.
    /// The ids or names to detach; none means the only session, or a choice from a menu.
    Detach(Vec<String>),
    /// Edit `client.toml` here, or `server.toml` on the host if there is a destination.
    Edit,
    /// Print the cached session names of a host, or of all hosts, for tab completion.
    Complete {
        host: Option<String>,
        /// Only the sessions that have a client.
        connected: bool,
    },
}

/// Container for the parsed command line of `ssj`.
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
    /// A command to run in the session instead of the shell, as `ssh host command` does.
    pub remote_command: Option<String>,
    /// Host name or address for the UDP connection when it differs from the destination.
    pub udp_host: Option<String>,
    /// The patterns of the config file. They come on top of the `SendEnv` of `ssh`.
    pub send_env: EnvFilter,
    /// The patterns of `--send-env`, on top of the others.
    pub send_env_add: Vec<String>,
    /// Overrides the setting of the config file.
    pub auto_install: Option<AutoInstall>,
    /// Where the release files are; the default comes from the repository of the package.
    pub download_url: Option<String>,
    /// The server file to upload instead of the one from the release.
    pub server_binary: Option<PathBuf>,
    /// A control byte that detaches wherever it is typed; `None` turns it off.
    pub detach_key: Option<u8>,
    pub transport: Transport,
    /// Join the only session without a client instead of starting another one.
    pub auto_attach: bool,
    /// Programs on the host may use the ssh agent of this machine.
    pub forward_agent: bool,
    pub local_echo: LocalEcho,
    /// Compression of input and output; `None` leaves it to the files.
    pub compress: Option<Compress>,
    /// `None` leaves it to the files and the default.
    pub congestion: Option<ssj_net::Congestion>,
    /// `list` prints JSON.
    pub json: bool,
    /// How much to tell (`-v` counted), 0 to 3.
    pub verbose: u8,
    /// No lines about connecting, disconnecting, and detaching (`-q`).
    pub quiet: bool,
    /// `kill` does not ask before it ends the only session of the host (`-f`).
    pub force: bool,
    /// Where the output of `-v` goes instead of the terminal.
    pub log_file: Option<PathBuf>,
    /// Never prompt and give up early: for refreshes that nobody waits for.
    pub batch: bool,
    /// Which of `ssh_command` and `detach_key` (and the others below) the command line set, since the
    /// config file only fills in the rest.
    pub explicit: Explicit,
}

/// Marks the options that the command line set, so that the config file does not replace them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Explicit {
    pub ssh_command: bool,
    pub detach_key: bool,
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
            remote_command: None,
            udp_host: None,
            send_env: EnvFilter::none(),
            send_env_add: Vec::new(),
            auto_install: None,
            download_url: None,
            server_binary: None,
            detach_key: Some(DEFAULT_DETACH_KEY),
            transport: Transport::Auto,
            auto_attach: false,
            forward_agent: false,
            local_echo: LocalEcho::Off,
            compress: None,
            congestion: None,
            json: false,
            verbose: 0,
            quiet: false,
            force: false,
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
        if !self.explicit.detach_key {
            self.detach_key = config.detach_key;
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
        self.send_env = config.send_env.clone();
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

/// The detach key when none is chosen: `ctrl-\\`.
pub const DEFAULT_DETACH_KEY: u8 = 0x1c;

/// A control key as `ctrl-]` or `^]` (letters in either case, and `[ \ ] ^ _`), or `none`. Enter,
/// Tab, and Escape are refused, since they are part of every line and every key sequence.
pub fn parse_detach_key(text: &str) -> Result<Option<u8>, String> {
    if text == "none" {
        return Ok(None);
    }
    let key = text.strip_prefix('^').or_else(|| {
        text.get(..5)
            .filter(|p| p.eq_ignore_ascii_case("ctrl-"))
            .map(|_| &text[5..])
    });
    let error = || "expected ctrl-A to ctrl-Z, ctrl-\\, ctrl-], ctrl-^, ctrl-_, or none".to_owned();
    let [byte] = key.ok_or_else(error)?.as_bytes() else {
        return Err(error());
    };
    let byte = match byte.to_ascii_uppercase() {
        letter @ b'A'..=b'Z' => letter - b'@',
        symbol @ (b'\\' | b']' | b'^' | b'_') => symbol - b'@',
        _ => return Err(error()),
    };
    if matches!(byte, 0x09 | 0x0a | 0x0d) {
        return Err("Tab and Enter cannot detach".to_owned());
    }
    Ok(Some(byte))
}

/// Parses the arguments of the command line.
///
/// As with `ssh`, everything after the destination belongs to the command that runs in the
/// session, options included (`ssj host ls -l`). The exception are the commands of `ssj`
/// (`ssj list host --json`), which keep their options after the destination. A command word is a
/// command only when a destination follows it: alone it is the name of a host.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let args: Vec<String> = args.into_iter().collect();
    let command = crate::options::command();
    let parse = |args: &[String]| {
        let words = std::iter::once("ssj").chain(args.iter().map(String::as_str));
        command.clone().try_get_matches_from(words)
    };
    // `ssj list` alone or `ssj list -- ls`: the host `list`, as `ssj connect list` would be
    let lone = |matches: &clap::ArgMatches| {
        matches
            .subcommand()
            .is_some_and(|(_, sub)| escaped(sub) && sub.get_one::<String>("destination").is_none())
    };
    let matches = match parse(&args) {
        Ok(matches) if !lone(&matches) => matches,
        Err(error) if error.kind() != ErrorKind::MissingRequiredArgument => {
            return Err(render(error));
        }
        first => {
            let connect: Vec<String> = std::iter::once("connect".to_owned())
                .chain(args.iter().cloned())
                .collect();
            match (first, parse(&connect)) {
                (_, Ok(matches)) => {
                    if let Some(hint) = lone_host(&matches).and_then(lone_command_hint) {
                        eprintln!("ssj: warning: {hint}");
                    }
                    matches
                }
                (Err(error), _) | (_, Err(error)) => return Err(render(error)),
            }
        }
    };
    let (word, matches) = match matches.subcommand() {
        Some((word, sub)) => (Some(word), sub),
        None => (None, &matches),
    };
    if let Some(word) = word.filter(|word| is_command_word(word)) {
        foreign_option(&command, matches, word)?;
    }
    let options = Options::from_arg_matches(matches).map_err(|e| e.to_string())?;
    let mut words = strings(matches, "destination").into_iter();
    let destination = words.next();
    let mut remote: Vec<String> = words.collect();
    // `ssj host -- --install` runs it on purpose
    if remote.first().is_some_and(|word| word == "--") {
        remote.remove(0);
    } else if let Some(hint) = remote.first().and_then(|word| misplaced_option(word)) {
        eprintln!("ssj: warning: {hint}");
    }
    if escaped(matches) {
        return Err("a command of ssj takes no command to run after `--`".to_owned());
    }
    let ids = match word {
        Some("attach" | "kill" | "detach") => strings(matches, "id"),
        _ => Vec::new(),
    };
    from_options(options, word, destination, ids, &remote)
}

/// Whether words follow a `--` after a command word (which `connect` does not have).
fn escaped(matches: &clap::ArgMatches) -> bool {
    matches.try_contains_id("escaped").unwrap_or(false)
}

/// The host of `ssj connect HOST`.
fn lone_host(matches: &clap::ArgMatches) -> Option<&str> {
    let (_, sub) = matches.subcommand()?;
    sub.get_many::<String>("destination")?
        .next()
        .map(String::as_str)
}

/// The text of an error of `clap`. Help and version go to the output and end the program.
fn render(error: clap::Error) -> String {
    if error.use_stderr() {
        error.render().to_string().trim_end().to_owned()
    } else {
        error.exit()
    }
}

/// The values of a positional argument.
fn strings(matches: &clap::ArgMatches, id: &str) -> Vec<String> {
    matches
        .get_many::<String>(id)
        .map_or_else(Vec::new, |values| values.cloned().collect())
}

/// A hint for a word after the destination that is an option of `ssj`. The host would run it as
/// a command, which a command never is, since it starts with a dash.
pub fn misplaced_option(word: &str) -> Option<String> {
    let name = word.split('=').next().unwrap_or(word);
    let known = crate::options::command().get_arguments().any(|arg| {
        match (name.strip_prefix("--"), name.strip_prefix('-')) {
            (Some(long), _) => {
                arg.get_long() == Some(long)
                    || arg
                        .get_all_aliases()
                        .is_some_and(|aliases| aliases.contains(&long))
            }
            (None, Some(short)) => short
                .chars()
                .next()
                .is_some_and(|first| arg.get_short() == Some(first)),
            _ => false,
        }
    });
    known.then(|| {
        format!(
            "`{name}` is after the destination, so the host runs it as a command; options go \
             before the destination, or after `--` to run it on the host on purpose"
        )
    })
}

/// A hint for a command word that stands alone: it is the name of a host then, which is what
/// someone who wanted the command does not mean.
pub fn lone_command_hint(word: &str) -> Option<String> {
    // `ssj edit` alone is the editor of `client.toml`
    (is_command_word(word) && word != "edit").then(|| {
        format!(
            "`{word}` is taken as the name of a host, since no destination follows it; the command \
             is `ssj {word} user@host`, and `ssj connect {word}` is the host without this warning"
        )
    })
}

/// The error for an option that the command does not read, as `--send-env` before `list`: the
/// settings of the connection belong in `client.toml`.
fn foreign_option(
    command: &clap::Command,
    matches: &clap::ArgMatches,
    word: &str,
) -> Result<(), String> {
    let allowed = COMMANDS
        .iter()
        .find(|found| found.word == word)
        .map_or(&[][..], |found| found.options);
    for arg in command.get_arguments() {
        let id = arg.get_id().as_str();
        // the positional words, the options that ssj gives itself, and the ones for help
        if (arg.get_long().is_none() && arg.get_short().is_none())
            || matches!(id, "complete" | "connected" | "batch" | "help" | "version")
        {
            continue;
        }
        if matches.value_source(id) == Some(ValueSource::CommandLine) && !allowed.contains(&id) {
            return Err(format!(
                "--{} is not an option of `ssj {word}` (a setting for every connection belongs in \
                 client.toml, which `ssj edit` opens)",
                id.replace('_', "-")
            ));
        }
    }
    Ok(())
}

/// Words that are commands of `ssj` when a destination follows them.
fn is_command_word(word: &str) -> bool {
    word != "connect" && COMMANDS.iter().any(|found| found.word == word)
}

fn from_options(
    options: Options,
    word: Option<&str>,
    destination: Option<String>,
    mut ids: Vec<String>,
    remote: &[String],
) -> Result<Args, String> {
    if let Some(host) = &options.complete {
        return Ok(Args {
            command: Command::Complete {
                host: (!host.is_empty()).then(|| host.clone()),
                connected: options.connected,
            },
            ..Args::blank()
        });
    }
    // a command for the session is everything after the destination, as with `ssh`.
    let remote_command = (!remote.is_empty()).then(|| remote.join(" "));
    if let Some(command) = &remote_command {
        if command.trim().is_empty() {
            return Err("the command is empty".to_owned());
        }
        if options.name.is_some() || options.auto_attach {
            return Err(
                "a command cannot be combined with --name or --auto-attach, since a session that runs already would not run it"
                    .to_owned(),
            );
        }
    }
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
    let auto_attach =
        (options.auto_attach || options.no_auto_attach).then_some(options.auto_attach);
    let forward_agent =
        (options.forward_agent || options.no_forward_agent).then_some(options.forward_agent);
    let command = match word {
        Some("list") => Command::List,
        Some("install") => Command::Install,
        Some("edit") => Command::Edit,
        Some("attach") => Command::Attach(ids.pop()),
        Some("kill") => Command::Kill(ids),
        Some("detach") => Command::Detach(ids),
        _ => Command::Connect,
    };
    // `ssj edit` alone is the editor for the local file.
    let (target, ssh_port) = match destination {
        Some(destination) => {
            let parsed = ssj_proto::destination::parse(&destination)
                .map_err(|e| format!("invalid destination {destination:?}: {e}"))?;
            (parsed.target(), parsed.port)
        }
        None if command == Command::Edit => (String::new(), None),
        None => return Err("missing destination (user@host)".to_owned()),
    };
    // only checked here: the patterns are added to the others when the session starts.
    EnvFilter::new(options.send_env.clone()).map_err(|e| format!("--send-env: {e:?}"))?;
    Ok(Args {
        command,
        destination: target,
        ssh_port,
        name: options.name,
        explicit: Explicit {
            ssh_command: options.ssh_command.is_some(),
            detach_key: options.detach_key.is_some(),
            transport: options.transport.is_some(),
            auto_attach: auto_attach.is_some(),
            forward_agent: forward_agent.is_some(),
            local_echo: options.local_echo.is_some(),
        },
        ssh_command: options
            .ssh_command
            .unwrap_or_else(|| vec!["ssh".to_owned()]),
        server_path: options
            .server_path
            .map_or(ServerPath::Default, ServerPath::Custom),
        server_args,
        remote_command,
        udp_host: options.udp_host,
        send_env: EnvFilter::none(),
        send_env_add: options.send_env.clone(),
        auto_install,
        download_url: options.download_url,
        server_binary: options.server_binary,
        detach_key: options.detach_key.unwrap_or(Some(DEFAULT_DETACH_KEY)),
        transport: options.transport.unwrap_or_default(),
        auto_attach: auto_attach.unwrap_or(false),
        forward_agent: forward_agent.unwrap_or(false),
        local_echo: options.local_echo.unwrap_or_default(),
        compress: options.compress,
        congestion: options.congestion,
        json: options.json,
        verbose: options.verbose.min(3),
        quiet: options.quiet,
        force: options.force,
        log_file: options.log_file,
        batch: options.batch,
    })
}

/// Name of the server file for this protocol version.
pub fn default_server_file() -> String {
    format!("ssj-server-p{PROTOCOL}")
}
