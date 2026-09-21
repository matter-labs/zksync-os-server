//! Functional DvP baseline: real RPC admission, serial blocks, and complete state checks.

use super::{RECEIPT_TIMEOUT, corpus, env_or};
use alloy::consensus::{SignableTransaction, TxEip1559};
use alloy::eips::eip2718::Encodable2718;
use alloy::network::{Ethereum, EthereumWallet};
use alloy::primitives::{Address, TxKind, U128, U256, keccak256};
use alloy::providers::{DynProvider, PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionReceipt;
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;
use zksync_os_integration_tests::contracts::TestERC20::TestERC20Instance;
use zksync_os_integration_tests::contracts::{TestDvp, TestERC20};
use zksync_os_integration_tests::{NEXT_TO_L1, TestEnvironment, Tester, test_multisetup};
use zksync_os_server::config::FeeConfig;

// Different amounts make reversed legs / arguments visible in the final state.
const ASSET_AMOUNT: u64 = 3;
const CASH_AMOUNT: u64 = 7;
// Includes two external transferFrom calls and three logs; independent of transfer-only benches.
const DVP_GAS_LIMIT: u64 = 500_000;

async fn launch(env: TestEnvironment) -> anyhow::Result<Tester> {
    let mut config = env.default_config().await?;
    config.prover_input_generator_config.enable_input_generation = false;
    config.batcher_config.enabled = false;
    config.general_config.retain_receipt_logs = true;
    config.fee_config = FeeConfig {
        base_fee_override: Some(U128::ZERO),
        pubdata_price_override: Some(U128::ZERO),
        ..Default::default()
    };
    config.mempool_config.minimal_protocol_basefee = 0;
    config.sequencer_config.parallel_blocks = 1;
    config.sequencer_config.parallel_skip_replay_wal = false;
    env.launch(config).await
}

/// Check status directly: a mined receipt alone does not imply successful execution.
async fn successful(
    pending: PendingTransactionBuilder<Ethereum>,
) -> anyhow::Result<TransactionReceipt> {
    let receipt = pending
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await?;
    anyhow::ensure!(
        receipt.status(),
        "transaction reverted: {}",
        receipt.transaction_hash
    );
    Ok(receipt)
}

struct Tokens {
    asset: TestERC20Instance<DynProvider>,
    cash: TestERC20Instance<DynProvider>,
}

impl Tokens {
    async fn deploy(tester: &Tester) -> anyhow::Result<Self> {
        let provider = DynProvider::new(tester.l2_provider.clone());
        let mut addresses = Vec::new();
        for (name, symbol) in [("DvP asset", "ASSET"), ("DvP cash", "CASH")] {
            let receipt = successful(
                TestERC20::deploy_builder(provider.clone(), U256::ZERO, name.into(), symbol.into())
                    .gas(6_000_000)
                    .gas_price(0)
                    .send()
                    .await?,
            )
            .await?;
            addresses.push(receipt.contract_address.expect("token deployment address"));
        }
        Ok(Self {
            asset: TestERC20::new(addresses[0], provider.clone()),
            cash: TestERC20::new(addresses[1], provider),
        })
    }

    async fn supplies(&self) -> anyhow::Result<[U256; 2]> {
        // Receipts may become visible before the pipelined replay WAL exposes their block
        // to eth_call. This also happens after setup's last approval, before load begins.
        tokio::time::timeout(RECEIPT_TIMEOUT, async {
            let mut supplies = [U256::ZERO; 2];
            for (supply, token) in supplies.iter_mut().zip([&self.asset, &self.cash]) {
                loop {
                    match token.totalSupply().call().await {
                        Ok(value) => {
                            *supply = value;
                            break;
                        }
                        Err(err) if err.to_string().contains("block `latest` not found") => {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        Err(err) => return Err(err.into()),
                    }
                }
            }
            anyhow::Ok(supplies)
        })
        .await?
    }
}

struct Pair {
    buyer: PrivateKeySigner,
    seller: Address,
    buyer_provider: DynProvider,
    seller_provider: DynProvider,
    settlement: Address,
    lots: u64,
}

impl Pair {
    async fn prepare(tester: &Tester, tokens: &Tokens, lots: u64) -> anyhow::Result<Self> {
        Self::prepare_with_buyer(tester, tokens, lots, PrivateKeySigner::random()).await
    }

    async fn prepare_with_buyer(
        tester: &Tester,
        tokens: &Tokens,
        lots: u64,
        buyer: PrivateKeySigner,
    ) -> anyhow::Result<Self> {
        let seller_key = keccak256(format!("loadtest-dvp-seller-{}", buyer.address()));
        let seller = PrivateKeySigner::from_slice(seller_key.as_slice())?;
        let buyer_provider = DynProvider::new(
            ProviderBuilder::new()
                .wallet(EthereumWallet::new(buyer.clone()))
                .connect(tester.l2_rpc_ws_url())
                .await?,
        );
        let seller_provider = DynProvider::new(
            ProviderBuilder::new()
                .wallet(EthereumWallet::new(seller.clone()))
                .connect(tester.l2_rpc_ws_url())
                .await?,
        );
        let receipt = successful(
            TestDvp::deploy_builder(
                tester.l2_provider.clone(),
                *tokens.asset.address(),
                *tokens.cash.address(),
                seller.address(),
                buyer.address(),
                U256::from(ASSET_AMOUNT),
                U256::from(CASH_AMOUNT),
            )
            .gas(3_000_000)
            .gas_price(0)
            .send()
            .await?,
        )
        .await?;
        let pair = Self {
            buyer,
            seller: seller.address(),
            buyer_provider,
            seller_provider,
            settlement: receipt.contract_address.expect("DvP deployment address"),
            lots,
        };
        for (token, owner, provider, amount) in [
            (
                &tokens.asset,
                pair.seller,
                &pair.seller_provider,
                ASSET_AMOUNT,
            ),
            (
                &tokens.cash,
                pair.buyer.address(),
                &pair.buyer_provider,
                CASH_AMOUNT,
            ),
        ] {
            let budget = U256::from(lots) * U256::from(amount);
            successful(
                token
                    .mint(owner, budget)
                    .gas(300_000)
                    .gas_price(0)
                    .send()
                    .await?,
            )
            .await?;
            successful(
                TestERC20::new(*token.address(), provider.clone())
                    .approve(pair.settlement, budget)
                    .gas(100_000)
                    .gas_price(0)
                    .send()
                    .await?,
            )
            .await?;
        }
        Ok(pair)
    }

    /// Four party balances, two allowances, and both settlement-contract balances.
    async fn state(&self, tokens: &Tokens) -> anyhow::Result<[U256; 8]> {
        Ok([
            tokens.asset.balanceOf(self.seller).call().await?,
            tokens.asset.balanceOf(self.buyer.address()).call().await?,
            tokens.cash.balanceOf(self.buyer.address()).call().await?,
            tokens.cash.balanceOf(self.seller).call().await?,
            tokens
                .asset
                .allowance(self.seller, self.settlement)
                .call()
                .await?,
            tokens
                .cash
                .allowance(self.buyer.address(), self.settlement)
                .call()
                .await?,
            tokens.asset.balanceOf(self.settlement).call().await?,
            tokens.cash.balanceOf(self.settlement).call().await?,
        ])
    }

    async fn assert_settled(&self, tokens: &Tokens, count: u64) -> anyhow::Result<()> {
        anyhow::ensure!(count <= self.lots, "settled more than the funded budget");
        let asset_left = U256::from(self.lots - count) * U256::from(ASSET_AMOUNT);
        let cash_left = U256::from(self.lots - count) * U256::from(CASH_AMOUNT);
        assert_eq!(
            self.state(tokens).await?,
            [
                asset_left,
                U256::from(count) * U256::from(ASSET_AMOUNT),
                cash_left,
                U256::from(count) * U256::from(CASH_AMOUNT),
                asset_left,
                cash_left,
                U256::ZERO,
                U256::ZERO,
            ],
            "incorrect settlement state for {}",
            self.settlement
        );
        Ok(())
    }

    async fn submit(
        &self,
        chain_id: u64,
        nonce: u64,
    ) -> anyhow::Result<PendingTransactionBuilder<Ethereum>> {
        Ok(self
            .buyer_provider
            .send_raw_transaction(&self.signed_transaction(chain_id, nonce)?)
            .await?)
    }

    fn signed_transaction(&self, chain_id: u64, nonce: u64) -> anyhow::Result<Vec<u8>> {
        let tx = TxEip1559 {
            chain_id,
            nonce,
            gas_limit: DVP_GAS_LIMIT,
            max_fee_per_gas: 0,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(self.settlement),
            value: U256::ZERO,
            access_list: Default::default(),
            input: TestDvp::settleCall {}.abi_encode().into(),
        };
        let signature = self.buyer.sign_hash_sync(&tx.signature_hash())?;
        Ok(tx.into_signed(signature).encoded_2718())
    }

    fn assert_events(&self, tokens: &Tokens, receipt: &TransactionReceipt) -> anyhow::Result<()> {
        let logs = receipt.inner.logs();
        anyhow::ensure!(
            logs.len() == 3,
            "expected two transfers and one settlement event, got {logs:?}"
        );
        for (log, token, from, to, amount) in [
            (
                &logs[0],
                tokens.asset.address(),
                self.seller,
                self.buyer.address(),
                ASSET_AMOUNT,
            ),
            (
                &logs[1],
                tokens.cash.address(),
                self.buyer.address(),
                self.seller,
                CASH_AMOUNT,
            ),
        ] {
            assert_eq!(log.address(), *token);
            let transfer = log.log_decode::<TestERC20::Transfer>()?.inner.data;
            assert_eq!(
                (transfer.from, transfer.to, transfer.value),
                (from, to, U256::from(amount))
            );
        }
        assert_eq!(logs[2].address(), self.settlement);
        let settled = logs[2].log_decode::<TestDvp::Settled>()?.inner.data;
        assert_eq!(
            (
                settled.buyer,
                settled.seller,
                settled.assetAmount,
                settled.cashAmount
            ),
            (
                self.buyer.address(),
                self.seller,
                U256::from(ASSET_AMOUNT),
                U256::from(CASH_AMOUNT)
            )
        );
        Ok(())
    }
}

/// The parallel harness routes by buyer. Each buyer has an exclusive seller, settlement
/// contract, and balance/allowance slots in the shared tokens, so lanes never share writes.
pub(super) struct ParallelDvp {
    tokens: Tokens,
    pairs: Vec<Pair>,
    initial_nonces: Vec<u64>,
    supplies: [U256; 2],
}

impl ParallelDvp {
    pub(super) async fn prepare(
        tester: &Tester,
        buyers: &[PrivateKeySigner],
        lots: u64,
    ) -> anyhow::Result<Self> {
        let tokens = Tokens::deploy(tester).await?;
        let mut pairs = Vec::with_capacity(buyers.len());
        let mut initial_nonces = Vec::with_capacity(buyers.len());
        let provider = DynProvider::new(tester.l2_provider.clone());
        let chain_id = provider.get_chain_id().await?;
        let deployer = tester.l2_wallet.default_signer().address();
        let mut nonce = provider.get_transaction_count(deployer).await?;
        // Queue setup in bounded batches instead of waiting for a block after every tx.
        // Reducing block_time to accelerate serial setup also caps parallel block age,
        // which changes the timed workload. Keep the ordinary block timing instead.
        for batch in buyers.chunks(128) {
            let mut pending = Vec::with_capacity(batch.len() * 5);
            for buyer in batch {
                let seller_key = keccak256(format!("loadtest-dvp-seller-{}", buyer.address()));
                let seller = PrivateKeySigner::from_slice(seller_key.as_slice())?;
                let settlement = deployer.create(nonce);
                pending.push((
                    TestDvp::deploy_builder(
                        provider.clone(),
                        *tokens.asset.address(),
                        *tokens.cash.address(),
                        seller.address(),
                        buyer.address(),
                        U256::from(ASSET_AMOUNT),
                        U256::from(CASH_AMOUNT),
                    )
                    .gas(3_000_000)
                    .gas_price(0)
                    .send()
                    .await?,
                    Some(settlement),
                ));
                nonce += 1;
                // Preserve deploy/mint/mint nonce order so cached corpus addresses stay valid.
                for (token, owner, amount) in [
                    (&tokens.asset, &seller, ASSET_AMOUNT),
                    (&tokens.cash, buyer, CASH_AMOUNT),
                ] {
                    let budget = U256::from(lots) * U256::from(amount);
                    pending.push((
                        token
                            .mint(owner.address(), budget)
                            .gas(300_000)
                            .gas_price(0)
                            .send()
                            .await?,
                        None,
                    ));
                    nonce += 1;
                    let approval = TxEip1559 {
                        chain_id,
                        nonce: 0, // Each deterministic buyer/seller is fresh on this test chain.
                        gas_limit: 100_000,
                        max_fee_per_gas: 0,
                        max_priority_fee_per_gas: 0,
                        to: TxKind::Call(*token.address()),
                        value: U256::ZERO,
                        access_list: Default::default(),
                        input: TestERC20::approveCall {
                            _spender: settlement,
                            _value: budget,
                        }
                        .abi_encode()
                        .into(),
                    };
                    let signature = owner.sign_hash_sync(&approval.signature_hash())?;
                    pending.push((
                        provider
                            .send_raw_transaction(&approval.into_signed(signature).encoded_2718())
                            .await?,
                        None,
                    ));
                }
                pairs.push(Pair {
                    buyer: buyer.clone(),
                    seller: seller.address(),
                    buyer_provider: provider.clone(),
                    seller_provider: provider.clone(),
                    settlement,
                    lots,
                });
            }
            for (tx, deployment) in pending {
                let receipt = successful(tx).await?;
                if let Some(address) = deployment {
                    anyhow::ensure!(
                        receipt.contract_address == Some(address),
                        "unexpected DvP deployment address"
                    );
                }
            }
            for buyer in batch {
                let nonce = provider.get_transaction_count(buyer.address()).await?;
                anyhow::ensure!(nonce == 1, "DvP buyer approval nonce mismatch");
                initial_nonces.push(nonce);
            }
        }
        let supplies = tokens.supplies().await?;
        Ok(Self {
            tokens,
            pairs,
            initial_nonces,
            supplies,
        })
    }

    pub(super) fn initial_nonce(&self, index: usize) -> u64 {
        self.initial_nonces[index]
    }

    pub(super) fn ensure_corpus(
        &self,
        chain_id: u64,
        index: usize,
        count: u64,
    ) -> anyhow::Result<std::path::PathBuf> {
        let pair = &self.pairs[index];
        let nonce = self.initial_nonce(index);
        let mut fingerprint = vec![chain_id, 1, nonce, DVP_GAS_LIMIT, ASSET_AMOUNT, CASH_AMOUNT];
        for address in [
            pair.settlement,
            pair.buyer.address(),
            pair.seller,
            *self.tokens.asset.address(),
            *self.tokens.cash.address(),
        ] {
            fingerprint.extend(
                address
                    .as_slice()
                    .chunks_exact(4)
                    .map(|bytes| u64::from(u32::from_be_bytes(bytes.try_into().unwrap()))),
            );
        }
        let path = corpus::signer_file("rpc-parallel-dvp", index);
        corpus::ensure_corpus(&path, count, corpus::fingerprint(&fingerprint), |n| {
            pair.signed_transaction(chain_id, nonce + n)
                .expect("sign DvP corpus transaction")
        })?;
        Ok(path)
    }

    pub(super) async fn verify(&self, counts: &[u64]) -> anyhow::Result<()> {
        assert_eq!(counts.len(), self.pairs.len());
        // Wait for the replay WAL before querying each pair's state.
        assert_eq!(self.tokens.supplies().await?, self.supplies);
        for (i, (pair, &count)) in self.pairs.iter().zip(counts).enumerate() {
            pair.assert_settled(&self.tokens, count).await?;
            assert_eq!(
                pair.buyer_provider
                    .get_transaction_count(pair.buyer.address())
                    .await?,
                self.initial_nonces[i] + count,
                "DvP buyer nonce mismatch"
            );
        }
        assert_eq!(self.tokens.supplies().await?, self.supplies);
        Ok(())
    }
}

#[test_multisetup([NEXT_TO_L1])]
async fn dvp_settlement(env: TestEnvironment) -> anyhow::Result<()> {
    let tester = launch(env).await?;
    let tokens = Tokens::deploy(&tester).await?;
    let pair = Pair::prepare(&tester, &tokens, 2).await?;
    let supplies = tokens.supplies().await?;
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let nonce = pair
        .buyer_provider
        .get_transaction_count(pair.buyer.address())
        .await?;
    pair.assert_settled(&tokens, 0).await?;
    for i in 0..2 {
        let receipt = successful(pair.submit(chain_id, nonce + i).await?).await?;
        pair.assert_events(&tokens, &receipt)?;
        pair.assert_settled(&tokens, i + 1).await?;
        tracing::info!(gas_used = receipt.gas_used, "DvP settlement gas");
    }
    let exhausted = pair
        .submit(chain_id, nonce + 2)
        .await?
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await?;
    assert!(
        !exhausted.status(),
        "settlement must respect the approved budget"
    );
    assert!(exhausted.inner.logs().is_empty());
    pair.assert_settled(&tokens, 2).await?;
    assert_eq!(tokens.supplies().await?, supplies);
    Ok(())
}

#[test_multisetup([NEXT_TO_L1])]
async fn dvp_rollback(env: TestEnvironment) -> anyhow::Result<()> {
    let tester = launch(env).await?;
    let tokens = Tokens::deploy(&tester).await?;
    for (cash_leg, insufficient_balance) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        let pair = Pair::prepare(&tester, &tokens, 1).await?;
        let (token, provider, amount) = if cash_leg {
            (&tokens.cash, &pair.buyer_provider, CASH_AMOUNT)
        } else {
            (&tokens.asset, &pair.seller_provider, ASSET_AMOUNT)
        };
        let token = TestERC20::new(*token.address(), provider.clone());
        let pending = if insufficient_balance {
            // Move one unit away, leaving allowance intact. transferFrom will decrement
            // allowance before failing its balance check, exercising that rollback too.
            token
                .transfer(Address::repeat_byte(0x42), U256::from(1))
                .gas(150_000)
                .gas_price(0)
                .send()
                .await?
        } else {
            token
                .approve(pair.settlement, U256::from(amount - 1))
                .gas(100_000)
                .gas_price(0)
                .send()
                .await?
        };
        successful(pending).await?;
        let before = pair.state(&tokens).await?;
        let supplies = tokens.supplies().await?;
        // Explicit gas ensures the reverted transaction is actually mined, not rejected by estimation.
        let receipt = TestDvp::new(pair.settlement, pair.buyer_provider.clone())
            .settle()
            .gas(DVP_GAS_LIMIT)
            .gas_price(0)
            .send()
            .await?
            .with_timeout(Some(RECEIPT_TIMEOUT))
            .get_receipt()
            .await?;
        assert!(
            !receipt.status(),
            "expected revert: cash_leg={cash_leg}, insufficient_balance={insufficient_balance}"
        );
        assert!(
            receipt.inner.logs().is_empty(),
            "reverted transfers must leave no logs"
        );
        assert_eq!(
            pair.state(&tokens).await?,
            before,
            "partial settlement survived a revert"
        );
        assert_eq!(tokens.supplies().await?, supplies);
    }
    Ok(())
}

#[test_multisetup([NEXT_TO_L1])]
async fn dvp_unauthorized(env: TestEnvironment) -> anyhow::Result<()> {
    let tester = launch(env).await?;
    let tokens = Tokens::deploy(&tester).await?;
    let pair = Pair::prepare(&tester, &tokens, 1).await?;
    let supplies = tokens.supplies().await?;
    let receipt = TestDvp::new(pair.settlement, pair.seller_provider.clone())
        .settle()
        .gas(DVP_GAS_LIMIT)
        .gas_price(0)
        .send()
        .await?
        .with_timeout(Some(RECEIPT_TIMEOUT))
        .get_receipt()
        .await?;
    assert!(
        !receipt.status(),
        "seller must not be able to trigger settlement"
    );
    assert!(receipt.inner.logs().is_empty());
    pair.assert_settled(&tokens, 0).await?;
    assert_eq!(tokens.supplies().await?, supplies);
    Ok(())
}

/// One transaction = one atomic swap = two token transfers. Setup is excluded from timing.
/// Stops at LOAD_TEST_DURATION_SECS (default 10) or LOAD_TEST_DVP_MAX_SWAPS_PER_PAIR (100),
/// whichever comes first. LOAD_TEST_DVP_PAIRS (4) and LOAD_TEST_CONCURRENCY (4) bound load.
#[test_multisetup([NEXT_TO_L1])]
#[test_runtime(flavor = "multi_thread")]
async fn effective_dvp_tps(env: TestEnvironment) -> anyhow::Result<()> {
    let duration = Duration::from_secs(env_or("LOAD_TEST_DURATION_SECS", 10));
    let pair_count: usize = env_or("LOAD_TEST_DVP_PAIRS", 4);
    let max_swaps: u64 = env_or("LOAD_TEST_DVP_MAX_SWAPS_PER_PAIR", 100);
    let concurrency: usize = env_or("LOAD_TEST_CONCURRENCY", 4);
    anyhow::ensure!(
        !duration.is_zero() && pair_count > 0 && max_swaps > 0 && concurrency > 0,
        "duration, pair count, maximum swaps, and concurrency must be positive"
    );
    let tester = launch(env).await?;
    let tokens = Tokens::deploy(&tester).await?;
    let chain_id = tester.l2_provider.get_chain_id().await?;
    let mut pairs = Vec::with_capacity(pair_count);
    for _ in 0..pair_count {
        let pair = Pair::prepare(&tester, &tokens, max_swaps).await?;
        // Setup includes the buyer's approval, so nonce zero is already consumed.
        let nonce = pair
            .buyer_provider
            .get_transaction_count(pair.buyer.address())
            .await?;
        pairs.push((pair, nonce));
    }
    let supplies = tokens.supplies().await?;
    let tokens = Arc::new(tokens);
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let start = Instant::now();
    let deadline = start + duration;
    let mut tasks = JoinSet::new();
    for (pair, initial_nonce) in pairs {
        let semaphore = semaphore.clone();
        let tokens = tokens.clone();
        tasks.spawn(async move {
            let mut completed = 0u64;
            let mut latency = Duration::ZERO;
            let mut gas = 0u128;
            let mut last_submission = start;
            while completed < max_swaps && Instant::now() < deadline {
                let Ok(permit) = tokio::time::timeout_at(deadline, semaphore.acquire()).await
                else {
                    break;
                };
                let permit = permit?;
                if Instant::now() >= deadline {
                    break;
                }
                let sent_at = Instant::now();
                let receipt = tokio::time::timeout(RECEIPT_TIMEOUT, async {
                    let pending = pair.submit(chain_id, initial_nonce + completed).await?;
                    last_submission = Instant::now();
                    successful(pending).await
                })
                .await??;
                latency += sent_at.elapsed();
                gas += u128::from(receipt.gas_used);
                pair.assert_events(&tokens, &receipt)?;
                completed += 1;
                drop(permit);
            }
            anyhow::Ok((pair, completed, latency, gas, last_submission))
        });
    }
    let mut outcomes = Vec::with_capacity(pair_count);
    while let Some(outcome) = tasks.join_next().await {
        outcomes.push(outcome??);
    }
    // Freeze measurement before RPC state verification.
    let finished = Instant::now();
    let elapsed = finished - start;
    let mut completed = 0u64;
    let mut latency = Duration::ZERO;
    let mut gas = 0u128;
    let mut last_submission = start;
    for (pair, count, pair_latency, pair_gas, pair_last_submission) in outcomes {
        pair.assert_settled(&tokens, count).await?;
        completed += count;
        latency += pair_latency;
        gas += pair_gas;
        last_submission = last_submission.max(pair_last_submission);
    }
    assert_eq!(
        tokens.supplies().await?,
        supplies,
        "settlement changed token supply"
    );
    anyhow::ensure!(completed > 0, "no DvP swaps completed");
    tracing::info!(
        pair_count, concurrency, completed, ?elapsed,
        drain_duration = ?finished.saturating_duration_since(last_submission),
        swaps_per_second = completed as f64 / elapsed.as_secs_f64(),
        avg_receipt_latency_ms = latency.as_secs_f64() * 1000.0 / completed as f64,
        avg_gas_per_swap = gas as f64 / completed as f64,
        "DvP load test complete"
    );
    Ok(())
}
