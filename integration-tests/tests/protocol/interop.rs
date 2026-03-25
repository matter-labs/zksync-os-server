//! Interop integration tests for cross-chain token transfers.

use alloy::{
    eips::eip1559::Eip1559Estimation,
    primitives::{Address, Bytes, FixedBytes, U256, address, keccak256},
    providers::utils::Eip1559Estimator,
    providers::{PendingTransactionBuilder, Provider},
    rpc::types::{TransactionReceipt, TransactionRequest},
    sol,
    sol_types::{SolCall, SolType, SolValue},
};
use anyhow::{Context, Result};
use zksync_os_contract_interface::Bridgehub;
use zksync_os_contract_interface::IMailbox::NewPriorityRequest;
use zksync_os_integration_tests::assert_traits::ProviderAssert;
use zksync_os_integration_tests::dyn_wallet_provider::EthDynProvider;
use zksync_os_integration_tests::{
    GatewayTester, Tester, assert_traits::ReceiptAssert, contracts::TestERC20, provider::ZksyncApi,
};
use zksync_os_rpc_api::types::LogProofTarget;
use zksync_os_types::{
    L1PriorityTxType, L1TxType, L2_INTEROP_CENTER_ADDRESS, REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
};

const L2_INTEROP_HANDLER_ADDRESS: Address = address!("000000000000000000000000000000000001000e");
const L2_NATIVE_TOKEN_VAULT_ADDRESS: Address = address!("0000000000000000000000000000000000010004");
const L2_ASSET_ROUTER_ADDRESS: Address = address!("0000000000000000000000000000000000010003");
const L1_MESSENGER_ADDRESS: Address = address!("0000000000000000000000000000000000008008");
const L2_MESSAGE_VERIFICATION_ADDRESS: Address =
    address!("0000000000000000000000000000000000010009");

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

        function interopProtocolFee() external view returns (uint256);

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

    // Bundle attribute functions for encoding
    function indirectCall(uint256 _gasLimit) external pure returns (bytes memory);
    function unbundlerAddress(bytes calldata _address) external pure returns (bytes memory);

    #[sol(rpc)]
    contract IL1Messenger {
        function sendToL1(bytes calldata _message) external returns (bytes32);
    }

    #[sol(rpc)]
    contract IMessageVerification {
        struct L2Message {
            uint16 txNumberInBatch;
            address sender;
            bytes data;
        }

        function proveL2MessageInclusionShared(
            uint256 _chainId,
            uint256 _blockOrBatchNumber,
            uint256 _index,
            L2Message calldata _message,
            bytes32[] calldata _proof
        ) external view returns (bool);
    }

    #[sol(rpc)]
    interface IGWAssetTrackerSettlement {
        function wrappedZKToken() external view returns (address);
        function agreeToPaySettlementFees(uint256 chainId) external;
    }

    #[sol(rpc)]
    interface IWrappedBaseToken {
        function deposit() external payable;
        function approve(address spender, uint256 amount) external returns (bool);
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

/// Setup test environment: deploy token on L1 and deposit to Chain A via the bridge.
///
/// This is the correct approach for tokens used in interop bundles on gateway-settled
/// chains. Tokens deposited from L1 populate `GWAssetTracker.chainBalance`, which is
/// required for `_decreaseChainBalance` to succeed when the interop bundle is processed.
///
/// Native L2 tokens (deployed directly on L2) cannot be used here because
/// `GWAssetTracker.registerNewTokenIfNeeded` always reverts, leaving their
/// chain balance at zero.
async fn setup_l1_token_on_chain_a(
    chain_a: &Tester,
    l2_recipient: Address,
) -> Result<(TestERC20::TestERC20Instance<EthDynProvider>, U256, [u8; 32])> {
    let deposit_amount = U256::from(1_000_000) * U256::from(10).pow(U256::from(18));

    // Deploy TestERC20 on L1 and mint to the L1 wallet.
    let l1_token = TestERC20::deploy(
        chain_a.l1_provider().clone(),
        U256::ZERO,
        "Test Token".to_string(),
        "TEST".to_string(),
    )
    .await
    .context("deploy L1 ERC20")?;

    l1_token
        .mint(
            chain_a.l1_wallet().default_signer().address(),
            deposit_amount,
        )
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    // Deposit the full supply from L1 to Chain A. This routes through
    // handleChainBalanceIncreaseOnGateway on the gateway, populating
    // GWAssetTracker.chainBalance[chainA][assetId].
    let l1_receipt =
        deposit_erc20_to_chain(chain_a, &l1_token, l2_recipient, deposit_amount).await?;

    // Wait for the L2 priority transaction to be included and succeed.
    let l1_to_l2_tx_log = l1_receipt
        .logs()
        .iter()
        .filter_map(|log| log.log_decode::<NewPriorityRequest>().ok())
        .next()
        .expect("no L1->L2 log produced by ERC20 deposit tx");
    PendingTransactionBuilder::new(
        chain_a.l2_zk_provider.root().clone(),
        l1_to_l2_tx_log.inner.txHash,
    )
    .expect_successful_receipt()
    .await
    .context("L2 part of ERC20 deposit")?;

    // Asset ID for L1-origin tokens: keccak256(l1ChainId, NTV_ADDRESS, l1TokenAddress).
    let l1_chain_id = chain_a.l1_provider().get_chain_id().await?;
    let asset_id = compute_asset_id(l1_chain_id, *l1_token.address());

    // Resolve the L2 token address that the NTV minted during the deposit.
    let vault = IL2NativeTokenVault::new(L2_NATIVE_TOKEN_VAULT_ADDRESS, &chain_a.l2_provider);
    let l2_token_addr = vault
        .tokenAddress(FixedBytes::<32>::from(asset_id))
        .call()
        .await?;
    let l2_token = TestERC20::new(l2_token_addr, chain_a.l2_provider.clone());

    Ok((l2_token, deposit_amount, asset_id))
}

/// Deposit an L1 ERC20 token to a chain via the bridge.
///
/// Mirrors the `deposit_erc20` helper in `erc20.rs` but derives the chain ID
/// directly from the RPC instead of from a static config, so it works with any
/// chain in a multi-chain test setup.
async fn deposit_erc20_to_chain(
    chain: &Tester,
    l1_erc20: &TestERC20::TestERC20Instance<EthDynProvider>,
    to: Address,
    amount: U256,
) -> Result<TransactionReceipt> {
    let chain_id = chain.l2_provider.get_chain_id().await?;
    let bridgehub = Bridgehub::new(
        chain.l2_zk_provider.get_bridgehub_contract().await?,
        chain.l1_provider().clone(),
        chain_id,
    );

    let max_priority_fee_per_gas = chain.l1_provider().get_max_priority_fee_per_gas().await?;
    let base_fees = chain
        .l1_provider()
        .estimate_eip1559_fees_with(Eip1559Estimator::new(|base_fee_per_gas, _| {
            Eip1559Estimation {
                max_fee_per_gas: base_fee_per_gas * 3 / 2,
                max_priority_fee_per_gas: 0,
            }
        }))
        .await?;
    let max_fee_per_gas = base_fees.max_fee_per_gas + max_priority_fee_per_gas;

    let l2_gas_limit = 1_500_000_u64;
    let tx_base_cost = bridgehub
        .l2_transaction_base_cost(
            max_fee_per_gas + max_priority_fee_per_gas,
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
        )
        .await?;

    let shared_bridge_address = bridgehub.shared_bridge_address().await?;

    // Approve the bridge to spend the L1 tokens.
    l1_erc20
        .approve(shared_bridge_address, amount)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let second_bridge_calldata = (*l1_erc20.address(), amount, to).abi_encode();

    let deposit_request = bridgehub
        .request_l2_transaction_two_bridges(
            tx_base_cost,
            U256::ZERO,
            l2_gas_limit,
            REQUIRED_L1_TO_L2_GAS_PER_PUBDATA_BYTE,
            to,
            shared_bridge_address,
            U256::ZERO,
            second_bridge_calldata,
        )
        .value(tx_base_cost)
        .max_fee_per_gas(max_fee_per_gas)
        .max_priority_fee_per_gas(max_priority_fee_per_gas)
        .into_transaction_request();

    chain
        .l1_provider()
        .send_transaction(deposit_request)
        .await?
        .expect_successful_receipt()
        .await
        .context("L1 ERC20 deposit transaction")
}

/// Relayer functionality: wait for finalization and obtain message proof (MessageRoot variant).
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

        if let Ok(Some(proof)) = provider
            .get_l2_to_l1_log_proof_with_target(tx_hash, 0, LogProofTarget::MessageRoot)
            .await
        {
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
        tester.l1_provider().clone(),
        chain_id,
    );

    let max_priority_fee_per_gas = tester.l1_provider().get_max_priority_fee_per_gas().await?;
    let base_l1_fees_data = tester
        .l1_provider()
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
        .l1_provider()
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

#[test_log::test(tokio::test)]
async fn test_interop_l2_to_l1_message_verification() -> anyhow::Result<()> {
    // 1. Send an L2->L1 message ("hello interop") on chain A
    // 2. Wait for block finalization and obtain the log proof
    // 3. Wait for the interop root to appear on chain B
    // 4. Call proveL2MessageInclusionShared on chain B and assert it returns true

    // 2 L2 chains + gateway.
    let multi_chain = GatewayTester::setup(2).await?;

    let chain_a = multi_chain.chain_a();
    let chain_b = multi_chain.chain_b();
    let gateway = multi_chain.gateway();

    let chain_a_id = chain_a.l2_provider.get_chain_id().await?;
    let gw_chain_id = gateway.l2_provider.get_chain_id().await?;
    let sender = chain_a.l2_wallet.default_signer().address();

    // Fund sender on chain A
    let deposit_amount = U256::from(100) * U256::from(10).pow(U256::from(18));
    fund_wallet_via_l1_deposit(chain_a, sender, deposit_amount).await?;

    // Send L2 -> L1 message via IL1Messenger
    let messenger = IL1Messenger::new(L1_MESSENGER_ADDRESS, &chain_a.l2_provider);
    let message_data = Bytes::from(b"hello interop".to_vec());

    let receipt = messenger
        .sendToL1(message_data.clone())
        .gas(100_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(0)
        .send()
        .await?
        .expect_successful_receipt()
        .await?;

    let block_number = receipt.block_number.expect("Block number not found");
    let tx_hash = receipt.transaction_hash;

    // Wait for block finalization and get the L2->L1 log proof (MessageRoot variant)
    let log_proof =
        relayer_get_message_proof(&chain_a.l2_zk_provider, tx_hash, block_number).await?;

    let gw_block_number = log_proof
        .gateway_block_number
        .expect("MessageRoot proof must contain gateway_block_number");

    // Wait for interop root to become available on chain B, keyed by gateway chain + GW block
    chain_b
        .l2_provider
        .expect_interop_root_inclusion(gw_chain_id, gw_block_number)
        .await?;

    // Verify message inclusion on chain B
    let verifier = IMessageVerification::new(L2_MESSAGE_VERIFICATION_ADDRESS, &chain_b.l2_provider);

    let included = verifier
        .proveL2MessageInclusionShared(
            U256::from(chain_a_id),
            U256::from(log_proof.batch_number),
            U256::from(log_proof.id),
            IMessageVerification::L2Message {
                txNumberInBatch: receipt
                    .transaction_index
                    .expect("Transaction index not found") as u16,
                sender,
                data: message_data,
            },
            log_proof.proof.clone(),
        )
        .call()
        .await?;

    assert!(included, "Message was NOT included in the interop proof");

    tracing::info!("✅ Interop L2->L1 message verification successful");

    Ok(())
}

#[test_log::test(tokio::test)]
async fn test_interop_bundle_send() -> Result<()> {
    // This test validates the first part of the interop flow:
    // setting up two chains and sending an interop bundle from chain A to chain B

    let multi_chain = GatewayTester::setup(2).await?;

    let chain_a = multi_chain.chain_a();
    let chain_b = multi_chain.chain_b();
    let gateway = multi_chain.gateway();

    let chain_a_id = chain_a.l2_provider.get_chain_id().await?;
    let chain_b_id = chain_b.l2_provider.get_chain_id().await?;
    let gw_chain_id = gateway.l2_provider.get_chain_id().await?;

    let sender = chain_a.l2_wallet.default_signer().address();

    // Fund sender wallet on both chains via L1 deposits
    let deposit_amount = U256::from(1000) * U256::from(10).pow(U256::from(18)); // 1000 ETH
    fund_wallet_via_l1_deposit(chain_a, sender, deposit_amount).await?;
    fund_wallet_via_l1_deposit(chain_b, sender, deposit_amount).await?;

    let (token, _deposited_amount, asset_id) = setup_l1_token_on_chain_a(chain_a, sender).await?;

    let amount_to_send = U256::from(100) * U256::from(10).pow(U256::from(18));

    token
        .approve(L2_NATIVE_TOKEN_VAULT_ADDRESS, amount_to_send)
        // fixme: temporary measure while v31 zksync-os does not support estimation with gasPrice=0
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(0)
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

    // The InteropCenter charges interopProtocolFee per call as msg.value (when not using fixed ZK fees).
    let protocol_fee = interop_center.interopProtocolFee().call().await?;
    let bundle_value = protocol_fee * U256::from(calls.len());

    // Send sendBundle transaction
    let receipt = interop_center
        .sendBundle(destination_chain_id.clone(), calls, bundle_attributes)
        .value(bundle_value)
        .gas(10_000_000)
        .max_fee_per_gas(1_000_000_000)
        .max_priority_fee_per_gas(0)
        .send()
        .await
        .context("Failed to send bundle transaction")?
        .expect_successful_receipt()
        .await
        .context("sendBundle on chain A")?;

    // Extract bundle data from the L1Messenger log.
    let l1_messenger_log = receipt
        .logs()
        .iter()
        .find(|log| log.address() == L1_MESSENGER_ADDRESS)
        .expect("L1Messenger log not found");

    // Decode the log data as bytes (it's ABI-encoded)
    let bundle_with_prefix: Bytes =
        <alloy::sol_types::sol_data::Bytes as SolType>::abi_decode(&l1_messenger_log.data().data)
            .expect("Failed to decode bundle from log");

    let bundle = Bytes::from(bundle_with_prefix[1..].to_vec()); // Remove 0x01 prefix

    // Get message proof
    let block_number = receipt.block_number.expect("Block number not found");
    let log_proof = relayer_get_message_proof(
        &chain_a.l2_zk_provider,
        receipt.transaction_hash,
        block_number,
    )
    .await?;

    let gw_block_number = log_proof
        .gateway_block_number
        .expect("MessageRoot proof must contain gateway_block_number");

    // Wait for interop root to get included on chain B, keyed by gateway chain + GW block
    chain_b
        .l2_provider
        .expect_interop_root_inclusion(gw_chain_id, gw_block_number)
        .await?;

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
    let _execute_receipt = execute_call
        .gas(50_000_000)
        .send()
        .await
        .context("Failed to send executeBundle transaction")?
        .expect_successful_receipt()
        .await
        .context("executeBundle on chain B")?;

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

#[test_log::test(tokio::test)]
#[ignore = "Requires two running L2 chains with wallet setup"]
async fn test_interop_erc20_transfer_manual() -> Result<()> {
    // This test would require:
    // - Two L2 chains running on localhost:3050 and localhost:3051
    // - Wallet with funds on both chains
    // - Interop relayer running to propagate roots
    Ok(())
}

#[test_log::test(tokio::test)]
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
