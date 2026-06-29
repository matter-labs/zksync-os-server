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
 * IMPORTANT — protocol layout: the atomic built-ins (`L2InteropCommitmentTree` at
 * `0x10012`, `AtomicFlowManager` at `0x10014`) and the atomic address layout
 * (`InteropCenter 0x1000d`, `InteropHandler 0x1000e`) come from the era-contracts
 * `atomic-imt-interop` branch. They are NOT present in the stack's default
 * `test-only-interop-demo` server image — see ATOMIC-INTEROP-PLAN.md. This module is
 * shipped so the demo is ready the moment an atomic server image + genesis land; the
 * example (`examples/atomic-swap-3chains.ts`) detects capability at runtime and fails
 * with a precise message rather than obscurely.
 *
 * The engine here is a faithful ethers-v6 port of the era-contracts off-chain engine
 * `l1-contracts/test/anvil-interop/src/helpers/imt-engine-lib.ts` (IMT engine B,
 * fixed depth 32). It reconstructs the tree from the contract's live leaf set, so it
 * works even if the server does not expose the `zks_getImt*` RPCs, as long as the
 * commitment-tree contract is deployed.
 */

import { ethers } from 'ethers';

// ─────────────────────────────────────────────────────────────────────────────
// Address layout (atomic-imt-interop branch). Overridable via env so the example
// can adapt if a published atomic image uses a different layout.
// ─────────────────────────────────────────────────────────────────────────────

const BUILT_IN = 0x10000;
const builtIn = (offset: number): string =>
  ethers.getAddress(ethers.zeroPadValue(ethers.toBeHex(BUILT_IN + offset), 20));

/** Atomic protocol address layout. Defaults match the `atomic-imt-interop` branch. */
export interface AtomicLayout {
  interopCenter: string;
  interopHandler: string;
  commitmentTree: string;
  atomicFlowManager: string;
}

