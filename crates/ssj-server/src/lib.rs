//! Server side of `ssj`: owns the shell of a session and talks to the client.

pub mod backend;
pub mod cli;
pub mod daemon;
pub mod launch;
pub mod listen;
pub mod session;
pub mod transport;
