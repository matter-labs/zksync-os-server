/**
 * Atomic interop demo: a cross-chain ATOMIC SWAP across the 3-chain stack.
 *
 * Demonstrates the IMT bundle model (all-or-nothing): chain A sends token X to a
 * user on chain B, and chain B sends token Y to the same user on chain A. Both legs
 * are bound into one atomic flow; either both execute or neither does. Atomicity is
 * proven per-leg via each chain's on-chain `L2InteropCommitmentTree` (an Indexed
 * Merkle Tree), coordinated by `AtomicFlowManager` — there is NO L1 coordination of
 * the swap itself.
 *
 * This mirrors `examples/bundle-transfer.ts` for token setup but drives the atomic
 * path (`atomicBundle` attribute + `executeAtomicBundle`) instead of the public
 * single-bundle path.
 *
 * ─────────────────────────────────────────────────────────────────────────────
 * REQUIREMENT — atomic server + genesis: the chains must predeploy the atomic
 * built-ins (`L2InteropCommitmentTree` @ 0x10012, `AtomicFlowManager` @ 0x10014)
 * and use the atomic protocol layout (`InteropCenter` @ 0x1000d, `InteropHandler`
 * @ 0x1000e), from the `ad-atomic-interop` server branch + the `atomic-imt-interop`
 * era-contracts genesis — exactly what this preset deploys (see ../ATOMIC_SWAP.md).
 * The script still detects capability at startup and prints a precise BLOCKED
 * message rather than failing obscurely.
 * ─────────────────────────────────────────────────────────────────────────────
 *
 * Usage (default A=6565 @ :3050, B=6566 @ :3051; override RPCs via env):
 *   PRIVATE_KEY=0x... \
 *   L2_RPC_URL=http://127.0.0.1:3050 L2_RPC_URL_SECOND=http://127.0.0.1:3051 \
 *   npm run atomic-swap
 *
 * Optional env:
 *   ATOMIC_DEADLINE_TS      settlement-layer (L1) timestamp for the flow deadline
 *                           (default: latest L1 block timestamp + 24h)
 *   ATOMIC_INTEROP_CENTER / ATOMIC_INTEROP_HANDLER /
 *   ATOMIC_COMMITMENT_TREE / ATOMIC_FLOW_MANAGER  layout overrides (if the
 *                          published atomic image uses a different layout)
 */

import { ethers } from 'ethers';
import {
  BundleBuilder,
  CallBuilder,
  L2_NATIVE_TOKEN_VAULT_ADDRESS,
  NativeTokenVaultAbi,
  ERC20Abi,
  computeAssetId,
  // atomic surface
  resolveAtomicLayout,
  detectAtomicCapability,
  AtomicInteropCenterAbi,
  AtomicInteropHandlerAbi,
  AtomicFlowManagerAbi,
  INTEROP_BUNDLE_TUPLE,
  atomicBundleAttr,
  computeFlowId,
  commitValue,
  sortLegs,
  atomicFinalityProofTuple,
  LegState,
  type ImtProof,
} from './lib';

