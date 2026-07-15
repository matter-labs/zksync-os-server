use super::*;
use crate::config::{ConsensusConfig, RegistryMode};
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::bls12381::primitives::variant::MinPk;

const ERA_A: [u8; 32] = [0xA; 32];
const ERA_B: [u8; 32] = [0xB; 32];

#[test]
fn truncation_flag_roundtrips_and_absence_reads_none() {
    let root = tempfile::tempdir().expect("tempdir");
    let engine = root.path().join("consensus");
    // Absent directory and absent flag both read as "not flagged".
    assert!(read_truncation_flag(&engine).expect("absent dir").is_none());
    std::fs::create_dir(&engine).expect("create directory");
    assert!(
        read_truncation_flag(&engine)
            .expect("absent flag")
            .is_none()
    );

    std::fs::write(truncation_flag_path(&engine), "1234\n").expect("write flag");
    assert_eq!(
        read_truncation_flag(&engine).expect("present").as_deref(),
        Some("1234")
    );
}

#[test]
fn era_guard_covers_the_whole_matrix() {
    let hash_at_anchor = alloy::primitives::B256::repeat_byte(0x1D);
    let wrong_hash = alloy::primitives::B256::repeat_byte(0x2E);
    let no_fork = || (None, hash_at_anchor);

    // Normal operation: the recorded era matches, at any WAL tip.
    assert_eq!(
        decide_consensus_era(Some(ERA_A), ERA_A, false, 500, 0, no_fork().0, no_fork().1).unwrap(),
        EraDecision::Proceed
    );

    // First consensus start: fresh everything, exactly at the cutover — no
    // acknowledgment needed, nothing finalized is being overridden.
    assert_eq!(
        decide_consensus_era(None, ERA_A, true, 20, 20, None, hash_at_anchor).unwrap(),
        EraDecision::Adopt
    );
    // ... but never off the cutover (the sequencer ran past the agreed anchor,
    // or the node is missing history).
    assert!(decide_consensus_era(None, ERA_A, true, 25, 20, None, hash_at_anchor).is_err());
    assert!(decide_consensus_era(None, ERA_A, true, 15, 20, None, hash_at_anchor).is_err());

    // A fork / re-migration: a different era over cleared engine state.
    // Overriding finalized history demands the acknowledgment, naming
    // exactly this anchor and this node's hash there.
    let err =
        decide_consensus_era(Some(ERA_A), ERA_B, true, 40, 40, None, hash_at_anchor).unwrap_err();
    assert!(err.to_string().contains("acknowledge_fork"), "got: {err}");
    assert!(
        decide_consensus_era(
            Some(ERA_A),
            ERA_B,
            true,
            40,
            40,
            Some((39, hash_at_anchor)),
            hash_at_anchor,
        )
        .is_err(),
        "an acknowledgment naming the wrong height must refuse"
    );
    assert!(
        decide_consensus_era(
            Some(ERA_A),
            ERA_B,
            true,
            40,
            40,
            Some((40, wrong_hash)),
            hash_at_anchor,
        )
        .is_err(),
        "an acknowledgment naming a hash this chain does not have must refuse \
             (the truncation landed wrong)"
    );
    assert_eq!(
        decide_consensus_era(
            Some(ERA_A),
            ERA_B,
            true,
            40,
            40,
            Some((40, hash_at_anchor)),
            hash_at_anchor,
        )
        .unwrap(),
        EraDecision::Adopt
    );
    // Even acknowledged, the cutover must be exact.
    assert!(
        decide_consensus_era(
            Some(ERA_A),
            ERA_B,
            true,
            41,
            40,
            Some((40, hash_at_anchor)),
            hash_at_anchor,
        )
        .is_err()
    );

    // Era mixing: a different era over EXISTING engine state is always fatal,
    // acknowledged or not.
    assert!(
        decide_consensus_era(
            Some(ERA_A),
            ERA_B,
            false,
            40,
            40,
            Some((40, hash_at_anchor)),
            hash_at_anchor,
        )
        .is_err()
    );

    // Legacy instance from before era tracking: adopt regardless of tip.
    assert_eq!(
        decide_consensus_era(None, ERA_A, false, 500, 0, None, hash_at_anchor).unwrap(),
        EraDecision::Adopt
    );
}

