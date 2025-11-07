# Genesis and Block 0

Genesis is the one-time process of starting a new chain and producing its first block.

## What happens at genesis

1. The sequencer starts with an empty database.
2. It loads a hardcoded file (genesis.json) that defines the initial on-chain state.
3. This file deploys exactly three system contracts:
    - GenesisUpgrade
    - L2WrappedBase
    - ForceDeployer
4. After loading these, the sequencer begins listening to L1 events that describe the first actual L2 block:
    - Bytecodes to deploy additional contracts (ForceDeployer enables this)
    - Parameters passed to GenesisUpgrade to finish remaining initialization steps

## Why these contracts exist

- ForceDeployer: Allows forced deployment of predefined contract bytecode needed at boot.
- GenesisUpgrade: Finalizes system configuration after initial contract deployment.
- L2WrappedBase: Provides a required base implementation (infrastructure dependency).

## Security perspective

L1 already knows the exact expected initial L2 state (the three contracts and their storage layout).  
This state is defined in zksync-era's genesis.json and is consumed by the zkStack tooling when setting up the ecosystem on L1.  
Because L1 has this canonical reference, it can validate that the L2 started correctly.

## Summary

Genesis = load predefined state from genesis.json -> deploy 3 core contracts -> process L1 events to finalize initialization -> produce block 0.