/** Canonical atomic layout from era-contracts `atomic-imt-interop`. */
export const DEFAULT_ATOMIC_LAYOUT: AtomicLayout = {
  interopCenter: builtIn(0x0d), // 0x...1000d
  interopHandler: builtIn(0x0e), // 0x...1000e
  commitmentTree: builtIn(0x12), // 0x...10012
  atomicFlowManager: builtIn(0x14), // 0x...10014
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

/**
 * `InteropCenter.sendBundle` is the same signature as the non-atomic path; the
 * `atomicBundle` attribute is just one more `_bundleAttributes` entry. We keep the
 * `InteropBundleSent` event so callers can recover the ABI-encoded `InteropBundle`
 * (needed verbatim by `executeAtomicBundle`).
 */
export const AtomicInteropCenterAbi = [
  'function sendBundle(bytes calldata _destinationChainId, tuple(bytes to, bytes data, bytes[] callAttributes)[] calldata _callStarters, bytes[] calldata _bundleAttributes) external payable returns (bytes32)',
  'function interopProtocolFee() view returns (uint256)',
  'event InteropBundleSent(bytes32 l2l1MsgHash, bytes32 interopBundleHash, tuple(bytes1 version, uint256 sourceChainId, uint256 destinationChainId, bytes32 destinationBaseTokenAssetId, bytes32 interopBundleSalt, tuple(bytes1 version, bool shadowAccount, address to, address from, uint256 value, bytes data)[] calls, tuple(bytes executionAddress, bytes unbundlerAddress, bool useFixedFee) bundleAttributes) interopBundle)',
];

/** ABI tuple type for the on-wire `InteropBundle` (matches Messaging.sol). */
export const INTEROP_BUNDLE_TUPLE =
  'tuple(bytes1 version, uint256 sourceChainId, uint256 destinationChainId, bytes32 destinationBaseTokenAssetId, bytes32 interopBundleSalt, tuple(bytes1 version, bool shadowAccount, address to, address from, uint256 value, bytes data)[] calls, tuple(bytes executionAddress, bytes unbundlerAddress, bool useFixedFee) bundleAttributes)';

const IMT_PROOF_TUPLE =
  'tuple(uint256 sourceChainId, uint256 batchNumber, bytes32 chainImtRoot, uint16 messageTxNumberInBatch, uint256 messageIndex, bytes32[] messageProof, tuple(uint256 value, uint256 nextIndex, uint256 nextValue) leaf, uint256 imtLeafIndex, bytes32[] imtProof)';

const ATOMIC_FINALITY_TUPLE = `tuple(bytes32 flowId, uint64 deadline, bytes32[] legBundleHashes, uint256[] chainIds, ${IMT_PROOF_TUPLE}[] proofs)`;

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
  `function authorizeRefund(bytes32 _flowId, bytes32[] calldata _legBundleHashes, uint256[] calldata _chainIds, uint64 _deadline, uint256 _missingLegIndex, ${IMT_PROOF_TUPLE} _proof) external`,
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

/** Fixed depth of the Indexed Merkle Tree — matches IMT_DEPTH in IndexedMerkleTree.sol. */
export const IMT_DEPTH = 32;

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

/** Mirror of `ImtProof` in IAtomicInterop.sol (inclusion + non-inclusion). */
export interface ImtProof {
  sourceChainId: string;
  batchNumber: string;
  chainImtRoot: string;
  messageTxNumberInBatch: number;
  messageIndex: string;
  messageProof: string[];
  leaf: IMTLeaf;
  imtLeafIndex: number;
  imtProof: string[];
}

/** Full atomicity proof for `executeAtomicBundle`. */
export interface AtomicFinalityProof {
  flowId: string;
  deadline: number;
  legBundleHashes: string[];
  chainIds: (bigint | number | string)[];
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

/** Leaf hash in canonical layout: keccak256(abi.encode(value, nextIndex, nextValue)). */
export function indexedLeafHash(leaf: IMTLeaf): string {
  return ethers.keccak256(
    ethers.AbiCoder.defaultAbiCoder().encode(
      ['uint256', 'uint256', 'uint256'],
      [leaf.value, leaf.nextIndex, leaf.nextValue]
    )
  );
}

/**
 * flowId = keccak256(abi.encode(sortedBundleHashes, sortedChainIds, deadline)).
 * Both arrays MUST already be strictly ascending.
 */
export function computeFlowId(
  bundleHashes: string[],
  chainIds: (bigint | number | string)[],
  deadline: number
): string {
  return ethers.keccak256(
    ethers.AbiCoder.defaultAbiCoder().encode(
      ['bytes32[]', 'uint256[]', 'uint64'],
      [bundleHashes, chainIds.map((c) => BigInt(c)), deadline]
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
    chainIds: order.map((i) => BigInt(legs[i].chainId)).sort((a, b) => (a < b ? -1 : a > b ? 1 : 0)),
    order,
  };
}

// ─────────────────────────────────────────────────────────────────────────────
// IMT engine B — fixed depth 32. Faithful port of imt-engine-lib.ts to ethers v6.
// ─────────────────────────────────────────────────────────────────────────────

/** efficientHash(a, b) = keccak256(a ++ b) over two 32-byte siblings. */
function efficientHash(left: string, right: string): string {
  return ethers.keccak256(ethers.concat([left, right]));
}

/** Precomputed zero-subtree hashes, length IMT_DEPTH + 1. */
export function computeZeros(): string[] {
  const zeros: string[] = new Array(IMT_DEPTH + 1);
  zeros[0] = indexedLeafHash({ value: '0', nextIndex: '0', nextValue: '0' });
  for (let i = 0; i < IMT_DEPTH; i++) {
    zeros[i + 1] = efficientHash(zeros[i], zeros[i]);
  }
  return zeros;
}

const ZEROS = computeZeros();

/** Sparse fixed-depth Indexed Merkle Tree reconstructed from the index-ordered leaf set. */
export class IndexedMerkleTree {
  readonly leaves: IMTLeaf[];
  private readonly nodes: Array<Map<number, string>>;

  constructor(leaves: IMTLeaf[]) {
    this.leaves = leaves;
    this.nodes = Array.from({ length: IMT_DEPTH + 1 }, () => new Map<number, string>());
    for (let i = 0; i < leaves.length; i++) {
      this.nodes[0].set(i, indexedLeafHash(leaves[i]));
    }
    for (let level = 0; level < IMT_DEPTH; level++) {
      const parents = new Set<number>();
      for (const childIndex of this.nodes[level].keys()) {
        parents.add(childIndex >> 1);
      }
      for (const parentIndex of parents) {
        const leftIndex = parentIndex * 2;
        const left = this.nodeAt(level, leftIndex);
        const right = this.nodeAt(level, leftIndex + 1);
        this.nodes[level + 1].set(parentIndex, efficientHash(left, right));
      }
    }
  }

  private nodeAt(level: number, index: number): string {
    return this.nodes[level].get(index) ?? ZEROS[level];
  }

  /** The current IMT root (level IMT_DEPTH, index 0). */
  root(): string {
    return this.nodeAt(IMT_DEPTH, 0);
  }

  /** Fixed-depth Merkle path (32 siblings, leaf level up) for the leaf at `index`. */
  merklePath(index: number): string[] {
    const path: string[] = new Array(IMT_DEPTH);
    let idx = index;
    for (let level = 0; level < IMT_DEPTH; level++) {
      const siblingIdx = idx % 2 === 0 ? idx + 1 : idx - 1;
      path[level] = this.nodeAt(level, siblingIdx);
      idx = Math.floor(idx / 2);
    }
    return path;
  }
}

/** Index of the low-nullifier leaf for `value`. */
export function findLowNullifierIndex(leaves: IMTLeaf[], value: string): number {
  const v = BigInt(value);
  for (let i = 0; i < leaves.length; i++) {
    const lv = BigInt(leaves[i].value);
    const nv = BigInt(leaves[i].nextValue);
    if (lv < v && (nv === 0n || v < nv)) return i;
  }
  throw new Error(`no low nullifier for value ${value} (already present or empty tree)`);
}

/** Index of the leaf holding `value`, or -1 if absent. */
export function findValueIndex(leaves: IMTLeaf[], value: string): number {
  const v = BigInt(value);
  return leaves.findIndex((l) => BigInt(l.value) === v);
}

/** Build an ethers contract handle for the commitment tree. */
export function commitmentTreeContract(
  address: string,
  runner: ethers.ContractRunner
): ethers.Contract {
  return new ethers.Contract(address, L2InteropCommitmentTreeAbi, runner);
}

/**
 * Reconstruct a chain's IMT from its live leaf set (`leafCount` + `leafAt`). The
 * reconstructed root is asserted against `tree.root()` by callers (the proof builders).
 */
export async function reconstructChainImt(
  tree: ethers.Contract,
  blockTag?: number
): Promise<{ leaves: IMTLeaf[]; engine: IndexedMerkleTree; root: string }> {
  const overrides = blockTag !== undefined ? { blockTag } : {};
  const count = Number(await tree.leafCount(overrides));
  const leaves: IMTLeaf[] = [];
  for (let i = 0; i < count; i++) {
    const l = await tree.leafAt(i, overrides);
    leaves.push({ value: l.value.toString(), nextIndex: l.nextIndex.toString(), nextValue: l.nextValue.toString() });
  }
  const engine = new IndexedMerkleTree(leaves);
  return { leaves, engine, root: engine.root() };
}

/** Convenience: low-nullifier index for inserting `value` into the current tree. */
export async function lowNullifierIndexFor(tree: ethers.Contract, value: string, blockTag?: number): Promise<number> {
  const imt = await reconstructChainImt(tree, blockTag);
  return findLowNullifierIndex(imt.leaves, value);
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof builders. The message-inclusion part authenticates the chain's IMT root
// AND carries the settlement-layer block number used for the deadline check.
// `buildSlProofBytes` mirrors the era-contracts harness; against a real atomic
// server, prefer the server-provided `messageProof` (see example).
// ─────────────────────────────────────────────────────────────────────────────

/** Default settlement-layer chain id encoded into harness proof bytes. */
export const DEFAULT_SL_CHAIN_ID = 506;

/**
 * Minimal format-valid multi-hop L2-message inclusion proof bytes parsed by the real
 * `MessageHashing._getProofData` to a chosen settlement-layer block `slBlock`
 * (finalProofNode == false). Mirrors `AtomicInteropTestUtils.slProofBytes`.
 */
export function buildSlProofBytes(slBlock: number, slChainId: number = DEFAULT_SL_CHAIN_ID): string[] {
  const metadata = ethers.zeroPadValue(ethers.toBeHex(1n << 248n), 32);
  const batchLeafProofMask = ethers.zeroPadValue('0x00', 32);
  const packedBatchInfo = ethers.zeroPadValue(ethers.toBeHex(BigInt(slBlock) << 128n), 32);
  const settlementLayerChainId = ethers.zeroPadValue(ethers.toBeHex(BigInt(slChainId)), 32);
  return [metadata, batchLeafProofMask, packedBatchInfo, settlementLayerChainId];
}

function messageProofForSlBlock(slBlock: number): {
  batchNumber: string;
  messageIndex: string;
  messageTxNumberInBatch: number;
  messageProof: string[];
} {
  return { batchNumber: '1', messageIndex: '0', messageTxNumberInBatch: 0, messageProof: buildSlProofBytes(slBlock) };
}

/** Build an inclusion `ImtProof` (leaf is the value's own leaf), carrying `slBlock` (<= deadline). */
export async function buildInclusionProof(params: {
  l2Tree: ethers.Contract;
  chainId: bigint | number | string;
  value: string;
  slBlock: number;
  l2BlockTag?: number;
}): Promise<ImtProof> {
  const { l2Tree, chainId, value, slBlock, l2BlockTag } = params;
  const imt = await reconstructChainImt(l2Tree, l2BlockTag);
  const idx = findValueIndex(imt.leaves, value);
  if (idx < 0) throw new Error(`value ${value} not found in chain ${chainId} IMT`);

  const onChainRoot: string = await l2Tree.root(l2BlockTag !== undefined ? { blockTag: l2BlockTag } : {});
  if (imt.root.toLowerCase() !== onChainRoot.toLowerCase()) {
    throw new Error(`off-chain IMT root ${imt.root} != on-chain root ${onChainRoot} for chain ${chainId}`);
  }

  return {
    sourceChainId: BigInt(chainId).toString(),
    chainImtRoot: imt.root,
    leaf: imt.leaves[idx],
    imtLeafIndex: idx,
    imtProof: imt.engine.merklePath(idx),
    ...messageProofForSlBlock(slBlock),
  };
}

/** Build a non-inclusion `ImtProof` (leaf is the low-nullifier), carrying `slBlock` (> deadline). */
export async function buildNonInclusionProof(params: {
  l2Tree: ethers.Contract;
  chainId: bigint | number | string;
  value: string;
  slBlock: number;
  l2BlockTag?: number;
}): Promise<ImtProof> {
  const { l2Tree, chainId, value, slBlock, l2BlockTag } = params;
  const imt = await reconstructChainImt(l2Tree, l2BlockTag);
  const lowIndex = findLowNullifierIndex(imt.leaves, value); // throws if value present

  const onChainRoot: string = await l2Tree.root(l2BlockTag !== undefined ? { blockTag: l2BlockTag } : {});
  if (imt.root.toLowerCase() !== onChainRoot.toLowerCase()) {
    throw new Error(`off-chain IMT root ${imt.root} != on-chain root ${onChainRoot} for chain ${chainId}`);
  }

  return {
    sourceChainId: BigInt(chainId).toString(),
    chainImtRoot: imt.root,
    leaf: imt.leaves[lowIndex],
    imtLeafIndex: lowIndex,
    imtProof: imt.engine.merklePath(lowIndex),
    ...messageProofForSlBlock(slBlock),
  };
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
    p.messageTxNumberInBatch,
    p.messageIndex,
    p.messageProof,
    leafTuple(p.leaf),
    p.imtLeafIndex,
    p.imtProof,
  ];
}

/** Build the `AtomicFinalityProof` tuple `executeAtomicBundle` consumes. */
export function atomicFinalityProofTuple(p: AtomicFinalityProof): unknown[] {
  return [
    p.flowId,
    p.deadline,
    p.legBundleHashes,
    p.chainIds.map((c) => BigInt(c)),
    p.proofs.map(proofTuple),
  ];
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
      reason: `no L2InteropCommitmentTree deployed at ${layout.commitmentTree} (the pinned demo image does not predeploy the atomic built-ins — needs an atomic server image + genesis; see ATOMIC-INTEROP-PLAN.md)`,
      layout,
    };
  }
  const tree = commitmentTreeContract(layout.commitmentTree, provider);
  // root() reverting / mismatching ABI means the contract there is not the tree.
  await tree.root();
  await tree.leafCount();
  return { supported: true, layout };
}
