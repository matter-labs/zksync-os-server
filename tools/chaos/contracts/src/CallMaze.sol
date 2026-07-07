// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// A seeded walk through the call-frame zoo, driven by the `call_maze` workload:
/// nested CALL / DELEGATECALL / STATICCALL, CREATE and CREATE2 at the leaves,
/// and reverts bubbling up from below — some caught mid-maze, some propagating
/// until the top-level catch. Every frame writes storage and emits the step it
/// took, so the transaction's receipt (logs, gas, status) is a dense fingerprint
/// of call-frame semantics for the watcher to compare across validators.
///
/// `walk` itself always succeeds: the dice decide what happens *inside* the
/// maze, and the top frame catches whatever escapes — callers never have to
/// predict the on-chain roll to know a healthy receipt is status 1.
contract CallMaze {
    event Step(uint8 depth, uint8 op, uint256 note);
    event Survived(uint8 depth, bytes reason);

    /// Rolling hash of recent walks. DELEGATECALL frames write here through the
    /// caller's storage — that aliasing is part of the point.
    mapping(uint256 => bytes32) public trail;
    uint256 public walks;

    function walk(uint256 seed, uint8 depth) external payable returns (bytes32) {
        walks += 1;
        bytes32 mark;
        try this.step{value: msg.value}(seed, depth) returns (bytes32 got) {
            mark = got;
        } catch (bytes memory reason) {
            // A planned dead end propagated all the way up: the maze below this
            // frame executed and then unwound, which is exactly the coverage.
            emit Survived(type(uint8).max, reason);
            mark = keccak256(reason);
        }
        trail[seed % 32] = mark;
        return mark;
    }

    /// One maze frame. External because CALL/DELEGATECALL/STATICCALL re-enter
    /// through the ABI; guarded by nothing — rig money, rig chain.
    function step(uint256 seed, uint8 depth) external payable returns (bytes32) {
        uint8 op = uint8(seed % 8);
        emit Step(depth, op, gasleft());
        bytes32 below = "";

        if (depth == 0) {
            // Leaf: deploy something half the time, plain derivation the rest.
            if (op % 2 == 0) {
                below = _deployLeaf(seed, op % 4 == 0);
            }
        } else {
            uint256 nextSeed = uint256(keccak256(abi.encode(seed, depth)));
            if (op == 0 || op == 1) {
                // Plain nested CALL, forwarding a wei when it has one.
                below = this.step{value: address(this).balance > 0 ? 1 : 0}(nextSeed, depth - 1);
            } else if (op == 2) {
                // DELEGATECALL into our own step: same code, this storage. A
                // dead end below surfaces as ok=false and propagates from here.
                (bool ok, bytes memory ret) =
                    address(this).delegatecall(abi.encodeCall(this.step, (nextSeed, depth - 1)));
                require(ok, "maze: delegate leg died");
                below = abi.decode(ret, (bytes32));
            } else if (op == 3) {
                // STATICCALL leg: a read-only frame mid-walk.
                below = this.peek(nextSeed);
            } else if (op == 4) {
                // A guaranteed revert below, caught here: whatever state the
                // callee frame made must be rolled back while ours survives.
                try this.bomb(nextSeed) {
                    below = "unreachable";
                } catch (bytes memory reason) {
                    emit Survived(depth, reason);
                    below = keccak256(reason);
                }
            } else if (op == 5 && seed & 16 == 16) {
                // A revert that propagates: frames above unwind until someone
                // catches it (a delegate leg, or the top-level walk).
                revert("maze: planned dead end");
            } else {
                below = _stepInline(nextSeed, depth - 1);
            }
        }

        bytes32 mark = keccak256(abi.encode(seed, depth, op, below));
        trail[seed % 32] = mark;
        return mark;
    }

    /// A STATICCALL-safe frame: no storage writes, no events, just derivation.
    function peek(uint256 seed) external pure returns (bytes32) {
        return keccak256(abi.encode(seed, "peek"));
    }

    /// Always reverts; the maze's caught-revert legs call this.
    function bomb(uint256 seed) external pure {
        revert(string(abi.encodePacked("maze: bomb ", seed % 10 == 0 ? "big" : "small")));
    }

    /// Same walk logic without a fresh call frame — keeps some legs cheap so
    /// depth is spent on the interesting external ones.
    function _stepInline(uint256 seed, uint8 depth) internal returns (bytes32) {
        bytes32 below = "";
        if (depth > 0) {
            below = _stepInline(uint256(keccak256(abi.encode(seed, depth))), depth - 1);
        }
        bytes32 mark = keccak256(abi.encode(seed, depth, uint8(7), below));
        trail[seed % 32] = mark;
        return mark;
    }

    function _deployLeaf(uint256 seed, bool useCreate2) internal returns (bytes32) {
        MazeLeaf leaf;
        if (useCreate2) {
            // Salt includes our walk count so repeated walks never collide on
            // an already-deployed CREATE2 address.
            leaf = new MazeLeaf{salt: keccak256(abi.encode(seed, walks))}();
        } else {
            leaf = new MazeLeaf();
        }
        return leaf.tag(seed);
    }
}

/// The maze's disposable leaf: exists so CREATE/CREATE2 and calls into fresh
/// code are part of the walk.
contract MazeLeaf {
    uint256 public stamp;

    function tag(uint256 seed) external returns (bytes32) {
        stamp = seed;
        return keccak256(abi.encode(address(this), seed));
    }
}
