use anyhow::{bail, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use steamworks::{
    networking_sockets::{ListenSocket, NetConnection},
    networking_types::{
        AppNetConnectionEnd, ListenSocketEvent, NetConnectionEnd, NetworkingConfigEntry,
        NetworkingConfigValue, NetworkingConnectionState, NetworkingIdentity, SendFlags,
    },
    Client, SteamId,
};

/// Virtual port shared by the listen socket and every dial. Both ends must
/// agree on it, and symmetric mode is a property of the whole port — see
/// `options()`.
const VIRTUAL_PORT: i32 = 77;

/// `k_nSteamNetworkingConfig_P2P_Transport_ICE_Enable_All`: let Steam gather
/// every candidate type — host addresses, STUN-reflexive, NAT punch. Steam
/// still measures its own relay route in parallel and keeps whichever is
/// faster, so this can only lower ping: two players on one LAN end up talking
/// over that LAN instead of bouncing off the nearest Valve datacenter, and
/// everyone else usually gets a direct internet path instead of a relayed one.
const ICE_ALL: i32 = 0x7fff_ffff;

/// Reconnect pacing. A session that drops has to come back fast — a game's TCP
/// stream survives a sub-second gap and dies on a ten-second one — while a
/// peer that is simply switched off must not be dialled in a tight loop.
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);
/// A session that stayed up this long counts as healthy, so its backoff starts
/// over: one blip after an hour of play costs 250 ms instead of the five
/// seconds that earlier blips had escalated to.
const RETRY_FORGET: Duration = Duration::from_secs(30);

/// Messages pulled from one peer per `drain`. The cap keeps a flooding peer
/// from starving the others; `drain` runs at the engine's hot cadence, so the
/// real ceiling is hundreds of packets per peer per millisecond.
const RECEIVE_BATCH: usize = 64;
const RECEIVE_BATCHES: usize = 4;

/// Floor for Steam's bandwidth estimate. The estimator halves the send rate on
/// loss; with no floor a short burst (a game's own level load, a Wi-Fi hiccup)
/// collapses the tunnel to a crawl and recovers over many seconds. 256 KB/s is
/// below any link that can host a LAN game at all.
const SEND_RATE_MIN: i32 = 256 * 1024;

fn options() -> Vec<NetworkingConfigEntry> {
    vec![
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::P2PTransportICEEnable, ICE_ALL),
        // Both peers dial each other (see `Engine::tick`), so a pair can never
        // be stuck waiting on the one side that has a problem. Symmetric mode
        // is what makes that safe: Steam merges the two half-open sessions
        // into one instead of leaving a duplicate to be rejected. It must be
        // set on the listen socket *and* every outbound connection.
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::SymmetricConnect, 1),
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::SendBufferSize, 512 * 1024),
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::SendRateMin, SEND_RATE_MIN),
        // Candidate gathering plus a NAT punch attempt needs more room than a
        // plain relay handshake; cutting it short would fall back to the relay
        // for the rest of the session.
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::TimeoutInitial, 15_000),
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::TimeoutConnected, 10_000),
    ]
}

/// A session transition worth one line in the event log. Only reported for
/// peers that were actually talking to us: a member who is simply offline must
/// not produce a line every time its dial times out.
pub enum Change {
    Up(u64),
    Down(u64, &'static str),
}

struct Peer {
    connection: NetConnection,
    /// When Steam first reported this session as connected; `None` while it is
    /// still negotiating a route. Cached because `send` and the UI ask for the
    /// state constantly and `info()` is a Steam API call behind a lock.
    up: Option<Instant>,
}

struct Retry {
    at: Instant,
    attempt: u32,
    /// The peer was connected before this entry appeared, so coming back is a
    /// recovery and deserves a log line.
    lost: bool,
}

pub struct Transport {
    listen: ListenSocket,
    peers: BTreeMap<u64, Peer>,
    retry: BTreeMap<u64, Retry>,
}

impl Transport {
    pub fn new(client: &Client) -> Result<Self> {
        client.networking_utils().init_relay_network_access();
        let listen = client
            .networking_sockets()
            .create_listen_socket_p2p(VIRTUAL_PORT, options())?;
        Ok(Self {
            listen,
            peers: BTreeMap::new(),
            retry: BTreeMap::new(),
        })
    }

