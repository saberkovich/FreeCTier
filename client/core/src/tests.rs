#[cfg(test)]
mod policy {
    use crate::{
        config::{Access, Network, SignedNetwork},
        packet::Packet,
        wire::{Frame, Kind},
    };
    use ed25519_dalek::{Signer, SigningKey};
    use rand::rngs::OsRng;
    use std::net::Ipv4Addr;
    use uuid::Uuid;

    /// Steam splits anything larger than one datagram into fragments, and a
    /// lost fragment of an unreliable send takes the whole game packet with
    /// it. `MTU_NO_FRAGMENT` is the preset that avoids that entirely: its
    /// packets plus our frame header fit inside `MTU_DataSize` (1200 bytes).
    #[test]
    fn no_fragment_mtu_fits_one_steam_datagram() {
        /// Steam's own per-message framing, with room to spare.
        const SEGMENT_OVERHEAD: usize = 24;
        let generated = crate::packet::MTU_NO_FRAGMENT as usize;
        assert!(
            generated + crate::wire::HEADER_LEN + SEGMENT_OVERHEAD <= crate::packet::STEAM_DATAGRAM
        );
        // The default is deliberately above that bound: a LAN-sized link
        // matters more to games than avoiding the second fragment. Both sides
        // are constants, so the check belongs to compilation, not to the run.
        const { assert!(crate::packet::MTU_DEFAULT > crate::packet::MTU_NO_FRAGMENT) };
    }

    /// Every MTU the settings can produce has to survive the packet path: the
    /// accept bound must cover the largest configurable link, and a full-size
    /// packet off such a link must parse and frame.
    #[test]
    fn configurable_mtu_range_is_accepted_and_clamped() {
        use crate::packet::{clamp_mtu, MTU, MTU_DEFAULT, MTU_MAX, MTU_MIN};
        assert_eq!(clamp_mtu(MTU_DEFAULT), MTU_DEFAULT);
        assert_eq!(clamp_mtu(0), MTU_MIN);
        assert_eq!(clamp_mtu(9000), MTU_MAX);
        assert_eq!(clamp_mtu(MTU_MIN), MTU_MIN);
        const { assert!(MTU_MAX as usize <= MTU) };
        for mtu in [MTU_MIN, MTU_DEFAULT, MTU_MAX] {
            let bytes = sized_packet(
                Ipv4Addr::new(10, 77, 0, 1),
                Ipv4Addr::new(10, 77, 0, 2),
                17,
                mtu as usize,
            );
            Packet::parse(&bytes).unwrap();
            Frame::encode(Kind::Ipv4, Uuid::nil(), &bytes).unwrap();
        }
    }

