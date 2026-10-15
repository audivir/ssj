use std::collections::HashSet;

/// Variable list of the client that passed the size and syntax checks of the protocol.
///
/// It does not decide which variables the server accepts. That filter is a separate step.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Env(Vec<(String, String)>);

/// Reason a variable list was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvError {
    TooMany,
    TooLarge,
    InvalidName,
    InvalidValue,
    Duplicate,
}

impl Env {
    pub const MAX_VARS: usize = 64;
    pub const MAX_NAME: usize = 256;
    pub const MAX_VALUE: usize = 8 * 1024;
    pub const MAX_TOTAL: usize = 64 * 1024;

    /// Checks count, sizes, names (`[A-Za-z_][A-Za-z0-9_]*`), and values (no NUL byte).
    pub fn new(vars: Vec<(String, String)>) -> Result<Self, EnvError> {
        if vars.len() > Self::MAX_VARS {
            return Err(EnvError::TooMany);
        }
        let mut total = 0usize;
        let mut seen = HashSet::new();
        for (name, value) in &vars {
            if name.len() > Self::MAX_NAME || value.len() > Self::MAX_VALUE {
                return Err(EnvError::TooLarge);
            }
            total += name.len() + value.len();
            if total > Self::MAX_TOTAL {
                return Err(EnvError::TooLarge);
            }
            if !valid_name(name) {
                return Err(EnvError::InvalidName);
            }
            if value.contains('\0') {
                return Err(EnvError::InvalidValue);
            }
            if !seen.insert(name.as_str()) {
                return Err(EnvError::Duplicate);
            }
        }
        Ok(Self(vars))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {
            bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }
        _ => false,
    }
}
