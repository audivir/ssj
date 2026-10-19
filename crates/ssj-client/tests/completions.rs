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
    let dir = std::env::temp_dir().join(format!("ssj-completions-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ssj");
    std::fs::write(
        &path,
        "#!/bin/sh\n\
         if [ \"$1\" = \"--complete\" ]; then\n\
           case \"$2\" in\n\
             me@box) printf 'work\\nbuild\\n' ;;\n\
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
    // the sessions of that host, filtered by what is typed
    assert_eq!(case("ssj attach me@box w", 3), "work");
    assert_eq!(case("ssj kill me@box ''", 3), "work build");
    // another host has none
    assert_eq!(case("ssj attach other@host ''", 3), "");
    // --name offers the names of all hosts
    assert_eq!(case("ssj --name ''", 2), "work build other");
}

#[test]
fn zsh_asks_for_commands_hosts_and_the_sessions_of_the_host() {
    if !installed("zsh") {
        return;
    }
    let file = generated("_ssj");
    // zsh -n finds a syntax error in the whole file
    run("zsh", &["-n", file.to_str().unwrap()], &fake_ssj());
    let first_line = std::fs::read_to_string(&file).unwrap();
    assert!(first_line.starts_with("#compdef ssj\n"));

    let case = |words: &str, current: usize| {
        // the completion system is replaced by functions that print what they are asked for
        let code = format!(
            "compdef() {{ :; }}\nsource '{}'\n\
             compadd() {{ if [[ $1 == -a ]]; then print -r -- \"names: ${{(P)2}}\"; else print -r -- \"add: $*\"; fi; }}\n\
             _hosts() {{ print hosts; }}\n\
             _alternative() {{ print \"alternative: $*\"; }}\n\
             words=({words}); CURRENT={current}; _ssj_words",
            file.display()
        );
        run("zsh", &["-f", "-c", &code], &fake_ssj())
            .trim()
            .to_owned()
    };
    // the first word is a command or a host
    assert!(case("''", 1).contains("alternative:"), "{}", case("''", 1));
    // the word after a command is a host
    assert_eq!(case("attach ''", 2), "hosts");
    assert_eq!(case("list ''", 2), "hosts");
    // the word after the host of attach and kill is a session of that host
    assert_eq!(case("attach me@box ''", 3), "names: work build");
    assert_eq!(case("kill me@box ''", 3), "names: work build");
    assert_eq!(case("attach other@host ''", 3), "names:");
    // list and install take nothing more
    assert_eq!(case("list me@box ''", 3), "");
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
    // options in front do not count as words
    assert_eq!(case("ssj -v attach me@box "), "build work");
    // no sessions after the other commands, or for another host, or before the host
    assert_eq!(case("ssj list me@box "), "");
    assert_eq!(case("ssj attach other@host "), "");
    assert_eq!(case("ssj attach "), "");
    // --name offers the names of all hosts
    assert_eq!(case("ssj --name "), "build other work");
}
