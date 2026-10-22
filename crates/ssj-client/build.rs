//! Writes the man page and the shell completions to `$OUT_DIR`, from where the release workflow
//! picks them up.

use std::{env, fs, path::PathBuf};

use clap::CommandFactory;
use clap_complete::Shell;

#[path = "src/options.rs"]
mod options;

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
All config files are flat \fIkey = value\fR lines of TOML, without tables. A typo in a key is an
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
    let command = build(options::Options::command());
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
    // the options with a free text: no fixed values, no file or host, and not the session name
    let free_options: Vec<String> = command
        .get_arguments()
        .filter(|arg| arg.get_action().takes_values())
        .filter(|arg| arg.get_possible_values().is_empty())
        .filter(|arg| {
            !matches!(
                arg.get_value_hint(),
                clap::ValueHint::FilePath | clap::ValueHint::AnyPath | clap::ValueHint::Hostname
            )
        })
        .filter_map(|arg| arg.get_long())
        .filter(|long| *long != "name")
        .map(str::to_owned)
        .collect();
    // the commands with their options in the man page
    let mut page_commands = String::from(
        ".SH COMMANDS\nThe options that each command reads. They may stand before the command word or after the destination; any other option is an error.\n",
    );
    for (word, ids) in options::COMMAND_OPTIONS {
        let list = ids
            .iter()
            .map(|id| {
                let long = long_of(&command, id);
                format!("\\fB\\-\\-{}\\fR", long.replace('-', "\\-"))
            })
            .collect::<Vec<_>>()
            .join(", ");
        page_commands.push_str(&format!(".TP\n\\fBssj {word}\\fR\n{list}\n"));
    }
    page.extend_from_slice(page_commands.as_bytes());
    page.extend_from_slice(FILES.as_bytes());
    fs::write(out.join("ssj.1"), page).expect("write the man page");

    let tree = completion_tree(&command);
    for (shell, file) in [
        (Shell::Bash, "ssj.bash"),
        (Shell::Zsh, "_ssj"),
        (Shell::Fish, "ssj.fish"),
    ] {
        let mut text = Vec::new();
        clap_complete::generate(shell, &mut tree.clone(), "ssj", &mut text);
        let text = String::from_utf8(text).expect("the completions are text");
        let text = match shell {
            Shell::Bash => bash(&text, &value_options, &free_options),
            Shell::Zsh => zsh(&text, &value_options, &free_options),
            Shell::Fish => fish(&text, &value_options, &free_options),
            _ => text,
        };
        fs::write(out.join(file), text).expect("write the completion");
    }
}

/// The long name of the option with this id.
fn long_of(command: &clap::Command, id: &str) -> String {
    command
        .get_arguments()
        .find(|arg| arg.get_id() == id)
        .and_then(|arg| arg.get_long())
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("COMMAND_OPTIONS names {id}, which is no option"))
}

/// What the commands do, for the completion of the first word.
const ABOUT: [(&str, &str); 7] = [
    ("list", "List the sessions of a host"),
    ("attach", "Join a running session"),
    ("kill", "End sessions"),
    ("detach", "Make the clients of sessions leave"),
    ("install", "Put the server file on a host"),
    ("edit", "Edit client.toml, or server.toml on a host"),
    ("connect", "Connect to a host named like a command"),
];

