// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// Reads the whole environment-opcode family and emits everything as events,
/// driven by the `context_probe` workload. The emitted logs are part of the
/// block's output commitment, so if two validators ever disagree on any of
/// these values — a timestamp, a basefee, a code hash — the block fails
/// verification on one of them before it can finalize; under an observer's
/// lagging apply, the same divergence surfaces in receipt comparison instead.
contract ContextProbe {
    event TxContext(address caller, address origin, uint256 callvalue, uint256 gasprice, uint256 gasStart);
    event BlockContext(
        uint256 number,
        uint256 timestamp,
        address coinbase,
        uint256 prevrandao,
        uint256 gaslimit,
        uint256 basefee,
        uint256 chainid,
        bytes32 recentBlockhash
    );
    event CodeContext(
        uint256 selfBalance,
        uint256 callerBalance,
        uint256 selfCodeSize,
        bytes32 selfCodeHash,
        bytes32 selfCodeKeccak,
        uint256 eoaCodeSize,
        bytes32 eoaCodeHash
    );

    function probe(address someEoa) external payable {
        emit TxContext(msg.sender, tx.origin, msg.value, tx.gasprice, gasleft());

        // blockhash of the previous block: recent enough to always be served.
        bytes32 recent = block.number > 0 ? blockhash(block.number - 1) : bytes32(0);
        emit BlockContext(
            block.number,
            block.timestamp,
            block.coinbase,
            block.prevrandao,
            block.gaslimit,
            block.basefee,
            block.chainid,
            recent
        );

        // EXTCODECOPY our own runtime code and hash it, alongside EXTCODEHASH,
        // plus the same questions asked about a plain EOA.
        address self = address(this);
        uint256 size;
        bytes32 rawHash;
        assembly {
            size := extcodesize(self)
        }
        bytes memory code = new bytes(size);
        assembly {
            extcodecopy(self, add(code, 32), 0, size)
            rawHash := extcodehash(self)
        }
        uint256 eoaSize;
        bytes32 eoaHash;
        assembly {
            eoaSize := extcodesize(someEoa)
            eoaHash := extcodehash(someEoa)
        }
        emit CodeContext(
            address(this).balance,
            msg.sender.balance,
            size,
            rawHash,
            keccak256(code),
            eoaSize,
            eoaHash
        );
    }
}
