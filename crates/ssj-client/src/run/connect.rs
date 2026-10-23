//! The start of a connection: the server on the host, the environment, and the transports.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use ssj_net::Identity;
use ssj_proto::bootstrap::{CHOICE_EXIT_CODE, Connect, Prestart};
use ssj_proto::control::{SessionInfo, SessionState};
use ssj_proto::{Env, EnvFilter};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use super::client::run_session;
use super::sessions::{Picked, pick_session, session_infos};
use super::{
    ATTEMPT_TIMEOUT, Announce, FIRST_ATTEMPT_TIMEOUT, Finished, Io, Params, STREAM_DEAD_AFTER,
    Tier, hangup_signals, notice, say,
};
use crate::bootstrap::{Remote, Spawned, StartError, run_remote, spawn_server, start_server};
use crate::cli::{Args, LocalEcho, Transport};
use crate::config::Config;
use crate::install::{Decision, confirm, decide, install, interactive};
use crate::status::{LineStatus, Status, TitleStatus};
use crate::terminal::{RawMode, window_size};
use crate::tunnel::{Held, Kind, SshConfig, with_held};
use crate::vlog;

/// The agent socket to forward, if the user asked for it.
fn agent_socket(args: &Args) -> Result<Option<PathBuf>, String> {
    if !args.forward_agent {
        return Ok(None);
    }
    match std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from) {
        Some(path) if path.is_absolute() => Ok(Some(path)),
        _ => Err("agent forwarding needs SSH_AUTH_SOCK, and there is no agent running".to_owned()),
    }
}

/// Selected variables of the local environment, see [`send_patterns`].
fn local_env(args: &Args, ssh: Option<&SshConfig>) -> Result<Env, String> {
    // a real answer has the host name; without one there are no patterns of ssh, which is not the
    // same as an answer without `SendEnv` lines.
    let from_ssh = ssh
        .filter(|config| config.hostname.is_some())
        .map(|config| config.send_env.clone());
    vlog!(2, "SendEnv of ssh: {from_ssh:?}");
    let filter = send_patterns(&args.send_env, &args.send_env_add, from_ssh)
        .map_err(|e| format!("--send-env: {e:?}"))?;
    let vars = std::env::vars_os()
        .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)));
    selected_env(vars, &filter)
}

/// The patterns of the variables to send: what `ssh` sends for the destination (`from_ssh`), or
/// `TERM`, `LANG`, and `LC_*` when `ssh` gave no answer, then the `send_env` of the config file and
/// the `--send-env` patterns on top.
fn send_patterns(
    config: &EnvFilter,
    added: &[String],
    from_ssh: Option<Vec<String>>,
) -> Result<EnvFilter, ssj_proto::PatternError> {
    let own = config.patterns().iter().chain(added).cloned();
    let fallback = EnvFilter::default_patterns();
    match from_ssh {
        // too many patterns with those of ssh: the own ones are the user's choice.
        Some(patterns) => EnvFilter::new(patterns)
            .and_then(|filter| filter.extended(own.clone()))
            .or_else(|_| fallback.extended(own)),
        None => fallback.extended(own),
    }
}

/// The variables that `filter` selects, and always `TERM`: the shell needs the terminal type
/// whatever else the user sends, as with `ssh`, which sends it with the request for the terminal.
/// Without it the shell takes the terminal for a dumb one and draws its prompt wrongly.
fn selected_env(
    vars: impl IntoIterator<Item = (String, String)>,
    filter: &EnvFilter,
) -> Result<Env, String> {
    let vars = vars
        .into_iter()
        .filter(|(name, _)| name == "TERM" || filter.matches(name))
        .collect();
    Env::new(vars).map_err(|e| format!("the selected environment is not valid: {e:?}"))
}

async fn resolve(host: &str, port: u16) -> Result<SocketAddr, String> {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("cannot resolve {host}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host} has no address"))
}

/// The arguments of the first start of the server, and whether `--reuse` is among them.
fn first_arguments(args: &Args, extra: &[String]) -> (Vec<String>, bool) {
    // with several sessions to join and a person at the terminal, the server hands the choice back.
    let mut extra = extra.to_vec();
    // `--reuse` may start a session, and a bare `--attach` cannot name one.
    let reuse = extra.iter().any(|word| word == "--reuse");
    let choosing = (reuse || extra == ["--attach"]) && interactive() && !args.batch;
    if choosing {
        extra.push("--choose".to_owned());
    }
    (extra, reuse)
}