// SimpleERC20 creation bytecode (constructor takes uint256 initial supply) —
// identical to examples/bundle-transfer.ts.
const TOKEN_BYTECODE =
  '0x60806040526040518060400160405280600a81526020017f5465737420546f6b656e000000000000000000000000000000000000000000008152505f9081620000499190620003f9565b506040518060400160405280600481526020017f544553540000000000000000000000000000000000000000000000000000000081525060019081620000909190620003f9565b50601260025f6101000a81548160ff021916908360ff160217905550348015620000b8575f80fd5b50604051620012f5380380620012f58339818101604052810190620000de919062000510565b806003819055508060045f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f20819055503373ffffffffffffffffffffffffffffffffffffffff165f73ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef8360405162000186919062000551565b60405180910390a3506200056c565b5f81519050919050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffd5b7f4e487b71000000000000000000000000000000000000000000000000000000005f52602260045260245ffd5b5f60028204905060018216806200021157607f821691505b602082108103620002275762000226620001cc565b5b50919050565b5f819050815f5260205f209050919050565b5f6020601f8301049050919050565b5f82821b905092915050565b5f600883026200028b7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff826200024e565b6200029786836200024e565b95508019841693508086168417925050509392505050565b5f819050919050565b5f819050919050565b5f620002e1620002db620002d584620002af565b620002b8565b620002af565b9050919050565b5f819050919050565b620002fc83620002c1565b620003146200030b82620002e8565b8484546200025a565b825550505050565b5f90565b6200032a6200031c565b62000337818484620002f1565b505050565b5b818110156200035e57620003525f8262000320565b6001810190506200033d565b5050565b601f821115620003ad5762000377816200022d565b62000382846200023f565b8101602085101562000392578190505b620003aa620003a1856200023f565b8301826200033c565b50505b505050565b5f82821c905092915050565b5f620003cf5f1984600802620003b2565b1980831691505092915050565b5f620003e98383620003be565b9150826002028217905092915050565b620004048262000195565b67ffffffffffffffff81111562000420576200041f6200019f565b5b6200042c8254620001f9565b6200043982828562000362565b5f60209050601f8311600181146200046f575f84156200045a578287015190505b620004668582620003dc565b865550620004d5565b601f1984166200047f866200022d565b5f5b82811015620004a85784890151825560018201915060208501945060208101905062000481565b86831015620004c85784890151620004c4601f891682620003be565b8355505b6001600288020188555050505b505050505050565b5f80fd5b620004ec81620002af565b8114620004f7575f80fd5b50565b5f815190506200050a81620004e1565b92915050565b5f60208284031215620005285762000527620004dd565b5b5f6200053784828501620004fa565b91505092915050565b6200054b81620002af565b82525050565b5f602082019050620005665f83018462000540565b92915050565b610d7b806200057a5f395ff3fe608060405234801561000f575f80fd5b5060043610610091575f3560e01c8063313ce56711610064578063313ce5671461013157806370a082311461014f57806395d89b411461017f578063a9059cbb1461019d578063dd62ed3e146101cd57610091565b806306fdde0314610095578063095ea7b3146100b357806318160ddd146100e357806323b872dd14610101575b5f80fd5b61009d6101fd565b6040516100aa919061094e565b60405180910390f35b6100cd60048036038101906100c891906109ff565b610288565b6040516100da9190610a57565b60405180910390f35b6100eb610375565b6040516100f89190610a7f565b60405180910390f35b61011b60048036038101906101169190610a98565b61037b565b6040516101289190610a57565b60405180910390f35b61013961065b565b6040516101469190610b03565b60405180910390f35b61016960048036038101906101649190610b1c565b61066d565b6040516101769190610a7f565b60405180910390f35b610187610682565b604051610194919061094e565b60405180910390f35b6101b760048036038101906101b291906109ff565b61070e565b6040516101c49190610a57565b60405180910390f35b6101e760048036038101906101e29190610b47565b6108a4565b6040516101f49190610a7f565b60405180910390f35b5f805461020990610bb2565b80601f016020809104026020016040519081016040528092919081815260200182805461023590610bb2565b80156102805780601f1061025757610100808354040283529160200191610280565b820191905f5260205f20905b81548152906001019060200180831161026357829003601f168201915b505050505081565b5f8160055f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f20819055508273ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff167f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925846040516103639190610a7f565b60405180910390a36001905092915050565b60035481565b5f8160045f8673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205410156103fc576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016103f390610c2c565b60405180910390fd5b8160055f8673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205410156104b7576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016104ae90610c94565b60405180910390fd5b8160045f8673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546105039190610cdf565b925050819055508160045f8573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546105569190610d12565b925050819055508160055f8673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546105e49190610cdf565b925050819055508273ffffffffffffffffffffffffffffffffffffffff168473ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef846040516106489190610a7f565b60405180910390a3600190509392505050565b60025f9054906101000a900460ff1681565b6004602052805f5260405f205f915090505481565b6001805461068f90610bb2565b80601f01602080910402602001604051908101604052809291908181526020018280546106bb90610bb2565b80156107065780601f106106dd57610100808354040283529160200191610706565b820191905f5260205f20905b8154815290600101906020018083116106e957829003601f168201915b505050505081565b5f8160045f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f2054101561078f576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161078690610c2c565b60405180910390fd5b8160045f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546107db9190610cdf565b925050819055508160045f8573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f82825461082e9190610d12565b925050819055508273ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef846040516108929190610a7f565b60405180910390a36001905092915050565b6005602052815f5260405f20602052805f5260405f205f91509150505481565b5f81519050919050565b5f82825260208201905092915050565b5f5b838110156108fb5780820151818401526020810190506108e0565b5f8484015250505050565b5f601f19601f8301169050919050565b5f610920826108c4565b61092a81856108ce565b935061093a8185602086016108de565b61094381610906565b840191505092915050565b5f6020820190508181035f8301526109668184610916565b905092915050565b5f80fd5b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f61099b82610972565b9050919050565b6109ab81610991565b81146109b5575f80fd5b50565b5f813590506109c6816109a2565b92915050565b5f819050919050565b6109de816109cc565b81146109e8575f80fd5b50565b5f813590506109f9816109d5565b92915050565b5f8060408385031215610a1557610a1461096e565b5b5f610a22858286016109b8565b9250506020610a33858286016109eb565b9150509250929050565b5f8115159050919050565b610a5181610a3d565b82525050565b5f602082019050610a6a5f830184610a48565b92915050565b610a79816109cc565b82525050565b5f602082019050610a925f830184610a70565b92915050565b5f805f60608486031215610aaf57610aae61096e565b5b5f610abc868287016109b8565b9350506020610acd868287016109b8565b9250506040610ade868287016109eb565b9150509250925092565b5f60ff82169050919050565b610afd81610ae8565b82525050565b5f602082019050610b165f830184610af4565b92915050565b5f60208284031215610b3157610b3061096e565b5b5f610b3e848285016109b8565b91505092915050565b5f8060408385031215610b5d57610b5c61096e565b5b5f610b6a858286016109b8565b9250506020610b7b858286016109b8565b9150509250929050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52602260045260245ffd5b5f6002820490506001821680610bc957607f821691505b602082108103610bdc57610bdc610b85565b5b50919050565b7f496e73756666696369656e742062616c616e63650000000000000000000000005f82015250565b5f610c166014836108ce565b9150610c2182610be2565b602082019050919050565b5f6020820190508181035f830152610c4381610c0a565b9050919050565b7f496e73756666696369656e7420616c6c6f77616e6365000000000000000000005f82015250565b5f610c7e6016836108ce565b9150610c8982610c4a565b602082019050919050565b5f6020820190508181035f830152610cab81610c72565b9050919050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f610ce9826109cc565b9150610cf4836109cc565b9250828203905081811115610d0c57610d0b610cb2565b5b92915050565b5f610d1c826109cc565b9150610d27836109cc565b9250828201905080821115610d3f57610d3e610cb2565b5b9291505056fea26469706673582212208b562ac4f0f974b2ee612ecf1be3e3c4caa136b06cc2b96ce39f3a0a66c1b9b664736f6c63430008140033';

