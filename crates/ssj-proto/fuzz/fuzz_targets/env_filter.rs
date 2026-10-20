// fuzz target. The first line is a comment, so that the file does not look like a script.

#![no_main]

use libfuzzer_sys::fuzz_target;
use ssj_proto::{Env, EnvFilter};

// the input is the patterns, one per line, then a zero byte, then the variable names, one per line.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (patterns, names) = text.split_once('\0').unwrap_or((&text, ""));
    let Ok(filter) = EnvFilter::new(patterns.lines().map(str::to_owned).collect()) else {
        return;
    };
    let vars: Vec<_> = names.lines().map(|n| (n.to_owned(), String::new())).collect();
    for (name, _) in &vars {
        filter.matches(name);
    }
    if let Ok(env) = Env::new(vars.clone()) {
        let (accepted, dropped) = filter.apply(&env);
        assert_eq!(accepted.len() + dropped.len(), vars.len());
    }
});
