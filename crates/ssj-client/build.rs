//! Writes the man page and the shell completions to `$OUT_DIR`, from where the release workflow
//! picks them up.

use std::{env, fs, path::PathBuf};

use clap::CommandFactory;
use clap_complete::Shell;

#[path = "src/options.rs"]
mod options;

/// The words that are commands when they stand first, and not hosts.
const COMMANDS: &str = "list attach kill detach install edit connect";

/// The commands that keep their options after the destination: `connect` is the plain connection,
/// whose words after the destination are for the command on the host.
fn option_commands() -> String {
    COMMANDS
        .split(' ')
        .filter(|word| *word != "connect")
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the options cannot show: the config files and the cache.
const COMMAND: &str = r#".SH RUNNING A COMMAND
\fBssj\fR \fIDESTINATION\fR \fICOMMAND\fR... runs the command in the session instead of the shell,
as \fBssh\fR does, and ends with its exit code. The login shell of the host runs it, so pipes,
\fB&&\fR, and \fBcd\fR work. The session survives a lost connection like any other, and can be joined
with \fBssj attach\fR.
.PP
As with \fBssh\fR, everything after the destination belongs to the command, options included:
\fBssj host ls \-la\fR runs \fBls \-la\fR on the host. Options of \fBssj\fR go before the destination
(\fBssj \-\-local\-echo auto host make \-j4\fR). A \fB\-\-\fR right after the destination is optional.
.PP
The commands of \fBssj\fR (\fBlist\fR, \fBattach\fR, \fBkill\fR, \fBdetach\fR, \fBinstall\fR, \fBedit\fR)
keep their options after the destination: \fBssj list host \-\-json\fR, and take only the options
listed under COMMANDS. A host with the name of one of them is written \fBssj connect list\fR, or
\fBssj attach \-\- ls\fR.
"#;

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
\fBcongestion\fR = "auto" | "bbr" | "cubic"
How fast to send. Auto (default) starts with Cubic, which is fastest on a clean link, and moves to BBR when packets are lost without a queue (Wi-Fi, mobile, long paths), where Cubic crawls.
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
\fBcongestion\fR = "auto" | "bbr" | "cubic"
How fast the server sends output (default auto).
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
    // where the tests and the release workflow find the files.
    println!("cargo:rustc-env=SSJ_ASSETS={}", out.display());
    // the generators list hidden options too, so they are left out here.
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
    // the completions offer the real flag names, the man page shows a pair on one line.
    let mut command = build(options::Options::command());
    let mut page = Vec::new();
    clap_mangen::Man::new(options::command().version(env!("CARGO_PKG_VERSION")))
        .render(&mut page)
        .expect("render the man page");
    page.extend_from_slice(COMMAND.as_bytes());
    // the options that take a value: the word after one of them is not a destination
    let value_options = command
        .get_arguments()
        .filter(|arg| arg.get_action().takes_values())
        .flat_map(|arg| {
            let long = arg.get_long().map(|long| format!("--{long}"));
            let short = arg.get_short().map(|short| format!("-{short}"));
            long.into_iter().chain(short)
        })
        .collect::<Vec<_>>()
        .join(" ");
    // the long options that each command reads, in the order of the table
    let command_longs: Vec<(&str, Vec<String>)> = options::COMMAND_OPTIONS
        .iter()
        .map(|(word, ids)| {
            let longs = ids
                .iter()
                .map(|id| {
                    command
                        .get_arguments()
                        .find(|arg| arg.get_id() == id)
                        .and_then(|arg| arg.get_long())
                        .map(str::to_owned)
                        .unwrap_or_else(|| panic!("COMMAND_OPTIONS names {id}, which is no option"))
                })
                .collect();
            (*word, longs)
        })
        .collect();
    let dashed = |longs: &[String]| {
        longs
            .iter()
            .map(|long| format!("--{long}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    // the commands with their options in the man page
    let mut commands_page = String::from(
        ".SH COMMANDS\nThe options that each command reads. Every option may stand before the command word, and these also after the destination.\n",
    );
    for (word, longs) in &command_longs {
        let list = longs
            .iter()
            .map(|long| format!("\\fB\\-\\-{}\\fR", long.replace('-', "\\-")))
            .collect::<Vec<_>>()
            .join(", ");
        commands_page.push_str(&format!(".TP\n\\fBssj {word}\\fR\n{list}\n"));
    }
    page.extend_from_slice(commands_page.as_bytes());
    page.extend_from_slice(FILES.as_bytes());
    fs::write(out.join("ssj.1"), page).expect("write the man page");
    // the options before the destination of `ssj connect`: all of them but those of the commands
    let all_options = dashed(
        &command
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(str::to_owned))
            .collect::<Vec<_>>(),
    );
    // what completes the value of each option, for the words after the destination of a command
    let zsh_values = command
        .get_arguments()
        .filter(|arg| arg.get_action().takes_values())
        .filter_map(|arg| {
            let long = arg.get_long()?;
            let possible: Vec<String> = arg
                .get_possible_values()
                .iter()
                .map(|value| value.get_name().to_owned())
                .collect();
            let action = if long == "name" {
                "_ssj_sessions".to_owned()
            } else if !possible.is_empty() {
                format!("compadd -- {}", possible.join(" "))
            } else {
                match arg.get_value_hint() {
                    clap::ValueHint::FilePath | clap::ValueHint::AnyPath => "_files".to_owned(),
                    clap::ValueHint::Hostname => "_ssj_hosts".to_owned(),
                    _ => return None,
                }
            };
            Some(format!("        --{long}) {action}; return 0 ;;"))
        })
        .collect::<Vec<_>>()
        .join("\n");
    // the arms of the `case` of Bash and of the `switch` of Fish
    let bash_cases = command_longs
        .iter()
        .map(|(word, longs)| format!("            {word}) opts=\"{}\" ;;", dashed(longs)))
        .collect::<Vec<_>>()
        .join("\n");
    let fish_cases = command_longs
        .iter()
        .map(|(word, longs)| {
            format!(
                "        case {word}\n            contains -- $argv[1] {}",
                longs.join(" ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    for (shell, file) in [
        (Shell::Bash, "ssj.bash"),
        (Shell::Zsh, "_ssj"),
        (Shell::Fish, "ssj.fish"),
    ] {
        let mut text = Vec::new();
        clap_complete::generate(shell, &mut command, "ssj", &mut text);
        let mut text = String::from_utf8(text).expect("the completions are text");
        if shell == Shell::Fish {
            // the options belong to the command words, and to the start
            text = text
                .lines()
                .map(|line| match line.strip_prefix("complete -c ssj ") {
                    Some(rest) => {
                        let name = format!(" {rest}")
                            .split(" -l ")
                            .nth(1)
                            .and_then(|tail| tail.split(' ').next().map(str::to_owned))
                            .expect("every option has a long name");
                        format!("complete -c ssj -n '__ssj_option_allowed {name}' {rest}")
                    }
                    None => line.to_owned(),
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n";
        }
        text.push_str(
            &dynamic(shell)
                .replace("__VALUE_OPTIONS__", &value_options)
                .replace("__COMMANDS__", COMMANDS)
                .replace("__COMMAND_PATTERN__", &option_commands().replace(' ', "|"))
                .replace("__BASH_OPTION_CASES__", &bash_cases)
                .replace("__FISH_OPTION_CASES__", &fish_cases),
        );
        if shell == Shell::Zsh {
            // the hosts of the ssh config for the hosts of options
            text = text.replace(":HOST:_hosts'", ":HOST:_ssj_hosts'");
            // the names of the sessions for `--name`.
            text = text.replace(
                "--name=[Join the session of this name, or start it]:NAME:_default",
                "--name=[Join the session of this name, or start it]:NAME:_ssj_sessions",
            );
            // the words after the options: a command or a host, then a host, then (for `attach` and
            // `kill`) the sessions of that host.
            text = text.replace(
                "*::words -- Command, session id or name, and destination:_default",
                "*::words -- Command, destination, and session id or name:_ssj_words",
            );
            let helpers = r#"_ssj_sessions() { local -a s; s=(${(f)"$(ssj --complete 2>/dev/null)"}); compadd -a s; }
# the aliases of `~/.ssh/config` (`Host merk-c1-mac5`) are known to `_ssh_hosts`, not to `_hosts`
_ssj_hosts() { if (( $+functions[_ssh_hosts] )); then _ssh_hosts; else _hosts; fi; }
# the value of the option before the word being completed, which stands after the destination
_ssj_option_value() {
    case $words[CURRENT-1] in
__VALUE_CASES__
        *) return 1 ;;
    esac
}
_ssj_words() {
    if (( CURRENT == 1 )); then
        _alternative 'commands:command:compadd __COMMANDS__' 'hosts:host:_ssj_hosts'
        return
    fi
    # `ssj connect [OPTIONS] HOST COMMAND...`: the options and the host, and then the command for it
    if [[ $words[1] == connect ]]; then
        _ssj_option_value && return
        local -a pos; local skip= w
        for w in ${words[2,CURRENT-1]}; do
            if [[ -n $skip ]]; then skip=
            elif [[ $w == -* ]]; then [[ " __VALUE_OPTIONS__ " == *" $w "* ]] && skip=1
            else pos+=($w); fi
        done
        if (( $#pos > 0 )); then _files
        elif [[ $PREFIX == -* ]]; then compadd -- __ALL_OPTIONS__
        else _ssj_hosts; fi
        return
    fi
    # `ssj attach -- ls`: the host is called `attach`, and the rest is the command for it
    if [[ $words[1] == (__COMMAND_PATTERN__) ]] && (( CURRENT >= 3 )) && [[ $words[2] == -- ]]; then
        _files
        return
    fi
    if [[ $words[1] == (__COMMAND_PATTERN__) ]]; then
        # the options of a command may stand after the destination (`ssj install host --server-binary`)
        local -A opts=(__OPTION_TABLE__)
        # the value of an option that the command reads
        [[ " ${opts[$words[1]]} " == *" $words[CURRENT-1] "* ]] && _ssj_option_value && return
        if [[ $PREFIX == -* ]]; then compadd -- ${=opts[$words[1]]}; return; fi
        if (( CURRENT == 2 )); then
            _ssj_hosts
        elif [[ ( $CURRENT -eq 3 && $words[1] == attach ) || ( $CURRENT -ge 3 && $words[1] == (kill|detach) ) ]]; then
            # `detach` only acts on a session that has a client
            local -a s extra; [[ $words[1] == detach ]] && extra=(--connected)
            s=(${(f)"$(ssj --complete $words[2] $extra 2>/dev/null)"}); compadd -a s
        fi
    else
        # any other first word is the destination, and the rest belongs to the command for the
        # host, dashes included (`ssj host ls -l`): files, since the layout of the host is mostly
        # the same (`~/.local/share/...`)
        _files
    fi
}
"#;
            let table = command_longs
                .iter()
                .map(|(word, longs)| format!("{word} '{}'", dashed(longs)))
                .collect::<Vec<_>>()
                .join(" ");
            let helpers = helpers
                .replace("__OPTION_TABLE__", &table)
                .replace("__ALL_OPTIONS__", &all_options)
                .replace("__VALUE_CASES__", &zsh_values)
                .replace("__VALUE_OPTIONS__", &value_options)
                .replace("__COMMANDS__", COMMANDS)
                .replace("__COMMAND_PATTERN__", &option_commands().replace(' ', "|"));
            // `#compdef` must stay the first line, or the completion is not found.
            text = match text.split_once('\n') {
                Some((first, rest)) => format!("{first}\n{helpers}{rest}"),
                None => format!("{helpers}{text}"),
            };
        }
        fs::write(out.join(file), text).expect("write the completion");
    }
}

/// Hooks that let `attach`, `kill`, `detach`, and `--name` offer the session names of
/// `ssj --complete`: for the id after the host of `attach`, `kill`, and `detach` those of that
/// host.
fn dynamic(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash => {
            r#"
_ssj_sessions() {
    local current=${COMP_WORDS[COMP_CWORD]}
    local previous=${COMP_WORDS[COMP_CWORD-1]}
    local value_options=" __VALUE_OPTIONS__ "
    # `--option=value`: bash splits the word at the `=`, and the value is a word of its own
    if [[ $current == = ]]; then
        current=
        # bash 3 does not pass the word to the function and reads it from here
        COMP_WORDS[COMP_CWORD]=
    elif [[ $previous == = ]]; then
        previous=${COMP_WORDS[COMP_CWORD-2]}
    fi
    # the words before this one that are neither options nor the values of options
    local -a pos=()
    local word skip= i
    for ((i = 1; i < COMP_CWORD; i++)); do
        word=${COMP_WORDS[i]}
        if [[ -n $skip ]]; then
            [[ $word == = ]] || skip=
        elif [[ $word == -- ]]; then
            pos+=("$word")
        elif [[ $word == -* ]]; then
            [[ $value_options == *" $word "* ]] && skip=1
        else
            pos+=("$word")
        fi
    done
    COMPREPLY=()
    # `ssj connect [OPTIONS] HOST COMMAND...`: the plain connection, for the host and its command
    local connect=
    if [[ ${pos[0]-} == connect ]]; then
        connect=1
        pos=("${pos[@]:1}")
    fi
    # the options that the command reads
    local opts=
    case ${pos[0]-} in
__BASH_OPTION_CASES__
    esac
    # the value of an option: the names of the sessions for `--name`, else what the options know
    case ${pos[0]-} in
        ''|__COMMAND_PATTERN__)
            if [[ $value_options == *" $previous "* && ( -z ${pos[0]-} || " $opts " == *" $previous "* ) ]]; then
                if [[ $previous == --name ]]; then
                    COMPREPLY=($(compgen -W "$(ssj --complete 2>/dev/null)" -- "$current"))
                elif [[ $previous == --udp-host ]] && declare -F _known_hosts_real >/dev/null; then
                    _known_hosts_real -- "$current"
                else
                    _ssj "$1" "$current" "$previous"
                fi
                return
            fi
            ;;
    esac
    # the options stand before the destination, or after a command word: the words after any other
    # destination are for the command on the host (`ssj host ls -l`)
    if [[ $current == -* ]]; then
        # after the destination of `connect` the dashes are for the host
        [[ -z $connect || ${#pos[@]} -eq 0 ]] || return
        [[ -z $connect ]] || { _ssj "$1" "$current" "$previous"; return; }
        if [[ -z ${pos[0]-} ]]; then
            _ssj "$1" "$current" "$previous"
            return
        fi
        COMPREPLY=($(compgen -W "$opts" -- "$current"))
        return
    fi
    local hosts=
    case ${#pos[@]} in
        0)
            [[ -n $connect ]] || COMPREPLY=($(compgen -W "__COMMANDS__" -- "$current"))
            hosts=1
            ;;
        *)
            # the words after the destination of `connect` are for the host
            [[ -z $connect ]] || return
            case ${pos[0]} in
                __COMMAND_PATTERN__)
                    if [[ ${#pos[@]} -eq 1 ]]; then
                        hosts=1
                    elif [[ ${pos[1]} == -- ]]; then
                        # `ssj attach -- ls`: the rest is the command for the host
                        return
                    else
                        # `kill` and `detach` take any number of sessions, `attach` one
                        local only=
                        case ${pos[0]} in
                            attach) [[ ${#pos[@]} -eq 2 ]] || only=none ;;
                            kill) ;;
                            # `detach` only acts on a session that has a client
                            detach) only=--connected ;;
                            *) only=none ;;
                        esac
                        if [[ $only != none ]]; then
                            COMPREPLY=($(compgen -W "$(ssj --complete "${pos[1]}" $only 2>/dev/null)" -- "$current"))
                        fi
                        # no file names where nothing is to be said
                        [[ ${#COMPREPLY[@]} -gt 0 ]] || compopt +o default 2>/dev/null
                    fi
                    ;;
                # any other first word is the destination, and the rest is files
                *) return ;;
            esac
            ;;
    esac
    # the aliases of the ssh config are known to `_known_hosts_real` of bash-completion, as for ssh
    if [[ -n $hosts ]] && declare -F _known_hosts_real >/dev/null; then
        local others=("${COMPREPLY[@]}")
        _known_hosts_real -- "$current"
        COMPREPLY=("${others[@]}" "${COMPREPLY[@]}")
    fi
}
complete -F _ssj_sessions -o bashdefault -o default ssj
"#
        }
        Shell::Fish => {
            r#"
# the words that are neither options nor the values of options, without the one being typed
function __ssj_raw_words
    set -l tokens (commandline -opc)
    set -e tokens[1]
    set -l skip 0
    for word in $tokens
        if test $skip = 1
            set skip 0
            continue
        end
        if test "$word" = --
            printf '%s\n' $word
            continue
        end
        switch $word
            case __VALUE_OPTIONS__
                set skip 1
            case '-*'
            case '*'
                printf '%s\n' $word
        end
    end
end
# `ssj connect [OPTIONS] HOST COMMAND...` is the plain connection: the word is left out, and the
# destination is the first word then
function __ssj_connect
    set -l words (__ssj_raw_words)
    test (count $words) -ge 1; and test "$words[1]" = connect
end
function __ssj_words
    set -l words (__ssj_raw_words)
    if test (count $words) -ge 1; and test "$words[1]" = connect
        set -e words[1]
    end
    if test (count $words) -gt 0
        printf '%s\n' $words
    end
end
# the options stand anywhere before the destination, and after a command word only those that the
# command reads: the words after any other destination are for the command on the host
# (`ssj host ls -l`)
function __ssj_option_allowed
    set -l words (__ssj_words)
    if test (count $words) -eq 0
        return 0
    end
    if __ssj_connect
        return 1
    end
    switch $words[1]
__FISH_OPTION_CASES__
        case '*'
            return 1
    end
end
function __ssj_wants_host
    set -l words (__ssj_words)
    not __ssj_connect; and test (count $words) -eq 1; and contains -- $words[1] __COMMANDS__
end
function __ssj_wants_sessions
    set -l words (__ssj_words)
    not __ssj_connect; or return 1
    test (count $words) -ge 2; and test $words[2] != --; or return 1
    switch $words[1]
        case attach
            test (count $words) -eq 2
        case kill detach
            true
        case '*'
            false
    end
end
function __ssj_sessions_of_host
    set -l words (__ssj_words)
    if test $words[1] = detach
        ssj --complete $words[2] --connected 2>/dev/null
    else
        ssj --complete $words[2] 2>/dev/null
    end
end
complete -c ssj -n '__ssj_wants_sessions' -f -a '(__ssj_sessions_of_host)'
complete -c ssj -n 'test (count (__ssj_words)) -eq 0; and not __ssj_connect' -f -a '__COMMANDS__'
complete -c ssj -n 'test (count (__ssj_words)) -eq 0' -f -a '(__fish_print_hostnames)'
complete -c ssj -n '__ssj_wants_host' -f -a '(__fish_print_hostnames)'
complete -c ssj -n '__ssj_option_allowed name' -l name -x -a '(ssj --complete 2>/dev/null)'
"#
        }
        _ => "",
    }
}