function requireEnv(name: string): string {
  const value = process.env[name];
  if (!value) throw new Error(`Missing env var: ${name}`);
  return value;
}

interface ChainCtx {
  name: string;
  provider: ethers.JsonRpcProvider;
  wallet: ethers.Wallet;
  chainId: bigint;
  token: string; // native token deployed on this chain
}

/** Deploy + register + approve a fresh ERC20 on a chain (mirrors bundle-transfer.ts setup). */
async function setupToken(name: string, rpc: string, pk: string, approveAmount: bigint): Promise<ChainCtx> {
  const provider = new ethers.JsonRpcProvider(rpc);
  const wallet = new ethers.Wallet(pk, provider);
  const chainId = (await provider.getNetwork()).chainId;
  console.log(`\n[${name}] chainId=${chainId} deploying ERC20...`);

  const initialSupply = ethers.parseUnits('1000000', 18);
  const ctorArgs = ethers.AbiCoder.defaultAbiCoder().encode(['uint256'], [initialSupply]);
  const deployTx = await wallet.sendTransaction({ data: TOKEN_BYTECODE + ctorArgs.substring(2) });
  const deployReceipt = await deployTx.wait();
  const token = deployReceipt!.contractAddress!;
  console.log(`[${name}] token at ${token}`);

  const ntv = new ethers.Contract(
    L2_NATIVE_TOKEN_VAULT_ADDRESS,
    [...NativeTokenVaultAbi, 'function ensureTokenIsRegistered(address _nativeToken) returns (bytes32)'],
    wallet
  );
  await (await ntv.ensureTokenIsRegistered(token)).wait();
  const erc20 = new ethers.Contract(token, ERC20Abi, wallet);
  await (await erc20.approve(L2_NATIVE_TOKEN_VAULT_ADDRESS, approveAmount)).wait();
  console.log(`[${name}] registered + approved NTV`);

  return { name, provider, wallet, chainId, token };
}

