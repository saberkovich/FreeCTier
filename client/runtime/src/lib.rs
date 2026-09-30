mod adapter;
mod steam;

pub const DEFAULT_APP_ID: u32 = 324810;

use anyhow::{ensure, Context, Result};
use ed25519_dalek::SigningKey;
use freec_core::{
    config::{Network, SignedNetwork},
    packet::Packet,
    storage::Store,
    wire::{Frame, Kind},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use steamworks::{
    CallbackHandle, Client, FriendFlags, GameLobbyJoinRequested, LobbyId, LobbyType, SteamId,
};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub steam: String,
    pub steam_id: Option<String>,
    pub nickname: Option<String>,
    pub relay: String,
    pub networks: Vec<NetworkView>,
    pub friends: Vec<FriendView>,
    pub events: Vec<String>,
    pub received: u64,
    pub sent: u64,
    pub dropped: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct FriendView {
    pub steam_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PeerView {
    pub steam_id: String,
    pub name: String,
    pub ip: String,
    pub active: bool,
    pub state: String,
    pub ping_ms: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NetworkView {
    pub id: String,
    pub name: String,
    pub subnet: String,
    pub owner: String,
    pub revision: u64,
    pub adapter: bool,
    pub lobby: Option<String>,
    pub members: Vec<PeerView>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Create { name: String },
    Invite { network: Uuid },
    InviteFriend { network: Uuid, steam_id: String },
    Join { lobby: String },
    SetAdapter { network: Uuid, enabled: bool },
    Revoke { network: Uuid, steam_id: String },
    Readmit { network: Uuid, steam_id: String },
    Delete { network: Uuid },
    Shutdown,
}

#[derive(Clone)]
pub struct Handle {
    tx: mpsc::SyncSender<Command>,
    pub snapshot: Arc<Mutex<Snapshot>>,
    worker: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl Handle {
    pub fn send(&self, command: Command) -> Result<()> {
        self.tx
            .try_send(command)
            .context("Worker is busy or stopped")
    }
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// Stop the worker and wait for it to drop the Steam client. `steam_api64.dll`
    /// can hang on process unload if `SteamAPI_Shutdown` has not run yet.
    pub fn shutdown(&self) {
        let worker = self.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
        let _ = self.tx.try_send(Command::Shutdown);
        if let Some(join) = worker {
            let (done_tx, done_rx) = mpsc::channel();
            let joined = thread::Builder::new()
                .name("freec-join".into())
                .spawn(move || {
                    let _ = join.join();
                    let _ = done_tx.send(());
                });
            if joined.is_ok() {
                let _ = done_rx.recv_timeout(Duration::from_secs(3));
            }
        }
    }
}

pub fn start(root: Option<PathBuf>, app_id: u32) -> Result<Handle> {
    let root = match root {
        Some(root) => root,
        None => directories::BaseDirs::new()
            .context("Cannot locate application data")?
            .config_dir()
            .join("FreeC Tier"),
    };
    std::fs::create_dir_all(&root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("runtime.lock"))?;
    fs2::FileExt::try_lock_exclusive(&lock)
        .context("FreeC Tier is already running. Close the other desktop/CLI instance first")?;
    let (tx, rx) = mpsc::sync_channel(64);
    let snapshot = Arc::new(Mutex::new(Snapshot {
        steam: "waiting".into(),
        relay: "unknown".into(),
        ..Default::default()
    }));
    let handle = Handle {
        tx,
        snapshot: snapshot.clone(),
        worker: Arc::new(Mutex::new(None)),
    };
    let worker = handle.worker.clone();
    let join = thread::Builder::new()
        .name("freec-steam".into())
        .spawn(move || {
            let _instance_lock = lock;
            run(root, app_id, rx, snapshot)
        })?;
    *worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(join);
    Ok(handle)
}

enum Event {
    Created(Uuid, std::result::Result<LobbyId, String>),
    JoinRequested(LobbyId),
    Joined(std::result::Result<LobbyId, String>),
}

struct Engine {
    client: Client,
    transport: steam::Transport,
    _callbacks: Vec<CallbackHandle>,
    events_tx: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    local: String,
    store: Store,
    key: SigningKey,
    networks: BTreeMap<Uuid, SignedNetwork>,
    adapters: BTreeMap<Uuid, adapter::Adapter>,
    lobby: Option<(Uuid, LobbyId)>,
    creating: bool,
    queued_invite: Option<(Uuid, u64)>,
    /// Only the explicitly joined lobby's Steam owner may bootstrap this network.
    pending: BTreeMap<Uuid, (u64, LobbyId, Instant)>,
    sent_revisions: BTreeMap<(u64, Uuid), u64>,
    last_sync: Instant,
    last_view: Instant,
    restore: Vec<Uuid>,
    refresh_configs: Instant,
}

impl Engine {
    fn new(root: &std::path::Path, app_id: u32) -> Result<Self> {
        let client = Client::init_app(app_id)?;
        ensure!(client.user().logged_on(), "Steam is not signed in");
        let local = client.user().steam_id().raw().to_string();
        let store = Store::new(root.join("accounts").join(&local))?;
        let key = store.owner_key()?;
        let networks = store
            .load()?
            .into_iter()
            .map(|n| (n.network.id, n))
            .collect();
        let restore = store.load_enabled()?;
        let transport = steam::Transport::new(&client)?;
        let (tx, rx) = mpsc::channel();
        let callback_tx = tx.clone();
        let callback = client.register_callback(move |event: GameLobbyJoinRequested| {
            let _ = callback_tx.send(Event::JoinRequested(event.lobby_steam_id));
        });
        Ok(Self {
            client,
            transport,
            _callbacks: vec![callback],
            events_tx: tx,
            events_rx: rx,
            local,
            store,
            key,
            networks,
            adapters: BTreeMap::new(),
            lobby: None,
            creating: false,
            queued_invite: None,
            pending: BTreeMap::new(),
            sent_revisions: BTreeMap::new(),
            last_sync: Instant::now(),
            last_view: Instant::now() - Duration::from_secs(1),
            restore,
            refresh_configs: Instant::now(),
        })
    }

    fn join(&self, lobby: LobbyId) {
        let tx = self.events_tx.clone();
        self.client.matchmaking().join_lobby(lobby, move |result| {
            let _ = tx.send(Event::Joined(
                result.map_err(|_| "Cannot join Steam lobby".into()),
            ));
        });
    }

    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Create { name } => {
                let index = (0..=255)
                    .find(|index| {
                        !self
                            .networks
                            .values()
                            .any(|n| n.network.subnet.octets()[2] == *index)
                    })
                    .context("No free subnet")?;
                let network = SignedNetwork::sign(
                    Network::new(name, self.local.parse()?, index, &self.key)?,
                    &self.key,
                )?;
                self.store.save(&network)?;
                let id = network.network.id;
                self.networks.insert(id, network);
                self.command(Command::Invite { network: id })?;
            }
            Command::Invite { network } => {
                ensure!(
                    self.networks
                        .get(&network)
                        .context("Unknown network")?
                        .network
                        .owner
                        == self.local,
                    "Only owner can invite"
                );
                ensure!(!self.creating, "Lobby creation already in progress");
                if let Some((id, lobby)) = self.lobby {
                    if id == network {
                        self.client.friends().activate_invite_dialog(lobby);
                        return Ok(());
                    }
                    self.client.matchmaking().leave_lobby(lobby);
                    self.lobby = None;
                }
                self.creating = true;
                let tx = self.events_tx.clone();
                self.client
                    .matchmaking()
                    .create_lobby(LobbyType::Private, 32, move |result| {
                        let _ = tx.send(Event::Created(network, result.map_err(|e| e.to_string())));
                    });
            }
            Command::InviteFriend { network, steam_id } => {
                let peer = steam_id.parse::<u64>()?;
                ensure!(
                    self.client
                        .friends()
                        .get_friend(SteamId::from_raw(peer))
                        .has_friend(FriendFlags::IMMEDIATE),
                    "This SteamID is not in your friends list"
                );
                ensure!(
                    self.networks
                        .get(&network)
                        .context("Unknown network")?
                        .network
                        .owner
                        == self.local,
                    "Only owner can invite"
                );
                if let Some((id, lobby)) = self.lobby.filter(|(id, _)| *id == network) {
                    let _ = id;
                    self.invite_friend(lobby, peer)?;
                } else {
                    ensure!(!self.creating, "Lobby creation in progress; retry shortly");
                    self.queued_invite = Some((network, peer));
                    self.command(Command::Invite { network })?;
                }
            }
            Command::Join { lobby } => self.join(LobbyId::from_raw(lobby.parse()?)),
            Command::SetAdapter { network, enabled } => {
                if !enabled {
                    let enabled: Vec<_> = self
                        .adapters
                        .keys()
                        .copied()
                        .filter(|id| *id != network)
                        .collect();
                    self.store.save_enabled(&enabled)?;
                    self.adapters.remove(&network);
                    return Ok(());
                }
                if self.adapters.contains_key(&network) {
                    return Ok(());
                }
                let config = &self
                    .networks
                    .get(&network)
                    .context("Unknown network")?
                    .network;
                ensure!(
                    !self
                        .adapters
                        .keys()
                        .any(|id| self.networks[id].network.subnet == config.subnet),
                    "Another active network has the same subnet"
                );
                self.adapters
                    .insert(network, adapter::Adapter::open(config, &self.local)?);
                let enabled: Vec<_> = self.adapters.keys().copied().collect();
                if let Err(error) = self.store.save_enabled(&enabled) {
                    self.adapters.remove(&network);
                    return Err(error);
                }
            }
            Command::Revoke { network, steam_id } => {
                let mut next = self
                    .networks
                    .get(&network)
                    .context("Unknown network")?
                    .network
                    .clone();
                next.revoke(&self.local, &steam_id)?;
                let signed = SignedNetwork::sign(next, &self.key)?;
                self.store.save(&signed)?;
                self.networks.insert(network, signed);
            }
            Command::Readmit { network, steam_id } => {
                let mut next = self
                    .networks
                    .get(&network)
                    .context("Unknown network")?
                    .network
                    .clone();
                next.admit(&self.local, steam_id.parse()?)?;
                let signed = SignedNetwork::sign(next, &self.key)?;
                self.store.save(&signed)?;
                self.networks.insert(network, signed);
            }
            Command::Delete { network } => {
                ensure!(self.networks.contains_key(&network), "Unknown network");
                self.store.delete(network)?;
                if let Some((id, lobby)) = self.lobby {
                    if id == network {
                        self.client.matchmaking().leave_lobby(lobby);
                        self.lobby = None;
                    }
                }
                if let Some((_, lobby, _)) = self.pending.remove(&network) {
                    self.client.matchmaking().leave_lobby(lobby);
                }
                self.adapters.remove(&network);
                self.networks.remove(&network);
                self.restore.retain(|id| *id != network);
                self.sent_revisions.retain(|(_, id), _| *id != network);
                if self.queued_invite.is_some_and(|(id, _)| id == network) {
                    self.queued_invite = None;
                }
            }
            Command::Shutdown => {}
        }
        Ok(())
    }

    fn invite_friend(&self, lobby: LobbyId, peer: u64) -> Result<()> {
        // Safe wrapper doesn't currently expose InviteUserToLobby. The raw call
        // runs only on the initialized Steam worker, with validated integer IDs.
        let sent = unsafe {
            steamworks::sys::SteamAPI_ISteamMatchmaking_InviteUserToLobby(
                steamworks::sys::SteamAPI_SteamMatchmaking_v009(),
                lobby.raw(),
                peer,
            )
        };
        ensure!(sent, "Steam could not send the invitation");
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Created(network, result) => {
                self.creating = false;
                let lobby = match result {
                    Ok(lobby) => lobby,
                    Err(error) => {
                        self.queued_invite = None;
                        return Err(anyhow::Error::msg(error));
                    }
                };
                let mm = self.client.matchmaking();
                let Some(config) = self.networks.get(&network).map(|n| &n.network) else {
                    mm.leave_lobby(lobby);
                    return Ok(());
                };
                if !(mm.set_lobby_data(lobby, "freectier_protocol", "1")
                    && mm.set_lobby_data(lobby, "freectier_network", &network.to_string())
                    && mm.set_lobby_data(lobby, "name", &config.name))
                {
                    mm.leave_lobby(lobby);
                    anyhow::bail!("Cannot publish lobby metadata");
                }
                self.lobby = Some((network, lobby));
                if let Some((id, peer)) = self.queued_invite.take() {
                    ensure!(id == network, "Invitation network changed");
                    self.invite_friend(lobby, peer)?;
                } else {
                    self.client.friends().activate_invite_dialog(lobby);
                }
            }
            Event::JoinRequested(lobby) => self.join(lobby),
            Event::Joined(result) => {
                let lobby = result.map_err(anyhow::Error::msg)?;
                let mm = self.client.matchmaking();
                let parsed = (|| -> Result<(Uuid, u64)> {
                    ensure!(
                        mm.lobby_data(lobby, "freectier_protocol").as_deref() == Some("1"),
                        "Not a FreeC Tier lobby"
                    );
                    let id = mm
                        .lobby_data(lobby, "freectier_network")
                        .context("Missing Network ID")?
                        .parse()?;
                    let owner = mm.lobby_owner(lobby).raw();
                    Ok((id, owner))
                })();
                match parsed {
                    Ok((id, owner)) => {
                        self.pending.insert(id, (owner, lobby, Instant::now()));
                    }
                    Err(error) => {
                        mm.leave_lobby(lobby);
                        return Err(error);
                    }
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self, view: &mut Snapshot) -> Result<()> {
        self.client.run_callbacks();
        ensure!(
            self.client.user().logged_on()
                && self.client.user().steam_id().raw().to_string() == self.local,
            "Steam session lost"
        );
        // Restore once per Steam session. Failure stays visible instead of
        // opening repeated elevation/driver attempts in a tight loop.
        if !self.restore.is_empty() {
            let restore = std::mem::take(&mut self.restore);
            for id in restore {
                if let Some(config) = self.networks.get(&id) {
                    if config.network.member(&self.local).is_some() {
                        if self.adapters.keys().any(|active| {
                            self.networks[active].network.subnet == config.network.subnet
                        }) {
                            log(view, "Адаптер не восстановлен: конфликт подсетей");
                            continue;
                        }
                        match adapter::Adapter::open(&config.network, &self.local) {
                            Ok(adapter) => {
                                self.adapters.insert(id, adapter);
                            }
                            Err(error) => log(view, &format!("Адаптер не восстановлен: {error:#}")),
                        }
                    }
                }
            }
        }
        if self.refresh_configs.elapsed() >= Duration::from_secs(30) {
            self.sent_revisions.clear();
            self.refresh_configs = Instant::now();
        }
        while let Ok(event) = self.events_rx.try_recv() {
            if let Err(error) = self.event(event) {
                log(view, &format!("Steam: {error:#}"));
            }
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, (_, _, time))| time.elapsed() > Duration::from_secs(60))
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some((_, lobby, _)) = self.pending.remove(&id) {
                self.client.matchmaking().leave_lobby(lobby);
            }
            log(view, "Истекло время ожидания конфигурации от владельца");
        }
        // Poll ephemeral admission lobby. Steam identifies each joining member.
        if let Some((id, lobby)) = self.lobby {
            let mut next = self.networks[&id].network.clone();
            if self
                .client
                .matchmaking()
                .lobby_owner(lobby)
                .raw()
                .to_string()
                == self.local
            {
                for member in self.client.matchmaking().lobby_members(lobby) {
                    // Revoked reservations need an explicit re-admission action,
                    // not merely the continued presence of the old lobby member.
                    if !next
                        .members
                        .iter()
                        .any(|m| m.steam_id == member.raw().to_string())
                    {
                        next.admit(&self.local, member.raw())?;
                    }
                }
                if next.revision != self.networks[&id].network.revision {
                    let signed = SignedNetwork::sign(next, &self.key)?;
                    self.store.save(&signed)?;
                    self.networks.insert(id, signed);
                }
            }
        }
        let local: u64 = self.local.parse()?;
        let mut allowed = BTreeSet::new();
        let mut dial = BTreeSet::new();
        for signed in self.networks.values() {
            if signed.network.member(&self.local).is_none() {
                continue;
            }
            for member in signed
                .network
                .members
                .iter()
                .filter(|m| m.active && m.steam_id != self.local)
            {
                let id = member.steam_id.parse()?;
                allowed.insert(id);
                // Deterministic initiator avoids simultaneous duplicate sessions.
                if local < id {
                    dial.insert(id);
                }
            }
        }
        for (owner, _, _) in self.pending.values() {
            allowed.insert(*owner);
            if local < *owner {
                dial.insert(*owner);
            }
        }
        let messages = self.transport.tick(&self.client, &allowed, &dial);
        for (sender, bytes) in messages {
            if let Err(error) = self.receive(sender, &bytes, view) {
                view.dropped += 1;
                // Count malformed packets without flooding logs with untrusted text.
                if view.dropped.is_power_of_two() {
                    log(view, &format!("Отклонён пакет: {error:#}"));
                }
            }
        }
        self.sent_revisions
            .retain(|(peer, _), _| self.transport.connected(*peer));
        if self.last_sync.elapsed() >= Duration::from_secs(1) {
            self.last_sync = Instant::now();
            for (&id, signed) in &self.networks {
                if signed.network.member(&self.local).is_none() {
                    continue;
                }
                for member in signed
                    .network
                    .members
                    .iter()
                    .filter(|m| m.active && m.steam_id != self.local)
                {
                    let peer = member.steam_id.parse()?;
                    if self.transport.connected(peer)
                        && self.sent_revisions.get(&(peer, id)) != Some(&signed.network.revision)
                    {
                        let bytes = Frame::encode(Kind::Config, id, &serde_json::to_vec(signed)?)?;
                        if self.transport.send(peer, &bytes, true).is_ok() {
                            self.sent_revisions
                                .insert((peer, id), signed.network.revision);
                        }
                    }
                }
            }
        }
        for (&id, adapter) in &self.adapters {
            // Bounded batch per interface keeps callbacks and other networks alive.
            for _ in 0..64 {
                let Some(bytes) = adapter.receive()? else {
                    break;
                };
                let result = (|| -> Result<()> {
                    let packet = Packet::parse(&bytes)?;
                    let recipients = packet.outgoing(&self.networks[&id].network, &self.local)?;
                    let frame = Frame::encode(Kind::Ipv4, id, &bytes)?;
                    for peer in recipients {
                        if self
                            .transport
                            .send(peer.parse()?, &frame, packet.reliable())
                            .is_ok()
                        {
                            view.sent += 1;
                        } else {
                            view.dropped += 1;
                        }
                    }
                    Ok(())
                })();
                if result.is_err() {
                    view.dropped += 1;
                }
            }
        }
        if self.last_view.elapsed() >= Duration::from_millis(500) {
            self.last_view = Instant::now();
            self.update_view(view);
        }
        Ok(())
    }

    fn receive(&mut self, sender: u64, bytes: &[u8], view: &mut Snapshot) -> Result<()> {
        let frame = Frame::decode(bytes)?;
        let sender_id = sender.to_string();
        match frame.kind {
            Kind::Config => {
                let signed: SignedNetwork = serde_json::from_slice(frame.payload)?;
                ensure!(
                    signed.network.id == frame.network,
                    "Envelope Network ID mismatch"
                );
                if let Some(previous) = self.networks.get(&frame.network) {
                    ensure!(
                        previous.network.member(&sender_id).is_some(),
                        "Config sender not authorized"
                    );
                    if !signed.check_update(previous)? {
                        if let Some((_, lobby, _)) = self.pending.remove(&frame.network) {
                            self.client.matchmaking().leave_lobby(lobby);
                        }
                        return Ok(());
                    }
                } else {
                    let (owner, _, _) = self
                        .pending
                        .get(&frame.network)
                        .context("Unsolicited network configuration")?;
                    ensure!(
                        *owner == sender && signed.network.owner == sender_id,
                        "Invitation owner mismatch"
                    );
                    signed.verify()?;
                    ensure!(
                        signed.network.member(&self.local).is_some(),
                        "Not admitted by owner"
                    );
                }
                self.store.save(&signed)?;
                if signed.network.member(&self.local).is_none() {
                    self.adapters.remove(&frame.network);
                }
                self.networks.insert(frame.network, signed);
                if let Some((_, lobby, _)) = self.pending.remove(&frame.network) {
                    self.client.matchmaking().leave_lobby(lobby);
                }
                log(view, "Конфигурация сети получена и сохранена");
            }
            Kind::Ipv4 => {
                let config = &self
                    .networks
                    .get(&frame.network)
                    .context("Unknown network")?
                    .network;
                let packet = Packet::parse(frame.payload)?;
                packet.incoming(config, &sender_id, &self.local)?;
                self.adapters
                    .get(&frame.network)
                    .context("Virtual adapter is disabled")?
                    .inject(frame.payload)?;
                view.received += 1;
            }
        }
        Ok(())
    }

    fn update_view(&self, view: &mut Snapshot) {
        view.steam = "online".into();
        view.steam_id = Some(self.local.clone());
        view.nickname = Some(self.client.friends().name());
        view.relay = format!(
            "{:?}",
            self.client.networking_utils().relay_network_status()
        );
        view.friends = self
            .client
            .friends()
            .get_friends(FriendFlags::IMMEDIATE)
            .into_iter()
            .map(|f| FriendView {
                steam_id: f.id().raw().to_string(),
                name: f.name(),
            })
            .collect();
        view.networks = self
            .networks
            .values()
            .map(|signed| {
                let n = &signed.network;
                NetworkView {
                    id: n.id.to_string(),
                    name: n.name.clone(),
                    subnet: format!("{}/24", n.subnet),
                    owner: n.owner.clone(),
                    revision: n.revision,
                    adapter: self.adapters.contains_key(&n.id),
                    lobby: self
                        .lobby
                        .filter(|(id, _)| *id == n.id)
                        .map(|(_, lobby)| lobby.raw().to_string()),
                    members: n
                        .members
                        .iter()
                        .map(|m| PeerView {
                            steam_id: m.steam_id.clone(),
                            ip: m.ip.to_string(),
                            active: m.active,
                            name: self
                                .client
                                .friends()
                                .get_friend(SteamId::from_raw(m.steam_id.parse().unwrap_or(0)))
                                .name(),
                            ping_ms: self
                                .transport
                                .ping(&self.client, m.steam_id.parse().unwrap_or(0)),
                            state: if !m.active {
                                "removed"
                            } else if m.steam_id == self.local {
                                "local"
                            } else {
                                self.transport.state(m.steam_id.parse().unwrap_or(0))
                            }
                            .into(),
                        })
                        .collect(),
                }
            })
            .collect();
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some((_, lobby)) = self.lobby {
            self.client.matchmaking().leave_lobby(lobby);
        }
        for (_, lobby, _) in self.pending.values() {
            self.client.matchmaking().leave_lobby(*lobby);
        }
    }
}

fn log(view: &mut Snapshot, message: &str) {
    eprintln!("{message}");
    view.events.push(message.to_owned());
    if view.events.len() > 50 {
        view.events.remove(0);
    }
}

fn run(root: PathBuf, app_id: u32, rx: mpsc::Receiver<Command>, shared: Arc<Mutex<Snapshot>>) {
    let mut engine: Option<Engine> = None;
    let mut view = Snapshot {
        steam: "waiting".into(),
        relay: "unknown".into(),
        ..Default::default()
    };
    let mut retry = Instant::now();
    let args: Vec<_> = std::env::args().collect();
    let mut startup_lobby = args
        .windows(2)
        .find(|pair| pair[0] == "+connect_lobby")
        .and_then(|pair| pair[1].parse::<u64>().ok());
    loop {
        if engine.is_none() && Instant::now() >= retry {
            match Engine::new(&root, app_id) {
                Ok(next) => {
                    log(&mut view, "Steam подключён. Конфигурации загружены.");
                    if let Some(lobby) = startup_lobby.take() {
                        next.join(LobbyId::from_raw(lobby));
                    }
                    engine = Some(next);
                }
                Err(error) => {
                    log(&mut view, &format!("Ожидание Steam: {error:#}"));
                    retry = Instant::now() + Duration::from_secs(5);
                }
            }
        }
        match rx.try_recv() {
            Ok(Command::Shutdown) | Err(mpsc::TryRecvError::Disconnected) => break,
            Ok(command) => {
                let result = match engine.as_mut() {
                    Some(engine) => engine.command(command),
                    None => Err(anyhow::anyhow!(
                        "Для этой операции запустите Steam и войдите в аккаунт"
                    )),
                };
                if let Err(error) = result {
                    log(&mut view, &format!("Ошибка: {error:#}"));
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some(current) = engine.as_mut() {
            if let Err(error) = current.tick(&mut view) {
                log(&mut view, &format!("Сессия приостановлена: {error:#}"));
                engine = None;
                view.steam = "waiting".into();
                view.relay = "unknown".into();
                for network in &mut view.networks {
                    network.adapter = false;
                    network.lobby = None;
                    for peer in &mut network.members {
                        peer.state = "offline".into();
                    }
                }
                retry = Instant::now() + Duration::from_secs(5);
            }
        }
        *shared.lock().unwrap_or_else(|p| p.into_inner()) = view.clone();
        thread::sleep(Duration::from_millis(5));
    }
}
