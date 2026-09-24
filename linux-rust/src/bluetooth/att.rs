use bluer::l2cap::{SeqPacket, Socket, SocketAddr};
use bluer::{Address, AddressType, Error, Result};
use hex;
use tracing::{debug, error, info};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, sleep};

const PSM_ATT: u16 = 0x001F;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

const OPCODE_ERROR_RESPONSE: u8 = 0x01;
const OPCODE_READ_REQUEST: u8 = 0x0A;
const OPCODE_READ_RESPONSE: u8 = 0x0B;
const OPCODE_WRITE_REQUEST: u8 = 0x12;
const OPCODE_WRITE_RESPONSE: u8 = 0x13;
const OPCODE_HANDLE_VALUE_NTF: u8 = 0x1B;
const OPCODE_HANDLE_VALUE_IND: u8 = 0x1D;
const OPCODE_HANDLE_VALUE_CFM: u8 = 0x1E;
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ATTHandles {
    AirPodsTransparency = 0x18,
    AirPodsLoudSoundReduction = 0x1B,
    AirPodsHearingAid = 0x2A,
    NothingEverything = 0x8002,
    NothingEverythingRead = 0x8005, // for some reason, and not the same as the write handle
}

#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ATTCCCDHandles {
    Transparency = ATTHandles::AirPodsTransparency as u16 + 1,
    LoudSoundReduction = ATTHandles::AirPodsLoudSoundReduction as u16 + 1,
    HearingAid = ATTHandles::AirPodsHearingAid as u16 + 1,
}

impl From<ATTHandles> for ATTCCCDHandles {
    fn from(handle: ATTHandles) -> Self {
        match handle {
            ATTHandles::AirPodsTransparency => ATTCCCDHandles::Transparency,
            ATTHandles::AirPodsLoudSoundReduction => ATTCCCDHandles::LoudSoundReduction,
            ATTHandles::AirPodsHearingAid => ATTCCCDHandles::HearingAid,
            ATTHandles::NothingEverything => panic!("No CCCD for NothingEverything handle"), // we don't request it
            ATTHandles::NothingEverythingRead => panic!("No CCD for NothingEverythingRead handle"), // it sends notifications without CCCD
        }
    }
}

struct ATTManagerState {
    sender: Option<mpsc::Sender<Vec<u8>>>,
    listeners: HashMap<u16, Vec<mpsc::UnboundedSender<Vec<u8>>>>,
}

impl ATTManagerState {
    fn new() -> Self {
        ATTManagerState {
            sender: None,
            listeners: HashMap::new(),
        }
    }
}

/// What a PDU received while a request is outstanding means for that request.
#[derive(Debug, PartialEq, Eq)]
enum ResponseMatch<'a> {
    /// The response to the request, without its opcode.
    Response(&'a [u8]),
    /// An Error Response for the request, with the ATT error code.
    Error(u8),
    /// Anything else: a late response to an earlier request that timed out,
    /// or a PDU of a type this client does not handle.
    Unrelated,
}

fn match_response(request_opcode: u8, pdu: &[u8]) -> ResponseMatch<'_> {
    let expected = match request_opcode {
        OPCODE_READ_REQUEST => OPCODE_READ_RESPONSE,
        OPCODE_WRITE_REQUEST => OPCODE_WRITE_RESPONSE,
        _ => return ResponseMatch::Unrelated,
    };
    match pdu {
        [opcode, value @ ..] if *opcode == expected => ResponseMatch::Response(value),
        // Error Response: opcode, request opcode in error, handle (2), error code.
        [OPCODE_ERROR_RESPONSE, failed, _, _, code, ..] if *failed == request_opcode => {
            ResponseMatch::Error(*code)
        }
        _ => ResponseMatch::Unrelated,
    }
}

#[derive(Clone)]
pub struct ATTManager {
    state: Arc<Mutex<ATTManagerState>>,
    /// Received responses. Held for the whole of a request, from send to
    /// response, so only one request is outstanding at a time as ATT requires
    /// and a response cannot be taken by another request.
    response_rx: Arc<Mutex<mpsc::UnboundedReceiver<Vec<u8>>>>,
    response_tx: mpsc::UnboundedSender<Vec<u8>>,
    tasks: Arc<Mutex<JoinSet<()>>>,
}

