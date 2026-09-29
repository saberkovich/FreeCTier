use crate::config::SignedNetwork;
use anyhow::{ensure, Context, Result};
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("networks"))?;
        fs::create_dir_all(root.join("identity"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load(&self) -> Result<Vec<SignedNetwork>> {
        let mut networks = Vec::new();
        for entry in fs::read_dir(self.root.join("networks"))? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            ensure!(
                fs::metadata(&path)?.len() <= crate::wire::MAX_CONTROL as u64,
                "Oversized network file"
            );
            let network: SignedNetwork = serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("Corrupt network file: {}", path.display()))?;
            network.verify()?;
            ensure!(
                path.file_stem().and_then(|s| s.to_str()) == Some(&network.network.id.to_string()),
                "Network filename mismatch"
            );
            networks.push(network);
        }
        networks.sort_by_key(|n| n.network.id);
        Ok(networks)
    }

    pub fn save(&self, network: &SignedNetwork) -> Result<()> {
        network.verify()?;
        atomic_write(
            &self
                .root
                .join("networks")
                .join(format!("{}.json", network.network.id)),
            &serde_json::to_vec_pretty(network)?,
        )
    }

    /// Forget a network on this computer. This does not change the owner's
    /// signed membership document on other computers.
    pub fn delete(&self, id: uuid::Uuid) -> Result<()> {
        let mut enabled = self.load_enabled()?;
        enabled.retain(|saved| *saved != id);
        self.save_enabled(&enabled)?;
        let path = self.root.join("networks").join(format!("{id}.json"));
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn load_enabled(&self) -> Result<Vec<uuid::Uuid>> {
        let path = self.root.join("enabled.json");
        match fs::read(path) {
            Ok(bytes) => {
                ensure!(bytes.len() <= 16384, "Oversized adapter settings");
                Ok(serde_json::from_slice(&bytes)?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save_enabled(&self, networks: &[uuid::Uuid]) -> Result<()> {
        atomic_write(
            &self.root.join("enabled.json"),
            &serde_json::to_vec(networks)?,
        )
    }

    /// Per-user application-data directory. DPAPI protection is handled by the
    /// Windows runtime before release; never transfer this private key to peers.
    pub fn owner_key(&self) -> Result<SigningKey> {
        let path = self.root.join("identity").join("owner.key");
        if path.exists() {
            let bytes: [u8; 32] = fs::read(path)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("Invalid owner key file"))?;
            return Ok(SigningKey::from_bytes(&bytes));
        }
        let key = SigningKey::generate(&mut OsRng);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        file.write_all(&key.to_bytes())?;
        file.sync_all()?;
        Ok(key)
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing parent directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}
