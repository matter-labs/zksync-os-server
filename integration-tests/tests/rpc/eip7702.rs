use alloy::eips::eip7702::Authorization;
use alloy::network::{EthereumWallet, TransactionBuilder, TransactionBuilder7702};
use alloy::primitives::U256;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::Counter;
use zksync_os_integration_tests::contracts::Counter::CounterInstance;
use zksync_os_integration_tests::{CURRENT_TO_L1, NEXT_TO_GATEWAY, Tester, test_multisetup};

/// End-to-end test for EIP-7702 (set-code transactions).
///
/// 1. A sponsor (the rich wallet) sends a type-0x04 transaction that carries an authorization
///    signed by a fresh EOA (`authority`) delegating it to a `Counter` contract, and in the same
///    transaction calls `increment(7)` against the authority. This exercises the full path:
///    mempool acceptance, authorization-list application by ZKsync OS, and execution of the
///    delegate's code in the authority's context.
/// 2. We assert the authority's code is the delegation designator `0xef0100 || counter` and that
///    the increment wrote to the authority's own storage.
/// 3. The (now delegated) authority then sends its *own* transaction. This exercises reth's
///    `validate_sender_bytecode`, which reads the sender's delegation bytecode via
///    `bytecode_by_hash` and only admits the transaction because the code is an EIP-7702
///    designator.
#[test_multisetup([CURRENT_TO_L1, NEXT_TO_GATEWAY])]
async fn delegate_and_call(tester: Tester) -> anyhow::Result<()> {
    let provider = tester.l2_provider.clone();
    let sponsor = tester.l2_wallet.default_signer().address();
    let chain_id = provider.get_chain_id().await?;

    // Deploy the contract that the EOA will delegate to.
    let counter = Counter::deploy(provider.clone()).await?;
    let counter_address = *counter.address();

    // A fresh EOA that will delegate its code. It starts with nonce 0 and no code.
    let authority_signer = PrivateKeySigner::random();
    let authority = authority_signer.address();
    assert!(
        provider.get_code_at(authority).await?.is_empty(),
        "authority must start as a plain EOA"
    );

    // Fund the authority up front so it can later submit its own (delegated) transaction.
    provider
        .send_transaction(
            alloy::rpc::types::TransactionRequest::default()
                .with_to(authority)
                .with_value(U256::from(1_000_000_000_000_000_000u64)),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    // The authority signs an authorization delegating itself to the counter contract.
    let authorization = Authorization {
        chain_id: U256::from(chain_id),
        address: counter_address,
        nonce: provider.get_transaction_count(authority).await?,
    };
    let signature = authority_signer.sign_hash_sync(&authorization.signature_hash())?;
    let signed_authorization = authorization.into_signed(signature);

    // Sponsored type-0x04 transaction: delegate `authority -> counter` and, in the same tx, call
    // `increment(7)` against the authority. `from` is the sponsor, so the sponsor pays for gas.
    let delegate_and_call = counter
        .increment(U256::from(7))
        .into_transaction_request()
        .with_from(sponsor)
        .with_to(authority)
        .with_authorization_list(vec![signed_authorization]);
    provider
        .send_transaction(delegate_and_call)
        .await?
        .expect_successful_receipt()
        .await?;

    // The authority's code must now be the EIP-7702 delegation designator `0xef0100 || counter`.
    let code = provider.get_code_at(authority).await?;
    let mut expected_designator = vec![0xef, 0x01, 0x00];
    expected_designator.extend_from_slice(counter_address.as_slice());
    assert_eq!(
        code.as_ref(),
        expected_designator.as_slice(),
        "authority code should be the 7702 delegation designator"
    );

    // The increment ran in the authority's context, so the counter lives in the authority's
    // storage (the original counter contract is untouched).
    let authority_counter = CounterInstance::new(authority, provider.clone());
    assert_eq!(
        authority_counter.counter().call().await?,
        U256::from(7),
        "increment should have written to the authority's storage"
    );
    assert_eq!(
        counter.counter().call().await?,
        U256::ZERO,
        "the delegate contract's own storage must be untouched"
    );

    // The delegated EOA now submits its OWN transaction. Reth's mempool validator reads its
    // delegation bytecode (via `bytecode_by_hash`) and admits it precisely because the code is a
    // 7702 designator. Without that, a sender-with-code would be rejected.
    let authority_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(authority_signer))
        .connect(tester.l2_rpc_url())
        .await?;
    authority_provider
        .send_transaction(
            counter
                .increment(U256::from(5))
                .into_transaction_request()
                .with_from(authority)
                .with_to(authority),
        )
        .await?
        .expect_successful_receipt()
        .await?;

    assert_eq!(
        authority_counter.counter().call().await?,
        U256::from(12),
        "delegated EOA should be able to send its own transaction and bump its counter"
    );

    Ok(())
}
