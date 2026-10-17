//! The config files of `ssj`: flat `key = value` lines, a strict subset of TOML.
//!
//! Strings, booleans, integers, and single-line lists of strings are supported. Tables are not.
//! A program reads the keys it knows with the `take_*` functions and then calls
//! [`Table::finish`], which turns every key left over into an error, so that a typo is not
//! silently ignored. Errors carry the line number.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Text(String),
    Bool(bool),
    Int(i64),
    List(Vec<String>),
}

struct Entry {
    key: String,
    value: Value,
    line: usize,
}

/// The keys of a file that have not been read yet.
pub struct Table(Vec<Entry>);

fn parse_string(text: &str) -> Result<(String, &str), String> {
    let rest = text
        .strip_prefix('"')
        .ok_or("expected a string in double quotes")?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((index, c)) = chars.next() {
        match c {
            '"' => return Ok((out, &rest[index + 1..])),
            '\\' => match chars.next() {
                Some((_, '"')) => out.push('"'),
                Some((_, '\\')) => out.push('\\'),
                _ => return Err("only \\\" and \\\\ are supported in strings".to_owned()),
            },
            c if c.is_control() => return Err("control character in string".to_owned()),
            c => out.push(c),
        }
    }
    Err("unterminated string".to_owned())
}

fn parse_value(text: &str) -> Result<Value, String> {
    if text.starts_with('"') {
        let (string, rest) = parse_string(text)?;
        return if rest.trim().is_empty() {
            Ok(Value::Text(string))
        } else {
            Err("text after the string".to_owned())
        };
    }
    if let Some(inner) = text.strip_prefix('[') {
        let mut rest = inner.trim_start();
        let mut items = Vec::new();
        loop {
            if let Some(after) = rest.strip_prefix(']') {
                return if after.trim().is_empty() {
                    Ok(Value::List(items))
                } else {
                    Err("text after the list".to_owned())
                };
            }
            let (item, after) = parse_string(rest)?;
            items.push(item);
            rest = after.trim_start();
            match rest.strip_prefix(',') {
                Some(after_comma) => rest = after_comma.trim_start(),
                None if rest.starts_with(']') => {}
                None => return Err("expected a comma or ] in the list".to_owned()),
            }
        }
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
    let (mut in_string, mut escaped) = (false, false);
    for (index, c) in line.char_indices() {
        if in_string {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => in_string = false,
                _ => escaped = false,
            }
        } else if c == '"' {
            in_string = true;
        } else if c == '#' {
            return &line[..index];
        }
    }
    line
}

/// Parses the text of a file.
pub fn parse(text: &str) -> Result<Table, String> {
    let mut entries: Vec<Entry> = Vec::new();
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
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
            return Err(at(format!("invalid key {key:?}")));
        }
        if entries.iter().any(|e| e.key == key) {
            return Err(at(format!("{key} is set twice")));
        }
        let value = parse_value(value.trim()).map_err(at)?;
        entries.push(Entry {
            key: key.to_owned(),
            value,
            line: number + 1,
        });
    }
    Ok(Table(entries))
}

impl Table {
    fn take(&mut self, key: &str) -> Option<Entry> {
        let index = self.0.iter().position(|e| e.key == key)?;
        Some(self.0.remove(index))
    }

    /// A string value, or `None` if the key is not set.
    pub fn take_text(&mut self, key: &str) -> Result<Option<String>, String> {
        match self.take(key) {
            None => Ok(None),
            Some(Entry {
                value: Value::Text(text),
                ..
            }) => Ok(Some(text)),
            Some(entry) => Err(format!(
                "line {}: {key} must be a string in double quotes",
                entry.line
            )),
        }
    }

    /// A list of strings, or `None` if the key is not set.
    pub fn take_list(&mut self, key: &str) -> Result<Option<Vec<String>>, String> {
        match self.take(key) {
            None => Ok(None),
            Some(Entry {
                value: Value::List(items),
                ..
            }) => Ok(Some(items)),
            Some(entry) => Err(format!(
                "line {}: {key} must be a list of strings, like [\"a\", \"b\"]",
                entry.line
            )),
        }
    }

    /// Seconds, written as a whole number or as `"none"`. `Some(None)` means `none`.
    pub fn take_seconds(&mut self, key: &str) -> Result<Option<Option<u64>>, String> {
        match self.take(key) {
            None => Ok(None),
            Some(Entry {
                value: Value::Int(n),
                line,
                ..
            }) => u64::try_from(n)
                .map(|n| Some(Some(n)))
                .map_err(|_| format!("line {line}: {key} must not be negative")),
            Some(Entry {
                value: Value::Text(text),
                ..
            }) if text == "none" => Ok(Some(None)),
            Some(entry) => Err(format!(
                "line {}: {key} must be a number of seconds or \"none\"",
                entry.line
            )),
        }
    }

    /// Fails if keys are left, which no program asked for.
    pub fn finish(self) -> Result<(), String> {
        match self.0.first() {
            None => Ok(()),
            Some(entry) => Err(format!("line {}: unknown key {:?}", entry.line, entry.key)),
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
