//! The QUIC endpoint of the session, which opens and closes while the session keeps running.

use std::io;
use std::ops::RangeInclusive;
use std::time::Duration;

use ssj_net::{Identity, SERVER_SEND_WINDOW, quic_server_with_send_window, server_tls};
use ssj_proto::SessionId;
use ssj_proto::bootstrap::Connect;
use tokio::sync::mpsc;

use crate::listen::{bind_in_range, endpoint_on};
use crate::session::Connection;
use crate::transport::serve;

/// Manages the current endpoint: at most one, bound to a fresh port with fresh certificates.
pub struct Frontend {
    id: SessionId,
    udp: bool,
    range: RangeInclusive<u16>,
    conns: mpsc::Sender<Connection>,
    stream_timeout: Duration,
    send_window: u64,
    current: Option<quinn::Endpoint>,
}

impl Frontend {
    pub fn new(
        id: SessionId,
        range: RangeInclusive<u16>,
        conns: mpsc::Sender<Connection>,
        stream_timeout: Duration,
    ) -> Self {
        Self {
            id,
            udp: true,
            range,
            conns,
            stream_timeout,
            send_window: SERVER_SEND_WINDOW,
            current: None,
        }
    }

    /// Turns UDP on or off. With it off, only `--stdio` reaches the session.
    pub fn with_udp(mut self, udp: bool) -> Self {
        self.udp = udp;
        self
    }

    /// How much output QUIC may hold unacknowledged (see `quic_server_with_send_window`).
    pub fn with_send_window(mut self, bytes: u64) -> Self {
        self.send_window = bytes;
        self
    }

    /// Replaces the endpoint by a new one that accepts only the client with fingerprint `pin`.
    ///
    /// The old endpoint is closed first, which also ends its connections. Its port stays busy
    /// until QUIC has drained them, so the new endpoint waits for that, and retries a busy port
    /// for a short time. Must run inside the tokio runtime.
    pub async fn open(&mut self, pin: [u8; 32]) -> io::Result<Connect> {
        if let Some(old) = self.current.take() {
            old.close(0u8.into(), b"replaced");
            let _ = tokio::time::timeout(Duration::from_secs(3), old.wait_idle()).await;
        }
        let identity = Identity::generate();
        let mut udp = None;
        if self.udp {
            let mut tries = 0;
            let socket = loop {
                match bind_in_range(&self.range) {
                    Ok(socket) => break socket,
                    Err(err) if err.kind() == io::ErrorKind::AddrInUse && tries < 20 => {
                        tries += 1;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(err) => return Err(err),
                }
            };
            let endpoint = endpoint_on(
                socket,
                quic_server_with_send_window(server_tls(&identity, pin), self.send_window),
            )?;
            udp = Some(endpoint.local_addr()?.port());
            tokio::spawn(serve(
                endpoint.clone(),
                self.conns.clone(),
                self.stream_timeout,
            ));
            self.current = Some(endpoint);
        }
        Ok(Connect {
            id: self.id.clone(),
            udp,
            fingerprint: identity.fingerprint(),
            new: false,
            // the caller adds what it knows: the control socket knows the name of the session
            // and its path.
            name: None,
            socket: None,
        })
    }

    /// The channel to the session, for streams that do not come from a listener.
    pub fn conns(&self) -> mpsc::Sender<Connection> {
        self.conns.clone()
    }

    /// Like [`Frontend::close`], and gives the close message time to reach the peers before the
    /// process ends. Without it the client of a finished session waits for an answer that never
    /// comes.
    pub async fn close_and_flush(&mut self) {
        if let Some(endpoint) = self.current.take() {
            endpoint.close(0u8.into(), b"closed");
            let _ = tokio::time::timeout(Duration::from_secs(1), endpoint.wait_idle()).await;
        }
    }

    /// Stops listening. Nobody can connect until the next [`Frontend::open`].
    pub fn close(&mut self) {
        if let Some(endpoint) = self.current.take() {
            endpoint.close(0u8.into(), b"closed");
        }
    }
}
