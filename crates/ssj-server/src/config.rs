//! The config file `server.toml`.
//!
//! Flags on the command line override the file ([`Config::with`]), except `accept_env`, which only
//! the file sets: the command line is built by the client, and the client must not choose its own
//! filter.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use ssj_config::parsed;
use ssj_config::serde::Deserializer;
use ssj_config::serde::de::{Error, Unexpected, Visitor};
use ssj_net::Congestion;
use ssj_proto::EnvFilter;

use crate::cli::{
    Settings, Timeout, check_live_backlog, check_replay_capacity, parse_away_after, parse_compress,
    parse_port_range, parse_slow_client,
};
use crate::session::SlowClient;

/// Settings of ssj-server, read when a session starts. Flags of the client override them.
#[derive(Clone, Debug, confique::Config)]
#[config(layer_attr(serde(deny_unknown_fields)))]
pub struct Config {
    /// UDP ports the server may use, "0" for any.
    #[config(default = "60000:61000", deserialize_with = port_range)]
    pub port_range: RangeInclusive<u16>,
    /// Seconds without a client, or "none".
    #[config(default = "none", deserialize_with = timeout)]
    pub detached_timeout: Timeout,
    /// Limit in seconds even after a deliberate detach, or "none".
    #[config(default = "none", deserialize_with = timeout)]
    pub max_detached: Timeout,
    /// Bytes of output kept for a client that is away.
    #[config(default = 4194304, deserialize_with = replay_capacity)]
    pub replay_capacity: usize,
    /// Seconds a silent client may block the output (wait).
    #[config(default = 10, deserialize_with = away_after)]
    pub away_after: Duration,
    /// "drop": the job never waits for the link. "wait": like ssh.
    #[config(default = "drop", deserialize_with = slow_client)]
    pub slow_client: SlowClient,
    /// Bytes that may wait for a connected client (drop).
    #[config(default = 1048576, deserialize_with = live_backlog)]
    pub live_backlog: usize,
    /// "off" or "auto": compress output when the link is the limit.
    #[config(default = "off", deserialize_with = compress)]
    pub compress: bool,
    /// "auto", "bbr", or "cubic".
    #[config(default = "auto", deserialize_with = congestion)]
    pub congestion: Congestion,
    /// Absolute path of the shell, such as "/bin/zsh". Default: $SHELL.
    #[config(deserialize_with = shell)]
    pub shell: Option<PathBuf>,
    /// Variables of the client that the shell gets, as patterns.
    #[config(default = ["TERM", "LANG", "LC_*"], deserialize_with = accept_env)]
    pub accept_env: EnvFilter,
}

parsed!(port_range, String => RangeInclusive<u16>, |s| parse_port_range(&s));
parsed!(replay_capacity, usize => usize, check_replay_capacity);
parsed!(away_after, u64 => Duration, |n| parse_away_after(&n.to_string()));
parsed!(slow_client, String => SlowClient, |s| parse_slow_client(&s));
parsed!(live_backlog, usize => usize, check_live_backlog);
parsed!(compress, String => bool, |s| parse_compress(&s));
parsed!(congestion, String => Congestion, |s| Congestion::parse(&s));
parsed!(shell, String => PathBuf, |s| {
    if s.starts_with('/') { Ok(PathBuf::from(s)) } else { Err("must be an absolute path".to_owned()) }
});
parsed!(accept_env, Vec<String> => EnvFilter, |p| EnvFilter::new(p).map_err(|e| format!("{e:?}")));

/// Reads a number of seconds, where `0` and `"none"` mean no limit.
fn timeout<'de, D: Deserializer<'de>>(d: D) -> Result<Timeout, D::Error> {
    struct Seconds;
    impl Visitor<'_> for Seconds {
        type Value = Timeout;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a number of seconds or \"none\"")
        }

        fn visit_i64<E: Error>(self, secs: i64) -> Result<Timeout, E> {
            let secs = u64::try_from(secs)
                .map_err(|_| E::invalid_value(Unexpected::Signed(secs), &self))?;
            Ok((secs > 0).then(|| Duration::from_secs(secs)))
        }

        fn visit_u64<E: Error>(self, secs: u64) -> Result<Timeout, E> {
            Ok((secs > 0).then(|| Duration::from_secs(secs)))
        }

        fn visit_str<E: Error>(self, word: &str) -> Result<Timeout, E> {
            if word == "none" {
                Ok(None)
            } else {
                Err(E::invalid_value(Unexpected::Str(word), &self))
            }
        }
    }
    d.deserialize_any(Seconds)
}

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

    /// Reads `$XDG_CONFIG_HOME/ssj/server.toml` if it exists.
    pub fn load() -> Result<Self, String> {
        match ssj_config::read_file("server.toml")? {
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

    /// Returns the settings with those of the command line in place of the ones of the file.
    pub fn with(mut self, flags: &Settings) -> Self {
        let flags = flags.clone();
        self.port_range = flags.port_range.unwrap_or(self.port_range);
        self.detached_timeout = flags.detached_timeout.unwrap_or(self.detached_timeout);
        self.max_detached = flags.max_detached.unwrap_or(self.max_detached);
        self.replay_capacity = flags.replay_capacity.unwrap_or(self.replay_capacity);
        self.away_after = flags.away_after.unwrap_or(self.away_after);
        self.slow_client = flags.slow_client.unwrap_or(self.slow_client);
        self.live_backlog = flags.live_backlog.unwrap_or(self.live_backlog);
        self.compress = flags.compress.unwrap_or(self.compress);
        self.congestion = flags.congestion.unwrap_or(self.congestion);
        self.shell = flags.shell.or(self.shell);
        self
    }
}
