use crate::config::Network;
use anyhow::{ensure, Context, Result};
use std::net::Ipv4Addr;

pub const MTU: usize = 1280;

#[derive(Debug)]
pub struct Packet<'a> {
    pub bytes: &'a [u8],
    pub source: Ipv4Addr,
    pub destination: Ipv4Addr,
    pub protocol: u8,
}

impl<'a> Packet<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= 20 && bytes.len() <= MTU,
            "Invalid IPv4 packet size"
        );
        ensure!(bytes[0] >> 4 == 4, "Only IPv4 is supported");
        let ihl = usize::from(bytes[0] & 15) * 4;
        ensure!(
            ihl >= 20 && ihl <= bytes.len(),
            "Invalid IPv4 header length"
        );
        let total = usize::from(u16::from_be_bytes([bytes[2], bytes[3]]));
        ensure!(total == bytes.len(), "IPv4 total length mismatch");
        let mut sum: u32 = bytes[..ihl]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u32::from(u16::from_be_bytes([pair[0], pair[1]])))
            .sum();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        ensure!(sum == 0xffff, "Invalid IPv4 checksum");
        ensure!(
            bytes[8] > 0 && matches!(bytes[9], 1 | 6 | 17),
            "Unsupported IP protocol or expired TTL"
        );
        Ok(Self {
            bytes,
            source: Ipv4Addr::new(bytes[12], bytes[13], bytes[14], bytes[15]),
            destination: Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]),
            protocol: bytes[9],
        })
    }

    pub fn reliable(&self) -> bool {
        self.protocol == 6
    }

    /// Only the explicitly supported discovery group is forwarded in the MVP.
    pub fn fanout(&self, network: &Network) -> bool {
        self.protocol == 17
            && (self.destination == Ipv4Addr::BROADCAST
                || self.destination == network.broadcast()
                || self.destination == Ipv4Addr::new(224, 0, 2, 60))
    }

    pub fn outgoing(&self, network: &Network, local: &str) -> Result<Vec<String>> {
        let member = network
            .member(local)
            .context("Local identity is not a member")?;
        ensure!(
            self.source == member.ip,
            "Source IP is not assigned to local identity"
        );
        if self.fanout(network) {
            Ok(network
                .members
                .iter()
                .filter(|m| m.active && m.steam_id != local)
                .map(|m| m.steam_id.clone())
                .collect())
        } else {
            let destination = network
                .members
                .iter()
                .find(|m| m.active && m.ip == self.destination && m.steam_id != local)
                .context("Destination is not a remote member")?;
            Ok(vec![destination.steam_id.clone()])
        }
    }

    pub fn incoming(&self, network: &Network, sender: &str, local: &str) -> Result<()> {
        ensure!(sender != local, "Looped packet");
        let remote = network.member(sender).context("Sender is not a member")?;
        let local = network
            .member(local)
            .context("Local identity is not a member")?;
        ensure!(self.source == remote.ip, "Spoofed source IP");
        ensure!(
            self.destination == local.ip || self.fanout(network),
            "Packet is not addressed to this interface"
        );
        Ok(())
    }
}
