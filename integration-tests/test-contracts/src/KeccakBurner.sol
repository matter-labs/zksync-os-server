pragma solidity ^0.8.13;

/// Compute-heavy contract for integration-test stress cases.
contract KeccakBurner {
    bytes32 public lastDigest;

    function burn(uint256 iterations, bytes32 seed) external returns (bytes32 digest) {
        digest = seed;
        for (uint256 i; i < iterations; ) {
            digest = keccak256(abi.encodePacked(digest, i));
            unchecked {
                ++i;
            }
        }
        lastDigest = digest;
    }
}
