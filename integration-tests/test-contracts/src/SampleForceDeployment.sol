pragma solidity ^0.8.13;

/// This is an example contract that can be used as a force deployed
/// contract during protocol upgrades.
/// It's important that the preimage for this contract's bytecode is
/// not available by default, so that server has to fetch it from L1.
contract SampleForceDeployment {
    function return42() public pure returns (uint256) {
        return 42;
    }
}
