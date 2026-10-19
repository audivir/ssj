//! Decisions at the start of a connection, as pure functions.

use crate::env::Env;
use crate::frame::{ErrorCode, Hello, Welcome};
use crate::replay::ReplayBuffer;
use crate::types::WindowSize;

/// What the server has to do besides answering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Spawn {
        size: WindowSize,
        env: Env,
        /// The shell gets a socket for the agent of the client.
        agent: bool,
    },
    Resize(WindowSize),
}

/// State of the server that the answer to `Hello` depends on.
pub struct ServerView<'a> {
    pub shell_started: bool,
    pub output: &'a ReplayBuffer,
    /// Input bytes consumed so far.
    pub input_received: u64,
}

/// Decides how the server answers a `Hello`.
pub fn negotiate(hello: Hello, view: &ServerView<'_>) -> Result<(Welcome, Action), ErrorCode> {
    match hello {
        Hello::Start { size, env, agent } => {
            if view.shell_started {
                return Err(ErrorCode::AlreadyStarted);
            }
            let welcome = Welcome {
                input_offset: 0,
                output_offset: 0,
            };
            Ok((welcome, Action::Spawn { size, env, agent }))
        }
        Hello::Resume {
            size,
            output_offset,
        } => {
            if !view.shell_started {
                return Err(ErrorCode::NotStarted);
            }
            // a client cannot have seen bytes that were never produced
            if output_offset > view.output.end() {
                return Err(ErrorCode::ReplayGap);
            }
            // Output that was forgotten while nobody was there to acknowledge it is skipped: the
            // welcome starts at the oldest byte that is left, and the client works out from the
            // difference how many bytes it missed.
            let welcome = Welcome {
                input_offset: view.input_received,
                output_offset: output_offset.max(view.output.start()),
            };
            Ok((welcome, Action::Resize(size)))
        }
        Hello::Attach { size } => {
            if !view.shell_started {
                return Err(ErrorCode::NotStarted);
            }
            // a client without history gets everything the server still holds
            let welcome = Welcome {
                input_offset: view.input_received,
                output_offset: character_boundary(view.output),
            };
            Ok((welcome, Action::Resize(size)))
        }
    }
}

/// The oldest retained offset, moved forward past the rest of a UTF-8 character that the start of
/// the buffer cuts in two.
fn character_boundary(output: &ReplayBuffer) -> u64 {
    let start = output.start();
    let head = output.read_from(start, 3).unwrap_or_default();
    let cut = head.iter().take_while(|byte| *byte & 0xc0 == 0x80).count();
    start + cut as u64
}

/// Reason a `Welcome` does not fit what the client sent and knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadWelcome;

/// Checks the `Welcome` of the server against the client state.
///
/// `input_acked` and `input_sent` are the acknowledged and the written input offsets. A welcome
/// that claims input the client never sent, or that takes back acknowledged input, is refused. For
/// `Resume` the output offset may move forward, never back: the bytes in between were forgotten
/// by the server.
pub fn check_welcome(
    hello: &Hello,
    welcome: Welcome,
    input_acked: u64,
    input_sent: u64,
) -> Result<(), BadWelcome> {
    let ok = match hello {
        Hello::Start { .. } => welcome.input_offset == 0 && welcome.output_offset == 0,
        Hello::Resume { output_offset, .. } => {
            welcome.output_offset >= *output_offset
                && (input_acked..=input_sent).contains(&welcome.input_offset)
        }
        // a client without history adopts the offsets of the server
        Hello::Attach { .. } => true,
    };
    if ok { Ok(()) } else { Err(BadWelcome) }
}
