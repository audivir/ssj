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

/// Parses the value of `--local-echo`.
pub fn parse_local_echo(text: &str) -> Result<LocalEcho, String> {
    match text {
        "off" => Ok(LocalEcho::Off),
        "auto" => Ok(LocalEcho::Auto),
        _ => Err("expected off or auto".to_owned()),
    }
}

/// Parses the value of `--compress`.
pub fn parse_compress(text: &str) -> Result<bool, String> {
    match text {
        "auto" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected auto or off".to_owned()),
    }
}

/// Parses the value of `--transport`.
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
    /// `None` turns the escape character off.
    pub escape: Option<u8>,
    /// A control byte that detaches wherever it is typed; `None` turns it off.
    pub detach_key: Option<u8>,
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
    /// No lines about connecting, disconnecting, and detaching (`-q`).
    pub quiet: bool,
    /// `kill` does not ask before it ends the only session of the host (`-f`).
    pub force: bool,
    /// Where the output of `-v` goes instead of the terminal.
    pub log_file: Option<PathBuf>,
    /// Never prompt and give up early: for refreshes that nobody waits for.
    pub batch: bool,
    /// Which of `ssh_command` and `escape` (and the others below) the command line set, since the
    /// config file only fills in the rest.
    pub explicit: Explicit,
}

/// Marks the options that the command line set, so that the config file does not replace them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Explicit {
    pub ssh_command: bool,
    pub escape: bool,
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
            escape: Some(b'~'),
            detach_key: None,
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
        if !self.explicit.escape {
            self.escape = config.escape;
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

/// One printable ASCII character, or `none`.
pub fn parse_escape(text: &str) -> Result<Option<u8>, String> {
    match text.as_bytes() {
        b"none" => Ok(None),
        [byte] if byte.is_ascii() && !byte.is_ascii_control() => Ok(Some(*byte)),
        _ => Err("expected one printable ASCII character or none".to_owned()),
    }
}

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

/// Where the first two positional words of the command line are (`None` when there are fewer).
///
/// Options that take a value are skipped together with their value, so that the value is not
/// taken for a word.
fn positionals(args: &[String]) -> [Option<usize>; 2] {
    let command = crate::options::command();
    let takes_value = |long: Option<&str>, short: Option<char>| {
        command.get_arguments().any(|arg| {
            let found = long.is_some_and(|l| arg.get_long() == Some(l))
                || short.is_some_and(|c| arg.get_short() == Some(c))
                || long.is_some_and(|l| arg.get_all_aliases().is_some_and(|a| a.contains(&l)));
            found && arg.get_action().takes_values()
        })
    };
    let mut found = [None, None];
    let mut count = 0;
    let mut index = 0;
    while index < args.len() && count < 2 {
        let word = args[index].as_str();
        if word == "--" {
            break;
        }
        if let Some(long) = word.strip_prefix("--") {
            if !long.contains('=') && takes_value(Some(long), None) {
                index += 1;
            }
        } else if let Some(shorts) = word.strip_prefix('-').filter(|s| !s.is_empty()) {
            for (at, c) in shorts.char_indices() {
                if takes_value(None, Some(c)) {
                    if at + c.len_utf8() == shorts.len() {
                        index += 1;
                    }
                    break;
                }
            }
        } else {
            found[count] = Some(index);
            count += 1;
        }
        index += 1;
    }
    found
}

/// Parses the arguments of the command line.
///
/// As with `ssh`, everything after the destination belongs to the command that runs in the
/// session, options included (`ssj host ls -l`). The exception are the commands of `ssj`
/// (`ssj list host --json`), which keep their options after the destination.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let args: Vec<String> = args.into_iter().collect();
    let [first, second] = positionals(&args);
    // `ssj kill --help`: a command word alone is not a host, and the help is its own
    if let Some(first) = first
        && second.is_none()
        && args.iter().any(|a| a == "--help" || a == "-h")
        && let Some(text) = command_help(&args[first])
    {
        println!("{text}");
        std::process::exit(0);
    }
    let mut head = args;
    let (mut first, mut second) = (first, second);
    // `ssj connect HOST [COMMAND]...` is the plain connection, for a host that is named like a
    // command and without the warning of a lone command word
    let mut explicit = false;
    if let Some(at) = first
        && head[at] == "connect"
    {
        if second.is_none() {
            return Err("usage: ssj connect user@host [COMMAND]...".to_owned());
        }
        head.remove(at);
        [first, second] = positionals(&head);
        explicit = true;
    }
    if !explicit
        && second.is_none()
        && let Some(at) = first
        && let Some(hint) = lone_command_hint(&head[at])
    {
        eprintln!("ssj: warning: {hint}");
    }
    let mut tail = Vec::new();
    let mut word = None;
    if let Some(first) = first {
        let is_command = !explicit && is_command_word(&head[first]) && second.is_some();
        if is_command {
            word = Some(head[first].clone());
        } else {
            tail = head.split_off(first + 1);
            if tail.first().is_some_and(|word| word == "--") {
                tail.remove(0);
            } else if let Some(hint) = tail.first().and_then(|word| misplaced_option(word)) {
                eprintln!("ssj: warning: {hint}");
            }
        }
    }
    let command = crate::options::command();
    let matches = match command
        .clone()
        .try_get_matches_from(std::iter::once("ssj".to_owned()).chain(head))
    {
        Ok(matches) => matches,
        Err(error) if error.use_stderr() => {
            return Err(error.render().to_string().trim_end().to_owned());
        }
        // help and version go to the output and end the program.
        Err(error) => error.exit(),
    };
    if let Some(word) = &word {
        foreign_option(&command, &matches, word)?;
    }
    let options = Options::from_arg_matches(&matches).map_err(|e| e.to_string())?;
    from_options(options, &tail)
}

