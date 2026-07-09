//! The on-chain validator registry: the contract and its node-side mirror.
//!
//! Consensus nodes read the registry's storage slots directly, so the slot
//! layout is a cross-component interface with two independent
//! implementations: the Solidity contract writes it, the Rust mirror
//! (`zksync_os_consensus_registry::v1`) reads it and manufactures it for
//! tests. These tests pin the two against each other and against the
//! checked-in bytecode, so neither can drift silently.

use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Context as _;
use commonware_codec::{DecodeExt as _, Encode as _};
use commonware_cryptography::Signer as _;
use commonware_cryptography::bls12381::primitives::{group, ops, variant::MinPk};
use commonware_cryptography::ed25519;
use std::time::Duration;
use zksync_os_consensus_registry::v1::{RawIdentity, RegistryStateBuilder};
use zksync_os_integration_tests::contracts::ValidatorRegistry;
use zksync_os_integration_tests::multi_node::MultiNodeTester;
use zksync_os_integration_tests::{CURRENT_TO_L1, Tester, test_multisetup};

/// The pinned bytecode is regenerated only as a deliberate, reviewed act —
/// exactly like the wire goldens. If this fails, either revert the contract
/// change or update the pin (`lib/consensus/registry/pinned/`) together with
/// whatever layout-version bump the change requires.
#[test]
fn pinned_bytecode_matches_the_contract_source() -> anyhow::Result<()> {
    let artifact: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(
        "../contracts/out/ValidatorRegistry.sol/ValidatorRegistry.json",
    )?)?;
    let fresh_runtime = artifact["deployedBytecode"]["object"]
        .as_str()
        .expect("artifact has runtime bytecode");
    let fresh_deploy = artifact["bytecode"]["object"]
        .as_str()
        .expect("artifact has deploy bytecode");
    assert_eq!(
        fresh_runtime,
        zksync_os_consensus_registry::v1::PINNED_RUNTIME_BYTECODE_HEX.trim(),
        "the compiled registry runtime bytecode no longer matches the pin",
    );
    assert_eq!(
        fresh_deploy,
        zksync_os_consensus_registry::v1::PINNED_DEPLOY_BYTECODE_HEX.trim(),
        "the compiled registry deploy bytecode no longer matches the pin",
    );
    Ok(())
}

/// A deterministic validator identity with a real proof of possession bound
/// to (owner, chain, registry).
fn test_identity(seed: u8, chain_id: u64, registry: Address) -> RawIdentity {
    let mut scalar = [0u8; 32];
    scalar[31] = seed;
    let bls_private = group::Private::decode(scalar.as_slice()).expect("small scalar is valid");
    let bls_public = ops::compute_public::<MinPk>(&bls_private);
    let owner = Address::repeat_byte(seed);
    let pop = zksync_os_consensus_registry::sign_proof_of_possession(
        &bls_private,
        owner,
        chain_id,
        registry,
    );
    let mut network_seed = [seed; 32];
    network_seed[0] = 0x10;
    let network_private =
        ed25519::PrivateKey::decode(network_seed.as_slice()).expect("any seed is a valid key");

    let mut bls_key = [0u8; 48];
    bls_key.copy_from_slice(&bls_public.encode());
    let mut network_key = [0u8; 32];
    network_key.copy_from_slice(&network_private.public_key().encode());
    let mut pop_bytes = [0u8; 96];
    pop_bytes.copy_from_slice(&pop.encode());
    RawIdentity {
        owner,
        bls_key,
        network_key,
        pop: pop_bytes,
        ingress: format!("10.0.0.{seed}:3054").parse().expect("socket"),
        egress: format!("10.0.0.{seed}").parse().expect("ip"),
    }
}