    /// Connection management: accept, poll state, dial, back off. Needs
    /// `run_callbacks` to have run recently, so it belongs to the engine's
    /// housekeeping pass and not to the packet path.
    pub fn service(
        &mut self,
        client: &Client,
        allowed: &BTreeSet<u64>,
        dial: &BTreeSet<u64>,
    ) -> Vec<Change> {
        let mut changes = Vec::new();
        self.peers.retain(|id, _| allowed.contains(id));
        self.retry.retain(|id, _| allowed.contains(id));
        while let Some(event) = self.listen.try_receive_event() {
            match event {
                ListenSocketEvent::Connecting(request) => {
                    let id = request.remote().steam_id().map(|id| id.raw());
                    if id.is_some_and(|id| allowed.contains(&id) && !self.peers.contains_key(&id)) {
                        let _ = request.accept();
                    } else {
                        // Symmetric mode hides duplicate requests from us, so
                        // this only rejects peers we do not know.
                        request.reject(
                            NetConnectionEnd::App(AppNetConnectionEnd::generic_normal()),
                            None,
                        );
                    }
                }
                ListenSocketEvent::Connected(event) => {
                    if let Some(id) = event.remote().steam_id().map(|id| id.raw()) {
                        if allowed.contains(&id) && !self.peers.contains_key(&id) {
                            self.peers.insert(
                                id,
                                Peer {
                                    connection: event.take_connection(),
                                    up: None,
                                },
                            );
                        }
                    }
                }
                ListenSocketEvent::Disconnected(_) => {
                    // Poll the owned handle below; a rejected duplicate connection
                    // must not evict a different live connection to the same peer.
                }
            }
        }
        let now = Instant::now();
        let mut dead = Vec::new();
        for (&id, peer) in &mut self.peers {
            while peer.connection.try_receive_event().is_some() {}
            let Ok(info) = peer.connection.info() else {
                dead.push((id, None));
                continue;
            };
            match info.state() {
                Ok(NetworkingConnectionState::Connected) => {
                    if peer.up.is_none() {
                        peer.up = Some(now);
                        if self.retry.remove(&id).is_some_and(|retry| retry.lost) {
                            changes.push(Change::Up(id));
                        }
                    }
                }
                Ok(
                    NetworkingConnectionState::Connecting | NetworkingConnectionState::FindingRoute,
                ) => {}
                _ => dead.push((id, info.end_reason())),
            }
        }
        for (id, end) in dead {
            let was_up = self.peers.remove(&id).and_then(|peer| peer.up);
            // A long healthy session earns a clean slate; a flapping one keeps
            // escalating towards RETRY_MAX.
            let healthy = was_up.is_some_and(|up| up.elapsed() >= RETRY_FORGET);
            let attempt = match self.retry.get(&id) {
                Some(retry) if !healthy => retry.attempt.saturating_add(1),
                _ => 1,
            };
            self.retry.insert(
                id,
                Retry {
                    at: now + backoff(attempt, id),
                    attempt,
                    lost: was_up.is_some(),
                },
            );
            if was_up.is_some() {
                changes.push(Change::Down(id, reason(end)));
            }
        }
        for &id in dial {
            if self.peers.contains_key(&id) {
                continue;
            }
            if self.retry.get(&id).is_some_and(|retry| now < retry.at) {
                continue;
            }
            match client.networking_sockets().connect_p2p(
                NetworkingIdentity::new_steam_id(SteamId::from_raw(id)),
                VIRTUAL_PORT,
                options(),
            ) {
                Ok(connection) => {
                    self.peers.insert(
                        id,
                        Peer {
                            connection,
                            up: None,
                        },
                    );
                }
                Err(_) => {
                    let previous = self.retry.get(&id);
                    let attempt = previous.map_or(1, |retry| retry.attempt.saturating_add(1));
                    let lost = previous.is_some_and(|retry| retry.lost);
                    self.retry.insert(
                        id,
                        Retry {
                            at: now + backoff(attempt, id),
                            attempt,
                            lost,
                        },
                    );
                }
            }
        }
        changes
    }

