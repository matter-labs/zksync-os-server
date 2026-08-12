# Driving an atomic swap on the atomic multi_chain preset

End-to-end **all-or-nothing cross-chain atomic swap** between the two L1-settling
chains in this preset (A = 6565 @ :3050, B = 6566 @ :3051): A sends token X to a
user on B and B sends token Y to the same user on A; both legs execute or neither.

This was validated green: both `executeAtomicBundle` calls reach `FullyExecuted`
(`BundleStatus = 2`) and both wrapped tokens mint (100 each).

## 1. Bring up the preset

```bash
./run_local.sh ./local-chains/v32.0-atomic/multi_chain
# wait until :3050 and :3051 answer eth_chainId (0x19a5 / 0x19a6)
```

## 2. Register the two chains for interop (once per anvil session)

Before `InteropCenter` on chain X accepts `sendBundle(destChainId = Y)`, X must learn
Y's base-token asset id. This is a permissionless L1 call,
`Bridgehub.chainRegistrationSender().registerChain(Y, X)`, which triggers the L2
service txs that populate `baseTokenAssetId(Y)` on chain X. Do both directions:

```bash
BH=$(cast call <bridgehub-from-chain_6565.yaml> 'chainRegistrationSender()(address)' --rpc-url http://127.0.0.1:8545)
K0=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80   # anvil #0
cast send --private-key $K0 $BH 'registerChain(uint256,uint256)' 6566 6565 --rpc-url http://127.0.0.1:8545
cast send --private-key $K0 $BH 'registerChain(uint256,uint256)' 6565 6566 --rpc-url http://127.0.0.1:8545
# poll until baseTokenAssetId(6566) on :3050 and baseTokenAssetId(6565) on :3051 are non-zero
```

(`baseTokenAssetId` is on the L2 Bridgehub `0x...10002`.)

## 3. Run the atomic-swap driver

The driver (TypeScript, ethers v6) sends both legs, fetches the real per-leg proofs
from this server's RPCs, waits for interop-root import, then executes. It ships
self-contained in this preset under [`atomic-swap/`](./atomic-swap) (the driver plus
the minimal vendored interop helpers it needs — only `ethers` at runtime). Point it
at the preset's RPCs:

```bash
cd local-chains/v32.0-atomic/multi_chain/atomic-swap
npm install
PRIVATE_KEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110 \
L2_RPC_URL=http://127.0.0.1:3050 \
L2_RPC_URL_SECOND=http://127.0.0.1:3051 \
L1_RPC_URL=http://127.0.0.1:8545 \
  npm run atomic-swap
```

## What the driver does (mirrors the `atomic_swap_l1_settled` integration test)

1. Deploy + register (NTV) + approve a TestnetERC20 on each chain.
2. Predict both leg bundle hashes via `InteropCenter.sendBundle` callStatic
   (bundleHash is independent of the atomic params), with
   `value = interopProtocolFee * callCount` (0 here).
3. `flowId = keccak256(abi.encode(sortedLegHashes, sortedChainIds, deadline))`.
4. For each leg: `commitValue = keccak256(ATOMIC_COMMIT_LEAF_TAG, flowId, bundleHash)`,
   get the IMT low-nullifier via `zks_getImtLowNullifierIndex`, then
   `sendBundle(..., [atomicBundle(flowId, deadline, lowNullifier)])`. This burns and
   inserts the commit value into the source chain's `L2InteropCommitmentTree`
   (`legState → Committed`); the bundle is NOT published to L1.
5. Per leg, fetch the real proof: find the commitment-tree (`0x10012`) publish's
   index in the send receipt's `l2ToL1Logs`, poll
   `zks_getL2ToL1LogProof(txHash, idx, "messageRoot")` (→ batch, leaf id, merkle
   path, settlement-layer block), and `zks_getImtInclusionProof(commitValue, sendBlock)`
   (→ IMT root, leaf, leaf index, merkle path). Assemble the per-leg `ImtProof`.
6. Wait for each chain's `L2InteropRootStorage.interopRoots(l1ChainId, slBlock)` to
   import the interop root at each leg's settlement block.
7. `InteropHandler.executeAtomicBundle(bundle, AtomicFinalityProof)` on each
   destination. `AtomicFlowManager.requireFlowFinalized` verifies an inclusion proof
   for **every** leg before any executes → both reach `FullyExecuted`, both mints land.

`deadline` is a settlement-layer block number set well above the L1 head (default
`10_000_000`); the proof's `gatewayBlockNumber` is the actual settlement block.

## Server RPCs this relies on (kl/l1-settled-interop-proof)

- `zks_getImtLowNullifierIndex(value, block)`
- `zks_getImtInclusionProof(commitValue, block)`
- `zks_getL2ToL1LogProof(txHash, index, "messageRoot")`
