/**
 * Barrel for the vendored interop-SDK subset the atomic-swap driver needs.
 *
 * These six modules are copied verbatim from the interop SDK
 * (local-prividium-3chains/sdk/src) and depend only on `ethers`. This barrel
 * re-exports exactly the symbols `atomic-swap-3chains.ts` imports, so the driver
 * is self-contained within the server repo with no cross-repo dependency.
 */

// Bundle building
export { BundleBuilder, CallBuilder } from './bundle-builder';

// Address / asset helpers
export { computeAssetId } from './address';

// Constants
export { L2_NATIVE_TOKEN_VAULT_ADDRESS } from './constants';

// ABIs
export { NativeTokenVaultAbi, ERC20Abi } from './abis';

// Atomic interop (IMT bundle model)
export {
  resolveAtomicLayout,
  detectAtomicCapability,
  AtomicInteropCenterAbi,
  AtomicInteropHandlerAbi,
  AtomicFlowManagerAbi,
  INTEROP_BUNDLE_TUPLE,
  commitmentTreeContract,
  atomicBundleAttr,
  computeFlowId,
  commitValue,
  sortLegs,
  atomicFlowTuple,
  atomicFinalityProofTuple,
  LegState,
  type ImtProof,
  type AtomicFlow,
} from './atomic';
