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

    // hosts: the first word, and the word after a command, when bash-completion is loaded.
    let hosts = |words: &str, cword: usize| {
        let code = format!(
            "source '{}'\n_known_hosts_real() {{ COMPREPLY=(boxa boxb); }}\n\
             COMP_WORDS=({words}); COMP_CWORD={cword}; \
             _ssj_sessions ssj \"${{COMP_WORDS[COMP_CWORD]}}\" \"${{COMP_WORDS[COMP_CWORD-1]}}\"; \
             echo \"${{COMPREPLY[*]}}\"",
            file.display()
        );
        run("bash", &["-c", &code], &fake_ssj()).trim().to_owned()
    };
    assert!(
        hosts("ssj ''", 1).ends_with("boxa boxb"),
        "{}",
        hosts("ssj ''", 1)
    );
    assert_eq!(hosts("ssj attach ''", 2), "boxa boxb");
    assert_eq!(hosts("ssj list ''", 2), "boxa boxb");
    // not for the session, nor for a second word after a host, nor for an option.
    assert_eq!(hosts("ssj attach me@box ''", 3), "work build");
    assert!(!hosts("ssj --na", 1).contains("boxa"));
    // the same as in zsh: the commands, the options of a command after its host, nothing of ssj
    // after any other host, and the values of options are not destinations.
    assert!(hosts("ssj ''", 1).starts_with("list attach kill detach install edit "));
    assert!(hosts("ssj install me@box --ser", 3).contains("--server-binary"));
    assert!(hosts("ssj kill me@box --fo", 3).contains("--force"));
    assert_eq!(hosts("ssj me@box ls --ser", 3), "");
    // only the options that the command reads
    assert_eq!(
        hosts("ssj edit me@box -", 3),
        "--verbose --log-file --ssh-command --server-path --server-arg"
    );
    assert!(!hosts("ssj kill me@box -", 3).contains("--json"));
    // `connect` is the plain connection: the hosts, the options of the connection before the
    // host, and files for the command on the host after it.
    assert_eq!(hosts("ssj connect ''", 2), "boxa boxb");
    assert_eq!(hosts("ssj connect --transport tcp ''", 4), "boxa boxb");
    assert!(hosts("ssj connect --tr", 2).contains("--transport"));
    assert_eq!(hosts("ssj connect me@box ''", 3), "");
    // the values of options: the fixed ones, files, and the hosts of the ssh config
    assert_eq!(hosts("ssj --transport ''", 2), "auto udp tcp stdio");
    assert_eq!(hosts("ssj attach me@box --compress ''", 4), "off auto");
    assert_eq!(hosts("ssj --udp-host ''", 2), "boxa boxb");
    assert_eq!(hosts("ssj install me@box --udp-host ''", 4), "");
    assert_eq!(hosts("ssj attach me@box --udp-host ''", 4), "boxa boxb");
    assert_eq!(hosts("ssj connect me@box -l", 3), "");
    assert!(hosts("ssj list me@box -", 3).contains("--json"));
    assert_eq!(hosts("ssj me@box ''", 2), "");
    assert_eq!(hosts("ssj attach -- ''", 3), "");
    assert_eq!(hosts("ssj --name x attach ''", 4), "boxa boxb");
    assert_eq!(hosts("ssj --transport udp list ''", 4), "boxa boxb");
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

    let case_with = |words: &str, current: usize, prefix: &str| {
        // the completion system is replaced by functions that print what they are asked for.
        let code = format!(
            "compdef() {{ :; }}\nsource '{}'\n\
             compadd() {{ if [[ $1 == -a ]]; then print -r -- \"names: ${{(P)2}}\"; else print -r -- \"add: $*\"; fi; }}\n\
             _hosts() {{ print hosts; }}\n\
             _files() {{ print files; }}\n\
             _alternative() {{ print \"alternative: $*\"; }}\n\
             PREFIX='{prefix}'; words=({words}); CURRENT={current}; _ssj_words",
            file.display()
        );
        run("zsh", &["-f", "-c", &code], &fake_ssj())
            .trim()
            .to_owned()
    };
    let case = |words: &str, current: usize| case_with(words, current, "");
    // the first word is a command or a host.
    assert!(case("''", 1).contains("alternative:"), "{}", case("''", 1));
    assert!(
        case("''", 1).contains(" install edit "),
        "{}",
        case("''", 1)
    );
    // the word after a command is a host.
    assert_eq!(case("attach ''", 2), "hosts");
    // the aliases of the ssh config are offered when zsh knows how to read them.
    let code = format!(
        "compdef() {{ :; }}\nsource '{}'\n\
         _ssh_hosts() {{ print ssh hosts; }}\n\
         _hosts() {{ print hosts; }}\n\
         words=(list ''); CURRENT=2; PREFIX=''; _ssj_words",
        file.display()
    );
    assert_eq!(
        run("zsh", &["-f", "-c", &code], &fake_ssj()).trim(),
        "ssh hosts"
    );
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
    // the options of a command stand anywhere, also after the destination.
    let options = case_with;
    let offered = options("install me@box --ser", 3, "--ser");
    assert!(
        offered.starts_with("add: -- ") && offered.contains(" --server-binary"),
        "{offered}"
    );
    assert!(options("install --ser", 2, "--ser").contains(" --server-binary"));
    // only the options that the command reads
    let install = options("install me@box -", 3, "-");
    assert!(install.contains(" --server-binary") && !install.contains(" --transport"));
    let kill = options("kill me@box -", 3, "-");
    assert!(kill.contains(" --force") && !kill.contains(" --json"));
    assert!(options("list me@box -", 3, "-").contains(" --json"));
    // `connect`: the hosts, the options of the connection before the host, files after it
    assert_eq!(case("connect ''", 2), "hosts");
    assert_eq!(case("connect --transport tcp ''", 4), "hosts");
    assert!(options("connect --tr", 2, "--tr").contains(" --transport"));
    assert_eq!(case("connect me@box ''", 3), "files");
    // the values of options, also after the destination of a command: files, the fixed values,
    // and the hosts of the ssh config
    assert_eq!(case("install me@box --server-binary ''", 4), "files");
    assert_eq!(case("list me@box --log-file ''", 4), "files");
    assert_eq!(case("attach me@box --udp-host ''", 4), "hosts");
    assert_eq!(case("install me@box --udp-host ''", 4), "");
    assert_eq!(
        case("attach me@box --transport ''", 4),
        "add: -- auto udp tcp stdio"
    );
    assert_eq!(case("attach me@box --compress ''", 4), "add: -- off auto");
    assert_eq!(case("connect --udp-host ''", 3), "hosts");
    assert_eq!(options("connect me@box -l", 3, "-l"), "files");
    assert!(options("kill me@box --fo", 3, "--fo").contains(" --force"));
    // after the host of a command for the host the words are files.
    assert_eq!(options("me@box ls --ser", 3, "--ser"), "files");
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

