pragma solidity ^0.8.28;

contract SimpleRevert {

    error SimpleError();

    function simpleRevert() external pure returns (uint256) {
        if (true) revert SimpleError();
        return 1;
    }

    function stringRevert() external pure returns (uint256) {
        revert("my message");
    }
}
