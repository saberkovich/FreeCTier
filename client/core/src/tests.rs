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
