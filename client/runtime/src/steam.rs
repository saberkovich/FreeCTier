use anyhow::{ensure, Result};
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

const VIRTUAL_PORT: i32 = 77;

fn options() -> Vec<NetworkingConfigEntry> {
    vec![
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::P2PTransportICEEnable, 0),
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::SendBufferSize, 512 * 1024),
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::TimeoutInitial, 10_000),
        NetworkingConfigEntry::new_int32(NetworkingConfigValue::TimeoutConnected, 10_000),
    ]
}

pub struct Transport {
    listen: ListenSocket,
    peers: BTreeMap<u64, NetConnection>,
    retry: BTreeMap<u64, (Instant, u32)>,
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

    pub fn tick(
        &mut self,
        client: &Client,
        allowed: &BTreeSet<u64>,
        dial: &BTreeSet<u64>,
    ) -> Vec<(u64, Vec<u8>)> {
        self.peers.retain(|id, _| allowed.contains(id));
        self.retry.retain(|id, _| allowed.contains(id));
        while let Some(event) = self.listen.try_receive_event() {
            match event {
                ListenSocketEvent::Connecting(request) => {
                    let id = request.remote().steam_id().map(|id| id.raw());
                    if id.is_some_and(|id| allowed.contains(&id) && !self.peers.contains_key(&id)) {
                        let _ = request.accept();
                    } else {
                        request.reject(
                            NetConnectionEnd::App(AppNetConnectionEnd::generic_normal()),
                            None,
                        );
                    }
                }
                ListenSocketEvent::Connected(event) => {
                    if let Some(id) = event.remote().steam_id().map(|id| id.raw()) {
                        if allowed.contains(&id) && !self.peers.contains_key(&id) {
                            self.peers.insert(id, event.take_connection());
                        }
                    }
                }
                ListenSocketEvent::Disconnected(_) => {
                    // Poll the owned handle below; a rejected duplicate connection
                    // must not evict a different live connection to the same peer.
                }
            }
        }
        let mut messages = Vec::new();
        let mut dead = Vec::new();
        for (&id, connection) in &mut self.peers {
            while connection.try_receive_event().is_some() {}
            match connection.info().and_then(|info| {
                info.state()
                    .map_err(|_| steamworks::networking_sockets::InvalidHandle)
            }) {
                Ok(NetworkingConnectionState::Connected) => {
                    self.retry.remove(&id);
                    if let Ok(batch) = connection.receive_messages(32) {
                        for message in batch {
                            if message.data().len()
                                <= freec_core::wire::MAX_CONTROL + freec_core::wire::HEADER_LEN
                            {
                                messages.push((id, message.data().to_vec()));
                            }
                        }
                    }
                }
                Ok(
                    NetworkingConnectionState::Connecting | NetworkingConnectionState::FindingRoute,
                ) => {}
                _ => dead.push(id),
            }
        }
        for id in dead {
            self.peers.remove(&id);
            let attempt = self.retry.get(&id).map_or(1, |(_, n)| n.saturating_add(1));
            let delay = (1u64 << attempt.min(5)) + id % 3;
            self.retry
                .insert(id, (Instant::now() + Duration::from_secs(delay), attempt));
        }
        for &id in dial {
            if !self.peers.contains_key(&id)
                && self
                    .retry
                    .get(&id)
                    .is_none_or(|(time, _)| Instant::now() >= *time)
            {
                match client.networking_sockets().connect_p2p(
                    NetworkingIdentity::new_steam_id(SteamId::from_raw(id)),
                    VIRTUAL_PORT,
                    options(),
                ) {
                    Ok(connection) => {
                        self.peers.insert(id, connection);
                    }
                    Err(_) => {
                        self.retry
                            .insert(id, (Instant::now() + Duration::from_secs(5), 1));
                    }
                }
            }
        }
        messages
    }

    pub fn connected(&self, id: u64) -> bool {
        self.peers
            .get(&id)
            .and_then(|c| c.info().ok())
            .and_then(|i| i.state().ok())
            == Some(NetworkingConnectionState::Connected)
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
        let connection = self.peers.get(&id)?;
        let (info, _) = client
            .networking_sockets()
            .get_realtime_connection_status(connection, 0)
            .ok()?;
        let ping = info.ping();
        (ping >= 0 && self.connected(id)).then_some(ping)
    }

    pub fn send(&self, id: u64, bytes: &[u8], reliable: bool) -> Result<()> {
        ensure!(self.connected(id), "Peer is not connected");
        let flags = if reliable {
            SendFlags::RELIABLE
        } else {
            SendFlags::UNRELIABLE | SendFlags::NO_NAGLE
        };
        self.peers[&id].send_message(bytes, flags)?;
        Ok(())
    }
}
