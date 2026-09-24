use {
    crate::bluetooth::l2cap::{self, ConnectError},
    bluer::Address,
    hex,
    std::{collections::HashMap, sync::Arc},
    tokio::{
        sync::{Mutex, mpsc},
        task::JoinSet,
        time::{Duration, Instant},
    },
    tracing::{debug, error, info},
};

const PSM_ATT: u16 = 0x001F;

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
    NothingEverything = 0x8002,
    /// Nothing earbuds notify on this handle, not on the one written to.
    NothingEverythingRead = 0x8005,
}

/// Why an ATT request got no successful response.
#[derive(Debug, thiserror::Error)]
pub enum AttError {
    #[error("ATT channel is not connected")]
    NotConnected,
    #[error("ATT send channel closed")]
    SendChannelClosed,
    #[error("ATT response channel closed")]
    ResponseChannelClosed,
    #[error("no response to ATT request {request:#04x} within {RESPONSE_TIMEOUT:?}")]
    Timeout { request: u8 },
    #[error("ATT request {request:#04x} failed with error {code:#04x}")]
    ErrorResponse { request: u8, code: u8 },
}

#[derive(Default)]
struct ATTManagerState {
    sender: Option<mpsc::Sender<Vec<u8>>>,
    listeners: HashMap<u16, Vec<mpsc::UnboundedSender<Vec<u8>>>>,
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
        },
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
            state: Arc::new(Mutex::new(ATTManagerState::default())),
            response_rx: Arc::new(Mutex::new(rx)),
            response_tx: tx,
            tasks: Arc::new(Mutex::new(JoinSet::new())),
        }
    }

    pub async fn connect(&mut self, addr: Address) -> Result<(), ConnectError> {
        info!("ATTManager connecting to {addr} on PSM {PSM_ATT:#06X}...");
        let seq_packet = Arc::new(l2cap::connect_seq_packet(addr, PSM_ATT).await?);

        let (tx, rx) = mpsc::channel(128);
        self.attach_transport(tx).await;

        let manager_clone = self.clone();
        let mut tasks = self.tasks.lock().await;
        let recv_socket = seq_packet.clone();
        tasks.spawn(async move {
            recv_thread(&manager_clone, &recv_socket).await;
        });
        tasks.spawn(send_thread(rx, seq_packet));

        Ok(())
    }

    /// Route outgoing PDUs to `tx` and forget the listeners of any earlier
    /// connection. The socket's send task drains `tx`; tests read it directly.
    async fn attach_transport(&self, tx: mpsc::Sender<Vec<u8>>) {
        *self.state.lock().await = ATTManagerState {
            sender: Some(tx),
            listeners: HashMap::new(),
        };
    }

    pub async fn register_listener(&self, handle: ATTHandles, tx: mpsc::UnboundedSender<Vec<u8>>) {
        let mut state = self.state.lock().await;
        state.listeners.entry(handle as u16).or_default().push(tx);
    }

    pub async fn write(&self, handle: ATTHandles, value: &[u8]) -> Result<(), AttError> {
        let [lsb, msb] = (handle as u16).to_le_bytes();
        let mut pdu = vec![OPCODE_WRITE_REQUEST, lsb, msb];
        pdu.extend_from_slice(value);
        self.request(OPCODE_WRITE_REQUEST, &pdu).await?;
        Ok(())
    }

    async fn send_packet(&self, data: &[u8]) -> Result<(), AttError> {
        // Clone the sender and release the lock before awaiting, so a full
        // channel cannot block the receive loop behind this mutex.
        let sender = self.state.lock().await.sender.clone();
        let Some(sender) = sender else {
            error!("Cannot send packet, sender is not available.");
            return Err(AttError::NotConnected);
        };
        sender.send(data.to_vec()).await.map_err(|e| {
            error!("Failed to send packet to channel: {e}");
            AttError::SendChannelClosed
        })
    }

    /// Send a request PDU starting with `request_opcode` and wait for its
    /// response, returned without the opcode. An Error Response for the
    /// request is returned as an error.
    async fn request(&self, request_opcode: u8, pdu: &[u8]) -> Result<Vec<u8>, AttError> {
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
                Ok(None) => return Err(AttError::ResponseChannelClosed),
                Err(_) => {
                    return Err(AttError::Timeout {
                        request: request_opcode,
                    });
                },
            };
            match match_response(request_opcode, &resp) {
                ResponseMatch::Response(value) => return Ok(value.to_vec()),
                ResponseMatch::Error(code) => {
                    return Err(AttError::ErrorResponse {
                        request: request_opcode,
                        code,
                    });
                },
                ResponseMatch::Unrelated => {
                    debug!("Ignoring unrelated PDU: {}", hex::encode(&resp));
                },
            }
        }
    }

    /// Handle one PDU from the server: notifications and indications go to the
    /// listeners of their handle, everything else to the waiting request.
    async fn handle_pdu(&self, data: &[u8]) {
        match data {
            // Notification or indication: opcode, 2-byte handle, value.
            [
                opcode @ (OPCODE_HANDLE_VALUE_NTF | OPCODE_HANDLE_VALUE_IND),
                lsb,
                msb,
                value @ ..,
            ] => {
                if *opcode == OPCODE_HANDLE_VALUE_IND {
                    // The server sends no further indication until this one
                    // is confirmed.
                    if let Err(e) = self.send_packet(&[OPCODE_HANDLE_VALUE_CFM]).await {
                        error!("Failed to confirm indication: {e}");
                    }
                }
                let handle = u16::from_le_bytes([*lsb, *msb]);
                let state = self.state.lock().await;
                if let Some(listeners) = state.listeners.get(&handle) {
                    for listener in listeners {
                        let _ = listener.send(value.to_vec());
                    }
                }
            },
            // Empty, or a notification too short to carry a handle.
            [] | [OPCODE_HANDLE_VALUE_NTF | OPCODE_HANDLE_VALUE_IND, ..] => {},
            // A response; request() matches it to what it sent.
            _ => {
                let _ = self.response_tx.send(data.to_vec());
            },
        }
    }
}

