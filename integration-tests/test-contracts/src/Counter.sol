pragma solidity ^0.8.13;

/// Sample contract with storage state.
contract Counter {
    uint256 counter;

    function increment(uint256 _by) public {
        counter += _by;
    }
}
