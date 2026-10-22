//! The config files of `ssj`: flat `key = value` lines of TOML, without tables.
//!
//! A program reads the keys it knows with the `take_*` functions and then calls
//! [`Table::finish`], which turns every key left over into an error, so that a typo is not
//! silently ignored. Errors carry the line number.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use toml::de::{DeTable, DeValue};

/// Container for the keys of a file that have not been read yet, each with its line.
pub struct Table<'a>(BTreeMap<String, (usize, DeValue<'a>)>);

/// Parses the text of a file.
pub fn parse(text: &str) -> Result<Table<'_>, String> {
    let line_of = |offset: usize| text[..offset].matches('\n').count() + 1;
    let root = DeTable::parse(text)
        .map_err(|e| {
            format!(
                "line {}: {}",
                line_of(e.span().map_or(0, |s| s.start)),
                e.message()
            )
        })?
        .into_inner();
    Ok(Table(
        root.into_iter()
            .map(|(key, value)| {
                let line = line_of(key.span().start);
                (key.into_inner().into_owned(), (line, value.into_inner()))
            })
            .collect(),
    ))
}

impl Table<'_> {
    /// Returns the value of `key` as read by `convert`, or `None` if the key is not set.
    ///
    /// `expected` ends the error text `<key> must be ...` when `convert` refuses the value.
    fn take<T>(
        &mut self,
        key: &str,
        expected: &str,
        convert: impl FnOnce(&DeValue<'_>) -> Option<T>,
    ) -> Result<Option<T>, String> {
        let Some((line, value)) = self.0.remove(key) else {
            return Ok(None);
        };
        convert(&value)
            .map(Some)
            .ok_or_else(|| format!("line {line}: {key} must be {expected}"))
    }

    /// A string value, or `None` if the key is not set.
    pub fn take_text(&mut self, key: &str) -> Result<Option<String>, String> {
        self.take(key, "a string in double quotes", |v| {
            v.as_str().map(str::to_owned)
        })
    }

    /// `true` or `false`, or `None` if the key is not set.
    pub fn take_bool(&mut self, key: &str) -> Result<Option<bool>, String> {
        self.take(key, "true or false", |v| v.as_bool())
    }

    /// A list of strings, or `None` if the key is not set.
    pub fn take_list(&mut self, key: &str) -> Result<Option<Vec<String>>, String> {
        self.take(key, "a list of strings, like [\"a\", \"b\"]", |v| {
            v.as_array()?
                .iter()
                .map(|item| item.get_ref().as_str().map(str::to_owned))
                .collect()
        })
    }

    /// A whole number that is not negative, or `None` if the key is not set.
    pub fn take_unsigned(&mut self, key: &str) -> Result<Option<u64>, String> {
        self.take(key, "a whole number that is not negative", |v| {
            let n = v.as_integer()?;
            u64::from_str_radix(n.as_str(), n.radix()).ok()
        })
    }

    /// Seconds, written as a whole number or as `"none"`. `Some(None)` means `none`.
    pub fn take_seconds(&mut self, key: &str) -> Result<Option<Option<u64>>, String> {
        self.take(
            key,
            "a number of seconds, not negative, or \"none\"",
            |v| match v.as_str() {
                Some("none") => Some(None),
                Some(_) => None,
                None => {
                    let n = v.as_integer()?;
                    u64::from_str_radix(n.as_str(), n.radix()).ok().map(Some)
                }
            },
        )
    }

    /// Fails if keys are left, which no program asked for.
    pub fn finish(self) -> Result<(), String> {
        match self.0.into_iter().min_by_key(|(_, (line, _))| *line) {
            None => Ok(()),
            Some((key, (line, _))) => Err(format!("line {line}: unknown key {key:?}")),
        }
    }
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
