//! Binding of the UDP socket in the configured port range.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::ops::RangeInclusive;
use std::sync::Arc;

use socket2::{Domain, Protocol, Socket, Type};

use crate::daemon::random_bytes;

fn bind_socket(port: u16) -> io::Result<UdpSocket> {
    // dual stack, so that one socket serves clients on IPv4 and IPv6
    let v6 = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).and_then(|socket| {
        socket.set_only_v6(false)?;
        socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
        Ok(socket)
    });
    match v6 {
        Ok(socket) => Ok(socket.into()),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => Err(err),
        Err(_) => UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)),
    }
}

/// Binds the first free port of the range, trying random ports before scanning in order.
///
/// The range is shared with all other users of the host, so a busy port is normal.
pub fn bind_in_range(range: &RangeInclusive<u16>) -> io::Result<UdpSocket> {
    let (low, high) = (u32::from(*range.start()), u32::from(*range.end()));
    let span = high - low + 1;
    for _ in 0..64 {
        let mut bytes = [0u8; 4];
        random_bytes(&mut bytes)?;
        let port = low + u32::from_le_bytes(bytes) % span;
        match bind_socket(port as u16) {
            Ok(socket) => return Ok(socket),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => continue,
            Err(err) => return Err(err),
        }
    }
    for port in low..=high {
        match bind_socket(port as u16) {
            Ok(socket) => return Ok(socket),
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        format!(
            "no free UDP port between {} and {} (port_range = \"0\" takes any port)",
            range.start(),
            range.end()
        ),
    ))
}

/// Server endpoint on a socket that was already bound. Must run inside the tokio runtime.
pub fn endpoint_on(socket: UdpSocket, config: quinn::ServerConfig) -> io::Result<quinn::Endpoint> {
    quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(config),
        socket,
        Arc::new(quinn::TokioRuntime),
    )
}
