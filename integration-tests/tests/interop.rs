//! Interop integration tests for cross-chain token transfers.

use alloy::{
    eips::eip1559::Eip1559Estimation,
    primitives::{Address, Bytes, FixedBytes, U256, address, keccak256},
    providers::utils::Eip1559Estimator,
    providers::{PendingTransactionBuilder, Provider},
    rpc::types::TransactionRequest,
    sol,
    sol_types::{SolCall, SolType, SolValue},
};
use anyhow::{Context, Result};
use test_log::test;
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_integration_tests::{
    MultiChainTester, Tester, assert_traits::ReceiptAssert, contracts::TestERC20,
    provider::ZksyncApi,
};
use zksync_os_types::{L1PriorityTxType, L1TxType, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE};

const L2_INTEROP_CENTER_ADDRESS: Address = address!("000000000000000000000000000000000001000d");
const L2_INTEROP_HANDLER_ADDRESS: Address = address!("000000000000000000000000000000000001000e");
const L2_NATIVE_TOKEN_VAULT_ADDRESS: Address = address!("0000000000000000000000000000000000010004");
const L2_ASSET_ROUTER_ADDRESS: Address = address!("0000000000000000000000000000000000010003");
const L1_MESSENGER_ADDRESS: Address = address!("0000000000000000000000000000000000008008");

sol! {
    #[sol(rpc)]
    contract IL2NativeTokenVault {
        function ensureTokenIsRegistered(address _nativeToken) external returns (bytes32);
        function assetId(address _tokenAddress) external view returns (bytes32);
        function tokenAddress(bytes32 _assetId) external view returns (address);
    }

    #[sol(rpc)]
    contract IInteropCenter {
        function sendBundle(
            bytes calldata _destinationChainId,
            InteropCallStarter[] calldata _callStarters,
            bytes[] calldata _bundleAttributes
        ) external payable returns (bytes32);

        struct InteropCallStarter {
            bytes to;
            bytes data;
            bytes[] callAttributes;
        }

        event InteropBundleSent(
            bytes indexed destinationChainId,
            bytes32 indexed bundleHash,
            bytes bundle
        );
    }

    #[sol(rpc)]
    contract IInteropHandler {
        struct L2Message {
            uint16 txNumberInBatch;
            address sender;
            bytes data;
        }

        struct MessageInclusionProof {
            uint256 chainId;
            uint256 l1BatchNumber;
            uint256 l2MessageIndex;
            L2Message message;
            bytes32[] proof;
        }

        function executeBundle(
            bytes calldata _bundle,
            MessageInclusionProof calldata _proof
        ) external;
    }

    #[sol(rpc)]
    contract IL2InteropRootStorage {
        function interopRoots(uint256 chainId, uint256 batchNumber) external view returns (bytes32);
        function addInteropRoot(uint256 chainId, uint256 batchNumber, bytes32[] memory sides) external;
    }

    // Bundle attribute functions for encoding
    function indirectCall(uint256 _gasLimit) external pure returns (bytes memory);
    function unbundlerAddress(bytes calldata _address) external pure returns (bytes memory);
}

/// Helper to format ERC-7930 interoperable address with just address (no chain reference)
fn format_evm_v1_address_only(addr: Address) -> Bytes {
    let mut result = Vec::new();
    result.extend_from_slice(&[0x00, 0x01]); // version
    result.extend_from_slice(&[0x00, 0x00]); // chain type (EVM)
    result.extend_from_slice(&[0x00]); // chain reference length = 0
    result.extend_from_slice(&[0x14]); // address length = 20
    result.extend_from_slice(addr.as_slice());
    Bytes::from(result)
}

/// Helper to format ERC-7930 interoperable address for EVM chain (chain ID only)
fn format_evm_v1(chain_id: u64) -> Bytes {
    let chain_ref = to_chain_reference(chain_id);
    let mut result = Vec::new();
    result.extend_from_slice(&[0x00, 0x01]); // version
    result.extend_from_slice(&[0x00, 0x00]); // chain type (EVM)
    result.push(chain_ref.len() as u8); // chain reference length
    result.extend_from_slice(&chain_ref);
    result.push(0x00); // address length = 0
    Bytes::from(result)
}

