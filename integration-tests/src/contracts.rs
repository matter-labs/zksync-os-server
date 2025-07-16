//! Test contracts that can be deployed and interacted with during a test's lifetime.
//! See `./test-contracts/README.md` for instructions on how to build the artifacts.

alloy::sol!(
    /// Simple contract that can emit events on demand.
    #[sol(rpc)]
    EventEmitter,
    "test-contracts/out/EventEmitter.sol/EventEmitter.json"
);