    #[test]
    fn offline_member_admits_with_password_proof_and_both_permissions() {
        use crate::{
            admission::{fingerprint, Password},
            config::Operation,
        };
        let owner_key = SigningKey::from_bytes(&[11; 32]);
        let member_key = SigningKey::from_bytes(&[12; 32]);
        let guest_key = SigningKey::from_bytes(&[13; 32]);
        let mut network = Network::new("Public".into(), 1, 0, &owner_key).unwrap();
        network.set_access("1", Access::Public).unwrap();
        network.admit("1", 2).unwrap();
        network
            .bind_key("1", "2", hex::encode(member_key.verifying_key().as_bytes()))
            .unwrap();
        network.set_permissions("1", "2", true, true).unwrap();
        network
            .set_password(
                "1",
                Some(Password::new("correct horse battery staple").unwrap()),
            )
            .unwrap();
        let base = SignedNetwork::sign(network, &owner_key).unwrap();
        let password = base.network.password.as_ref().unwrap();
        let snapshot = fingerprint(&base).unwrap();
        let guest_public = hex::encode(guest_key.verifying_key().as_bytes());
        assert!(password
            .prove("wrong", &snapshot, "3", &guest_public)
            .is_err());
        let proof = password
            .prove(
                "correct horse battery staple",
                &snapshot,
                "3",
                &guest_public,
            )
            .unwrap();
        let operation = Operation::Admit {
            steam_id: "3".into(),
            public_key: guest_public.clone(),
            proof: Some(proof.clone()),
        };
        let admitted = base.delegate("2", operation.clone(), &member_key).unwrap();
        assert!(admitted.check_update(&base).unwrap());
        assert!(admitted.network.member("3").is_some());
        assert!(base.delegate("2", operation, &guest_key).is_err());
        assert!(base
            .delegate(
                "2",
                Operation::Admit {
                    steam_id: "4".into(),
                    public_key: guest_public.clone(),
                    proof: Some(proof.clone())
                },
                &member_key
            )
            .is_err());
        let mut tampered = admitted.clone();
        tampered.network.members[2].can_kick = true;
        assert!(tampered.verify().is_err());
        let mut changed = base.network.clone();
        changed
            .set_password("1", Some(Password::new("new password").unwrap()))
            .unwrap();
        let changed = SignedNetwork::sign(changed, &owner_key).unwrap();
        assert!(changed
            .delegate(
                "2",
                Operation::Admit {
                    steam_id: "3".into(),
                    public_key: guest_public,
                    proof: Some(proof)
                },
                &member_key
            )
            .is_err());
        let dir = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::new(dir.path().to_owned()).unwrap();
        store.save(&admitted).unwrap();
        store.load().unwrap()[0].verify().unwrap();
    }

    #[test]
    fn private_delegated_permissions_are_independent_and_kick_protects_privileged_members() {
        use crate::config::Operation;
        let owner_key = SigningKey::from_bytes(&[21; 32]);
        let member_key = SigningKey::from_bytes(&[22; 32]);
        let mut network = Network::new("Private".into(), 1, 0, &owner_key).unwrap();
        network.admit("1", 2).unwrap();
        network
            .bind_key("1", "2", hex::encode(member_key.verifying_key().as_bytes()))
            .unwrap();
        network.admit("1", 3).unwrap();
        network.set_permissions("1", "2", true, true).unwrap();
        assert!(network.may_invite("2"));
        assert!(network.may_kick("2", "3"));
        assert!(!network.may_kick("2", "1"));
        assert!(!network.may_kick("2", "2"));
        let base = SignedNetwork::sign(network.clone(), &owner_key).unwrap();
        let kicked = base
            .delegate(
                "2",
                Operation::Revoke {
                    steam_id: "3".into(),
                },
                &member_key,
            )
            .unwrap();
        assert!(kicked.check_update(&base).unwrap());
        assert!(kicked.network.member("3").is_none());
        assert!(kicked
            .delegate(
                "2",
                Operation::Admit {
                    steam_id: "3".into(),
                    public_key: hex::encode(member_key.verifying_key().as_bytes()),
                    proof: None
                },
                &member_key
            )
            .is_err());
        network.set_permissions("1", "3", true, false).unwrap();
        assert!(!network.may_kick("2", "3"));
        network.set_permissions("1", "2", false, true).unwrap();
        assert!(!network.may_invite("2"));
        network.set_permissions("1", "2", true, false).unwrap();
        assert!(network.may_invite("2"));
        assert!(!network.may_kick("2", "3"));
    }

    #[test]
    fn delegated_forks_cannot_replace_each_other_or_change_password_policy() {
        use crate::config::Operation;
        let owner = SigningKey::from_bytes(&[31; 32]);
        let peer = SigningKey::from_bytes(&[32; 32]);
        let mut network = Network::new("LAN".into(), 1, 0, &owner).unwrap();
        network.set_access("1", Access::Public).unwrap();
        network.admit("1", 2).unwrap();
        let public_key = hex::encode(peer.verifying_key().as_bytes());
        network.bind_key("1", "2", public_key.clone()).unwrap();
        let base = SignedNetwork::sign(network, &owner).unwrap();
        let left = base
            .delegate(
                "2",
                Operation::Admit {
                    steam_id: "3".into(),
                    public_key: public_key.clone(),
                    proof: None,
                },
                &peer,
            )
            .unwrap();
        let right = base
            .delegate(
                "2",
                Operation::Admit {
                    steam_id: "4".into(),
                    public_key,
                    proof: None,
                },
                &peer,
            )
            .unwrap();
        assert!(left.check_update(&right).is_err());
        let mut forged = left;
        forged.network.access = Access::Private;
        assert!(forged.verify().is_err());
    }

