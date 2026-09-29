#[cfg(test)]
mod policy {
    use crate::{
        config::{Network, SignedNetwork},
        packet::Packet,
        wire::{Frame, Kind},
    };
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use std::net::Ipv4Addr;
    use uuid::Uuid;

    fn packet(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8) -> Vec<u8> {
        let mut out = vec![0u8; 20];
        out[0] = 0x45;
        out[2..4].copy_from_slice(&(20u16).to_be_bytes());
        out[8] = 64;
        out[9] = protocol;
        out[12..16].copy_from_slice(&src.octets());
        out[16..20].copy_from_slice(&dst.octets());
        let mut sum: u32 = out
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| u32::from(u16::from_be_bytes([p[0], p[1]])))
            .sum();
        while sum > 0xffff {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        out[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
        out
    }

    #[test]
    fn ip_assignment_is_stable_after_revoke_and_readmit() {
        let key = SigningKey::generate(&mut OsRng);
        let mut network = Network::new("Minecraft".into(), 76561198000000001, 0, &key).unwrap();
        let ip = network
            .admit(&network.owner.clone(), 76561198000000002)
            .unwrap();
        network
            .revoke(&network.owner.clone(), "76561198000000002")
            .unwrap();
        assert_eq!(
            network
                .admit(&network.owner.clone(), 76561198000000002)
                .unwrap(),
            ip
        );
    }

    #[test]
    fn signed_updates_reject_ip_reassignment() {
        let key = SigningKey::generate(&mut OsRng);
        let mut old = Network::new("LAN".into(), 76561198000000001, 1, &key).unwrap();
        old.admit(&old.owner.clone(), 76561198000000002).unwrap();
        let signed_old = SignedNetwork::sign(old.clone(), &key).unwrap();
        let mut changed = old;
        changed.revision += 1;
        changed.members[1].ip = Ipv4Addr::new(10, 77, 1, 9);
        let signed_changed = SignedNetwork::sign(changed, &key).unwrap();
        assert!(signed_changed.check_update(&signed_old).is_err());
    }

    #[test]
    fn frames_are_network_scoped_and_bounded() {
        let id = Uuid::new_v4();
        let encoded = Frame::encode(
            Kind::Ipv4,
            id,
            &packet(Ipv4Addr::new(10, 77, 0, 1), Ipv4Addr::new(10, 77, 0, 2), 6),
        )
        .unwrap();
        let frame = Frame::decode(&encoded).unwrap();
        assert_eq!(frame.network, id);
        assert_eq!(Packet::parse(frame.payload).unwrap().protocol, 6);
    }

    #[test]
    fn packet_router_rejects_source_spoofing_and_supports_broadcast() {
        let key = SigningKey::generate(&mut OsRng);
        let mut network = Network::new("LAN".into(), 76561198000000001, 0, &key).unwrap();
        network
            .admit(&network.owner.clone(), 76561198000000002)
            .unwrap();
        let bad = packet(
            Ipv4Addr::new(10, 77, 0, 99),
            Ipv4Addr::new(10, 77, 0, 2),
            17,
        );
        assert!(Packet::parse(&bad)
            .unwrap()
            .outgoing(&network, &network.owner)
            .is_err());
        for destination in [
            Ipv4Addr::BROADCAST,
            network.broadcast(),
            Ipv4Addr::new(224, 0, 2, 60),
        ] {
            let bytes = packet(Ipv4Addr::new(10, 77, 0, 1), destination, 17);
            let parsed = Packet::parse(&bytes).unwrap();
            assert_eq!(
                parsed.outgoing(&network, &network.owner).unwrap(),
                vec!["76561198000000002"]
            );
            parsed
                .incoming(&network, &network.owner, "76561198000000002")
                .unwrap();
        }
        network
            .revoke(&network.owner.clone(), "76561198000000002")
            .unwrap();
        let bytes = packet(Ipv4Addr::new(10, 77, 0, 2), Ipv4Addr::new(10, 77, 0, 1), 17);
        assert!(Packet::parse(&bytes)
            .unwrap()
            .incoming(&network, "76561198000000002", &network.owner)
            .is_err());
    }

    #[test]
    fn untrusted_and_stale_configurations_cannot_replace_saved_state() {
        let key = SigningKey::generate(&mut OsRng);
        let other = SigningKey::generate(&mut OsRng);
        let initial = Network::new("LAN".into(), 76561198000000001, 0, &key).unwrap();
        let signed = SignedNetwork::sign(initial.clone(), &key).unwrap();
        let mut forged = signed.clone();
        forged.network.name = "Forged".into();
        assert!(forged.verify().is_err());
        let mut changed = initial.clone();
        changed.owner_key = hex::encode(other.verifying_key().as_bytes());
        changed.revision += 1;
        assert!(SignedNetwork::sign(changed, &other)
            .unwrap()
            .check_update(&signed)
            .is_err());
        let mut new = initial;
        new.admit(&new.owner.clone(), 76561198000000002).unwrap();
        let next = SignedNetwork::sign(new, &key).unwrap();
        assert!(next.check_update(&signed).unwrap());
        assert!(!signed.check_update(&next).unwrap());
    }

    #[test]
    fn malformed_frames_and_ipv4_headers_are_rejected_without_panics() {
        for size in 0..24 {
            assert!(Frame::decode(&vec![0; size]).is_err());
        }
        let bytes = packet(Ipv4Addr::new(10, 77, 0, 1), Ipv4Addr::new(10, 77, 0, 2), 6);
        for size in 0..20 {
            assert!(Packet::parse(&bytes[..size]).is_err());
        }
        let mut corrupt = bytes.clone();
        corrupt[12] ^= 1;
        assert!(Packet::parse(&corrupt).is_err());
        let mut invalid_ihl = bytes.clone();
        invalid_ihl[0] = 0x4f;
        assert!(Packet::parse(&invalid_ihl).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(Packet::parse(&trailing).is_err());
        assert!(Frame::encode(Kind::Ipv4, Uuid::new_v4(), &vec![0; 1281]).is_err());
    }

    #[test]
    fn networks_do_not_route_each_others_addresses() {
        let key = SigningKey::generate(&mut OsRng);
        let mut one = Network::new("One".into(), 76561198000000001, 0, &key).unwrap();
        let mut two = Network::new("Two".into(), 76561198000000001, 1, &key).unwrap();
        one.admit(&one.owner.clone(), 76561198000000002).unwrap();
        two.admit(&two.owner.clone(), 76561198000000003).unwrap();
        let bytes = packet(Ipv4Addr::new(10, 77, 0, 2), Ipv4Addr::new(10, 77, 0, 1), 6);
        let packet = Packet::parse(&bytes).unwrap();
        assert!(packet.reliable());
        packet
            .incoming(&one, "76561198000000002", &one.owner)
            .unwrap();
        assert!(packet
            .incoming(&two, "76561198000000002", &two.owner)
            .is_err());
    }

    #[test]
    fn storage_survives_restart_and_atomic_replacement() {
        use crate::storage::Store;
        let directory = tempfile::tempdir().unwrap();
        let store = Store::new(directory.path().to_owned()).unwrap();
        let key = store.owner_key().unwrap();
        let first = SignedNetwork::sign(
            Network::new("Saved".into(), 76561198000000001, 0, &key).unwrap(),
            &key,
        )
        .unwrap();
        store.save(&first).unwrap();
        let mut changed = first.network.clone();
        changed
            .admit(&changed.owner.clone(), 76561198000000002)
            .unwrap();
        let second = SignedNetwork::sign(changed, &key).unwrap();
        store.save(&second).unwrap();
        store.save_enabled(&[second.network.id]).unwrap();
        drop(store);
        let reopened = Store::new(directory.path().to_owned()).unwrap();
        assert_eq!(reopened.owner_key().unwrap().to_bytes(), key.to_bytes());
        assert_eq!(reopened.load().unwrap()[0].network, second.network);
        assert_eq!(reopened.load_enabled().unwrap(), vec![second.network.id]);
        reopened.delete(second.network.id).unwrap();
        reopened.delete(second.network.id).unwrap();
        assert!(reopened.load_enabled().unwrap().is_empty());
        assert!(Store::new(directory.path().to_owned())
            .unwrap()
            .load()
            .unwrap()
            .is_empty());
        assert_eq!(reopened.owner_key().unwrap().to_bytes(), key.to_bytes());
    }
}
