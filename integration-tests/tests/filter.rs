use alloy::consensus::transaction::{Recovered, TransactionInfo};
use alloy::network::{TransactionBuilder, TxSigner};
use alloy::primitives::{Address, BlockHash, IntoLogData, TxHash, U256};
use alloy::providers::Provider;
use alloy::rpc::json_rpc::RpcRecv;
use alloy::rpc::types::{Filter, Log, Transaction, TransactionRequest};
use alloy::sol_types::SolEvent;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::EventEmitter;
use zksync_os_integration_tests::contracts::EventEmitter::{EventEmitterInstance, TestEvent};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;

trait FilterSuite: Sized {
    type Expected: RpcRecv + PartialEq;

    /// Initialize the test suite with custom logic (e.g., this is where pre-requisite contracts are
    /// deployed).
    async fn init(tester: &Tester) -> anyhow::Result<Self>;

    /// Register a new filter with the L2 node. Returns its id.
    async fn create_filter(&self, tester: &Tester) -> anyhow::Result<U256>;

    /// Run custom logic to change L2 node's state and hence make the filter pick it up. Returns
    /// a change (as defined per `eth_getFilterChanges`) expected to be picked up by the filter after
    /// this method's invocation.
    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected>;

    /// Run custom logic before filter is uninstalled.
    async fn before_uninstall_hook(
        &self,
        _tester: &Tester,
        _filter_id: U256,
        _expected: Self::Expected,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

async fn run_test<S: FilterSuite>() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;
    let suite = S::init(&tester).await?;
    let filter_id = suite.create_filter(&tester).await?;
    let expected_change = suite.prepare_expected(&tester).await?;

    let actual_changes = tester
        .l2_provider
        .get_filter_changes::<S::Expected>(filter_id)
        .await?;
    tracing::debug!(?expected_change, ?actual_changes, "comparing changes");
    assert!(
        actual_changes.contains(&expected_change),
        "filter changes do not contain expected element"
    );

    let extra_changes = tester
        .l2_provider
        .get_filter_changes::<S::Expected>(filter_id)
        .await?;
    assert!(
        extra_changes.is_empty(),
        "filter should not pick up any extra changes after suite is over"
    );

    suite
        .before_uninstall_hook(&tester, filter_id, expected_change)
        .await?;
    tester.l2_provider.uninstall_filter(filter_id).await?;
    let err = tester
        .l2_provider
        .get_filter_changes::<S::Expected>(filter_id)
        .await
        .expect_err("filter should be uninstalled");
    assert!(
        err.to_string().contains("filter not found"),
        "`eth_getFilterChanges` should report uninstalled filter as not found"
    );
    let err = tester
        .l2_provider
        .get_filter_logs(filter_id)
        .await
        .expect_err("filter should be uninstalled");
    assert!(
        err.to_string().contains("filter not found"),
        "`eth_getFilterLogs` should report uninstalled filter as not found"
    );

    Ok(())
}

struct NewBlockSuite;

impl FilterSuite for NewBlockSuite {
    type Expected = BlockHash;

    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(NewBlockSuite)
    }

    async fn create_filter(&self, tester: &Tester) -> anyhow::Result<U256> {
        Ok(tester.l2_provider.new_block_filter().await?)
    }

    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        let receipt = tester
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(Address::random())
                    .with_value(U256::from(100)),
            )
            .await?
            .expect_successful_receipt()
            .await?;
        Ok(receipt.block_hash.expect("receipt has no block hash"))
    }
}

struct PendingTxSuite<const FULL: bool>;

impl FilterSuite for PendingTxSuite<false> {
    type Expected = TxHash;

    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(PendingTxSuite)
    }

    async fn create_filter(&self, tester: &Tester) -> anyhow::Result<U256> {
        Ok(tester
            .l2_provider
            .new_pending_transactions_filter(false)
            .await?)
    }

    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        let pending_tx = tester
            .l2_provider
            .send_transaction(
                TransactionRequest::default()
                    .with_to(Address::random())
                    .with_value(U256::from(100)),
            )
            .await?
            .expect_register()
            .await?;
        Ok(*pending_tx.tx_hash())
    }
}

