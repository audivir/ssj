//! The options of `ssj` as `clap` sees them, which `ssj-assets` also turns into the completions
//! and the man page.

use std::path::PathBuf;

use clap::{Args, ValueHint};
use ssj_net::{Compress, Congestion};
use ssj_proto::SessionName;

use crate::cli::{LocalEcho, Transport, parse_detach_key};

/// A control byte, or `None` for no key.
type Key = Option<u8>;
/// The words of a command.
type Words = Vec<String>;

/// A command of `ssj`.
pub struct CommandWord {
    pub word: &'static str,
    /// What it does, in a line for the completion of the first word.
    pub about: &'static str,
    /// What it does, for `ssj WORD --help`.
    pub help: &'static str,
    /// The options that it reads, by the id of the argument. Before the command word every option
    /// may stand, since the word is not known yet; after it only these make sense, and the help,
    /// the completions, and the man page show only these. `connect` reads all of them.
    pub options: &'static [&'static str],
}

/// The options of every command that runs the server on the host.
macro_rules! remote_options {
    ($($more:literal),*) => {
        &["verbose", "log_file", "ssh_command", "server_path", "server_arg" $(, $more)*]
    };
}

/// The commands of `ssj`.
pub const COMMANDS: [CommandWord; 7] = [
    CommandWord {
        word: "list",
        about: "List the sessions of a host",
        help: "Lists the sessions on the host: id, state, age, protocol, and name. A terminal gets a \
               header.",
        options: remote_options!("json"),
    },
    CommandWord {
        word: "attach",
        about: "Join a running session",
        help: "Joins a running session. ID is an id or a name. Without it the only session is \
               joined, and with several a menu asks which one.",
        options: remote_options!(
            "quiet",
            "transport",
            "local_echo",
            "congestion",
            "compress",
            "port_range",
            "install",
            "no_install",
            "download_url",
            "server_binary",
            "udp_host",
            "detach_key"
        ),
    },
    CommandWord {
        word: "kill",
        about: "End sessions",
        help: "Ends sessions on the host. Each ID is an id or a name. Without an ID the only \
               session is ended after a question, and with several sessions a menu asks which \
               one. A name that is also the id of another session gets a menu as well.",
        options: remote_options!("force"),
    },
    CommandWord {
        word: "detach",
        about: "Make the clients of sessions leave",
        help: "Makes the clients of sessions leave. The sessions keep running. Each ID is an id or \
               a name. Without an ID the only session that has a client is detached, and with \
               several a menu asks which one.",
        options: remote_options!(),
    },
    CommandWord {
        word: "install",
        about: "Put the server file on a host",
        help: "Puts the server file on the host: downloaded from the release of this version, or \
               the file given with --server-binary.",
        options: &[
            "verbose",
            "log_file",
            "ssh_command",
            "download_url",
            "server_binary",
        ],
    },
    // the host runs the server to check `server.toml`.
    CommandWord {
        word: "edit",
        about: "Edit client.toml, or server.toml on a host",
        help: "Edits client.toml in $EDITOR, or server.toml on the host. The text is checked \
               before it is saved.",
        options: remote_options!(),
    },
    CommandWord {
        word: "connect",
        about: "Connect to a host named like a command",
        help: "Connects to the host, as `ssj DESTINATION [COMMAND]...` does. The word is for a \
               host that is named like a command of ssj (`ssj connect list`); it is no different \
               otherwise.",
        options: &[],
    },
];