    #[test]
    fn legacy_signed_configuration_survives_load_save_and_policy_update() {
        let key = SigningKey::from_bytes(&[7; 32]);
        // Exact schema-1 signing representation, predating invitation policies.
        let legacy = format!(
            "{{\"schema\":1,\"id\":\"67ae3f2d-0734-47f8-bf1b-e5e2bc64a289\",\"name\":\"Minecraft\",\"owner\":\"76561198000000001\",\"owner_key\":\"{}\",\"revision\":2,\"subnet\":\"10.77.0.0\",\"members\":[{{\"steam_id\":\"76561198000000001\",\"ip\":\"10.77.0.1\",\"active\":true}},{{\"steam_id\":\"76561198000000002\",\"ip\":\"10.77.0.2\",\"active\":true}}]}}",
            hex::encode(key.verifying_key().as_bytes())
        );
        let signature = hex::encode(key.sign(legacy.as_bytes()).to_bytes());
        let envelope = format!("{{\"network\":{legacy},\"signature\":\"{signature}\"}}");
        let signed: SignedNetwork = serde_json::from_str(&envelope).unwrap();
        signed.verify().unwrap();
        assert_eq!(serde_json::to_string(&signed.network).unwrap(), legacy);
        assert_eq!(signed.network.access, Access::Private);
        assert!(!signed.network.may_invite("76561198000000002"));
        let dir = tempfile::tempdir().unwrap();
        let store = crate::storage::Store::new(dir.path().to_owned()).unwrap();
        store.save(&signed).unwrap();
        let loaded = store.load().unwrap().remove(0);
        assert_eq!(loaded.signature, signature);
        let mut updated = loaded.network.clone();
        let owner = updated.owner.clone();
        updated.set_access(&owner, Access::Public).unwrap();
        updated
            .set_member_invite(&owner, "76561198000000002", false)
            .unwrap();
        let signed_update = SignedNetwork::sign(updated, &key).unwrap();
        assert!(signed_update.check_update(&loaded).unwrap());
        let encoded = serde_json::to_vec(&signed_update).unwrap();
        let decoded: SignedNetwork = serde_json::from_slice(&encoded).unwrap();
        decoded.verify().unwrap();
        let mut tampered = decoded.clone();
        tampered.network.access = Access::Private;
        assert!(tampered.verify().is_err());
        let mut tampered = decoded;
        tampered.network.members[1].can_invite = true;
        assert!(tampered.verify().is_err());
    }

