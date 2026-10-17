//! The config file `server.toml`.
//!
//! ```toml
//! port_range = "60000:61000"          # UDP ports the server may use
//! detached_timeout = 604800           # seconds without a client, or "none"
//! max_detached = "none"               # upper limit even after a deliberate detach
//! replay_capacity = 4194304           # bytes of output kept for an absent client
//! shell = "/bin/zsh"                  # default: $SHELL
//! accept_env = ["TERM", "LANG", "LC_*"]
//! ```
//!
//! Flags on the command line override the file, except `accept_env`, which only the file sets:
//! the command line is built by the client, and the client must not choose its own filter.

use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use ssj_proto::EnvFilter;

use crate::cli::{Args, check_replay_capacity, parse_port_range};

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub port_range: Option<RangeInclusive<u16>>,
    pub detached_timeout: Option<Option<Duration>>,
    pub max_detached: Option<Option<Duration>>,
    pub replay_capacity: Option<usize>,
    pub shell: Option<PathBuf>,
    pub accept_env: Option<EnvFilter>,
}

impl Config {
    pub fn from_text(text: &str) -> Result<Self, String> {
        let mut table = ssj_config::parse(text)?;
        let mut config = Self::default();
        if let Some(range) = table.take_text("port_range")? {
            config.port_range =
                Some(parse_port_range(&range).map_err(|e| format!("port_range: {e}"))?);
        }
        config.detached_timeout = table
            .take_seconds("detached_timeout")?
            .map(|s| s.filter(|s| *s > 0).map(Duration::from_secs));
        config.max_detached = table
            .take_seconds("max_detached")?
            .map(|s| s.filter(|s| *s > 0).map(Duration::from_secs));
        if let Some(size) = table.take_unsigned("replay_capacity")? {
            let size =
                usize::try_from(size).map_err(|_| "replay_capacity is too large".to_owned())?;
            config.replay_capacity =
                Some(check_replay_capacity(size).map_err(|e| format!("replay_capacity: {e}"))?);
        }
        if let Some(shell) = table.take_text("shell")? {
            if !shell.starts_with('/') {
                return Err("shell must be an absolute path".to_owned());
            }
            config.shell = Some(PathBuf::from(shell));
        }
        if let Some(patterns) = table.take_list("accept_env")? {
            config.accept_env =
                Some(EnvFilter::new(patterns).map_err(|e| format!("accept_env: {e:?}"))?);
        }
        table.finish()?;
        Ok(config)
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

    /// The filter for the variables of the client.
    pub fn accept_env(&self) -> EnvFilter {
        self.accept_env
            .clone()
            .unwrap_or_else(EnvFilter::default_patterns)
    }
}

impl Args {
    /// Fills in what the command line left open from the config file.
    pub fn apply_config(&mut self, config: &Config) {
        if !self.explicit.port_range
            && let Some(range) = &config.port_range
        {
            self.port_range = range.clone();
        }
        if !self.explicit.detached_timeout
            && let Some(timeout) = config.detached_timeout
        {
            self.detached_timeout = timeout;
        }
        if !self.explicit.max_detached
            && let Some(limit) = config.max_detached
        {
            self.max_detached = limit;
        }
        if !self.explicit.replay_capacity
            && let Some(size) = config.replay_capacity
        {
            self.replay_capacity = size;
        }
        if !self.explicit.shell && self.shell.is_none() {
            self.shell = config.shell.clone();
        }
    }
}
