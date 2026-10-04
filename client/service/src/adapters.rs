//! Wintun adapter ownership inside the elevated service. ONE persistent
//! adapter (`FreeC`) hosts every network: each network is just a unicast
//! address (10.77.X.1/24) added to or removed from the shared interface —
//! the engine allocates unique third octets, so the /24 routes never
//! conflict. The reader tags each packet with its network by the destination
//! third octet; multicast and the limited broadcast reach the adapter once
//! and are duplicated to every active network, mirroring how the stack used
//! to replicate them across per-network adapters. Validation stays in the
//! UI engine — the service performs no policy checks.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use uuid::Uuid;

/// Mirrors `freec_core::packet::MTU`; kept in sync by tests in freec-core.
const MTU: u32 = 1280;

/// The single shared adapter, created once per service lifetime and reused
/// across clients and networks.
struct Active {
    interface: u32,
    _session: Arc<wintun::Session>,
    reader: Option<JoinHandle<()>>,
}

/// `api` loads lazily: a missing wintun.dll must not prevent the service from
/// reporting a precise error to the client on each open attempt.
#[derive(Default)]
struct Api {
    api: Option<wintun::Wintun>,
}

pub struct Adapters {
    api: Api,
    active: Option<Active>,
    /// Open networks and their local IPs; the source of truth for the
    /// address sweep.
    networks: BTreeMap<Uuid, Ipv4Addr>,
    /// Third octet → network, published to the reader as an atomically
    /// swapped snapshot.
    routes: Arc<Mutex<Arc<BTreeMap<u8, Uuid>>>>,
    /// Where the reader delivers frames; None while no client is attached.
    dest: Arc<Mutex<Option<Sender<Vec<u8>>>>>,
    stop_flag: Arc<AtomicBool>,
}

impl Adapters {
    pub fn new(stop_flag: Arc<AtomicBool>) -> Self {
        Self {
            api: Api::default(),
            active: None,
            networks: BTreeMap::new(),
            routes: Arc::new(Mutex::new(Arc::new(BTreeMap::new()))),
            dest: Arc::new(Mutex::new(None)),
            stop_flag,
        }
    }

    /// Best-effort setup at service start: the NIC exists from boot and any
    /// addresses left by a killed previous run are wiped. A failure (missing
    /// wintun.dll, no adapter rights) defers to the first open attempt.
    pub fn prepare(&mut self) {
        match self.ensure_active() {
            Ok(_) => self.sync_addresses(),
            Err(error) => crate::log(&format!(
                "adapter not created at start (deferred): {error:#}"
            )),
        }
    }

    /// Attach the current client's frame sink; packets read before an attach
    /// (or after a detach) are dropped.
    pub fn attach(&self, sender: Sender<Vec<u8>>) {
        *self.dest.lock().unwrap() = Some(sender);
    }

    /// Open (or re-open) a network on the shared adapter: register its
    /// address and publish its route. Idempotent for live networks.
    pub fn open(&mut self, network: Uuid, local_ip: Ipv4Addr) -> Result<()> {
        let index = self.ensure_active()?;
        self.networks.insert(network, local_ip);
        self.rebuild_routes();
        add_address(index, local_ip)?;
        self.sync_addresses();
        crate::log(&format!("network {local_ip} active on the shared adapter"));
        Ok(())
    }

    /// Close a network: its address is swept and its route unpublished; the
    /// adapter itself stays for the remaining and future networks.
    pub fn close(&mut self, network: Uuid) {
        if self.networks.remove(&network).is_some() {
            self.rebuild_routes();
            self.sync_addresses();
            crate::log("network address removed from the shared adapter");
        }
    }

    /// The client disconnected: detach the sink and sweep every address, so
    /// nothing routes into an adapter nobody is reading.
    pub fn clear(&mut self) {
        *self.dest.lock().unwrap() = None;
        if !self.networks.is_empty() {
            self.networks.clear();
            self.rebuild_routes();
            self.sync_addresses();
        }
    }

    /// Feed an inbound (already validated by the UI) packet into the shared
    /// adapter; the network tag is informational.
    pub fn inject(&self, _network: Uuid, bytes: &[u8]) -> Result<()> {
        let session = &self
            .active
            .as_ref()
            .context("Adapter is not open")?
            ._session;
        let mut packet = session.allocate_send_packet(bytes.len().try_into()?)?;
        packet.bytes_mut().copy_from_slice(bytes);
        session.send_packet(packet);
        Ok(())
    }

    fn rebuild_routes(&self) {
        let next: BTreeMap<u8, Uuid> = self
            .networks
            .iter()
            .map(|(network, ip)| (ip.octets()[2], *network))
            .collect();
        *self.routes.lock().unwrap() = Arc::new(next);
    }

    fn ensure_api(&mut self) -> Result<&wintun::Wintun> {
        if self.api.api.is_none() {
            let dll = std::env::current_exe()?
                .parent()
                .context("Executable has no parent")?
                .join("wintun.dll");
            anyhow::ensure!(
                dll.is_file(),
                "Place the official AMD64 wintun.dll beside the service executable"
            );
            self.api.api =
                Some(unsafe { wintun::load_from_path(dll) }.context("Cannot load wintun.dll")?);
        }
        Ok(self.api.api.as_ref().expect("api loaded"))
    }