#[test]
fn acknowledge_fork_parses_and_rejects_garbage() {
    assert_eq!(parse_acknowledge_fork(&None).unwrap(), None);
    let hash = alloy::primitives::B256::repeat_byte(0xAB);
    let parsed = parse_acknowledge_fork(&Some(format!("42:{hash}"))).unwrap();
    assert_eq!(parsed, Some((42, hash)));
    assert!(parse_acknowledge_fork(&Some("42".to_string())).is_err());
    assert!(parse_acknowledge_fork(&Some("x:0xab".to_string())).is_err());
    assert!(parse_acknowledge_fork(&Some("42:nothex".to_string())).is_err());
}

#[test]
fn rollback_requires_acknowledgment_over_consensus_state() {
    assert!(check_rollback_acknowledged(true, false).is_err());
    check_rollback_acknowledged(true, true).unwrap();
    check_rollback_acknowledged(false, false).unwrap();
    check_rollback_acknowledged(false, true).unwrap();
}
/// Deterministic key material for configuration tests, in the config's own hex
/// entry format.
fn test_validator(seed: u8, port: u16) -> (String, String, String) {
    use commonware_codec::{DecodeExt as _, Encode as _};
    use commonware_cryptography::Signer as _;
    let network = ed25519::PrivateKey::decode([seed; 32].as_slice()).expect("seed");
    // Scalars must be canonical; small seed bytes are.
    let bls = group::Private::decode(
        [
            0u8,
            seed.max(1),
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            1,
        ]
        .as_slice(),
    )
    .expect("canonical scalar");
    let bls_public =
        commonware_cryptography::bls12381::primitives::ops::compute_public::<MinPk>(&bls);
    let entry = format!(
        "{}:{}@127.0.0.1:{port}",
        alloy::hex::encode(network.public_key().encode()),
        alloy::hex::encode(bls_public.encode()),
    );
    (
        entry,
        alloy::hex::encode(network.encode()),
        alloy::hex::encode(bls.encode()),
    )
}

fn config_with(
    validators: Vec<String>,
    committees: Vec<crate::config::CommitteeScheduleEntryConfig>,
    network_key: impl Into<smart_config::value::SecretString>,
    bls_key: impl Into<smart_config::value::SecretString>,
) -> ConsensusConfig {
    ConsensusConfig {
        enabled: true,
        network_key: Some(network_key.into()),
        bls_key: Some(bls_key.into()),
        validators,
        committees,
        ..ConsensusConfig::default()
    }
}

#[test]
fn validators_shorthand_is_a_single_epoch_zero_committee() {
    let (a, a_net, a_bls) = test_validator(1, 4001);
    let (b, _, _) = test_validator(2, 4002);
    let setup = ConsensusSetup::from_config(
        &config_with(vec![a, b], vec![], a_net, a_bls),
        std::env::temp_dir(),
        6565,
    )
    .expect("valid config");
    assert_eq!(setup.schedule.entries().len(), 1);
    assert_eq!(setup.schedule.entries()[0].activation_epoch, 0);
    assert_eq!(setup.committee.len(), 2);
}

#[test]
fn schedule_entries_resolve_and_union_the_address_book() {
    let (a, a_net, a_bls) = test_validator(1, 4001);
    let (b, _, _) = test_validator(2, 4002);
    let (c, _, _) = test_validator(3, 4003);
    let committees = vec![
        crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: 0,
            validators: vec![a.clone(), b.clone()],
            source: Default::default(),
        },
        crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: 2,
            validators: vec![a.clone(), b.clone(), c.clone()],
            source: Default::default(),
        },
    ];
    let setup = ConsensusSetup::from_config(
        &config_with(vec![], committees, a_net, a_bls),
        std::env::temp_dir(),
        6565,
    )
    .expect("valid config");
    assert_eq!(setup.schedule.entries().len(), 2);
    // The address book carries the epoch-2 joiner so it is dialable early.
    assert_eq!(setup.committee.len(), 3);
}

#[test]
fn a_key_in_no_committee_requires_acknowledgment() {
    let (a, _, _) = test_validator(1, 4001);
    let (b, _, _) = test_validator(2, 4002);
    // Validator 3 is configured with its own keys but appears in no committee.
    let (_, outsider_net, outsider_bls) = test_validator(3, 4003);
    let config = config_with(vec![a, b], vec![], outsider_net, outsider_bls);
    let err = ConsensusSetup::from_config(&config, std::env::temp_dir(), 6565)
        .map(|_| ())
        .expect_err("must refuse a non-member without acknowledgment");
    assert!(err.to_string().contains("acknowledge_non_member"));

    let acknowledged = ConsensusConfig {
        acknowledge_non_member: true,
        ..config
    };
    ConsensusSetup::from_config(&acknowledged, std::env::temp_dir(), 6565)
        .map(|_| ())
        .expect("acknowledged follower mode starts");
}

