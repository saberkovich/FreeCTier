use crate::admission::{self, Password};
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
    /// Per-member permission to invite new participants. Only meaningful while
    /// the network access is public; the owner always may invite.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub can_invite: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub can_kick: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_key: Option<String>,
}

/// Who may invite new participants. Admission itself is always signed by the
/// owner, so public access widens who can start an invitation, not who admits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    #[default]
    Private,
    Public,
}

fn is_private(access: &Access) -> bool {
    *access == Access::Private
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
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
    // Omitting default policy fields preserves the schema-1 signing bytes.
    // Non-default policy is included in the owner's signature.
    #[serde(default, skip_serializing_if = "is_private")]
    pub access: Access,
    pub members: Vec<Member>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<Password>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedNetwork {
    pub network: Network,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<Box<Delegation>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delegation {
    pub previous: SignedNetwork,
    pub signer: String,
    pub operation: Operation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Admit {
        steam_id: String,
        public_key: String,
        proof: Option<String>,
    },
    Revoke {
        steam_id: String,
    },
}

impl Network {
    pub fn upgrade(&mut self, actor: &str) -> Result<()> {
        ensure!(actor == self.owner, "Only owner upgrades policy");
        if self.schema == 1 {
            for member in &mut self.members {
                if member.steam_id != self.owner && self.access == Access::Private {
                    member.can_invite = false;
                }
            }
            self.schema = 2;
            self.revision += 1;
        }
        Ok(())
    }
    pub fn new(name: String, owner: u64, subnet_index: u8, key: &SigningKey) -> Result<Self> {
        let network = Self {
            schema: 2,
            id: Uuid::new_v4(),
            name: name.trim().to_owned(),
            owner: owner.to_string(),
            owner_key: hex::encode(key.verifying_key().as_bytes()),
            revision: 1,
            subnet: Ipv4Addr::new(10, 77, subnet_index, 0),
            access: Access::Private,
            members: vec![Member {
                steam_id: owner.to_string(),
                ip: Ipv4Addr::new(10, 77, subnet_index, 1),
                active: true,
                can_invite: true,
                can_kick: true,
                public_key: Some(hex::encode(key.verifying_key().as_bytes())),
            }],
            password: None,
        };
        network.validate()?;
        Ok(network)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.schema, 1 | 2) && self.revision > 0,
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
            if let Some(key) = &member.public_key {
                admission::public_key(key)?;
            }
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
        if let Some(password) = &self.password {
            password.validate()?;
        }
        if let Some(key) = &self.member(&self.owner).unwrap().public_key {
            ensure!(key == &self.owner_key, "Owner key mismatch");
        }
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
            can_invite: self.access == Access::Public,
            can_kick: false,
            public_key: None,
        });
        self.revision += 1;
        Ok(ip)
    }

    /// Whether the given active member may start an invitation. The owner always
    /// may; other members only when the network is public and their per-member
    /// permission is enabled.
    pub fn may_invite(&self, steam_id: &str) -> bool {
        if steam_id == self.owner {
            return true;
        }
        (self.schema >= 2 || self.access == Access::Public)
            && self
                .members
                .iter()
                .any(|m| m.active && m.steam_id == steam_id && m.can_invite)
    }

    /// Admission requested by an authenticated participant. Revoked reservations
    /// may only be restored through the owner's explicit readmit action.
    pub fn admit_requested(&mut self, actor: &str, inviter: &str, steam_id: u64) -> Result<bool> {
        ensure!(actor == self.owner, "Only the owner admits members");
        ensure!(
            self.may_invite(inviter),
            "Sender may not invite to this network"
        );
        if let Some(member) = self
            .members
            .iter()
            .find(|m| m.steam_id == steam_id.to_string())
        {
            ensure!(
                member.active,
                "Revoked members require explicit re-admission"
            );
            return Ok(false);
        }
        self.admit(actor, steam_id)?;
        Ok(true)
    }

    pub fn set_access(&mut self, actor: &str, access: Access) -> Result<()> {
        ensure!(actor == self.owner, "Only the owner can change access");
        if self.access != access {
            self.access = access;
            if access == Access::Private {
                self.password = None;
            }
            // Changing the mode sets its defaults for current participants.
            for member in &mut self.members {
                if member.steam_id != self.owner {
                    member.can_invite = access == Access::Public;
                }
            }
            self.revision += 1;
        }
        Ok(())
    }

    pub fn set_member_invite(
        &mut self,
        actor: &str,
        steam_id: &str,
        can_invite: bool,
    ) -> Result<()> {
        ensure!(actor == self.owner, "Only the owner can change permissions");
        ensure!(steam_id != self.owner, "The owner always may invite");
        let member = self
            .members
            .iter_mut()
            .find(|m| m.steam_id == steam_id)
            .context("Unknown member")?;
        if member.can_invite != can_invite {
            member.can_invite = can_invite;
            self.revision += 1;
        }
        Ok(())
    }

    pub fn may_kick(&self, actor: &str, target: &str) -> bool {
        if actor == target || target == self.owner {
            return false;
        }
        let Some(member) = self.member(target) else {
            return false;
        };
        actor == self.owner
            || (self.member(actor).is_some_and(|m| m.can_kick)
                && !member.can_invite
                && !member.can_kick)
    }

    pub fn set_permissions(
        &mut self,
        actor: &str,
        target: &str,
        invite: bool,
        kick: bool,
    ) -> Result<()> {
        ensure!(
            actor == self.owner && target != self.owner,
            "Only owner can change member permissions"
        );
        let member = self
            .members
            .iter_mut()
            .find(|m| m.steam_id == target)
            .context("Unknown member")?;
        if member.can_invite != invite || member.can_kick != kick {
            member.can_invite = invite;
            member.can_kick = kick;
            self.revision += 1;
        }
        Ok(())
    }

    pub fn set_password(&mut self, actor: &str, password: Option<Password>) -> Result<()> {
        ensure!(actor == self.owner, "Only owner can change password");
        ensure!(
            password.is_none() || self.access == Access::Public,
            "Password requires a public network"
        );
        if self.password != password {
            self.password = password;
            self.revision += 1;
        }
        Ok(())
    }

    pub fn bind_key(&mut self, actor: &str, steam_id: &str, key: String) -> Result<()> {
        ensure!(actor == self.owner, "Only owner binds legacy member keys");
        admission::public_key(&key)?;
        let member = self
            .members
            .iter_mut()
            .find(|m| m.active && m.steam_id == steam_id)
            .context("Unknown member")?;
        if let Some(old) = &member.public_key {
            ensure!(old == &key, "Member key changed");
        } else {
            member.public_key = Some(key);
            self.revision += 1;
        }
        Ok(())
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
        Ok(Self {
            network,
            signature,
            delegation: None,
        })
    }

    pub fn verify(&self) -> Result<()> {
        self.verify_depth(0)
    }

    fn verify_depth(&self, depth: usize) -> Result<()> {
        ensure!(depth <= 24, "Delegation chain needs an owner checkpoint");
        if let Some(delegation) = &self.delegation {
            delegation.previous.verify_depth(depth + 1)?;
            let expected = delegation
                .previous
                .apply(&delegation.signer, &delegation.operation)?;
            ensure!(expected == self.network, "Unauthorized delegated changes");
            let signer = delegation
                .previous
                .network
                .member(&delegation.signer)
                .context("Signer inactive")?;
            let key = signer
                .public_key
                .as_ref()
                .context("Signer has no certified key")?;
            admission::verify(
                key,
                &serde_json::to_vec(&("FreeCTier/delegation/v2", &self.network, delegation))?,
                &self.signature,
            )?;
            self.network.validate()?;
            return Ok(());
        }
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
        ensure!(
            self.network.schema >= previous.network.schema,
            "Configuration schema downgrade"
        );
        if self.delegation.is_some() && self.network.revision > previous.network.revision {
            let mut parent = self;
            while let Some(delegation) = &parent.delegation {
                parent = &delegation.previous;
                if admission::fingerprint(parent)? == admission::fingerprint(previous)? {
                    break;
                }
            }
            ensure!(
                admission::fingerprint(parent)? == admission::fingerprint(previous)?,
                "Concurrent admission; retry after synchronization"
            );
        }
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

    fn apply(&self, signer: &str, operation: &Operation) -> Result<Network> {
        let mut next = self.network.clone();
        ensure!(next.member(signer).is_some(), "Signer inactive");
        match operation {
            Operation::Admit {
                steam_id,
                public_key,
                proof,
            } => {
                ensure!(
                    next.access == Access::Public || next.may_invite(signer),
                    "Invitation not permitted"
                );
                ensure!(
                    !next.members.iter().any(|m| &m.steam_id == steam_id),
                    "Member already known; only owner may restore access"
                );
                admission::public_key(public_key)?;
                if let Some(password) = &next.password {
                    password.verify(
                        proof.as_deref().context("Password required")?,
                        &admission::fingerprint(self)?,
                        steam_id,
                        public_key,
                    )?;
                }
                let id: u64 = steam_id.parse()?;
                ensure!(id.to_string() == *steam_id, "Invalid SteamID");
                next.admit(&next.owner.clone(), id)?;
                next.members.last_mut().unwrap().public_key = Some(public_key.clone());
            }
            Operation::Revoke { steam_id } => {
                ensure!(next.may_kick(signer, steam_id), "Exclusion not permitted");
                next.revoke(&next.owner.clone(), steam_id)?;
            }
        }
        Ok(next)
    }

    pub fn delegate(&self, signer: &str, operation: Operation, key: &SigningKey) -> Result<Self> {
        self.verify()?;
        let network = self.apply(signer, &operation)?;
        let delegation = Box::new(Delegation {
            previous: self.clone(),
            signer: signer.into(),
            operation,
        });
        let signature = hex::encode(
            key.sign(&serde_json::to_vec(&(
                "FreeCTier/delegation/v2",
                &network,
                &delegation,
            ))?)
            .to_bytes(),
        );
        let next = Self {
            network,
            signature,
            delegation: Some(delegation),
        };
        next.verify()?;
        ensure!(
            serde_json::to_vec(&next)?.len() <= crate::wire::MAX_CONTROL,
            "Admission history full; owner must synchronize"
        );
        Ok(next)
    }
}
