//! Adapter facade backed by the FreeC Tier service over a named pipe. The
//! elevated service owns the Wintun sessions; this side keeps the exact
//! interface the engine expects (`open`/`receive`/`inject`/`Drop`) so the
//! packet path stays synchronous and unchanged. Packet validation is not
//! weakened: anti-spoof checks need the Steam sender identity and remain in
//! the engine.

use anyhow::{Context, Result};
use freec_core::config::Network;
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

/// Per-network inbound queue depth. Overflow drops packets, mirroring Wintun
/// ring backpressure on the service side.
const QUEUE_DEPTH: usize = 1024;
/// How long `open` waits for the service connection and the open ack. Wintun
/// adapter creation can take seconds on a cold system, so this is generous.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

enum WriterMessage {
    Pipe(std::fs::File),
    Frame(Vec<u8>),
}

struct PipeClient {
    /// Frames and pipe handovers for the writer thread.
    outbound: Sender<WriterMessage>,
    /// Inbound packet queues keyed by network; the demux thread feeds them.
    queues: Mutex<BTreeMap<Uuid, SyncSender<Vec<u8>>>>,
    /// Networks with an open adapter and their local IPs, so a reconnect can
    /// re-register them with the service exactly as before.
    live: Mutex<BTreeMap<Uuid, Ipv4Addr>>,
    /// Correlates OpenAdapter requests with their acks.
    pending_opens: Mutex<BTreeMap<Uuid, Sender<Result<(), String>>>>,
    connected: AtomicBool,
}

unsafe impl Send for PipeClient {}
unsafe impl Sync for PipeClient {}

impl PipeClient {
    fn send(&self, frame: Vec<u8>) -> bool {
        self.outbound.send(WriterMessage::Frame(frame)).is_ok()
    }

    fn wait_connected(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while !self.connected.load(Ordering::SeqCst) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        true
    }

    fn register_queue(&self, network: Uuid) -> Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::sync_channel(QUEUE_DEPTH);
        self.queues.lock().unwrap().insert(network, tx);
        rx
    }

    fn drop_queue(&self, network: &Uuid) {
        self.queues.lock().unwrap().remove(network);
    }

    fn open_waiter(&self, network: Uuid) -> Receiver<Result<(), String>> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending_opens.lock().unwrap().insert(network, tx);
        rx
    }

    fn resolve_open(&self, network: Uuid, result: Result<(), String>) {
        if let Some(waiter) = self.pending_opens.lock().unwrap().remove(&network) {
            let _ = waiter.send(result);
        }
    }
}

static PIPE: OnceLock<Arc<PipeClient>> = OnceLock::new();

/// Returns the shared pipe client, spawning its threads on first use.
fn client() -> Arc<PipeClient> {
    Arc::clone(PIPE.get_or_init(|| {
        let (outbound, outbound_rx) = std::sync::mpsc::channel();
        let client = Arc::new(PipeClient {
            outbound,
            queues: Mutex::new(BTreeMap::new()),
            live: Mutex::new(BTreeMap::new()),
            pending_opens: Mutex::new(BTreeMap::new()),
            connected: AtomicBool::new(false),
        });
        {
            let client = Arc::clone(&client);
            std::thread::Builder::new()
                .name("freec-pipe-reader".into())
                .spawn(move || pipe_reader(client))
                .expect("spawn pipe reader");
        }
        {
            let outbound_rx = outbound_rx;
            std::thread::Builder::new()
                .name("freec-pipe-writer".into())
                .spawn(move || pipe_writer(outbound_rx))
                .expect("spawn pipe writer");
        }
        client
    }))
}

fn pipe_writer(outbound_rx: Receiver<WriterMessage>) {
    let mut file: Option<std::fs::File> = None;
    let mut broken = true;
    loop {
        match outbound_rx.recv() {
            Ok(WriterMessage::Pipe(next)) => {
                file = Some(next);
                broken = false;
            }
            Ok(WriterMessage::Frame(frame)) => {
                if broken {
                    continue; // frames while disconnected are dropped
                }
                let Some(handle) = file.as_mut() else {
                    continue;
                };
                if freec_ipc::write_frame(handle, &frame).is_err() {
                    broken = true;
                }
            }
            Err(_) => break,
        }
    }
}

