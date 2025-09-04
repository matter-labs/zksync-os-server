# Updates

## Verification keys

If you did any change to zkos binary (for example including a binary from the new version of zkos), you should do following steps:

* commit it here & an inside zksync-airbender-prover (you'll be committing multiblock_batch.bin binary).
* generate verification keys and update era-contracts
    * you can use the tool from https://github.com/mm-zk/zksync_tools/tree/main/zkos/generate_vk
    * you need to find the latest era-contracts tag that we used (probably on top of [zksync-os-stable branch](https://github.com/matter-labs/era-contracts/tree/zksync-os-stable))
    * once the script generate the change, commit it into era-contracts repo.

Then follow instructions below for era-contracts updates.

## Updating era contracts 

If you do any change to era-contracts, we should update zkos-l1-state.json (especially if this is a breaking change -- be careful with those when we're in production).

* commit your change to era-contracts, and generate a new release/tag (we name them as zkos-v0.29.3 for example)
* go to zksync-era, checkout zksync-os-integration, and update the contracts dependency there (this step will hopefully disappear soon)
* then you can run the tool from: https://github.com/mm-zk/zksync_tools/tree/main/zkos/update_state_json
  * this tool will generate state.json (and genesis.json if needed), and if you run it with COMMIT_CHANGES=true, it will also create a branch in zksync-os-server.
* check that the server is still working (start anvil with new state, and run a clear server).
* commit your change to zksync-os-server and optionally create a new release.

WARNING: instructions above assume that you didn't change genesis hash (any change to L2Upgrade Handler, ComplexUpgrader or WrappedBasedToken might change it).
If you did, then you have to regenerate hashes, which is a longer process.
  
