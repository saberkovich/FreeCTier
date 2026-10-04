//! Named-pipe server: creation with a user-restricted DACL, stop-responsive
//! overlapped connect, and the client session loop bridging frames to Wintun.

use crate::adapters::Adapters;
use crate::{log, Ctl};
use anyhow::{Context, Result};
use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, WaitForMultipleObjects, WaitForSingleObject,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

/// Only one pipe instance exists; the UI process opens exactly this path.
pub const PIPE_PATH: &str = "\\\\.\\pipe\\FreeCTierService";

fn stopped() -> io::Error {
    io::Error::other("service stopping")
}

fn is_stopped_error<E: std::fmt::Display>(error: &E) -> bool {
    error.to_string().contains("service stopping")
}

/// Build `\\.\pipe\FreeCTierService` with a DACL that admits SYSTEM,
/// Administrators and the user the service was installed for.
fn create_server(user_sid: &str) -> Result<HANDLE> {
    let name = HSTRING::from(PIPE_PATH);
    let sddl = HSTRING::from(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{user_sid})"));
    unsafe {
        let mut descriptor = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            &sddl,
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .context("Cannot build pipe security descriptor")?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        let handle = CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            1024 * 1024,
            1024 * 1024,
            0,
            Some(&attributes),
        );
        // CreateNamedPipeW copies the security descriptor, so free it now.
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
        if handle.is_invalid() {
            return Err(io::Error::last_os_error()).context("Cannot create named pipe");
        }
        Ok(handle)
    }
}

