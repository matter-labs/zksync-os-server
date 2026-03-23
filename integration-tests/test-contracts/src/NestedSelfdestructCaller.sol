pragma solidity ^0.8.0;

contract SelfdestructTarget {
    function destroy(address payable beneficiary) external {
        selfdestruct(beneficiary);
    }
}

contract NestedSelfdestructCaller {
    SelfdestructTarget public target;

    constructor() {
        target = new SelfdestructTarget();
    }

    function trigger(address payable beneficiary) external {
        target.destroy(beneficiary);
    }
}
