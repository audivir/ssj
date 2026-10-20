//! The completion scripts, run in the shells they are for.
//!
//! A fake `ssj` stands in for `ssj --complete HOST`, which lists the session names that are cached
//! for a host. A shell that is not installed is skipped, unless `SSJ_REQUIRE_SHELLS` is set, as in
//! CI, where a missing shell would otherwise hide a broken script.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Where the generated files are (the build script writes them next to the man page).
fn generated(name: &str) -> PathBuf {
    Path::new(env!("SSJ_ASSETS")).join(name)
}

/// A directory with a fake `ssj`: `--complete` prints the names of a host.
fn fake_ssj() -> PathBuf {
    // one directory for each call, since tests of one process run at the same time and a file
    // that is being written cannot be run.
    static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ssj-completions-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ssj");
    std::fs::write(
        &path,
        "#!/bin/sh\n\
         if [ \"$1\" = \"--complete\" ]; then\n\
           case \"$2\" in\n\
             me@box) if [ \"$3\" = \"--connected\" ]; then printf 'work\\n'; else printf 'work\\nbuild\\n'; fi ;;\n\
             \"\") printf 'work\\nbuild\\nother\\n' ;;\n\
           esac\n\
         fi\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    dir
}

fn installed(shell: &str) -> bool {
    let found = Command::new(shell)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !found && std::env::var_os("SSJ_REQUIRE_SHELLS").is_some() {
        panic!("{shell} is not installed, and SSJ_REQUIRE_SHELLS is set");
    }
    found
}

fn run(shell: &str, args: &[&str], path_dir: &Path) -> String {
    let path = format!(
        "{}:{}",
        path_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new(shell)
        .args(args)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{shell} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn bash_offers_the_sessions_of_the_host_after_attach_and_kill() {
    if !installed("bash") {
        return;
    }
    let file = generated("ssj.bash");
    let case = |words: &str, cword: usize| {
        let code = format!(
            "source '{}'\nCOMP_WORDS=({words}); COMP_CWORD={cword}; _ssj_sessions; echo \"${{COMPREPLY[*]}}\"",
            file.display()
        );
        run("bash", &["-c", &code], &fake_ssj()).trim().to_owned()
    };
    // the sessions of that host, filtered by what is typed.
    assert_eq!(case("ssj attach me@box w", 3), "work");
    assert_eq!(case("ssj kill me@box ''", 3), "work build");
    // kill takes more than one, the others do not.
    assert_eq!(case("ssj kill me@box work ''", 4), "work build");
    assert_eq!(case("ssj attach me@box work ''", 4), "");
    // detach offers only the sessions that have a client, and more than one.
    assert_eq!(case("ssj detach me@box ''", 3), "work");
    assert_eq!(case("ssj detach me@box work ''", 4), "work");
    // another host has none.
    assert_eq!(case("ssj attach other@host ''", 3), "");
    // --name offers the names of all hosts.
    assert_eq!(case("ssj --name ''", 2), "work build other");
}

#[test]
fn zsh_asks_for_commands_hosts_and_the_sessions_of_the_host() {
    if !installed("zsh") {
        return;
    }
    let file = generated("_ssj");
    // zsh -n finds a syntax error in the whole file.
    run("zsh", &["-n", file.to_str().unwrap()], &fake_ssj());
    let first_line = std::fs::read_to_string(&file).unwrap();
    assert!(first_line.starts_with("#compdef ssj\n"));

    let case = |words: &str, current: usize| {
        // the completion system is replaced by functions that print what they are asked for.
        let code = format!(
            "compdef() {{ :; }}\nsource '{}'\n\
             compadd() {{ if [[ $1 == -a ]]; then print -r -- \"names: ${{(P)2}}\"; else print -r -- \"add: $*\"; fi; }}\n\
             _hosts() {{ print hosts; }}\n\
             _files() {{ print files; }}\n\
             _alternative() {{ print \"alternative: $*\"; }}\n\
             words=({words}); CURRENT={current}; _ssj_words",
            file.display()
        );
        run("zsh", &["-f", "-c", &code], &fake_ssj())
            .trim()
            .to_owned()
    };
    // the first word is a command or a host.
    assert!(case("''", 1).contains("alternative:"), "{}", case("''", 1));
    // the word after a command is a host.
    assert_eq!(case("attach ''", 2), "hosts");
    assert_eq!(case("list ''", 2), "hosts");
    // the word after the host of attach and kill is a session of that host.
    assert_eq!(case("attach me@box ''", 3), "names: work build");
    assert_eq!(case("kill me@box ''", 3), "names: work build");
    assert_eq!(case("kill me@box work ''", 4), "names: work build");
    assert_eq!(case("attach me@box work ''", 4), "");
    assert_eq!(case("detach me@box ''", 3), "names: work");
    assert_eq!(case("detach me@box work ''", 4), "names: work");
    assert_eq!(case("attach other@host ''", 3), "names:");
    // list and install take nothing more.
    assert_eq!(case("list me@box ''", 3), "");
    // after the destination come the words of the command for the host: files, not hosts.
    assert_eq!(case("me@box ''", 2), "files");
    assert_eq!(case("me@box rm ''", 3), "files");
    assert_eq!(case("me@box rm -rf ''", 4), "files");
    // a host that is called like a command, after the dashes
    assert_eq!(case("attach -- ''", 3), "files");
}

#[test]
fn fish_offers_the_sessions_of_the_host_after_attach_and_kill() {
    if !installed("fish") {
        return;
    }
    let file = generated("ssj.fish");
    let case = |line: &str| {
        let code = format!("source '{}'; complete -C '{line}'", file.display());
        let out = run("fish", &["-c", &code], &fake_ssj());
        let mut names: Vec<String> = out
            .lines()
            .filter_map(|l| l.split('\t').next())
            .filter(|n| matches!(*n, "work" | "build" | "other"))
            .map(str::to_owned)
            .collect();
        names.sort();
        names.join(" ")
    };
    assert_eq!(case("ssj attach me@box "), "build work");
    assert_eq!(case("ssj kill me@box w"), "work");
    // kill takes more than one, the others do not.
    assert_eq!(case("ssj kill me@box work "), "build work");
    assert_eq!(case("ssj attach me@box work "), "");
    assert_eq!(case("ssj detach me@box "), "work");
    assert_eq!(case("ssj detach me@box work "), "work");
    // options in front do not count as words.
    assert_eq!(case("ssj -v attach me@box "), "build work");
    // no sessions after the other commands, or for another host, or before the host.
    assert_eq!(case("ssj list me@box "), "");
    assert_eq!(case("ssj attach other@host "), "");
    assert_eq!(case("ssj attach "), "");
    // --name offers the names of all hosts.
    assert_eq!(case("ssj --name "), "build other work");
}

/// Options with a fixed set of values, and the values.
const FIXED: [(&str, &str); 4] = [
    ("--local-echo", "auto off"),
    ("--compress", "auto off"),
    ("--congestion", "auto bbr cubic"),
    ("--transport", "auto stdio tcp udp"),
];

#[test]
fn bash_completes_the_values_of_options_with_a_fixed_set() {
    if !installed("bash") {
        return;
    }
    let file = generated("ssj.bash");
    // what bash does: it splits the words at the `=`, and calls the function with the command,
    // the word under the cursor, and the one before it.
    let case = |words: &str, cword: usize| {
        let code = format!(
            "source '{}'\nCOMP_WORDS=({words}); COMP_CWORD={cword}; \
             _ssj_sessions ssj \"${{COMP_WORDS[COMP_CWORD]}}\" \"${{COMP_WORDS[COMP_CWORD-1]}}\"; \
             printf '%s\\n' \"${{COMPREPLY[@]}}\"",
            file.display()
        );
        let mut values: Vec<String> = run("bash", &["-c", &code], &fake_ssj())
            .lines()
            .map(str::to_owned)
            .collect();
        values.sort();
        values.join(" ")
    };
    for (option, values) in FIXED {
        // after a space
        assert_eq!(
            case(&format!("ssj {option} ''"), 2),
            values,
            "{option} <TAB>"
        );
        let first = &values[..1];
        let wanted: Vec<&str> = values.split(' ').filter(|v| v.starts_with(first)).collect();
        assert_eq!(
            case(&format!("ssj {option} {first}"), 2),
            wanted.join(" "),
            "{option} {first}<TAB>"
        );
        // after the `=`, with nothing typed and with the start of a value
        assert_eq!(
            case(&format!("ssj {option} ="), 2),
            values,
            "{option}=<TAB>"
        );
        assert_eq!(
            case(&format!("ssj {option} = {first}"), 3),
            wanted.join(" "),
            "{option}={first}<TAB>"
        );
    }
}

#[test]
fn fish_completes_the_values_of_options_with_a_fixed_set() {
    if !installed("fish") {
        return;
    }
    let file = generated("ssj.fish");
    let case = |line: &str| {
        let code = format!("source '{}'; complete -C '{line}'", file.display());
        let out = run("fish", &["-c", &code], &fake_ssj());
        let mut values: Vec<String> = out
            .lines()
            .filter_map(|l| l.split('\t').next())
            .map(|v| v.rsplit('=').next().unwrap_or(v).to_owned())
            .collect();
        values.sort();
        values.join(" ")
    };
    for (option, values) in FIXED {
        assert_eq!(case(&format!("ssj {option} ")), values, "{option} <TAB>");
        assert_eq!(case(&format!("ssj {option}=")), values, "{option}=<TAB>");
    }
}

#[test]
fn zsh_lists_the_values_of_options_with_a_fixed_set() {
    let text = std::fs::read_to_string(generated("_ssj")).unwrap();
    for (option, values) in FIXED {
        let mut sorted: Vec<&str> = values.split(' ').collect();
        // clap lists the values in the order of the definition, which the help also shows.
        sorted.sort_unstable();
        let line = text
            .lines()
            .find(|l| l.contains(&format!("'{option}=[")))
            .unwrap_or_else(|| panic!("no line for {option}"));
        let listed = line
            .rsplit_once(":(")
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(values, _)| values)
            .unwrap();
        let mut listed: Vec<&str> = listed.split(' ').collect();
        listed.sort_unstable();
        assert_eq!(listed, sorted, "{option}");
    }
}
