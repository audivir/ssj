//! The config files of `ssj`: flat `key = value` lines of TOML, read into a struct that derives
//! [`confique::Config`].
//!
//! The doc comments and defaults of the struct are the documentation of the file: [`template`]
//! writes them as the file a new user starts with. A key that the struct does not have is an
//! error, so that a typo is not silently ignored. Errors carry the line number.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub use confique::{Config, serde};

/// Defines a `deserialize_with` function that reads a `$from` and turns it into a `$to` with
/// `$parse`, whose `String` error becomes the message of the key.
#[macro_export]
macro_rules! parsed {
    ($name:ident, $from:ty => $to:ty, $parse:expr) => {
        fn $name<'de, D: $crate::serde::Deserializer<'de>>(d: D) -> Result<$to, D::Error> {
            use $crate::serde::{Deserialize, de::Error};
            let parse: fn($from) -> Result<$to, String> = $parse;
            parse(<$from>::deserialize(d)?).map_err(D::Error::custom)
        }
    };
}

/// Parses the text of a file, with the defaults of `C` for the keys it leaves out.
pub fn parse<C: Config>(text: &str) -> Result<C, String> {
    use confique::Layer;
    let layer: C::Layer = toml::from_str(text).map_err(|e| {
        let start = e.span().map_or(0, |s| s.start);
        let line = text[..start].matches('\n').count() + 1;
        // an error in a value names its key, which the span of the value does not include.
        let key = text[text[..start].rfind('\n').map_or(0, |i| i + 1)..start]
            .split_once('=')
            .map_or(String::new(), |(key, _)| format!("{}: ", key.trim()));
        format!("line {line}: {key}{}", e.message())
    })?;
    C::from_layer(layer.with_fallback(C::Layer::default_values())).map_err(|e| e.to_string())
}

/// Returns the file that a new user starts with: every key as a comment, with its documentation
/// and default.
pub fn template<C: Config>() -> String {
    confique::toml::template::<C>(confique::toml::FormatOptions::default())
}

/// Path of a file of the program: `$XDG_CONFIG_HOME/ssj/<name>`, default `~/.config/ssj/<name>`.
pub fn path(name: &str) -> Option<PathBuf> {
    let absolute = |var: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let base = absolute("XDG_CONFIG_HOME")
        .or_else(|| absolute("HOME").map(|home| home.join(".config")))?;
    Some(base.join("ssj").join(name))
}

/// Reads the file if it exists. A file that group or others can write is refused.
pub fn read_file(name: &str) -> Result<Option<(PathBuf, String)>, String> {
    let Some(path) = path(name) else {
        return Ok(None);
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
    };
    let mode = fs::metadata(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .permissions()
        .mode();
    if mode & 0o022 != 0 {
        return Err(format!(
            "{} can be changed by others (mode {:o})",
            path.display(),
            mode & 0o777
        ));
    }
    Ok(Some((path, text)))
}
