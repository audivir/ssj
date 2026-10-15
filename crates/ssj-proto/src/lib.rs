//! Wire format, replay buffer, and resume logic of `ssj`, without any IO.
//!
//! Everything here works on byte slices and plain values, so the parsers can be fuzzed and the
//! state logic tested without sockets.

pub mod env;
pub mod frame;
pub mod replay;
pub mod resume;
pub mod types;

pub use env::{Env, EnvError};
pub use frame::{
    ClientFrame, DecodeError, EncodeError, ErrorCode, ExitStatus, Hello, ServerFrame, Welcome,
    decode_client, decode_server,
};
pub use replay::{ReplayBuffer, ReplayError};
pub use types::{SessionId, WindowSize};
