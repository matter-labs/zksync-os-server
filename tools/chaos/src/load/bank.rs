//! The rig's money: deterministic accounts, one-time L1 funding, fee tracking,
//! and wedged-nonce rescue. Every workload draws from here; none of them owns
//! an account another workload uses.

use crate::setup::Manifest;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Context as _;
use std::time::{Duration, Instant};
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_types::REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE;

/// Anvil's default account #0 — rich on the checked-in L1 state; pays for deposits.
const ANVIL_RICH_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
/// L2 funds per account; traffic spends wei and gas, so this lasts any run.
const FUNDING_ETH: u64 = 100;
/// Gas for a plain transfer. Generous on purpose: zksync-os transfers cost far
/// more than bare 21k (account abstraction + pubdata overhead), and an
/// under-gassed transaction passes pool validation but aborts in the VM at
/// build time — leaving it stuck in the pool as a permanent pending ghost.
pub const TRANSFER_GAS: u64 = 300_000;
/// Fee floors. This chain's basefee tracks L1 pricing and swings by orders of
/// magnitude on a young chain; a transaction priced honestly during a trough
/// becomes unincludable when fees climb — and, being lowest-nonce, wedges its
/// whole sender. Overpaying a floor of 1 gwei costs nothing on a rig and makes
/// pricing insensitive to the swings.
const MIN_MAX_FEE: u128 = 1_000_000_000;
const MIN_TIP: u128 = 1_000;
/// Consecutive maintenance passes without on-chain nonce progress (while a
/// local backlog exists) before a sender's pending range is repriced.
const STALLED_PASSES_BEFORE_REPRICE: u8 = 3;
/// Patient-send policy for the prepare phase (deploys, seeding): these must
/// ride out transient refusals — a validator under pipeline backpressure says
/// "not currently accepting transactions" for a while and then recovers.
const PATIENT_ATTEMPTS: u32 = 12;
const PATIENT_DELAY: Duration = Duration::from_secs(5);

/// One funded account pinned to one validator's RPC.
pub struct Sender {
    pub address: Address,
    /// The raw key, for flows that must sign without sending (the nonce-race
    /// saga submits one signature through two different validators).
    pub signer: PrivateKeySigner,
    pub provider: DynProvider,
    pub validator: usize,
    pub nonce: u64,
    pub last_hash: Option<B256>,
    /// On-chain (latest) nonce at the previous maintenance pass, for detecting a
    /// wedged sender.
    last_chain_nonce: u64,
    stalled_passes: u8,
}

impl Sender {
    /// Sends and retries through transient refusals; for setup traffic, not
    /// the tick loop (which counts errors and moves on by design).
    pub async fn send_patiently(
        &self,
        request: TransactionRequest,
    ) -> anyhow::Result<alloy::providers::PendingTransactionBuilder<alloy::network::Ethereum>> {
        let mut last_error = None;
        for attempt in 0..PATIENT_ATTEMPTS {
            match self.provider.send_transaction(request.clone()).await {
                Ok(pending) => return Ok(pending),
                Err(error) => {
                    let text = error.to_string();
                    println!(
                        "setup send attempt {} of {PATIENT_ATTEMPTS} refused: {}",
                        attempt + 1,
                        text.chars().take(120).collect::<String>(),
                    );
                    last_error = Some(error);
                    tokio::time::sleep(PATIENT_DELAY).await;
                }
            }
        }
        Err(last_error.expect("at least one attempt ran").into())
    }
}

/// All the accounts a load run uses, grouped by purpose:
/// - `senders`: the tick loop's round-robin pool, shared by every tick-driven
///   workload;
/// - `deployer`: deploys workload contracts and sends prepare-phase calls
///   (ERC20 mints), so setup traffic never races the tick loop's nonces;
/// - `racers`: the nonce-race saga's dedicated accounts;
/// - `withdrawer`: the withdrawals saga's L2 account.
pub struct Bank {
    pub senders: Vec<Sender>,
    pub deployer: Sender,
    pub racers: Vec<Sender>,
    pub withdrawer: Option<Sender>,
    pub chain_id: u64,
    /// Cached `(max_fee, tip)`, refreshed by maintenance from whichever
    /// validator answers; stale fees are fine for a rig.
    pub fees: (u128, u128),
}