/** A token-transfer call starter, sending `amount` of `source`'s token to `recipient` on dest. */
function legCall(source: ChainCtx, amount: bigint, recipient: string) {
  return CallBuilder.tokenTransfer(source.chainId, source.token, amount, recipient);
}

/**
 * Predict a leg's bundle hash WITHOUT the atomic attribute (the bundleHash is
 * independent of flowId/deadline). Uses callStatic on InteropCenter.sendBundle.
 */
async function predictBundleHash(
  source: ChainCtx,
  dest: ChainCtx,
  amount: bigint,
  recipient: string,
  interopCenterAddr: string,
  value: bigint
): Promise<string> {
  const ic = new ethers.Contract(interopCenterAddr, AtomicInteropCenterAbi, source.wallet);
  const builder = new BundleBuilder(dest.chainId).addCall(legCall(source, amount, recipient));
  return ic.sendBundle.staticCall(
    builder.getEncodedDestination(),
    builder.getCalls(),
    builder.getBundleAttributes(),
    { value }
  );
}

/** Send an atomic leg (burn + IMT insert) and return the ABI-encoded bundle + its hash. */
async function sendAtomicLeg(params: {
  source: ChainCtx;
  dest: ChainCtx;
  amount: bigint;
  recipient: string;
  flowId: string;
  deadline: number;
  predictedHash: string;
  interopCenterAddr: string;
  value: bigint;
}): Promise<{ bundleData: string; bundleHash: string; sendBlock: number }> {
  const { source, dest, amount, recipient, flowId, deadline, predictedHash, interopCenterAddr, value } = params;

  // Low-nullifier index from the server's IMT engine (zks_getImtLowNullifierIndex). The atomic
  // server always exposes it; a null result means the value is already present (or the tree is
  // uninitialized), both fatal for a fresh send.
  const v = commitValue(flowId, predictedHash);
  const block = await source.provider.getBlockNumber();
  const lnRpc = await source.provider.send('zks_getImtLowNullifierIndex', [v, block]);
  if (lnRpc === null || lnRpc === undefined) {
    throw new Error(`no low-nullifier for commit value ${v} at block ${block} (already committed?)`);
  }
  const lowNull = Number(lnRpc);

  const ic = new ethers.Contract(interopCenterAddr, AtomicInteropCenterAbi, source.wallet);
  const builder = new BundleBuilder(dest.chainId).addCall(legCall(source, amount, recipient));
  const tx = await ic.sendBundle(
    builder.getEncodedDestination(),
    builder.getCalls(),
    [atomicBundleAttr(flowId, deadline, lowNull)],
    { value }
  );
  const receipt = await tx.wait();

  // Recover the emitted InteropBundle (needed verbatim by executeAtomicBundle).
  const iface = new ethers.Interface(AtomicInteropCenterAbi);
  let bundleData: string | undefined;
  let bundleHash: string | undefined;
  for (const log of receipt!.logs) {
    if (log.address.toLowerCase() !== interopCenterAddr.toLowerCase()) continue;
    const parsed = iface.parseLog({ topics: log.topics as string[], data: log.data });
    if (parsed?.name === 'InteropBundleSent') {
      bundleHash = parsed.args.interopBundleHash;
      // Re-encode the InteropBundle struct from the event arg (verbatim bytes that
      // executeAtomicBundle expects).
      const b = parsed.args.interopBundle;
      bundleData = ethers.AbiCoder.defaultAbiCoder().encode([INTEROP_BUNDLE_TUPLE], [b]);
      break;
    }
  }
  if (!bundleData || !bundleHash) throw new Error('InteropBundleSent event not found');
  if (bundleHash.toLowerCase() !== predictedHash.toLowerCase()) {
    throw new Error(`bundleHash ${bundleHash} != predicted ${predictedHash}`);
  }
  console.log(`[${source.name}->${dest.name}] sent atomic leg, bundleHash=${bundleHash} lowNullifier=${lowNull}`);
  return { bundleData, bundleHash, sendBlock: receipt!.blockNumber };
}