/// The command with a subcommand for each command word and the options that each reads, for the
/// man page and the completions. [`command`] adds what only the parser needs.
pub fn commands() -> clap::Command {
    let options: Vec<clap::Arg> = Options::augment_args(clap::Command::new("ssj"))
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .cloned()
        .collect();
    let mut tree = clap::Command::new("ssj")
        .about("A roaming remote shell that survives jumps")
        .version(env!("CARGO_PKG_VERSION"))
        .disable_help_subcommand(true)
        .args(options.iter().cloned());
    let id = || {
        clap::Arg::new("id")
            .index(2)
            .value_name("ID")
            .help("The id or the name of a session")
            .value_parser(selector)
    };
    for found in COMMANDS {
        let mut sub = clap::Command::new(found.word)
            .about(found.about)
            .before_help(found.help)
            .help_template("{usage-heading} {usage}\n\n{before-help}{all-args}")
            // only `ssj edit` can do without
            .arg(destination().required(found.word != "edit"));
        if found.word == "connect" {
            sub = sub.args(options.iter().cloned());
        }
        for id in found.options {
            let arg = options
                .iter()
                .find(|arg| arg.get_id() == id)
                .unwrap_or_else(|| panic!("COMMANDS names {id}, which is no option"));
            sub = sub.arg(arg.clone());
        }
        // the ids of sessions follow the destination: one for `attach`, any for the others.
        match found.word {
            "attach" => sub = sub.arg(id()),
            "kill" | "detach" => sub = sub.arg(id().num_args(0..)),
            _ => {}
        }
        tree = tree.subcommand(sub);
    }
    tree
}

/// The command for parsing and for the help. Without a command word the line is a destination and
/// the command for the host. The options are global, so that they may stand before the command
/// word too; each command has the options that it does not read as hidden ones, so that
/// [`crate::cli::parse`] can name them in its error.
pub fn command() -> clap::Command {
    let all = Options::augment_args(clap::Command::new("ssj"));
    let mut tree = commands()
        .args(all.get_arguments().filter(|arg| arg.is_hide_set()).cloned())
        .override_usage(
            "ssj [OPTIONS] DESTINATION [REMOTE-COMMAND]...\n       ssj [OPTIONS] COMMAND DESTINATION [ID]...",
        )
        .arg(line());
    tree = tree.mut_subcommand("connect", |sub| sub.mut_arg("destination", |_| line()));
    for found in COMMANDS {
        tree = tree.mut_subcommand(found.word, |sub| {
            let missing: Vec<clap::Arg> = all
                .get_arguments()
                .filter(|arg| sub.get_arguments().all(|has| has.get_id() != arg.get_id()))
                .map(|arg| arg.clone().hide(true))
                .collect();
            sub.args(missing)
        });
    }
    tree.mut_args(|arg| {
        let option = arg.get_long().is_some() || arg.get_short().is_some();
        arg.global(option)
    })
}

/// The destination, the first word that is no option.
fn destination() -> clap::Arg {
    clap::Arg::new("destination")
        .index(1)
        .value_name("DESTINATION")
        .help("[user@]host[:port], or ssh://[user@]host[:port]")
        .value_hint(ValueHint::Hostname)
}

/// The destination and the command for the host, which is every word after it, options of `ssj`
/// included, as with `ssh`.
fn line() -> clap::Arg {
    destination()
        .num_args(1..)
        .trailing_var_arg(true)
        .value_names(["DESTINATION", "REMOTE-COMMAND"])
        .help("[user@]host[:port] or ssh://[user@]host[:port], and a command to run instead of the shell")
}

/// Container for the options of `ssj`.
#[derive(Args, Debug)]
pub struct Options {
    /// Print what is happening: -v for the steps, -vv for details, -vvv for every frame
    #[arg(short = 'v', long = "verbose", action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Do not print the lines about connecting, disconnecting, and detaching
    #[arg(short = 'q', long)]
    pub quiet: bool,

    /// Do not ask before `kill` ends the only session of the host
    #[arg(short = 'f', long)]
    pub force: bool,