impl FilterSuite for PendingTxSuite<true> {
    type Expected = Transaction;

    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(PendingTxSuite)
    }

    async fn create_filter(&self, tester: &Tester) -> anyhow::Result<U256> {
        Ok(tester
            .l2_provider
            .new_pending_transactions_filter(true)
            .await?)
    }

    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        // Build and sign transaction without L2 provider. This way we can reuse the envelope for
        // the expected RPC type below.
        let tx_envelope = TransactionRequest::default()
            .with_to(Address::random())
            .with_value(U256::from(100))
            .with_nonce(0)
            .with_gas_limit(100_000)
            .with_max_fee_per_gas(1000)
            .with_max_priority_fee_per_gas(0)
            .with_chain_id(tester.l2_provider.get_chain_id().await?)
            .build(&tester.l2_wallet)
            .await?;
        tester
            .l2_provider
            .send_tx_envelope(tx_envelope.clone())
            .await?
            .expect_register()
            .await?;
        let transaction = Transaction::from_transaction(
            Recovered::new_unchecked(tx_envelope, tester.l2_wallet.default_signer().address()),
            TransactionInfo::default(),
        );
        Ok(transaction)
    }
}

struct NewLogsSuite {
    event_emitter: EventEmitterInstance<EthDynProvider>,
}

impl FilterSuite for NewLogsSuite {
    type Expected = Log;

    async fn init(tester: &Tester) -> anyhow::Result<Self> {
        let event_emitter = EventEmitter::deploy(tester.l2_provider.clone()).await?;
        Ok(NewLogsSuite { event_emitter })
    }

    async fn create_filter(&self, tester: &Tester) -> anyhow::Result<U256> {
        let filter = Filter::new()
            .address(*self.event_emitter.address())
            .event_signature(TestEvent::SIGNATURE_HASH);
        Ok(tester.l2_provider.new_filter(&filter).await?)
    }

    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        let event_number = U256::from(42);
        let receipt = self
            .event_emitter
            .emitEvent(event_number)
            .send()
            .await?
            .expect_successful_receipt()
            .await?;
        let block = tester
            .l2_provider
            .get_block_by_number(receipt.block_number.unwrap().into())
            .await?
            .expect("no block found");

        let event = TestEvent {
            number: event_number,
        };
        Ok(Log {
            inner: alloy::primitives::Log {
                address: *self.event_emitter.address(),
                data: event.into_log_data(),
            },
            block_hash: receipt.block_hash,
            block_number: Some(block.header.number),
            block_timestamp: Some(block.header.timestamp),
            transaction_hash: Some(receipt.transaction_hash),
            transaction_index: receipt.transaction_index,
            log_index: Some(0),
            removed: false,
        })
    }

    async fn before_uninstall_hook(
        &self,
        tester: &Tester,
        filter_id: U256,
        expected: Self::Expected,
    ) -> anyhow::Result<()> {
        let logs = tester.l2_provider.get_filter_logs(filter_id).await?;
        assert!(
            logs.contains(&expected),
            "`eth_getFilterLogs` should contain the log too"
        );
        let logs = tester.l2_provider.get_filter_logs(filter_id).await?;
        assert!(
            logs.contains(&expected),
            "`eth_getFilterLogs` should return the log again when called for second time"
        );

        Ok(())
    }
}

#[test_log::test(tokio::test)]
async fn new_block_filter() -> anyhow::Result<()> {
    // Test that `eth_newBlockFilter` picks up new canonized blocks
    run_test::<NewBlockSuite>().await
}

#[test_log::test(tokio::test)]
async fn pending_tx_hash_filter() -> anyhow::Result<()> {
    // Test that `eth_newPendingTransactionFilter(full=false)` picks up new pending transactions' hashes
    run_test::<PendingTxSuite<false>>().await
}

#[test_log::test(tokio::test)]
async fn pending_tx_full_filter() -> anyhow::Result<()> {
    // Test that `eth_newPendingTransactionFilter(full=true)` picks up new pending transactions
    run_test::<PendingTxSuite<true>>().await
}

#[test_log::test(tokio::test)]
async fn new_log_filter() -> anyhow::Result<()> {
    // Test that:
    // * `eth_newFilter` picks up new logs
    // * `eth_getFilterLogs` returns all matching logs (regardless of what was already polled through `eth_getFilterChanges`)
    run_test::<NewLogsSuite>().await
}