/// The command with a subcommand for each command word, which only the generators see. The
/// parser has one flat list of options (`options.rs`), and the generators write the options of a
/// command, their values, and the hints for files and hosts from the subcommands. What the hosts
/// have and the sessions of a host stay with the functions that are added in the shells.
fn completion_tree(command: &clap::Command) -> clap::Command {
    // the options only: the words after them belong to the commands below
    let options: Vec<clap::Arg> = command
        .get_arguments()
        .filter(|arg| arg.get_long().is_some() || arg.get_short().is_some())
        .cloned()
        .collect();
    let mut tree = clap::Command::new("ssj")
        .version(env!("CARGO_PKG_VERSION"))
        .disable_help_subcommand(true)
        .args(options.clone());
    let destination = || {
        clap::Arg::new("destination")
            .value_name("DESTINATION")
            .value_hint(clap::ValueHint::Hostname)
    };
    for (word, about) in ABOUT {
        let mut sub = clap::Command::new(word)
            .about(about)
            .disable_help_flag(true)
            .disable_version_flag(true)
            // only `ssj edit` can do without
            .arg(destination().required(word != "edit"));
        if word == "connect" {
            // the plain connection reads the options of the connection, and the words after the
            // destination are for the command on the host
            sub = sub.args(options.iter().cloned()).arg(
                clap::Arg::new("remote")
                    .value_name("COMMAND")
                    .num_args(0..)
                    .trailing_var_arg(true)
                    .value_hint(clap::ValueHint::AnyPath),
            );
        } else {
            let ids = options::COMMAND_OPTIONS
                .iter()
                .find(|(name, _)| *name == word)
                .map_or(&[][..], |(_, ids)| *ids);
            for id in ids {
                let arg = command
                    .get_arguments()
                    .find(|arg| arg.get_id() == id)
                    .unwrap_or_else(|| panic!("COMMAND_OPTIONS names {id}, which is no option"));
                sub = sub.arg(arg.clone());
            }
            // the ids of sessions follow the destination: one for `attach`, any for the others
            match word {
                "attach" => {
                    sub = sub.arg(clap::Arg::new("id").value_name("ID"));
                }
                "kill" | "detach" => {
                    sub = sub.arg(clap::Arg::new("id").value_name("ID").num_args(0..));
                }
                _ => {}
            }
            // nothing is left to say after them, which `_arguments` of zsh would answer with the
            // options even before a dash is typed: a rest that completes nothing keeps them back
            if word != "kill" && word != "detach" {
                sub = sub.arg(clap::Arg::new("rest").value_name("NOTHING").num_args(0..));
            }
        }
        tree = tree.subcommand(sub);
    }
    tree
}

/// The functions that the zsh file adds to what `clap` writes.
const ZSH_HELPERS: &str = r#"
# the hosts of ~/.ssh/config are known to `_ssh_hosts`, not to `_hosts`
_ssj_hosts() { if (( $+functions[_ssh_hosts] )); then _ssh_hosts; else _hosts; fi; }
# a word that nothing completes
_ssj_nothing() { return 1; }
_ssj_sessions() { local -a s; s=(${(f)"$(ssj --complete 2>/dev/null)"}); compadd -a s; }
# the sessions of the host that was typed: the first positional word of the command is it
_ssj_session_ids() {
    local -a s extra
    # the value of an option that the command does not read is not an id
    [[ " __VALUE_OPTIONS__ " == *" $words[CURRENT-1] "* ]] && return 1
    # `detach` only acts on a session that has a client; the context names the command
    [[ $curcontext == *:ssj-command-detach:* ]] && extra=(--connected)
    s=(${(f)"$(ssj --complete $line[1] $extra 2>/dev/null)"}); compadd -a s
}
"#;

/// The zsh file: the functions of `clap` with the hosts of the ssh config, the sessions, and the
/// files for the words after a destination that is no command.
fn zsh(text: &str, value_options: &str, free_options: &[String]) -> String {
    let mut text = text
        .replace(":_hosts'", ":_ssj_hosts'")
        .replace(":id:_default'", ":id:_ssj_session_ids'")
        // one colon: the options may still stand among the ids
        .replace("'*::id:", "'*:id:")
        // one colon: the options may still stand among the words that this rest takes
        .replace("'*::rest:_default'", "'*:rest:_ssj_nothing'")
        .replace(
            "--name=[Join the session of this name, or start it]:NAME:_default",
            "--name=[Join the session of this name, or start it]:NAME:_ssj_sessions",
        )
        // the first word is a command or a host
        .replace(
            "_describe -t commands 'ssj commands' commands \"$@\"",
            "_describe -t commands 'ssj commands' commands \"$@\"; _ssj_hosts",
        );
    // `ssj attach -- ls`: the host is called `attach`, and the rest is the command for it
    let line = "    local context curcontext=\"$curcontext\" state line\n";
    assert!(text.contains(line), "the declaration of the state");
    let commands = ABOUT
        .map(|(word, _)| word)
        .into_iter()
        .filter(|word| *word != "connect")
        .collect::<Vec<_>>()
        .join("|");
    text = text.replacen(
        line,
        &format!(
            "{line}    if (( CURRENT >= 4 )) && [[ $words[2] == ({commands}) && $words[3] == -- ]]; then\n        _files\n        return\n    fi\n"
        ),
        1,
    );
    // any other first word is the destination, and the rest belongs to the command for the host,
    // dashes included (`ssj host ls -l`): files, since the layout of the host is mostly the same
    // (`~/.local/share/...`)
    let end = "        esac\n    ;;\nesac";
    assert!(text.contains(end), "the end of the case of the commands");
    text = text.replacen(
        end,
        // the status is that of the completion: with several matchers (the `matcher-list` of
        // oh-my-zsh has three) a function that ends with 1 is run again for the next matcher,
        // and the lists of all of them are shown
        "            (*)\n[[ -n $line[1] ]] && _files && ret=0\n;;\n        esac\n    ;;\nesac\n    return ret",
        1,
    );
    // `#compdef` must stay the first line, or the completion is not found.
    // an option with a free text completes nothing, where the default would offer files
    let text = text
        .lines()
        .map(|line| {
            let free = free_options
                .iter()
                .any(|long| line.contains(&format!("--{long}=[")));
            if free {
                line.replace(":_default'", ":_ssj_nothing'")
            } else {
                line.to_owned()
            }
        })
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    let helpers = ZSH_HELPERS.replace("__VALUE_OPTIONS__", value_options);
    match text.split_once('\n') {
        Some((first, rest)) => format!("{first}\n{helpers}{rest}"),
        None => format!("{helpers}{text}"),
    }
}

