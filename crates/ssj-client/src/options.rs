//! The options of `ssj` as `clap` sees them, which `ssj-assets` also turns into the completions
//! and the man page.

use std::path::PathBuf;

use clap::{CommandFactory, Parser, ValueHint};
use ssj_net::{Compress, Congestion};
use ssj_proto::SessionName;

use crate::cli::{LocalEcho, Transport, parse_detach_key};

/// A control byte, or `None` for no key.
type Key = Option<u8>;
/// The words of a command.
type Words = Vec<String>;

/// The flags that have a counterpart which turns them off: the id of each, its name, and the
/// line that the help shows for both.
const PAIRS: [[&str; 4]; 3] = [
    [
        "auto_attach",
        "no_auto_attach",
        "auto-attach",
        "auto-attach / --no-auto-attach",
    ],
    [
        "forward_agent",
        "no_forward_agent",
        "forward-agent",
        "forward-agent / --no-forward-agent",
    ],
    ["install", "no_install", "install", "install / --no-install"],
];

/// The options that each command reads, by the id of the argument. Before the command word every
/// option may stand, since the word is not known yet; after it only these make sense, and the
/// completions and the man page show only these. The words after a destination that is not a
/// command belong to the command on the host and have none.
pub const COMMAND_OPTIONS: [(&str, &[&str]); 6] = [
    (
        "list",
        &[
            "verbose",
            "log_file",
            "ssh_command",
            "server_path",
            "server_arg",
            "json",
        ],
    ),
    (
        "attach",
        &[
            "verbose",
            "quiet",
            "log_file",
            "transport",
            "local_echo",
            "congestion",
            "compress",
            "ssh_command",
            "server_path",
            "server_arg",
            "port_range",
            "install",
            "no_install",
            "download_url",
            "server_binary",
            "udp_host",
            "detach_key",
        ],
    ),
    (
        "kill",
        &[
            "verbose",
            "log_file",
            "ssh_command",
            "server_path",
            "server_arg",
            "force",
        ],
    ),
    (
        "detach",
        &[
            "verbose",
            "log_file",
            "ssh_command",
            "server_path",
            "server_arg",
        ],
    ),
    (
        "install",
        &[
            "verbose",
            "log_file",
            "ssh_command",
            "download_url",
            "server_binary",
        ],
    ),
    // the host runs the server to check `server.toml`
    (
        "edit",
        &[
            "verbose",
            "log_file",
            "ssh_command",
            "server_path",
            "server_arg",
        ],
    ),
];

/// The command for parsing and for `--help`. The help shows a flag and its counterpart on one line
/// (`--auto-attach / --no-auto-attach`), the way `typer` does: the flag gets that text as its long
/// name, which nobody types, and keeps the real name as a hidden alias. The counterpart is hidden.
pub fn command() -> clap::Command {
    let mut command = Options::command();
    for [on, off, name, shown] in PAIRS {
        command = command
            .mut_arg(off, |arg| arg.hide(true))
            .mut_arg(on, |arg| arg.long(shown).alias(name));
    }
    command
}

#[derive(Parser, Debug)]
#[command(
    name = "ssj",
    version,
    about = "A roaming remote shell that survives jumps",
    override_usage = "ssj [OPTIONS] [COMMAND] DESTINATION [ID]\n       ssj [OPTIONS] DESTINATION REMOTE-COMMAND...",
    after_help = "Commands: connect DESTINATION [COMMAND]..., list, attach DESTINATION [ID], kill DESTINATION [ID], detach DESTINATION [ID], install, edit [DESTINATION]. A word is a command only when a \
                  destination follows it.\n\
                  DESTINATION is [user@]host[:port], ssh://[user@]host[:port], or an IPv6 address."
)]
pub struct Options {
    /// Command, session id or name, and destination
    #[arg(value_name = "ARGS")]
    pub words: Vec<String>,

    /// A command to run in the session instead of the shell, after the destination
    #[arg(last = true, value_name = "REMOTE-COMMAND")]
    pub command: Vec<String>,

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

    /// Join the only session without a client, or always start a new one (default)
    #[arg(long, overrides_with = "no_auto_attach")]
    pub auto_attach: bool,

    /// Always start a new session (the default)
    #[arg(long, overrides_with = "auto_attach")]
    pub no_auto_attach: bool,

    /// Let programs on the host use your ssh agent (SSH_AUTH_SOCK) for signing, or not (default)
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

    /// Install the server on the host without asking, or never install it
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