/// The layout golden: drive the real contract on a real chain, then check
/// that every slot the Rust mirror predicts holds exactly the value the
/// contract wrote. `RegistryStateBuilder` is what simulator tests use in
/// place of the contract, so this equivalence is what makes those tests
/// meaningful.
#[test_multisetup([CURRENT_TO_L1])]
async fn contract_storage_matches_the_rust_layout_mirror(main_node: Tester) -> anyhow::Result<()> {
    let chain_id = main_node.l2_provider.get_chain_id().await?;
    let governance = main_node.l2_wallet.default_signer().address();

    // A margin below the nodes' two-epoch lookahead must not be deployable:
    // the contract would accept entries no node can observe in time.
    let too_small_margin = ValidatorRegistry::deploy(
        main_node.l2_provider.clone(),
        governance,
        U256::ZERO,
        U256::from(100u64),
        U256::from(1u64),
    )
    .await;
    assert!(
        too_small_margin.is_err(),
        "a margin below the lookahead must revert at deployment"
    );

    // Deploy with an epoch geometry that keeps the activation-margin guard
    // easy to satisfy, then populate: two identities and one schedule entry.
    let registry = ValidatorRegistry::deploy(
        main_node.l2_provider.clone(),
        governance,
        U256::ZERO,
        U256::from(100u64),
        U256::from(2u64),
    )
    .await?;
    let address = *registry.address();

    let identities = [
        test_identity(1, chain_id, address),
        test_identity(2, chain_id, address),
    ];
    for identity in &identities {
        let mut bls_low = [0u8; 32];
        bls_low[..16].copy_from_slice(&identity.bls_key[32..]);
        registry
            .registerIdentity(
                identity.owner,
                B256::from_slice(&identity.bls_key[..32]),
                B256::from(bls_low),
                B256::from(identity.network_key),
                B256::from_slice(&identity.pop[..32]),
                B256::from_slice(&identity.pop[32..64]),
                B256::from_slice(&identity.pop[64..]),
                zksync_os_consensus_registry::v1::pack_ingress(identity.ingress),
                zksync_os_consensus_registry::v1::pack_egress(identity.egress),
            )
            .send()
            .await?
            .get_receipt()
            .await?;
    }
    registry
        .appendScheduleEntry(U256::from(3u64), vec![U256::ZERO, U256::from(1u64)])
        .send()
        .await?
        .get_receipt()
        .await?;

    // The mirror's image of the same content, compared slot by slot against
    // what the contract actually wrote.
    let mirror = RegistryStateBuilder::new(address)
        .identity(identities[0].clone())
        .identity(identities[1].clone())
        .schedule_entry(3, vec![0, 1]);
    let slots = mirror.build_slots();
    assert!(slots.len() > 20, "the mirror image should be substantial");
    for (slot, expected) in slots {
        let actual = main_node.l2_provider.get_storage_at(address, slot).await?;
        assert_eq!(
            B256::from(actual.to_be_bytes::<32>()),
            expected,
            "slot {slot:#x} diverges between the contract and the Rust mirror",
        );
    }

    // The contract's own reads agree, and the write guards hold: an entry
    // activating in the past (below the margin) must be rejected.
    assert_eq!(registry.identityCount().call().await?, U256::from(2u64));
    assert_eq!(
        registry.scheduleEntryCount().call().await?,
        U256::from(1u64)
    );
    let too_soon = registry
        .appendScheduleEntry(U256::ZERO, vec![U256::ZERO])
        .send()
        .await;
    assert!(too_soon.is_err(), "activation below the margin must revert");

    Ok(())
}

