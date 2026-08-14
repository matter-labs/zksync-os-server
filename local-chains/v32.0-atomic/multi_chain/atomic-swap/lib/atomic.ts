/**
 * Atomic interop (IMT bundle model) — SDK surface.
 *
 * This module adds the client-side pieces needed to drive an **atomic** cross-chain
 * flow (the all-or-nothing IMT bundle model) on top of the existing interop SDK,
 * without touching the public/private (non-atomic) paths.
 *
 * Atomicity is enforced per-leg by each chain's on-chain Indexed Merkle Tree
 * (`L2InteropCommitmentTree`), coordinated by `AtomicFlowManager`. A send commits a
 * value to the source chain's IMT (instead of publishing to L1); an execute requires
 * an inclusion proof for *every* leg of the flow before any leg runs.
 *
 * Protocol layout: the atomic built-ins (`L2InteropCommitmentTree` at `0x10012`,
 * `AtomicFlowManager` at `0x10014`) and the atomic address layout
 * (`InteropCenter 0x1000d`, `InteropHandler 0x1000e`) come from the era-contracts
 * `atomic-imt-interop` branch and are predeployed by this preset's genesis. The
 * driver (`atomic-swap-3chains.ts`) still detects capability at runtime and fails
 * with a precise message rather than obscurely.
 *
 * No off-chain IMT engine is vendored here: per-leg proofs come COMPLETE from the
 * server RPCs (`zks_getImtInclusionProof` / `zks_getImtNonInclusionProof`, plus
 * `zks_getImtLowNullifierIndex` for sends) — see ../../ATOMIC_SWAP.md.
 */

import { ethers } from 'ethers';
import { InteropCenterAbi } from './abis';
import {
  L2_INTEROP_CENTER_ADDRESS,
  L2_INTEROP_HANDLER_ADDRESS,
  L2_INTEROP_COMMITMENT_TREE_ADDRESS,
  L2_ATOMIC_FLOW_MANAGER_ADDRESS,
} from './constants';

// ─────────────────────────────────────────────────────────────────────────────
// Address layout (single table in constants.ts). Overridable via env so the
// driver can adapt if a published atomic image uses a different layout.
// ─────────────────────────────────────────────────────────────────────────────

/** Atomic protocol address layout. Defaults match the `atomic-imt-interop` branch. */
export interface AtomicLayout {
  interopCenter: string;
  interopHandler: string;
  commitmentTree: string;
  atomicFlowManager: string;
}

/** Canonical atomic layout from era-contracts `atomic-imt-interop`. */
export const DEFAULT_ATOMIC_LAYOUT: AtomicLayout = {
  interopCenter: L2_INTEROP_CENTER_ADDRESS,
  interopHandler: L2_INTEROP_HANDLER_ADDRESS,
  commitmentTree: L2_INTEROP_COMMITMENT_TREE_ADDRESS,
  atomicFlowManager: L2_ATOMIC_FLOW_MANAGER_ADDRESS,
};

