use crate::config::Network;
use anyhow::{ensure, Context, Result};
use std::net::Ipv4Addr;

/// Largest IPv4 packet accepted off the tunnel, in either direction. The
/// interface MTU is a user setting, so the accept bound is the top of its
/// range and not the configured value: a peer may run with a larger MTU than
/// ours, and its packets must still be delivered.
pub const MTU: usize = MTU_MAX as usize;

/// Default interface MTU. Games are the workload here, and a conservative
/// tunnel MTU is a worse failure than an extra Steam fragment: engines that
/// assume a LAN-sized link emit large discovery and state packets, and some
/// refuse to work with a link below the IPv6 minimum of 1280.
pub const MTU_DEFAULT: u32 = 1280;
/// Lowest MTU the settings allow — the IPv4 minimum reassembly buffer.
pub const MTU_MIN: u32 = 576;
/// Highest MTU the settings allow — a plain Ethernet link.
pub const MTU_MAX: u32 = 1500;
/// Payload one Steam datagram carries (`MTU_DataSize`, 1200 by default).
pub const STEAM_DATAGRAM: usize = 1200;
/// Largest MTU whose packets, header included, still fit a single Steam
/// datagram. Above it Steam splits a packet into fragments, and losing one
/// fragment of an unreliable message loses the whole game packet — the trade
/// against the compatibility of a LAN-sized link, which is why this is a
/// preset and not the default. Guarded by
/// `no_fragment_mtu_fits_one_steam_datagram`.
pub const MTU_NO_FRAGMENT: u32 = 1152;

/// Keep a configured MTU inside the supported range. Values come from the UI
/// and from settings files written by older versions, so they are never
/// trusted.
pub fn clamp_mtu(value: u32) -> u32 {
    value.clamp(MTU_MIN, MTU_MAX)
}

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