    /// Create (or reuse) the shared adapter, configure it once, and start
    /// the persistent reader. A finished reader (broken ring) forces a
    /// rebuild of the session. Returns the adapter's interface index.
    fn ensure_active(&mut self) -> Result<u32> {
        if let Some(active) = &self.active {
            if active
                .reader
                .as_ref()
                .is_some_and(|reader| !reader.is_finished())
            {
                return Ok(active.interface);
            }
            crate::log("reader finished; rebuilding the shared adapter session");
            self.active = None;
        }
        let api = self.ensure_api()?;
        let adapter = match wintun::Adapter::open(api, "FreeC") {
            Ok(adapter) => adapter,
            Err(open_error) => {
                crate::log(&format!("adapter open failed ({open_error}), creating"));
                let created = wintun::Adapter::create(api, "FreeC", "FreeC Tier", None);
                match created {
                    Ok(adapter) => adapter,
                    Err(create_error) => {
                        crate::log(&format!("adapter create failed: {create_error}"));
                        return Err(create_error).context("Cannot open Wintun adapter");
                    }
                }
            }
        };
        let index = interface_index(unsafe {
            std::mem::transmute::<wintun::NET_LUID_LH, [u8; 8]>(adapter.get_luid())
        })?;
        configure_interface(index)?;
        let session = Arc::new(adapter.start_session(wintun::MAX_RING_CAPACITY)?);
        crate::log(&format!("shared adapter ready, interface index {index}"));
        let reader = self.spawn_reader(Arc::clone(&session))?;
        self.active = Some(Active {
            interface: index,
            _session: session,
            reader: Some(reader),
        });
        Ok(index)
    }

