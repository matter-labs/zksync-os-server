pragma solidity ^0.8.0;

import "./TracingSecondary.sol";

contract TracingPrimary {
    TracingSecondary secondary;

    constructor(address _secondary) {
        secondary = TracingSecondary(_secondary);
    }

    function name() public pure returns (string memory) {
        return "Primary";
    }

    function calculate(uint256 value) public returns (uint) {
        return secondary.multiply(value);
    }

    function shouldRevert() public view returns (uint) {
        return secondary.shouldRevert();
    }

    function multiCalculate(uint256 value, uint256 times) public returns (uint) {
        for (uint256 i = 0; i < times; i++) {
            value = secondary.multiply(value);
        }
        return value;
    }
}
