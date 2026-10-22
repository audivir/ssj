//! The config file `client.toml`.
//!
//! Flags on the command line override the file.

use ssj_config::parsed;
use ssj_net::Congestion;
use ssj_proto::EnvFilter;

use crate::cli::{
    LocalEcho, Transport, parse_compress, parse_detach_key, parse_escape, parse_local_echo,
    parse_transport,
};

/// What to do when the server is not installed on the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ssj_config::serde::Deserialize)]
#[serde(crate = "ssj_config::serde", rename_all = "lowercase")]
pub enum AutoInstall {
    /// Ask on a terminal. Without a terminal, do not install.
    #[default]
    Ask,
    /// Install without asking.
    Always,
    /// Never install; say how to do it by hand.
    Never,
}

/// A byte typed at the terminal, or `None` for none.
type Key = Option<u8>;

/// Settings of ssj, read at every start. Flags on the command line override them.
#[derive(Clone, Debug, PartialEq, Eq, confique::Config)]
#[config(layer_attr(serde(deny_unknown_fields)))]
pub struct Config {
    /// "ask", "always", or "never": what to do when the host has no server.
    #[config(default = "ask")]
    pub auto_install: AutoInstall,
    /// One character, or "none".
    #[config(default = "~", deserialize_with = escape_char)]
    pub escape_char: Key,
    /// "none", or a key such as "ctrl-]" that detaches anywhere.
    #[config(default = "none", deserialize_with = detach_key)]
    pub detach_key: Key,
    /// Replaces ssh, such as "ssh -J jump".
    #[config(default = "ssh", deserialize_with = ssh_command)]
    pub ssh_command: Vec<String>,
    /// More variables to send, besides TERM and the SendEnv of ssh.
    #[config(default = [], deserialize_with = send_env)]
    pub send_env: EnvFilter,
    /// "auto", "udp", "tcp", or "stdio".
    #[config(default = "auto", deserialize_with = transport)]
    pub transport: Transport,
    /// `ssj host` joins the only session without a client.
    #[config(default = false)]
    pub auto_attach: bool,
    /// Let programs on the host use your ssh agent (-A).
    #[config(default = false)]
    pub forward_agent: bool,
    /// "off" or "auto": show typed characters at once.
    #[config(default = "off", deserialize_with = local_echo)]
    pub local_echo: LocalEcho,
    /// "off" or "auto": compress while the link is the limit. Not set: the server decides.
    #[config(deserialize_with = compress)]
    pub compress: Option<bool>,
    /// "auto", "bbr", or "cubic": how fast to send. Not set: the server decides.
    #[config(deserialize_with = congestion)]
    pub congestion: Option<Congestion>,
}

parsed!(escape_char, String => Key, |s| parse_escape(&s));
parsed!(detach_key, String => Key, |s| parse_detach_key(&s));
parsed!(ssh_command, String => Vec<String>, |s| {
    let words: Vec<String> = s.split_whitespace().map(str::to_owned).collect();
    if words.is_empty() { Err("is empty".to_owned()) } else { Ok(words) }
});
parsed!(send_env, Vec<String> => EnvFilter, |p| EnvFilter::new(p).map_err(|e| format!("{e:?}")));
parsed!(transport, String => Transport, |s| parse_transport(&s));
parsed!(local_echo, String => LocalEcho, |s| parse_local_echo(&s));
parsed!(compress, String => bool, |s| parse_compress(&s));
parsed!(congestion, String => Congestion, |s| Congestion::parse(&s));

impl Default for Config {
    fn default() -> Self {
        Self::from_text("").expect("the defaults are valid")
    }
}

impl Config {
    /// Parses the text of a config file and rejects unknown keys and bad values.
    pub fn from_text(text: &str) -> Result<Self, String> {
        ssj_config::parse(text)
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

    /// The text a new file starts with.
    pub fn template() -> String {
        ssj_config::template::<Self>()
    }
}
