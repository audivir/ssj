//! The config file `client.toml`.
//!
//! ```toml
//! auto_install = "ask"            # "ask" (default), "always", or "never"
//! escape_char = "~"               # one character, or "none"
//! ssh_command = "ssh -J jump"     # replaces `ssh`
//! send_env = ["TERM", "LANG", "LC_*"]
//! transport = "auto"              # "auto" (default), "udp", "tcp", or "stdio"
//! ```
//!
//! Flags on the command line override the file.

use ssj_proto::EnvFilter;

use crate::cli::{Transport, parse_escape, parse_transport};

/// What to do when the server is not installed on the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AutoInstall {
    /// Ask on a terminal. Without a terminal, do not install.
    #[default]
    Ask,
    /// Install without asking.
    Always,
    /// Never install; say how to do it by hand.
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub auto_install: AutoInstall,
    /// `None` turns the escape character off.
    pub escape: Option<u8>,
    pub ssh_command: Vec<String>,
    pub send_env: EnvFilter,
    pub transport: Transport,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_install: AutoInstall::default(),
            escape: Some(b'~'),
            ssh_command: vec!["ssh".to_owned()],
            send_env: EnvFilter::default_patterns(),
            transport: Transport::default(),
        }
    }
}

impl Config {
    pub fn from_text(text: &str) -> Result<Self, String> {
        let mut table = ssj_config::parse(text)?;
        let mut config = Self::default();
        if let Some(word) = table.take_text("auto_install")? {
            config.auto_install = match word.as_str() {
                "ask" => AutoInstall::Ask,
                "always" => AutoInstall::Always,
                "never" => AutoInstall::Never,
                _ => {
                    return Err("auto_install must be \"ask\", \"always\", or \"never\"".to_owned());
                }
            };
        }
        if let Some(text) = table.take_text("escape_char")? {
            config.escape = parse_escape(&text).map_err(|e| format!("escape_char: {e}"))?;
        }
        if let Some(text) = table.take_text("ssh_command")? {
            let words: Vec<String> = text.split_whitespace().map(str::to_owned).collect();
            if words.is_empty() {
                return Err("ssh_command is empty".to_owned());
            }
            config.ssh_command = words;
        }
        if let Some(patterns) = table.take_list("send_env")? {
            config.send_env = EnvFilter::new(patterns).map_err(|e| format!("send_env: {e:?}"))?;
        }
        if let Some(text) = table.take_text("transport")? {
            config.transport = parse_transport(&text).map_err(|e| format!("transport: {e}"))?;
        }
        table.finish()?;
        Ok(config)
    }

    /// Reads `$XDG_CONFIG_HOME/ssj/client.toml` if it exists.
    pub fn load() -> Result<Self, String> {
        match ssj_config::read_file("client.toml")? {
            None => Ok(Self::default()),
            Some((path, text)) => {
                Self::from_text(&text).map_err(|e| format!("{}: {e}", path.display()))
            }
        }
    }
}