/// The Windows Firewall treats a freshly created Wintun adapter as an
/// unidentified public network and blocks all inbound traffic from the
/// tunnel (games listening for LAN peers, pings). Allow anything within
/// the FreeC Tier subnets; idempotent across service restarts.
fn open_firewall() {
    use std::os::windows::process::CommandExt;
    for arguments in [
        vec![
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            "name=FreeC Tier",
        ],
        vec![
            "advfirewall",
            "firewall",
            "add",
            "rule",
            "name=FreeC Tier",
            "dir=in",
            "action=allow",
            "protocol=any",
            "remoteip=10.77.0.0/16",
            "profile=any",
        ],
    ] {
        let result = std::process::Command::new("netsh.exe")
            .args(&arguments)
            .creation_flags(0x0800_0000)
            .output();
        match result {
            Ok(output) if output.status.success() => {}
            Ok(output) => log(&format!(
                "firewall rule update failed: {} {}",
                String::from_utf8_lossy(&output.stdout).trim(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => log(&format!("firewall rule update failed: {error}")),
        }
    }
}

/// Overlapped `ConnectNamedPipe` that stays responsive to service stop.
fn wait_client(handle: HANDLE, stop: HANDLE) -> io::Result<()> {
    let event = unsafe { CreateEventW(None, true, false, None)? };
    let result = (|| {
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        match unsafe { ConnectNamedPipe(handle, Some(&mut overlapped)) } {
            Ok(()) => Ok(()),
            Err(error) if error.code() == ERROR_IO_PENDING.into() => {
                let events = [overlapped.hEvent, stop];
                let wait = unsafe { WaitForMultipleObjects(&events, false, u32::MAX) };
                if wait == WAIT_OBJECT_0 {
                    unsafe {
                        let mut bytes = 0u32;
                        GetOverlappedResult(handle, &overlapped, &mut bytes, false)?;
                    }
                    Ok(())
                } else {
                    unsafe {
                        let _ = CancelIoEx(handle, None);
                        let mut bytes = 0u32;
                        let _ = GetOverlappedResult(handle, &overlapped, &mut bytes, true);
                    }
                    Err(stopped())
                }
            }
            Err(error) if error.code() == ERROR_PIPE_CONNECTED.into() => Ok(()),
            Err(error) => Err(error.into()),
        }
    })();
    unsafe {
        let _ = CloseHandle(event);
    }
    result
}

unsafe impl Send for PipeIo {}
unsafe impl Sync for PipeIo {}

/// Framed overlapped I/O over the connected client pipe. Both directions
/// wake up on service stop via `CancelIoEx`, so every method may fail with
/// the `service stopping` sentinel while the service shuts down.
struct PipeIo {
    handle: HANDLE,
    read_event: HANDLE,
    write_event: HANDLE,
    stop: HANDLE,
}

impl PipeIo {
    fn new(handle: HANDLE, stop: HANDLE) -> Result<Self> {
        Ok(Self {
            handle,
            read_event: unsafe { CreateEventW(None, false, false, None)? },
            write_event: unsafe { CreateEventW(None, false, false, None)? },
            stop,
        })
    }

    /// One overlapped read, waiting on the operation event while polling the
    /// stop flag. Returns the number of bytes transferred.
    fn read_one(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut overlapped = OVERLAPPED {
            hEvent: self.read_event,
            ..Default::default()
        };
        match unsafe { ReadFile(self.handle, Some(buffer), None, Some(&mut overlapped)) } {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_IO_PENDING.into() => loop {
                if is_stop_set(self.stop) {
                    unsafe {
                        let _ = CancelIoEx(self.handle, None);
                        let mut bytes = 0u32;
                        let _ = GetOverlappedResult(self.handle, &overlapped, &mut bytes, true);
                    }
                    return Err(stopped());
                }
                if unsafe { WaitForSingleObject(self.read_event, 200) } == WAIT_OBJECT_0 {
                    break;
                }
            },
            Err(error) => return Err(error.into()),
        }
        let mut bytes = 0u32;
        unsafe {
            GetOverlappedResult(self.handle, &overlapped, &mut bytes, false)?;
        }
        Ok(bytes as usize)
    }

    /// One overlapped write with the same stop semantics as [`Self::read_one`].
    fn write_one(&self, buffer: &[u8]) -> io::Result<usize> {
        let mut overlapped = OVERLAPPED {
            hEvent: self.write_event,
            ..Default::default()
        };
        match unsafe { WriteFile(self.handle, Some(buffer), None, Some(&mut overlapped)) } {
            Ok(()) => {}
            Err(error) if error.code() == ERROR_IO_PENDING.into() => loop {
                if is_stop_set(self.stop) {
                    unsafe {
                        let _ = CancelIoEx(self.handle, None);
                        let mut bytes = 0u32;
                        let _ = GetOverlappedResult(self.handle, &overlapped, &mut bytes, true);
                    }
                    return Err(stopped());
                }
                if unsafe { WaitForSingleObject(self.write_event, 200) } == WAIT_OBJECT_0 {
                    break;
                }
            },
            Err(error) => return Err(error.into()),
        }
        let mut bytes = 0u32;
        unsafe {
            GetOverlappedResult(self.handle, &overlapped, &mut bytes, false)?;
        }
        Ok(bytes as usize)
    }

    fn read_exact(&self, buffer: &mut [u8]) -> io::Result<()> {
        log(&format!("svc io: reading {} bytes", buffer.len()));
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

    fn read_frame(&self) -> io::Result<Vec<u8>> {
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

    fn write_frame(&self, frame: &[u8]) -> io::Result<()> {
        self.write_all(&(frame.len() as u32).to_le_bytes())?;
        self.write_all(frame)
    }
}

fn is_stop_set(stop: HANDLE) -> bool {
    unsafe { WaitForSingleObject(stop, 0) == WAIT_OBJECT_0 }
}

/// Accept loop: create the pipe, wait for a client, run the session, repeat
/// until the service is asked to stop.
pub fn serve(ctl: &Arc<Ctl>, user_sid: &str) {
    open_firewall();
    log(&format!(
        "listening on {PIPE_PATH}; pipe DACL user SID: {}",
        if user_sid.is_empty() {
            "(none recorded)"
        } else {
            user_sid
        }
    ));
    let adapters = Arc::new(Mutex::new(Adapters::new(ctl.stop_flag.clone())));
    adapters.lock().unwrap().prepare();
    loop {
        if ctl.is_stopping() {
            break;
        }
        let server = match create_server(user_sid) {
            Ok(handle) => handle,
            Err(error) => {
                log(&format!("pipe create failed: {error:#}"));
                std::thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }
        };
        *ctl.client.lock().unwrap() = Some(server.0 as isize);
        match wait_client(server, ctl.stop_event) {
            Ok(()) => {
                let mut pid = 0u32;
                unsafe {
                    let _ = windows::Win32::System::Pipes::GetNamedPipeClientProcessId(
                        server, &mut pid,
                    );
                }
                log(&format!("client connected (pid {pid})"));
                log("session: waiting for hello");
                if let Err(error) = run_session(server, ctl, &adapters) {
                    if !is_stopped_error(&error) {
                        log(&format!("client session failed: {error:#}"));
                    }
                }
                log("client session ended");
            }
            Err(error) if is_stopped_error(&error) => {
                unsafe {
                    let _ = DisconnectNamedPipe(server);
                    let _ = CloseHandle(server);
                }
                *ctl.client.lock().unwrap() = None;
                break;
            }
            Err(error) => {
                log(&format!("pipe connect failed: {error:#}"));
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        *ctl.client.lock().unwrap() = None;
        adapters.lock().unwrap().clear();
        unsafe {
            let _ = DisconnectNamedPipe(server);
            let _ = CloseHandle(server);
        }
    }
    log("service loop finished");
}

/// One connected client: handshake, then a dispatch loop until the client
/// disconnects or the service stops. Adapters are dropped on any exit — the
/// client re-registers them after reconnecting.
fn run_session(handle: HANDLE, ctl: &Arc<Ctl>, adapters: &Arc<Mutex<Adapters>>) -> Result<()> {
    let io = Arc::new(PipeIo::new(handle, ctl.stop_event)?);
    let handshake = io.read_frame()?;
    match freec_ipc::decode_client_frame(&handshake)? {
        freec_ipc::ClientMessage::Hello => {}
        other => anyhow::bail!("Expected hello, got {other:?}"),
    }
    io.write_frame(&freec_ipc::encode_service_frame(
        &freec_ipc::ServiceMessage::Welcome,
    )?)?;
    log("handshake complete");

    let (outbound, outbound_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let writer_io = Arc::clone(&io);
    let writer_stop = ctl.stop_flag.clone();
    let writer = std::thread::Builder::new()
        .name("freec-svc-writer".into())
        .spawn(move || {
            while let Ok(frame) = outbound_rx.recv() {
                if writer_io.write_frame(&frame).is_err() {
                    break;
                }
                if writer_stop.load(Ordering::SeqCst) {
                    break;
                }
            }
        })
        .context("Cannot spawn pipe writer")?;
    adapters.lock().unwrap().attach(outbound.clone());

    loop {
        if ctl.is_stopping() {
            break;
        }
        log("session: waiting for frame");
        let frame = match io.read_frame() {
            Ok(frame) => frame,
            Err(error) if is_stopped_error(&error) => break,
            Err(error) => return Err(error.into()),
        };

        match freec_ipc::decode_client_frame(&frame)? {
            freec_ipc::ClientMessage::Hello => anyhow::bail!("Duplicate hello"),
            freec_ipc::ClientMessage::OpenAdapter { network, local_ip } => {
                let result = adapters.lock().unwrap().open(network, local_ip);
                let reply = match result {
                    Ok(()) => freec_ipc::ServiceMessage::AdapterOpened { network },
                    Err(error) => freec_ipc::ServiceMessage::AdapterError {
                        network: Some(network),
                        message: format!("{error:#}"),
                    },
                };
                // Replies must go through the single writer thread: the
                // adapter packet pumps start producing frames immediately,
                // and two concurrent WriteFile calls on one pipe handle (one
                // shared event) corrupt completion and lose frames.
                if outbound
                    .send(freec_ipc::encode_service_frame(&reply)?)
                    .is_err()
                {
                    break;
                }
                log(&format!("adapter {network} open handled"));
            }
            freec_ipc::ClientMessage::CloseAdapter { network } => {
                adapters.lock().unwrap().close(network);
                log(&format!("adapter {network} closed"));
            }
            freec_ipc::ClientMessage::Packet { network, packet } => {
                // A missing adapter means the UI closed it while frames were
                // in flight; dropping mirrors Wintun ring overflow.
                let _ = adapters.lock().unwrap().inject(network, &packet);
            }
        }
    }
    drop(outbound);
    let _ = writer.join();
    adapters.lock().unwrap().clear();
    Ok(())
}
