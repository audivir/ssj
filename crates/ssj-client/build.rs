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
\fBdetach_key\fR = "none" | "ctrl-]"
A key that detaches from the session wherever it is typed. Off by default, since the key is not
sent to the program.
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
\fBforward_agent\fR = true | false
Let programs on the host use the ssh agent of this machine (\fB\-A\fR). Off by default.
.TP
\fBcongestion\fR = "bbr" | "cubic"
How fast to send. BBR (default) keeps its speed on a link with random loss; Cubic is a little faster on a clean gigabit link.
.TP
\fBcompress\fR = "off" | "auto"
Compress input and output while the link is the limit. Off by default, since compressing text of an attacker with a secret can leak the secret.
.TP
\fBlocal_echo\fR = "off" | "auto"
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
\fBcompress\fR = "off" | "auto"
Compress output while the link is the limit (default off).
.TP
\fBcongestion\fR = "bbr" | "cubic"
How fast the server sends output (default bbr).
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
    let build = |full: clap::Command| {
        let visible: Vec<clap::Arg> = full
            .get_arguments()
            .filter(|arg| {
                !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version")
            })
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
        command
    };
    // the completions offer the real flag names, the man page shows a pair on one line
    let mut command = build(options::Options::command());
    let mut page = Vec::new();
    clap_mangen::Man::new(options::command().version(env!("CARGO_PKG_VERSION")))
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
            // the words after the options: a command or a host, then a host, then (for `attach` and
            // `kill`) the sessions of that host
            text = text.replace(
                "*::words -- Command, session id or name, and destination:_default",
                "*::words -- Command, destination, and session id or name:_ssj_words",
            );
            let helpers = r#"_ssj_sessions() { local -a s; s=(${(f)"$(ssj --complete 2>/dev/null)"}); compadd -a s; }
_ssj_words() {
    if (( CURRENT == 1 )); then
        _alternative 'commands:command:compadd list attach kill detach install' 'hosts:host:_hosts'
    elif [[ $words[1] == (list|attach|kill|detach|install) ]]; then
        if (( CURRENT == 2 )); then
            _hosts
        elif (( CURRENT == 3 )) && [[ $words[1] == (attach|kill|detach) ]]; then
            local -a s; s=(${(f)"$(ssj --complete $words[2] 2>/dev/null)"}); compadd -a s
        fi
    else
        _hosts
    fi
}
"#;
            // `#compdef` must stay the first line, or the completion is not found
            text = match text.split_once('\n') {
                Some((first, rest)) => format!("{first}\n{helpers}{rest}"),
                None => format!("{helpers}{text}"),
            };
        }
        fs::write(out.join(file), text).expect("write the completion");
    }
}

/// Hooks that let `attach`, `kill`, `detach`, and `--name` offer the session names of `ssj --complete`: for
/// the id after the host of `attach`, `kill`, and `detach` those of that host.
fn dynamic(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => {
            r#"
_ssj_sessions() {
    local current=${COMP_WORDS[COMP_CWORD]}
    local previous=${COMP_WORDS[COMP_CWORD-1]}
    # ssj attach|kill|detach HOST <name>: the sessions of that host
    case ${COMP_WORDS[1]} in
        attach|kill|detach)
            if [[ $COMP_CWORD -eq 3 ]]; then
                COMPREPLY=($(compgen -W "$(ssj --complete "${COMP_WORDS[2]}" 2>/dev/null)" -- "$current"))
                [[ ${#COMPREPLY[@]} -gt 0 ]] && return
            fi
            ;;
    esac
    case $previous in
        --name)
            COMPREPLY=($(compgen -W "$(ssj --complete 2>/dev/null)" -- "$current"))
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
complete -c ssj -n '__fish_seen_subcommand_from attach kill detach; and test (count (commandline -opc | string match -v -- "-*")) -eq 3' -f -a '(ssj --complete (commandline -opc | string match -v -- "-*")[3] 2>/dev/null)'
complete -c ssj -l name -x -a '(ssj --complete 2>/dev/null)'
"#
        }
        _ => "",
    }
}
