//! The config file `client.toml`: flat `key = value` lines, a strict subset of TOML.
//!
//! Strings, booleans, and integers are supported, tables are not. Unknown keys are errors, so
//! that a typo is not silently ignored.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub auto_install: AutoInstall,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    Text(String),
    Bool(bool),
    Int(i64),
}

fn parse_value(text: &str) -> Result<Value, String> {
    if let Some(inner) = text.strip_prefix('"') {
        let inner = inner.strip_suffix('"').ok_or("unterminated string")?;
        let mut out = String::new();
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Err("unescaped quote in string".to_owned()),
                '\\' => match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    _ => return Err("only \\\" and \\\\ are supported in strings".to_owned()),
                },
                c if c.is_control() => return Err("control character in string".to_owned()),
                c => out.push(c),
            }
        }
        return Ok(Value::Text(out));
    }
    match text {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        number => number
            .parse()
            .map(Value::Int)
            .map_err(|_| format!("cannot read the value {number:?}")),
    }
}

/// Removes a comment, which starts at a `#` outside of a string.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (index, c) in line.char_indices() {
        match c {
            '\\' if in_string => escaped = !escaped,
            '"' if in_string && !escaped => in_string = false,
            '"' if !in_string => in_string = true,
            '#' if !in_string => return &line[..index],
            _ => escaped = false,
        }
        if c != '\\' {
            escaped = false;
        }
    }
    line
}

impl Config {
    pub fn from_text(text: &str) -> Result<Self, String> {
        let mut config = Self::default();
        let mut seen = Vec::new();
        for (number, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let at = |message: String| format!("line {}: {message}", number + 1);
            let (key, value) = line
                .split_once('=')
                .ok_or_else(|| at("expected key = value".to_owned()))?;
            let key = key.trim();
            if !key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') || key.is_empty() {
                return Err(at(format!("invalid key {key:?}")));
            }
            if seen.contains(&key) {
                return Err(at(format!("{key} is set twice")));
            }
            seen.push(key);
            let value = parse_value(value.trim()).map_err(at)?;
            match (key, value) {
                ("auto_install", Value::Text(word)) => {
                    config.auto_install = match word.as_str() {
                        "ask" => AutoInstall::Ask,
                        "always" => AutoInstall::Always,
                        "never" => AutoInstall::Never,
                        _ => return Err(at("auto_install is ask, always, or never".to_owned())),
                    }
                }
                ("auto_install", _) => {
                    return Err(at("auto_install is ask, always, or never".to_owned()));
                }
                (other, Value::Bool(_) | Value::Int(_) | Value::Text(_)) => {
                    return Err(at(format!("unknown key {other:?}")));
                }
            }
        }
        Ok(config)
    }

    /// Path of the file: `$XDG_CONFIG_HOME/ssj/client.toml`, default `~/.config/ssj/client.toml`.
    pub fn path() -> Option<PathBuf> {
        let absolute = |name: &str| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
        };
        let base = absolute("XDG_CONFIG_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".config")))?;
        Some(base.join("ssj").join("client.toml"))
    }

    /// Reads the file if it exists. A file that others can write is refused.
    pub fn load() -> Result<Self, String> {
        let Some(path) = Self::path() else {
            return Ok(Self::default());
        };
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
        };
        let mode = fs::metadata(&path)
            .map_err(|e| e.to_string())?
            .permissions()
            .mode();
        if mode & 0o022 != 0 {
            return Err(format!(
                "{} can be changed by others (mode {:o})",
                path.display(),
                mode & 0o777
            ));
        }
        Self::from_text(&text).map_err(|e| format!("{}: {e}", path.display()))
    }
}
