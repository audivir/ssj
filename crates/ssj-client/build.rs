//! Writes the man page and the shell completions to `$OUT_DIR`, from where the release workflow
//! picks them up.

use std::{env, fs, path::PathBuf};

use clap::CommandFactory;
use clap_complete::Shell;

#[path = "src/options.rs"]
mod options;

/// What the options cannot show: the config files and the cache.
const FILES: &str = r#".SH FILES
All config files are flat \fIkey = value\fR lines, a strict subset of TOML. A typo in a key is an
error with the line number, and a file that others can write is refused. Options on the command
line override the file.
.TP
\fI$XDG_CONFIG_HOME/ssj/client.toml\fR
(default \fI~/.config/ssj/client.toml\fR) Settings of \fBssj\fR:
.RS
.TP
\fBauto_install\fR = "ask" | "always" | "never"
Whether to install the server when it is missing.
.TP
\fBescape_char\fR = "~" | "none"
The escape character.
.TP
\fBssh_command\fR = "ssh -J jump"
The command that replaces \fBssh\fR.
.TP
\fBsend_env\fR = ["TERM", "LANG", "LC_*"]
The variables sent to the server.
.TP
\fBtransport\fR = "auto" | "udp" | "tcp" | "stdio"
How to reach the server.
.TP
\fBauto_attach\fR = true | false
Join the only session without a client.
.TP
\\fBlocal_echo\\fR = "off" | "auto"
Show typed characters at once, before the server has echoed them.
.RE
.TP
\fI$XDG_CONFIG_HOME/ssj/server.toml\fR
Settings of \fBssj-server\fR, read on the host:
.RS
.TP
\fBport_range\fR = "60000:61000"
The UDP ports the server may use, "0" for any.
.TP
\fBdetached_timeout\fR = seconds | "none"
How long a session lives without a client.
.TP
\fBmax_detached\fR = seconds | "none"
An upper limit that also applies after a deliberate detach.
.TP
\fBreplay_capacity\fR = bytes
The output kept for a client that is away.
.TP
\fBslow_client\fR = "drop" | "wait"
What the server does with output that a connected client is too slow to take: "drop" (default) never lets the job wait for the link and skips the oldest output, with a notice; "wait" slows the job down, as ssh does.
.TP
\fBlive_backlog\fR = bytes
With "drop": how much output may wait for a connected client (default 1 MiB).
.TP
\fBaway_after\fR = seconds
With "wait": how long a silent client may block the output.
.TP
\fBshell\fR = "/bin/zsh"
The shell to start (default: $SHELL).
.TP
\fBaccept_env\fR = ["TERM", "LANG", "LC_*"]
The variables of the client that the shell gets. Only the file sets it.
.RE
.TP
\fI$XDG_CACHE_HOME/ssj/sessions/\fR
The session names last seen on each host, for tab completion. Written by \fBssj list\fR and
refreshed in the background.
"#;

fn main() {
    println!("cargo:rerun-if-changed=src/options.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    // where the tests and the release workflow find the files
    println!("cargo:rustc-env=SSJ_ASSETS={}", out.display());
    // the generators list hidden options too, so they are left out here
    let full = options::Options::command();
    let visible: Vec<clap::Arg> = full
        .get_arguments()
        .filter(|arg| !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version"))
        .cloned()
        .collect();
    let mut command = clap::Command::new("ssj")
        .version(env!("CARGO_PKG_VERSION"))
        .args(visible);
    if let Some(about) = full.get_about() {
        command = command.about(about.clone());
    }
    if let Some(usage) = full.get_overridden_usage() {
        command = command.override_usage(usage.clone());
    }
    if let Some(help) = full.get_after_help() {
        command = command.after_help(help.clone());
    }
    let mut page = Vec::new();
    clap_mangen::Man::new(command.clone())
        .render(&mut page)
        .expect("render the man page");
    page.extend_from_slice(FILES.as_bytes());
    fs::write(out.join("ssj.1"), page).expect("write the man page");
    for (shell, file) in [
        (Shell::Bash, "ssj.bash"),
        (Shell::Zsh, "_ssj"),
        (Shell::Fish, "ssj.fish"),
    ] {
        let mut text = Vec::new();
        clap_complete::generate(shell, &mut command, "ssj", &mut text);
        let mut text = String::from_utf8(text).expect("the completions are text");
        text.push_str(dynamic(shell));
        if shell == Shell::Zsh {
            // the names of the sessions for `--name`
            text = text.replace(
                "--name=[Join the session of this name, or start it]:NAME:_default",
                "--name=[Join the session of this name, or start it]:NAME:_ssj_sessions",
            );
            text = format!(
                "_ssj_sessions() {{ local -a s; s=(${{(f)\"$(ssj --complete 2>/dev/null)\"}}); compadd -a s; }}\n{text}"
            );
        }
        fs::write(out.join(file), text).expect("write the completion");
    }
}

/// Hooks that let `attach`, `kill`, and `--name` offer the session names of `ssj --complete`.
fn dynamic(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => {
            r#"
_ssj_sessions() {
    local previous=${COMP_WORDS[COMP_CWORD-1]}
    case $previous in
        attach|kill|--name)
            COMPREPLY=($(compgen -W "$(ssj --complete 2>/dev/null)" -- "${COMP_WORDS[COMP_CWORD]}"))
            [[ ${#COMPREPLY[@]} -gt 0 ]] && return
            ;;
    esac
    _ssj "$@"
}
complete -F _ssj_sessions -o bashdefault -o default ssj
"#
        }
        Shell::Fish => {
            r#"
complete -c ssj -n '__fish_seen_subcommand_from attach kill' -f -a '(ssj --complete 2>/dev/null)'
complete -c ssj -l name -x -a '(ssj --complete 2>/dev/null)'
"#
        }
        _ => "",
    }
}
