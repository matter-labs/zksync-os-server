//! One-shot setup binary for the interop-load harness.
//!
//! Deploys a TestERC20 on L1, deposits it through the bridge into chain A so
//! that the gateway records a chain balance (without which `sendBundle` for
//! ERC20 transfers reverts at the destination). Then deploys an EventEmitter
//! on chain B for the message-shape interop calls. Writes `setup.json`
//! containing every address and asset id the harness needs.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use alloy::eips::eip1559::Eip1559Estimation;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, FixedBytes, U256, keccak256};
use alloy::providers::utils::Eip1559Estimator;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::SolValue;
use anyhow::{Context, anyhow};
use clap::Parser;
use serde::Serialize;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_types::{L1PriorityTxType, L1TxType, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE};

const L2_NATIVE_TOKEN_VAULT_ADDRESS: Address =
    alloy::primitives::address!("0000000000000000000000000000000000010004");
const L2_BASE_TOKEN_ADDRESS: Address =
    alloy::primitives::address!("000000000000000000000000000000000000800a");
const L1_RECEIPT_TIMEOUT: Duration = Duration::from_secs(120);
const L2_RECEIPT_TIMEOUT: Duration = Duration::from_secs(180);

fn local_reqwest_client() -> alloy::transports::http::reqwest::Client {
    alloy::transports::http::reqwest::ClientBuilder::new()
        .no_proxy()
        .build()
        .expect("local reqwest client")
}

sol! {
    #[sol(rpc)]
    TestERC20,
    "../integration-tests/test-contracts/out/TestERC20.sol/TestERC20.json"
}

sol! {
    #[sol(rpc)]
    EventEmitter,
    "../integration-tests/test-contracts/out/EventEmitter.sol/EventEmitter.json"
}

sol! {
    #[sol(rpc)]
    InteropRecipient,
    "../integration-tests/test-contracts/out/InteropRecipient.sol/InteropRecipient.json"
}

sol! {
    #[sol(rpc)]
    contract IL2NativeTokenVault {
        function tokenAddress(bytes32 assetId) external view returns (address);
    }
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "One-shot setup for interop-load: deploy + register interop ERC20, deploy chain-B receiver"
)]
struct Args {
    #[arg(long)]
    chain_a_rpc: String,
    #[arg(long)]
    chain_b_rpc: String,
    #[arg(long)]
    extra_recipient_rpc: Vec<String>,
    #[arg(long)]
    l1_rpc: String,
    #[arg(long)]
    bridgehub: Address,
    #[arg(long)]
    l1_rich_privkey: String,
    /// Private key for the chain-A account that will receive the deposited
    /// ERC20 supply. Should be the same key the harness uses as --rich-privkey.
    #[arg(long)]
    chain_a_recipient_privkey: String,
    #[arg(long, default_value = "1000000000000000000000000")]
    mint_amount_wei: String,
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Serialize)]
struct SetupRecord {
    l1_chain_id: u64,
    chain_a_id: u64,
    chain_b_id: u64,
    l1_token_address: Address,
    l2_token_address_chain_a: Address,
    l2_native_token_vault: Address,
    l2_base_token: Address,
    erc20_asset_id: FixedBytes<32>,
    base_token_asset_id: FixedBytes<32>,
    event_emitter_chain_a: Address,
    event_emitter_chain_b: Address,
    /// Address of the InteropRecipient contract, deployed at the same address
    /// on chain A and chain B by a fresh dedicated deployer with nonce 0.
    /// Receives both Message-shape and Base-shape interop bundles.
    interop_recipient: Address,
    deposited_amount: String,
}

