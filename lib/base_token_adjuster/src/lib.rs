use crate::metrics::{METRICS, OperationResult, OperationResultLabels};
use alloy::network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy::primitives::Address;
use alloy::primitives::utils::format_ether;
use alloy::providers::ext::DebugApi;
use alloy::providers::fillers::{FillProvider, TxFiller};
use alloy::providers::{DynProvider, Provider, WalletProvider};
use alloy::rpc::types::TransactionReceipt;
use alloy::rpc::types::trace::geth::{CallConfig, GethDebugTracingOptions};
use anyhow::Context;
use num::rational::Ratio;
use num::{BigUint, ToPrimitive};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use zksync_os_contract_interface::{
    IChainAdminOwnable::{self, IChainAdminOwnableInstance},
    IERC20, ZkChain,
};
use zksync_os_external_price_api::cmc_api::CmcPriceApiClient;
use zksync_os_external_price_api::coingecko_api::CoinGeckoPriceAPIClient;
use zksync_os_external_price_api::forced_price_client::ForcedPriceClient;
use zksync_os_external_price_api::{
    APIToken, ExternalPriceApiClientConfig, PriceApiClient, ZK_L1_ADDRESS,
};
use zksync_os_operator_signer::SignerConfig;
use zksync_os_types::{TokenApiRatio, TokenPricesForFees};

mod metrics;

#[derive(Debug, Clone)]
pub struct BaseTokenPriceUpdaterConfig {
    /// How often to fetch external prices.
    pub price_polling_interval: Duration,
    /// How many percent a quote needs to change in order for update to be propagated to L1.
    /// Exists to save on gas.
    pub l1_update_deviation_percentage: u32,
    /// Maximum number of attempts to fetch quote from a remote API before failing over.
    pub price_fetching_max_attempts: u32,
    /// Override for address of the base token address.
    pub base_token_addr_override: Option<Address>,
    /// Override for decimals of the base token.
    pub base_token_decimals_override: Option<u8>,
    /// Override for address of the gateway base token address used to calculate ETH<->GatewayBaseToken ratio on gateway using chains.
    pub gateway_base_token_addr_override: Option<Address>,
    /// Signer configuration to update base token price on L1.
    /// Must be consistent with the key set on the chain admin contract.
    /// It's not used for chains with ETH as base token and it's expected to be set for all other chains.
    /// Supports both local private keys and GCP KMS keys.
    pub token_multiplier_setter_signer: Option<SignerConfig>,
    /// Max fee per gas we are willing to spend (in wei).
    pub max_fee_per_gas_wei: u128,
    /// Max priority fee per gas we are willing to spend (in wei).
    pub max_priority_fee_per_gas_wei: u128,
    /// Predefined fallback prices for tokens in case external API fetching fails on startup.
    pub fallback_prices: HashMap<Address, f64>,
}

impl BaseTokenPriceUpdaterConfig {
    pub fn fallback_price(&self, token: APIToken) -> Option<TokenApiRatio> {
        let price_f64 = match token {
            APIToken::ETH => self
                .fallback_prices
                .get(&Address::ZERO)
                .or_else(|| self.fallback_prices.get(&Address::with_last_byte(0x01)))
                .copied()?,
            APIToken::ZK => self.fallback_prices.get(&ZK_L1_ADDRESS).copied()?,
            APIToken::ERC20 { address, .. } => self.fallback_prices.get(&address).copied()?,
        };
        let decimals = token.decimals();
        Some(TokenApiRatio::from_f64_decimals_and_timestamp(
            price_f64, decimals, None,
        ))
    }
}

#[derive(Debug)]
pub struct BaseTokenPriceUpdater<
    F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>,
    P: Provider<Ethereum> + Clone,
> {
    base_token: APIToken,
    sl_token: APIToken,
    price_api_client: Box<dyn PriceApiClient>,
    config: BaseTokenPriceUpdaterConfig,
    last_l1_ratio: Ratio<BigUint>,
    chain_admin_contract: IChainAdminOwnableInstance<FillProvider<F, P>, Ethereum>,
    token_multiplier_setter_address: Option<Address>,
    zk_chain_address: Address,
    token_price_sender: watch::Sender<Option<TokenPricesForFees>>,
}

