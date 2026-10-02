//! Wintun adapter ownership: moved from the runtime's `adapter.rs` — same
//! naming, netsh calls and session handling — but running inside the elevated
//! service and bridged to the named pipe instead of being polled by the
//! engine.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;
use uuid::Uuid;

/// Mirrors `freec_core::packet::MTU`; kept in sync by tests in freec-core.
const MTU: u32 = 1280;

struct Entry {
    _session: Arc<wintun::Session>,
    reader: Option<JoinHandle<()>>,
}

/// `api` loads lazily: a missing wintun.dll must not prevent the service from
/// reporting a precise error to the client on each open attempt.
#[derive(Default)]
pub struct Adapters {
    api: Option<wintun::Wintun>,
    map: BTreeMap<Uuid, Entry>,
}

fn netsh(args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    let netsh =
        std::path::PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is missing")?)
            .join("System32/netsh.exe");
    let output = std::process::Command::new(&netsh)
        .args(args)
        .creation_flags(0x0800_0000)
        .output()
        .context("Cannot run netsh")?;
    anyhow::ensure!(
        output.status.success(),
        "Cannot configure Wintun: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

impl Adapters {
    fn ensure_api(&mut self) -> Result<&wintun::Wintun> {
        if self.api.is_none() {
            let dll = std::env::current_exe()?
                .parent()
                .context("Executable has no parent")?
                .join("wintun.dll");
            anyhow::ensure!(
                dll.is_file(),
                "Place the official AMD64 wintun.dll beside the service executable"
            );
            self.api =
                Some(unsafe { wintun::load_from_path(dll) }.context("Cannot load wintun.dll")?);
        }
        Ok(self.api.as_ref().expect("api loaded"))
    }

    /// Open (or reuse) the adapter for a network and start its read pump.
    /// `on_packet` receives full wire frames for the pipe writer thread.
    pub fn open(
        &mut self,
        network: Uuid,
        local_ip: Ipv4Addr,
        stop_flag: Arc<AtomicBool>,
        on_packet: Sender<Vec<u8>>,
    ) -> Result<()> {
        if self.map.contains_key(&network) {
            return Ok(());
        }
        let api = self.ensure_api()?;
        let name = format!("FreeC-{}", network.simple());
        let adapter = wintun::Adapter::open(api, &name).or_else(|_| {
            wintun::Adapter::create(api, &name, "FreeC Tier", Some(network.as_u128()))
        })?;
        // Address assignment creates the connected /24 route. Never configure
        // a default gateway or DNS. Passing argv avoids shell interpolation.
        let ip = local_ip.to_string();
        netsh(&[
            "interface",
            "ipv4",
            "set",
            "address",
            &format!("name={name}"),
            "source=static",
            &format!("address={ip}"),
            "mask=255.255.255.0",
            "gateway=none",
            "store=active",
        ])?;
        netsh(&[
            "interface",
            "ipv4",
            "set",
            "subinterface",
            &name,
            &format!("mtu={MTU}"),
            "store=active",
        ])?;
        let session = Arc::new(adapter.start_session(wintun::MAX_RING_CAPACITY)?);
        let reader_session = Arc::clone(&session);
        let reader_network = network;
        let reader = std::thread::Builder::new()
            .name(format!("freec-svc-{}", network.simple()))
            .spawn(move || {
                while !stop_flag.load(Ordering::SeqCst) {
                    match reader_session.try_receive() {
                        Ok(Some(packet)) => {
                            let frame = freec_ipc::encode_service_frame(
                                &freec_ipc::ServiceMessage::Packet {
                                    network: reader_network,
                                    packet: packet.bytes().to_vec(),
                                },
                            );
                            match frame {
                                Ok(frame) => {
                                    if on_packet.send(frame).is_err() {
                                        break; // pipe closed; session tears down
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(1)),
                        Err(_) => break,
                    }
                }
            })
            .context("Cannot spawn adapter reader")?;
        self.map.insert(
            network,
            Entry {
                _session: session,
                reader: Some(reader),
            },
        );
        Ok(())
    }

    pub fn close(&mut self, network: Uuid) {
        if let Some(entry) = self.map.remove(&network) {
            if let Some(reader) = entry.reader {
                let _ = reader.join();
            }
        }
    }

    /// Feed an inbound (already validated by the UI) packet into the adapter.
    pub fn inject(&self, network: Uuid, bytes: &[u8]) -> Result<()> {
        let entry = self
            .map
            .get(&network)
            .context("Adapter is not open for this network")?;
        let session = &entry._session;
        let mut packet = session.allocate_send_packet(bytes.len().try_into()?)?;
        packet.bytes_mut().copy_from_slice(bytes);
        session.send_packet(packet);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }
}