    /// Everything the peers have queued, appended to `out`. This is the hot
    /// half of the transport: it only touches per-connection receive queues, so
    /// it is safe and cheap to call on every engine wake-up — which is what
    /// keeps an inbound game packet from waiting on the housekeeping cadence.
    pub fn drain(&mut self, out: &mut Vec<(u64, Vec<u8>)>) {
        const LIMIT: usize = freec_core::wire::MAX_CONTROL + freec_core::wire::HEADER_LEN;
        for (&id, peer) in &mut self.peers {
            if peer.up.is_none() {
                continue;
            }
            for _ in 0..RECEIVE_BATCHES {
                let Ok(batch) = peer.connection.receive_messages(RECEIVE_BATCH) else {
                    break;
                };
                let count = batch.len();
                for message in batch {
                    if message.data().len() <= LIMIT {
                        out.push((id, message.data().to_vec()));
                    }
                }
                if count < RECEIVE_BATCH {
                    break;
                }
            }
        }
    }

    pub fn connected(&self, id: u64) -> bool {
        self.peers.get(&id).is_some_and(|peer| peer.up.is_some())
    }

    pub fn state(&self, id: u64) -> &'static str {
        if self.connected(id) {
            "connected"
        } else if self.peers.contains_key(&id) {
            "connecting"
        } else if self.retry.contains_key(&id) {
            "reconnecting"
        } else {
            "offline"
        }
    }

    pub fn ping(&self, client: &Client, id: u64) -> Option<i32> {
        let peer = self.peers.get(&id)?;
        peer.up?;
        let (info, _) = client
            .networking_sockets()
            .get_realtime_connection_status(&peer.connection, 0)
            .ok()?;
        let ping = info.ping();
        (ping >= 0).then_some(ping)
    }

    pub fn send(&self, id: u64, bytes: &[u8], reliable: bool) -> Result<()> {
        let Some(peer) = self.peers.get(&id).filter(|peer| peer.up.is_some()) else {
            bail!("Peer is not connected");
        };
        // NO_NAGLE on every send. Nagle coalesces small writes, but what we
        // hand Steam is already a whole IP packet that a game decided to emit
        // now; waiting out Steam's nagle timer would add milliseconds to every
        // tunnelled TCP segment for no bandwidth gain.
        let flags = if reliable {
            SendFlags::RELIABLE | SendFlags::NO_NAGLE
        } else {
            SendFlags::UNRELIABLE | SendFlags::NO_NAGLE
        };
        peer.connection.send_message(bytes, flags)?;
        Ok(())
    }
}

/// Exponential backoff between `RETRY_MIN` and `RETRY_MAX`, plus a per-peer
/// offset so two peers that lost each other do not keep redialling in lockstep.
fn backoff(attempt: u32, id: u64) -> Duration {
    RETRY_MIN
        .saturating_mul(1u32 << attempt.saturating_sub(1).min(5))
        .min(RETRY_MAX)
        + Duration::from_millis(id % 97)
}

/// Steam's end reason as one short Russian phrase for the event log. The point
/// is to tell "their machine went away" apart from "our link to Steam broke",
/// which is the first question when a session drops.
fn reason(end: Option<NetConnectionEnd>) -> &'static str {
    match end {
        Some(NetConnectionEnd::App(_) | NetConnectionEnd::MiscPeerSentNoConnection) => {
            "участник отключился"
        }
        Some(NetConnectionEnd::RemoteTimeout) => "нет ответа от участника",
        Some(NetConnectionEnd::MiscTimeout) => "таймаут соединения",
        Some(NetConnectionEnd::LocalOfflineMode) => "Steam в офлайн-режиме",
        Some(
            NetConnectionEnd::LocalManyRelayConnectivity
            | NetConnectionEnd::MiscSteamConnectivity
            | NetConnectionEnd::LocalNetworkConfig,
        ) => "нет связи с сетью Steam",
        Some(NetConnectionEnd::MiscP2PRendezvous | NetConnectionEnd::MiscP2PNATFirewall) => {
            "не удалось согласовать маршрут"
        }
        _ => "соединение потеряно",
    }
}