const L2_INTEROP_ROOT_STORAGE = '0x0000000000000000000000000000000000010008';

/**
 * Server-completed per-leg proof (mirror of the `zks_getImtInclusionProof` /
 * `zks_getImtNonInclusionProof` response): the IMT half against the batch-end (inclusion) or
 * batch-begin (non-inclusion) root, plus the settlement half authenticating that root as a
 * chain-batch-root leaf against the imported interop root.
 */
interface RpcImtProof {
  batchNumber: number;
  settlementBlockNumber: number;
  provesAgainstBeginRoot: boolean;
  chainImtRoot: string;
  settlementProof: string[];
  leaf: { value: string; nextIndex: string; nextValue: string };
  imtLeafIndex: number;
  imtProof: string[];
}

/**
 * Poll `zks_getImtInclusionProof(value, sendBlock)` until the batch containing the send is
 * executed on L1 and the proof is available. The server errors with "batch not available yet"
 * until execution; a `null` result means the commit value is genuinely absent (fail fast).
 */
async function waitForImtInclusionProof(
  provider: ethers.JsonRpcProvider,
  value: string,
  sendBlock: number
): Promise<RpcImtProof> {
  for (let i = 0; i < 150; i++) {
    try {
      const p = await provider.send('zks_getImtInclusionProof', [value, sendBlock]);
      if (p) return p as RpcImtProof;
      throw new Error(`commit value ${value} not present in IMT (server returned null)`);
    } catch (err) {
      const msg = (err as Error).message ?? '';
      if (!/not been finalized|not available/i.test(msg)) throw err;
    }
    await new Promise((r) => setTimeout(r, 2000));
  }
  throw new Error(`timed out waiting for IMT inclusion proof of ${value}`);
}

/** Attach the source chain id to a server proof, forming the on-chain `ImtProof`. */
function toLegProof(chainId: bigint, p: RpcImtProof): ImtProof {
  return {
    sourceChainId: chainId.toString(),
    batchNumber: String(p.batchNumber),
    chainImtRoot: p.chainImtRoot,
    provesAgainstBeginRoot: p.provesAgainstBeginRoot,
    settlementProof: p.settlementProof,
    leaf: { value: String(p.leaf.value), nextIndex: String(p.leaf.nextIndex), nextValue: String(p.leaf.nextValue) },
    imtLeafIndex: Number(p.imtLeafIndex),
    imtProof: p.imtProof,
  };
}