/// Polls every validator's `/status.consensus.registry` until all satisfy the
/// predicate; returns the satisfying snapshots (aligned with validator indices).
async fn wait_for_registry_on_all(
    cluster: &MultiNodeTester,
    what: &str,
    predicate: impl Fn(&zksync_os_status_server::RegistryStatus) -> bool,
) -> anyhow::Result<Vec<zksync_os_status_server::RegistryStatus>> {
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let mut snapshots = Vec::new();
        for index in 0..cluster.len() {
            let registry = cluster
                .node(index)
                .status()
                .await
                .ok()
                .and_then(|status| status.consensus)
                .and_then(|consensus| consensus.registry);
            match registry {
                Some(registry) if predicate(&registry) => snapshots.push(registry),
                _ => break,
            }
        }
        if snapshots.len() == cluster.len() {
            return Ok(snapshots);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "timed out waiting for registry status on all validators: {what}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The shadow-mode L3, end to end on a live committee: governance deploys and
/// populates the real registry contract with the committee's real keys and
/// proofs of possession; every validator's shadow derivation tracks it out of
/// its own chain state and reports a match — then governance schedules a
/// committee the config does not know, and every validator surfaces the drift,
/// identically.
#[test_log::test(tokio::test)]
async fn shadow_registry_tracks_governance_and_surfaces_drift() -> anyhow::Result<()> {
    // Epoch boundaries every ~5s at the 250ms block time; epoch T's committee
    // derives from state at height (T−1)·20 − 1.
    const EPOCH_LENGTH: u64 = 20;

    // The registry's address is configuration, known before deployment: a fresh
    // deployer's first transaction lands at a computable address, so the
    // committee starts with the address configured and an undeployed registry.
    let deployer_signer = PrivateKeySigner::random();
    let registry_address = deployer_signer.address().create(0);

    let cluster =
        MultiNodeTester::start_with_shadow_registry(3, EPOCH_LENGTH, registry_address).await?;

    // Pre-deployment steady state: derivations run, find nothing scheduled, and
    // carry the config committee — quietly (a shadow rollout must not alarm
    // before governance deploys).
    let pre_deployment = wait_for_registry_on_all(&cluster, "pre-deployment carry", |registry| {
        registry.outcome == "carried_no_entry" && registry.matches_config
    })
    .await?;
    assert!(
        pre_deployment
            .iter()
            .all(|registry| registry.mode == "shadow")
    );

    // Governance arrives: fund the deployer, deploy at the precomputed address,
    // and hand ownership of writes to the governance wallet (node 0's rich
    // wallet, which also pays for them).
    let node0 = cluster.node(0);
    let chain_id = node0.l2_provider.get_chain_id().await?;
    let governance_address = node0.l2_wallet.default_signer().address();
    node0
        .l2_provider
        .send_transaction(
            TransactionRequest::default()
                .with_to(deployer_signer.address())
                .with_value(U256::from(10u128.pow(18))),
        )
        .await?
        .get_receipt()
        .await?;
    let deployer = ProviderBuilder::new()
        .wallet(EthereumWallet::from(deployer_signer))
        .connect(node0.l2_rpc_url())
        .await
        .context("failed to connect the deployer to L2")?;
    let deployed = ValidatorRegistry::deploy(
        deployer,
        governance_address,
        U256::ZERO,
        U256::from(EPOCH_LENGTH),
        U256::from(2u64),
    )
    .await?;
    assert_eq!(
        *deployed.address(),
        registry_address,
        "the deployment must land at the configured address"
    );
    let registry = ValidatorRegistry::new(registry_address, node0.l2_provider.clone());

    // Register the committee's real identities: keys parsed from the very
    // entries the validators run with, proofs of possession signed by their
    // real BLS keys.
    for index in 0..cluster.len() {
        let entry = cluster.committee_entry(index);
        let (keys_part, address_part) = entry.split_once('@').context("committee entry format")?;
        let (network_hex, bls_hex) = keys_part.split_once(':').context("committee entry keys")?;
        let network_key: [u8; 32] = alloy::hex::decode(network_hex)?
            .try_into()
            .ok()
            .context("ed25519 key is 32 bytes")?;
        let bls_public: [u8; 48] = alloy::hex::decode(bls_hex)?
            .try_into()
            .ok()
            .context("BLS key is 48 bytes")?;
        let bls_private =
            group::Private::decode(alloy::hex::decode(cluster.bls_key_hex(index))?.as_slice())
                .expect("harness BLS keys decode");
        let pop = zksync_os_consensus_registry::sign_proof_of_possession(
            &bls_private,
            governance_address,
            chain_id,
            registry_address,
        );
        let mut pop_bytes = [0u8; 96];
        pop_bytes.copy_from_slice(&pop.encode());
        let socket: std::net::SocketAddr = address_part.parse()?;
        let mut bls_low = [0u8; 32];
        bls_low[..16].copy_from_slice(&bls_public[32..]);
        registry
            .registerIdentity(
                governance_address,
                B256::from_slice(&bls_public[..32]),
                B256::from(bls_low),
                B256::from(network_key),
                B256::from_slice(&pop_bytes[..32]),
                B256::from_slice(&pop_bytes[32..64]),
                B256::from_slice(&pop_bytes[64..]),
                zksync_os_consensus_registry::v1::pack_ingress(socket),
                zksync_os_consensus_registry::v1::pack_egress(socket.ip()),
            )
            .send()
            .await?
            .get_receipt()
            .await?;
    }

    // Governance schedules the whole committee — the entry every validator's
    // config also declares, so shadow must report a clean match once the
    // lookahead boundary passes.
    let current = registry.currentEpoch().call().await?.to::<u64>();
    let match_epoch = current + 3;
    registry
        .appendScheduleEntry(
            U256::from(match_epoch),
            (0..cluster.len() as u64).map(U256::from).collect(),
        )
        .send()
        .await?
        .get_receipt()
        .await?;
    let matched = wait_for_registry_on_all(&cluster, "derived match", |registry| {
        registry.last_epoch >= match_epoch
    })
    .await?;
    for registry in &matched {
        assert_eq!(registry.outcome, "derived", "{registry:?}");
        assert!(registry.matches_config, "{registry:?}");
    }
    assert!(
        matched
            .windows(2)
            .all(|pair| pair[0].committee_hash == pair[1].committee_hash),
        "validators derived different committees: {matched:?}"
    );

    // The deliberate mismatch: governance schedules a committee the config does
    // not know (validator 2 dropped). Every validator surfaces the drift — and
    // they all derive the *same* wrong-relative-to-config answer, because the
    // derivation is a pure function of chain state.
    let current = registry.currentEpoch().call().await?.to::<u64>();
    let drift_epoch = (match_epoch + 2).max(current + 3);
    registry
        .appendScheduleEntry(
            U256::from(drift_epoch),
            vec![U256::from(0u64), U256::from(1u64)],
        )
        .send()
        .await?
        .get_receipt()
        .await?;
    let drifted = wait_for_registry_on_all(&cluster, "drift surfaced", |registry| {
        registry.last_epoch >= drift_epoch
    })
    .await?;
    for registry in &drifted {
        assert_eq!(registry.outcome, "derived", "{registry:?}");
        assert!(
            !registry.matches_config,
            "the mismatch must surface as drift: {registry:?}"
        );
        assert_eq!(registry.committee_size, 2, "{registry:?}");
    }
    assert!(
        drifted
            .windows(2)
            .all(|pair| pair[0].committee_hash == pair[1].committee_hash),
        "validators derived different committees: {drifted:?}"
    );

    // Consensus itself never looked at any of it: the chain is alive and agreed
    // past everything shadow observed.
    let tip = cluster.max_height().await?;
    cluster
        .wait_for_block_on_all(tip + 3, Duration::from_secs(60))
        .await?;
    cluster.assert_block_hashes_agree(tip + 3).await?;
    Ok(())
}
