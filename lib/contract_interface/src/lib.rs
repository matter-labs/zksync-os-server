pub mod calldata;
pub mod models;

alloy::sol! {
    // `Messaging.sol`
    struct L2CanonicalTransaction {
        uint256 txType;
        uint256 from;
        uint256 to;
        uint256 gasLimit;
        uint256 gasPerPubdataByteLimit;
        uint256 maxFeePerGas;
        uint256 maxPriorityFeePerGas;
        uint256 paymaster;
        uint256 nonce;
        uint256 value;
        uint256[4] reserved;
        bytes data;
        bytes signature;
        uint256[] factoryDeps;
        bytes paymasterInput;
        bytes reservedDynamic;
    }

    // `Messaging.sol`
    #[derive(Debug)]
    struct InteropRoot {
        uint256 chainId;
        uint256 blockOrBatchNumber;
        bytes32[] sides;
    }

    interface ServerNotifier {
        event MigrateToGateway(uint256 indexed chainId, uint256 migrationNumber);
        event MigrateFromGateway(uint256 indexed chainId, uint256 migrationNumber);
        event UpgradeTimestampUpdated(uint256 indexed chainId, uint256 indexed protocolVersion, uint256 upgradeTimestamp);
    }

    interface ISystemContext {
        function setSettlementLayerChainId(uint256 _newSettlementLayerChainId);
    }

    interface IInteropCenter {
        function setInteropFee(uint256 _interopFee);
        function interopProtocolFee() external view returns (uint256);
    }

    #[sol(rpc)]
    interface IGWAssetTracker {
        function gatewaySettlementFee() external view returns (uint256);
    }

    // `DynamicIncrementalMerkle.sol`
    struct Bytes32PushTree {
        uint256 _nextLeafIndex;
        bytes32[] _sides;
        bytes32[] _zeros;
    }

    // `IMessageRoot.sol`
    #[sol(rpc)]
    interface IMessageRoot {
        // Event that is being emitted by GW
        event NewInteropRoot (
            uint256 indexed chainId,
            uint256 indexed blockNumber,
            uint256 indexed logId,
            bytes32[] sides
        );

        // Event that is being emmited by L1
        event AppendedChainRoot(uint256 indexed chainId, uint256 indexed batchNumber, bytes32 indexed chainRoot);

        function addInteropRoot (
            uint256 chainId,
            uint256 blockOrBatchNumber,
            bytes32[] calldata sides
        );

        function addInteropRootsInBatch(InteropRoot[] calldata interopRootsInput);

        uint256 public interopRootLogId;

        function getChainTree(uint256 chainId) public view returns (Bytes32PushTree);

        event AppendedChainBatchRoot(uint256 indexed chainId, uint256 indexed batchNumber, bytes32 chainBatchRoot);
        function getMerklePathForChain(uint256 _chainId) external view returns (bytes32[] memory);
        mapping(uint256 chainId => uint256 chainIndex) public chainIndex;
    }

    // `ZKChainStorage.sol`
    enum PubdataPricingMode {
        Rollup,
        Validium
    }

    // `IMailbox.sol`
    interface IMailbox {
        event NewPriorityRequest(
            uint256 txId,
            bytes32 txHash,
            uint64 expirationTimestamp,
            L2CanonicalTransaction transaction,
            bytes[] factoryDeps
        );
    }

    // `IBridgehub.sol`
    #[sol(rpc)]
    interface IBridgehub {
        function getZKChain(uint256 _chainId) external view returns (address);
        function chainTypeManager(uint256 _chainId) external view returns (address);
        function sharedBridge() public view returns (address);
        function getAllZKChainChainIDs() external view returns (uint256[] memory);
        function messageRoot() external view returns (address);
        function whitelistedSettlementLayers(uint256 _chainId) external view returns (bool);
        function chainAssetHandler() external view returns (address);

        struct L2TransactionRequestDirect {
            uint256 chainId;
            uint256 mintValue;
            address l2Contract;
            uint256 l2Value;
            bytes l2Calldata;
            uint256 l2GasLimit;
            uint256 l2GasPerPubdataByteLimit;
            bytes[] factoryDeps;
            address refundRecipient;
        }

        struct L2TransactionRequestTwoBridgesOuter {
            uint256 chainId;
            uint256 mintValue;
            uint256 l2Value;
            uint256 l2GasLimit;
            uint256 l2GasPerPubdataByteLimit;
            address refundRecipient;
            address secondBridgeAddress;
            uint256 secondBridgeValue;
            bytes secondBridgeCalldata;
        }

        function requestL2TransactionDirect(
            L2TransactionRequestDirect calldata _request
        ) external payable returns (bytes32 canonicalTxHash);

        function requestL2TransactionTwoBridges(
            L2TransactionRequestTwoBridgesOuter calldata _request
        ) external payable returns (bytes32 canonicalTxHash);

        function l2TransactionBaseCost(
            uint256 _chainId,
            uint256 _gasPrice,
            uint256 _l2GasLimit,
            uint256 _l2GasPerPubdataByteLimit
        ) external view returns (uint256);
    }

    #[sol(rpc)]
    interface IChainAssetHandler {
        struct MigrationInterval {
            uint256 migrateToGWBatchNumber;
            uint256 migrateFromGWBatchNumber;
            uint256 settlementLayerBatchLowerBound;
            uint256 settlementLayerBatchUpperBound;
            uint256 settlementLayerChainId;
            bool isActive;
        }

        function migrationNumber(uint256 _chainId) external view returns (uint256);
        event MigrationFinalized(
            uint256 indexed chainId,
            uint256 migrationNumber,
            bytes32 indexed assetId,
            address indexed zkChain
        );
        function migrationInterval(
            uint256 _chainId,
            uint256 _migrationNumber
        ) external view returns (MigrationInterval memory interval);
    }

    // `IChainTypeManager.sol`
    #[sol(rpc)]
    interface IChainTypeManager {
        address public validatorTimelockPostV29;

        function serverNotifierAddress() external view returns (address);

        enum Action {
            Add,
            Replace,
            Remove
        }

        struct FacetCut {
            address facet;
            Action action;
            bool isFreezable;
            bytes4[] selectors;
        }

        struct DiamondCutData {
            FacetCut[] facetCuts;
            address initAddress;
            bytes initCalldata;
        }

        struct VerifierParams {
            bytes32 recursionNodeLevelVkHash;
            bytes32 recursionLeafLevelVkHash;
            bytes32 recursionCircuitsSetVksHash;
        }

        struct ProposedUpgrade {
            L2CanonicalTransaction l2ProtocolUpgradeTx;
            bytes32 bootloaderHash;
            bytes32 defaultAccountHash;
            bytes32 evmEmulatorHash;
            address verifier;
            VerifierParams verifierParams;
            bytes l1ContractsUpgradeCalldata;
            bytes postUpgradeCalldata;
            uint256 upgradeTimestamp;
            uint256 newProtocolVersion;
        }

        /// Defines an upgrade from version A to version B
        event NewProtocolVersion(uint256 indexed oldProtocolVersion, uint256 indexed newProtocolVersion);

        /// Provides an actual data for the upgrade execution.
        event NewUpgradeCutData(uint256 indexed protocolVersion, DiamondCutData diamondCutData);

        /// Address of the L1 bytecodes supplier used for upgrades (v31+).
        function L1_BYTECODES_SUPPLIER() external view returns (address);

        /// The block number on the CTM's chain where `setUpgradeDiamondCutInner` ran for the
        /// given (old) protocol version. Non-zero means this CTM owns the upgrade cut data for
        /// that version. Populated starting with the V31 ChainTypeManager.
        function upgradeCutDataBlock(uint256 protocolVersion) external view returns (uint256);
    }

    // `SettlementLayerV31UpgradeBase.sol` — the per-chain upgrade init contract.
    // `NewUpgradeCutData` carries a placeholder `additionalForceDeploymentsData`
    // that `upgradeChainFromVersion` rewrites per-chain inside the delegatecall
    // via `getL2UpgradeTxData(bridgehub, chainId, existingTxData)`. The server
    // must call this before executing the L2 upgrade tx — otherwise the
    // placeholder's empty `additionalForceDeploymentsData` would revert inside
    // `performForceDeployedContractsInit`.
    #[sol(rpc)]
    interface ISettlementLayerV31Upgrade {
        function getL2UpgradeTxData(
            address _bridgehub,
            uint256 _chainId,
            bool _zksyncOS,
            bytes memory _existingTxData
        ) external view returns (bytes memory);
    }

    // `IZKChain.sol`
    #[sol(rpc)]
    interface IZKChain {
        function storedBatchHash(uint256 _batchNumber) external view returns (bytes32);
        function getTotalBatchesCommitted() external view returns (uint256);
        function getTotalBatchesVerified() external view returns (uint256);
        function getTotalBatchesExecuted() external view returns (uint256);
        function getTotalPriorityTxs() external view returns (uint256);
        function getPubdataPricingMode() external view returns (PubdataPricingMode);
        function getAdmin() external view returns (address);
        function getChainTypeManager() external view returns (address);
        function getProtocolVersion() external view returns (uint256);
        function getL2SystemContractsUpgradeTxHash() external view returns (bytes32);
        function getL2SystemContractsUpgradeBatchNumber() external view returns (uint256);
        function baseTokenGasPriceMultiplierNominator() external view returns (uint128);
        function baseTokenGasPriceMultiplierDenominator() external view returns (uint128);
        function getBaseToken() external view returns (address);
        function getSettlementLayer() external view returns (address);
    }

    // Taken from `common/Config.sol`
    enum L2DACommitmentScheme {
        NONE,
        EMPTY_NO_DA,
        PUBDATA_KECCAK256,
        BLOBS_AND_PUBDATA_KECCAK256,
        BLOBS_ZKSYNC_OS
    }

    // Taken from `IExecutor.sol`
    interface IExecutor {
        struct StoredBatchInfo {
            uint64 batchNumber;
            bytes32 batchHash;
            uint64 indexRepeatedStorageChanges;
            uint256 numberOfLayer1Txs;
            bytes32 priorityOperationsHash;
            bytes32 dependencyRootsRollingHash;
            bytes32 l2LogsTreeRoot;
            uint256 timestamp;
            bytes32 commitment;
        }

        struct CommitBatchInfoZKsyncOS {
            uint64 batchNumber;
            bytes32 newStateCommitment;
            uint256 numberOfLayer1Txs;
            uint256 numberOfLayer2Txs;
            bytes32 priorityOperationsHash;
            bytes32 dependencyRootsRollingHash;
            bytes32 l2LogsTreeRoot;
            L2DACommitmentScheme daCommitmentScheme;
            bytes32 daCommitment;
            uint64 firstBlockTimestamp;
            uint64 firstBlockNumber;
            uint64 lastBlockTimestamp;
            uint64 lastBlockNumber;
            uint256 chainId;
            bytes operatorDAInput;
            uint256 slChainId;
        }

        event BlockCommit(uint256 indexed batchNumber, bytes32 indexed batchHash, bytes32 indexed commitment);
        event BlockExecution(uint256 indexed batchNumber, bytes32 indexed batchHash, bytes32 indexed commitment);
        #[derive(Debug)]
        event ReportCommittedBatchRangeZKsyncOS(
            uint64 indexed batchNumber,
            uint64 indexed firstBlockNumber,
            uint64 indexed lastBlockNumber
        );
        #[derive(Debug)]
        event BlocksRevert(uint256 totalBatchesCommitted, uint256 totalBatchesVerified, uint256 totalBatchesExecuted);

        function commitBatchesSharedBridge(
            address _chainAddress,
            uint256 _processFrom,
            uint256 _processTo,
            bytes calldata _commitData
        ) external;

        function proofPayload(StoredBatchInfo old, StoredBatchInfo[] newInfo, uint256[] proof);

        function proveBatchesSharedBridge(
            address _chainAddress,
            uint256 _processBatchFrom,
            uint256 _processBatchTo,
            bytes calldata _proofData
        );

        struct PriorityOpsBatchInfo {
            bytes32[] leftPath;
            bytes32[] rightPath;
            bytes32[] itemHashes;
        }

        struct L2Log {
           uint8 l2ShardId;
           bool isService;
           uint16 txNumberInBatch;
           address sender;
           bytes32 key;
           bytes32 value;
       }

        function executeBatchesSharedBridge(
            address _chainAddress,
            uint256 _processFrom,
            uint256 _processTo,
            bytes calldata _executeData
        );
    }

    // taken from v29 version of `IExecutor.sol`
    // We need this to make the server work with the v29 version of contracts during the upgrade, and it can be removed after
    interface IExecutorV29 {
        struct CommitBatchInfoZKsyncOS {
            uint64 batchNumber;
            bytes32 newStateCommitment;
            uint256 numberOfLayer1Txs;
            bytes32 priorityOperationsHash;
            bytes32 dependencyRootsRollingHash;
            bytes32 l2LogsTreeRoot;
            address l2DaValidator;
            bytes32 daCommitment;
            uint64 firstBlockTimestamp;
            uint64 lastBlockTimestamp;
            uint256 chainId;
            bytes operatorDAInput;
        }
    }

    // taken from v30 version of `IExecutor.sol`
    // This format is still required to submit v30 batches before the upgrade to v31.
    interface IExecutorV30 {
        struct CommitBatchInfoZKsyncOS {
            uint64 batchNumber;
            bytes32 newStateCommitment;
            uint256 numberOfLayer1Txs;
            bytes32 priorityOperationsHash;
            bytes32 dependencyRootsRollingHash;
            bytes32 l2LogsTreeRoot;
            L2DACommitmentScheme daCommitmentScheme;
            bytes32 daCommitment;
            uint64 firstBlockTimestamp;
            uint64 firstBlockNumber;
            uint64 lastBlockTimestamp;
            uint64 lastBlockNumber;
            uint256 chainId;
            bytes operatorDAInput;
        }
    }

    // `IL1GenesisUpgrade.sol`
    interface IL1GenesisUpgrade {
        event GenesisUpgrade(
            address indexed _zkChain,
            L2CanonicalTransaction _l2Transaction,
            uint256 indexed _protocolVersion,
            bytes[] _factoryDeps
        );
    }

    // `IChainAdmin.sol`
    interface IChainAdmin {
        event UpdateUpgradeTimestamp(uint256 indexed protocolVersion, uint256 upgradeTimestamp);
    }

    // `IChainAdminOwnable.sol`
    #[sol(rpc)]
    interface IChainAdminOwnable {
        function setTokenMultiplier(address _chainContract, uint128 _nominator, uint128 _denominator) external;
        // Not present in `IChainAdminOwnable`, but `ChainAdminOwnable` which is the only implementor has it.
        function tokenMultiplierSetter() external view returns (address);
    }

    // `BytecodesSupplier.sol`
    interface IBytecodeSupplier {
        event EVMBytecodePublished(bytes32 indexed bytecodeHash, bytes bytecode);
    }

    #[sol(rpc)]
    interface IMultisigCommitter {

        function commitBatchesMultisig(
            address chainAddress,
            uint256 _processBatchFrom,
            uint256 _processBatchTo,
            bytes calldata _batchData,
            address[] calldata signers,
            bytes[] calldata signatures
        ) external;

        function getSigningThreshold(address chainAddress) external view returns (uint64);

        function isValidator(address chainAddress, address validator) external view returns (bool);

        function getValidatorsCount(address chainAddress) external view returns (uint256);

        function getValidatorsMember(address chainAddress, uint256 index) external view returns (address);
    }

    #[sol(rpc)]
    interface IERC20 {
        function decimals() external view returns (uint8);
    }
}
