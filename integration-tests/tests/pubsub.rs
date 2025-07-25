use alloy::consensus::transaction::{Recovered, TransactionInfo};
use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, IntoLogData, TxHash, U256};
use alloy::providers::Provider;
use alloy::pubsub::Subscription;
use alloy::rpc::json_rpc::RpcRecv;
use alloy::rpc::types::{Filter, Header, Log, Transaction, TransactionRequest};
use alloy::sol_types::SolEvent;
use futures::StreamExt;
use tokio::time::error::Elapsed;
use zksync_os_integration_tests::Tester;
use zksync_os_integration_tests::assert_traits::ReceiptAssert;
use zksync_os_integration_tests::contracts::EventEmitter;
use zksync_os_integration_tests::contracts::EventEmitter::{EventEmitterInstance, TestEvent};
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;

trait PubsubSuite: Sized {
    type Expected: RpcRecv + PartialEq;

    /// Initialize the test suite with custom logic (e.g., this is where pre-requisite contracts are
    /// deployed).
    async fn init(tester: &Tester) -> anyhow::Result<Self>;

    /// Returns a new subscription with the L2 node.
    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>>;

    /// Run custom logic to change L2 node's state and hence make the subscription pick it up. Returns
    /// an item expected to be seen in the subscription stream when polled next time.
    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected>;
}

async fn run_test<S: PubsubSuite>() -> anyhow::Result<()> {
    let tester = Tester::setup().await?;
    let suite = S::init(&tester).await?;
    let mut stream = suite.subscribe(&tester).await?.into_stream();
    let expected_item = suite.prepare_expected(&tester).await?;

    let actual_item = stream.next().await.expect("stream ended unexpectedly");
    assert_eq!(
        actual_item, expected_item,
        "next item in stream should match expected item"
    );

    let _: Elapsed = tokio::time::timeout(std::time::Duration::from_secs(1), stream.next())
        .await
        .expect_err("stream should not return anything after expected item");

    Ok(())
}

struct NewBlockSuite;

impl PubsubSuite for NewBlockSuite {
    type Expected = Header;

    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(NewBlockSuite)
    }

    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>> {
        Ok(tester.l2_provider.subscribe_blocks().await?)
    }

    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        // Submit a transaction and wait for it to get mined, thus producing a new block
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

        // Get expected block header from JSON-RPC API
        let block_hash = receipt.block_hash.expect("receipt has no block hash");
        let block = tester
            .l2_provider
            .get_block_by_hash(block_hash)
            .hashes()
            .await?
            .expect("could not retrieve block header");
        Ok(block.header)
    }
}

struct PendingTxSuite<const FULL: bool>;

impl PubsubSuite for PendingTxSuite<false> {
    type Expected = TxHash;

    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(PendingTxSuite)
    }

    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>> {
        Ok(tester.l2_provider.subscribe_pending_transactions().await?)
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

impl PubsubSuite for PendingTxSuite<true> {
    type Expected = Transaction;

    async fn init(_tester: &Tester) -> anyhow::Result<Self> {
        Ok(PendingTxSuite)
    }

    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>> {
        Ok(tester
            .l2_provider
            .subscribe_full_pending_transactions()
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

impl PubsubSuite for NewLogsSuite {
    type Expected = Log;

    async fn init(tester: &Tester) -> anyhow::Result<Self> {
        let event_emitter = EventEmitter::deploy(tester.l2_provider.clone()).await?;
        Ok(NewLogsSuite { event_emitter })
    }

    async fn subscribe(&self, tester: &Tester) -> anyhow::Result<Subscription<Self::Expected>> {
        let filter = Filter::new()
            .address(*self.event_emitter.address())
            .event_signature(TestEvent::SIGNATURE_HASH);
        Ok(tester.l2_provider.subscribe_logs(&filter).await?)
    }

    async fn prepare_expected(&self, tester: &Tester) -> anyhow::Result<Self::Expected> {
        // Make `EventEmitter` emit `TestEvent` with the given number.
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
}

#[test_log::test(tokio::test)]
async fn new_block_pubsub() -> anyhow::Result<()> {
    // Test that `eth_subscribe` can subscribe to new block headers
    run_test::<NewBlockSuite>().await
}

#[test_log::test(tokio::test)]
async fn pending_tx_hash_pubsub() -> anyhow::Result<()> {
    // Test that `eth_subscribe` can subscribe to pending transaction hashes
    run_test::<PendingTxSuite<false>>().await
}

#[test_log::test(tokio::test)]
async fn pending_tx_full_pubsub() -> anyhow::Result<()> {
    // Test that `eth_subscribe` can subscribe to pending transactions
    run_test::<PendingTxSuite<true>>().await
}

#[test_log::test(tokio::test)]
async fn new_log_pubsub() -> anyhow::Result<()> {
    // Test that `eth_subscribe` can subscribe to new logs
    run_test::<NewLogsSuite>().await
}
