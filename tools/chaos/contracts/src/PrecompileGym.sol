// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// Precompile exercises with the expected answer baked in, driven by the
/// `precompiles` workload. Each exercise calls one precompile with a known
/// vector and reverts on any deviation — a wrong result becomes a failed
/// transaction, visible in rig stats and in cross-validator receipt comparison
/// with no off-chain oracle. The rig calibrates at startup (eth_call per
/// exercise) and drops the ones this chain's VM does not support.
contract PrecompileGym {
    event Exercised(uint8 indexed exercise);

    uint8 public constant EXERCISES = 8;

    /// Runs one exercise; ids match `EXERCISES` order below.
    function exercise(uint8 id) public {
        if (id == 0) _ecrecover();
        else if (id == 1) _sha256();
        else if (id == 2) _ripemd160();
        else if (id == 3) _identity();
        else if (id == 4) _modexp();
        else if (id == 5) _ecadd();
        else if (id == 6) _ecmul();
        else if (id == 7) _ecpairing();
        else revert("gym: unknown exercise");
        emit Exercised(id);
    }

    /// Runs a seeded subset in one transaction (for the tick workload).
    function workout(uint256 seed, uint8 count) external {
        for (uint8 i = 0; i < count; i++) {
            exercise(uint8(uint256(keccak256(abi.encode(seed, i))) % EXERCISES));
        }
    }

    /// ecrecover of a fixed message/signature must yield the signer that made
    /// it (signed once offline with the throwaway key keccak256("chaos gym key")).
    function _ecrecover() internal view {
        bytes32 digest = keccak256("chaos gym message");
        address expected = 0x1E8f383eAB2B348ca45ee1E33eDc5Eb8015DD7b2;
        address got = ecrecover(
            digest,
            28,
            0x2fd994477dad3c5a05f17e7d0fc5b03cba0bb9e99bd30489aba9ebf931cff8bc,
            0x32861a07c45e216556373ef12c284f2100eb8712ff54ee4c004af4b7d1d80f9d
        );
        require(got == expected, "gym: ecrecover mismatch");
    }

    function _sha256() internal view {
        bytes32 got = sha256("abc");
        require(
            got == 0xba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad,
            "gym: sha256 mismatch"
        );
    }

    function _ripemd160() internal view {
        bytes20 got = ripemd160("abc");
        require(
            got == bytes20(hex"8eb208f7e05d987a9b044a8e98c6b087f15a0bfc"),
            "gym: ripemd160 mismatch"
        );
    }

    /// The identity precompile (0x04) must echo its input.
    function _identity() internal view {
        bytes memory input = abi.encode("chaos identity", address(this));
        (bool ok, bytes memory out) = address(0x04).staticcall(input);
        require(ok && keccak256(out) == keccak256(input), "gym: identity mismatch");
    }

    /// 3^7 mod 11 == 9 through modexp (0x05).
    function _modexp() internal view {
        bytes memory input = abi.encodePacked(
            uint256(1), uint256(1), uint256(1),
            uint8(3), uint8(7), uint8(11)
        );
        (bool ok, bytes memory out) = address(0x05).staticcall(input);
        require(ok && out.length == 1 && out[0] == 0x09, "gym: modexp mismatch");
    }

    /// G + G on alt_bn128 (0x06) must be 2G.
    function _ecadd() internal view {
        (bool ok, bytes memory out) = address(0x06).staticcall(
            abi.encode(uint256(1), uint256(2), uint256(1), uint256(2))
        );
        require(ok, "gym: ecadd failed");
        (uint256 x, uint256 y) = abi.decode(out, (uint256, uint256));
        require(
            x == 0x030644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd3
                && y == 0x15ed738c0e0a7c92e7845f96b2ae9c0a68a6a449e3538fc7ff3ebf7a5a18a2c4,
            "gym: ecadd mismatch"
        );
    }

    /// 2 * G on alt_bn128 (0x07) must equal G + G.
    function _ecmul() internal view {
        (bool ok, bytes memory out) = address(0x07).staticcall(
            abi.encode(uint256(1), uint256(2), uint256(2))
        );
        require(ok, "gym: ecmul failed");
        (uint256 x, uint256 y) = abi.decode(out, (uint256, uint256));
        require(
            x == 0x030644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd3
                && y == 0x15ed738c0e0a7c92e7845f96b2ae9c0a68a6a449e3538fc7ff3ebf7a5a18a2c4,
            "gym: ecmul mismatch"
        );
    }

    /// The empty pairing (0x08) is vacuously true and returns 1.
    function _ecpairing() internal view {
        (bool ok, bytes memory out) = address(0x08).staticcall("");
        require(ok && out.length == 32 && out[31] == 0x01, "gym: ecpairing mismatch");
    }
}
