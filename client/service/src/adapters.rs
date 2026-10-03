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
        // Configure the interface through the IP Helper API instead of
        // netsh: netsh resolves the adapter by name and loses the race with
        // a freshly created adapter in session 0; the LUID-based path has no
        // such lookup. The address entry also creates the connected /24
        // route, exactly like the previous netsh call did.
        let index = interface_index(unsafe {
            std::mem::transmute::<wintun::NET_LUID_LH, [u8; 8]>(adapter.get_luid())
        })?;
        configure_interface(index, local_ip)?;
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

/// Interface index for an adapter LUID, as the IP Helper API expects.
fn interface_index(luid: [u8; 8]) -> Result<u32> {
    use windows::Win32::NetworkManagement::IpHelper::ConvertInterfaceLuidToIndex;
    use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
    // wintun_raw and windows define the same 8-byte union layout.
    let mut converted = NET_LUID_LH::default();
    converted.Value = u64::from_ne_bytes(luid);
    let mut index = 0u32;
    let result = unsafe { ConvertInterfaceLuidToIndex(&converted, &mut index) };
    anyhow::ensure!(result.is_ok(), "ConvertInterfaceLuidToIndex: {:?}", result);
    Ok(index)
}

/// Address + MTU via IP Helper: no child process, no name lookup, and the
/// /24 on-link route is created with the address entry.
fn configure_interface(index: u32, local_ip: Ipv4Addr) -> Result<()> {
    use windows::Win32::Foundation::{ERROR_OBJECT_ALREADY_EXISTS, WIN32_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        CreateUnicastIpAddressEntry, GetIpInterfaceEntry, InitializeUnicastIpAddressEntry,
        SetIpInterfaceEntry, MIB_IPINTERFACE_ROW, MIB_UNICASTIPADDRESS_ROW,
    };
    use windows::Win32::Networking::WinSock::{
        IpPrefixOriginManual, IpSuffixOriginManual, AF_INET, IN_ADDR, IN_ADDR_0, SOCKADDR_INET,
    };
    unsafe {
        // MTU. The stack reports SitePrefixLength 255 for IPv4 interfaces,
        // which SetIpInterfaceEntry rejects unless normalized to zero.
        let mut interface = MIB_IPINTERFACE_ROW {
            Family: AF_INET,
            InterfaceIndex: index,
            ..Default::default()
        };
        GetIpInterfaceEntry(&mut interface)
            .ok()
            .context("GetIpInterfaceEntry")?;
        interface.NlMtu = MTU;
        interface.SitePrefixLength = 0;
        SetIpInterfaceEntry(&mut interface)
            .ok()
            .context("SetIpInterfaceEntry (MTU)")?;

        // Unicast address; re-registering a live adapter is a no-op.
        let mut unicast = MIB_UNICASTIPADDRESS_ROW::default();
        InitializeUnicastIpAddressEntry(&mut unicast);
        unicast.InterfaceIndex = index;
        unicast.Address = SOCKADDR_INET {
            Ipv4: windows::Win32::Networking::WinSock::SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: 0,
                sin_addr: IN_ADDR {
                    S_un: IN_ADDR_0 {
                        S_addr: u32::from_be_bytes(local_ip.octets()),
                    },
                },
                sin_zero: [0; 8],
            },
        };
        unicast.OnLinkPrefixLength = 24;
        unicast.PrefixOrigin = IpPrefixOriginManual;
        unicast.SuffixOrigin = IpSuffixOriginManual;
        let result = CreateUnicastIpAddressEntry(&unicast);
        if result != WIN32_ERROR(0) && result != ERROR_OBJECT_ALREADY_EXISTS {
            anyhow::bail!("CreateUnicastIpAddressEntry: {:?}", result);
        }
    }
    Ok(())
}