fn pipe_reader(client: Arc<PipeClient>) {
    loop {
        client.connected.store(false, Ordering::SeqCst);
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(freec_ipc::PIPE_PATH)
        {
            Ok(file) => file,
            Err(_) => {
                // Service not running (yet): keep retrying quietly.
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        // Handshake: the service verifies the protocol and answers Welcome.
        let handshake = freec_ipc::encode_frame(&freec_ipc::ClientMessage::Hello)
            .and_then(|frame| freec_ipc::write_frame(&mut &file, &frame))
            .and_then(|()| freec_ipc::read_frame(&mut &file))
            .and_then(|frame| freec_ipc::decode_service_frame(&frame));
        if !matches!(handshake, Ok(freec_ipc::ServiceMessage::Welcome)) {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        // Hand the writer its handle copy, then restore every adapter that
        // was open before the disconnect.
        let Ok(writer_handle) = file.try_clone() else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let _ = client.outbound.send(WriterMessage::Pipe(writer_handle));
        for (network, local_ip) in client.live.lock().unwrap().clone() {
            if let Ok(frame) = freec_ipc::encode_frame(&freec_ipc::ClientMessage::OpenAdapter {
                network,
                local_ip,
            }) {
                let _ = client.send(frame);
            }
        }
        client.connected.store(true, Ordering::SeqCst);
        while let Ok(frame) = freec_ipc::read_frame(&mut &file) {
            match freec_ipc::decode_service_frame(&frame) {
                Ok(freec_ipc::ServiceMessage::Packet { network, packet }) => {
                    if let Some(queue) = client.queues.lock().unwrap().get(&network) {
                        let _ = queue.try_send(packet);
                    }
                }
                Ok(freec_ipc::ServiceMessage::AdapterOpened { network }) => {
                    client.resolve_open(network, Ok(()));
                }
                Ok(freec_ipc::ServiceMessage::AdapterError { network, message }) => match network {
                    Some(network) => client.resolve_open(network, Err(message)),
                    None => eprintln!("FreeC Tier service: {message}"),
                },
                Ok(freec_ipc::ServiceMessage::Welcome) => {}
                Err(error) => {
                    let _ = error;
                }
            }
        }
        client.connected.store(false, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Open the adapter for a network through the service. Blocks until the
/// service acknowledges (or times out with a precise error).
fn open_service_adapter(network: Uuid, local_ip: Ipv4Addr) -> Result<Receiver<Vec<u8>>> {
    let client = client();
    if !client.wait_connected(OPEN_TIMEOUT) {
        anyhow::bail!("Служба FreeC Tier не запущена — переустановите её в настройках приложения");
    }
    let inbound = client.register_queue(network);
    client.live.lock().unwrap().insert(network, local_ip);
    let waiter = client.open_waiter(network);
    let frame =
        freec_ipc::encode_frame(&freec_ipc::ClientMessage::OpenAdapter { network, local_ip })?;
    if !client.send(frame) {
        client.drop_queue(&network);
        client.live.lock().unwrap().remove(&network);
        anyhow::bail!("Служба FreeC Tier не запущена — переустановите её в настройках приложения");
    }
    // A timeout still cleans up: the live map drives adapter re-registration
    // after reconnects, and a stale entry would reopen an adapter the engine
    // considers closed. The service may complete the open later — its late
    // ack finds no waiter and is dropped harmlessly, and a retry hits the
    // service's already-open fast path.
    let result = match waiter.recv_timeout(OPEN_TIMEOUT) {
        Ok(result) => result,
        Err(_) => {
            client.pending_opens.lock().unwrap().remove(&network);
            client.drop_queue(&network);
            client.live.lock().unwrap().remove(&network);
            anyhow::bail!("Служба FreeC Tier не отвечает");
        }
    };
    match result {
        Ok(()) => Ok(inbound),
        Err(message) => {
            client.drop_queue(&network);
            client.live.lock().unwrap().remove(&network);
            anyhow::bail!("{message}")
        }
    }
}

pub struct Adapter {
    network: Uuid,
    outbound: Sender<WriterMessage>,
    inbound: Receiver<Vec<u8>>,
}

impl Adapter {
    pub fn open(network: &Network, local: &str) -> Result<Self> {
        let local_ip = network.member(local).context("Not a network member")?.ip;
        let client = client();
        let inbound = open_service_adapter(network.id, local_ip)?;
        Ok(Self {
            network: network.id,
            outbound: client.outbound.clone(),
            inbound,
        })
    }

    /// Non-blocking poll of packets injected by the service.
    pub fn receive(&self) -> Result<Option<Vec<u8>>> {
        match self.inbound.try_recv() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Ok(None), // service restarting
        }
    }

    /// Hand a validated inbound packet to the service for injection.
    pub fn inject(&self, bytes: &[u8]) -> Result<()> {
        let frame = freec_ipc::encode_frame(&freec_ipc::ClientMessage::Packet {
            network: self.network,
            packet: bytes.to_vec(),
        })?;
        self.outbound
            .send(WriterMessage::Frame(frame))
            .map_err(|_| anyhow::anyhow!("Служба FreeC Tier недоступна"))
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        let client = client();
        client.drop_queue(&self.network);
        client.live.lock().unwrap().remove(&self.network);
        if let Ok(frame) = freec_ipc::encode_frame(&freec_ipc::ClientMessage::CloseAdapter {
            network: self.network,
        }) {
            let _ = client.send(frame);
        }
    }
}

#[cfg(test)]
mod service_live {
    use super::*;

    struct ServiceGuard(std::process::Child);
    impl ServiceGuard {
        fn kill(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    impl Drop for ServiceGuard {
        fn drop(&mut self) {
            self.kill();
        }
    }

    /// End-to-end plumbing check against a real console-mode service: the
    /// OpenAdapter reply must arrive well within the open window (a regression
    /// test for replies lost to concurrent pipe writes). The adapter itself
    /// fails without elevation, but the reply path under test is identical.
    /// Skipped when the service binary has not been built yet, or when
    /// another (e.g. the installed) service instance already owns the pipe.
    #[test]
    fn open_reply_arrives_from_service() {
        let exe = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug/freec-service.exe");
        if !exe.is_file() {
            eprintln!("skipping: {} is not built", exe.display());
            return;
        }
        let stderr = std::process::Stdio::piped();
        let mut child = std::process::Command::new(&exe)
            .arg("--console")
            .stdout(std::process::Stdio::null())
            .stderr(stderr)
            .spawn()
            .expect("spawn console service");
        // A stale foreign service (e.g. the installed one) owns the pipe; the
        // spawned instance then cannot bind and the probe cannot judge the
        // reply path. Its stderr tells the two cases apart.
        let mut child_stderr = child.stderr.take().expect("stderr");
        let stderr_thread = std::thread::spawn(move || {
            let mut captured = String::new();
            use std::io::Read;
            let _ = child_stderr.read_to_string(&mut captured);
            captured
        });
        let mut guard = ServiceGuard(child);
        std::thread::sleep(Duration::from_millis(1500));
        let started = std::time::Instant::now();
        let result = open_service_adapter(Uuid::new_v4(), Ipv4Addr::new(10, 77, 9, 9));
        if started.elapsed() >= OPEN_TIMEOUT {
            guard.kill();
            let captured = stderr_thread.join().unwrap_or_default();
            if captured.contains("pipe create failed") {
                eprintln!("skipping: another service instance owns the pipe");
                return;
            }
            panic!("service did not reply within the open window: {result:?}");
        }
        eprintln!("open result: {result:?}");
    }
}