#[test]
fn a_mismatched_bls_pairing_is_refused_loudly() {
    let (a, a_net, _) = test_validator(1, 4001);
    let (b, _, _) = test_validator(2, 4002);
    // The schedule lists validator 1's network key with validator 1's BLS key,
    // but this node is (mis)configured with validator 3's signing key.
    let (_, _, wrong_bls) = test_validator(3, 4003);
    let err = ConsensusSetup::from_config(
        &config_with(vec![a, b], vec![], a_net, wrong_bls),
        std::env::temp_dir(),
        6565,
    )
    .map(|_| ())
    .expect_err("a BLS pairing mismatch would silently never vote");
    assert!(err.to_string().contains("never vote"), "got: {err}");
}

#[test]
fn conflicting_member_identities_across_entries_are_refused() {
    let (a, a_net, a_bls) = test_validator(1, 4001);
    let (b, _, _) = test_validator(2, 4002);
    // Same network key as `b`, different port — two entries disagree about who
    // the validator is.
    let (b_moved, _, _) = test_validator(2, 5002);
    let committees = vec![
        crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: 0,
            validators: vec![a.clone(), b],
            source: Default::default(),
        },
        crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: 2,
            validators: vec![a, b_moved],
            source: Default::default(),
        },
    ];
    let err = ConsensusSetup::from_config(
        &config_with(vec![], committees, a_net, a_bls),
        std::env::temp_dir(),
        6565,
    )
    .map(|_| ())
    .expect_err("conflicting identities must be refused");
    assert!(err.to_string().contains("different BLS key or address"));
}

#[test]
fn the_first_schedule_entry_must_activate_at_epoch_zero() {
    let (a, a_net, a_bls) = test_validator(1, 4001);
    let (b, _, _) = test_validator(2, 4002);
    let committees = vec![crate::config::CommitteeScheduleEntryConfig {
        activation_epoch: 3,
        validators: vec![a, b],
        source: Default::default(),
    }];
    let err = ConsensusSetup::from_config(
        &config_with(vec![], committees, a_net, a_bls),
        std::env::temp_dir(),
        6565,
    )
    .map(|_| ())
    .expect_err("a schedule with a hole before its first entry must be refused");
    assert!(err.to_string().contains("committee schedule"), "got: {err}");
}

/// One committee entry at epoch 0 plus the registry flip at `flip`.
fn committees_with_flip(
    members: Vec<String>,
    flip: u64,
) -> Vec<crate::config::CommitteeScheduleEntryConfig> {
    vec![
        crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: 0,
            validators: members,
            source: Default::default(),
        },
        crate::config::CommitteeScheduleEntryConfig {
            activation_epoch: flip,
            validators: vec![],
            source: crate::config::CommitteeEntrySource::Registry,
        },
    ]
}

