//! Adapter facade backed by the FreeC Tier service over a named pipe. The
//! elevated service owns the Wintun sessions; this side keeps the exact
//! interface the engine expects (`open`/`receive`/`inject`/`Drop`) so the
//! packet path stays synchronous and unchanged. Packet validation is not
//! weakened: anti-spoof checks need the Steam sender identity and remain in
//! the engine.
//!
//! The client pipe handle is overlapped-mode: the reader and writer threads
//! issue concurrent ReadFile/WriteFile on the same pipe object, and
//! synchronous handles would deadlock those against each other.

use anyhow::{Context, Result};
use freec_core::config::Network;
use std::collections::BTreeMap;
use std::io;
use std::net::Ipv4Addr;
use std::os::windows::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

/// Per-network inbound queue depth. Overflow drops packets, mirroring Wintun
/// ring backpressure on the service side.
const QUEUE_DEPTH: usize = 1024;
/// How long `open` waits for the service connection and the open ack.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// FILE_FLAG_OVERLAPPED.
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

/// Client-side diagnostics into the app data folder; the service's own log
/// is in ProgramData, this one is per-user for the UI process.
fn client_log(message: &str) {
    let Some(dir) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) else {
        return;
    };
    let path = dir.join("FreeC Tier").join("pipe-client.log");
    let _ = std::fs::create_dir_all(path.parent().expect("log parent"));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{stamp}: {message}");
    }
}

enum WriterMessage {
    Pipe(PipeIo),
    Frame(Vec<u8>),
}

/// Overlapped I/O wrapper over one connected client pipe handle. The
/// direction (read/write) is chosen per call: the reader thread issues
/// ReadFile, the writer thread WriteFile — both overlapped.
struct PipeIo {
    handle: windows::Win32::Foundation::HANDLE,
    event: windows::Win32::Foundation::HANDLE,
}

impl PipeIo {
    /// Takes ownership of the pipe handle for the reading side.
    fn reader(file: std::fs::File) -> Result<Self> {
        use std::os::windows::io::IntoRawHandle;
        Self::new(file.into_raw_handle())
    }

    /// Wraps a duplicated handle for the writing side.
    fn writer(file: std::fs::File) -> Result<Self> {
        use std::os::windows::io::IntoRawHandle;
        Self::new(file.into_raw_handle())
    }

    fn new(raw: *mut std::ffi::c_void) -> Result<Self> {
        use windows::Win32::System::Threading::CreateEventW;
        Ok(Self {
            handle: windows::Win32::Foundation::HANDLE(raw),
            event: unsafe { CreateEventW(None, false, false, None)? },
        })
    }

    /// One overlapped operation, waiting indefinitely for completion — the
    /// client lives for the whole process, so there is nothing to abort for.
    fn read_one(&self, buffer: &mut [u8]) -> io::Result<usize> {
        use windows::Win32::Storage::FileSystem::ReadFile;
        use windows::Win32::System::Threading::WaitForSingleObject;
        use windows::Win32::System::IO::OVERLAPPED;
        let mut overlapped = OVERLAPPED {
            hEvent: self.event,
            ..Default::default()
        };
        match unsafe { ReadFile(self.handle, Some(buffer), None, Some(&mut overlapped)) } {
            Ok(()) => {}
            Err(error) if error.code() == windows::Win32::Foundation::ERROR_IO_PENDING.into() => {
                unsafe { WaitForSingleObject(self.event, u32::MAX) };
            }
            Err(error) => return Err(error.into()),
        }
        let mut bytes = 0u32;
        unsafe {
            windows::Win32::System::IO::GetOverlappedResult(
                self.handle,
                &overlapped,
                &mut bytes,
                false,
            )?;
        }
        Ok(bytes as usize)
    }

