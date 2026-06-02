use anyhow::Context;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};

/// Ask the OS for an unused port by binding to port 0.
/// There is a brief TOCTOU window between returning the port and the caller
/// binding to it, which is acceptable for local dev / CI use.
pub fn pick_unused_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .context("bind port 0 to pick unused port")?;
    Ok(listener.local_addr()?.port())
}
