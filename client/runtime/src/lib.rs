mod adapter;
mod steam;

pub const DEFAULT_APP_ID: u32 = 324810;

/// How long the worker waits between looks at Steam. Also the floor of the
/// init backoff: a readiness check costs nothing, a failed init does.
const INIT_RETRY: Duration = Duration::from_secs(5);
/// Ceiling for the init backoff — far enough apart that a client stuck in a
/// state we cannot fix never sees a steady stream of attempts from us.
const INIT_RETRY_MAX: Duration = Duration::from_secs(60);

use anyhow::{ensure, Context, Result};
use base64::Engine as _;
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
    /// Steam avatars as `data:image/png;base64,…`, keyed by SteamID. Kept out
    /// of the snapshot on purpose: the UI polls that every second, and an
    /// avatar is tens of kilobytes that never change.
    avatars: Arc<Mutex<BTreeMap<String, String>>>,
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
    /// Avatars the worker has already decoded, for the requested members only.
    /// Unknown IDs are simply absent — the caller asks again later, and the
    /// worker keeps nudging Steam until the image arrives.
    pub fn avatars(&self, ids: &[String]) -> BTreeMap<String, String> {
        let cache = self.avatars.lock().unwrap_or_else(|p| p.into_inner());
        ids.iter()
            .filter_map(|id| cache.get(id).map(|url| (id.clone(), url.clone())))
            .collect()
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
    let avatars = Arc::new(Mutex::new(BTreeMap::new()));
    let handle = Handle {
        tx,
        snapshot: snapshot.clone(),
        avatars: avatars.clone(),
        worker: Arc::new(Mutex::new(None)),
    };
    let worker = handle.worker.clone();
    let join = thread::Builder::new()
        .name("freec-steam".into())
        .spawn(move || {
            let _instance_lock = lock;
            run(root, app_id, rx, snapshot, avatars)
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
    avatars: Arc<Mutex<BTreeMap<String, String>>>,
    /// SteamIDs already passed to `RequestUserInformation`. Steam answers with
    /// a callback whenever it feels like it, so the ask must not repeat on
    /// every view refresh.
    avatars_asked: BTreeSet<u64>,
}

impl Engine {
    fn new(
        root: &std::path::Path,
        app_id: u32,
        avatars: Arc<Mutex<BTreeMap<String, String>>>,
    ) -> Result<Self> {
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
            avatars,
            avatars_asked: BTreeSet::new(),
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

    /// Pulls avatars for everyone currently on screen. Steam serves them from
    /// its own local cache, so a hit is a memory copy; a miss means the image
    /// is not downloaded yet and `RequestUserInformation` starts that once.
    fn refresh_avatars(&mut self) {
        let wanted: BTreeSet<u64> = std::iter::once(self.local.as_str())
            .chain(
                self.networks
                    .values()
                    .flat_map(|signed| signed.network.members.iter())
                    .map(|member| member.steam_id.as_str()),
            )
            .filter_map(|id| id.parse::<u64>().ok())
            .filter(|id| *id != 0)
            .collect();
        let friends = self.client.friends();
        let mut cache = self.avatars.lock().unwrap_or_else(|p| p.into_inner());
        for id in wanted {
            if cache.contains_key(&id.to_string()) {
                continue;
            }
            match friends.get_friend(SteamId::from_raw(id)).medium_avatar() {
                Some(rgba) => {
                    if let Some(url) = encode_avatar(&rgba) {
                        cache.insert(id.to_string(), url);
                    }
                }
                None => {
                    if self.avatars_asked.insert(id) {
                        friends.request_user_information(SteamId::from_raw(id), false);
                    }
                }
            }
        }
    }

    fn update_view(&mut self, view: &mut Snapshot) {
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
        self.refresh_avatars();
    }
}

/// Steam hands out avatars as 64×64 RGBA. PNG keeps the alpha channel and is
/// the only encoding an `<img>` takes without extra work in the webview.
fn encode_avatar(rgba: &[u8]) -> Option<String> {
    const SIDE: u32 = 64;
    if rgba.len() != (SIDE * SIDE * 4) as usize {
        return None;
    }
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, SIDE, SIDE);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().ok()?;
    writer.write_image_data(rgba).ok()?;
    writer.finish().ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    ))
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

/// Actionable follow-up for Steam init failures we recognize. Field
/// evidence: re-logging the Steam account once fixed ConnectToGlobalUser,
/// but a user who had already re-logged still failed with «Cannot create
/// IPC pipe» while both tokens were clean — that shape is either a
/// steam.exe left over from another Windows account (owns the IPC pipe,
/// refuses everyone else) or antivirus blocking the IPC.
fn wait_hint(
    detail: &str,
    elevated: bool,
    steam: &SteamState,
    app_id: u32,
    in_library: Option<bool>,
) -> String {
    let connection_failure = detail.contains("ConnectToGlobalUser") || detail.contains("IPC pipe");
    if !connection_failure {
        return String::new();
    }
    if steam.same_user == Some(false) {
        return " Steam запущен от другой учётной записи Windows — перезагрузите компьютер, чтобы сбросить зависший процесс.".into();
    }
    if steam.processes > 1 {
        return " Запущено несколько процессов Steam, один из них завис — перезагрузите компьютер, чтобы сбросить его.".into();
    }
    // The app connects to Steam under a published AppID, and Steam refuses the
    // connection outright when the signed-in account holds no license for it.
    // Nothing in the token or process state shows that, so an account that
    // never added the (free) app looks exactly like a blocked one.
    if in_library == Some(false) {
        return format!(
            " Похоже, приложения {app_id}, через которое работает FreeC Tier, нет в вашей библиотеке Steam. Откройте store.steampowered.com/app/{app_id} и нажмите «Играть» — оно бесплатное, скачивать его не нужно. Если оно уже в библиотеке, причина внешняя: чаще всего антивирус, добавьте FreeC Tier и Steam в его исключения."
        );
    }
    match (elevated, steam.elevated) {
        (true, Some(false)) => {
            " Перезапустите Steam: выйдите из аккаунта и войдите заново. Если не поможет — перезапустите FreeC Tier без прав администратора.".into()
        }
        (true, _) => {
            " Перезапустите Steam: выйдите из аккаунта и войдите заново. Если не поможет — включите контроль учётных записей (UAC) и перезапустите оба приложения.".into()
        }
        // Tokens look correct on both sides — the connection is blocked from
        // the outside, which is almost always antivirus.
        _ => " Steam выглядит запущенным корректно, но соединение с ним блокируется. Чаще всего это антивирус — добавьте FreeC Tier и Steam в его исключения. Если не поможет — перезагрузите компьютер.".into(),
    }
}

/// Whether the signed-in Steam account has this AppID in its library. Steam
/// keeps a per-app key under `HKCU\Software\Valve\Steam\Apps` for everything
/// the account owns, installed or not, so a missing key is a strong signal
/// that the license is missing — but only a signal, never a gate: the hint it
/// drives still names the fallback cause.
#[cfg(not(windows))]
pub fn steam_app_in_library(_app_id: u32) -> Option<bool> {
    None
}

#[cfg(windows)]
pub fn steam_app_in_library(app_id: u32) -> Option<bool> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    let path = HSTRING::from(format!("Software\\Valve\\Steam\\Apps\\{app_id}"));
    let mut key = HKEY::default();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &path, Some(0), KEY_READ, &mut key) };
    if status == ERROR_SUCCESS {
        unsafe {
            let _ = RegCloseKey(key);
        };
        return Some(true);
    }
    // Steam itself has to be present for the absence to mean anything: without
    // the parent key we are looking at a machine Steam never wrote to.
    let mut parent = HKEY::default();
    let apps = HSTRING::from("Software\\Valve\\Steam\\Apps");
    let has_apps =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &apps, Some(0), KEY_READ, &mut parent) };
    if has_apps == ERROR_SUCCESS {
        unsafe {
            let _ = RegCloseKey(parent);
        };
        return Some(false);
    }
    None
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