async fn recv_thread(manager: &ATTManager, sp: &bluer::l2cap::SeqPacket) {
    let mut buf = vec![0u8; 1024];
    loop {
        match sp.recv(&mut buf).await {
            Ok(0) => {
                info!("Remote closed the connection.");
                break;
            },
            Ok(n) => {
                let data = &buf[..n];
                debug!("Received {} bytes: {}", n, hex::encode(data));
                manager.handle_pdu(data).await;
            },
            Err(e) => {
                error!("read error: {e}");
                break;
            },
        }
    }
    manager.state.lock().await.sender = None;
}

async fn send_thread(mut rx: mpsc::Receiver<Vec<u8>>, sp: Arc<bluer::l2cap::SeqPacket>) {
    while let Some(data) = rx.recv().await {
        if let Err(e) = sp.send(&data).await {
            error!("Failed to send data: {e}");
            break;
        }
        debug!("Sent {} bytes: {}", data.len(), hex::encode(&data));
    }
    info!("send thread finished.");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager whose outgoing PDUs land in the returned receiver.
    async fn connected_manager() -> (ATTManager, mpsc::Receiver<Vec<u8>>) {
        let manager = ATTManager::new();
        let (tx, rx) = mpsc::channel(16);
        manager.attach_transport(tx).await;
        (manager, rx)
    }

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
        let pdu = [
            OPCODE_ERROR_RESPONSE,
            OPCODE_WRITE_REQUEST,
            0x02,
            0x80,
            0x03,
        ];
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

    #[test]
    fn requests_other_than_read_and_write_match_nothing() {
        assert_eq!(
            match_response(OPCODE_HANDLE_VALUE_CFM, &[OPCODE_WRITE_RESPONSE]),
            ResponseMatch::Unrelated
        );
    }

    #[tokio::test]
    async fn write_sends_the_handle_little_endian_and_returns_on_write_response() {
        let (manager, mut sent) = connected_manager().await;

        let write = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .write(ATTHandles::NothingEverything, &[0xAA, 0xBB])
                    .await
            }
        });
        let pdu = sent.recv().await.unwrap();
        manager.handle_pdu(&[OPCODE_WRITE_RESPONSE]).await;

        assert_eq!(pdu, [OPCODE_WRITE_REQUEST, 0x02, 0x80, 0xAA, 0xBB]);
        assert!(write.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn write_skips_a_late_read_response_and_takes_its_own() {
        let (manager, mut sent) = connected_manager().await;

        let write = tokio::spawn({
            let manager = manager.clone();
            async move { manager.write(ATTHandles::NothingEverything, &[0x01]).await }
        });
        sent.recv().await.unwrap();
        manager.handle_pdu(&[OPCODE_READ_RESPONSE, 0x42]).await;
        manager.handle_pdu(&[OPCODE_WRITE_RESPONSE]).await;

        assert!(write.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn write_returns_the_error_code_of_an_error_response() {
        let (manager, mut sent) = connected_manager().await;

        let write = tokio::spawn({
            let manager = manager.clone();
            async move { manager.write(ATTHandles::NothingEverything, &[0x01]).await }
        });
        sent.recv().await.unwrap();
        manager
            .handle_pdu(&[
                OPCODE_ERROR_RESPONSE,
                OPCODE_WRITE_REQUEST,
                0x02,
                0x80,
                0x03,
            ])
            .await;

        let err = write.await.unwrap().unwrap_err();
        assert!(
            matches!(
                err,
                AttError::ErrorResponse {
                    request: OPCODE_WRITE_REQUEST,
                    code: 0x03
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn write_drops_responses_queued_before_it_was_sent() {
        let (manager, mut sent) = connected_manager().await;
        // A response to an earlier request that already timed out.
        manager.handle_pdu(&[OPCODE_WRITE_RESPONSE]).await;

        let write = tokio::spawn({
            let manager = manager.clone();
            async move { manager.write(ATTHandles::NothingEverything, &[0x01]).await }
        });
        sent.recv().await.unwrap();
        manager
            .handle_pdu(&[
                OPCODE_ERROR_RESPONSE,
                OPCODE_WRITE_REQUEST,
                0x02,
                0x80,
                0x0E,
            ])
            .await;

        assert!(matches!(
            write.await.unwrap(),
            Err(AttError::ErrorResponse { code: 0x0E, .. })
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn write_times_out_without_a_response() {
        let (manager, _sent) = connected_manager().await;
        let start = Instant::now();

        let result = manager.write(ATTHandles::NothingEverything, &[0x01]).await;

        assert!(matches!(
            result,
            Err(AttError::Timeout {
                request: OPCODE_WRITE_REQUEST
            })
        ));
        assert_eq!(start.elapsed(), RESPONSE_TIMEOUT);
    }

    #[tokio::test]
    async fn write_without_a_connection_fails_with_not_connected() {
        let manager = ATTManager::new();

        let result = manager.write(ATTHandles::NothingEverything, &[0x01]).await;

        assert!(matches!(result, Err(AttError::NotConnected)));
    }

    #[tokio::test]
    async fn write_after_the_socket_task_ended_fails_with_channel_closed() {
        let (manager, sent) = connected_manager().await;
        drop(sent);

        let result = manager.write(ATTHandles::NothingEverything, &[0x01]).await;

        assert!(matches!(result, Err(AttError::SendChannelClosed)));
    }

    #[tokio::test]
    async fn notification_reaches_the_listeners_of_its_handle_only() {
        let (manager, mut sent) = connected_manager().await;
        let (read_tx, mut read_rx) = mpsc::unbounded_channel();
        let (write_tx, mut write_rx) = mpsc::unbounded_channel();
        manager
            .register_listener(ATTHandles::NothingEverythingRead, read_tx)
            .await;
        manager
            .register_listener(ATTHandles::NothingEverything, write_tx)
            .await;

        manager
            .handle_pdu(&[OPCODE_HANDLE_VALUE_NTF, 0x05, 0x80, 0x55, 0x20])
            .await;

        assert_eq!(read_rx.try_recv().unwrap(), [0x55, 0x20]);
        assert!(write_rx.try_recv().is_err());
        // Notifications are not confirmed.
        assert!(sent.try_recv().is_err());
    }

    #[tokio::test]
    async fn indication_is_confirmed_and_delivered() {
        let (manager, mut sent) = connected_manager().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager
            .register_listener(ATTHandles::NothingEverythingRead, tx)
            .await;

        manager
            .handle_pdu(&[OPCODE_HANDLE_VALUE_IND, 0x05, 0x80, 0x01])
            .await;

        assert_eq!(sent.try_recv().unwrap(), [OPCODE_HANDLE_VALUE_CFM]);
        assert_eq!(rx.try_recv().unwrap(), [0x01]);
    }

    #[tokio::test]
    async fn truncated_notification_is_dropped() {
        let (manager, mut sent) = connected_manager().await;
        let (tx, mut rx) = mpsc::unbounded_channel();
        manager
            .register_listener(ATTHandles::NothingEverythingRead, tx)
            .await;

        manager.handle_pdu(&[OPCODE_HANDLE_VALUE_IND, 0x05]).await;
        manager.handle_pdu(&[]).await;

        assert!(rx.try_recv().is_err());
        assert!(sent.try_recv().is_err());
        assert!(manager.response_rx.lock().await.try_recv().is_err());
    }
}
