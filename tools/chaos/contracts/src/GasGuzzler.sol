// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// Burns most of a transaction's gas in a chosen way, driven by the
/// `gas_guzzler` workload: block-full conditions on demand, and — because every
/// validator re-executes proposals before voting — worst-case *verification*
/// time, probing whether view timing holds when blocks are expensive.
contract GasGuzzler {
    event Burned(uint8 mode, uint256 rounds, uint256 gasLeft);

    mapping(uint256 => uint256) private churn;
    uint256 private scratch;

    /// Stop burning when this much gas remains: enough for the event, the
    /// return path, and the refund bookkeeping.
    uint256 private constant FLOOR = 60_000;

    ///   0: keccak treadmill (pure compute)
    ///   1: memory expansion (quadratic-cost surface)
    ///   2: cold SSTORE churn (fresh slots each round — worst-case writes)
    ///   3: event spam (log/bloom pressure)
    function burn(uint8 mode, uint256 seed) external {
        uint256 rounds = 0;
        if (mode == 0) {
            bytes32 acc = bytes32(seed);
            while (gasleft() > FLOOR) {
                acc = keccak256(abi.encode(acc, rounds));
                rounds++;
            }
            scratch = uint256(acc);
        } else if (mode == 1) {
            // Each round touches a fresh, larger memory region.
            while (gasleft() > FLOOR + 100_000) {
                bytes memory region = new bytes(32 * 1024);
                assembly {
                    mstore(add(region, mload(region)), seed)
                }
                rounds++;
                seed = uint256(keccak256(abi.encode(seed)));
            }
        } else if (mode == 2) {
            while (gasleft() > FLOOR + 30_000) {
                churn[uint256(keccak256(abi.encode(seed, rounds)))] = rounds + 1;
                rounds++;
            }
        } else {
            while (gasleft() > FLOOR) {
                emit Burned(mode, rounds, gasleft());
                rounds++;
            }
        }
        emit Burned(mode, rounds, gasleft());
    }
}