/// What the Steam client process looks like from this process: how many
/// `steam.exe` instances run, and the first one's token elevation and
/// Windows user. Powers both the demotion gate and the connection-failure
/// diagnostics: a steam.exe left over from another Windows account owns the
/// IPC pipe and refuses everyone else, and several instances mean a hung one.
#[derive(Debug, Clone, Copy, Default)]
pub struct SteamState {
    pub processes: u32,
    pub elevated: Option<bool>,
    pub same_user: Option<bool>,
}

#[cfg(not(windows))]
pub fn steam_state() -> SteamState {
    SteamState::default()
}

#[cfg(windows)]
pub fn steam_state() -> SteamState {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        EqualSid, GetTokenInformation, TokenElevation, TokenUser, SECURITY_MAX_SID_SIZE,
        TOKEN_ELEVATION, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let mut state = SteamState::default();
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return state;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut first_pid = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let name = String::from_utf16_lossy(&entry.szExeFile);
                if name
                    .trim_end_matches('\0')
                    .eq_ignore_ascii_case("steam.exe")
                {
                    state.processes += 1;
                    if first_pid.is_none() {
                        first_pid = Some(entry.th32ProcessID);
                    }
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        let Some(pid) = first_pid else {
            return state;
        };

        // Our user SID for the same-account check. GetTokenInformation puts
        // the TOKEN_USER header and the SID it points to in one buffer, which
        // must be 8-byte aligned for the TOKEN_USER header (PSID pointer).
        const SID_BUFFER: usize = (SECURITY_MAX_SID_SIZE as usize + 16).div_ceil(8);
        let mut my_token = Default::default();
        let mut my_buffer = [0u64; SID_BUFFER];
        let my_sid: Option<windows::Win32::Security::PSID> = (|| {
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut my_token).ok()?;
            let mut returned = 0u32;
            GetTokenInformation(
                my_token,
                TokenUser,
                Some(my_buffer.as_mut_ptr().cast()),
                u32::try_from(my_buffer.len() * 8).expect("buffer size"),
                &mut returned,
            )
            .ok()?;
            Some((*my_buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid)
        })();

        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return state;
        };
        let mut token = Default::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        let _ = CloseHandle(process);
        if opened.is_err() {
            return state;
        };
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0u32;
        state.elevated = Some(
            GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut _),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
            .is_ok()
                && elevation.TokenIsElevated != 0,
        );
        if let Some(my_sid) = my_sid {
            let mut their_buffer = [0u64; SID_BUFFER];
            let mut returned = 0u32;
            if GetTokenInformation(
                token,
                TokenUser,
                Some(their_buffer.as_mut_ptr().cast()),
                u32::try_from(their_buffer.len() * 8).expect("buffer size"),
                &mut returned,
            )
            .is_ok()
            {
                let their_sid = (*their_buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid;
                state.same_user = Some(EqualSid(my_sid, their_sid).is_ok());
            }
        }
        let _ = CloseHandle(token);
        state
    }
}

