pragma solidity ^0.8.0;

contract Inner {
    uint256 public value;

    constructor(uint256 _value) {
        value = _value;
    }
}

contract NestedCreateFactory {
    address public child;

    constructor(uint256 _value) {
        Inner inner = new Inner(_value);
        child = address(inner);
    }
}
