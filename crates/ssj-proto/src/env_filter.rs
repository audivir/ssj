//! Selection of environment variables by name pattern, for `SendEnv` and `AcceptEnv`.

use crate::env::Env;

/// List of name patterns with `*` (any characters) and `?` (one character).
///
/// A pattern with a wildcard never matches loader and startup variables such as `LD_PRELOAD`.
/// Those are matched only by a pattern that names them exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvFilter(Vec<String>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatternError {
    TooMany,
    TooLong,
    /// Patterns consist of `A-Z`, `a-z`, `0-9`, `_`, `*`, and `?`.
    InvalidCharacter,
    Empty,
}

impl EnvFilter {
    pub const MAX_PATTERNS: usize = 64;
    pub const MAX_PATTERN_LEN: usize = 256;

    pub fn new(patterns: Vec<String>) -> Result<Self, PatternError> {
        if patterns.len() > Self::MAX_PATTERNS {
            return Err(PatternError::TooMany);
        }
        for pattern in &patterns {
            if pattern.is_empty() {
                return Err(PatternError::Empty);
            }
            if pattern.len() > Self::MAX_PATTERN_LEN {
                return Err(PatternError::TooLong);
            }
            if !pattern
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'*' | b'?'))
            {
                return Err(PatternError::InvalidCharacter);
            }
        }
        Ok(Self(patterns))
    }

    /// `TERM`, `LANG`, and `LC_*`.
    pub fn default_patterns() -> Self {
        Self(vec!["TERM".into(), "LANG".into(), "LC_*".into()])
    }

    pub fn matches(&self, name: &str) -> bool {
        self.0.iter().any(|pattern| {
            if !pattern.contains(['*', '?']) {
                return pattern == name;
            }
            !is_startup_variable(name) && glob(pattern.as_bytes(), name.as_bytes())
        })
    }

    /// Splits `vars` into the accepted variables and the names of the dropped ones.
    pub fn apply(&self, vars: &Env) -> (Env, Vec<String>) {
        let (mut accepted, mut dropped) = (Vec::new(), Vec::new());
        for (name, value) in vars.iter() {
            if self.matches(name) {
                accepted.push((name.to_owned(), value.to_owned()));
            } else {
                dropped.push(name.to_owned());
            }
        }
        // a subset of a valid `Env` is valid
        (
            Env::new(accepted).expect("subset of a valid environment"),
            dropped,
        )
    }
}

fn is_startup_variable(name: &str) -> bool {
    name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("SSH_")
        || matches!(name, "BASH_ENV" | "ENV" | "PROMPT_COMMAND" | "IFS" | "PS4")
}

/// Wildcard match in linear time: after a mismatch it falls back to the last `*` only.
fn glob(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == b'?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((star_p, star_n)) => {
                    star = Some((star_p, star_n + 1));
                    p = star_p + 1;
                    n = star_n + 1;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}
