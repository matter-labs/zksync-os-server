// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// Failure modes on demand, driven by the `failing` workload: transactions that
/// are *supposed* to end badly, so the rig exercises failed-transaction handling
/// (status-0 receipts identical on every node, no mempool wedging) as a
/// first-class citizen.
contract Reverter {
    error ChaosCustomError(uint256 seed);

    uint256 private sink;

    /// Every mode ends in a failed transaction; the caller picks how.
    ///   0: require(false) with a reason string
    ///   1: a custom error
    ///   2: assert(false) — a Panic(0x01)
    ///   3: revert with ~4 KiB of revert data (returndata on the failure path)
    ///   4: burn gas until out of gas (pair with a modest gas limit)
    function fail(uint8 mode, uint256 seed) external {
        if (mode == 0) {
            require(false, "chaos: planned revert");
        } else if (mode == 1) {
            revert ChaosCustomError(seed);
        } else if (mode == 2) {
            assert(false);
        } else if (mode == 3) {
            bytes memory blob = new bytes(4096);
            for (uint256 i = 0; i < blob.length; i += 32) {
                assembly {
                    mstore(add(add(blob, 32), i), seed)
                }
            }
            assembly {
                revert(add(blob, 32), mload(blob))
            }
        } else {
            // Touch storage forever; the transaction's gas limit decides when
            // this dies of out-of-gas.
            uint256 acc = seed;
            while (true) {
                acc = uint256(keccak256(abi.encode(acc)));
                sink = acc;
            }
        }
    }
}
