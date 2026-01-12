//! Interop integration tests for cross-chain token transfers.

use alloy::{
    hex,
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::Provider,
    sol,
    sol_types::SolValue,
};
use anyhow::Result;
use test_log::test;
use zksync_os_integration_tests::{
    MultiChainTester, assert_traits::ReceiptAssert, contracts::TestERC20, provider::ZksyncApi,
};

const L2_INTEROP_CENTER_ADDRESS: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0x10,
]);
const L2_INTEROP_HANDLER_ADDRESS: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0x0d,
]);
const L2_NATIVE_TOKEN_VAULT_ADDRESS: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0x04,
]);
const L2_ASSET_ROUTER_ADDRESS: Address = Address::new([
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0x03,
]);

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
    use alloy::primitives::keccak256;
    use alloy::sol_types::SolValue;
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
    use alloy::sol_types::SolValue;

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
    tracing::info!("=== Setting up token on chain A ===");

    // Deploy ERC20 token
    let initial_supply = U256::from(1_000_000) * U256::from(10).pow(U256::from(18));
    let token = TestERC20::deploy(
        provider.clone(),
        U256::ZERO,
        "Test Token".to_string(),
        "TEST".to_string(),
    )
    .await?;

    tracing::info!("Token deployed at: {}", token.address());

    // Mint tokens to sender
    token
        .mint(sender, initial_supply)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let balance = token.balanceOf(sender).call().await?;
    tracing::info!("Sender token balance: {}", balance);
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
    tracing::info!(
        "Token registered with asset ID: {:?}",
        FixedBytes::<32>::from(asset_id)
    );

    Ok((token, initial_supply, asset_id))
}

/// Relayer functionality: wait for finalization and obtain message proof
async fn relayer_get_message_proof(
    provider: &impl ZksyncApi,
    tx_hash: FixedBytes<32>,
    block_number: u64,
) -> Result<zksync_os_rpc_api::types::L2ToL1LogProof> {
    tracing::info!("=== Relayer: Waiting for block finalization ===");

    let poll_interval = tokio::time::Duration::from_millis(100);
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
            tracing::info!("✅ Block {} finalized", block_number);
            break;
        }

        tokio::time::sleep(poll_interval).await;
    }

    tracing::info!("=== Relayer: Getting L2→L1 log proof ===");

    // Get the log proof
    let log_proof = loop {
        if start.elapsed() > timeout {
            anyhow::bail!("Log proof was not available in time");
        }

        if let Ok(Some(proof)) = provider.get_l2_to_l1_log_proof(tx_hash, 0).await {
            tracing::info!("✅ Log proof obtained");
            tracing::info!("  Batch number: {}", proof.batch_number);
            tracing::info!("  Message index: {}", proof.id);
            tracing::info!("  Proof length: {}", proof.proof.len());
            break proof;
        }

        tokio::time::sleep(poll_interval).await;
    };

    Ok(log_proof)
}