impl Bank {
    pub async fn build(
        manifest: &Manifest,
        target_of: &[usize],
        sender_count: usize,
        key_seed: u64,
    ) -> anyhow::Result<Bank> {
        let mut senders = Vec::with_capacity(sender_count);
        for index in 0..sender_count {
            let validator = target_of[index % target_of.len()];
            senders.push(build_account(manifest, b"chaos-load", key_seed, index, validator).await?);
        }
        // Two racers, pinned to *different* validators when the cluster has two
        // (the race is same-nonce submissions to two mempools).
        let validators = manifest.validators.len();
        let racers = vec![
            build_account(manifest, b"chaos-load-racer", key_seed, 0, 0).await?,
            build_account(manifest, b"chaos-load-racer", key_seed, 1, 1 % validators).await?,
        ];
        let deployer =
            build_account(manifest, b"chaos-load-deployer", key_seed, 0, target_of[0]).await?;
        let withdrawer = build_account(
            manifest,
            b"chaos-load-withdrawer",
            key_seed,
            0,
            target_of[0],
        )
        .await?;

        let chain_id = deployer.provider.get_chain_id().await?;
        Ok(Bank {
            senders,
            deployer,
            racers,
            withdrawer: Some(withdrawer),
            chain_id,
            fees: (2_000_000_000, 100_000_000),
        })
    }