/// Convert chain ID to minimal bytes representation
fn to_chain_reference(chain_id: u64) -> Vec<u8> {
    if chain_id == 0 {
        return vec![0];
    }
    let mut bytes = chain_id.to_be_bytes().to_vec();
    // Remove leading zeros
    while bytes.len() > 1 && bytes[0] == 0 {
        bytes.remove(0);
    }
    bytes
}

/// Helper to compute asset ID (keccak256(abi.encode(chainId, ntvAddress, tokenAddress)))
fn compute_asset_id(chain_id: u64, token_address: Address) -> [u8; 32] {
    let encoded = (
        U256::from(chain_id),
        L2_NATIVE_TOKEN_VAULT_ADDRESS,
        token_address,
    )
        .abi_encode();
    keccak256(&encoded).into()
}

/// Helper to build second bridge calldata for asset router
fn build_second_bridge_calldata(
    asset_id: [u8; 32],
    amount: U256,
    receiver: Address,
    maybe_token_address: Address,
) -> Bytes {
    // encodeBridgeBurnData(amount, receiver, maybeTokenAddress)
    let inner = (amount, receiver, maybe_token_address).abi_encode();

    // encodeAssetRouterBridgehubDepositData(assetId, transferData)
    // Manual encoding to match ethers.js AbiCoder.encode(['bytes32', 'bytes'], [assetId, transferData])
    let mut result = vec![0x01]; // NEW_ENCODING_VERSION

    // Encode (bytes32, bytes) manually:
    // 1. bytes32 value directly (32 bytes)
    result.extend_from_slice(&asset_id);

    // 2. offset to bytes data (always 0x40 = 64, since bytes32 is 32 bytes and offset itself is 32 bytes)
    result.extend_from_slice(&[0u8; 31]); // 31 zeros
    result.push(0x40); // offset = 64

    // 3. length of bytes data (32 bytes)
    let inner_len = inner.len();
    result.extend_from_slice(&U256::from(inner_len).to_be_bytes::<32>());

    // 4. bytes data itself (padded to 32-byte boundary)
    result.extend_from_slice(&inner);
    // Pad to 32-byte boundary if needed
    let padding = (32 - (inner.len() % 32)) % 32;
    result.extend_from_slice(&vec![0u8; padding]);

    Bytes::from(result)
}

/// Setup test environment: deploy token and prepare for interop transfer
async fn setup_token_on_chain_a(
    provider: &zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider,
    sender: Address,
) -> Result<(
    TestERC20::TestERC20Instance<zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider>,
    U256,
    [u8; 32],
)> {
    // Deploy ERC20 token
    let initial_supply = U256::from(1_000_000) * U256::from(10).pow(U256::from(18));
    let token = TestERC20::deploy(
        provider.clone(),
        U256::ZERO,
        "Test Token".to_string(),
        "TEST".to_string(),
    )
    .await?;

    // Mint tokens to sender
    token
        .mint(sender, initial_supply)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let balance = token.balanceOf(sender).call().await?;
    assert_eq!(balance, initial_supply, "Token minting failed");

    // Register token with Native Token Vault
    let chain_id = provider.get_chain_id().await?;
    let vault = IL2NativeTokenVault::new(L2_NATIVE_TOKEN_VAULT_ADDRESS, provider);
    vault
        .ensureTokenIsRegistered(*token.address())
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let asset_id = compute_asset_id(chain_id, *token.address());

    Ok((token, initial_supply, asset_id))
}

/// Relayer functionality: wait for finalization and obtain message proof
async fn relayer_get_message_proof(
    provider: &impl ZksyncApi,
    tx_hash: FixedBytes<32>,
    block_number: u64,
) -> Result<zksync_os_rpc_api::types::L2ToL1LogProof> {
    let poll_interval = tokio::time::Duration::from_millis(10);
    let timeout = tokio::time::Duration::from_secs(300); // 5 minutes
    let start = tokio::time::Instant::now();

    // Wait for the block to be finalized
    loop {
        if start.elapsed() > timeout {
            anyhow::bail!("Block was not finalized in time");
        }

        if let Ok(finalized_block) = provider.get_block_number().await
            && finalized_block >= block_number
        {
            break;
        }

        tokio::time::sleep(poll_interval).await;
    }

    // Get the log proof
    let log_proof = loop {
        if start.elapsed() > timeout {
            anyhow::bail!("Log proof was not available in time");
        }

        if let Ok(Some(proof)) = provider.get_l2_to_l1_log_proof(tx_hash, 0).await {
            break proof;
        }

        tokio::time::sleep(poll_interval).await;
    };

    Ok(log_proof)
}

