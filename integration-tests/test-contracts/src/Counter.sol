pragma solidity ^0.8.13;

/// Sample contract with storage state.
contract Counter {
    uint256 counter;

    function increment(uint256 _by) public {
        counter += _by;
        // Force at least one event in the block with the increment tx to check block hash computations
        // in `zks_getProof`
        emit Incremented(_by, counter);
    }
}

event Incremented(uint256 indexed by, uint256 indexed newValue);
