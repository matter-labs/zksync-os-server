// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// @title TestERC20 - Simple ERC20 with permissionless mint
contract TestERC20 is ERC20 {
    constructor() ERC20("Test ERC20 token", "TEST_TOKEN") {}

    /// @notice Mint new tokens to `to`
    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }
}