/// The help of a command of `ssj`, or `None` for a word that is not one.
pub fn command_help(word: &str) -> Option<&'static str> {
    Some(match word {
        "list" => {
            "Usage: ssj list [OPTIONS] DESTINATION

Lists the sessions on the host: id, state, age, protocol, and name. A terminal gets a header.

Options:
      --json  print a JSON array instead"
        }
        "connect" => {
            "Usage: ssj connect [OPTIONS] DESTINATION [COMMAND]...

Connects to the host, as `ssj DESTINATION [COMMAND]...` does. The word is for a host that is
named like a command of ssj (`ssj connect list`); it is no different otherwise."
        }
        "attach" => {
            "Usage: ssj attach [OPTIONS] DESTINATION [ID]

Joins a running session. ID is an id or a name. Without it the only session is joined, and with
several a menu asks which one."
        }
        "kill" => {
            "Usage: ssj kill [OPTIONS] DESTINATION [ID...]

Ends sessions on the host. Each ID is an id or a name. Without an ID the only session is ended
after a question, and with several sessions a menu asks which one. A name that is also the id of
another session gets a menu as well.

Options:
  -f, --force  do not ask before the only session is ended"
        }
        "detach" => {
            "Usage: ssj detach [OPTIONS] DESTINATION [ID...]

Makes the clients of sessions leave. The sessions keep running. Each ID is an id or a name. Without
an ID the only session that has a client is detached, and with several a menu asks which one."
        }
        "install" => {
            "Usage: ssj install [OPTIONS] DESTINATION

Puts the server file on the host: downloaded from the release of this version, or the file given
with --server-binary.

Options:
      --server-binary FILE  upload this file instead of the download
      --download-url URL    take the files from this place"
        }
        "edit" => {
            "Usage: ssj edit [OPTIONS] [DESTINATION]

Edits client.toml in $EDITOR, or server.toml on the host. The text is checked before it is saved."
        }
        _ => return None,
    })
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
    matches!(word, "list" | "attach" | "kill" | "detach" | "install").then(|| {
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
    use clap::parser::ValueSource;
    let allowed = crate::options::COMMAND_OPTIONS
        .iter()
        .find(|(name, _)| *name == word)
        .map_or(&[][..], |(_, ids)| *ids);
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
    matches!(
        word,
        "list" | "attach" | "kill" | "detach" | "install" | "edit"
    )
}

fn from_options(options: Options, tail: &[String]) -> Result<Args, String> {
    // A command for the session is everything after the destination, as with `ssh`.
    let words = options.words.clone();
    let remote_command = (!tail.is_empty()).then(|| tail.join(" "));
    if remote_command.is_none() && !options.command.is_empty() {
        return Err("a command of ssj takes no command to run after `--`".to_owned());
    }
    if let Some(command) = &remote_command {
        if command.trim().is_empty() {
            return Err("the command is empty".to_owned());
        }
        if words.len() != 1 {
            return Err(
                "a command to run needs exactly one destination before it (ssj DESTINATION -- COMMAND)"
                    .to_owned(),
            );
        }
        if options.name.is_some() || options.auto_attach {
            return Err(
                "a command cannot be combined with --name or --auto-attach, since a session that runs already would not run it"
                    .to_owned(),
            );
        }
    }
    let positionals = &words;
    if let Some(host) = &options.complete {
        return Ok(Args {
            command: Command::Complete {
                host: (!host.is_empty()).then(|| host.clone()),
                connected: options.connected,
            },
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
    let detach_key = match &options.detach_key {
        Some(text) => parse_detach_key(text).map_err(|e| format!("--detach-key: {e}"))?,
        None => None,
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
    // `list`, `attach`, and `kill` are commands only when a destination follows them.
    let command_word = positionals
        .first()
        .filter(|_| positionals.len() > 1)
        .map(String::as_str);
    // `ssj edit` alone is the editor for the local file; a host that is called `edit` is reached
    // as `user@edit`.
    let local_edit = positionals.as_slice() == ["edit"];
    let (command, destination) = match command_word {
        _ if local_edit => (Command::Edit, String::new()),
        Some(word @ ("list" | "attach" | "kill" | "detach" | "install" | "edit")) => {
            let rest = &positionals[1..];
            let (mut ids, destination) = match rest {
                [destination] => (Vec::new(), destination.clone()),
                [destination, ids @ ..]
                    if matches!(word, "kill" | "detach")
                        || (ids.len() == 1 && word == "attach") =>
                {
                    let mut checked = Vec::new();
                    for id in ids {
                        if !is_selector(id) {
                            return Err("not a valid session id or name".to_owned());
                        }
                        checked.push(id.clone());
                    }
                    (checked, destination.clone())
                }
                _ => {
                    return Err(format!(
                        "usage: ssj {word} user@host{}",
                        match word {
                            "kill" | "detach" => " [ID...]",
                            "attach" => " [ID]",
                            _ => "",
                        }
                    ));
                }
            };
            let command = match word {
                "list" => Command::List,
                "install" => Command::Install,
                "edit" => Command::Edit,
                "attach" => Command::Attach(ids.pop()),
                "detach" => Command::Detach(ids),
                _ => Command::Kill(ids),
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
    // only checked here: the patterns are added to the others when the session starts.
    EnvFilter::new(options.send_env.clone()).map_err(|e| format!("--send-env: {e:?}"))?;
    Ok(Args {
        command,
        destination: target,
        ssh_port,
        name,
        explicit: Explicit {
            ssh_command: options.ssh_command.is_some(),
            escape: options.escape_char.is_some(),
            detach_key: options.detach_key.is_some(),
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
        remote_command,
        udp_host: options.udp_host,
        send_env: EnvFilter::none(),
        send_env_add: options.send_env.clone(),
        auto_install,
        download_url: options.download_url,
        server_binary: options.server_binary.map(PathBuf::from),
        escape,
        detach_key,
        transport: transport.unwrap_or_default(),
        auto_attach: auto_attach.unwrap_or(false),
        forward_agent: forward_agent.unwrap_or(false),
        local_echo: local_echo.unwrap_or_default(),
        compress,
        congestion,
        json: options.json,
        verbose: options.verbose.min(3),
        quiet: options.quiet,
        force: options.force,
        log_file: options.log_file.map(PathBuf::from),
        batch: options.batch,
    })
}

/// Name of the server file for this protocol version.
pub fn default_server_file() -> String {
    format!("ssj-server-p{PROTOCOL}")
}