/** Poll L2InteropRootStorage until the interop root for (l1ChainId, slBlock) is imported. */
async function waitForInteropRoot(
  provider: ethers.JsonRpcProvider,
  l1ChainId: bigint,
  slBlock: number
): Promise<void> {
  const storage = new ethers.Contract(
    L2_INTEROP_ROOT_STORAGE,
    ['function interopRoots(uint256 chainId, uint256 batchNumber) view returns (bytes32)'],
    provider
  );
  for (let i = 0; i < 150; i++) {
    const r = await storage.interopRoots(l1ChainId, slBlock);
    if (r && r !== ethers.ZeroHash) return;
    await new Promise((r) => setTimeout(r, 2000));
  }
  throw new Error(`timed out waiting for interop root (l1=${l1ChainId}, slBlock=${slBlock})`);
}

async function main() {
  const PRIVATE_KEY = requireEnv('PRIVATE_KEY');
  const RPC_A = requireEnv('L2_RPC_URL');
  const RPC_B = requireEnv('L2_RPC_URL_SECOND');
  // The deadline is a settlement-layer (L1) TIMESTAMP: each leg's inclusion proof carries its
  // batch's L1 settlement timestamp, checked on-chain against this value.
  const l1Provider = new ethers.JsonRpcProvider(process.env.L1_RPC_URL ?? 'http://127.0.0.1:8545');
  const l1ChainId = (await l1Provider.getNetwork()).chainId;
  const l1Now = (await l1Provider.getBlock('latest'))!.timestamp;
  const deadline = Number(process.env.ATOMIC_DEADLINE_TS ?? l1Now + 24 * 3600);
  const layout = resolveAtomicLayout();
  const aAmount = ethers.parseUnits('100', 18);
  const bAmount = ethers.parseUnits('100', 18);

  console.log('=== ATOMIC SWAP DEMO (IMT bundle model) ===');
  console.log('Atomic layout:', layout);

  // ── Capability gate ────────────────────────────────────────────────────────
  const providerA = new ethers.JsonRpcProvider(RPC_A);
  const providerB = new ethers.JsonRpcProvider(RPC_B);
  for (const [label, provider] of [
    ['A', providerA],
    ['B', providerB],
  ] as const) {
    const cap = await detectAtomicCapability(provider, layout);
    if (!cap.supported) {
      console.error(`\nBLOCKED: chain ${label} does not support atomic interop.`);
      console.error(`  reason: ${cap.reason}`);
      console.error('  This demo needs an atomic-capable zksync-os-server image + genesis.');
      console.error('  See ATOMIC_SWAP.md in this preset.');
      process.exit(2);
    }
  }
  console.log('Atomic built-ins detected on both chains. Proceeding.');

  // ── Token setup ──────────────────────────────────────────────────────────────
  const user = new ethers.Wallet(PRIVATE_KEY).address;
  const a = await setupToken('A', RPC_A, PRIVATE_KEY, aAmount);
  const b = await setupToken('B', RPC_B, PRIVATE_KEY, bAmount);

  // Bundle send value = interopProtocolFee * callCount. Each leg has one call, so
  // value == interopProtocolFee (0 when fees are disabled). InteropCenter reverts
  // with MsgValueMismatch otherwise.
  const fee: bigint = await new ethers.Contract(layout.interopCenter, AtomicInteropCenterAbi, providerA).interopProtocolFee();
  console.log('interopProtocolFee (per call):', fee.toString());

  // ── Predict bundle hashes + compute flowId ───────────────────────────────────
  console.log('\n=== PREDICT LEG HASHES + FLOW ID ===');
  const hAB = await predictBundleHash(a, b, aAmount, user, layout.interopCenter, fee);
  const hBA = await predictBundleHash(b, a, bAmount, user, layout.interopCenter, fee);
  const { legBundleHashes, chainIds } = sortLegs([
    { bundleHash: hAB, chainId: a.chainId },
    { bundleHash: hBA, chainId: b.chainId },
  ]);
  const flowId = computeFlowId(legBundleHashes, chainIds, deadline, l1ChainId);
  console.log('hAB:', hAB);
  console.log('hBA:', hBA);
  console.log('flowId:', flowId, 'deadline:', deadline);

  // ── Send both atomic legs (burn + IMT insert) ────────────────────────────────
  console.log('\n=== SEND ATOMIC LEGS ===');
  const ab = await sendAtomicLeg({
    source: a,
    dest: b,
    amount: aAmount,
    recipient: user,
    flowId,
    deadline,
    predictedHash: hAB,
    interopCenterAddr: layout.interopCenter,
    value: fee,
  });
  const ba = await sendAtomicLeg({
    source: b,
    dest: a,
    amount: bAmount,
    recipient: user,
    flowId,
    deadline,
    predictedHash: hBA,
    interopCenterAddr: layout.interopCenter,
    value: fee,
  });

  // ── Assert both legs Committed ───────────────────────────────────────────────
  const mgrA = new ethers.Contract(layout.atomicFlowManager, AtomicFlowManagerAbi, a.provider);
  const mgrB = new ethers.Contract(layout.atomicFlowManager, AtomicFlowManagerAbi, b.provider);
  const stAB = Number(await mgrA.legState(flowId, ab.bundleHash));
  const stBA = Number(await mgrB.legState(flowId, ba.bundleHash));
  console.log(`legState(A, AB)=${LegState[stAB]}  legState(B, BA)=${LegState[stBA]}`);
  if (stAB !== LegState.Committed || stBA !== LegState.Committed) {
    throw new Error('both legs must be Committed after send');
  }

  // ── PHASE 2: wait for L1 settlement, fetch COMPLETE proofs from the server ────
  // `zks_getImtInclusionProof` now returns the full per-leg ImtProof: the IMT half against the
  // batch-END root plus the settlement half authenticating that root as chain-batch-root leaf 3
  // against the imported interop root (the commitment tree publishes no L2->L1 message anymore).
  console.log('\n=== FETCH IMT PROOFS ===');
  console.log('waiting for the send batches to execute on L1...');
  const abRaw = await waitForImtInclusionProof(a.provider, commitValue(flowId, hAB), ab.sendBlock);
  const baRaw = await waitForImtInclusionProof(b.provider, commitValue(flowId, hBA), ba.sendBlock);
  console.log(`AB proof: batch=${abRaw.batchNumber} slBlock=${abRaw.settlementBlockNumber}; BA proof: batch=${baRaw.batchNumber} slBlock=${baRaw.settlementBlockNumber}`);

  const proofAB = toLegProof(a.chainId, abRaw);
  const proofBA = toLegProof(b.chainId, baRaw);
  // Order proofs to match legBundleHashes ascending.
  const proofs: ImtProof[] = legBundleHashes[0].toLowerCase() === hAB.toLowerCase() ? [proofAB, proofBA] : [proofBA, proofAB];
  const finality = atomicFinalityProofTuple({
    flowId,
    deadline,
    settlementLayerChainId: l1ChainId,
    legBundleHashes,
    legSourceChainIds: chainIds,
    proofs,
  });

  // ── Wait for interop roots to import on both executing chains ─────────────────
  // Both executeAtomicBundle calls verify every leg, so each executing chain must
  // have imported the L1 interop root at each leg's settlement block.
  console.log('\n=== WAIT FOR INTEROP ROOTS ===');
  // Every leg's proof must carry the L1 block its batch settled at; a missing value
  // would otherwise surface later as an unexplained executeAtomicBundle revert.
  const slBlocks = ([
    ['AB', abRaw],
    ['BA', baRaw],
  ] as const).map(([leg, p]) => {
    if (typeof p.settlementBlockNumber !== 'number') {
      throw new Error(`leg ${leg}: inclusion proof (batch ${p.batchNumber}) has no settlementBlockNumber`);
    }
    return p.settlementBlockNumber;
  });
  console.log(`waiting for interop roots (L1 ${l1ChainId}) at blocks ${slBlocks} on both chains...`);
  for (const ctx of [a, b]) {
    for (const sl of slBlocks) {
      await waitForInteropRoot(ctx.provider, l1ChainId, sl);
    }
  }
  console.log('interop roots imported on both chains');

  // ── PHASE 3: executeAtomicBundle on each destination ──────────────────────────
  console.log('\n=== EXECUTE ATOMIC BUNDLES ===');
  const handlerB = new ethers.Contract(layout.interopHandler, AtomicInteropHandlerAbi, b.wallet);
  const handlerA = new ethers.Contract(layout.interopHandler, AtomicInteropHandlerAbi, a.wallet);
  // AB executes on its destination chain B; BA executes on its destination chain A.
  const rB = await (await handlerB.executeAtomicBundle(ab.bundleData, finality)).wait();
  console.log('executed AB on B, status:', rB.status);
  const rA = await (await handlerA.executeAtomicBundle(ba.bundleData, finality)).wait();
  console.log('executed BA on A, status:', rA.status);

  // ── Assert both bundles FullyExecuted ─────────────────────────────────────────
  // BundleStatus enum (Messaging.sol): Unreceived=0, Verified=1, FullyExecuted=2, Unbundled=3.
  const FULLY_EXECUTED = 2;
  const stAB2 = Number(await handlerB.bundleStatus(ab.bundleHash));
  const stBA2 = Number(await handlerA.bundleStatus(ba.bundleHash));
  console.log(`bundleStatus(AB on B)=${stAB2} bundleStatus(BA on A)=${stBA2} (2=FullyExecuted)`);
  if (stAB2 !== FULLY_EXECUTED || stBA2 !== FULLY_EXECUTED) throw new Error('both bundles must reach FullyExecuted');

  // ── Verify mints ─────────────────────────────────────────────────────────────
  console.log('\n=== VERIFY MINTS ===');
  const assetAonB = computeAssetId(a.chainId, L2_NATIVE_TOKEN_VAULT_ADDRESS, a.token);
  const assetBonA = computeAssetId(b.chainId, L2_NATIVE_TOKEN_VAULT_ADDRESS, b.token);
  const ntvB = new ethers.Contract(
    L2_NATIVE_TOKEN_VAULT_ADDRESS,
    [...NativeTokenVaultAbi, 'function tokenAddress(bytes32 _assetId) view returns (address)'],
    b.provider
  );
  const ntvA = new ethers.Contract(
    L2_NATIVE_TOKEN_VAULT_ADDRESS,
    [...NativeTokenVaultAbi, 'function tokenAddress(bytes32 _assetId) view returns (address)'],
    a.provider
  );
  const wrappedAonB = await ntvB.tokenAddress(assetAonB);
  const wrappedBonA = await ntvA.tokenAddress(assetBonA);
  const balAonB = await new ethers.Contract(wrappedAonB, ERC20Abi, b.provider).balanceOf(user);
  const balBonA = await new ethers.Contract(wrappedBonA, ERC20Abi, a.provider).balanceOf(user);
  console.log(`wrapped A on B balance: ${ethers.formatUnits(balAonB, 18)}`);
  console.log(`wrapped B on A balance: ${ethers.formatUnits(balBonA, 18)}`);
  if (balAonB < aAmount || balBonA < bAmount) {
    throw new Error('atomic swap did not mint both legs');
  }

  console.log('\nAtomic swap complete: both legs executed atomically.');
}

main().catch((err) => {
  console.error('Error:', err);
  process.exit(1);
});