    fn write_one(&self, buffer: &[u8]) -> io::Result<usize> {
        use windows::Win32::Storage::FileSystem::WriteFile;
        use windows::Win32::System::Threading::WaitForSingleObject;
        use windows::Win32::System::IO::OVERLAPPED;
        let mut overlapped = OVERLAPPED {
            hEvent: self.event,
            ..Default::default()
        };
        match unsafe { WriteFile(self.handle, Some(buffer), None, Some(&mut overlapped)) } {
            Ok(()) => {}
            Err(error) if error.code() == windows::Win32::Foundation::ERROR_IO_PENDING.into() => {
                unsafe { WaitForSingleObject(self.event, u32::MAX) };
            }
            Err(error) => return Err(error.into()),
        }
        let mut bytes = 0u32;
        unsafe {
            windows::Win32::System::IO::GetOverlappedResult(
                self.handle,
                &overlapped,
                &mut bytes,
                false,
            )?;
        }
        Ok(bytes as usize)
    }

    fn read_exact(&self, buffer: &mut [u8]) -> io::Result<()> {
        let mut done = 0;
        while done < buffer.len() {
            let count = self.read_one(&mut buffer[done..])?;
            if count == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe closed"));
            }
            done += count;
        }
        Ok(())
    }

    fn write_all(&self, mut buffer: &[u8]) -> io::Result<()> {
        while !buffer.is_empty() {
            let done = self.write_one(buffer)?;
            if done == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "pipe write stalled",
                ));
            }
            buffer = &buffer[done..];
        }
        Ok(())
    }

    fn read_frame(&mut self) -> io::Result<Vec<u8>> {
        let mut length = [0u8; 4];
        self.read_exact(&mut length)?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > freec_ipc::MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame size"));
        }
        let mut frame = vec![0u8; length];
        self.read_exact(&mut frame)?;
        Ok(frame)
    }

    fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        self.write_all(&(frame.len() as u32).to_le_bytes())?;
        self.write_all(frame)
    }
}

impl Drop for PipeIo {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        unsafe {
            let _ = CloseHandle(self.handle);
            let _ = CloseHandle(self.event);
        }
    }
}

unsafe impl Send for PipeIo {}
unsafe impl Sync for PipeIo {}

impl io::Read for PipeIo {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.read_one(buffer)
    }
}