impl ATTManager {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        ATTManager {
            state: Arc::new(Mutex::new(ATTManagerState::new())),
            response_rx: Arc::new(Mutex::new(rx)),
            response_tx: tx,
            tasks: Arc::new(Mutex::new(JoinSet::new())),
        }
    }

    pub async fn connect(&mut self, addr: Address) -> Result<()> {
        info!(
            "ATTManager connecting to {} on PSM {:#06X}...",
            addr, PSM_ATT
        );
        let target_sa = SocketAddr::new(addr, AddressType::BrEdr, PSM_ATT);

        let socket = Socket::new_seq_packet()?;
        let seq_packet_result =
            tokio::time::timeout(CONNECT_TIMEOUT, socket.connect(target_sa)).await;
        let seq_packet = match seq_packet_result {
            Ok(Ok(s)) => Arc::new(s),
            Ok(Err(e)) => {
                error!("L2CAP connect failed: {}", e);
                return Err(e.into());
            }
            Err(_) => {
                error!("L2CAP connect timed out");
                return Err(Error::from(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Connection timeout",
                )));
            }
        };

        // Wait for connection to be fully established
        let start = Instant::now();
        loop {
            match seq_packet.peer_addr() {
                Ok(peer) if peer.cid != 0 => break,
                Ok(_) => {}
                Err(e) => {
                    if e.raw_os_error() == Some(107) {
                        // ENOTCONN
                        error!("Peer has disconnected during connection setup.");
                        return Err(e.into());
                    }
                    error!("Error getting peer address: {}", e);
                }
            }
            if start.elapsed() >= CONNECT_TIMEOUT {
                error!("Timed out waiting for L2CAP connection to be fully established.");
                return Err(Error::from(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Connection timeout",
                )));
            }
            sleep(POLL_INTERVAL).await;
        }

        info!("L2CAP connection established with {}", addr);

        let (tx, rx) = mpsc::channel(128);
        let state = ATTManagerState::new();
        {
            let mut s = self.state.lock().await;
            *s = state;
            s.sender = Some(tx);
        }

        let manager_clone = self.clone();
        let mut tasks = self.tasks.lock().await;
        tasks.spawn(recv_thread(manager_clone, seq_packet.clone()));
        tasks.spawn(send_thread(rx, seq_packet));

        Ok(())
    }

    pub async fn register_listener(&self, handle: ATTHandles, tx: mpsc::UnboundedSender<Vec<u8>>) {
        let mut state = self.state.lock().await;
        state.listeners.entry(handle as u16).or_default().push(tx);
    }

    pub async fn enable_notifications(&self, handle: ATTHandles) -> Result<()> {
        self.write_cccd(handle.into(), &[0x01, 0x00]).await
    }

    pub async fn read(&self, handle: ATTHandles) -> Result<Vec<u8>> {
        let lsb = (handle as u16 & 0xFF) as u8;
        let msb = ((handle as u16 >> 8) & 0xFF) as u8;
        let pdu = vec![OPCODE_READ_REQUEST, lsb, msb];
        self.request(&pdu).await
    }

    pub async fn write(&self, handle: ATTHandles, value: &[u8]) -> Result<()> {
        let lsb = (handle as u16 & 0xFF) as u8;
        let msb = ((handle as u16 >> 8) & 0xFF) as u8;
        let mut pdu = vec![OPCODE_WRITE_REQUEST, lsb, msb];
        pdu.extend_from_slice(value);
        self.request(&pdu).await?;
        Ok(())
    }

    async fn write_cccd(&self, handle: ATTCCCDHandles, value: &[u8]) -> Result<()> {
        let lsb = (handle as u16 & 0xFF) as u8;
        let msb = ((handle as u16 >> 8) & 0xFF) as u8;
        let mut pdu = vec![OPCODE_WRITE_REQUEST, lsb, msb];
        pdu.extend_from_slice(value);
        self.request(&pdu).await?;
        Ok(())
    }

    async fn send_packet(&self, data: &[u8]) -> Result<()> {
        // Clone the sender and release the lock before awaiting, so a full
        // channel cannot block the receive loop behind this mutex.
        let sender = self.state.lock().await.sender.clone();
        if let Some(sender) = sender {
            sender.send(data.to_vec()).await.map_err(|e| {
                error!("Failed to send packet to channel: {}", e);
                Error::from(std::io::Error::new(
                    std::io::ErrorKind::NotConnected,
                    "L2CAP send channel closed",
                ))
            })
        } else {
            error!("Cannot send packet, sender is not available.");
            Err(Error::from(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "L2CAP stream not connected",
            )))
        }
    }

    /// Send a request PDU and wait for its response, returned without the
    /// opcode. An Error Response for the request is returned as an error.
    async fn request(&self, pdu: &[u8]) -> Result<Vec<u8>> {
        let request_opcode = *pdu
            .first()
            .expect("read and write build their PDU starting with the opcode");
        let mut rx = self.response_rx.lock().await;
        // Responses still queued answer requests that already gave up.
        while let Ok(stale) = rx.try_recv() {
            debug!("Dropping stale response: {}", hex::encode(&stale));
        }
        self.send_packet(pdu).await?;

        debug!("Waiting for response...");
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let resp = match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(resp)) => resp,
                Ok(None) => {
                    return Err(Error::from(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "Response channel closed",
                    )));
                }
                Err(_) => {
                    return Err(Error::from(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "Response timeout",
                    )));
                }
            };
            match match_response(request_opcode, &resp) {
                ResponseMatch::Response(value) => return Ok(value.to_vec()),
                ResponseMatch::Error(code) => {
                    return Err(Error::from(std::io::Error::other(format!(
                        "ATT error {:#04x} for request {:#04x}",
                        code, request_opcode
                    ))));
                }
                ResponseMatch::Unrelated => {
                    debug!("Ignoring unrelated PDU: {}", hex::encode(&resp));
                }
            }
        }
    }
}

