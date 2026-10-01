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
    /// A member asks the network owner to admit another SteamID into the
    /// network. Payload is the decimal SteamID to admit.
    Request = 3,
    Hello = 4,
    Offer = 5,
    Join = 6,
}

pub struct Frame<'a> {
    pub kind: Kind,
    pub network: Uuid,
    pub payload: &'a [u8],
}

fn request_steam_id(payload: &[u8]) -> Result<u64> {
    ensure!(
        !payload.is_empty() && payload.len() <= 20,
        "Invalid invitation request size"
    );
    ensure!(
        payload.iter().all(u8::is_ascii_digit),
        "Invalid invited SteamID"
    );
    let text = std::str::from_utf8(payload)?;
    let id: u64 = text.parse()?;
    ensure!(id > 0 && id.to_string() == text, "Invalid invited SteamID");
    Ok(id)
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
            3 => Kind::Request,
            4 => Kind::Hello,
            5 => Kind::Offer,
            6 => Kind::Join,
            _ => anyhow::bail!("Unknown frame kind"),
        };
        let network = Uuid::from_slice(&bytes[8..24])?;
        let payload = &bytes[HEADER_LEN..];
        ensure!(
            kind != Kind::Ipv4 || payload.len() <= crate::packet::MTU,
            "Oversized IP packet"
        );
        if kind == Kind::Request {
            request_steam_id(payload)?;
        }
        Ok(Self {
            kind,
            network,
            payload,
        })
    }

    pub fn encode(kind: Kind, network: Uuid, payload: &[u8]) -> Result<Vec<u8>> {
        if kind == Kind::Request {
            request_steam_id(payload)?;
        }
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

    pub fn invited_steam_id(&self) -> Result<u64> {
        ensure!(self.kind == Kind::Request, "Not an invitation request");
        request_steam_id(self.payload)
    }
}