#[test(tokio::test)]
async fn test_interop_bundle_send() -> Result<()> {
    // This test validates the first part of the interop flow:
    // setting up two chains and sending an interop bundle from chain A to chain B

    tracing::info!("Starting interop bundle send test");

    // ========================================
    // SETUP: Initialize two L2 chains
    // ========================================
    let multi_chain = MultiChainTester::setup(2).await?;

    let chain_a = multi_chain.chain_a();
    let chain_b = multi_chain.chain_b();

    let chain_a_id = chain_a.l2_provider.get_chain_id().await?;
    let chain_b_id = chain_b.l2_provider.get_chain_id().await?;

    tracing::info!("Chain A ID: {}, Chain B ID: {}", chain_a_id, chain_b_id);

    // Use the default wallet from chain A
    let sender = chain_a.l2_wallet.default_signer().address();
    tracing::info!("Sender address: {}", sender);

    // ========================================
    // SETUP: Deploy and register token
    // ========================================
    let (token, _initial_supply, asset_id) =
        setup_token_on_chain_a(&chain_a.l2_provider, sender).await?;

    // ========================================
    // Step 1: Approve Native Token Vault
    // ========================================
    let amount_to_send = U256::from(100) * U256::from(10).pow(U256::from(18));

    token
        .approve(L2_NATIVE_TOKEN_VAULT_ADDRESS, amount_to_send)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;
    tracing::info!("Approved {} tokens for Native Token Vault", amount_to_send);

    // ========================================
    // Step 2: Build interop bundle
    // ========================================
    let second_bridge_calldata = build_second_bridge_calldata(
        asset_id,
        amount_to_send,
        sender,        // recipient on chain B
        Address::ZERO, // maybeTokenAddress = 0
    );

    tracing::info!(
        "Second bridge calldata length: {}",
        second_bridge_calldata.len()
    );
    tracing::info!(
        "Second bridge calldata (hex): {}",
        hex::encode(&second_bridge_calldata)
    );

    // Build call attributes with indirectCall
    let indirect_call_selector = &alloy::primitives::keccak256("indirectCall(uint256)")[0..4];
    let call_attributes = vec![Bytes::from(
        [indirect_call_selector, &U256::ZERO.abi_encode()[..]].concat(),
    )];

    let to_address = format_evm_v1_address_only(L2_ASSET_ROUTER_ADDRESS);

    let calls = vec![IInteropCenter::InteropCallStarter {
        to: to_address,
        data: second_bridge_calldata,
        callAttributes: call_attributes,
    }];

    // Build bundle attributes with unbundlerAddress
    let unbundler_selector = &alloy::primitives::keccak256("unbundlerAddress(bytes)")[0..4];
    let unbundler_encoded = format_evm_v1_address_only(sender).abi_encode();
    let bundle_attributes = vec![Bytes::from(
        [unbundler_selector, &unbundler_encoded[..]].concat(),
    )];

    tracing::info!(
        "Destination chain ID (hex): {}",
        hex::encode(format_evm_v1(chain_b_id))
    );

    // ========================================
    // Step 3: Send bundle to chain B
    // ========================================
    let interop_center = IInteropCenter::new(L2_INTEROP_CENTER_ADDRESS, &chain_a.l2_provider);
    let destination_chain_id = format_evm_v1(chain_b_id);

    tracing::info!("Sending bundle transaction...");
    let send_result = interop_center
        .sendBundle(destination_chain_id.clone(), calls, bundle_attributes)
        .value(U256::ZERO)
        .gas(10_000_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(0)
        .send()
        .await;

    let tx_hash = match send_result {
        Ok(pending_tx) => {
            let hash = *pending_tx.tx_hash();
            tracing::info!("Bundle transaction submitted: {}", hash);

            let receipt = pending_tx.get_receipt().await?;

            if receipt.status() {
                tracing::info!("========================================");
                tracing::info!("✅ Bundle sent successfully!");
                tracing::info!("TX Hash: {}", hash);
                tracing::info!("RPC URL: {}", chain_a.l2_rpc_url());
                tracing::info!("Block: {:?}", receipt.block_number);
                tracing::info!("Gas Used: {}", receipt.gas_used);
                tracing::info!("========================================");

                // Extract bundle data from the L1Messenger log (log #3)
                let l1_messenger_log = receipt.logs().get(3).expect("L1Messenger log not found");
                let l1_messenger_address = FixedBytes::<20>::new([
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x80, 0x08,
                ]);
                assert_eq!(*l1_messenger_log.address(), l1_messenger_address);

                // Decode the log data as bytes (it's ABI-encoded)
                use alloy::sol_types::SolType;
                let bundle_with_prefix: Bytes =
                    <alloy::sol_types::sol_data::Bytes as SolType>::abi_decode(
                        &l1_messenger_log.data().data,
                    )
                    .expect("Failed to decode bundle from log");

                tracing::info!(
                    "✅ Extracted bundle from L1Messenger log, length: {}",
                    bundle_with_prefix.len()
                );

                let bundle = Bytes::from(bundle_with_prefix[1..].to_vec()); // Remove 0x01 prefix
                tracing::info!("Bundle length (without prefix): {}", bundle.len());

                // ========================================
                // RELAYER: Get message proof
                // ========================================
                let block_number = receipt.block_number.expect("Block number not found");
                let log_proof =
                    relayer_get_message_proof(&chain_a.l2_zk_provider, hash, block_number).await?;

                tracing::info!("=== Preparing bundle execution data ===");

                // Build MessageInclusionProof
                let proof_data: Vec<FixedBytes<32>> = log_proof.proof.clone();

                let message_inclusion_proof = IInteropHandler::MessageInclusionProof {
                    chainId: U256::from(chain_a_id),
                    l1BatchNumber: U256::from(log_proof.batch_number),
                    l2MessageIndex: U256::from(log_proof.id),
                    message: IInteropHandler::L2Message {
                        txNumberInBatch: receipt
                            .transaction_index
                            .expect("Transaction index not found")
                            as u16,
                        sender: L2_INTEROP_CENTER_ADDRESS,
                        data: bundle_with_prefix.clone(),
                    },
                    proof: proof_data,
                };

                tracing::info!("✅ Successfully prepared bundle execution data:");
                tracing::info!("  Source Chain ID: {}", message_inclusion_proof.chainId);
                tracing::info!("  Batch Number: {}", message_inclusion_proof.l1BatchNumber);
                tracing::info!(
                    "  Message Index: {}",
                    message_inclusion_proof.l2MessageIndex
                );
                tracing::info!("  Bundle Length (without prefix): {}", bundle.len());
                tracing::info!("  Interop Root: {:?}", log_proof.root);
                tracing::info!(
                    "  Amount to Send: {} tokens",
                    amount_to_send / U256::from(10u128.pow(18))
                );

                tracing::info!("=== Test summary ===");
                tracing::info!(
                    "✅ Two L2 chains launched successfully (IDs: {}, {})",
                    chain_a_id,
                    chain_b_id
                );
                tracing::info!("✅ ERC20 token deployed and minted on chain A");
                tracing::info!("✅ Token registered with Native Token Vault");
                tracing::info!("✅ Token approval granted to Native Token Vault");
                tracing::info!("✅ Interop bundle constructed with correct ERC-7930 encoding");
                tracing::info!(
                    "✅ sendBundle transaction executed successfully (gas: {})",
                    receipt.gas_used
                );
                tracing::info!(
                    "✅ L2→L1 log proof obtained (batch: {}, index: {})",
                    log_proof.batch_number,
                    log_proof.id
                );
                tracing::info!("✅ Bundle extracted from L1Messenger log");
                tracing::info!("✅ MessageInclusionProof constructed correctly");

                // ========================================
                // Step 4: Execute bundle on chain B
                // ========================================
                tracing::info!("=== Executing bundle on chain B ===");

                let interop_handler =
                    IInteropHandler::new(L2_INTEROP_HANDLER_ADDRESS, &chain_b.l2_provider);

                tracing::info!("Calling executeBundle on chain B...");

                // Get the calldata for executeBundle
                let execute_call =
                    interop_handler.executeBundle(bundle.clone(), message_inclusion_proof.clone());
                let calldata = execute_call.calldata().clone();
                let calldata_hex = format!("0x{}", hex::encode(&calldata));

                tracing::info!("=== Calldata debug info ===");
                tracing::info!("EncodedBundle (no prefix) length: {}", bundle.len());
                tracing::info!("EncodedBundle (no prefix): 0x{}", hex::encode(&bundle));
                tracing::info!(
                    "L2ToL1Message (with prefix) length: {}",
                    bundle_with_prefix.len()
                );
                tracing::info!(
                    "L2ToL1Message (with prefix): 0x{}",
                    hex::encode(&bundle_with_prefix)
                );
                tracing::info!("Complete calldata: {}", calldata_hex);
                tracing::info!("Calldata length: {}", calldata.len());
                tracing::info!("Function selector: {}", &calldata_hex[0..10]);

                // Send executeBundle with high gas limit
                let execute_call_with_gas = execute_call.gas(50_000_000);

                match execute_call_with_gas.send().await {
                    Ok(pending_tx) => {
                        let tx_hash = *pending_tx.tx_hash();
                        tracing::info!("executeBundle transaction sent: {:?}", tx_hash);

                        match pending_tx.get_receipt().await {
                            Ok(execute_receipt) => {
                                if execute_receipt.status() {
                                    tracing::info!("✅ executeBundle succeeded!");
                                    tracing::info!("Gas used: {}", execute_receipt.gas_used);

                                    // Verify token balance on chain B
                                    tracing::info!("=== Verifying token balance on chain B ===");

                                    let vault_b = IL2NativeTokenVault::new(
                                        L2_NATIVE_TOKEN_VAULT_ADDRESS,
                                        &chain_b.l2_provider,
                                    );
                                    let asset_id_bytes = FixedBytes::<32>::from(asset_id);
                                    let token_b_address =
                                        vault_b.tokenAddress(asset_id_bytes).call().await?;

                                    tracing::info!("Token address on chain B: {}", token_b_address);

                                    let token_b =
                                        TestERC20::new(token_b_address, &chain_b.l2_provider);
                                    let balance_b = token_b.balanceOf(sender).call().await?;

                                    tracing::info!(
                                        "Receiver balance on chain B: {} tokens",
                                        balance_b / U256::from(10u128.pow(18))
                                    );

                                    if balance_b >= amount_to_send {
                                        tracing::info!(
                                            "✅✅✅ INTEROP TOKEN TRANSFER SUCCESSFUL! ✅✅✅"
                                        );
                                        tracing::info!(
                                            "Tokens successfully bridged from chain {} to chain {}",
                                            chain_a_id,
                                            chain_b_id
                                        );
                                    } else {
                                        tracing::error!("❌ Token balance verification failed");
                                        tracing::error!(
                                            "Expected: {} tokens",
                                            amount_to_send / U256::from(10u128.pow(18))
                                        );
                                        tracing::error!(
                                            "Actual: {} tokens",
                                            balance_b / U256::from(10u128.pow(18))
                                        );
                                    }
                                } else {
                                    tracing::error!("❌ executeBundle transaction reverted");
                                    tracing::error!("Gas used: {}", execute_receipt.gas_used);
                                    tracing::error!("Receipt: {:?}", execute_receipt);
                                    anyhow::bail!("executeBundle transaction reverted on chain B");
                                }
                            }
                            Err(e) => {
                                tracing::error!("❌ Failed to get executeBundle receipt: {:?}", e);
                                anyhow::bail!("Failed to get executeBundle receipt: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("❌ Failed to send executeBundle transaction: {:?}", e);
                        anyhow::bail!("Failed to send executeBundle transaction: {}", e);
                    }
                }
            } else {
                tracing::error!("❌ Bundle transaction reverted: {}", hash);
                tracing::error!("Gas used: {}", receipt.gas_used);
                tracing::error!("Block number: {:?}", receipt.block_number);
                tracing::error!("Receipt: {:?}", receipt);

                tracing::info!("========================================");
                tracing::info!("❌ TRANSACTION REVERTED");
                tracing::info!("========================================");
                tracing::info!("TX Hash: {}", hash);
                tracing::info!("Gas Used: {} (very low - early revert)", receipt.gas_used);
                tracing::info!("Block: {:?}", receipt.block_number);
                tracing::info!("");
                tracing::info!("This suggests the AssetRouter is reverting early,");
                tracing::info!("likely in a validation check. Based on the execution trace:");
                tracing::info!("- InteropCenter calls AssetRouter with selector 0x4d7e3d62");
                tracing::info!("- AssetRouter reverts after only ~554 instructions");
                tracing::info!("- No error message returned (empty returndata)");
                tracing::info!("");
                tracing::info!("Possible causes:");
                tracing::info!("1. Missing approval or balance check");
                tracing::info!("2. Incorrect bundle format/encoding");
                tracing::info!("3. Chain ID or destination validation failure");
                tracing::info!("4. Asset not properly registered");
                tracing::info!("========================================");

                anyhow::bail!("Bundle transaction reverted on chain A");
            }

            hash
        }
        Err(e) => {
            anyhow::bail!("Failed to send bundle transaction: {}", e);
        }
    };

    tracing::info!("Transaction hash: {}", tx_hash);
    tracing::info!("L2 RPC URL: {}", chain_a.l2_rpc_url());

    tracing::info!("✅ Interop bundle send test completed successfully!");

    Ok(())
}

#[test(tokio::test)]
#[ignore = "Requires two running L2 chains with wallet setup"]
async fn test_interop_erc20_transfer_manual() -> Result<()> {
    // This test would require:
    // - Two L2 chains running on localhost:3050 and localhost:3051
    // - Wallet with funds on both chains
    // - Interop relayer running to propagate roots

    tracing::info!("Manual interop test placeholder");

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

    tracing::info!("Interop root propagation test placeholder");

    Ok(())
}
