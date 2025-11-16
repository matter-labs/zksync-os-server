# Factory dependencies hardcode

Until the server supports fetching bytecodes from `BytecodeSupplier`, we hardcode the factory deps inside the code. Note, that this assumes that only one upgrade is supported at the moment, i.e. the same factory deps would be used for any upgrade.

To update the contracts, go to the `zksync-os-stable` branch of `era-contracts` and do the following:

```
# in era-contracts/l1-contracts
yarn write-factory-deps-zksync-os --output <path to the contracts.json>
```

The fact that the hashes in this code correspond to the actual hashes used in the upgrade is to be checked by the person that prepares the upgrade (e.g. via [protocol upgrade verification tool](https://github.com/matter-labs/protocol-upgrade-verification-tool)).