impl io::Write for PipeIo {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.write_one(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
    /// Last connect failure, human-readable; surfaced into the event log by
    /// the engine so the user sees WHY the service is unreachable.
    issue: Mutex<Option<String>>,
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

    /// Records a connect failure reason; the engine surfaces it once.
    fn record_issue(&self, message: &str) {
        let mut guard = self.issue.lock().unwrap();
        if guard.as_deref() != Some(message) {
            *guard = Some(message.to_owned());
        }
    }

    /// The current connect issue, if the pipe is not connected.
    pub fn current_issue(&self) -> Option<String> {
        self.issue.lock().unwrap().clone()
    }

    fn clear_issue(&self) {
        *self.issue.lock().unwrap() = None;
    }

    fn resolve_open(&self, network: Uuid, result: Result<(), String>) {
        if let Some(waiter) = self.pending_opens.lock().unwrap().remove(&network) {
            let _ = waiter.send(result);
        }
    }
}

static PIPE: OnceLock<Arc<PipeClient>> = OnceLock::new();

/// The current connect issue for the engine's event log, if the pipe is
/// not connected; None once the handshake completes.
pub fn current_issue() -> Option<String> {
    PIPE.get().and_then(|client| client.current_issue())
}

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
            issue: Mutex::new(None),
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

/// Human-readable reason for a pipe connect failure, for the event log.
fn describe_connect_error(error: &io::Error) -> String {
    match error.raw_os_error() {
        Some(2) => "служба не запущена".to_owned(),
        Some(5) => "доступ к пайпу службы запрещён".to_owned(),
        Some(231) => "служба занята другим подключением".to_owned(),
        Some(232) => "служба перезапускается".to_owned(),
        _ => format!("{error}"),
    }
}

fn pipe_writer(outbound_rx: Receiver<WriterMessage>) {
    let mut io: Option<PipeIo> = None;
    let mut broken = true;
    loop {
        match outbound_rx.recv() {
            Ok(WriterMessage::Pipe(next)) => {
                io = Some(next);
                broken = false;
            }
            Ok(WriterMessage::Frame(frame)) => {
                if broken {
                    continue; // frames while disconnected are dropped
                }
                let Some(handle) = io.as_mut() else { continue };
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
            .custom_flags(FILE_FLAG_OVERLAPPED)
            .open(freec_ipc::PIPE_PATH)
        {
            Ok(file) => file,
            Err(error) => {
                // Service not running (yet): keep retrying quietly, but
                // surface the reason into the event log.
                client.record_issue(&describe_connect_error(&error));
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        let Ok(writer_file) = file.try_clone() else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let Ok(mut io) = PipeIo::reader(file) else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        client_log("connected to the service pipe, sending hello");
        // Handshake: the service verifies the protocol and answers Welcome.
        let handshake = freec_ipc::encode_frame(&freec_ipc::ClientMessage::Hello)
            .and_then(|frame| io.write_frame(&frame))
            .and_then(|()| io.read_frame())
            .and_then(|frame| freec_ipc::decode_service_frame(&frame));
        match &handshake {
            Ok(freec_ipc::ServiceMessage::Welcome) => client_log("handshake complete"),
            _ => client_log(&format!("handshake failed: {handshake:?}")),
        }
        if !matches!(handshake, Ok(freec_ipc::ServiceMessage::Welcome)) {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        // Hand the writer a duplicate of the same connection (NOT a second
        // connection: the service accepts exactly one client), then restore
        // every adapter that was open before the disconnect.
        let Ok(writer_io) = PipeIo::writer(writer_file) else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        client_log("writer pipe handed over");
        let _ = client.outbound.send(WriterMessage::Pipe(writer_io));
        for (network, local_ip) in client.live.lock().unwrap().clone() {
            if let Ok(frame) = freec_ipc::encode_frame(&freec_ipc::ClientMessage::OpenAdapter {
                network,
                local_ip,
            }) {
                let _ = client.send(frame);
            }
        }
        client.clear_issue();
        client.connected.store(true, Ordering::SeqCst);
        while let Ok(frame) = io.read_frame() {
            match freec_ipc::decode_service_frame(&frame) {
                Ok(freec_ipc::ServiceMessage::Packet { network, packet }) => {
                    if let Some(queue) = client.queues.lock().unwrap().get(&network) {
                        let _ = queue.try_send(packet);
                    }
                }
                Ok(freec_ipc::ServiceMessage::AdapterOpened { network }) => {
                    client_log(&format!("open ack ok for {network}"));
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
    client_log(&format!("open sent for {network}, waiting ack"));
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
        Ok(result) => {
            client_log(&format!("open ack for {network}: {result:?}"));
            result
        }
        Err(_) => {
            client_log(&format!("open ack TIMEOUT for {network}"));
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

    /// Deterministic pipe-ownership probe: creating the server pipe with
    /// FILE_FLAG_FIRST_PIPE_INSTANCE only succeeds when no other instance
    /// (e.g. the installed service) owns the name.
    #[cfg(windows)]
    fn pipe_name_free() -> bool {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Security::SECURITY_ATTRIBUTES;
        use windows::Win32::Storage::FileSystem::{
            FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
        };
        use windows::Win32::System::Pipes::CreateNamedPipeW;
        use windows::Win32::System::Pipes::{PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT};
        let name = windows::core::HSTRING::from(freec_ipc::PIPE_PATH);
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                1024,
                1024,
                0,
                Some(&SECURITY_ATTRIBUTES::default()),
            )
        };
        if handle.is_invalid() {
            return false;
        }
        unsafe {
            let _ = CloseHandle(handle);
        }
        true
    }

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
    #[ignore = "live service probe; run explicitly with --ignored on a quiet machine"]
    fn open_reply_arrives_from_service() {
        #[cfg(windows)]
        if !pipe_name_free() {
            eprintln!("skipping: another service instance owns the pipe");
            return;
        }
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
        let result = open_service_adapter(Uuid::new_v4(), Ipv4Addr::new(10, 77, 200, 9));
        if started.elapsed() >= OPEN_TIMEOUT {
            guard.kill();
            let captured = stderr_thread.join().unwrap_or_default();
            if captured.contains("pipe create failed") {
                eprintln!("skipping: another service instance owns the pipe");
                return;
            }
            panic!(
                "service did not reply within the open window: {result:?}; service stderr: {captured:?}"
            );
        }
        eprintln!("open result: {result:?}");
    }
}
