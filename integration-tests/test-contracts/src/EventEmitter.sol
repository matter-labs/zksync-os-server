pragma solidity ^0.8.13;

contract EventEmitter {
    event TestEvent(
        uint256 number
    );

    function emitEvent(uint256 number) public {
        emit TestEvent(number);
    }
}
