mod adapter;
mod steam;

pub const DEFAULT_APP_ID: u32 = 324810;

use anyhow::{ensure, Context, Result};
use ed25519_dalek::SigningKey;
use freec_core::{
    admission::{self, Password},
    config::{Access, Network, Operation, SignedNetwork},
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
    CallbackHandle, Client, FriendFlags, GameLobbyJoinRequested, LobbyId, LobbyType,
    SteamAPIInitError, SteamId,
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
    pub joins: Vec<JoinView>,
    pub public_networks: Vec<PublicView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JoinView {
    pub id: String,
    pub name: String,
    pub password: bool,
}
#[derive(Debug, Clone, Serialize)]
pub struct PublicView {
    pub lobby: String,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinRequest {
    public_key: String,
    snapshot: String,
    proof: Option<String>,
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
    /// Stored member preference, including while the network is private.
    pub can_invite: bool,
    /// Effective permission after applying access mode and active membership.
    pub may_invite: bool,
    pub can_kick: bool,
    pub may_kick: bool,
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
    /// True when access is public.
    pub public: bool,
    /// Whether the local client may invite for this network.
    pub can_invite: bool,
    pub password: bool,
    pub members: Vec<PeerView>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    Create {
        name: String,
        #[serde(default)]
        public: bool,
        #[serde(default)]
        password: String,
    },
    Invite {
        network: Uuid,
    },
    InviteFriend {
        network: Uuid,
        steam_id: String,
    },
    Join {
        lobby: String,
    },
    SetAdapter {
        network: Uuid,
        enabled: bool,
    },
    Revoke {
        network: Uuid,
        steam_id: String,
    },
    Readmit {
        network: Uuid,
        steam_id: String,
    },
    SetAccess {
        network: Uuid,
        public: bool,
    },
    SetMemberInvite {
        network: Uuid,
        steam_id: String,
        can_invite: bool,
    },
    SetPermissions {
        network: Uuid,
        steam_id: String,
        can_invite: bool,
        can_kick: bool,
    },
    SetPassword {
        network: Uuid,
        password: String,
    },
    SubmitPassword {
        network: Uuid,
        password: String,
    },
    CancelJoin {
        network: Uuid,
    },
    Discover,
    Delete {
        network: Uuid,
    },
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
    Found(std::result::Result<Vec<LobbyId>, String>),
    Published(Uuid, std::result::Result<LobbyId, String>),
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
    /// Last surfaced service-connect issue, to avoid event-log spam.
    last_service_issue: Option<String>,
    /// Last surfaced outgoing-packet error, to avoid event-log spam.
    last_outgoing_error: Option<String>,
    refresh_configs: Instant,
    /// Cooldown for processing admission Join frames, keyed by applicant. Each
    /// accepted frame costs a signature and a store write, so the sender-side
    /// retry gates alone must not bound the owner's work.
    join_cooldown: BTreeMap<(Uuid, u64), Instant>,
    offers: BTreeMap<Uuid, (u64, SignedNetwork)>,
    pins: BTreeMap<Uuid, String>,
    public_networks: Vec<PublicView>,
    last_hello: Instant,
    published: BTreeMap<Uuid, LobbyId>,
    publishing: Option<Uuid>,
    publish_retry: Instant,
    join_sent: BTreeMap<Uuid, (String, Instant)>,
    grace: BTreeMap<u64, Instant>,
    guests: BTreeMap<(Uuid, u64), Instant>,
}

impl Engine {
    fn new(root: &std::path::Path, app_id: u32) -> Result<Self> {
        let client = Client::init_app(app_id)?;
        ensure!(
            client.user().logged_on(),
            "Steam не авторизован — войдите в аккаунт"
        );
        let local = client.user().steam_id().raw().to_string();
        let store = Store::new(root.join("accounts").join(&local))?;
        let key = store.owner_key()?;
        let mut networks: BTreeMap<_, _> = store
            .load()?
            .into_iter()
            .map(|n| (n.network.id, n))
            .collect();
        for signed in networks.values_mut() {
            if signed.network.owner == local {
                let mut next = signed.network.clone();
                if signed.delegation.is_some() {
                    next.revision += 1;
                }
                next.upgrade(&local)?;
                next.bind_key(&local, &local, hex::encode(key.verifying_key().as_bytes()))?;
                if next.revision == signed.network.revision && signed.delegation.is_none() {
                    continue;
                }
                *signed = SignedNetwork::sign(next, &key)?;
                store.save(signed)?;
            }
        }
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
            last_service_issue: None,
            last_outgoing_error: None,
            refresh_configs: Instant::now(),
            join_cooldown: BTreeMap::new(),
            offers: BTreeMap::new(),
            pins: BTreeMap::new(),
            public_networks: Vec::new(),
            last_hello: Instant::now() - Duration::from_secs(10),
            published: BTreeMap::new(),
            publishing: None,
            publish_retry: Instant::now() - Duration::from_secs(30),
            join_sent: BTreeMap::new(),
            grace: BTreeMap::new(),
            guests: BTreeMap::new(),
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

    fn admission_lobby(&self, id: Uuid) -> Option<LobbyId> {
        self.published.get(&id).copied().or_else(|| {
            self.lobby
                .filter(|(network, _)| *network == id)
                .map(|(_, lobby)| lobby)
        })
    }

    fn applicant_in_lobby(&self, id: Uuid, peer: u64) -> bool {
        if self
            .guests
            .get(&(id, peer))
            .is_some_and(|time| time.elapsed() < Duration::from_secs(20))
        {
            return true;
        }
        self.admission_lobby(id).is_some_and(|lobby| {
            self.client
                .matchmaking()
                .lobby_members(lobby)
                .iter()
                .any(|m| m.raw() == peer)
        })
    }

    fn publish_metadata(&self, id: Uuid, lobby: LobbyId) -> Result<()> {
        let config = &self.networks.get(&id).context("Unknown network")?.network;
        let mm = self.client.matchmaking();
        ensure!(
            mm.set_lobby_data(lobby, "freectier_protocol", "2")
                && mm.set_lobby_data(lobby, "freectier_network", &id.to_string())
                && mm.set_lobby_data(lobby, "freectier_owner", &config.owner)
                && mm.set_lobby_data(lobby, "freectier_key", &config.owner_key)
                && mm.set_lobby_data(
                    lobby,
                    "freectier_public",
                    if config.access == Access::Public {
                        "1"
                    } else {
                        "0"
                    }
                )
                && mm.set_lobby_data(lobby, "name", &config.name),
            "Cannot publish network"
        );
        Ok(())
    }

    /// Networks turn their adapter on by default: creation, first admission and
    /// re-admission queue the request here. It is persisted so restarts keep the
    /// network enabled, and executed by the tick's restore pass, which reports
    /// subnet conflicts in the event log.
    fn request_auto_enable(&mut self, network: Uuid) -> Result<()> {
        let mut enabled: Vec<_> = self.store.load_enabled()?;
        if !enabled.contains(&network) {
            enabled.push(network);
            self.store.save_enabled(&enabled)?;
        }
        if !self.restore.contains(&network) {
            self.restore.push(network);
        }
        Ok(())
    }

    fn save_network(&mut self, signed: SignedNetwork) -> Result<()> {
        self.store.save(&signed)?;
        let id = signed.network.id;
        if let Some(previous) = self.networks.get(&id) {
            for member in previous
                .network
                .members
                .iter()
                .filter(|m| m.active && signed.network.member(&m.steam_id).is_none())
            {
                let peer = member.steam_id.parse()?;
                self.grace.insert(peer, Instant::now());
                let _ = self.transport.send(
                    peer,
                    &Frame::encode(Kind::Config, id, &serde_json::to_vec(&signed)?)?,
                    true,
                );
            }
        }
        if signed.network.member(&self.local).is_none() {
            self.adapters.remove(&id);
        }
        self.networks.insert(id, signed);
        Ok(())
    }

    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Create {
                name,
                public,
                password,
            } => {
                let index = (0..=255)
                    .find(|index| {
                        !self
                            .networks
                            .values()
                            .any(|n| n.network.subnet.octets()[2] == *index)
                    })
                    .context("No free subnet")?;
                let mut config = Network::new(name, self.local.parse()?, index, &self.key)?;
                if public {
                    config.set_access(&self.local, Access::Public)?;
                }
                if !password.is_empty() {
                    config.set_password(&self.local, Some(Password::new(&password)?))?;
                }
                let network = SignedNetwork::sign(config, &self.key)?;
                self.store.save(&network)?;
                let id = network.network.id;
                self.networks.insert(id, network);
                self.request_auto_enable(id)?;
                self.command(Command::Invite { network: id })?;
            }
            Command::Invite { network } => {
                let can = self
                    .networks
                    .get(&network)
                    .context("Unknown network")?
                    .network
                    .may_invite(&self.local);
                ensure!(can, "You are not allowed to invite to this network");
                if let Some(lobby) = self.published.get(&network) {
                    self.client.friends().activate_invite_dialog(*lobby);
                    return Ok(());
                }
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
                self.client.matchmaking().create_lobby(
                    if self.networks[&network].network.access == Access::Public {
                        LobbyType::Public
                    } else {
                        LobbyType::Private
                    },
                    32,
                    move |result| {
                        let _ = tx.send(Event::Created(network, result.map_err(|e| e.to_string())));
                    },
                );
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
                        .may_invite(&self.local),
                    "You are not allowed to invite to this network"
                );
                if let Some(lobby) = self.admission_lobby(network) {
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
                let current = self.networks.get(&network).context("Unknown network")?;
                let signed = if current.network.owner == self.local {
                    let mut next = current.network.clone();
                    next.revoke(&self.local, &steam_id)?;
                    SignedNetwork::sign(next, &self.key)?
                } else {
                    current.delegate(&self.local, Operation::Revoke { steam_id }, &self.key)?
                };
                self.save_network(signed)?;
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
                // Restore the adapter only if the user kept this network enabled.
                if self.store.load_enabled()?.contains(&network) && !self.restore.contains(&network)
                {
                    self.restore.push(network);
                }
            }
            Command::SetAccess { network, public } => {
                let mut next = self
                    .networks
                    .get(&network)
                    .context("Unknown network")?
                    .network
                    .clone();
                next.set_access(
                    &self.local,
                    if public {
                        Access::Public
                    } else {
                        Access::Private
                    },
                )?;
                let signed = SignedNetwork::sign(next, &self.key)?;
                self.store.save(&signed)?;
                self.networks.insert(network, signed);
            }
            Command::SetMemberInvite {
                network,
                steam_id,
                can_invite,
            } => {
                let mut next = self
                    .networks
                    .get(&network)
                    .context("Unknown network")?
                    .network
                    .clone();
                next.set_member_invite(&self.local, &steam_id, can_invite)?;
                let signed = SignedNetwork::sign(next, &self.key)?;
                self.store.save(&signed)?;
                self.networks.insert(network, signed);
            }
            Command::Delete { network } => {
                if let Some(lobby) = self.published.remove(&network) {
                    self.client.matchmaking().leave_lobby(lobby);
                }
                self.join_sent.remove(&network);
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
                self.offers.remove(&network);
                self.pins.remove(&network);
                self.restore.retain(|id| *id != network);
                self.sent_revisions.retain(|(_, id), _| *id != network);
                self.join_cooldown.retain(|(id, _), _| *id != network);
                if self.queued_invite.is_some_and(|(id, _)| id == network) {
                    self.queued_invite = None;
                }
            }
            Command::Shutdown => {}
            Command::SetPermissions {
                network,
                steam_id,
                can_invite,
                can_kick,
            } => {
                let mut next = self
                    .networks
                    .get(&network)
                    .context("Unknown network")?
                    .network
                    .clone();
                next.set_permissions(&self.local, &steam_id, can_invite, can_kick)?;
                let signed = SignedNetwork::sign(next, &self.key)?;
                self.store.save(&signed)?;
                self.networks.insert(network, signed);
            }
            Command::SetPassword { network, password } => {
                let mut next = self
                    .networks
                    .get(&network)
                    .context("Unknown network")?
                    .network
                    .clone();
                ensure!(next.owner == self.local, "Only owner can change password");
                next.set_password(
                    &self.local,
                    if password.is_empty() {
                        None
                    } else {
                        Some(Password::new(&password)?)
                    },
                )?;
                let signed = SignedNetwork::sign(next, &self.key)?;
                self.store.save(&signed)?;
                self.networks.insert(network, signed);
            }
            Command::SubmitPassword { network, password } => {
                self.request_join(network, &password)?
            }
            Command::CancelJoin { network } => {
                self.offers.remove(&network);
                self.join_sent.remove(&network);
                self.pins.remove(&network);
                if let Some((_, lobby, _)) = self.pending.remove(&network) {
                    self.client.matchmaking().leave_lobby(lobby);
                }
            }
            Command::Discover => {
                let tx = self.events_tx.clone();
                let mm = self.client.matchmaking();
                mm.add_request_lobby_list_string_filter(steamworks::StringFilter(
                    steamworks::LobbyKey::try_new("freectier_protocol")?,
                    "2",
                    steamworks::StringFilterKind::Equal,
                ));
                mm.add_request_lobby_list_string_filter(steamworks::StringFilter(
                    steamworks::LobbyKey::try_new("freectier_public")?,
                    "1",
                    steamworks::StringFilterKind::Equal,
                ));
                mm.set_request_lobby_list_distance_filter(steamworks::DistanceFilter::Worldwide);
                mm.request_lobby_list(move |result| {
                    let _ = tx.send(Event::Found(result.map_err(|e| e.to_string())));
                });
            }
        }
        Ok(())
    }

    fn request_join(&mut self, network: Uuid, password: &str) -> Result<()> {
        ensure!(
            self.pending
                .get(&network)
                .is_some_and(|(_, _, time)| time.elapsed() >= Duration::from_secs(3)),
            "Ожидаем ответ владельца; повторите через несколько секунд"
        );
        let (peer, signed) = self.offers.get(&network).context("No pending invitation")?;
        let public_key = hex::encode(self.key.verifying_key().as_bytes());
        let snapshot = admission::fingerprint(signed)?;
        let proof = signed
            .network
            .password
            .as_ref()
            .map(|p| p.prove(password, &snapshot, &self.local, &public_key))
            .transpose()?;
        let request = JoinRequest {
            public_key,
            snapshot,
            proof,
        };
        self.transport.send(
            *peer,
            &Frame::encode(Kind::Join, network, &serde_json::to_vec(&request)?)?,
            true,
        )?;
        self.join_sent
            .insert(network, (request.snapshot, Instant::now()));
        if let Some((_, _, time)) = self.pending.get_mut(&network) {
            *time = Instant::now();
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
                if config.owner != self.local && !config.may_invite(&self.local) {
                    mm.leave_lobby(lobby);
                    self.queued_invite = None;
                    anyhow::bail!("Invitation permission was revoked");
                }
                if !(mm.set_lobby_data(lobby, "freectier_protocol", "2")
                    && mm.set_lobby_data(lobby, "freectier_network", &network.to_string())
                    && mm.set_lobby_data(lobby, "freectier_owner", &config.owner)
                    && mm.set_lobby_data(lobby, "freectier_key", &config.owner_key)
                    && mm.set_lobby_data(
                        lobby,
                        "freectier_public",
                        if config.access == Access::Public {
                            "1"
                        } else {
                            "0"
                        },
                    )
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
                        mm.lobby_data(lobby, "freectier_protocol").as_deref() == Some("2"),
                        "Not a FreeC Tier lobby"
                    );
                    let id = mm
                        .lobby_data(lobby, "freectier_network")
                        .context("Missing Network ID")?
                        .parse()?;
                    let pin = mm
                        .lobby_data(lobby, "freectier_key")
                        .context("Missing owner key")?;
                    admission::public_key(&pin)?;
                    if let Some(known) = self.networks.get(&id) {
                        ensure!(known.network.owner_key == pin, "Owner key changed");
                    }
                    self.pins.insert(id, pin);
                    let owner = mm
                        .lobby_data(lobby, "freectier_owner")
                        .map(|value| value.parse::<u64>())
                        .transpose()?
                        .unwrap_or_else(|| mm.lobby_owner(lobby).raw());
                    ensure!(owner > 0, "Invalid network owner");
                    if let Some(known) = self.networks.get(&id) {
                        ensure!(
                            known.network.owner == owner.to_string(),
                            "Network owner changed"
                        );
                    }
                    Ok((id, owner))
                })();
                match parsed {
                    Ok((id, owner)) => {
                        if let Some((_, old, _)) = self.pending.remove(&id) {
                            if old != lobby {
                                mm.leave_lobby(old);
                            }
                        }
                        self.offers.remove(&id);
                        self.join_sent.remove(&id);
                        self.pending.insert(id, (owner, lobby, Instant::now()));
                    }
                    Err(error) => {
                        mm.leave_lobby(lobby);
                        return Err(error);
                    }
                }
            }
            Event::Found(result) => {
                let mm = self.client.matchmaking();
                self.public_networks = result
                    .map_err(anyhow::Error::msg)?
                    .into_iter()
                    .filter(|lobby| {
                        mm.lobby_data(*lobby, "freectier_protocol").as_deref() == Some("2")
                            && mm.lobby_data(*lobby, "freectier_public").as_deref() == Some("1")
                    })
                    .map(|lobby| PublicView {
                        lobby: lobby.raw().to_string(),
                        name: mm.lobby_data(lobby, "name").unwrap_or_default(),
                    })
                    .collect();
            }
            Event::Published(id, result) => {
                self.publishing = None;
                let lobby = result.map_err(anyhow::Error::msg)?;
                if self.networks.get(&id).is_some_and(|n| {
                    n.network.access == Access::Public && n.network.may_invite(&self.local)
                }) {
                    if let Err(error) = self.publish_metadata(id, lobby) {
                        self.client.matchmaking().leave_lobby(lobby);
                        return Err(error);
                    }
                    self.published.insert(id, lobby);
                } else {
                    self.client.matchmaking().leave_lobby(lobby);
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
                if self.adapters.contains_key(&id) {
                    // A manual enable won the race; keep its state.
                    continue;
                }
                if let Some(config) = self.networks.get(&id) {
                    if config.network.member(&self.local).is_some() {
                        if self.adapters.keys().any(|active| {
                            self.networks[active].network.subnet == config.network.subnet
                        }) {
                            log(view, "Сеть не включилась автоматически: конфликт подсетей");
                            continue;
                        }
                        match adapter::Adapter::open(&config.network, &self.local) {
                            Ok(adapter) => {
                                self.adapters.insert(id, adapter);
                                log(view, "Сеть включена автоматически");
                            }
                            Err(error) => log(
                                view,
                                &format!("Сеть не включилась автоматически: {error:#}"),
                            ),
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
            self.offers.remove(&id);
            self.pins.remove(&id);
            self.join_sent.remove(&id);
            if let Some((_, lobby, _)) = self.pending.remove(&id) {
                self.client.matchmaking().leave_lobby(lobby);
            }
            log(view, "Истекло время ожидания подключения к сети");
        }
        let stale: Vec<_> = self
            .published
            .keys()
            .copied()
            .filter(|id| {
                !self.networks.get(id).is_some_and(|n| {
                    n.network.access == Access::Public && n.network.may_invite(&self.local)
                })
            })
            .collect();
        for id in stale {
            if let Some(lobby) = self.published.remove(&id) {
                self.client.matchmaking().leave_lobby(lobby);
            }
        }
        if self.publishing.is_none() && self.publish_retry.elapsed() >= Duration::from_secs(10) {
            if let Some((&id, _)) = self.networks.iter().find(|(id, n)| {
                n.network.access == Access::Public
                    && n.network.may_invite(&self.local)
                    && n.network
                        .member(&self.local)
                        .is_some_and(|m| m.public_key.is_some())
                    && !self.published.contains_key(id)
                    && !self.lobby.is_some_and(|(existing, _)| existing == **id)
            }) {
                self.publishing = Some(id);
                self.publish_retry = Instant::now();
                let tx = self.events_tx.clone();
                self.client
                    .matchmaking()
                    .create_lobby(LobbyType::Public, 32, move |result| {
                        let _ = tx.send(Event::Published(id, result.map_err(|e| e.to_string())));
                    });
            }
        }
        // Lobby membership only opens the control channel, never the LAN.
        self.join_cooldown
            .retain(|_, time| time.elapsed() < Duration::from_secs(30));
        if let Some((id, lobby)) = self.lobby {
            if let Some(config) = self.networks.get(&id).map(|s| s.network.clone()) {
                let may_invite = config.may_invite(&self.local);
                if !may_invite {
                    self.client.matchmaking().leave_lobby(lobby);
                    self.lobby = None;
                }
                if may_invite {
                    let mm = self.client.matchmaking();
                    mm.set_lobby_data(
                        lobby,
                        "freectier_public",
                        if config.access == Access::Public {
                            "1"
                        } else {
                            "0"
                        },
                    );
                    // Steam has no safe wrapper for SetLobbyType in this crate.
                    unsafe {
                        steamworks::sys::SteamAPI_ISteamMatchmaking_SetLobbyType(
                            steamworks::sys::SteamAPI_SteamMatchmaking_v009(),
                            lobby.raw(),
                            if config.access == Access::Public {
                                steamworks::sys::ELobbyType::k_ELobbyTypePublic
                            } else {
                                steamworks::sys::ELobbyType::k_ELobbyTypePrivate
                            },
                        );
                    }
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
        for (owner, lobby, _) in self.pending.values() {
            allowed.insert(*owner);
            if local < *owner {
                dial.insert(*owner);
            }
            for member in self.client.matchmaking().lobby_members(*lobby) {
                if member.raw() != local {
                    allowed.insert(member.raw());
                    if local < member.raw() {
                        dial.insert(member.raw());
                    }
                }
            }
        }
        if let Some((_, lobby)) = self.lobby {
            for member in self.client.matchmaking().lobby_members(lobby) {
                if member.raw() != local {
                    allowed.insert(member.raw());
                    if local < member.raw() {
                        dial.insert(member.raw());
                    }
                }
            }
        }
        for lobby in self.published.values() {
            for member in self.client.matchmaking().lobby_members(*lobby) {
                if member.raw() != local {
                    allowed.insert(member.raw());
                    if local < member.raw() {
                        dial.insert(member.raw());
                    }
                }
            }
        }
        self.grace
            .retain(|_, time| time.elapsed() < Duration::from_secs(5));
        allowed.extend(self.grace.keys().copied());
        self.guests
            .retain(|_, time| time.elapsed() < Duration::from_secs(20));
        for (_, peer) in self.guests.keys() {
            allowed.insert(*peer);
            if local < *peer {
                dial.insert(*peer);
            }
        }
        if let Some(issue) = adapter::current_issue() {
            if self.last_service_issue.as_deref() != Some(issue.as_str()) {
                log(view, &format!("Служба: {issue}"));
                self.last_service_issue = Some(issue);
            }
        } else if self.last_service_issue.take().is_some() {
            log(view, "Служба: подключение восстановлено");
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
        if self.last_hello.elapsed() >= Duration::from_secs(3) {
            self.last_hello = Instant::now();
            let key = hex::encode(self.key.verifying_key().as_bytes());
            for (&id, (owner, lobby, _)) in &self.pending {
                let mut peers: BTreeSet<_> = self
                    .client
                    .matchmaking()
                    .lobby_members(*lobby)
                    .into_iter()
                    .map(|m| m.raw())
                    .collect();
                peers.insert(*owner);
                for peer in peers {
                    if peer != local && self.transport.connected(peer) {
                        let _ = self.transport.send(
                            peer,
                            &Frame::encode(Kind::Hello, id, key.as_bytes())?,
                            true,
                        );
                    }
                }
            }
            for (&id, signed) in &self.networks {
                if signed
                    .network
                    .member(&self.local)
                    .is_some_and(|m| m.public_key.is_none())
                {
                    let owner = signed.network.owner.parse()?;
                    if self.transport.connected(owner) {
                        let _ = self.transport.send(
                            owner,
                            &Frame::encode(Kind::Hello, id, key.as_bytes())?,
                            true,
                        );
                    }
                }
            }
        }
        let auto_join: Vec<_> = self
            .offers
            .iter()
            .filter(|(id, (_, offer))| {
                offer.network.password.is_none()
                    && self
                        .pending
                        .get(id)
                        .is_some_and(|(_, _, time)| time.elapsed() >= Duration::from_secs(3))
                    && self
                        .join_sent
                        .get(id)
                        .is_none_or(|(_, time)| time.elapsed() >= Duration::from_secs(5))
            })
            .map(|(id, _)| *id)
            .collect();
        for id in auto_join {
            if let Err(error) = self.request_join(id, "") {
                log(view, &format!("Подключение: {error:#}"));
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
                    // The stack's own chatter (IGMP reports, IPv6, etc.) is
                    // not an error — skip it quietly.
                    let packet = match Packet::parse(&bytes) {
                        Ok(packet) => packet,
                        Err(_) => return Ok(()),
                    };
                    // Self-destined packets (e.g. a ping to the VPN IP) come
                    // back through the ring: loop them straight into the
                    // adapter so the local stack sees them, instead of
                    // tunneling a packet to ourselves (the incoming path
                    // rejects sender == local by design).
                    let local_ip = self.networks[&id]
                        .network
                        .member(&self.local)
                        .context("Local identity is not a member")?
                        .ip;
                    if packet.destination == local_ip {
                        adapter.inject(&bytes)?;
                        return Ok(());
                    }
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
                if let Err(error) = result {
                    view.dropped += 1;
                    let message = format!("{error:#}");
                    if self.last_outgoing_error.as_deref() != Some(message.as_str()) {
                        log(view, &format!("Адаптер: {message}"));
                        self.last_outgoing_error = Some(message);
                    }
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
                    let (owner, lobby, _) = self
                        .pending
                        .get(&frame.network)
                        .context("Unsolicited network configuration")?;
                    ensure!(
                        signed.network.owner == owner.to_string()
                            && (sender == *owner
                                || self
                                    .client
                                    .matchmaking()
                                    .lobby_members(*lobby)
                                    .iter()
                                    .any(|m| m.raw() == sender)),
                        "Invitation owner mismatch"
                    );
                    signed.verify()?;
                    ensure!(
                        self.pins.get(&frame.network) == Some(&signed.network.owner_key),
                        "Owner key mismatch"
                    );
                    let offer = &self
                        .offers
                        .get(&frame.network)
                        .context("Admission without offer")?
                        .1;
                    ensure!(
                        signed.check_update(offer)?,
                        "Admission must extend offered policy"
                    );
                    ensure!(
                        signed
                            .network
                            .member(&self.local)
                            .and_then(|m| m.public_key.as_deref())
                            == Some(hex::encode(self.key.verifying_key().as_bytes()).as_str()),
                        "Applicant key changed"
                    );
                    ensure!(
                        signed.network.member(&self.local).is_some(),
                        "Not admitted by owner"
                    );
                }
                let signed = if signed.network.owner == self.local && signed.delegation.is_some() {
                    let mut next = signed.network;
                    next.revision += 1;
                    SignedNetwork::sign(next, &self.key)?
                } else {
                    signed
                };
                self.save_network(signed)?;
                // First admission: the network joins enabled by default.
                self.request_auto_enable(frame.network)?;
                self.offers.remove(&frame.network);
                self.join_sent.remove(&frame.network);
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
            Kind::Request => {
                let peer = frame.invited_steam_id()?;
                let config = &self
                    .networks
                    .get(&frame.network)
                    .context("Unknown network")?
                    .network;
                ensure!(
                    config.owner == self.local
                        && config.member(&sender_id).is_some()
                        && config.may_invite(&sender_id),
                    "Unauthorized owner consultation"
                );
                ensure!(
                    self.guests.contains_key(&(frame.network, peer)) || self.guests.len() < 64,
                    "Too many pending admissions"
                );
                self.guests.insert((frame.network, peer), Instant::now());
            }
            Kind::Hello => {
                let key = std::str::from_utf8(frame.payload)?.to_owned();
                admission::public_key(&key)?;
                if let Some(signed) = self.networks.get(&frame.network) {
                    if signed
                        .network
                        .member(&sender_id)
                        .is_some_and(|m| m.public_key.is_none())
                        && signed.network.owner == self.local
                    {
                        let mut next = signed.network.clone();
                        next.bind_key(&self.local, &sender_id, key)?;
                        let signed = SignedNetwork::sign(next, &self.key)?;
                        self.store.save(&signed)?;
                        self.networks.insert(frame.network, signed);
                    }
                }
                let signed = self
                    .networks
                    .get(&frame.network)
                    .context("Unknown network")?;
                ensure!(
                    signed.network.member(&self.local).is_some(),
                    "Local access revoked"
                );
                let in_lobby = self.applicant_in_lobby(frame.network, sender);
                ensure!(
                    in_lobby || signed.network.member(&sender_id).is_some(),
                    "Unsolicited hello"
                );
                if in_lobby
                    && signed.network.owner != self.local
                    && signed.network.member(&sender_id).is_none()
                    && signed.network.may_invite(&self.local)
                {
                    let owner = signed.network.owner.parse()?;
                    if self.transport.connected(owner) {
                        let _ = self.transport.send(
                            owner,
                            &Frame::encode(Kind::Request, frame.network, sender_id.as_bytes())?,
                            true,
                        );
                    }
                }
                if signed.network.member(&sender_id).is_some() {
                    self.transport.send(
                        sender,
                        &Frame::encode(Kind::Config, frame.network, &serde_json::to_vec(signed)?)?,
                        true,
                    )?;
                } else if signed.network.may_invite(&self.local) {
                    // Only inviters hand out offers, so a lobby of a member whose
                    // invite flag was revoked is never an admission channel.
                    self.transport.send(
                        sender,
                        &Frame::encode(Kind::Offer, frame.network, &serde_json::to_vec(signed)?)?,
                        true,
                    )?;
                }
            }
            Kind::Join => {
                ensure!(
                    self.join_cooldown
                        .get(&(frame.network, sender))
                        .is_none_or(|time| time.elapsed() >= Duration::from_secs(2)),
                    "Admission request flood"
                );
                self.join_cooldown
                    .insert((frame.network, sender), Instant::now());
                let request: JoinRequest = serde_json::from_slice(frame.payload)?;
                let signed = self
                    .networks
                    .get(&frame.network)
                    .context("Unknown network")?;
                ensure!(
                    self.applicant_in_lobby(frame.network, sender),
                    "Applicant not in admission lobby"
                );
                ensure!(
                    admission::fingerprint(signed)? == request.snapshot,
                    "Stale network snapshot"
                );
                let mut next = signed.delegate(
                    &self.local,
                    Operation::Admit {
                        steam_id: sender_id.clone(),
                        public_key: request.public_key,
                        proof: request.proof,
                    },
                    &self.key,
                )?;
                if next.network.owner == self.local {
                    next = SignedNetwork::sign(next.network, &self.key)?;
                }
                self.store.save(&next)?;
                self.networks.insert(frame.network, next.clone());
                self.transport.send(
                    sender,
                    &Frame::encode(Kind::Config, frame.network, &serde_json::to_vec(&next)?)?,
                    true,
                )?;
                log(view, "Участник допущен автоматически");
            }
            Kind::Offer => {
                let signed: SignedNetwork = serde_json::from_slice(frame.payload)?;
                signed.verify()?;
                let (owner, lobby, _) = self
                    .pending
                    .get(&frame.network)
                    .context("Unsolicited offer")?;
                ensure!(
                    signed.network.id == frame.network && signed.network.owner == owner.to_string(),
                    "Wrong network offer"
                );
                ensure!(
                    self.pins.get(&frame.network) == Some(&signed.network.owner_key),
                    "Owner key mismatch"
                );
                ensure!(
                    sender == *owner
                        || self
                            .client
                            .matchmaking()
                            .lobby_members(*lobby)
                            .iter()
                            .any(|m| m.raw() == sender),
                    "Offer from outside lobby"
                );
                ensure!(
                    signed.network.member(&sender_id).is_some(),
                    "Offer from nonmember"
                );
                ensure!(
                    signed.network.may_invite(&sender_id),
                    "Inviter lacks permission"
                );
                if let Some((old_peer, old)) = self.offers.get(&frame.network) {
                    ensure!(
                        signed.network.revision >= old.network.revision,
                        "Stale password policy"
                    );
                    if signed.network.revision == old.network.revision {
                        ensure!(
                            admission::fingerprint(&signed)? == admission::fingerprint(old)?,
                            "Conflicting signed configurations"
                        );
                        if (*old_peer == *owner && sender != *owner)
                            || (sender != *owner
                                && *old_peer < sender
                                && self.transport.connected(*old_peer))
                        {
                            return Ok(());
                        }
                    }
                }
                self.offers.insert(frame.network, (sender, signed));
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
                        .admission_lobby(n.id)
                        .map(|lobby| lobby.raw().to_string()),
                    public: n.access == Access::Public,
                    can_invite: n.may_invite(&self.local),
                    password: n.password.is_some(),
                    members: n
                        .members
                        .iter()
                        .map(|m| PeerView {
                            steam_id: m.steam_id.clone(),
                            ip: m.ip.to_string(),
                            active: m.active,
                            can_invite: m.steam_id == n.owner || m.can_invite,
                            may_invite: n.may_invite(&m.steam_id),
                            can_kick: m.can_kick,
                            may_kick: n.may_kick(&self.local, &m.steam_id),
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
        view.joins = self
            .offers
            .values()
            .map(|(_, n)| JoinView {
                id: n.network.id.to_string(),
                name: n.network.name.clone(),
                password: n.network.password.is_some(),
            })
            .collect();
        view.public_networks = self.public_networks.clone();
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
        for lobby in self.published.values() {
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

/// Steam's init path fills a detailed `SteamErrMsg`, but the crate's
/// `Display` shows only static text ("Some other failure") and drops it —
/// pull the payload back out so the event log shows the real reason.
fn describe_steam_error(error: &anyhow::Error) -> String {
    for cause in error.chain() {
        if let Some(init) = cause.downcast_ref::<SteamAPIInitError>() {
            let detail = match init {
                SteamAPIInitError::FailedGeneric(message)
                | SteamAPIInitError::NoSteamClient(message)
                | SteamAPIInitError::VersionMismatch(message) => message.clone(),
            };
            if !detail.trim().is_empty() {
                return detail;
            }
        }
    }
    format!("{error:#}")
}

/// Actionable follow-up for Steam init failures we recognize. Steam's own
/// detail ("ConnectToGlobalUser failed.") alone doesn't tell the user what
/// to do; the two known causes are an elevated app process (Steam refuses
/// API connections across different integrity levels) and a stuck client.
fn wait_hint(detail: &str, elevated: bool) -> &'static str {
    if detail.contains("ConnectToGlobalUser") {
        if elevated {
            " Приложение запущено от имени администратора — Steam не подключается к таким процессам. Перезапустите FreeC Tier обычным способом."
        } else {
            " Steam открыт, но не принимает подключение приложения. Перезапустите Steam и дождитесь входа в аккаунт."
        }
    } else {
        ""
    }
}

/// Whether this process runs with an elevated token.
#[cfg(windows)]
pub fn process_elevated() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = Default::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(windows))]
pub fn process_elevated() -> bool {
    false
}

fn run(root: PathBuf, app_id: u32, rx: mpsc::Receiver<Command>, shared: Arc<Mutex<Snapshot>>) {
    let mut engine: Option<Engine> = None;
    let mut view = Snapshot {
        steam: "waiting".into(),
        relay: "unknown".into(),
        ..Default::default()
    };
    let mut retry = Instant::now();
    let mut wait_error: Option<String> = None;
    let args: Vec<_> = std::env::args().collect();
    let mut startup_lobby = args
        .windows(2)
        .find(|pair| pair[0] == "+connect_lobby")
        .and_then(|pair| pair[1].parse::<u64>().ok());
    loop {
        if engine.is_none() && Instant::now() >= retry {
            match Engine::new(&root, app_id) {
                Ok(next) => {
                    wait_error = None;
                    log(&mut view, "Steam подключён. Конфигурации загружены.");
                    if let Some(lobby) = startup_lobby.take() {
                        next.join(LobbyId::from_raw(lobby));
                    }
                    engine = Some(next);
                }
                Err(error) => {
                    // The same reason repeats on every retry; logging it each
                    // time floods the 50-line event log with "Ожидание Steam".
                    let detail = describe_steam_error(&error);
                    if wait_error.as_deref() != Some(detail.as_str()) {
                        log(
                            &mut view,
                            &format!(
                                "Ожидание Steam: {detail}{}",
                                wait_hint(&detail, process_elevated())
                            ),
                        );
                        wait_error = Some(detail);
                    }
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

#[cfg(test)]
mod steam_error {
    use super::*;

    #[test]
    fn init_message_payload_is_surfaced() {
        // The crate's Display prints "Some other failure"; the useful text
        // is in the payload and must reach the event log.
        let error = anyhow::Error::from(SteamAPIInitError::FailedGeneric(
            "Steam must be running and signed in".into(),
        ));
        assert_eq!(
            describe_steam_error(&error),
            "Steam must be running and signed in"
        );
    }

    #[test]
    fn non_init_errors_pass_through() {
        let error = anyhow::anyhow!("Steam не авторизован — войдите в аккаунт");
        assert_eq!(
            describe_steam_error(&error),
            "Steam не авторизован — войдите в аккаунт"
        );
    }
}

#[cfg(test)]
mod wait_hint_tests {
    use super::*;

    #[test]
    fn connect_to_global_user_gets_actionable_hints() {
        assert!(wait_hint("ConnectToGlobalUser failed.", true).contains("администратора"));
        assert!(wait_hint("ConnectToGlobalUser failed.", false).contains("Перезапустите Steam"));
        // Unrecognized details and non-init errors get no hint.
        assert_eq!(
            wait_hint("Steam client appears to be out of date", true),
            ""
        );
        assert_eq!(wait_hint("", false), "");
    }
}