/// Starts the server. If it is missing, installs it when the policy and the user agree. `spawned`
/// is the `ssh` of the first start, which was begun before its input was known.
async fn start_or_install(
    args: &Args,
    config: &Config,
    fingerprint: &[u8; 32],
    extra: &[String],
    prestart: Option<&Prestart>,
    spawned: Option<Spawned>,
) -> Result<(Connect, Option<Held>), String> {
    vlog!(
        1,
        "starting the server on {} through {}",
        args.destination,
        args.ssh_command[0]
    );
    let (extra, reuse) = first_arguments(args, extra);
    let extra = &extra[..];
    match start_server(args, fingerprint, extra, prestart, spawned).await {
        Ok(connect) => Ok(connect),
        Err(StartError::Failed(message)) => Err(message),
        Err(StartError::Choose(sessions)) => {
            choose_and_start(args, fingerprint, prestart, sessions, reuse).await
        }
        // only an explicit `ssj attach` has to find a session; `--name` and `--auto-attach` start
        // one when there is none, and so may install the server.
        Err(StartError::Missing) if extra.first().is_some_and(|word| word == "--attach") => {
            Err(format!(
                "ssj-server is not installed on {}, so no session runs there",
                args.destination
            ))
        }
        Err(StartError::Missing) => {
            vlog!(1, "the server is not installed on the host");
            let policy = args.auto_install.unwrap_or(config.auto_install);
            match decide(policy, &args.destination, interactive(), confirm) {
                Decision::Skip(message) => Err(message),
                Decision::Install => {
                    let path = install(args).await?;
                    eprintln!("ssj: installed {path}");
                    match start_server(args, fingerprint, extra, prestart, None).await {
                        Ok(connect) => Ok(connect),
                        Err(StartError::Missing) => {
                            Err("the server is still missing after the installation".to_owned())
                        }
                        Err(StartError::Failed(message)) => Err(message),
                        Err(StartError::Choose(_)) => {
                            Err("the host has no session to choose from".to_owned())
                        }
                    }
                }
            }
        }
    }
}

/// Asks which of the sessions to join, or whether to start another, and starts the server for
/// that answer.
async fn choose_and_start(
    args: &Args,
    fingerprint: &[u8; 32],
    prestart: Option<&Prestart>,
    sessions: Vec<SessionInfo>,
    allow_new: bool,
) -> Result<(Connect, Option<Held>), String> {
    let title = format!(
        "ssj: {} sessions on {} {}. Which one?",
        sessions.len(),
        args.destination,
        if allow_new {
            "have no client"
        } else {
            "are running"
        }
    );
    let extra = match pick_session(title, sessions, allow_new).await {
        Picked::Cancelled => return Err("cancelled".to_owned()),
        Picked::Session(id) => {
            vlog!(1, "joining the session {id}");
            vec!["--attach".to_owned(), id]
        }
        Picked::New => {
            vlog!(1, "starting a new session");
            Vec::new()
        }
    };
    match start_server(args, fingerprint, &extra, prestart, None).await {
        Ok(connect) => Ok(connect),
        Err(StartError::Failed(message)) => Err(message),
        Err(StartError::Missing | StartError::Choose(_)) => {
            Err("the session is gone; run the command again".to_owned())
        }
    }
}

/// Runs `--kill` or `--detach` on the host. Without an id, and with a person at the terminal,
/// several sessions are offered in a menu.
pub(super) async fn run_choosing(
    args: &Args,
    flag: &str,
    id: Option<String>,
    question: &str,
) -> Result<Remote, String> {
    let mut extra = vec![flag.to_owned()];
    extra.extend(id.clone());
    // a person is asked when there is no id, and when the id or name fits several sessions
    let choosing = interactive() && !args.batch;
    if choosing {
        extra.push("--choose".to_owned());
    }
    let mut remote = run_remote(args, &extra, "").await?;
    if choosing && remote.status.code() == Some(i32::from(CHOICE_EXIT_CODE)) {
        let mut sessions = session_infos(&remote.stdout);
        if flag == "--detach" {
            // only a session with a client can be detached.
            sessions.retain(|s| s.state == SessionState::Connected);
        }
        let id = match sessions.len() {
            0 => return Err("no client is connected".to_owned()),
            1 => sessions[0].id.as_str().to_owned(),
            n => {
                let title = format!("ssj: {n} sessions on {}. {question}", args.destination);
                match pick_session(title, sessions, false).await {
                    Picked::Session(id) => id,
                    _ => return Err("cancelled".to_owned()),
                }
            }
        };
        remote = run_remote(args, &[flag.to_owned(), id], "").await?;
    }
    Ok(remote)
}

/// Looks up the address that UDP will use, while the `ssh` of the bootstrap is still busy, so that
/// the start does not wait for it afterwards.
async fn udp_address(args: &Args, config: Option<&SshConfig>) -> Option<IpAddr> {
    if !matches!(args.transport, Transport::Auto | Transport::Udp) {
        return None;
    }
    let host = udp_host(args, config);
    let address = resolve(&host, 0).await.ok().map(|addr| addr.ip());
    vlog!(2, "the address of {host} for UDP: {address:?}");
    address
}