#[cfg(windows)]
pub fn steam_token_elevated() -> Option<bool> {
    steam_state().elevated
}

/// What `HKCU\Software\Valve\Steam\ActiveProcess` reports about the running
/// client. `pid` is the `steam.exe` that claimed the key; `ActiveUser` is the
/// account slot of the signed-in user and stays 0 whenever Steam runs without
/// a live session — the login window, a client still reconnecting, or one the
/// user left in offline mode. That window is exactly when `SteamAPI_Init`
/// answers «ConnectToGlobalUser failed».
#[derive(Debug, Clone, Copy, Default)]
pub struct SteamActive {
    pub pid: u32,
    pub active_user: u32,
}

#[cfg(not(windows))]
pub fn steam_active() -> SteamActive {
    SteamActive::default()
}

#[cfg(windows)]
pub fn steam_active() -> SteamActive {
    use windows::core::{w, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    // The key lives under HKCU, so whatever we read here always describes the
    // client of the Windows account we run as — no cross-user ambiguity.
    let read = |name: PCWSTR| -> u32 {
        let mut value = 0u32;
        let mut size = u32::try_from(std::mem::size_of::<u32>()).expect("dword size");
        let status = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                w!("Software\\Valve\\Steam\\ActiveProcess"),
                name,
                RRF_RT_REG_DWORD,
                None,
                Some(std::ptr::addr_of_mut!(value).cast()),
                Some(&mut size),
            )
        };
        if status == ERROR_SUCCESS {
            value
        } else {
            0
        }
    };
    SteamActive {
        pid: read(w!("pid")),
        active_user: read(w!("ActiveUser")),
    }
}

