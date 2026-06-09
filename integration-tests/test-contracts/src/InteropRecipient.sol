// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// Minimal IERC7786Recipient implementation used by the interop-load harness.
///
/// The InteropHandler on the destination chain dispatches incoming bundle
/// calls via `IERC7786Recipient.receiveMessage` and checks that the returned
/// bytes4 equals the function selector. Any contract that wants to receive
/// arbitrary cross-chain calls or base-token transfers needs to implement
/// this signature.
contract InteropRecipient {
    uint256 public counter;
    event Received(bytes32 indexed receiveId, address indexed forwardedTo, uint256 value);

    function receiveMessage(
        bytes32 receiveId,
        bytes calldata /* sender */,
        bytes calldata payload
    ) external payable returns (bytes4) {
        unchecked {
            counter += 1;
        }

        // If the payload encodes a 20-byte recipient address and the call
        // carries value, forward that value to the recipient. This makes the
        // same contract work as both the "message" target (no value, no
        // payload-address) and the "base-token forwarder" target (value > 0,
        // payload = bytes20(recipient)).
        address forwardedTo = address(0);
        if (msg.value > 0 && payload.length >= 20) {
            forwardedTo = address(bytes20(payload[0:20]));
            (bool ok, ) = forwardedTo.call{value: msg.value}("");
            require(ok, "interop-recipient: forward failed");
        }

        emit Received(receiveId, forwardedTo, msg.value);
        return this.receiveMessage.selector;
    }
}
