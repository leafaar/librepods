//! Opening the L2CAP SeqPacket channels that AACP and ATT run over.

use {
    bluer::{
        Address, AddressType,
        l2cap::{SeqPacket, Socket, SocketAddr},
    },
    std::io,
    tokio::time::{Duration, Instant, sleep, timeout},
    tracing::{error, info},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Why an L2CAP channel to a device could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("creating the L2CAP socket failed")]
    Socket(#[source] io::Error),
    #[error("L2CAP connect to PSM {psm:#06x} failed")]
    Connect {
        psm: u16,
        #[source]
        source: io::Error,
    },
    #[error("L2CAP connect to PSM {psm:#06x} timed out")]
    ConnectTimedOut { psm: u16 },
    #[error("the device disconnected during L2CAP connection setup")]
    PeerDisconnected(#[source] io::Error),
    #[error("timed out waiting for the L2CAP connection to be fully established")]
    SetupTimedOut,
}

/// Connect to `psm` on `addr` over BR/EDR and wait until the channel has a
/// peer CID, so the first send does not race the connection setup.
pub(crate) async fn connect_seq_packet(addr: Address, psm: u16) -> Result<SeqPacket, ConnectError> {
    let target = SocketAddr::new(addr, AddressType::BrEdr, psm);
    let socket = Socket::new_seq_packet().map_err(|e| {
        error!("Failed to create L2CAP socket: {e}");
        ConnectError::Socket(e)
    })?;
    let seq_packet = match timeout(CONNECT_TIMEOUT, socket.connect(target)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            error!("L2CAP connect failed: {e}");
            return Err(ConnectError::Connect { psm, source: e });
        },
        Err(_) => {
            error!("L2CAP connect timed out");
            return Err(ConnectError::ConnectTimedOut { psm });
        },
    };

    let start = Instant::now();
    loop {
        match seq_packet.peer_addr() {
            Ok(peer) if peer.cid != 0 => break,
            Ok(_) => {},
            Err(e) if e.kind() == io::ErrorKind::NotConnected => {
                error!("Peer has disconnected during connection setup.");
                return Err(ConnectError::PeerDisconnected(e));
            },
            Err(e) => error!("Error getting peer address: {e}"),
        }
        if start.elapsed() >= CONNECT_TIMEOUT {
            error!("Timed out waiting for L2CAP connection to be fully established.");
            return Err(ConnectError::SetupTimedOut);
        }
        sleep(POLL_INTERVAL).await;
    }
    info!("L2CAP connection established with {addr}");
    Ok(seq_packet)
}
