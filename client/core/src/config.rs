use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, net::Ipv4Addr};
use uuid::Uuid;

/// Steam IDs cross the JS boundary as decimal strings, never IEEE-754 numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Member {
    pub steam_id: String,
    pub ip: Ipv4Addr,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub schema: u8,
    pub id: Uuid,
    pub name: String,
    pub owner: String,
    pub owner_key: String,
    pub revision: u64,
    /// MVP uses canonical 10.77.x.0/24 subnets.
    pub subnet: Ipv4Addr,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedNetwork {
    pub network: Network,
    pub signature: String,
}

impl Network {
    pub fn new(name: String, owner: u64, subnet_index: u8, key: &SigningKey) -> Result<Self> {
        let network = Self {
            schema: 1,
            id: Uuid::new_v4(),
            name: name.trim().to_owned(),
            owner: owner.to_string(),
            owner_key: hex::encode(key.verifying_key().as_bytes()),
            revision: 1,
            subnet: Ipv4Addr::new(10, 77, subnet_index, 0),
            members: vec![Member {
                steam_id: owner.to_string(),
                ip: Ipv4Addr::new(10, 77, subnet_index, 1),
                active: true,
            }],
        };
        network.validate()?;
        Ok(network)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.revision > 0,
            "Unsupported configuration version"
        );
        ensure!(
            !self.name.trim().is_empty()
                && self.name.len() <= 128
                && !self.name.chars().any(char::is_control),
            "Invalid network name"
        );
        ensure!(self.owner.parse::<u64>()? > 0, "Invalid owner SteamID");
        let octets = self.subnet.octets();
        ensure!(
            octets[0] == 10 && octets[1] == 77 && octets[3] == 0,
            "Expected 10.77.x.0/24"
        );
        ensure!(
            !self.members.is_empty() && self.members.len() <= 254,
            "Invalid member count"
        );
        let mut ids = BTreeSet::new();
        let mut ips = BTreeSet::new();
        for member in &self.members {
            let id = member.steam_id.parse::<u64>()?;
            ensure!(
                id > 0 && id.to_string() == member.steam_id,
                "Invalid SteamID"
            );
            ensure!(
                ids.insert(id) && ips.insert(member.ip),
                "Duplicate member or IP"
            );
            ensure!(
                member.ip.octets()[..3] == octets[..3]
                    && (1..=254).contains(&member.ip.octets()[3]),
                "IP outside subnet"
            );
        }
        ensure!(
            self.member(&self.owner).is_some(),
            "Owner must remain active"
        );
        Ok(())
    }

    pub fn member(&self, steam_id: &str) -> Option<&Member> {
        self.members
            .iter()
            .find(|m| m.active && m.steam_id == steam_id)
    }

    pub fn admit(&mut self, actor: &str, steam_id: u64) -> Result<Ipv4Addr> {
        ensure!(actor == self.owner, "Only the owner can admit members");
        ensure!(steam_id > 0, "Invalid SteamID");
        let id = steam_id.to_string();
        if let Some(member) = self.members.iter_mut().find(|m| m.steam_id == id) {
            if !member.active {
                member.active = true;
                self.revision += 1;
            }
            return Ok(member.ip);
        }
        let host = (1..=254)
            .find(|host| !self.members.iter().any(|m| m.ip.octets()[3] == *host))
            .context("No unreserved addresses remain")?;
        let octets = self.subnet.octets();
        let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], host);
        self.members.push(Member {
            steam_id: id,
            ip,
            active: true,
        });
        self.revision += 1;
        Ok(ip)
    }

    pub fn revoke(&mut self, actor: &str, steam_id: &str) -> Result<()> {
        ensure!(
            actor == self.owner && steam_id != self.owner,
            "Only owner can remove other members"
        );
        let member = self
            .members
            .iter_mut()
            .find(|m| m.steam_id == steam_id)
            .context("Unknown member")?;
        if member.active {
            member.active = false;
            self.revision += 1;
        }
        Ok(())
    }

    pub fn broadcast(&self) -> Ipv4Addr {
        let [a, b, c, _] = self.subnet.octets();
        Ipv4Addr::new(a, b, c, 255)
    }
}

impl SignedNetwork {
    pub fn sign(network: Network, key: &SigningKey) -> Result<Self> {
        network.validate()?;
        ensure!(
            network.owner_key == hex::encode(key.verifying_key().as_bytes()),
            "Wrong owner key"
        );
        let signature = hex::encode(key.sign(&serde_json::to_vec(&network)?).to_bytes());
        Ok(Self { network, signature })
    }

    pub fn verify(&self) -> Result<()> {
        self.network.validate()?;
        let bytes: [u8; 32] = hex::decode(&self.network.owner_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid public key"))?;
        let key = VerifyingKey::from_bytes(&bytes)?;
        let signature = Signature::from_slice(&hex::decode(&self.signature)?)?;
        key.verify_strict(&serde_json::to_vec(&self.network)?, &signature)?;
        Ok(())
    }

    /// Existing networks pin both Steam identity and signing key. First admission
    /// must be authorized separately by the authenticated invitation flow.
    pub fn check_update(&self, previous: &Self) -> Result<bool> {
        self.verify()?;
        let new = &self.network;
        let old = &previous.network;
        ensure!(
            new.id == old.id && new.owner == old.owner && new.owner_key == old.owner_key,
            "Network identity changed"
        );
        ensure!(
            new.subnet == old.subnet,
            "Subnet changes require explicit migration"
        );
        if new.revision < old.revision {
            return Ok(false);
        }
        if new.revision == old.revision {
            ensure!(new == old, "Conflicting configuration at the same revision");
            return Ok(false);
        }
        for member in &old.members {
            if !new
                .members
                .iter()
                .any(|m| m.steam_id == member.steam_id && m.ip == member.ip)
            {
                bail!("An existing IP reservation was changed");
            }
        }
        Ok(true)
    }
}