/// The address of the server: the one given, else the `HostName` of the `ssh` configuration (the
/// destination may be an alias), else the destination itself.
fn udp_host(args: &Args, config: Option<&SshConfig>) -> String {
    args.udp_host
        .clone()
        .or_else(|| config.and_then(|c| c.hostname.clone()))
        .unwrap_or_else(|| args.host().to_owned())
}

async fn build_tiers(
    args: &Args,
    connect: &Connect,
    config: Option<&SshConfig>,
    address: Option<IpAddr>,
) -> Result<Vec<Tier>, String> {
    let choice = args.transport;
    // UDP does not travel through a jump host, so it is not even tried there.
    let udp_possible = matches!(choice, Transport::Auto | Transport::Udp) && connect.udp.is_some();
    let config = config.filter(|_| udp_possible);
    let jump = choice == Transport::Auto && config.is_some_and(|c| c.jump);
    let order = crate::tunnel::tier_order(choice, jump, connect.udp.is_some());
    let mut tiers = Vec::new();
    let mut notes = Vec::new();
    for kind in order {
        match kind {
            Kind::Udp => {
                let port = connect.udp.expect("offered");
                // the address of the server: the one given, else the `HostName` of the `ssh`
                // configuration (the destination may be an alias), else the destination itself.
                let host = udp_host(args, config);
                let found = match address {
                    Some(ip) => Ok(SocketAddr::new(ip, port)),
                    None => resolve(&host, port).await,
                };
                match found {
                    Ok(addr) => tiers.push(Tier::Udp {
                        addr,
                        resolver: Some(Box::new(move || {
                            let host = host.clone();
                            Box::pin(async move { resolve(&host, port).await })
                        })),
                    }),
                    Err(message) => notes.push(message),
                }
            }
            Kind::Stdio => tiers.push(crate::tunnel::stdio_tier(
                args,
                &connect.id,
                ATTEMPT_TIMEOUT,
            )),
        }
    }
    if tiers.is_empty() {
        match choice {
            Transport::Udp if connect.udp.is_none() => {
                notes.push("the server offers no UDP port".to_owned())
            }
            _ => {}
        }
        return Err(notes.join("; "));
    }
    Ok(tiers)
}

/// Lets the first stream transport run over the `ssh` that started the server, if the server
/// still holds it.
fn with_first_stream_held(mut tiers: Vec<Tier>, held: Option<Held>) -> Vec<Tier> {
    let Some(held) = held else {
        return tiers;
    };
    if let Some(at) = tiers.iter().position(|t| matches!(t, Tier::Stream { .. })) {
        let tier = tiers.remove(at);
        tiers.insert(at, with_held(tier, held, ATTEMPT_TIMEOUT));
    }
    tiers
}

