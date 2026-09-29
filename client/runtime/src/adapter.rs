use anyhow::{Context, Result};
use freec_core::{config::Network, packet::MTU};
use std::{process::Command, sync::Arc};

/// First vertical slice runs elevated. A service/Named Pipe boundary is planned
/// before distribution; no OS network configuration is changed on startup.
pub struct Adapter {
    session: Arc<wintun::Session>,
}

impl Adapter {
    pub fn open(network: &Network, local: &str) -> Result<Self> {
        let dll = std::env::current_exe()?
            .parent()
            .context("Executable has no parent")?
            .join("wintun.dll");
        anyhow::ensure!(
            dll.is_file(),
            "Place the official AMD64 wintun.dll beside the executable"
        );
        // Absolute executable-relative path: never search the working directory.
        let api = unsafe { wintun::load_from_path(dll) }.context("Cannot load wintun.dll")?;
        let name = format!("FreeC-{}", &network.id.simple().to_string()[..12]);
        let adapter = wintun::Adapter::open(&api, &name)
            .or_else(|_| {
                wintun::Adapter::create(&api, &name, "FreeC Tier", Some(network.id.as_u128()))
            })
            .context("Cannot open Wintun adapter; this prototype requires administrator rights")?;
        let ip = network.member(local).context("Not a network member")?.ip;
        // Address assignment creates the connected /24 route. Never configure
        // a default gateway or DNS. Passing argv avoids shell interpolation.
        let netsh = std::path::PathBuf::from(
            std::env::var_os("SystemRoot").context("SystemRoot is missing")?,
        )
        .join("System32/netsh.exe");
        for args in [
            vec![
                "interface".into(),
                "ipv4".into(),
                "set".into(),
                "address".into(),
                format!("name={name}"),
                "source=static".into(),
                format!("address={ip}"),
                "mask=255.255.255.0".into(),
                "gateway=none".into(),
                "store=active".into(),
            ],
            vec![
                "interface".into(),
                "ipv4".into(),
                "set".into(),
                "subinterface".into(),
                name.clone(),
                format!("mtu={MTU}"),
                "store=active".into(),
            ],
        ] {
            let output = Command::new(&netsh).args(args).output()?;
            anyhow::ensure!(
                output.status.success(),
                "Cannot configure Wintun: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(Self {
            session: Arc::new(adapter.start_session(wintun::MAX_RING_CAPACITY)?),
        })
    }

    pub fn receive(&self) -> Result<Option<Vec<u8>>> {
        Ok(self
            .session
            .try_receive()?
            .map(|packet| packet.bytes().to_vec()))
    }

    pub fn inject(&self, bytes: &[u8]) -> Result<()> {
        let mut packet = self.session.allocate_send_packet(bytes.len().try_into()?)?;
        packet.bytes_mut().copy_from_slice(bytes);
        self.session.send_packet(packet);
        Ok(())
    }
}

impl Drop for Adapter {
    fn drop(&mut self) {
        let _ = self.session.shutdown();
    }
}