/// Why `SteamAPI_Init` cannot succeed yet, or `None` once the client is signed
/// in and ready.
///
/// Games shipped through Steam never need this check: Steam launches them only
/// after it has a session, so their one and only init call always lands in a
/// good state. A standalone process starts whenever the user double-clicks it,
/// so it has to wait for that state itself — and it must not wait by polling
/// `SteamAPI_Init`, because a failed init keeps the Steam pipe it already
/// opened (see `run`).
fn steam_not_ready(state: &SteamState, active: &SteamActive) -> Option<&'static str> {
    if state.processes == 0 {
        return Some("Steam не запущен — запустите клиент и войдите в аккаунт.");
    }
    if active.pid == 0 {
        return Some("Steam ещё запускается — подождите, пока клиент откроется.");
    }
    if active.active_user == 0 {
        return Some(
            "Steam запущен, но вход в аккаунт не завершён — войдите в аккаунт и выключите режим «Не в сети».",
        );
    }
    None
}

/// Readiness gate for the worker loop. Windows-only: elsewhere there is no
/// Steam client to inspect and the init call itself is the only signal.
#[cfg(windows)]
fn steam_wait_reason() -> Option<&'static str> {
    steam_not_ready(&steam_state(), &steam_active())
}

#[cfg(not(windows))]
fn steam_wait_reason() -> Option<&'static str> {
    None
}

/// Launch `exe` with the Windows shell's (normally unelevated) token via
/// `CreateProcessWithTokenW`. Unlike the `explorer.exe` handoff hack this is
/// deterministic: the child token IS the shell token, whatever the system's
/// UAC quirks are.
#[cfg(windows)]
pub fn demote_via_shell(exe: &std::path::Path) -> bool {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        DuplicateTokenEx, SecurityImpersonation, TokenPrimary, TOKEN_ACCESS_MASK,
        TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{
        CreateProcessWithTokenW, OpenProcess, OpenProcessToken, CREATE_NO_WINDOW,
        LOGON_WITH_PROFILE, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId};
    unsafe {
        let shell = GetShellWindow();
        if shell.is_invalid() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(shell, Some(&mut pid));
        if pid == 0 {
            return false;
        }
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut token = Default::default();
        let opened = OpenProcessToken(
            process,
            TOKEN_ACCESS_MASK(TOKEN_DUPLICATE.0 | TOKEN_QUERY.0),
            &mut token,
        );
        let _ = CloseHandle(process);
        if opened.is_err() {
            return false;
        }
        let mut new_token = Default::default();
        let duplicated = DuplicateTokenEx(
            token,
            TOKEN_ACCESS_MASK(TOKEN_ASSIGN_PRIMARY.0 | TOKEN_DUPLICATE.0 | TOKEN_QUERY.0),
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut new_token,
        );
        let _ = CloseHandle(token);
        if duplicated.is_err() {
            return false;
        }
        let path = HSTRING::from(exe.as_os_str());
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut info = PROCESS_INFORMATION::default();
        let result = CreateProcessWithTokenW(
            new_token,
            LOGON_WITH_PROFILE,
            PCWSTR(path.as_ptr()),
            None,
            CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup,
            &mut info,
        );
        let _ = CloseHandle(new_token);
        result.is_ok()
    }
}

#[cfg(not(windows))]
pub fn demote_via_shell(_exe: &std::path::Path) -> bool {
    false
}