    /// Funds every account that has no L2 balance yet, through a real bridgehub
    /// deposit per account, then waits for the L2 balances to appear.
    pub async fn fund(&self, manifest: &Manifest) -> anyhow::Result<()> {
        let mut accounts: Vec<&Sender> = Vec::new();
        accounts.extend(self.senders.iter());
        accounts.extend(self.racers.iter());
        accounts.extend(self.withdrawer.iter());
        accounts.push(&self.deployer);

        let mut unfunded = Vec::new();
        for account in accounts {
            let balance = account
                .provider
                .get_balance(account.address)
                .await
                .context("is the cluster up? cannot reach a validator RPC")?;
            if balance == U256::ZERO {
                unfunded.push(account);
            }
        }
        if unfunded.is_empty() {
            println!("all accounts already funded");
            return Ok(());
        }
        println!("funding {} accounts via L1 deposits", unfunded.len());

        let rich: PrivateKeySigner = ANVIL_RICH_KEY.parse()?;
        let l1 = ProviderBuilder::new()
            .wallet(EthereumWallet::from(rich))
            .connect_http(format!("http://127.0.0.1:{}", manifest.host_l1_port).parse()?)
            .erased();
        let bridgehub_address: Address = manifest.bridgehub_address.parse()?;
        let bridgehub = Bridgehub::new(bridgehub_address, l1.clone(), self.chain_id);

        let amount = U256::from(FUNDING_ETH) * U256::from(10u128.pow(18));
        let l1_gas_price = l1.get_gas_price().await?;
        for account in &unfunded {
            let base_cost = bridgehub
                .l2_transaction_base_cost(
                    l1_gas_price.saturating_mul(2),
                    TRANSFER_GAS,
                    REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
                )
                .await?;
            let receipt = l1
                .send_transaction(
                    bridgehub
                        .request_l2_transaction_direct(
                            amount + base_cost,
                            account.address,
                            amount,
                            vec![],
                            TRANSFER_GAS,
                            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
                            account.address,
                        )
                        .value(amount + base_cost)
                        .into_transaction_request(),
                )
                .await?
                .get_receipt()
                .await?;
            anyhow::ensure!(
                receipt
                    .logs()
                    .iter()
                    .any(|log| log.log_decode::<NewPriorityRequest>().is_ok()),
                "deposit for {} produced no priority request",
                account.address,
            );
        }

        // The L1 watcher relays deposits into L2 blocks; wait for the balances.
        let deadline = Instant::now() + Duration::from_secs(180);
        for account in &unfunded {
            loop {
                let balance = account
                    .provider
                    .get_balance(account.address)
                    .await
                    .unwrap_or(U256::ZERO);
                if balance > U256::ZERO {
                    break;
                }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "account {} still unfunded after deposit",
                    account.address,
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        println!("all accounts funded");
        Ok(())
    }

    /// The periodic pass: refresh the fee cache, then revive senders whose
    /// lowest pending transaction became unincludable (e.g. priced during a fee
    /// trough before fees climbed). A stuck range is resubmitted as plain
    /// transfers at current prices — replacing a fancier stuck payload with a
    /// boring transfer is fine on a rig; what matters is that the nonce moves.
    pub async fn maintain(&mut self) -> Vec<(usize, bool)> {
        for sender in self.senders.iter() {
            if let Ok(gas_price) = sender.provider.get_gas_price().await {
                self.fees = (
                    gas_price.saturating_mul(4).max(MIN_MAX_FEE),
                    (gas_price / 10).max(MIN_TIP),
                );
                break;
            }
        }

        let mut submissions = Vec::new();
        let (chain_id, fees) = (self.chain_id, self.fees);
        for (index, sender) in self.senders.iter_mut().enumerate() {
            let Ok(chain_nonce) = sender.provider.get_transaction_count(sender.address).await
            else {
                continue;
            };
            if chain_nonce >= sender.nonce || chain_nonce > sender.last_chain_nonce {
                sender.last_chain_nonce = chain_nonce;
                sender.stalled_passes = 0;
                continue;
            }
            sender.stalled_passes += 1;
            if sender.stalled_passes < STALLED_PASSES_BEFORE_REPRICE {
                continue;
            }
            sender.stalled_passes = 0;
            let upper = sender.nonce.min(chain_nonce + 32);
            println!(
                "repricing wedged sender {} (nonces {chain_nonce}..{upper})",
                sender.address
            );
            for nonce in chain_nonce..upper {
                let request = TransactionRequest::default()
                    .with_chain_id(chain_id)
                    .with_from(sender.address)
                    .with_to(Address::from_word(keccak256(nonce.to_be_bytes())))
                    .with_value(U256::from(1u64))
                    .with_nonce(nonce)
                    .with_gas_limit(TRANSFER_GAS)
                    .with_max_fee_per_gas(fees.0)
                    .with_max_priority_fee_per_gas(fees.1);
                let accepted = match sender.provider.send_transaction(request).await {
                    Ok(pending) => {
                        sender.last_hash = Some(*pending.tx_hash());
                        true
                    }
                    Err(_) => false,
                };
                submissions.push((index, accepted));
            }
        }
        submissions
    }
}

/// Derives and connects one deterministic account. The namespace keeps the
/// account groups (`senders` / `racers` / `deployer`) on disjoint keys for any
/// one seed; resuming a seed against a used cluster continues from the pending
/// nonce, a fresh cluster yields 0.
async fn build_account(
    manifest: &Manifest,
    namespace: &[u8],
    key_seed: u64,
    index: usize,
    validator: usize,
) -> anyhow::Result<Sender> {
    let mut material = namespace.to_vec();
    material.extend_from_slice(&key_seed.to_be_bytes());
    material.extend_from_slice(&index.to_be_bytes());
    let signer = PrivateKeySigner::from_bytes(&keccak256(material))?;
    let address = signer.address();
    let wallet_signer = signer.clone();
    let url = format!(
        "http://127.0.0.1:{}",
        manifest.validators[validator].host_rpc_port
    );
    let provider = ProviderBuilder::new()
        .wallet(EthereumWallet::from(wallet_signer))
        .connect_http(url.parse()?)
        .erased();
    let nonce = provider
        .get_transaction_count(address)
        .pending()
        .await
        .unwrap_or(0);
    Ok(Sender {
        address,
        signer,
        provider,
        validator,
        nonce,
        last_hash: None,
        last_chain_nonce: 0,
        stalled_passes: 0,
    })
}