    /// The persistent packet pump: adapter ring → tagged pipe frames.
    fn spawn_reader(&mut self, session: Arc<wintun::Session>) -> Result<JoinHandle<()>> {
        let routes = Arc::clone(&self.routes);
        let dest = Arc::clone(&self.dest);
        let stop_flag = Arc::clone(&self.stop_flag);
        std::thread::Builder::new()
            .name("freec-svc-adapter".into())
            .spawn(move || {
                while !stop_flag.load(Ordering::SeqCst) {
                    let bytes = match session.try_receive() {
                        Ok(Some(packet)) => packet.bytes().to_vec(),
                        Ok(None) => {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(_) => break, // ring closed: service stopping
                    };
                    let routes = routes.lock().unwrap().clone();
                    let Some(sender) = dest.lock().unwrap().clone() else {
                        continue; // no client attached — nothing to deliver to
                    };
                    for network in target_networks(&bytes, &routes) {
                        let frame =
                            freec_ipc::encode_service_frame(&freec_ipc::ServiceMessage::Packet {
                                network,
                                packet: bytes.clone(),
                            });
                        match frame {
                            Ok(frame) => {
                                if sender.send(frame).is_err() {
                                    // Pipe closed: detach so later packets
                                    // skip delivery instead of erroring.
                                    dest.lock().unwrap().take();
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            })
            .context("Cannot spawn adapter reader")
    }

    /// Delete every IPv4 address on the shared interface that does not
    /// belong to an open network; leftovers from crashes and closed
    /// networks would otherwise keep stale routes alive.
    fn sync_addresses(&self) {
        let Some(active) = &self.active else {
            return;
        };
        let keep: BTreeSet<Ipv4Addr> = self.networks.values().copied().collect();
        use windows::Win32::NetworkManagement::IpHelper::{
            DeleteUnicastIpAddressEntry, FreeMibTable, GetUnicastIpAddressTable,
            MIB_UNICASTIPADDRESS_TABLE,
        };
        use windows::Win32::Networking::WinSock::AF_INET;
        unsafe {
            let mut table: *mut MIB_UNICASTIPADDRESS_TABLE = std::ptr::null_mut();
            if GetUnicastIpAddressTable(AF_INET, &mut table).is_err() {
                return;
            }
            if !table.is_null() {
                let count = (*table).NumEntries as usize;
                let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), count);
                for row in rows {
                    if row.InterfaceIndex != active.interface {
                        continue;
                    }
                    let ip = ipv4_of_in_addr(row.Address.Ipv4.sin_addr);
                    if !keep.contains(&ip) {
                        let _ = DeleteUnicastIpAddressEntry(row);
                    }
                }
                FreeMibTable(table as _);
            }
        }
    }
}

/// Networks a packet read from the adapter belongs to. Unicast and subnet
/// broadcast map by the destination third octet (each network owns its
/// 10.77.X.0/24); multicast and the limited broadcast reach the adapter
/// once and go to every active network. Non-IPv4 stack chatter is dropped.
fn target_networks(packet: &[u8], routes: &BTreeMap<u8, Uuid>) -> Vec<Uuid> {
    if packet.len() < 20 || packet[0] >> 4 != 4 {
        return Vec::new();
    }
    let destination = [packet[16], packet[17], packet[18], packet[19]];
    if (224..=239).contains(&destination[0]) || destination == [255, 255, 255, 255] {
        return routes.values().copied().collect();
    }
    routes
        .get(&destination[2])
        .map(|network| vec![*network])
        .unwrap_or_default()
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

/// Interface-level setup, done once per adapter: MTU. The stack reports
/// SitePrefixLength 255 for IPv4 interfaces, which SetIpInterfaceEntry
/// rejects unless normalized to zero.
fn configure_interface(index: u32) -> Result<()> {
    use windows::Win32::NetworkManagement::IpHelper::{
        GetIpInterfaceEntry, SetIpInterfaceEntry, MIB_IPINTERFACE_ROW,
    };
    use windows::Win32::Networking::WinSock::AF_INET;
    unsafe {
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
    }
    Ok(())
}

/// Add a network's unicast address; the connected /24 route is created with
/// the address entry. Re-adding a live address is a no-op.
fn add_address(index: u32, local_ip: Ipv4Addr) -> Result<()> {
    use windows::Win32::Foundation::{ERROR_OBJECT_ALREADY_EXISTS, WIN32_ERROR};
    use windows::Win32::NetworkManagement::IpHelper::{
        CreateUnicastIpAddressEntry, InitializeUnicastIpAddressEntry, MIB_UNICASTIPADDRESS_ROW,
    };
    use windows::Win32::Networking::WinSock::{
        IpPrefixOriginManual, IpSuffixOriginManual, AF_INET, SOCKADDR_INET,
    };
    unsafe {
        let mut unicast = MIB_UNICASTIPADDRESS_ROW::default();
        InitializeUnicastIpAddressEntry(&mut unicast);
        unicast.InterfaceIndex = index;
        unicast.Address = SOCKADDR_INET {
            Ipv4: windows::Win32::Networking::WinSock::SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: 0,
                sin_addr: in_addr_of(local_ip),
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

use windows::Win32::Networking::WinSock::{IN_ADDR, IN_ADDR_0};

/// `S_un.S_addr` stores the address octets as its memory image; a
/// native-endian load/store of the u32 keeps that image byte-exact, while
/// the `_be_` variants mirror it (10.77.0.1 became 1.0.77.10 once).
fn in_addr_of(ip: Ipv4Addr) -> IN_ADDR {
    IN_ADDR {
        S_un: IN_ADDR_0 {
            S_addr: u32::from_ne_bytes(ip.octets()),
        },
    }
}

fn ipv4_of_in_addr(addr: IN_ADDR) -> Ipv4Addr {
    Ipv4Addr::from(unsafe { addr.S_un.S_addr }.to_ne_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire image of the address must be the octets in order; the
    /// mirrored image (1.0.77.10 for 10.77.0.1) once shipped and silently
    /// broke every ping while the service log looked perfect.
    #[test]
    fn in_addr_memory_image_is_wire_order() {
        let addr = in_addr_of(Ipv4Addr::new(10, 77, 0, 1));
        let image = unsafe { addr.S_un.S_addr.to_ne_bytes() };
        assert_eq!(image, [10, 77, 0, 1]);
        assert_eq!(ipv4_of_in_addr(addr), Ipv4Addr::new(10, 77, 0, 1));
    }

    const NET_A: Uuid = Uuid::from_bytes([1; 16]);
    const NET_B: Uuid = Uuid::from_bytes([2; 16]);

    fn routes() -> BTreeMap<u8, Uuid> {
        BTreeMap::from([(0, NET_A), (1, NET_B)])
    }

    fn packet(destination: [u8; 4]) -> Vec<u8> {
        let mut bytes = vec![0x45, 0, 0, 20, 0, 0, 0, 0, 64, 17, 0, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&destination);
        bytes.extend_from_slice(&[0; 8]);
        bytes
    }

    #[test]
    fn unicast_maps_by_third_octet() {
        assert_eq!(
            target_networks(&packet([10, 77, 0, 5]), &routes()),
            vec![NET_A]
        );
        assert_eq!(
            target_networks(&packet([10, 77, 1, 5]), &routes()),
            vec![NET_B]
        );
        // Subnet broadcast follows the same mapping.
        assert_eq!(
            target_networks(&packet([10, 77, 1, 255]), &routes()),
            vec![NET_B]
        );
        // Unknown third octet and non-IPv4 chatter drop.
        assert!(target_networks(&packet([10, 77, 7, 5]), &routes()).is_empty());
        assert!(target_networks(&[0x60, 0, 0, 0], &routes()).is_empty());
        assert!(target_networks(&[], &routes()).is_empty());
    }

    #[test]
    fn multicast_reaches_every_active_network() {
        let multicast = packet([224, 0, 2, 60]);
        assert_eq!(target_networks(&multicast, &routes()), vec![NET_A, NET_B]);
        let limited = packet([255, 255, 255, 255]);
        assert_eq!(target_networks(&limited, &routes()), vec![NET_A, NET_B]);
    }
}