    #[test]
    fn participant_requests_enforce_current_policy_and_preserve_revocations() {
        use crate::config::Operation;
        let owner_key = SigningKey::from_bytes(&[8; 32]);
        let inviter_key = SigningKey::from_bytes(&[9; 32]);
        let applicant_key = SigningKey::from_bytes(&[10; 32]);
        let mut network =
            Network::new("Requests".into(), 76561198000000001, 0, &owner_key).unwrap();
        let owner = network.owner.clone();
        let inviter = "76561198000000002";
        let applicant = "76561198000000003";
        network.admit(&owner, inviter.parse().unwrap()).unwrap();
        network
            .bind_key(
                &owner,
                inviter,
                hex::encode(inviter_key.verifying_key().as_bytes()),
            )
            .unwrap();
        let applicant_public = hex::encode(applicant_key.verifying_key().as_bytes());
        let admit_applicant = || Operation::Admit {
            steam_id: applicant.to_string(),
            public_key: applicant_public.clone(),
            proof: None,
        };
        let sign = |network: &Network| SignedNetwork::sign(network.clone(), &owner_key).unwrap();

        // Private network: even the owner's blessing of the request flow does not
        // widen invitations; the signer needs the flag on a public network.
        assert!(sign(&network)
            .delegate(inviter, admit_applicant(), &inviter_key)
            .is_err());
        network.set_access(&owner, Access::Public).unwrap();
        network.set_member_invite(&owner, inviter, false).unwrap();
        assert!(sign(&network)
            .delegate(inviter, admit_applicant(), &inviter_key)
            .is_err());
        network.set_member_invite(&owner, inviter, true).unwrap();
        let signed = sign(&network);
        let admitted = signed
            .delegate(inviter, admit_applicant(), &inviter_key)
            .unwrap();
        assert!(admitted.network.member(applicant).is_some());
        // An already known reservation, active or revoked, is never admitted twice.
        assert!(admitted
            .delegate(inviter, admit_applicant(), &inviter_key)
            .is_err());
        let ip = admitted.network.member(applicant).unwrap().ip;
        // The owner applies the same admission directly; the reservation is stable.
        network.admit(&owner, applicant.parse().unwrap()).unwrap();
        assert_eq!(network.member(applicant).unwrap().ip, ip);
        network.revoke(&owner, applicant).unwrap();
        let signed = sign(&network);
        assert!(signed
            .delegate(inviter, admit_applicant(), &inviter_key)
            .is_err());
        assert!(network.member(applicant).is_none());
        network.revoke(&owner, inviter).unwrap();
        let signed = sign(&network);
        // A revoked signer cannot start admissions at all.
        assert!(signed
            .delegate(inviter, admit_applicant(), &inviter_key)
            .is_err());
        assert_eq!(network.admit(&owner, 76561198000000003).unwrap(), ip);
        assert!(network.set_member_invite(&owner, &owner, false).is_err());
    }

    #[test]
    fn invitation_wire_requests_reject_malformed_ids() {
        let id = Uuid::new_v4();
        let bytes = Frame::encode(Kind::Request, id, b"76561198000000003").unwrap();
        assert_eq!(
            Frame::decode(&bytes).unwrap().invited_steam_id().unwrap(),
            76561198000000003
        );
        for payload in [
            b"".as_slice(),
            b"0",
            b"01",
            b"-1",
            b"+1",
            b" 1",
            b"1\n",
            b"18446744073709551616",
            b"111111111111111111111",
            b"\xff",
        ] {
            assert!(Frame::encode(Kind::Request, id, payload).is_err());
            let mut bytes = Frame::encode(Kind::Config, id, payload).unwrap();
            bytes[4] = Kind::Request as u8;
            assert!(Frame::decode(&bytes).is_err());
        }
    }

    fn packet(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8) -> Vec<u8> {
        sized_packet(src, dst, protocol, 20)
    }

