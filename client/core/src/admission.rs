//! Password knowledge proofs. Participants store a public verifier, never the
//! password or the derived signing key. Proofs bind the applicant and snapshot.
use anyhow::{ensure, Context, Result};
use argon2::Argon2;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Password {
    pub salt: String,
    pub verifier: String,
}

impl Password {
    pub fn new(password: &str) -> Result<Self> {
        ensure!(
            !password.is_empty() && password.len() <= 256,
            "Password must contain 1–256 UTF-8 bytes"
        );
        let mut salt = [0; 16];
        OsRng.fill_bytes(&mut salt);
        let mut value = Self {
            salt: hex::encode(salt),
            verifier: String::new(),
        };
        value.verifier = hex::encode(value.key(password)?.verifying_key().as_bytes());
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            hex::decode(&self.salt)?.len() == 16,
            "Invalid password salt"
        );
        public_key(&self.verifier)?;
        Ok(())
    }

    fn key(&self, password: &str) -> Result<SigningKey> {
        ensure!(
            !password.is_empty() && password.len() <= 256,
            "Invalid password length"
        );
        let mut bytes = [0; 32];
        Argon2::default()
            .hash_password_into(password.as_bytes(), &hex::decode(&self.salt)?, &mut bytes)
            .map_err(|_| anyhow::anyhow!("Password derivation failed"))?;
        let key = SigningKey::from_bytes(&bytes);
        bytes.fill(0);
        Ok(key)
    }

    pub fn prove(
        &self,
        password: &str,
        snapshot: &str,
        steam_id: &str,
        member_key: &str,
    ) -> Result<String> {
        let key = self.key(password)?;
        ensure!(
            hex::encode(key.verifying_key().as_bytes()) == self.verifier,
            "Неверный пароль сети"
        );
        Ok(hex::encode(
            key.sign(&proof_message(snapshot, steam_id, member_key)?)
                .to_bytes(),
        ))
    }

    pub fn verify(
        &self,
        proof: &str,
        snapshot: &str,
        steam_id: &str,
        member_key: &str,
    ) -> Result<()> {
        verify(
            &self.verifier,
            &proof_message(snapshot, steam_id, member_key)?,
            proof,
        )
    }
}

fn proof_message(snapshot: &str, steam_id: &str, member_key: &str) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&(
        "FreeCTier/password/v2",
        snapshot,
        steam_id,
        member_key,
    ))?)
}

pub fn public_key(key: &str) -> Result<VerifyingKey> {
    let bytes: [u8; 32] = hex::decode(key)?
        .try_into()
        .ok()
        .context("Invalid public key")?;
    Ok(VerifyingKey::from_bytes(&bytes)?)
}

pub fn verify(key: &str, message: &[u8], signature: &str) -> Result<()> {
    public_key(key)?.verify_strict(message, &Signature::from_slice(&hex::decode(signature)?)?)?;
    Ok(())
}

pub fn fingerprint(value: &impl Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}