async fn recv_thread(manager: ATTManager, sp: Arc<SeqPacket>) {
    let mut buf = vec![0u8; 1024];
    loop {
        match sp.recv(&mut buf).await {
            Ok(0) => {
                info!("Remote closed the connection.");
                break;
            }
            Ok(n) => {
                let data = &buf[..n];
                debug!("Received {} bytes: {}", n, hex::encode(data));
                if data.is_empty() {
                    continue;
                }
                if data[0] == OPCODE_HANDLE_VALUE_NTF || data[0] == OPCODE_HANDLE_VALUE_IND {
                    // Notification or indication: opcode, 2-byte handle, value.
                    if data.len() < 3 {
                        continue;
                    }
                    if data[0] == OPCODE_HANDLE_VALUE_IND {
                        // The server sends no further indication until this
                        // one is confirmed.
                        if let Err(e) = manager.send_packet(&[OPCODE_HANDLE_VALUE_CFM]).await {
                            error!("Failed to confirm indication: {}", e);
                        }
                    }
                    let handle = (data[1] as u16) | ((data[2] as u16) << 8);
                    let value = data[3..].to_vec();
                    let state = manager.state.lock().await;
                    if let Some(listeners) = state.listeners.get(&handle) {
                        for listener in listeners {
                            let _ = listener.send(value.clone());
                        }
                    }
                } else {
                    // A response; request() matches it to what it sent.
                    let _ = manager.response_tx.send(data.to_vec());
                }
            }
            Err(e) => {
                error!("read error: {}", e);
                break;
            }
        }
    }
    let mut state = manager.state.lock().await;
    state.sender = None;
}

async fn send_thread(mut rx: mpsc::Receiver<Vec<u8>>, sp: Arc<SeqPacket>) {
    while let Some(data) = rx.recv().await {
        if let Err(e) = sp.send(&data).await {
            error!("Failed to send data: {}", e);
            break;
        }
        debug!("Sent {} bytes: {}", data.len(), hex::encode(&data));
    }
    info!("send thread finished.");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_response_returns_value() {
        assert_eq!(
            match_response(OPCODE_READ_REQUEST, &[OPCODE_READ_RESPONSE, 0x01, 0x02]),
            ResponseMatch::Response(&[0x01, 0x02])
        );
    }

    #[test]
    fn write_response_is_empty() {
        assert_eq!(
            match_response(OPCODE_WRITE_REQUEST, &[OPCODE_WRITE_RESPONSE]),
            ResponseMatch::Response(&[])
        );
    }

    #[test]
    fn error_response_for_the_request_is_an_error() {
        let pdu = [OPCODE_ERROR_RESPONSE, OPCODE_WRITE_REQUEST, 0x02, 0x80, 0x03];
        assert_eq!(
            match_response(OPCODE_WRITE_REQUEST, &pdu),
            ResponseMatch::Error(0x03)
        );
    }

    #[test]
    fn late_or_foreign_pdus_are_unrelated() {
        // A read response arriving after its request timed out, while a write waits.
        assert_eq!(
            match_response(OPCODE_WRITE_REQUEST, &[OPCODE_READ_RESPONSE, 0x01]),
            ResponseMatch::Unrelated
        );
        // An error for an earlier read, while a write waits.
        let pdu = [OPCODE_ERROR_RESPONSE, OPCODE_READ_REQUEST, 0x02, 0x80, 0x0A];
        assert_eq!(
            match_response(OPCODE_WRITE_REQUEST, &pdu),
            ResponseMatch::Unrelated
        );
        // A truncated error response.
        assert_eq!(
            match_response(
                OPCODE_READ_REQUEST,
                &[OPCODE_ERROR_RESPONSE, OPCODE_READ_REQUEST]
            ),
            ResponseMatch::Unrelated
        );
        assert_eq!(
            match_response(OPCODE_READ_REQUEST, &[]),
            ResponseMatch::Unrelated
        );
    }
}