    /// Header-only IPv4 packet padded out to `len` bytes.
    fn sized_packet(src: Ipv4Addr, dst: Ipv4Addr, protocol: u8, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        out[0] = 0x45;
        out[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        out[8] = 64;
        out[9] = protocol;
        out[12..16].copy_from_slice(&src.octets());
        out[16..20].copy_from_slice(&dst.octets());
        let mut sum: u32 = out[..20]
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
        assert!(SignedNetwork::sign(changed, &other).is_err());
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
        // One byte past the widest MTU the settings allow is not a packet any
        // peer can legitimately send, whatever MTU it runs with.
        assert!(Frame::encode(
            Kind::Ipv4,
            Uuid::new_v4(),
            &vec![0; crate::packet::MTU_MAX as usize + 1]
        )
        .is_err());
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
    fn invite_permissions_follow_access_and_member_flags() {
        let key = SigningKey::generate(&mut OsRng);
        let mut network = Network::new("LAN".into(), 76561198000000001, 0, &key).unwrap();
        network
            .admit(&network.owner.clone(), 76561198000000002)
            .unwrap();
        let owner = network.owner.clone();
        assert!(network.may_invite(&owner));
        assert!(!network.may_invite("76561198000000002"));
        network.set_access(&owner, Access::Public).unwrap();
        assert!(network.may_invite("76561198000000002"));
        network
            .set_member_invite(&owner, "76561198000000002", false)
            .unwrap();
        assert!(!network.may_invite("76561198000000002"));
        assert!(network
            .set_access("76561198000000002", Access::Private)
            .is_err());
        assert!(network
            .set_member_invite("76561198000000002", "76561198000000002", true)
            .is_err());
    }

    #[test]
    fn permission_changes_produce_accepted_updates() {
        let key = SigningKey::generate(&mut OsRng);
        let mut network = Network::new("LAN".into(), 76561198000000001, 0, &key).unwrap();
        network
            .admit(&network.owner.clone(), 76561198000000002)
            .unwrap();
        let signed = SignedNetwork::sign(network.clone(), &key).unwrap();
        let owner = network.owner.clone();
        network.set_access(&owner, Access::Public).unwrap();
        network
            .set_member_invite(&owner, "76561198000000002", false)
            .unwrap();
        let next = SignedNetwork::sign(network, &key).unwrap();
        assert!(next.check_update(&signed).unwrap());
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

#[cfg(test)]
mod i18n {
    use crate::i18n::{pick_language, placeholders, translate, FALLBACK};
    use std::collections::{BTreeSet, HashMap};

    fn parse(tag: &str, json: &str) -> HashMap<String, String> {
        serde_json::from_str(json).unwrap_or_else(|error| panic!("{tag}.json: {error}"))
    }

    fn source() -> HashMap<String, String> {
        let (_, json) = crate::i18n::LOCALES
            .iter()
            .find(|(tag, _)| *tag == FALLBACK)
            .expect("the source language ships");
        parse(FALLBACK, json)
    }

    /// Translations come back from Crowdin, so every file is checked against
    /// the source: valid JSON, no keys the code never asks for, and the same
    /// placeholders — a renamed `{name}` would silently drop the value.
    #[test]
    fn every_locale_parses_and_matches_the_source() {
        let source = source();
        for (tag, json) in crate::i18n::LOCALES {
            for (key, text) in parse(tag, json) {
                let original = source
                    .get(&key)
                    .unwrap_or_else(|| panic!("{tag}.json has `{key}`, which en.json lacks"));
                assert_eq!(
                    placeholders(&text),
                    placeholders(original),
                    "{tag}.json `{key}` placeholders"
                );
            }
        }
        // Russian is maintained alongside the source, not by Crowdin, so it
        // must be complete; other languages may lag and fall back to English.
        let russian = parse("ru", include_str!("../../../locales/ru.json"));
        let missing: BTreeSet<_> = source
            .keys()
            .filter(|key| !russian.contains_key(*key))
            .collect();
        assert!(missing.is_empty(), "ru.json lacks {missing:?}");
    }

    #[test]
    fn placeholders_fill_and_unknown_text_stays_visible() {
        assert_eq!(
            translate(
                "en",
                "event.peer_down",
                &[("name", &"Bob"), ("reason", &"gone")]
            ),
            "Connection lost: Bob — gone"
        );
        assert_eq!(
            translate(
                "ru",
                "event.peer_down",
                &[("name", &"Bob"), ("reason", &"gone")]
            ),
            "Соединение потеряно: Bob — gone"
        );
        // A placeholder nobody filled stays readable rather than vanishing.
        assert_eq!(
            translate("en", "event.peer_down", &[]),
            "Connection lost: {name} — {reason}"
        );
        // An unknown language falls back to the source, an unknown key shows itself.
        assert_eq!(translate("xx", "tray.quit", &[]), "Quit");
        assert_eq!(translate("ru", "no.such_key", &[]), "no.such_key");
    }

    /// First-run language. A display language we ship wins; otherwise the
    /// former Soviet republics outside the Baltics get Russian — by country or
    /// by interface language — and everybody else English.
    #[test]
    fn first_run_language_follows_the_country() {
        assert_eq!(pick_language("ru-RU", &["RU"]), "ru");
        assert_eq!(
            pick_language("ru-LV", &["LV"]),
            "ru",
            "a Russian Windows is a choice"
        );
        assert_eq!(pick_language("en-US", &["KZ"]), "ru");
        assert_eq!(pick_language("en-US", &["US", "UZ"]), "ru");
        assert_eq!(pick_language("uk-UA", &[]), "ru");
        assert_eq!(pick_language("kk-KZ", &[]), "ru");
        assert_eq!(pick_language("ka", &["GE"]), "ru");
        assert_eq!(pick_language("en-GB", &["GB"]), "en");
        assert_eq!(pick_language("de-DE", &["DE"]), "en");
        for baltic in ["LV", "LT", "EE"] {
            assert_eq!(pick_language("en-US", &[baltic]), "en", "{baltic}");
        }
        assert_eq!(pick_language("lv-LV", &["LV"]), "en");
        assert_eq!(pick_language("lt-LT", &["LT"]), "en");
        assert_eq!(pick_language("et-EE", &["EE"]), "en");
        assert_eq!(pick_language("", &[]), "en");
    }

    /// Every key the code asks for exists in the source catalog. A scan, not a
    /// parser: literal keys after `t(`, `tf(`, `h(` and in `data-i18n*`
    /// attributes, in every file that renders user-facing text.
    #[test]
    fn every_key_used_in_code_exists() {
        let source = source();
        let files = [
            (
                "runtime/src/lib.rs",
                include_str!("../../runtime/src/lib.rs"),
            ),
            (
                "runtime/src/steam.rs",
                include_str!("../../runtime/src/steam.rs"),
            ),
            (
                "runtime/src/adapter.rs",
                include_str!("../../runtime/src/adapter.rs"),
            ),
            (
                "gui/src-tauri/src/main.rs",
                include_str!("../../gui/src-tauri/src/main.rs"),
            ),
            ("core/src/admission.rs", include_str!("admission.rs")),
            ("gui/src/main.ts", include_str!("../../gui/src/main.ts")),
        ];
        let mut found = 0;
        for (name, text) in files {
            for key in used_keys(text) {
                found += 1;
                assert!(
                    source.contains_key(key),
                    "{name} uses `{key}`, missing from en.json"
                );
            }
        }
        assert!(
            found > 100,
            "the scan found only {found} keys — is it still matching?"
        );
    }

    fn used_keys(text: &str) -> Vec<&str> {
        let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let mut keys = Vec::new();
        for (index, _) in text.match_indices('(') {
            let head = &text[..index];
            let call = ["tf", "t", "h"].into_iter().find(|name| {
                head.ends_with(name)
                    && !head[..head.len() - name.len()]
                        .bytes()
                        .last()
                        .is_some_and(ident)
            });
            if call.is_some() {
                keys.extend(quoted(&text[index + 1..]));
            }
        }
        for (index, _) in text.match_indices("data-i18n") {
            let rest = &text[index..];
            if let Some(eq) = rest.find('=') {
                if rest[..eq].bytes().all(|b| ident(b) || b == b'-') {
                    keys.extend(quoted(&rest[eq + 1..]));
                }
            }
        }
        keys.retain(|key| {
            key.contains('.')
                && key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
        });
        keys
    }

    /// The string literal right after an opening paren or `=`, if any.
    fn quoted(rest: &str) -> Option<&str> {
        let rest = rest.trim_start();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let body = &rest[1..];
        body.find(quote).map(|end| &body[..end])
    }
}