/// Fund a wallet on L2 via L1 deposit
async fn fund_wallet_via_l1_deposit(tester: &Tester, wallet: Address, amount: U256) -> Result<()> {
    let chain_id = tester.l2_provider.get_chain_id().await?;

    let bridgehub = Bridgehub::new(
        tester.l2_zk_provider.get_bridgehub_contract().await?,
        tester.l1_provider.clone(),
        chain_id,
    );

    let max_priority_fee_per_gas = tester.l1_provider.get_max_priority_fee_per_gas().await?;
    let base_l1_fees_data = tester
        .l1_provider
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = base_l1_fees_data.max_fee_per_gas + max_priority_fee_per_gas;
    let gas_limit = tester
        .l2_provider
        .estimate_gas(
            TransactionRequest::default()
                .transaction_type(L1PriorityTxType::TX_TYPE)
                .from(wallet)
                .to(wallet)
                .value(amount),
        )
        .await?;

    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await?;

    let l1_deposit_request = bridgehub
        .request_l2_transaction_direct(
            amount + tx_base_cost,
            wallet,
            amount,
            vec![],
            gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            wallet,
        )
        .value(amount + tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();

    let l1_deposit_receipt = tester
        .l1_provider
        .send_transaction(l1_deposit_request)
        .await?
        .expect_successful_receipt()
        .await?;

    let l1_to_l2_tx_log = l1_deposit_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .expect("no L1->L2 logs produced by deposit tx");
    let l2_tx_hash = l1_to_l2_tx_log.inner.txHash;

    // Wait for L2 transaction to be included
    PendingTransactionBuilder::new(tester.l2_zk_provider.root().clone(), l2_tx_hash)
        .expect_successful_receipt()
        .await?;

    Ok(())
}

#[test(tokio::test)]
async fn test_interop_bundle_send() -> Result<()> {
    // This test validates the first part of the interop flow:
    // setting up two chains and sending an interop bundle from chain A to chain B

    let multi_chain = MultiChainTester::setup(2).await?;

    let chain_a = multi_chain.chain_a();
    let chain_b = multi_chain.chain_b();

    let chain_a_id = chain_a.l2_provider.get_chain_id().await?;
    let chain_b_id = chain_b.l2_provider.get_chain_id().await?;

    let sender = chain_a.l2_wallet.default_signer().address();

    // Fund sender wallet on both chains via L1 deposits
    let deposit_amount = U256::from(1000) * U256::from(10).pow(U256::from(18)); // 1000 ETH
    fund_wallet_via_l1_deposit(chain_a, sender, deposit_amount).await?;
    fund_wallet_via_l1_deposit(chain_b, sender, deposit_amount).await?;

    let (token, _initial_supply, asset_id) =
        setup_token_on_chain_a(&chain_a.l2_provider, sender).await?;

    let amount_to_send = U256::from(100) * U256::from(10).pow(U256::from(18));

    token
        .approve(L2_NATIVE_TOKEN_VAULT_ADDRESS, amount_to_send)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    // Build interop bundle
    let second_bridge_calldata = build_second_bridge_calldata(
        asset_id,
        amount_to_send,
        sender,        // recipient on chain B
        Address::ZERO, // maybeTokenAddress = 0
    );

    // Build call attributes with indirectCall
    let call_attributes = vec![Bytes::from(
        indirectCallCall {
            _gasLimit: U256::ZERO,
        }
        .abi_encode(),
    )];

    let to_address = format_evm_v1_address_only(L2_ASSET_ROUTER_ADDRESS);

    let calls = vec![IInteropCenter::InteropCallStarter {
        to: to_address,
        data: second_bridge_calldata,
        callAttributes: call_attributes,
    }];

    // Build bundle attributes with unbundlerAddress
    let bundle_attributes = vec![Bytes::from(
        unbundlerAddressCall {
            _address: format_evm_v1_address_only(sender),
        }
        .abi_encode(),
    )];

    // Send bundle to chain B
    let interop_center = IInteropCenter::new(L2_INTEROP_CENTER_ADDRESS, &chain_a.l2_provider);
    let destination_chain_id = format_evm_v1(chain_b_id);

    // Send sendBundle transaction
    let pending_tx = interop_center
        .sendBundle(destination_chain_id.clone(), calls, bundle_attributes)
        .value(U256::ZERO)
        .gas(10_000_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(0)
        .send()
        .await
        .context("Failed to send bundle transaction")?;

    let hash = *pending_tx.tx_hash();
    let receipt = pending_tx.get_receipt().await?;

    if !receipt.status() {
        anyhow::bail!("Bundle transaction reverted on chain A");
    }

    // Extract bundle data from the L1Messenger log (log #3)
    let l1_messenger_log = receipt.logs().get(3).expect("L1Messenger log not found");
    assert_eq!(l1_messenger_log.address(), L1_MESSENGER_ADDRESS.as_slice());

    // Decode the log data as bytes (it's ABI-encoded)
    let bundle_with_prefix: Bytes =
        <alloy::sol_types::sol_data::Bytes as SolType>::abi_decode(&l1_messenger_log.data().data)
            .expect("Failed to decode bundle from log");

    let bundle = Bytes::from(bundle_with_prefix[1..].to_vec()); // Remove 0x01 prefix

    // Get message proof
    let block_number = receipt.block_number.expect("Block number not found");
    let log_proof = relayer_get_message_proof(&chain_a.l2_zk_provider, hash, block_number).await?;

    // Build MessageInclusionProof
    let proof_data: Vec<FixedBytes<32>> = log_proof.proof.clone();

    let message_inclusion_proof = IInteropHandler::MessageInclusionProof {
        chainId: U256::from(chain_a_id),
        l1BatchNumber: U256::from(log_proof.batch_number),
        l2MessageIndex: U256::from(log_proof.id),
        message: IInteropHandler::L2Message {
            txNumberInBatch: receipt
                .transaction_index
                .expect("Transaction index not found") as u16,
            sender: L2_INTEROP_CENTER_ADDRESS,
            data: bundle_with_prefix.clone(),
        },
        proof: proof_data,
    };

    // Execute bundle on chain B
    let interop_handler = IInteropHandler::new(L2_INTEROP_HANDLER_ADDRESS, &chain_b.l2_provider);

    let execute_call =
        interop_handler.executeBundle(bundle.clone(), message_inclusion_proof.clone());

    // Send executeBundle with high gas limit
    let pending_tx = execute_call
        .gas(50_000_000)
        .send()
        .await
        .context("Failed to send executeBundle transaction")?;

    let execute_receipt = pending_tx
        .get_receipt()
        .await
        .context("Failed to get executeBundle receipt")?;

    if !execute_receipt.status() {
        anyhow::bail!("executeBundle transaction reverted on chain B");
    }

    // Verify token balance on chain B
    let vault_b = IL2NativeTokenVault::new(L2_NATIVE_TOKEN_VAULT_ADDRESS, &chain_b.l2_provider);
    let asset_id_bytes = FixedBytes::<32>::from(asset_id);
    let token_b_address = vault_b.tokenAddress(asset_id_bytes).call().await?;

    let token_b = TestERC20::new(token_b_address, &chain_b.l2_provider);
    let balance_b = token_b.balanceOf(sender).call().await?;

    if balance_b < amount_to_send {
        anyhow::bail!(
            "Token balance verification failed: expected {}, got {}",
            amount_to_send / U256::from(10u128.pow(18)),
            balance_b / U256::from(10u128.pow(18))
        );
    }

    tracing::info!(
        "✅ Interop transfer successful: {} -> {}",
        chain_a_id,
        chain_b_id
    );

    Ok(())
}

#[test(tokio::test)]
#[ignore = "Requires two running L2 chains with wallet setup"]
async fn test_interop_erc20_transfer_manual() -> Result<()> {
    // This test would require:
    // - Two L2 chains running on localhost:3050 and localhost:3051
    // - Wallet with funds on both chains
    // - Interop relayer running to propagate roots
    Ok(())
}

#[test(tokio::test)]
#[ignore = "Requires relayer integration - to be implemented"]
async fn test_interop_root_propagation() -> Result<()> {
    // This test would verify that interop roots are properly propagated between chains
    // via the interop relayer.
    //
    // Steps:
    // 1. Set up two chains with MultiChainTester
    // 2. Send a transaction on chain A that produces an L2->L1 log
    // 3. Wait for the transaction to be included in a batch
    // 4. Start the interop relayer (or mock root propagation)
    // 5. Verify that the root appears in L2InteropRootStorage on chain B
    Ok(())
}