#[test]
fn fish_offers_the_commands_and_the_hosts() {
    if !installed("fish") {
        return;
    }
    let file = generated("ssj.fish");
    let case = |line: &str| {
        // `__fish_print_hostnames` is replaced, since it reads the files of the person.
        let code = format!(
            "function __fish_print_hostnames; printf 'boxa\\nboxb\\n'; end; source '{}'; complete -C '{line}'",
            file.display()
        );
        let out = run("fish", &["-c", &code], &fake_ssj());
        let mut names: Vec<String> = out
            .lines()
            .filter_map(|l| l.split('\t').next())
            .filter(|n| matches!(*n, "boxa" | "boxb" | "attach" | "list"))
            .map(str::to_owned)
            .collect();
        names.sort();
        names.join(" ")
    };
    assert_eq!(case("ssj "), "attach boxa boxb list");
    assert_eq!(case("ssj attach "), "boxa boxb");
    assert_eq!(case("ssj -v list "), "boxa boxb");
    // after the host the sessions follow, not the hosts.
    assert_eq!(case("ssj attach me@box "), "");
    // the values of options are not destinations.
    assert_eq!(case("ssj --name x attach "), "boxa boxb");
    assert_eq!(case("ssj --transport udp "), "attach boxa boxb list");
    // the dashes after another destination are for the command on the host.
    let options = |line: &str| {
        let code = format!("source '{}'; complete -C '{line}'", file.display());
        run("fish", &["-c", &code], &fake_ssj())
            .lines()
            .any(|l| l.starts_with("--server-binary"))
    };
    assert!(options("ssj install me@box --ser"));
    assert!(!options("ssj list me@box --ser"));
    // `connect`: the hosts, the options before the host, and the command for the host after it
    assert_eq!(case("ssj connect "), "boxa boxb");
    assert_eq!(case("ssj connect --transport tcp "), "boxa boxb");
    assert!(options("ssj connect --ser"));
    // the values of options
    assert_eq!(case("ssj --udp-host "), "boxa boxb");
    assert_eq!(case("ssj attach me@box --udp-host "), "boxa boxb");
    // the values of an option that the command does not read are not completed
    assert_eq!(case("ssj install me@box --udp-host "), "");
    assert!(!options("ssj connect me@box --ser"));
    assert!(options("ssj --ser"));
    assert!(!options("ssj me@box ls --ser"));
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