#[test]
fn registry_mode_validation_covers_the_matrix() {
    let (a, a_net, a_bls) = test_validator(1, 4001);
    let (b, _, _) = test_validator(2, 4002);
    let registry_address = Some(alloy::primitives::Address::repeat_byte(0x42));
    let base = |mode,
                committees: Option<Vec<crate::config::CommitteeScheduleEntryConfig>>,
                address| ConsensusConfig {
        registry_mode: mode,
        registry_address: address,
        ..config_with(
            if committees.is_none() {
                vec![a.clone(), b.clone()]
            } else {
                vec![]
            },
            committees.unwrap_or_default(),
            a_net.clone(),
            a_bls.clone(),
        )
    };
    let setup = |config: &ConsensusConfig| {
        ConsensusSetup::from_config(config, std::env::temp_dir(), 6565).map(|_| ())
    };

    // Schedule mode: an address that would be silently ignored is refused.
    setup(&base(RegistryMode::Schedule, None, None)).expect("plain schedule mode");
    let err = setup(&base(RegistryMode::Schedule, None, registry_address)).unwrap_err();
    assert!(err.to_string().contains("registry_address"), "got: {err}");

    // Shadow mode: the address is required; a flip entry is refused.
    setup(&base(RegistryMode::Shadow, None, registry_address)).expect("shadow mode");
    let err = setup(&base(RegistryMode::Shadow, None, None)).unwrap_err();
    assert!(err.to_string().contains("registry_address"), "got: {err}");
    let flip = committees_with_flip(vec![a.clone(), b.clone()], 4);
    let err = setup(&base(
        RegistryMode::Shadow,
        Some(flip.clone()),
        registry_address,
    ))
    .unwrap_err();
    assert!(err.to_string().contains("config_shadow"), "got: {err}");

    // Config-shadow mode: needs both the address and exactly one flip entry.
    setup(&base(
        RegistryMode::ConfigShadow,
        Some(flip.clone()),
        registry_address,
    ))
    .expect("config_shadow mode");
    let err = setup(&base(RegistryMode::ConfigShadow, None, registry_address)).unwrap_err();
    assert!(err.to_string().contains("source: registry"), "got: {err}");
    let mut two_flips = flip.clone();
    two_flips.push(crate::config::CommitteeScheduleEntryConfig {
        activation_epoch: 9,
        validators: vec![],
        source: crate::config::CommitteeEntrySource::Registry,
    });
    let err = setup(&base(
        RegistryMode::ConfigShadow,
        Some(two_flips),
        registry_address,
    ))
    .unwrap_err();
    assert!(err.to_string().contains("exactly one"), "got: {err}");

    // A flip entry listing validators, or claiming epoch 0, is refused.
    let mut listing = committees_with_flip(vec![a.clone(), b.clone()], 4);
    listing[1].validators = vec![a.clone()];
    let err = setup(&base(
        RegistryMode::ConfigShadow,
        Some(listing),
        registry_address,
    ))
    .unwrap_err();
    assert!(err.to_string().contains("must be empty"), "got: {err}");
    let at_zero = committees_with_flip(vec![a.clone(), b.clone()], 0);
    let err = setup(&base(
        RegistryMode::ConfigShadow,
        Some(at_zero),
        registry_address,
    ))
    .unwrap_err();
    assert!(err.to_string().contains("epoch 0"), "got: {err}");
}

#[test]
fn config_shadow_mode_resolves_the_flip_and_mirror_entries_do_not_override() {
    let (a, a_net, a_bls) = test_validator(1, 4001);
    let (b, _, _) = test_validator(2, 4002);
    let mut committees = committees_with_flip(vec![a.clone(), b.clone()], 4);
    // A mirror entry after the flip: config tracking a registry rotation.
    committees.push(crate::config::CommitteeScheduleEntryConfig {
        activation_epoch: 7,
        validators: vec![a.clone(), b.clone()],
        source: Default::default(),
    });
    let config = ConsensusConfig {
        registry_mode: RegistryMode::ConfigShadow,
        registry_address: Some(alloy::primitives::Address::repeat_byte(0x42)),
        ..config_with(vec![], committees, a_net, a_bls)
    };
    let setup = ConsensusSetup::from_config(&config, std::env::temp_dir(), 6565).expect("valid");
    let registry = setup.registry.expect("registry participates");
    assert_eq!(registry.flip_epoch, Some(4));
    assert_eq!(registry.chain_id, 6565);
    // The provider's source carries the flip: epoch 3 is config-settled;
    // everything at or after the flip — including the epoch the mirror
    // entry names — waits for a derivation (mirrors never override).
    use zksync_os_consensus_core::types::Epoch;
    assert!(setup.provider.settled_for(Epoch::new(3)));
    assert!(!setup.provider.settled_for(Epoch::new(4)));
    assert!(!setup.provider.settled_for(Epoch::new(7)));
    // Shadow mode has no flip: everything stays config-settled.
    let shadow = ConsensusSetup::from_config(
        &ConsensusConfig {
            registry_mode: RegistryMode::Shadow,
            registry_address: Some(alloy::primitives::Address::repeat_byte(0x42)),
            ..config_with(
                vec![a.clone(), b.clone()],
                vec![],
                config.network_key.clone().unwrap(),
                config.bls_key.clone().unwrap(),
            )
        },
        std::env::temp_dir(),
        6565,
    )
    .expect("valid shadow config");
    assert!(shadow.registry.expect("participates").flip_epoch.is_none());
    assert!(shadow.provider.settled_for(Epoch::new(1_000)));
}
