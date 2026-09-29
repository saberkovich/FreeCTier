use anyhow::{ensure, Result};
use uuid::Uuid;

pub const HEADER_LEN: usize = 24;
pub const MAX_CONTROL: usize = 128 * 1024;
const MAGIC: &[u8; 4] = b"FCT1";

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Config = 1,
    Ipv4 = 2,
}

pub struct Frame<'a> {
    pub kind: Kind,
    pub network: Uuid,
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    pub fn decode(bytes: &'a [u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= HEADER_LEN && bytes.len() <= MAX_CONTROL + HEADER_LEN,
            "Invalid frame size"
        );
        ensure!(
            &bytes[..4] == MAGIC && bytes[5..8] == [0, 0, 0],
            "Unsupported wire version/flags"
        );
        let kind = match bytes[4] {
            1 => Kind::Config,
            2 => Kind::Ipv4,
            _ => anyhow::bail!("Unknown frame kind"),
        };
        let network = Uuid::from_slice(&bytes[8..24])?;
        let payload = &bytes[HEADER_LEN..];
        ensure!(
            kind != Kind::Ipv4 || payload.len() <= crate::packet::MTU,
            "Oversized IP packet"
        );
        Ok(Self {
            kind,
            network,
            payload,
        })
    }

    pub fn encode(kind: Kind, network: Uuid, payload: &[u8]) -> Result<Vec<u8>> {
        ensure!(
            payload.len() <= MAX_CONTROL
                && (kind != Kind::Ipv4 || payload.len() <= crate::packet::MTU),
            "Oversized payload"
        );
        let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[kind as u8, 0, 0, 0]);
        bytes.extend_from_slice(network.as_bytes());
        bytes.extend_from_slice(payload);
        Ok(bytes)
    }
}