/// The bash file: the function of `clap` and, in front of it, the one that is registered. It
/// finds the words that are no options, and offers the hosts and the sessions to them, and leaves
/// the options and their values to `clap`.
fn bash(text: &str, value_options: &str, free_options: &[String]) -> String {
    let commands = ABOUT.map(|(word, _)| word).join(" ");
    let free = free_options
        .iter()
        .map(|long| format!("--{long}"))
        .collect::<Vec<_>>()
        .join(" ");
    let wrapper = BASH_WRAPPER
        .replace("__FREE_OPTIONS__", &free)
        .replace("__VALUE_OPTIONS__", value_options)
        .replace("__COMMANDS__", &commands);
    format!("{text}{wrapper}")
}

/// The function that bash calls for `ssj`.
const BASH_WRAPPER: &str = r#"
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
    # the options and their values, for the start and for the words after a command word, are
    # for `clap`; the words after any other destination are for the command on the host
    local first=${pos[0]-} is_command=
    case " __COMMANDS__ " in *" $first "*) [[ -z $first ]] || is_command=1 ;; esac
    # after the destination of `connect` the words are for the host as well
    [[ $first == connect && ${#pos[@]} -gt 1 ]] && is_command=
    if [[ -z $first || -n $is_command ]]; then
        if [[ $value_options == *" $previous "* ]]; then
            # a free text has nothing to offer, and no files
            if [[ " __FREE_OPTIONS__ " == *" $previous "* ]]; then
                compopt +o default +o bashdefault 2>/dev/null
                COMPREPLY=()
                return
            fi
            # an option that the command does not read has no values: ask `clap` for its options
            if [[ -n $first ]]; then
                local saved=${COMP_WORDS[COMP_CWORD]}
                COMP_WORDS[COMP_CWORD]=-
                _ssj "$1" - ""
                COMP_WORDS[COMP_CWORD]=$saved
                local known=" ${COMPREPLY[*]} "
                COMPREPLY=()
                [[ $known == *" $previous "* ]] || return
            fi
            if [[ $previous == --name ]]; then
                COMPREPLY=($(compgen -W "$(ssj --complete 2>/dev/null)" -- "$current"))
            elif [[ $previous == --udp-host ]] && declare -F _known_hosts_real >/dev/null; then
                _known_hosts_real -- "$current"
            else
                _ssj "$1" "$current" "$previous"
            fi
            return
        fi
        if [[ $current == -* ]]; then
            _ssj "$1" "$current" "$previous"
            return
        fi
    elif [[ $current == -* ]]; then
        return
    fi
    local hosts=
    if [[ -z $first ]]; then
        COMPREPLY=($(compgen -W "__COMMANDS__" -- "$current"))
        hosts=1
    elif [[ -n $is_command ]]; then
        if [[ ${#pos[@]} -eq 1 ]]; then
            hosts=1
        elif [[ ${pos[1]} == -- ]]; then
            # `ssj attach -- ls`: the rest is the command for the host
            return
        else
            # `kill` and `detach` take any number of sessions, `attach` one
            local only=
            case $first in
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
    else
        # any other first word is the destination, and the rest is files
        return
    fi
    # the aliases of the ssh config are known to `_known_hosts_real` of bash-completion, as for ssh
    if [[ -n $hosts ]] && declare -F _known_hosts_real >/dev/null; then
        local others=("${COMPREPLY[@]}")
        _known_hosts_real -- "$current"
        COMPREPLY=("${others[@]}" "${COMPREPLY[@]}")
    fi
}
complete -F _ssj_sessions -o bashdefault -o default ssj
"#;

/// The fish file: the rules of `clap`, which find the command word with `argparse`, and the rules
/// for the hosts and the sessions. The options of `connect` stand before its destination only.
fn fish(text: &str, value_options: &str, free_options: &[String]) -> String {
    let commands = ABOUT.map(|(word, _)| word).join(" ");
    let text = text.replace(
        "__fish_ssj_using_subcommand connect\"",
        "__fish_ssj_using_subcommand connect; and test (count (__ssj_words)) -le 1\"",
    );
    // `argparse` of `clap` fails while the value of an option is missing, so before the command
    // word the rules count the words themselves: those of the options also while a value is
    // typed, the others (the commands) not
    let text = text
        .lines()
        .map(
            |line| match line.strip_prefix("complete -c ssj -n \"__fish_ssj_needs_command\"") {
                Some(rest) if rest.contains(" -l ") || rest.contains(" -s ") => {
                    format!("complete -c ssj -n \"__ssj_no_command\"{rest}")
                }
                Some(rest) => format!("complete -c ssj -n \"__ssj_at_start\"{rest}"),
                None => line.to_owned(),
            },
        )
        .collect::<Vec<_>>()
        .join("\n");
    // the options with a free text complete nothing, files included
    let text = text
        .lines()
        .map(|line| {
            let free = free_options
                .iter()
                .any(|long| line.contains(&format!(" -l {long} ")) && line.ends_with(" -r"));
            if free {
                format!("{line} -f\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect::<String>();
    // the commands that keep their options after the destination, with the words after it
    // for the parser, not for the host
    let option_commands = ABOUT
        .map(|(word, _)| word)
        .into_iter()
        .filter(|word| *word != "connect")
        .collect::<Vec<_>>()
        .join(" ");
    let rules = FISH_RULES
        .replace("__OPTION_COMMANDS__", &option_commands)
        .replace("__VALUE_OPTIONS__", value_options)
        .replace("__COMMANDS__", &commands);
    format!("{text}{rules}")
}

/// What the fish file adds.
const FISH_RULES: &str = r#"
# the words that are neither options nor the values of options, without the one being typed
function __ssj_words
    set -l tokens (commandline -opc)
    set -e tokens[1]
    set -l skip 0
    for word in $tokens
        if test $skip = 1
            set skip 0
            continue
        end
        if test "$word" = --
            printf '%s
' $word
            continue
        end
        switch $word
            case __VALUE_OPTIONS__
                set skip 1
            case '-*'
            case '*'
                printf '%s
' $word
        end
    end
end
function __ssj_no_command
    test (count (__ssj_words)) -eq 0
end
# no command yet, and no value of an option is being typed
function __ssj_at_start
    __ssj_no_command; or return 1
    set -l tokens (commandline -opc)
    not contains -- $tokens[-1] __VALUE_OPTIONS__
end
# the word before the one being typed is an option that takes a value
function __ssj_value_pending
    set -l tokens (commandline -opc)
    contains -- $tokens[-1] __VALUE_OPTIONS__
end
function __ssj_wants_host
    set -l words (__ssj_words)
    not __ssj_value_pending; and test (count $words) -eq 1; and contains -- $words[1] __COMMANDS__
end
# the option before the word being typed is not one that the command reads: fish is asked whether
# the command completes the name of the option
function __ssj_foreign_value
    __ssj_value_pending; or return 1
    set -l tokens (commandline -opc)
    set -l offered (complete -C (string join ' ' -- $tokens[1..-2] $tokens[-1]) | string replace -r '\t.*' '')
    not contains -- $tokens[-1] $offered
end
# in a command of ssj no file is wanted, but for the values of options and after `--`
function __ssj_no_files
    set -l words (__ssj_words)
    test (count $words) -ge 1; and contains -- $words[1] __OPTION_COMMANDS__; and test "$words[2]" != --; or return 1
    not __ssj_value_pending; or __ssj_foreign_value
end
function __ssj_wants_sessions
    set -l words (__ssj_words)
    not __ssj_value_pending; or return 1
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
complete -c ssj -n '__ssj_no_files' -f
complete -c ssj -n '__ssj_wants_sessions' -f -a '(__ssj_sessions_of_host)'
complete -c ssj -n '__ssj_at_start' -f -a '(__fish_print_hostnames)'
complete -c ssj -n '__ssj_wants_host' -f -a '(__fish_print_hostnames)'
complete -c ssj -n '__ssj_no_command' -l name -x -a '(ssj --complete 2>/dev/null)'
"#;