/** Resolve the atomic layout, allowing per-field env overrides. */
export function resolveAtomicLayout(env: NodeJS.ProcessEnv = process.env): AtomicLayout {
  return {
    interopCenter: env.ATOMIC_INTEROP_CENTER ?? DEFAULT_ATOMIC_LAYOUT.interopCenter,
    interopHandler: env.ATOMIC_INTEROP_HANDLER ?? DEFAULT_ATOMIC_LAYOUT.interopHandler,
    commitmentTree: env.ATOMIC_COMMITMENT_TREE ?? DEFAULT_ATOMIC_LAYOUT.commitmentTree,
    atomicFlowManager: env.ATOMIC_FLOW_MANAGER ?? DEFAULT_ATOMIC_LAYOUT.atomicFlowManager,
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// ABIs (atomic surface). Imported from here, never declared inline elsewhere.
// ─────────────────────────────────────────────────────────────────────────────

// Atomic sends use the plain `InteropCenter.sendBundle` (the `atomicBundle` attribute
// is just one more `_bundleAttributes` entry), so reuse the base ABI + the fee getter.
export const AtomicInteropCenterAbi = [
  ...InteropCenterAbi,
  'function interopProtocolFee() view returns (uint256)',
];

/** ABI tuple type for the on-wire `InteropBundle` (matches Messaging.sol). */
export const INTEROP_BUNDLE_TUPLE =
  'tuple(bytes1 version, uint256 sourceChainId, uint256 destinationChainId, bytes32 destinationBaseTokenAssetId, bytes32 interopBundleSalt, tuple(bytes1 version, bool shadowAccount, address to, address from, uint256 value, bytes data)[] calls, tuple(bytes executionAddress, bytes unbundlerAddress, bool useFixedFee, bytes32 salt) bundleAttributes)';

const IMT_PROOF_TUPLE =
  'tuple(uint256 sourceChainId, uint256 batchNumber, bytes32 chainImtRoot, bool provesAgainstBeginRoot, bytes32[] settlementProof, tuple(uint256 value, uint256 nextIndex, uint256 nextValue) leaf, uint256 imtLeafIndex, bytes32[] imtProof)';

const ATOMIC_FLOW_TUPLE =
  'tuple(bytes32 flowId, uint64 deadline, uint256 settlementLayerChainId, bytes32[] legBundleHashes, uint256[] legSourceChainIds)';

const ATOMIC_FINALITY_TUPLE = `tuple(${ATOMIC_FLOW_TUPLE} flow, ${IMT_PROOF_TUPLE}[] proofs)`;

export const AtomicInteropHandlerAbi = [
  `function executeAtomicBundle(bytes memory _bundle, ${ATOMIC_FINALITY_TUPLE} _finality) external`,
  'function bundleStatus(bytes32 bundleHash) view returns (uint8)',
  'event BundleExecuted(bytes32 indexed bundleHash)',
];

export const AtomicFlowManagerAbi = [
  'function legState(bytes32 _flowId, bytes32 _bundleHash) view returns (uint8)',
  'function commitmentTree() view returns (address)',
  'function interopCenter() view returns (address)',
  'function interopHandler() view returns (address)',
  `function authorizeRefund(${ATOMIC_FLOW_TUPLE} _flow, uint256 _missingLegIndex, ${IMT_PROOF_TUPLE} _absence) external`,
  'function claimRefund(bytes32 _flowId, bytes calldata _bundle) external',
  'event FlowCommitted(bytes32 indexed flowId, bytes32 indexed bundleHash, uint64 deadline, uint256 leafIndex)',
  'event FlowRefundAuthorized(bytes32 indexed flowId, bytes32 indexed bundleHash)',
  'event FlowRefunded(bytes32 indexed flowId, bytes32 indexed bundleHash)',
];

export const L2InteropCommitmentTreeAbi = [
  'function root() view returns (bytes32)',
  'function leafCount() view returns (uint256)',
  'function leafAt(uint256 _index) view returns (tuple(uint256 value, uint256 nextIndex, uint256 nextValue))',
  'function merklePath(uint256 _index) view returns (bytes32[])',
  'function appender() view returns (address)',
];

/** Source-leg state (mirrors `LegState` in IAtomicInterop.sol). */
export enum LegState {
  Unset = 0,
  Committed = 1,
  Revertable = 2,
  Reverted = 3,
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure helpers — no server / no provider needed.
// ─────────────────────────────────────────────────────────────────────────────

/** Domain tag for commit values: bytes4(keccak256("AtomicInterop.commit.v1")). */
export const ATOMIC_COMMIT_LEAF_TAG: string = ethers
  .keccak256(ethers.toUtf8Bytes('AtomicInterop.commit.v1'))
  .slice(0, 10);

/** ERC-7786 `atomicBundle(bytes32,uint64,uint256)` attribute selector. */
export const ATOMIC_BUNDLE_SELECTOR: string = ethers
  .id('atomicBundle(bytes32,uint64,uint256)')
  .slice(0, 10);

/** Indexed-tree leaf, fields as uint256 (string) in on-chain field order. */
export interface IMTLeaf {
  value: string;
  nextIndex: string;
  nextValue: string;
}

/**
 * Mirror of `ImtProof` in IAtomicInterop.sol (inclusion + non-inclusion). Served COMPLETE by the
 * server: `zks_getImtInclusionProof` returns the IMT half anchored at the batch-END root (leaf 3
 * of the chain batch root) plus the `settlementProof` authenticating that root against the
 * imported interop root; `zks_getImtNonInclusionProof` does the same for the batch-BEGIN root
 * (leaf 2) with a low-nullifier leaf. The client only adds `sourceChainId`.
 */
export interface ImtProof {
  sourceChainId: string;
  batchNumber: string;
  chainImtRoot: string;
  // Timeout-branch selector (begin vs end IMT root); the finality path this driver
  // exercises always proves against the batch-end root, so this is false.
  provesAgainstBeginRoot: boolean;
  settlementProof: string[];
  leaf: IMTLeaf;
  imtLeafIndex: number;
  imtProof: string[];
}

/** The flow definition (mirror of `AtomicFlow` in IAtomicInterop.sol). */
export interface AtomicFlow {
  flowId: string;
  deadline: number;
  settlementLayerChainId: bigint | number | string;
  legBundleHashes: string[];
  legSourceChainIds: (bigint | number | string)[];
}

/** Full atomicity proof for `executeAtomicBundle`: the flow + one inclusion proof per leg. */
export interface AtomicFinalityProof extends AtomicFlow {
  proofs: ImtProof[];
}

/**
 * Encode the ERC-7786 `atomicBundle(flowId, deadline, lowNullifierIndex)` attribute:
 * `selector(4) || abi.encode(bytes32, uint64, uint256)`.
 */
export function atomicBundleAttr(flowId: string, deadline: number, lowNullifierIndex: bigint | number): string {
  return ethers.concat([
    ATOMIC_BUNDLE_SELECTOR,
    ethers.AbiCoder.defaultAbiCoder().encode(
      ['bytes32', 'uint64', 'uint256'],
      [flowId, deadline, lowNullifierIndex]
    ),
  ]);
}

/** The value inserted into a chain's IMT for a leg (a bytes32, also a valid uint256). */
export function commitValue(flowId: string, bundleHash: string): string {
  return ethers.keccak256(
    ethers.AbiCoder.defaultAbiCoder().encode(
      ['bytes4', 'bytes32', 'bytes32'],
      [ATOMIC_COMMIT_LEAF_TAG, flowId, bundleHash]
    )
  );
}

/**
 * flowId = keccak256(abi.encode(legBundleHashes, legSourceChainIds, deadline, settlementLayerChainId)).
 * `legBundleHashes` MUST be strictly ascending; `legSourceChainIds` is POSITIONAL (aligned 1:1
 * with the hashes, may repeat, need not be sorted). `deadline` is a settlement-layer timestamp.
 */
export function computeFlowId(
  bundleHashes: string[],
  chainIds: (bigint | number | string)[],
  deadline: number,
  settlementLayerChainId: bigint | number | string
): string {
  return ethers.keccak256(
    ethers.AbiCoder.defaultAbiCoder().encode(
      ['bytes32[]', 'uint256[]', 'uint64', 'uint256'],
      [bundleHashes, chainIds.map((c) => BigInt(c)), deadline, BigInt(settlementLayerChainId)]
    )
  );
}

/**
 * Sort two legs into the strictly-ascending (legBundleHashes, chainIds) the protocol
 * expects, returning both the sorted arrays and the permutation so callers can order
 * their per-leg proofs to match.
 */
export function sortLegs(
  legs: { bundleHash: string; chainId: bigint | number }[]
): { legBundleHashes: string[]; chainIds: bigint[]; order: number[] } {
  const order = legs.map((_, i) => i).sort((a, b) => (BigInt(legs[a].bundleHash) < BigInt(legs[b].bundleHash) ? -1 : 1));
  return {
    legBundleHashes: order.map((i) => legs[i].bundleHash),
    // POSITIONAL: chainIds[i] is the source chain of legBundleHashes[i]. Sorting them
    // independently would misalign the pairs, which the on-chain source-chain binding rejects.
    chainIds: order.map((i) => BigInt(legs[i].chainId)),
    order,
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// Contract handle. Proofs come COMPLETE from the server RPCs
// (`zks_getImtInclusionProof` / `zks_getImtNonInclusionProof` — IMT half +
// settlement half; `zks_getImtLowNullifierIndex` for sends), so no off-chain
// IMT engine is vendored here.
// ─────────────────────────────────────────────────────────────────────────────

/** Build an ethers contract handle for the commitment tree. */
export function commitmentTreeContract(
  address: string,
  runner: ethers.ContractRunner
): ethers.Contract {
  return new ethers.Contract(address, L2InteropCommitmentTreeAbi, runner);
}

// ─────────────────────────────────────────────────────────────────────────────
// Tuple encoders (ordered for ethers contract calls).
// ─────────────────────────────────────────────────────────────────────────────

export function leafTuple(l: IMTLeaf): unknown[] {
  return [l.value, l.nextIndex, l.nextValue];
}

export function proofTuple(p: ImtProof): unknown[] {
  return [
    p.sourceChainId,
    p.batchNumber,
    p.chainImtRoot,
    p.provesAgainstBeginRoot,
    p.settlementProof,
    leafTuple(p.leaf),
    p.imtLeafIndex,
    p.imtProof,
  ];
}

/** Build the `AtomicFlow` tuple (the flow definition). */
export function atomicFlowTuple(f: AtomicFlow): unknown[] {
  return [
    f.flowId,
    f.deadline,
    BigInt(f.settlementLayerChainId),
    f.legBundleHashes,
    f.legSourceChainIds.map((c) => BigInt(c)),
  ];
}

/** Build the `AtomicFinalityProof` tuple `executeAtomicBundle` consumes. */
export function atomicFinalityProofTuple(p: AtomicFinalityProof): unknown[] {
  return [atomicFlowTuple(p), p.proofs.map(proofTuple)];
}

// ─────────────────────────────────────────────────────────────────────────────
// Capability detection — lets a driver fail with a precise message before an
// atomic-capable image/genesis exists.
// ─────────────────────────────────────────────────────────────────────────────

export interface AtomicCapability {
  supported: boolean;
  /** human-readable reason when unsupported */
  reason?: string;
  layout: AtomicLayout;
}

/**
 * Detect whether the chain behind `provider` supports the atomic IMT path: the
 * commitment tree must be deployed and responsive (`root()` + `leafCount()`).
 * Returns a structured result instead of throwing so a driver can print a clear
 * "blocked: image does not support atomic interop" message.
 */
export async function detectAtomicCapability(
  provider: ethers.Provider,
  layout: AtomicLayout = resolveAtomicLayout()
): Promise<AtomicCapability> {
  const code = await provider.getCode(layout.commitmentTree);
  if (code === '0x' || code === '0x0') {
    return {
      supported: false,
      reason: `no L2InteropCommitmentTree deployed at ${layout.commitmentTree} (needs an atomic server image + genesis; see ATOMIC_SWAP.md)`,
      layout,
    };
  }
  const tree = commitmentTreeContract(layout.commitmentTree, provider);
  // A reverting probe means the contract there is not the tree (ABI mismatch).
  for (const probe of ['root', 'leafCount'] as const) {
    try {
      await tree[probe]();
    } catch (err) {
      return {
        supported: false,
        reason: `${probe}() probe failed on contract at ${layout.commitmentTree} — not an L2InteropCommitmentTree: ${(err as Error).message}`,
        layout,
      };
    }
  }
  return { supported: true, layout };
}