    /// Write the -v output to this file and not to the terminal
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath, overrides_with = "log_file")]
    pub log_file: Option<PathBuf>,

    /// Join the session of this name, or start it
    #[arg(long, value_name = "NAME", overrides_with = "name", value_parser = parse_name)]
    pub name: Option<SessionName>,

    /// Join the only session without a client
    #[arg(long, overrides_with = "no_auto_attach")]
    pub auto_attach: bool,

    /// Always start a new session (the default)
    #[arg(long, overrides_with = "auto_attach")]
    pub no_auto_attach: bool,

    /// Let programs on the host use your ssh agent (SSH_AUTH_SOCK) for signing
    #[arg(short = 'A', long, overrides_with = "no_forward_agent")]
    pub forward_agent: bool,

    /// Do not forward the agent (the default)
    #[arg(long, overrides_with = "forward_agent")]
    pub no_forward_agent: bool,

    /// How to reach the server
    #[arg(long, value_name = "WAY", overrides_with = "transport")]
    pub transport: Option<Transport>,

    /// Show typed characters before the server echoes them
    #[arg(long, value_name = "WHEN", overrides_with = "local_echo")]
    pub local_echo: Option<LocalEcho>,

    /// How fast to send (default: auto)
    #[arg(long, value_name = "WAY", overrides_with = "congestion")]
    pub congestion: Option<Congestion>,

    /// Compress input and output while the link is the limit
    #[arg(long, value_name = "WHEN", overrides_with = "compress")]
    pub compress: Option<Compress>,

    /// Command that replaces ssh, split at whitespace
    #[arg(
        long,
        value_name = "COMMAND",
        allow_hyphen_values = true,
        overrides_with = "ssh_command",
        value_parser = parse_ssh_command
    )]
    pub ssh_command: Option<Words>,

    /// Path of the server file on the host
    #[arg(long, value_name = "PATH", value_hint = ValueHint::AnyPath, overrides_with = "server_path")]
    pub server_path: Option<String>,

    /// Extra argument for ssj-server (repeatable)
    #[arg(long, value_name = "ARG", allow_hyphen_values = true)]
    pub server_arg: Vec<String>,

    /// UDP ports the server may use, such as 60000-61000, or 0 for any
    #[arg(long, value_name = "RANGE", overrides_with = "port_range")]
    pub port_range: Option<String>,

    /// How long a session lives without a client
    #[arg(long, value_name = "TIME", overrides_with = "detached_timeout")]
    pub detached_timeout: Option<String>,

    /// Install the server on the host without asking
    #[arg(long, overrides_with = "no_install")]
    pub install: bool,

    /// Never install the server
    #[arg(long, overrides_with = "install")]
    pub no_install: bool,

    /// Where the release files are, such as https://host/owner/ssj/releases/download/v0.1.0
    #[arg(long, value_name = "URL", overrides_with = "download_url")]
    pub download_url: Option<String>,

    /// The server file to upload instead of the one from the release
    #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath, overrides_with = "server_binary")]
    pub server_binary: Option<PathBuf>,

    /// Host name or address for UDP when it differs from the destination
    #[arg(long, value_name = "HOST", value_hint = ValueHint::Hostname, overrides_with = "udp_host")]
    pub udp_host: Option<String>,

    /// A key that detaches anywhere, such as ctrl-] (default: ctrl-\\)
    #[arg(long, value_name = "KEY", overrides_with = "detach_key", value_parser = parse_detach_key)]
    pub detach_key: Option<Key>,

    /// Environment variable pattern to send as well as the SendEnv of ssh (repeatable)
    #[arg(long, value_name = "PATTERN")]
    pub send_env: Vec<String>,

    /// Print the sessions of `list` as JSON
    #[arg(long)]
    pub json: bool,

    /// Print the cached session names for tab completion, of one host or of all
    #[arg(long, value_name = "HOST", num_args = 0..=1, default_missing_value = "", hide = true, overrides_with = "complete")]
    pub complete: Option<String>,

    /// With --complete: only the sessions that have a client
    #[arg(long, hide = true)]
    pub connected: bool,

    /// Never prompt, give up after 2 seconds (for background refreshes)
    #[arg(long, hide = true)]
    pub batch: bool,
}

fn selector(text: &str) -> Result<String, &'static str> {
    if ssj_proto::is_selector(text) {
        Ok(text.to_owned())
    } else {
        Err("not a valid session id or name")
    }
}

fn parse_name(text: &str) -> Result<SessionName, &'static str> {
    SessionName::parse(text).ok_or("1 to 32 characters of letters, digits, '.', '_', '-'")
}

/// Splits the command at whitespace.
pub fn parse_ssh_command(text: &str) -> Result<Words, &'static str> {
    let words: Words = text.split_whitespace().map(str::to_owned).collect();
    if words.is_empty() {
        Err("is empty")
    } else {
        Ok(words)
    }
}