struct BridgeDepositBaseTokenArgs<'a> {
    bridgehub_address: Address,
    chain_id: u64,
    chain_rpc: &'a str,
    target: Address,
    amount: U256,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    label: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mint_amount = U256::from_str(&args.mint_amount_wei).context("parse --mint-amount-wei")?;

    let l1_signer =
        PrivateKeySigner::from_str(&args.l1_rich_privkey).context("parse --l1-rich-privkey")?;
    let chain_a_signer = PrivateKeySigner::from_str(&args.chain_a_recipient_privkey)
        .context("parse --chain-a-recipient-privkey")?;
    let chain_a_recipient = chain_a_signer.address();

    let l1_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(l1_signer.clone()))
        .connect_reqwest(local_reqwest_client(), args.l1_rpc.parse()?);
    let chain_a_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(chain_a_signer))
        .connect_reqwest(local_reqwest_client(), args.chain_a_rpc.parse()?);
    let chain_b_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(l1_signer.clone()))
        .connect_reqwest(local_reqwest_client(), args.chain_b_rpc.parse()?);

    let l1_chain_id = l1_provider.get_chain_id().await?;
    let chain_a_id = chain_a_provider.get_chain_id().await?;
    let chain_b_id = chain_b_provider.get_chain_id().await?;
    println!(
        "Connected: L1={l1_chain_id}, chain_a={chain_a_id}, chain_b={chain_b_id}, recipient={chain_a_recipient}"
    );

    println!("Deploying TestERC20 on L1...");
    let l1_token = TestERC20::deploy(
        l1_provider.clone(),
        U256::ZERO,
        "InteropLoadToken".to_string(),
        "ILT".to_string(),
    )
    .await
    .context("deploy L1 ERC20")?;
    let l1_token_address = *l1_token.address();
    println!("Deployed L1 ERC20 at {l1_token_address}");

    println!("Minting {mint_amount} to L1 deployer...");
    let mint_receipt = l1_token
        .mint(l1_signer.address(), mint_amount)
        .send()
        .await
        .context("send mint tx")?
        .with_timeout(Some(L1_RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .context("mint receipt")?;
    anyhow::ensure!(mint_receipt.status(), "L1 mint reverted");

    println!("Depositing to chain A via Bridgehub...");
    let bridgehub = Bridgehub::new(args.bridgehub, l1_provider.clone(), chain_a_id);
    let max_priority_fee_per_gas = l1_provider.get_max_priority_fee_per_gas().await?;
    let base_fees = l1_provider
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = base_fees.max_fee_per_gas + max_priority_fee_per_gas;
    let l2_gas_limit = 2_500_000_u64;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await?;
    let shared_bridge_address = bridgehub.shared_bridge_address().await?;

    let approve_receipt = l1_token
        .approve(shared_bridge_address, mint_amount)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .send()
        .await
        .context("approve bridge")?
        .with_timeout(Some(L1_RECEIPT_TIMEOUT))
        .get_receipt()
        .await?;
    anyhow::ensure!(approve_receipt.status(), "approve reverted");

    let second_bridge_calldata = (l1_token_address, mint_amount, chain_a_recipient).abi_encode();
    let deposit_request = bridgehub
        .request_l2_transaction_two_bridges(
            tx_base_cost,
            U256::ZERO,
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            chain_a_recipient,
            shared_bridge_address,
            U256::ZERO,
            second_bridge_calldata,
        )
        .value(tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();
    let deposit_receipt = l1_provider
        .send_transaction(deposit_request)
        .await
        .context("submit deposit")?
        .with_timeout(Some(L1_RECEIPT_TIMEOUT))
        .get_receipt()
        .await?;
    anyhow::ensure!(deposit_receipt.status(), "deposit tx reverted on L1");

    let l1_to_l2_log = deposit_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .ok_or_else(|| anyhow!("no NewPriorityRequest log in deposit receipt"))?;
    let l2_tx_hash = l1_to_l2_log.inner.txHash;
    println!("Waiting for L2 deposit tx {l2_tx_hash} on chain A...");
    // Don't go through alloy's PendingTransactionBuilder: the priority tx type
    // (0x7f) is rejected by the generic Ethereum receipt deserializer. Poll
    // raw JSON-RPC for the status field instead.
    poll_tx_status(&args.chain_a_rpc, l2_tx_hash, L2_RECEIPT_TIMEOUT)
        .await
        .context("L2 deposit receipt")?;

    // Asset id for L1-origin tokens: keccak256(abi.encode(l1_chain_id, NTV_ADDRESS, l1_token)).
    let asset_id_bytes = {
        let encoded = (
            U256::from(l1_chain_id),
            L2_NATIVE_TOKEN_VAULT_ADDRESS,
            l1_token_address,
        )
            .abi_encode();
        keccak256(&encoded)
    };
    let erc20_asset_id = asset_id_bytes;
    println!("Computed ERC20 asset_id: {erc20_asset_id}");

    let vault_a = IL2NativeTokenVault::new(L2_NATIVE_TOKEN_VAULT_ADDRESS, chain_a_provider.clone());
    let l2_token_address_chain_a = vault_a
        .tokenAddress(erc20_asset_id)
        .call()
        .await
        .context("query L2 token address on chain A")?;
    anyhow::ensure!(
        l2_token_address_chain_a != Address::ZERO,
        "NTV returned zero address for ERC20 asset id; deposit did not register"
    );
    println!("Resolved L2 ERC20 on chain A: {l2_token_address_chain_a}");

    // Base-token asset id uses the L1 chain id and the L2 base-token sentinel
    // address (this is what the integration test does for ETH-style asset).
    let base_token_asset_id_bytes = {
        let encoded = (
            U256::from(l1_chain_id),
            L2_NATIVE_TOKEN_VAULT_ADDRESS,
            Address::ZERO,
        )
            .abi_encode();
        keccak256(&encoded)
    };
    let base_token_asset_id = base_token_asset_id_bytes;
    println!("Computed base-token asset_id: {base_token_asset_id}");

    let recipient_deployer_fund_amount = U256::from(100_000_000_000_000_000_u128); // 0.1 ETH
    let rich_base_token_min_balance = U256::from(600_000_000_000_000_000_000_u128); // 600 ETH
    let rich_base_token_fund_amount = U256::from(700_000_000_000_000_000_000_u128); // 700 ETH
    if chain_a_provider.get_balance(chain_a_recipient).await? < rich_base_token_min_balance {
        println!("Funding chain A recipient via Bridgehub base-token deposit...");
        bridge_deposit_base_token(
            &l1_provider,
            &chain_a_provider,
            BridgeDepositBaseTokenArgs {
                bridgehub_address: args.bridgehub,
                chain_id: chain_a_id,
                chain_rpc: &args.chain_a_rpc,
                target: chain_a_recipient,
                amount: rich_base_token_fund_amount,
                max_fee_per_gas,
                max_priority_fee_per_gas,
                label: "chain A recipient".to_owned(),
            },
        )
        .await?;
    }

    println!("Funding chain B deployer via Bridgehub base-token deposit...");
    let chain_b_bridgehub = Bridgehub::new(args.bridgehub, l1_provider.clone(), chain_b_id);
    let chain_b_deployer = l1_signer.address();
    let chain_b_fund_amount = rich_base_token_fund_amount;
    let chain_b_l2_gas_limit = chain_b_provider
        .estimate_gas(
            TransactionRequest::default()
                .transaction_type(L1PriorityTxType::TX_TYPE)
                .from(chain_b_deployer)
                .to(chain_b_deployer)
                .value(chain_b_fund_amount),
        )
        .await
        .context("estimate chain B base-token deposit gas")?;
    let chain_b_tx_base_cost = chain_b_bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            chain_b_l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await
        .context("chain B base-token deposit base cost")?;
    let chain_b_deposit_request = chain_b_bridgehub
        .request_l2_transaction_direct(
            chain_b_fund_amount + chain_b_tx_base_cost,
            chain_b_deployer,
            chain_b_fund_amount,
            vec![],
            chain_b_l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            chain_b_deployer,
        )
        .value(chain_b_fund_amount + chain_b_tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();
    let chain_b_deposit_receipt = l1_provider
        .send_transaction(chain_b_deposit_request)
        .await
        .context("submit chain B base-token deposit")?
        .with_timeout(Some(L1_RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .context("chain B base-token deposit receipt")?;
    anyhow::ensure!(
        chain_b_deposit_receipt.status(),
        "chain B base-token deposit reverted on L1"
    );
    let chain_b_l1_to_l2_log = chain_b_deposit_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .ok_or_else(|| anyhow!("no NewPriorityRequest log in chain B deposit receipt"))?;
    let chain_b_l2_tx_hash = chain_b_l1_to_l2_log.inner.txHash;
    poll_tx_status(&args.chain_b_rpc, chain_b_l2_tx_hash, L2_RECEIPT_TIMEOUT)
        .await
        .context("chain B base-token deposit receipt")?;

    // Deploy EventEmitter on BOTH chains. The harness's message-shape bundle
    // calls EventEmitter.emitEvent, which on the source side runs as a
    // simulation (so the contract must have code on chain A) and on the
    // destination side executes for real (so it must have code on chain B).
    // We use a dedicated signer for each chain so their nonces start at 0,
    // which lets us deploy to the same address with CREATE.
    println!("Deploying EventEmitter on chain A (using recipient signer)...");
    let event_emitter_a = EventEmitter::deploy(chain_a_provider.clone())
        .await
        .context("deploy EventEmitter on chain A")?;
    let event_emitter_chain_a = *event_emitter_a.address();
    println!("EventEmitter on chain A: {event_emitter_chain_a}");

    println!("Deploying EventEmitter on chain B...");
    let event_emitter_b = EventEmitter::deploy(chain_b_provider.clone())
        .await
        .context("deploy EventEmitter on chain B")?;
    let event_emitter_chain_b = *event_emitter_b.address();
    println!("EventEmitter on chain B: {event_emitter_chain_b}");
    if event_emitter_chain_a != event_emitter_chain_b {
        // We don't strictly need them to match because the harness can pick
        // one for source-side simulation; the destination only cares that the
        // contract exists where it claims. But if they DO match we get a
        // simpler bundle encoding. Warn for now.
        eprintln!(
            "warning: EventEmitter addresses differ across chains (A={event_emitter_chain_a}, B={event_emitter_chain_b}). Harness will still target chain B address; expect source-side simulation revert."
        );
    }

    // Deploy InteropRecipient at the same address on both chains. The trick:
    // a fresh dedicated key with nonce 0 on both chains, so CREATE puts the
    // contract at the same deterministic address. The harness's Message and
    // Base shapes both target this address.
    //
    // We salt the deployer key with the current wallclock so re-running setup
    // against a chain that already saw a previous deployer (e.g. chain was
    // restarted but state survived, or one chain advanced the deployer's
    // nonce in a prior partial run) still gets a fresh nonce=0 deployer.
    let salt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let recipient_deployer_bytes =
        alloy::primitives::keccak256(format!("interop-load/recipient-deployer/{salt}").as_bytes());
    let recipient_deployer = PrivateKeySigner::from_bytes(&recipient_deployer_bytes)
        .context("derive recipient deployer key")?;
    let recipient_deployer_addr = recipient_deployer.address();
    println!("Recipient deployer address: {recipient_deployer_addr}");

    // Fund the deployer on chain A (from the chain_a_recipient signer).
    let fund_amount = recipient_deployer_fund_amount;
    fund_address_on_chain(
        &chain_a_provider,
        recipient_deployer_addr,
        fund_amount,
        "chain A",
    )
    .await?;

    // Fund the deployer on chain B (from the l1_signer, which is the only
    // pre-funded account on chain B).
    fund_address_on_chain(
        &chain_b_provider,
        recipient_deployer_addr,
        fund_amount,
        "chain B",
    )
    .await?;

    // Deploy on chain A with the dedicated deployer at nonce 0.
    println!("Deploying InteropRecipient on chain A...");
    let chain_a_recipient_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(recipient_deployer.clone()))
        .connect_reqwest(local_reqwest_client(), args.chain_a_rpc.parse()?);
    let recipient_a = InteropRecipient::deploy(chain_a_recipient_provider)
        .await
        .context("deploy InteropRecipient on chain A")?;
    let interop_recipient_chain_a = *recipient_a.address();
    println!("InteropRecipient on chain A: {interop_recipient_chain_a}");

    println!("Deploying InteropRecipient on chain B...");
    let chain_b_recipient_provider = ProviderBuilder::new()
        .wallet(EthereumWallet::new(recipient_deployer.clone()))
        .connect_reqwest(local_reqwest_client(), args.chain_b_rpc.parse()?);
    let recipient_b = InteropRecipient::deploy(chain_b_recipient_provider)
        .await
        .context("deploy InteropRecipient on chain B")?;
    let interop_recipient_chain_b = *recipient_b.address();
    println!("InteropRecipient on chain B: {interop_recipient_chain_b}");

    anyhow::ensure!(
        interop_recipient_chain_a == interop_recipient_chain_b,
        "InteropRecipient addresses differ across chains (A={interop_recipient_chain_a}, B={interop_recipient_chain_b}); harness requires matching addresses"
    );
    let interop_recipient = interop_recipient_chain_a;

    for extra_rpc in &args.extra_recipient_rpc {
        let extra_chain_id = get_chain_id(extra_rpc).await?;
        println!("Deploying InteropRecipient on extra chain {extra_chain_id}...");
        let extra_provider = ProviderBuilder::new()
            .wallet(EthereumWallet::new(l1_signer.clone()))
            .connect_reqwest(local_reqwest_client(), extra_rpc.parse()?);
        if extra_provider.get_balance(l1_signer.address()).await? < rich_base_token_min_balance {
            println!("Funding chain {extra_chain_id} sender via Bridgehub base-token deposit...");
            bridge_deposit_base_token(
                &l1_provider,
                &extra_provider,
                BridgeDepositBaseTokenArgs {
                    bridgehub_address: args.bridgehub,
                    chain_id: extra_chain_id,
                    chain_rpc: extra_rpc,
                    target: l1_signer.address(),
                    amount: rich_base_token_fund_amount,
                    max_fee_per_gas,
                    max_priority_fee_per_gas,
                    label: format!("chain {extra_chain_id} sender"),
                },
            )
            .await?;
        }
        fund_address_on_chain(
            &extra_provider,
            recipient_deployer_addr,
            fund_amount,
            &format!("chain {extra_chain_id}"),
        )
        .await?;

        let extra_recipient_provider = ProviderBuilder::new()
            .wallet(EthereumWallet::new(recipient_deployer.clone()))
            .connect_reqwest(local_reqwest_client(), extra_rpc.parse()?);
        let recipient_extra = InteropRecipient::deploy(extra_recipient_provider)
            .await
            .with_context(|| format!("deploy InteropRecipient on chain {extra_chain_id}"))?;
        let interop_recipient_extra = *recipient_extra.address();
        println!("InteropRecipient on chain {extra_chain_id}: {interop_recipient_extra}");
        anyhow::ensure!(
            interop_recipient_extra == interop_recipient,
            "InteropRecipient address differs on chain {extra_chain_id} (expected={interop_recipient}, got={interop_recipient_extra})"
        );
    }

    let record = SetupRecord {
        l1_chain_id,
        chain_a_id,
        chain_b_id,
        l1_token_address,
        l2_token_address_chain_a,
        l2_native_token_vault: L2_NATIVE_TOKEN_VAULT_ADDRESS,
        l2_base_token: L2_BASE_TOKEN_ADDRESS,
        erc20_asset_id,
        base_token_asset_id,
        event_emitter_chain_a,
        event_emitter_chain_b,
        interop_recipient,
        deposited_amount: mint_amount.to_string(),
    };

    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent).context("create output dir")?;
    }
    std::fs::write(&args.output, serde_json::to_vec_pretty(&record)?)
        .with_context(|| format!("write {}", args.output.display()))?;
    println!("Wrote {}", args.output.display());

    // Fund the chain-A recipient with extra base-token ETH if needed.
    let recipient_balance = chain_a_provider.get_balance(chain_a_recipient).await?;
    println!("chain_a recipient balance now {recipient_balance} wei");

    Ok(())
}

async fn get_chain_id(rpc_url: &str) -> anyhow::Result<u64> {
    let provider = ProviderBuilder::new().connect_reqwest(local_reqwest_client(), rpc_url.parse()?);
    Ok(provider.get_chain_id().await?)
}

async fn bridge_deposit_base_token<L1P, L2P>(
    l1_provider: &L1P,
    l2_provider: &L2P,
    args: BridgeDepositBaseTokenArgs<'_>,
) -> anyhow::Result<()>
where
    L1P: Provider + Clone,
    L2P: Provider + Clone,
{
    let BridgeDepositBaseTokenArgs {
        bridgehub_address,
        chain_id,
        chain_rpc,
        target,
        amount,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        label,
    } = args;
    let bridgehub = Bridgehub::new(bridgehub_address, l1_provider.clone(), chain_id);
    let l2_gas_limit = l2_provider
        .estimate_gas(
            TransactionRequest::default()
                .transaction_type(L1PriorityTxType::TX_TYPE)
                .from(target)
                .to(target)
                .value(amount),
        )
        .await
        .with_context(|| format!("estimate {label} base-token deposit gas"))?;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await
        .with_context(|| format!("{label} base-token deposit base cost"))?;
    let deposit_request = bridgehub
        .request_l2_transaction_direct(
            amount + tx_base_cost,
            target,
            amount,
            vec![],
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            target,
        )
        .value(amount + tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();
    let deposit_receipt = l1_provider
        .send_transaction(deposit_request)
        .await
        .with_context(|| format!("submit {label} base-token deposit"))?
        .with_timeout(Some(L1_RECEIPT_TIMEOUT))
        .get_receipt()
        .await
        .with_context(|| format!("{label} base-token deposit receipt"))?;
    anyhow::ensure!(
        deposit_receipt.status(),
        "{label} base-token deposit reverted on L1"
    );
    let l1_to_l2_log = deposit_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .ok_or_else(|| anyhow!("no NewPriorityRequest log in {label} deposit receipt"))?;
    poll_tx_status(chain_rpc, l1_to_l2_log.inner.txHash, L2_RECEIPT_TIMEOUT)
        .await
        .with_context(|| format!("{label} base-token deposit L2 receipt"))?;
    Ok(())
}

async fn fund_address_on_chain<P: Provider + Clone>(
    provider: &P,
    target: Address,
    amount: U256,
    label: &str,
) -> anyhow::Result<()> {
    use alloy::network::TransactionBuilder;
    use alloy::rpc::types::TransactionRequest;
    let tx = TransactionRequest::default()
        .with_to(target)
        .with_value(amount)
        .with_max_fee_per_gas(1_000_000_000)
        .with_max_priority_fee_per_gas(0);
    let pending = provider
        .send_transaction(tx)
        .await
        .with_context(|| format!("fund {target} on {label}"))?;
    pending
        .with_timeout(Some(Duration::from_secs(60)))
        .get_receipt()
        .await
        .with_context(|| format!("fund receipt for {target} on {label}"))?;
    Ok(())
}

async fn poll_tx_status(
    rpc_url: &str,
    tx_hash: FixedBytes<32>,
    timeout: Duration,
) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let started = std::time::Instant::now();
    loop {
        if started.elapsed() > timeout {
            anyhow::bail!("timed out waiting for tx {tx_hash}");
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getTransactionReceipt",
            "params": [tx_hash],
        });
        let resp: serde_json::Value = client
            .post(rpc_url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if let Some(receipt) = resp.get("result").and_then(|v| v.as_object()) {
            let status = receipt
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("0x0");
            anyhow::ensure!(status == "0x1", "tx {tx_hash} reverted (status={status})");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
