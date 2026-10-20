//! The destination on the command line: `[user@]host[:port]`, `ssh://[user@]host[:port]`, and
//! IPv6 literals.
//!
//! The text goes to `ssh` as an argument and to DNS as a name, so only the characters of names and
//! addresses are accepted, and nothing that `ssh` could take for an option.

use std::fmt;
use std::net::Ipv6Addr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Destination {
    pub user: Option<String>,
    /// A host name, an IPv4 address, or an IPv6 address without brackets (with an optional zone).
    pub host: String,
    pub port: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationError {
    Empty,
    BadUser,
    BadHost,
    BadPort,
}

impl fmt::Display for DestinationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "the destination is empty",
            Self::BadUser => "the user name has characters that are not allowed",
            Self::BadHost => "the host name has characters that are not allowed",
            Self::BadPort => "the port must be a number from 1 to 65535",
        })
    }
}

const MAX_HOST: usize = 253;
const MAX_USER: usize = 64;

impl Destination {
    /// The argument for `ssh`: `user@host` or `host`, without the port and without brackets.
    pub fn target(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }

    fn is_v6(&self) -> bool {
        self.host.contains(':')
    }
}

/// The canonical form, which parses back to the same destination.
impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(user) = &self.user {
            write!(f, "{user}@")?;
        }
        if self.is_v6() {
            write!(f, "[{}]", self.host)?;
        } else {
            f.write_str(&self.host)?;
        }
        if let Some(port) = self.port {
            write!(f, ":{port}")?;
        }
        Ok(())
    }
}

pub fn parse(text: &str) -> Result<Destination, DestinationError> {
    if text.is_empty() {
        return Err(DestinationError::Empty);
    }
    let rest = text.strip_prefix("ssh://").unwrap_or(text);
    // the address form of a URI may end with a slash.
    let rest = if rest.len() < text.len() {
        rest.strip_suffix('/').unwrap_or(rest)
    } else {
        rest
    };
    let (user, hostport) = match rest.split_once('@') {
        Some((user, hostport)) => {
            if !valid_user(user) {
                return Err(DestinationError::BadUser);
            }
            (Some(user.to_owned()), hostport)
        }
        None => (None, rest),
    };
    let (host, port) = split_host_port(hostport)?;
    Ok(Destination { user, host, port })
}

fn split_host_port(text: &str) -> Result<(String, Option<u16>), DestinationError> {
    if let Some(inner) = text.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or(DestinationError::BadHost)?;
        let port = match after {
            "" => None,
            after => Some(parse_port(
                after.strip_prefix(':').ok_or(DestinationError::BadHost)?,
            )?),
        };
        return if valid_v6(host) {
            Ok((host.to_owned(), port))
        } else {
            Err(DestinationError::BadHost)
        };
    }
    match text.matches(':').count() {
        0 => valid_name(text)
            .then(|| (text.to_owned(), None))
            .ok_or(DestinationError::BadHost),
        1 => {
            let (host, port) = text.split_once(':').expect("one colon");
            if !valid_name(host) {
                return Err(DestinationError::BadHost);
            }
            Ok((host.to_owned(), Some(parse_port(port)?)))
        }
        // more colons: an IPv6 address without brackets, which has no room for a port.
        _ => valid_v6(text)
            .then(|| (text.to_owned(), None))
            .ok_or(DestinationError::BadHost),
    }
}

fn parse_port(text: &str) -> Result<u16, DestinationError> {
    let plain = (1..=5).contains(&text.len())
        && text.bytes().all(|b| b.is_ascii_digit())
        && !text.starts_with('0');
    plain
        .then(|| text.parse().ok())
        .flatten()
        .ok_or(DestinationError::BadPort)
}

fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')
}

fn valid_user(user: &str) -> bool {
    (1..=MAX_USER).contains(&user.len())
        && !user.starts_with(['-', '.'])
        && user.bytes().all(|b| word_byte(b) || b == b'+')
}

/// A host name, an alias from the `ssh` configuration, or an IPv4 address.
fn valid_name(host: &str) -> bool {
    (1..=MAX_HOST).contains(&host.len())
        && !host.starts_with(['-', '.'])
        && host.bytes().all(word_byte)
}

/// An IPv6 address, with an optional zone (`fe80::1%en0`).
fn valid_v6(host: &str) -> bool {
    let (address, zone) = match host.split_once('%') {
        Some((address, zone)) => (address, Some(zone)),
        None => (host, None),
    };
    address.parse::<Ipv6Addr>().is_ok()
        && zone.is_none_or(|zone| {
            (1..=32).contains(&zone.len()) && zone.bytes().all(word_byte) && !zone.starts_with('-')
        })
}
