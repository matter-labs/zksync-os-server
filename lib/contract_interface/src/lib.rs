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
    interface IBridgehub {
        function getZKChain(uint256 _chainId) external view returns (address);
    }
}
