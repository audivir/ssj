//! The options of `ssj` as `clap` sees them. The file has no dependency on the rest of the crate,
//! so `build.rs` includes it to generate the completions and the man page. Every value is text
//! here, and `cli::parse` checks it.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "ssj",
    version,
    about = "A roaming remote shell that survives jumps",
    override_usage = "ssj [OPTIONS] [COMMAND] [ID] DESTINATION",
    after_help = "Commands: list, attach [ID], kill [ID], install. A word is a command only when a \
                  destination follows it.\n\
                  DESTINATION is [user@]host[:port], ssh://[user@]host[:port], or an IPv6 address."
)]
pub struct Options {
    /// Command, session id or name, and destination
    #[arg(value_name = "ARGS")]
    pub words: Vec<String>,

    /// Join the session of this name, or start it
    #[arg(long, value_name = "NAME", overrides_with = "name")]
    pub name: Option<String>,

    /// Join the only session without a client instead of starting another
    #[arg(long, overrides_with = "no_auto_attach")]
    pub auto_attach: bool,

    /// Always start a new session (the default)
    #[arg(long, overrides_with = "auto_attach")]
    pub no_auto_attach: bool,

    /// How to reach the server: auto, udp, tcp, or stdio
    #[arg(long, value_name = "WAY", overrides_with = "transport")]
    pub transport: Option<String>,

    /// Show typed characters before the server echoes them: off or auto
    #[arg(long, value_name = "WHEN", overrides_with = "local_echo")]
    pub local_echo: Option<String>,

    /// Compress input and output while the link is the limit: off or auto
    #[arg(long, value_name = "WHEN", overrides_with = "compress")]
    pub compress: Option<String>,

    /// Command that replaces ssh, split at whitespace
    #[arg(
        long,
        value_name = "COMMAND",
        allow_hyphen_values = true,
        overrides_with = "ssh_command"
    )]
    pub ssh_command: Option<String>,

    /// Path of the server file on the host
    #[arg(long, value_name = "PATH", overrides_with = "server_path")]
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

    /// Directory with the server files to upload
    #[arg(long, value_name = "DIR", overrides_with = "server_dir")]
    pub server_dir: Option<String>,

    /// The server file to upload, whatever the system of the host is
    #[arg(long, value_name = "FILE", overrides_with = "server_binary")]
    pub server_binary: Option<String>,

    /// Host name or address for UDP when it differs from the destination
    #[arg(long, value_name = "HOST", overrides_with = "udp_host")]
    pub udp_host: Option<String>,

    /// One printable character, or none
    #[arg(long, value_name = "CHAR", overrides_with = "escape_char")]
    pub escape_char: Option<String>,

    /// Environment variable pattern to send (repeatable)
    #[arg(long, value_name = "PATTERN")]
    pub send_env: Vec<String>,

    /// Print the sessions of `list` as JSON
    #[arg(long)]
    pub json: bool,

    /// Print the cached session names for tab completion, of one host or of all
    #[arg(long, value_name = "HOST", num_args = 0..=1, default_missing_value = "", hide = true, overrides_with = "complete")]
    pub complete: Option<String>,

    /// Never prompt, give up after 2 seconds (for background refreshes)
    #[arg(long, hide = true)]
    pub batch: bool,
}