fn run(
    root: PathBuf,
    app_id: u32,
    rx: mpsc::Receiver<Command>,
    shared: Arc<Mutex<Snapshot>>,
    avatars: Arc<Mutex<BTreeMap<String, String>>>,
) {
    let mut engine: Option<Engine> = None;
    let mut view = Snapshot {
        steam: "waiting".into(),
        relay: "unknown".into(),
        ..Default::default()
    };
    let mut retry = Instant::now();
    let mut backoff = INIT_RETRY;
    let mut wait_error: Option<String> = None;
    let args: Vec<_> = std::env::args().collect();
    let mut startup_lobby = args
        .windows(2)
        .find(|pair| pair[0] == "+connect_lobby")
        .and_then(|pair| pair[1].parse::<u64>().ok());
    loop {
        if engine.is_none() && Instant::now() >= retry {
            // Ask Windows whether Steam is signed in before asking Steam. An
            // init attempt against a client that is still starting or sitting
            // on the login screen cannot succeed, and it is not free: see the
            // SteamAPI_Shutdown note below.
            match steam_wait_reason() {
                Some(reason) => {
                    if wait_error.as_deref() != Some(reason) {
                        log(&mut view, &format!("Ожидание Steam: {reason}"));
                        wait_error = Some(reason.to_owned());
                    }
                    // Nothing was spent on the client, so the next look can
                    // come soon and the backoff stays at its floor.
                    retry = Instant::now() + INIT_RETRY;
                    backoff = INIT_RETRY;
                }
                None => match Engine::new(&root, app_id, avatars.clone()) {
                    Ok(next) => {
                        wait_error = None;
                        backoff = INIT_RETRY;
                        log(&mut view, "Steam подключён. Конфигурации загружены.");
                        if let Some(lobby) = startup_lobby.take() {
                            next.join(LobbyId::from_raw(lobby));
                        }
                        engine = Some(next);
                    }
                    Err(error) => {
                        // `SteamAPI_Init` opens the Steam pipe before it connects
                        // the user, and a failure at the second step keeps the
                        // first: nothing in the error path releases it. Retrying
                        // without this leaks one pipe per attempt until the client
                        // refuses to hand out more and starts answering «Cannot
                        // create IPC pipe … Steam is probably not running» for a
                        // Steam that is running perfectly well. Games call init
                        // once and quit on failure, so they never hit this; a
                        // resident app has to pair every attempt with a shutdown.
                        unsafe { steamworks::sys::SteamAPI_Shutdown() };
                        // The same reason repeats on every retry; logging it each
                        // time floods the 50-line event log with "Ожидание Steam".
                        let detail = describe_steam_error(&error);
                        if wait_error.as_deref() != Some(detail.as_str()) {
                            let elevated = process_elevated();
                            let steam = steam_state();
                            let in_library = steam_app_in_library(app_id);
                            let mut line = format!(
                                "Ожидание Steam: {detail}{}",
                                wait_hint(&detail, elevated, &steam, app_id, in_library)
                            );
                            let connection_failure = detail.contains("ConnectToGlobalUser")
                                || detail.contains("IPC pipe");
                            if connection_failure {
                                // Process and token states are the diagnosis the
                                // user cannot see otherwise; log them verbatim.
                                line.push_str(&format!(
                                    " [приложение: {} · Steam: {}{}{}{}]",
                                    if elevated {
                                        "админ"
                                    } else {
                                        "обычный"
                                    },
                                    match steam.elevated {
                                        Some(true) => "админ",
                                        Some(false) => "обычный",
                                        None => "не найден",
                                    },
                                    if steam.processes > 1 {
                                        format!(" · процессов: {}", steam.processes)
                                    } else {
                                        String::new()
                                    },
                                    match steam.same_user {
                                        Some(false) => " · другой пользователь",
                                        _ => "",
                                    },
                                    match in_library {
                                        Some(false) => " · нет в библиотеке",
                                        Some(true) => " · есть в библиотеке",
                                        None => "",
                                    },
                                ));
                            }
                            log(&mut view, &line);
                            wait_error = Some(detail);
                        }
                        // Steam looked ready and still refused us, so the fault is
                        // outside both processes (antivirus, a wedged client).
                        // Back off instead of retrying at a fixed beat — a stuck
                        // state must not turn into steady pressure on the client.
                        retry = Instant::now() + backoff;
                        backoff = (backoff * 2).min(INIT_RETRY_MAX);
                    }
                },
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
        const APP: u32 = DEFAULT_APP_ID;
        let normal = SteamState {
            processes: 1,
            elevated: Some(false),
            same_user: Some(true),
        };
        // Cross-account steam.exe owns the IPC pipe: only a reboot helps.
        let foreign = SteamState {
            processes: 1,
            elevated: Some(false),
            same_user: Some(false),
        };
        let owned = Some(true);
        assert!(
            wait_hint("ConnectToGlobalUser failed.", false, &foreign, APP, owned)
                .contains("другой учётной записи")
        );
        // Several steam.exe instances: a hung one is holding the pipe.
        let doubled = SteamState {
            processes: 2,
            elevated: Some(false),
            same_user: Some(true),
        };
        assert!(wait_hint(
            "Cannot create IPC pipe to Steam client process.",
            false,
            &doubled,
            APP,
            owned
        )
        .contains("несколько процессов Steam"));
        // Clean tokens on both sides: point at antivirus, not at Steam restarts
        // the user has already tried.
        let hint = wait_hint(
            "Cannot create IPC pipe to Steam client process.",
            false,
            &normal,
            APP,
            owned,
        );
        assert!(hint.contains("антивирус"));
        // The account never added the app the client connects under: Steam
        // refuses the connection and no token or process state shows why.
        let missing = wait_hint(
            "ConnectToGlobalUser failed.",
            false,
            &normal,
            APP,
            Some(false),
        );
        assert!(missing.contains("библиотеке Steam"), "{missing}");
        assert!(missing.contains(&APP.to_string()), "{missing}");
        // Unknown library state must not invent a cause.
        assert!(
            wait_hint("ConnectToGlobalUser failed.", false, &normal, APP, None)
                .contains("антивирус")
        );
        // Elevated app + unelevated Steam keeps the elevation advice.
        let unelevated_steam = SteamState {
            processes: 1,
            elevated: Some(false),
            same_user: Some(true),
        };
        assert!(wait_hint(
            "ConnectToGlobalUser failed.",
            true,
            &unelevated_steam,
            APP,
            owned
        )
        .contains("без прав администратора"));
        let elevated_steam = SteamState {
            processes: 1,
            elevated: Some(true),
            same_user: Some(true),
        };
        assert!(wait_hint(
            "ConnectToGlobalUser failed.",
            true,
            &elevated_steam,
            APP,
            owned
        )
        .contains("UAC"));
        // Unrecognized details and non-init errors get no hint.
        assert_eq!(
            wait_hint(
                "Steam client appears to be out of date",
                true,
                &elevated_steam,
                APP,
                Some(false)
            ),
            ""
        );
        assert_eq!(wait_hint("", false, &normal, APP, Some(false)), "");
    }

    /// Live probe of the Steam client diagnostics. Asserts only structural
    /// invariants; run with --nocapture to read the machine's actual state.
    #[test]
    fn steam_state_probe_is_structurally_sound() {
        let state = steam_state();
        eprintln!("steam_state: {state:?}");
        if state.processes == 0 {
            eprintln!("Steam is not running on this machine — nothing to assert");
            return;
        }
        assert!(
            state.elevated.is_some(),
            "elevation must be resolved when steam.exe runs"
        );
        assert!(
            state.same_user.is_some(),
            "same-user must be resolved when steam.exe runs"
        );
        // A client that ran at least once has written its Apps key, so the
        // library probe must answer rather than shrug — otherwise the missing
        // license can never be told apart from a blocked connection.
        let mine = steam_app_in_library(DEFAULT_APP_ID);
        eprintln!("app {DEFAULT_APP_ID} in library: {mine:?}");
        assert!(mine.is_some(), "library state must resolve with Steam up");
        assert_eq!(
            steam_app_in_library(u32::MAX),
            Some(false),
            "an AppID nobody owns must read as absent, not unknown"
        );
    }
}

#[cfg(test)]
mod steam_ready_tests {
    use super::*;

    fn running(processes: u32) -> SteamState {
        SteamState {
            processes,
            elevated: Some(false),
            same_user: Some(true),
        }
    }

    #[test]
    fn init_waits_until_the_client_has_a_signed_in_user() {
        // No client at all: the plain case, and the only one the old loop
        // told the user about correctly.
        assert!(steam_not_ready(&running(0), &SteamActive::default())
            .expect("no steam.exe must block init")
            .contains("не запущен"));
        // steam.exe is up but has not claimed ActiveProcess yet.
        assert!(steam_not_ready(
            &running(1),
            &SteamActive {
                pid: 0,
                active_user: 0,
            }
        )
        .expect("a client that has not registered must block init")
        .contains("ещё запускается"));
        // The shape behind «ConnectToGlobalUser failed»: client running,
        // registered, nobody signed in.
        assert!(steam_not_ready(
            &running(1),
            &SteamActive {
                pid: 1234,
                active_user: 0,
            }
        )
        .expect("a signed-out client must block init")
        .contains("вход в аккаунт не завершён"));
        // Signed in: this is the only state worth spending an init on.
        assert_eq!(
            steam_not_ready(
                &running(1),
                &SteamActive {
                    pid: 1234,
                    active_user: 7,
                }
            ),
            None
        );
    }

    #[test]
    fn init_backoff_stays_within_its_bounds() {
        let mut backoff = INIT_RETRY;
        for _ in 0..10 {
            backoff = (backoff * 2).min(INIT_RETRY_MAX);
            assert!(backoff >= INIT_RETRY && backoff <= INIT_RETRY_MAX);
        }
        assert_eq!(backoff, INIT_RETRY_MAX);
    }
}