async fn register_operator<P: Provider + WalletProvider<Wallet = EthereumWallet>>(
    provider: &mut P,
    signer_config: SignerConfig,
) -> anyhow::Result<Address> {
    let address = signer_config
        .register_with_wallet(provider.wallet_mut())
        .await?;

    let balance = provider.get_balance(address).await?;
    METRICS
        .l1_updater_balance
        .set(format_ether(balance).parse()?);
    METRICS.l1_updater_address[&address.to_string()].set(1);

    if balance.is_zero() {
        anyhow::bail!("Token multiplier setter's address {address} has zero balance");
    }

    Ok(address)
}

impl<F: TxFiller<Ethereum> + WalletProvider<Wallet = EthereumWallet>, P: Provider<Ethereum> + Clone>
    BaseTokenPriceUpdater<F, P>
{
    pub async fn new(
        zk_chain_l1: ZkChain<DynProvider>,
        mut l1_provider: FillProvider<F, P>,
        base_token_adjuster_config: BaseTokenPriceUpdaterConfig,
        external_price_api_client_config: ExternalPriceApiClientConfig,
        token_price_sender: watch::Sender<Option<TokenPricesForFees>>,
    ) -> anyhow::Result<Self> {
        let base_token_address = zk_chain_l1.get_base_token_address().await?;

        let token_multiplier_setter_address = if let Some(signer_config) =
            base_token_adjuster_config
                .token_multiplier_setter_signer
                .clone()
        {
            Some(register_operator(&mut l1_provider, signer_config).await?)
        } else {
            None
        };

        let base_token_address = base_token_adjuster_config
            .base_token_addr_override
            .unwrap_or(base_token_address);
        let base_token = match base_token_address {
            addr if addr == Address::ZERO || addr == Address::with_last_byte(0x01) => APIToken::ETH,
            addr if addr == ZK_L1_ADDRESS => APIToken::ZK,
            addr => {
                let erc20 = IERC20::new(addr, l1_provider.clone());
                let decimals = if let Some(decimals_override) =
                    base_token_adjuster_config.base_token_decimals_override
                {
                    decimals_override
                } else {
                    erc20
                        .decimals()
                        .call()
                        .await
                        .context("Failed to call `decimals`")?
                };

                APIToken::ERC20 {
                    address: base_token_address,
                    decimals,
                }
            }
        };

        if base_token != APIToken::ETH && token_multiplier_setter_address.is_none() {
            tracing::warn!(
                "Token multiplier setter signer is not configured, but base token is not ETH. \
                 Base token price updater will not be able to update the base token price on L1."
            );
        }

        // Currently SL is always L1 and its base token is ETH.
        let sl_token = APIToken::ETH;

        let price_api_client = match external_price_api_client_config {
            ExternalPriceApiClientConfig::Forced { forced } => {
                Box::new(ForcedPriceClient::new(forced)) as Box<dyn PriceApiClient>
            }
            ExternalPriceApiClientConfig::CoinGecko {
                base_url,
                coingecko_api_key,
                client_timeout,
            } => Box::new(CoinGeckoPriceAPIClient::new(
                base_url,
                coingecko_api_key,
                client_timeout,
            )?) as Box<dyn PriceApiClient>,
            ExternalPriceApiClientConfig::CoinMarketCap {
                base_url,
                cmc_api_key,
                client_timeout,
            } => Box::new(CmcPriceApiClient::new(
                base_url,
                cmc_api_key,
                client_timeout,
            )?) as Box<dyn PriceApiClient>,
        };

        let l1_nominator = zk_chain_l1
            .base_token_gas_price_multiplier_nominator()
            .await?;
        let l1_denominator = zk_chain_l1
            .base_token_gas_price_multiplier_denominator()
            .await?;
        let last_l1_ratio = Ratio::new(BigUint::from(l1_nominator), BigUint::from(l1_denominator));

        let chain_admin_address = zk_chain_l1.get_admin().await?;
        let chain_admin_contract = IChainAdminOwnable::new(chain_admin_address, l1_provider);
        let token_multiplier_setter_on_l1 = chain_admin_contract
            .tokenMultiplierSetter()
            .call()
            .await
            .context("Failed to call `tokenMultiplierSetter`")?;
        if let Some(token_multiplier_setter_address) = token_multiplier_setter_address {
            anyhow::ensure!(
                token_multiplier_setter_address == token_multiplier_setter_on_l1,
                "Configured token multiplier setter address {token_multiplier_setter_address} \
                 does not match the one set on L1 chain admin contract {token_multiplier_setter_on_l1}"
            )
        }

        tracing::info!(
            ?token_multiplier_setter_address, %chain_admin_address, ?last_l1_ratio,
            "initialized base token price updater",
        );

        Ok(Self {
            base_token,
            sl_token,
            price_api_client,
            config: base_token_adjuster_config,
            last_l1_ratio,
            chain_admin_contract,
            token_multiplier_setter_address,
            zk_chain_address: *zk_chain_l1.address(),
            token_price_sender,
        })
    }

    // `_stop_receiver` is currently unused.
    pub async fn run(&mut self, _stop_receiver: watch::Receiver<bool>) -> anyhow::Result<()> {
        let mut timer = tokio::time::interval(self.config.price_polling_interval);

        loop {
            timer.tick().await;

            if let Err(err) = self.loop_iteration().await {
                tracing::warn!("Error in the `base_token_price_updater`'s loop iteration {err}");

                // Token prices are required for fee calculation, so block production is blocked till
                // `token_price_sender` is populated. In case first loop iteration fails we populate it
                // with predefined config values if available.
                if self.token_price_sender.borrow().is_none() {
                    let base_token_ratio = self.config.fallback_price(self.base_token);
                    let sl_token_ratio = self.config.fallback_price(self.sl_token);

                    if let Some(base_token_usd_price) = base_token_ratio
                        && let Some(sl_token_usd_price) = sl_token_ratio
                    {
                        tracing::warn!(
                            ?base_token_usd_price,
                            ?sl_token_usd_price,
                            "Populating token prices for fees with fallback config values"
                        );
                        self.token_price_sender
                            .send_replace(Some(TokenPricesForFees {
                                base_token_usd_price,
                                sl_token_usd_price,
                            }));
                    } else {
                        tracing::error!(
                            base_token = ?self.base_token,
                            sl_token = ?self.sl_token,
                            "Initial token price fetch iteration failed and no fallback prices are configured, \
                                token prices for fees remain unset, blocking sequencer"
                        );
                    }
                }
            }
        }
    }

    async fn loop_iteration(&mut self) -> anyhow::Result<()> {
        let tokens_to_watch = HashSet::from([self.base_token, self.sl_token, APIToken::ETH]);

        let mut token_prices = HashMap::new();
        for token in tokens_to_watch {
            let ratio = self.retry_fetch_ratio(token).await?;
            token_prices.insert(token, ratio.clone());

            // Record fetched price converted to f64 with decimals applied.
            let token_ratio_f64 =
                (ratio.ratio * BigUint::from(10u32).pow(token.decimals().into())).to_f64();
            if let Some(token_ratio_f64) = token_ratio_f64 {
                METRICS.token_price[&token.to_string()].set(token_ratio_f64);
                tracing::debug!("Fetched price for token {token}: {token_ratio_f64} USD");
            }
        }

        let base_token_ratio = token_prices.get(&self.base_token).unwrap();
        let sl_token_ratio = token_prices.get(&self.sl_token).unwrap();
        let eth_token_ratio = token_prices.get(&APIToken::ETH).unwrap();

        self.token_price_sender
            .send_replace(Some(TokenPricesForFees {
                base_token_usd_price: base_token_ratio.clone(),
                sl_token_usd_price: sl_token_ratio.clone(),
            }));

        let eth_to_base_price = &eth_token_ratio.ratio / &base_token_ratio.ratio;
        self.maybe_update_l1_ratio(eth_to_base_price).await?;

        Ok(())
    }

    async fn retry_fetch_ratio(&self, token: APIToken) -> anyhow::Result<TokenApiRatio> {
        let max_retries = self.config.price_fetching_max_attempts;
        let mut last_error = None;

        for attempt in 0..max_retries {
            let start_time = Instant::now();
            match self.price_api_client.fetch_ratio(token).await {
                Ok(ratio) => {
                    METRICS.external_price_api_latency[&OperationResultLabels {
                        result: OperationResult::Success,
                    }]
                        .observe(start_time.elapsed());
                    return Ok(ratio);
                }
                Err(err) => {
                    tracing::info!(
                        "Attempt {attempt}/{max_retries} to fetch ratio from external price api failed with err: {err}. Retrying...",
                    );
                    last_error = Some(err);
                    METRICS.external_price_api_latency[&OperationResultLabels {
                        result: OperationResult::Failure,
                    }]
                        .observe(start_time.elapsed());
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
        Err(last_error
            .unwrap()
            .context("Failed to fetch base token ratio after multiple attempts"))
    }

    /// Compares the new ratio with the current one on L1 and updates it if the deviation exceeds the configured threshold.
    async fn maybe_update_l1_ratio(&mut self, new_ratio: Ratio<BigUint>) -> anyhow::Result<()> {
        /// Timeout for L1 transaction inclusion.
        const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(300);

        let Some(token_multiplier_setter_address) = self.token_multiplier_setter_address else {
            // No setter address configured, nothing to do.
            return Ok(());
        };
        let old_ratio = self.last_l1_ratio.clone();

        // Instead of converting to signed bigint it's easier to do an if-else here.
        let diff = if new_ratio > old_ratio {
            &new_ratio - &old_ratio
        } else {
            &old_ratio - &new_ratio
        };
        let deviation = (diff / &old_ratio) * BigUint::from(100u32);

        if deviation
            < Ratio::from_integer(BigUint::from(self.config.l1_update_deviation_percentage))
        {
            tracing::debug!(
                ?old_ratio, ?new_ratio, %deviation,
                "L1 base token ratio deviation within threshold, not updating L1 ratio",
            );
            return Ok(());
        }

        tracing::info!(
            ?old_ratio, ?new_ratio, %deviation,
            "L1 base token ratio deviation exceeded threshold, updating L1 ratio",
        );

        let (numer, denom) = match (new_ratio.numer().to_u128(), new_ratio.denom().to_u128()) {
            (Some(numer), Some(denom)) => (numer, denom),
            _ => {
                let mut numer = new_ratio.numer().clone();
                let mut denom = new_ratio.denom().clone();

                // Scale down both nominator and denominator to fit into u128.
                while numer.to_u128().is_none() || denom.to_u128().is_none() {
                    numer /= BigUint::from(10u32);
                    denom /= BigUint::from(10u32);
                }

                if (&numer).min(&denom) < &BigUint::from(100u32) {
                    anyhow::bail!(
                        "New ratio's nominator or denominator is too large to fit into u128 even after scaling down, ratio: {new_ratio:?}"
                    );
                }

                (numer.to_u128().unwrap(), denom.to_u128().unwrap())
            }
        };

        let eip1559_est = self
            .chain_admin_contract
            .provider()
            .estimate_eip1559_fees()
            .await?;
        tracing::debug!(
            eip1559_est.max_priority_fee_per_gas,
            "estimated median priority fee (20% percentile) for the last 10 blocks"
        );
        if eip1559_est.max_fee_per_gas > self.config.max_fee_per_gas_wei {
            tracing::warn!(
                max_fee_per_gas = self.config.max_fee_per_gas_wei,
                estimated_max_fee_per_gas = eip1559_est.max_fee_per_gas,
                "Base token updater's configured maxFeePerGas is lower than the one estimated from network"
            );
        }
        if eip1559_est.max_priority_fee_per_gas > self.config.max_priority_fee_per_gas_wei {
            tracing::warn!(
                max_priority_fee_per_gas = self.config.max_priority_fee_per_gas_wei,
                estimated_max_priority_fee_per_gas = eip1559_est.max_priority_fee_per_gas,
                "Base token updater's configured maxPriorityFeePerGas is lower than the one estimated from network"
            );
        }

        let tx_request = self
            .chain_admin_contract
            .setTokenMultiplier(self.zk_chain_address, numer, denom)
            .into_transaction_request()
            .with_from(token_multiplier_setter_address)
            .with_max_fee_per_gas(self.config.max_fee_per_gas_wei)
            .with_max_priority_fee_per_gas(self.config.max_priority_fee_per_gas_wei);
        let provider = self.chain_admin_contract.provider();
        let tx_handle = provider.send_transaction(tx_request).await?;
        let receipt = tx_handle
            // We are being optimistic with our transaction inclusion here. But, even if
            // reorg happens and transaction will not be included it's ok, it can be sent
            // on the next iteration if still needed.
            .with_required_confirmations(1)
            // Ensure we don't wait indefinitely and crash if the transaction is not
            // included on L1 in a reasonable time.
            .with_timeout(Some(TRANSACTION_TIMEOUT))
            .get_receipt()
            .await?;
        validate_tx_receipt(provider, receipt).await?;

        let balance = format_ether(
            provider
                .get_balance(token_multiplier_setter_address)
                .await?,
        );
        let nonce = provider
            .get_transaction_count(token_multiplier_setter_address)
            .await?;
        METRICS.l1_updater_balance.set(balance.parse()?);
        METRICS.l1_updater_nonce.set(nonce);

        if let Some(r) = new_ratio.to_f64() {
            METRICS.ratio_l1.set(r);
        }
        self.last_l1_ratio = new_ratio;

        Ok(())
    }
}

async fn validate_tx_receipt(
    provider: &impl Provider,
    receipt: TransactionReceipt,
) -> anyhow::Result<()> {
    if receipt.status() {
        // Transaction succeeded - log output and return OK(())
        let l1_transaction_fee = receipt.gas_used as u128 * receipt.effective_gas_price;
        tracing::info!(
            tx_hash = ?receipt.transaction_hash,
            l1_block_number = receipt.block_number.unwrap(),
            gas_used = receipt.gas_used,
            l1_transaction_fee_ether = format_ether(l1_transaction_fee),
            "`setTokenMultiplier` succeeded on L1",
        );
        METRICS.gas_used.observe(receipt.gas_used);
        METRICS
            .l1_transaction_fee_ether
            .observe(format_ether(l1_transaction_fee).parse()?);
        Ok(())
    } else {
        tracing::error!(
            tx_hash = ?receipt.transaction_hash,
            l1_block_number = receipt.block_number.unwrap(),
            "Transaction `setTokenMultiplier` failed on L1",
        );
        if let Ok(trace) = provider
            .debug_trace_transaction(
                receipt.transaction_hash,
                GethDebugTracingOptions::call_tracer(CallConfig::default()),
            )
            .await
        {
            let call_frame = trace
                .try_into_call_frame()
                .expect("requested call tracer but received a different call frame type");
            // We print top-level call frame's output as it likely contains serialized custom
            // error pointing to the underlying problem (i.e. starts with the error's 4byte
            // signature).
            tracing::error!(
                ?call_frame.output,
                ?call_frame.error,
                ?call_frame.revert_reason,
                "Failed transaction's top-level call frame"
            );
        }
        anyhow::bail!(
            "`setTokenMultiplier` transaction failed, see L1 transaction's trace for more details (tx_hash='{:?}')",
            receipt.transaction_hash
        );
    }
}