/// With `fresh` the server may start the shell of a new session at once (see `Prestart`).
pub(super) async fn connect_session(
    args: Args,
    config: &Config,
    extra: &[String],
    attach: bool,
    fresh: bool,
) -> u8 {
    let identity = Identity::generate();
    let prepared = async {
        let fingerprint = identity.fingerprint();
        // the login takes longer than `ssh -G`, and the server reads its input only after it, so
        // the login starts first and the input follows.
        let spawned = spawn_server(&args, &first_arguments(&args, extra).0).ok();
        let ssh = crate::tunnel::ssh_config(&args).await;
        vlog!(
            2,
            "ssh -G: HostName {:?}, jump host or proxy: {}",
            ssh.as_ref().and_then(|c| c.hostname.as_deref()),
            ssh.as_ref().is_some_and(|c| c.jump)
        );
        let env = local_env(&args, ssh.as_ref())?;
        let agent = agent_socket(&args)?;
        let prestart = fresh.then(|| Prestart {
            size: window_size(),
            env: env.clone(),
            agent: agent.is_some(),
        });
        let (connect, address) = tokio::join!(
            start_or_install(
                &args,
                config,
                &fingerprint,
                extra,
                prestart.as_ref(),
                spawned
            ),
            udp_address(&args, ssh.as_ref())
        );
        let (connect, held) = connect?;
        let new = connect.new;
        vlog!(
            1,
            "the server runs: session {}, udp {:?}",
            connect.id.as_str(),
            connect.udp
        );
        let tiers = build_tiers(&args, &connect, ssh.as_ref(), address).await?;
        let tiers = with_first_stream_held(tiers, held);
        vlog!(
            1,
            "transports in order: {}",
            tiers.iter().map(Tier::name).collect::<Vec<_>>().join(", ")
        );
        Ok::<_, String>((connect, new, env, tiers, agent))
    }
    .await;
    let (connect, new_session, env, tiers, agent) = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            eprintln!("ssj: {message}");
            return 1;
        }
    };

    // the server says the name: it is not only the one the user typed (`ssj attach host` joins a
    // session whose name the user may not remember).
    let session_name = connect.name.as_ref().map(|name| name.as_str().to_owned());
    let raw = match RawMode::enable() {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("ssj: cannot set up the terminal: {err}");
            return 1;
        }
    };
    let status: Box<dyn Status> = if raw.is_some() {
        Box::new(TitleStatus::default())
    } else {
        Box::new(LineStatus)
    };
    let (size_tx, resizes) = mpsc::unbounded_channel();
    if let Ok(mut winch) = signal(SignalKind::window_change()) {
        tokio::spawn(async move {
            while winch.recv().await.is_some() {
                if size_tx.send(window_size()).is_err() {
                    break;
                }
            }
        });
    }
    let io = Io {
        input: Box::new(tokio::io::stdin()),
        output: Box::new(tokio::io::stdout()),
        size: window_size(),
        resizes,
        status,
        hangups: hangup_signals(),
        net: {
            let watch = crate::netwatch::start();
            vlog!(2, "network change sources: {:?}", watch.active);
            watch.events
        },
    };
    let result = run_session(
        Params {
            identity,
            connect,
            tiers,
            attempt_timeout: ATTEMPT_TIMEOUT,
            first_attempt_timeout: FIRST_ATTEMPT_TIMEOUT,
            stream_dead_after: STREAM_DEAD_AFTER,
            env,
            detach_key: args.detach_key,
            hangup_detaches: std::env::var_os("SSH_TTY").is_some(),
            attach,
            local_echo: raw.is_some() && args.local_echo == LocalEcho::Auto,
            compress: args.compress == Some(ssj_net::Compress::Auto),
            agent,
            announce: (raw.is_some() && !args.quiet).then(|| Announce {
                destination: args.destination.clone(),
                started: new_session,
                name: session_name,
            }),
        },
        io,
    )
    .await;
    drop(raw);

    match result {
        Ok(Finished::Exited(code)) => {
            notice(&args, &format!("[disconnected from {}]", args.destination));
            code
        }
        Ok(Finished::HungUp) => 129,
        Ok(Finished::Detached) => {
            let port = args.ssh_port.map(|p| format!(":{p}")).unwrap_or_default();
            say(
                &args,
                &format!(
                    "[detached from session; it keeps running: ssj attach {}{port}]",
                    args.destination
                ),
            );
            0
        }
        Err(message) => {
            eprintln!("ssj: {message}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vec<(String, String)> {
        [
            ("TERM", "xterm-ghostty"),
            ("LC_THEME", "dark"),
            ("HOME", "/h"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec()
    }

    fn names(filter: Vec<String>) -> Vec<String> {
        let env = selected_env(vars(), &EnvFilter::new(filter).unwrap()).unwrap();
        env.iter().map(|(name, _)| name.to_owned()).collect()
    }

    #[test]
    fn term_is_sent_even_when_the_user_names_other_variables() {
        assert_eq!(names(vec!["LC_THEME".to_owned()]), ["TERM", "LC_THEME"]);
        assert_eq!(names(vec![]), ["TERM"]);
    }

    fn texts(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn what_ssh_sends_is_sent_and_without_an_answer_the_default_is() {
        let none = EnvFilter::none();
        let ssh = send_patterns(&none, &texts(&["A"]), Some(texts(&["LC_THEME"]))).unwrap();
        assert!(ssh.matches("LC_THEME") && ssh.matches("A"));
        // ssh sends no locale here, and neither does ssj.
        assert!(!ssh.matches("LANG") && !ssh.matches("LC_ALL"));
        let nothing = send_patterns(&none, &[], Some(Vec::new())).unwrap();
        assert!(!nothing.matches("LANG"));
        let fallback = send_patterns(&none, &texts(&["A"]), None).unwrap();
        assert!(fallback.matches("LANG") && fallback.matches("LC_ALL") && fallback.matches("A"));
    }

    #[test]
    fn the_config_file_adds_to_ssh() {
        let config = EnvFilter::new(texts(&["EDITOR"])).unwrap();
        let both = send_patterns(&config, &texts(&["A"]), Some(texts(&["LANG"]))).unwrap();
        assert!(both.matches("EDITOR") && both.matches("A") && both.matches("LANG"));
    }

    #[test]
    fn the_default_selection_does_not_send_a_variable_twice() {
        let env = selected_env(vars(), &EnvFilter::default_patterns()).unwrap();
        assert_eq!(env.len(), 2);
    }
}
