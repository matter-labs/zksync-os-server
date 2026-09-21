// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {TestERC20} from "./TestERC20.sol";

/// A repeatable, pre-agreed delivery-versus-payment trade for integration tests.
/// Approvals bound the amount that can settle; each call exchanges one fixed lot.
/// Only intended for the test tokens, which have no callbacks or transfer fees.
contract TestDvp {
    TestERC20 public immutable asset;
    TestERC20 public immutable cash;
    address public immutable seller;
    address public immutable buyer;
    uint256 public immutable assetAmount;
    uint256 public immutable cashAmount;

    event Settled(address indexed buyer, address indexed seller, uint256 assetAmount, uint256 cashAmount);

    constructor(
        TestERC20 _asset,
        TestERC20 _cash,
        address _seller,
        address _buyer,
        uint256 _assetAmount,
        uint256 _cashAmount
    ) {
        require(address(_asset) != address(0) && address(_cash) != address(0), "zero token");
        require(_asset != _cash, "same token");
        require(_seller != address(0) && _buyer != address(0) && _seller != _buyer, "invalid parties");
        require(_assetAmount > 0 && _cashAmount > 0, "zero amount");
        asset = _asset;
        cash = _cash;
        seller = _seller;
        buyer = _buyer;
        assetAmount = _assetAmount;
        cashAmount = _cashAmount;
    }

    function settle() external {
        require(msg.sender == buyer, "only buyer");
        require(asset.transferFrom(seller, buyer, assetAmount), "delivery failed");
        // Failure here must roll back delivery, including its allowance change and log.
        require(cash.transferFrom(buyer, seller, cashAmount), "payment failed");
        emit Settled(buyer, seller, assetAmount, cashAmount);
    }
}
