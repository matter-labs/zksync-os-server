/**
 * Two-party atomic swap where NEITHER SIDE TOUCHES THE OTHER'S CHAIN.
 *
 * Unlike `atomic-swap-3chains.ts` (one process, both keys, both RPCs), this models the real
 * setting: Alice can reach only chain A, Bob only chain B, and both can read L1. Everything that
 * crosses the boundary goes through `Exchange` — an explicit message channel. If a step needs data
 * that isn't in the channel or on the party's own chain, the protocol is wrong, and the `Party`
 * class makes that impossible to fudge: it holds exactly one provider.
 *
 * The flow (see ATOMIC_SWAP.md for the protocol-level description):
 *   1. each side builds its OWN leg locally (buildBundle, which cross-checks its reconstruction
 *      against that chain's own `sendBundle` staticCall) and publishes `bundleData`
 *   2. each side verifies the COUNTERPARTY's bundle by pure keccak/ABI — no RPC — and both derive
 *      the same flowId independently; a mismatch aborts before anything is committed
 *   3. each side commits its own leg, then publishes its `ImtProof`
 *   4. each side executes the leg landing on its own chain, using both proofs
 *
 * Usage (chains from run_local.sh ./local-chains/v32.0/multi_chain):
 *   PRIVATE_KEY=0x... COUNTERPARTY_PRIVATE_KEY=0x... \
 *   L2_RPC_URL=http://127.0.0.1:3050 L2_RPC_URL_SECOND=http://127.0.0.1:3051 \
 *   L1_RPC_URL=http://127.0.0.1:8545 npx ts-node atomic-swap-two-party.ts
 */

import { ethers } from 'ethers';
import {
  BundleBuilder,
  CallBuilder,
  L2_NATIVE_TOKEN_VAULT_ADDRESS,
  NativeTokenVaultAbi,
  ERC20Abi,
  computeAssetId,
  resolveAtomicLayout,
  AtomicInteropCenterAbi,
  AtomicInteropHandlerAbi,
  AtomicFlowManagerAbi,
  INTEROP_BUNDLE_TUPLE,
  atomicBundleAttr,
  interopBundleSaltAttr,
  randomSalt,
  computeFlowId,
  commitValue,
  sortLegs,
  atomicFinalityProofTuple,
  LegState,
  type ImtProof,
} from './lib';
import { buildBundle } from './lib/build-bundle';
import { formatEvmV1WithAddress } from './lib/address';

const TOKEN_BYTECODE =
  '0x60806040526040518060400160405280600a81526020017f5465737420546f6b656e000000000000000000000000000000000000000000008152505f9081620000499190620003f9565b506040518060400160405280600481526020017f544553540000000000000000000000000000000000000000000000000000000081525060019081620000909190620003f9565b50601260025f6101000a81548160ff021916908360ff160217905550348015620000b8575f80fd5b50604051620012f5380380620012f58339818101604052810190620000de919062000510565b806003819055508060045f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f20819055503373ffffffffffffffffffffffffffffffffffffffff165f73ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef8360405162000186919062000551565b60405180910390a3506200056c565b5f81519050919050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffd5b7f4e487b71000000000000000000000000000000000000000000000000000000005f52602260045260245ffd5b5f60028204905060018216806200021157607f821691505b602082108103620002275762000226620001cc565b5b50919050565b5f819050815f5260205f209050919050565b5f6020601f8301049050919050565b5f82821b905092915050565b5f600883026200028b7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff826200024e565b6200029786836200024e565b95508019841693508086168417925050509392505050565b5f819050919050565b5f819050919050565b5f620002e1620002db620002d584620002af565b620002b8565b620002af565b9050919050565b5f819050919050565b620002fc83620002c1565b620003146200030b82620002e8565b8484546200025a565b825550505050565b5f90565b6200032a6200031c565b62000337818484620002f1565b505050565b5b818110156200035e57620003525f8262000320565b6001810190506200033d565b5050565b601f821115620003ad5762000377816200022d565b62000382846200023f565b8101602085101562000392578190505b620003aa620003a1856200023f565b8301826200033c565b50505b505050565b5f82821c905092915050565b5f620003cf5f1984600802620003b2565b1980831691505092915050565b5f620003e98383620003be565b9150826002028217905092915050565b620004048262000195565b67ffffffffffffffff81111562000420576200041f6200019f565b5b6200042c8254620001f9565b6200043982828562000362565b5f60209050601f8311600181146200046f575f84156200045a578287015190505b620004668582620003dc565b865550620004d5565b601f1984166200047f866200022d565b5f5b82811015620004a85784890151825560018201915060208501945060208101905062000481565b86831015620004c85784890151620004c4601f891682620003be565b8355505b6001600288020188555050505b505050505050565b5f80fd5b620004ec81620002af565b8114620004f7575f80fd5b50565b5f815190506200050a81620004e1565b92915050565b5f60208284031215620005285762000527620004dd565b5b5f6200053784828501620004fa565b91505092915050565b6200054b81620002af565b82525050565b5f602082019050620005665f83018462000540565b92915050565b610d7b806200057a5f395ff3fe608060405234801561000f575f80fd5b5060043610610091575f3560e01c8063313ce56711610064578063313ce5671461013157806370a082311461014f57806395d89b411461017f578063a9059cbb1461019d578063dd62ed3e146101cd57610091565b806306fdde0314610095578063095ea7b3146100b357806318160ddd146100e357806323b872dd14610101575b5f80fd5b61009d6101fd565b6040516100aa919061094e565b60405180910390f35b6100cd60048036038101906100c891906109ff565b610288565b6040516100da9190610a57565b60405180910390f35b6100eb610375565b6040516100f89190610a7f565b60405180910390f35b61011b60048036038101906101169190610a98565b61037b565b6040516101289190610a57565b60405180910390f35b61013961065b565b6040516101469190610b03565b60405180910390f35b61016960048036038101906101649190610b1c565b61066d565b6040516101769190610a7f565b60405180910390f35b610187610682565b604051610194919061094e565b60405180910390f35b6101b760048036038101906101b291906109ff565b61070e565b6040516101c49190610a57565b60405180910390f35b6101e760048036038101906101e29190610b47565b6108a4565b6040516101f49190610a7f565b60405180910390f35b5f805461020990610bb2565b80601f016020809104026020016040519081016040528092919081815260200182805461023590610bb2565b80156102805780601f1061025757610100808354040283529160200191610280565b820191905f5260205f20905b81548152906001019060200180831161026357829003601f168201915b505050505081565b5f8160055f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f20819055508273ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff167f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925846040516103639190610a7f565b60405180910390a36001905092915050565b60035481565b5f8160045f8673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205410156103fc576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016103f390610c2c565b60405180910390fd5b8160055f8673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205410156104b7576040517f08c379a00000000000000000000000000000000000000000000000000000000081526004016104ae90610c94565b60405180910390fd5b8160045f8673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546105039190610cdf565b925050819055508160045f8573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546105569190610d12565b925050819055508160055f8673ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546105e49190610cdf565b925050819055508273ffffffffffffffffffffffffffffffffffffffff168473ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef846040516106489190610a7f565b60405180910390a3600190509392505050565b60025f9054906101000a900460ff1681565b6004602052805f5260405f205f915090505481565b6001805461068f90610bb2565b80601f01602080910402602001604051908101604052809291908181526020018280546106bb90610bb2565b80156107065780601f106106dd57610100808354040283529160200191610706565b820191905f5260205f20905b8154815290600101906020018083116106e957829003601f168201915b505050505081565b5f8160045f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f2054101561078f576040517f08c379a000000000000000000000000000000000000000000000000000000000815260040161078690610c2c565b60405180910390fd5b8160045f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546107db9190610cdf565b925050819055508160045f8573ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f82825461082e9190610d12565b925050819055508273ffffffffffffffffffffffffffffffffffffffff163373ffffffffffffffffffffffffffffffffffffffff167fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef846040516108929190610a7f565b60405180910390a36001905092915050565b6005602052815f5260405f20602052805f5260405f205f91509150505481565b5f81519050919050565b5f82825260208201905092915050565b5f5b838110156108fb5780820151818401526020810190506108e0565b5f8484015250505050565b5f601f19601f8301169050919050565b5f610920826108c4565b61092a81856108ce565b935061093a8185602086016108de565b61094381610906565b840191505092915050565b5f6020820190508181035f8301526109668184610916565b905092915050565b5f80fd5b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f61099b82610972565b9050919050565b6109ab81610991565b81146109b5575f80fd5b50565b5f813590506109c6816109a2565b92915050565b5f819050919050565b6109de816109cc565b81146109e8575f80fd5b50565b5f813590506109f9816109d5565b92915050565b5f8060408385031215610a1557610a1461096e565b5b5f610a22858286016109b8565b9250506020610a33858286016109eb565b9150509250929050565b5f8115159050919050565b610a5181610a3d565b82525050565b5f602082019050610a6a5f830184610a48565b92915050565b610a79816109cc565b82525050565b5f602082019050610a925f830184610a70565b92915050565b5f805f60608486031215610aaf57610aae61096e565b5b5f610abc868287016109b8565b9350506020610acd868287016109b8565b9250506040610ade868287016109eb565b9150509250925092565b5f60ff82169050919050565b610afd81610ae8565b82525050565b5f602082019050610b165f830184610af4565b92915050565b5f60208284031215610b3157610b3061096e565b5b5f610b3e848285016109b8565b91505092915050565b5f8060408385031215610b5d57610b5c61096e565b5b5f610b6a858286016109b8565b9250506020610b7b858286016109b8565b9150509250929050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52602260045260245ffd5b5f6002820490506001821680610bc957607f821691505b602082108103610bdc57610bdc610b85565b5b50919050565b7f496e73756666696369656e742062616c616e63650000000000000000000000005f82015250565b5f610c166014836108ce565b9150610c2182610be2565b602082019050919050565b5f6020820190508181035f830152610c4381610c0a565b9050919050565b7f496e73756666696369656e7420616c6c6f77616e6365000000000000000000005f82015250565b5f610c7e6016836108ce565b9150610c8982610c4a565b602082019050919050565b5f6020820190508181035f830152610cab81610c72565b9050919050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f610ce9826109cc565b9150610cf4836109cc565b9250828203905081811115610d0c57610d0b610cb2565b5b92915050565b5f610d1c826109cc565b9150610d27836109cc565b9250828201905080821115610d3f57610d3e610cb2565b5b9291505056fea26469706673582212208b562ac4f0f974b2ee612ecf1be3e3c4caa136b06cc2b96ce39f3a0a66c1b9b664736f6c63430008140033';

const COMMITMENT_TREE_ADDR = '0x0000000000000000000000000000000000010012';
const L2_INTEROP_ROOT_STORAGE = '0x0000000000000000000000000000000000010008';

interface RpcImtProof {
  batchNumber: number;
  settlementBlockNumber?: number;
  chainImtRoot: string;
  settlementProof: string[];
  leaf: { value: string; nextIndex: string; nextValue: string };
  imtLeafIndex: number;
  imtProof: string[];
}

// ─────────────────────────────────────────────────────────────────────────────
// The exchange boundary
// ─────────────────────────────────────────────────────────────────────────────

/** What one side publishes about its leg, before committing. */
interface LegAnnouncement {
  chainId: string;
  sender: string;
  /**
   * Informational only: the sender's local token address. Identity is carried by the assetId in
   * the terms and checked against the bundle — never trust this field, the counterparty controls it
   * and it may be an address you cannot inspect anyway.
   */
  token: string;
  bundleData: string;
  bundleHash: string;
}

/**
 * The ONLY channel between the parties. In production this is a message queue, an HTTPS endpoint,
 * or a chat window — the point is that it carries bytes, not chain access. Everything here is
 * self-verifying, so the channel need not be trusted: a tampered bundle fails its hash check, and
 * a forged proof fails on-chain verification.
 */
class Exchange {
  private legs = new Map<string, LegAnnouncement>();
  private proofs = new Map<string, ImtProof>();
  /**
   * Agreed out of band before anything else, per leg.
   *
   * The asset is identified by `assetId`, NOT by a token address. The assetId is chain-independent
   * — `keccak(originChainId, ntv, originToken)`, the same 32 bytes everywhere — which matters here
   * for three reasons:
   *   - a party can name an asset without knowing (or being able to derive) its address on the
   *     counterparty's chain; for a bridged asset they can read the id from their OWN chain
   *   - it distinguishes otherwise identical-looking assets (A-native "USDT" vs B-native "USDT"
   *     have different ids and are NOT fungible)
   *   - verification becomes a direct comparison against the bundle instead of a re-derivation
   *     that assumes the NTV address and the token's origin
   * Token addresses stay private to each party, who needs only their own for approve/balance.
   */
  terms!: {
    deadline: number;
    settlementChainId: bigint;
    /** What Alice sends from chain A. */
    legA: { sourceChainId: bigint; assetId: string; amount: bigint; recipient: string };
    /** What Bob sends from chain B. */
    legB: { sourceChainId: bigint; assetId: string; amount: bigint; recipient: string };
  };

  publishLeg(party: string, leg: LegAnnouncement) {
    console.log(`  [exchange] ${party} publishes leg  hash=${leg.bundleHash}`);
    this.legs.set(party, leg);
  }
  readLeg(party: string): LegAnnouncement {
    const leg = this.legs.get(party);
    if (!leg) throw new Error(`no leg published by ${party}`);
    return leg;
  }
  publishProof(party: string, proof: ImtProof) {
    console.log(`  [exchange] ${party} publishes proof (batch ${proof.batchNumber})`);
    this.proofs.set(party, proof);
  }
  readProof(party: string): ImtProof {
    const proof = this.proofs.get(party);
    if (!proof) throw new Error(`no proof published by ${party}`);
    return proof;
  }
}

// ─────────────────────────────────────────────────────────────────────────────
// A party — one chain, one key, no view of the counterparty
// ─────────────────────────────────────────────────────────────────────────────

class Party {
  readonly provider: ethers.JsonRpcProvider;
  readonly wallet: ethers.Wallet;
  chainId!: bigint;
  /** This party's own token address — needed locally for approve/balance, never shared as identity. */
  token!: string;
  /** The chain-independent id of that token, read from this chain's NTV after registration. */
  assetId!: string;

  constructor(
    readonly name: string,
    rpc: string,
    privateKey: string,
    readonly layout: ReturnType<typeof resolveAtomicLayout>
  ) {
    this.provider = new ethers.JsonRpcProvider(rpc);
    this.wallet = new ethers.Wallet(privateKey, this.provider);
  }

  get address(): string {
    return this.wallet.address;
  }

  /** Deploy + NTV-register + approve this party's token. Own chain only. */
  async setupToken(approveAmount: bigint): Promise<void> {
    this.chainId = (await this.provider.getNetwork()).chainId;
    const initialSupply = ethers.parseUnits('1000000', 18);
    const ctorArgs = ethers.AbiCoder.defaultAbiCoder().encode(['uint256'], [initialSupply]);
    const deployTx = await this.wallet.sendTransaction({ data: TOKEN_BYTECODE + ctorArgs.substring(2) });
    this.token = (await deployTx.wait())!.contractAddress!;
    const ntv = new ethers.Contract(
      L2_NATIVE_TOKEN_VAULT_ADDRESS,
      [...NativeTokenVaultAbi, 'function ensureTokenIsRegistered(address _nativeToken) returns (bytes32)'],
      this.wallet
    );
    await (await ntv.ensureTokenIsRegistered(this.token)).wait();
    await (await new ethers.Contract(this.token, ERC20Abi, this.wallet).approve(
      L2_NATIVE_TOKEN_VAULT_ADDRESS,
      approveAmount
    )).wait();
    // Read the id the NTV assigned, rather than deriving it: this is the value that will appear in
    // the bundle, and it is what the counterparty verifies against the terms.
    this.assetId = await new ethers.Contract(
      L2_NATIVE_TOKEN_VAULT_ADDRESS,
      ['function assetId(address token) view returns (bytes32)'],
      this.provider
    ).assetId(this.token);
    console.log(
      `[${this.name}] chain ${this.chainId}, token ${this.token} (registered + approved), assetId ${this.assetId}`
    );
  }

  /**
   * Build this party's leg WITHOUT sending it, and announce it. `buildBundle` asserts its
   * reconstruction against this chain's own `sendBundle` staticCall, so a wrong hash can never
   * reach the counterparty.
   */
  async buildAndAnnounceLeg(
    exchange: Exchange,
    destChainId: bigint,
    amount: bigint,
    recipient: string,
    salt: string
  ): Promise<LegAnnouncement> {
    const calldata = CallBuilder.tokenTransfer(this.chainId, this.token, amount, recipient);
    const built = await buildBundle({
      provider: this.provider,
      sender: this.address,
      destinationChainId: destChainId,
      salt,
      calls: [
        {
          to: calldata.to.startsWith('0x') && calldata.to.length === 42 ? calldata.to : decodeEvmAddress(calldata.to),
          data: calldata.data,
          indirectCall: true,
        },
      ],
      interopCenter: this.layout.interopCenter,
      // verifyAgainstChain defaults to true — the hash-match assertion.
    });
    console.log(`[${this.name}] built leg locally, hash matches on-chain sendBundle ✓`);
    const announcement: LegAnnouncement = {
      chainId: this.chainId.toString(),
      sender: this.address,
      token: this.token,
      bundleData: built.bundleData,
      bundleHash: built.bundleHash,
    };
    exchange.publishLeg(this.name, announcement);
    return announcement;
  }

  /**
   * Verify a counterparty's announced leg using ONLY the bytes — pure keccak/ABI, no RPC. This is
   * what makes the exchange trustless: the bytes are bound to the hash that goes into the flowId.
   */
  verifyCounterpartyLeg(
    leg: LegAnnouncement,
    expect: { amount: bigint; recipient: string; assetId: string }
  ): void {
    const recomputed = ethers.keccak256(
      ethers.AbiCoder.defaultAbiCoder().encode(['uint256', 'bytes'], [BigInt(leg.chainId), leg.bundleData])
    );
    if (recomputed.toLowerCase() !== leg.bundleHash.toLowerCase()) {
      throw new Error(`[${this.name}] counterparty bundleData does not hash to its declared hash`);
    }
    const [decoded] = ethers.AbiCoder.defaultAbiCoder().decode([INTEROP_BUNDLE_TUPLE], leg.bundleData);
    if (BigInt(decoded[2]) !== this.chainId) {
      throw new Error(`[${this.name}] counterparty leg targets chain ${decoded[2]}, not mine (${this.chainId})`);
    }
    if (decoded[5].length !== 1) throw new Error(`[${this.name}] expected exactly one call in counterparty leg`);
    const call = decoded[5][0];

    // ── Bundle attributes: the griefing surface ────────────────────────────────────────────────
    // A NON-EMPTY executionAddress restricts who may call executeAtomicBundle on THIS chain. If the
    // counterparty pins it to an address they control, they can execute MY leg on their chain and
    // then simply decline to execute theirs here — and once both legs are committed there is no
    // refund (authorizeRefund requires a leg to be ABSENT). They lose nothing; I lose everything.
    // An empty value means permissionless execution, which is what makes the swap self-healing:
    // anyone, including me or a relayer, can push it through.
    const attrs = decoded[6];
    const [attrExecutionAddress, attrUnbundlerAddress, attrUseFixedFee] = [attrs[0], attrs[1], attrs[2]];
    if (attrExecutionAddress !== '0x') {
      throw new Error(
        `[${this.name}] counterparty leg pins executionAddress to ${attrExecutionAddress} — ` +
          `execution would not be permissionless, so they could withhold it after taking my leg`
      );
    }
    // Not used on the atomic path, but it should not be a surprise either: the default is the
    // sender on their own chain, which is what an unadorned sendBundle produces.
    const expectedUnbundler = formatEvmV1WithAddress(BigInt(leg.chainId), leg.sender);
    if (attrUnbundlerAddress.toLowerCase() !== expectedUnbundler.toLowerCase()) {
      throw new Error(
        `[${this.name}] unexpected unbundlerAddress ${attrUnbundlerAddress} (expected default ${expectedUnbundler})`
      );
    }
    if (attrUseFixedFee !== false) {
      throw new Error(`[${this.name}] counterparty leg sets useFixedFee, which was not agreed`);
    }
    // The bundle carries the REWRITTEN destination call produced by the source asset router:
    //   finalizeDeposit(uint256 sourceChainId, bytes32 assetId, bytes transferData)
    // with transferData = abi.encode(originalCaller, receiver, originToken, amount, erc20Metadata).
    // (Not the pre-rewrite `NEW_ENCODING_VERSION ++ (assetId, burnData)` handed to sendBundle.)
    const FINALIZE_DEPOSIT = '0x9c884fd1';
    if (!call[5].startsWith(FINALIZE_DEPOSIT)) {
      throw new Error(`[${this.name}] counterparty call is not finalizeDeposit (selector ${call[5].slice(0, 10)})`);
    }
    const [, assetId, transferData] = ethers.AbiCoder.defaultAbiCoder().decode(
      ['uint256', 'bytes32', 'bytes'],
      ethers.dataSlice(call[5], 4)
    );
    const [, receiver, , amount] = ethers.AbiCoder.defaultAbiCoder().decode(
      ['address', 'address', 'address', 'uint256', 'bytes'],
      transferData
    );
    if (receiver.toLowerCase() !== expect.recipient.toLowerCase()) {
      throw new Error(`[${this.name}] counterparty leg pays ${receiver}, expected ${expect.recipient}`);
    }
    if (amount !== expect.amount) {
      throw new Error(`[${this.name}] counterparty leg sends ${amount}, expected ${expect.amount}`);
    }
    // WHICH asset. The bundle carries an assetId, and the terms name an assetId, so this is a
    // direct comparison — no re-derivation from (chain, token), which would assume both the NTV's
    // address and the token's origin chain. Without this check a counterparty could send the right
    // amount of the WRONG asset and everything else would still pass.
    if (assetId.toLowerCase() !== expect.assetId.toLowerCase()) {
      throw new Error(
        `[${this.name}] counterparty leg moves asset ${assetId}, terms require ${expect.assetId}`
      );
    }
    console.log(
      `[${this.name}] verified counterparty leg: ${ethers.formatUnits(amount, 18)} of asset ${expect.assetId.slice(0, 12)}… → me ✓`
    );
  }

  /** Commit this party's leg on its own chain. */
  async commitLeg(params: {
    preimage: { deadline: number; settlementLayerChainId: bigint; legBundleHashes: string[]; legSourceChainIds: bigint[] };
    flowId: string;
    myBundleHash: string;
    destChainId: bigint;
    amount: bigint;
    recipient: string;
    salt: string;
  }): Promise<{ sendBlock: number; commitValue: string }> {
    const v = commitValue(params.flowId, params.myBundleHash);
    const block = await this.provider.getBlockNumber();
    const lowNull = await this.provider.send('zks_getImtLowNullifierIndex', [v, block]);
    if (lowNull === null || lowNull === undefined) throw new Error(`[${this.name}] no low-nullifier for ${v}`);

    const ic = new ethers.Contract(this.layout.interopCenter, AtomicInteropCenterAbi, this.wallet);
    const fee: bigint = await ic.interopProtocolFee();
    // Same construction the single-process driver uses, so the committed bundle matches the one
    // announced in step 1 byte for byte.
    const builder = new BundleBuilder(params.destChainId).addCall(
      CallBuilder.tokenTransfer(this.chainId, this.token, params.amount, params.recipient)
    );
    const tx = await ic.sendBundle(
      builder.getEncodedDestination(),
      builder.getCalls(),
      [atomicBundleAttr(params.preimage, Number(lowNull)), interopBundleSaltAttr(params.salt)],
      { value: fee }
    );
    const receipt = await tx.wait();
    console.log(`[${this.name}] committed own leg (block ${receipt!.blockNumber}, lowNullifier ${lowNull})`);
    return { sendBlock: receipt!.blockNumber, commitValue: v };
  }

  /** Fetch this party's own leg proof from its own chain. */
  async fetchOwnProof(commitVal: string, sendBlock: number): Promise<{ proof: ImtProof; slBlock: number }> {
    for (let i = 0; i < 150; i++) {
      try {
        const p: RpcImtProof | null = await this.provider.send('zks_getImtInclusionProof', [commitVal, sendBlock]);
        if (!p) throw new Error(`commit value ${commitVal} absent from IMT`);
        return {
          proof: {
            sourceChainId: this.chainId.toString(),
            batchNumber: String(p.batchNumber),
            chainImtRoot: p.chainImtRoot,
            provesAgainstBeginRoot: false,
            settlementProof: p.settlementProof,
            leaf: { value: String(p.leaf.value), nextIndex: String(p.leaf.nextIndex), nextValue: String(p.leaf.nextValue) },
            imtLeafIndex: Number(p.imtLeafIndex),
            imtProof: p.imtProof,
          },
          slBlock: Number(p.settlementBlockNumber),
        };
      } catch (err) {
        if (!/not been finalized|not available/i.test((err as Error).message ?? '')) throw err;
      }
      await new Promise((r) => setTimeout(r, 2000));
    }
    throw new Error(`[${this.name}] timed out waiting for own inclusion proof`);
  }

  /**
   * Verify a counterparty proof without touching their chain: recompute the commit value from the
   * bundle bytes we already hold, and check the proof commits to it.
   */
  verifyCounterpartyProof(proof: ImtProof, flowId: string, counterpartyLeg: LegAnnouncement): void {
    const expected = commitValue(flowId, counterpartyLeg.bundleHash);
    if (BigInt(proof.leaf.value) !== BigInt(expected)) {
      throw new Error(`[${this.name}] counterparty proof commits to ${proof.leaf.value}, expected ${expected}`);
    }
    if (BigInt(proof.sourceChainId) !== BigInt(counterpartyLeg.chainId)) {
      throw new Error(`[${this.name}] counterparty proof has wrong source chain`);
    }
    console.log(`[${this.name}] verified counterparty proof binds to the agreed bundle ✓`);
  }

  /** Wait until this chain has imported the interop root for a settlement block. */
  async waitForInteropRoot(l1ChainId: bigint, slBlock: number): Promise<void> {
    const storage = new ethers.Contract(
      L2_INTEROP_ROOT_STORAGE,
      ['function interopRoots(uint256 chainId, uint256 batchNumber) view returns (bytes32)'],
      this.provider
    );
    for (let i = 0; i < 150; i++) {
      const r = await storage.interopRoots(l1ChainId, slBlock);
      if (r && r !== ethers.ZeroHash) return;
      await new Promise((r) => setTimeout(r, 2000));
    }
    throw new Error(`[${this.name}] timed out waiting for interop root at ${slBlock}`);
  }

  /** Execute the leg that lands on THIS chain (i.e. the counterparty's bundle). */
  async executeIncomingLeg(counterpartyLeg: LegAnnouncement, finality: unknown): Promise<void> {
    const handler = new ethers.Contract(this.layout.interopHandler, AtomicInteropHandlerAbi, this.wallet);
    const tx = await handler.executeAtomicBundle(counterpartyLeg.bundleData, finality);
    const receipt = await tx.wait();
    const status = await new ethers.Contract(
      this.layout.interopHandler,
      AtomicInteropHandlerAbi,
      this.provider
    ).bundleStatus(counterpartyLeg.bundleHash);
    console.log(`[${this.name}] executed incoming leg (tx status ${receipt!.status}, bundleStatus ${status})`);
    if (Number(status) !== 2) throw new Error(`[${this.name}] bundle not FullyExecuted (status ${status})`);
  }
}

function decodeEvmAddress(interoperable: string): string {
  return ethers.getAddress(ethers.dataSlice(interoperable, ethers.dataLength(interoperable) - 20));
}

// ─────────────────────────────────────────────────────────────────────────────

async function main() {
  const layout = resolveAtomicLayout();
  const l1 = new ethers.JsonRpcProvider(process.env.L1_RPC_URL ?? 'http://127.0.0.1:8545');
  const l1ChainId = (await l1.getNetwork()).chainId;
  const deadline = Number(process.env.ATOMIC_DEADLINE_TS ?? (await l1.getBlock('latest'))!.timestamp + 24 * 3600);

  const alice = new Party('Alice', process.env.L2_RPC_URL!, process.env.PRIVATE_KEY!, layout);
  const bob = new Party(
    'Bob',
    process.env.L2_RPC_URL_SECOND!,
    process.env.COUNTERPARTY_PRIVATE_KEY ?? '0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d',
    layout
  );
  const exchange = new Exchange();
  const amount = ethers.parseUnits('100', 18);
  console.log('=== SETUP (each party, own chain only) ===');
  await alice.setupToken(amount);
  await bob.setupToken(amount);

  // Terms are agreed AFTER each side knows its own token address — in a real flow these would be
  // negotiated off-band ("I'll send you 100 of X on chain A for 100 of Y on chain B").
  exchange.terms = {
    deadline,
    settlementChainId: l1ChainId,
    legA: { sourceChainId: alice.chainId, assetId: alice.assetId, amount, recipient: bob.address },
    legB: { sourceChainId: bob.chainId, assetId: bob.assetId, amount, recipient: alice.address },
  };
  console.log(
    `terms: ${ethers.formatUnits(amount, 18)} of asset ${alice.assetId.slice(0, 12)}… (from ${alice.chainId}) ` +
      `⇄ ${ethers.formatUnits(amount, 18)} of asset ${bob.assetId.slice(0, 12)}… (from ${bob.chainId})`
  );

  console.log('\n=== 1. BUILD + ANNOUNCE LEGS (nothing committed) ===');
  const saltA = randomSalt();
  const saltB = randomSalt();
  await alice.buildAndAnnounceLeg(exchange, bob.chainId, amount, bob.address, saltA);
  await bob.buildAndAnnounceLeg(exchange, alice.chainId, amount, alice.address, saltB);

  console.log('\n=== 2. VERIFY COUNTERPARTY LEG (pure keccak/ABI, no RPC) ===');
  const legA = exchange.readLeg('Alice');
  const legB = exchange.readLeg('Bob');
  // The asset each side expects comes from the agreed terms, NOT from the counterparty's claim —
  // otherwise "which asset" is whatever they say it is.
  alice.verifyCounterpartyLeg(legB, {
    amount: exchange.terms.legB.amount,
    recipient: exchange.terms.legB.recipient,
    assetId: exchange.terms.legB.assetId,
  });
  bob.verifyCounterpartyLeg(legA, {
    amount: exchange.terms.legA.amount,
    recipient: exchange.terms.legA.recipient,
    assetId: exchange.terms.legA.assetId,
  });

  // Both derive the flow independently; identical flowIds mean they agreed on the same flow.
  const { legBundleHashes, chainIds } = sortLegs([
    { bundleHash: legA.bundleHash, chainId: BigInt(legA.chainId) },
    { bundleHash: legB.bundleHash, chainId: BigInt(legB.chainId) },
  ]);
  const preimage = { deadline, settlementLayerChainId: l1ChainId, legBundleHashes, legSourceChainIds: chainIds };
  const flowIdAlice = computeFlowId(legBundleHashes, chainIds, deadline, l1ChainId);
  const flowIdBob = computeFlowId(legBundleHashes, chainIds, deadline, l1ChainId);
  if (flowIdAlice !== flowIdBob) throw new Error('parties derived different flowIds');
  console.log(`both parties derived flowId ${flowIdAlice} ✓`);

  console.log('\n=== 3. COMMIT OWN LEGS ===');
  const aCommit = await alice.commitLeg({
    preimage, flowId: flowIdAlice, myBundleHash: legA.bundleHash,
    destChainId: bob.chainId, amount, recipient: bob.address, salt: saltA,
  });
  const bCommit = await bob.commitLeg({
    preimage, flowId: flowIdBob, myBundleHash: legB.bundleHash,
    destChainId: alice.chainId, amount, recipient: alice.address, salt: saltB,
  });

  console.log('\n=== 4. PUBLISH + VERIFY PROOFS ===');
  const aProof = await alice.fetchOwnProof(aCommit.commitValue, aCommit.sendBlock);
  exchange.publishProof('Alice', aProof.proof);
  const bProof = await bob.fetchOwnProof(bCommit.commitValue, bCommit.sendBlock);
  exchange.publishProof('Bob', bProof.proof);

  alice.verifyCounterpartyProof(exchange.readProof('Bob'), flowIdAlice, legB);
  bob.verifyCounterpartyProof(exchange.readProof('Alice'), flowIdBob, legA);

  console.log('\n=== 5. WAIT FOR INTEROP ROOTS (own chain) ===');
  for (const slBlock of [aProof.slBlock, bProof.slBlock]) {
    await alice.waitForInteropRoot(l1ChainId, slBlock);
    await bob.waitForInteropRoot(l1ChainId, slBlock);
  }
  console.log('interop roots imported on both chains ✓');

  console.log('\n=== 6. EXECUTE INCOMING LEGS ===');
  const byHash = new Map([
    [legA.bundleHash, exchange.readProof('Alice')],
    [legB.bundleHash, exchange.readProof('Bob')],
  ]);
  const finality = atomicFinalityProofTuple({
    flowId: flowIdAlice,
    deadline,
    settlementLayerChainId: l1ChainId,
    legBundleHashes,
    legSourceChainIds: chainIds,
    proofs: legBundleHashes.map((h) => byHash.get(h)!),
  });
  await bob.executeIncomingLeg(legA, finality); // Alice's leg lands on Bob's chain
  await alice.executeIncomingLeg(legB, finality); // Bob's leg lands on Alice's chain

  console.log('\n=== VERIFY MINTS ===');
  for (const [receiver, other] of [[bob, alice], [alice, bob]] as const) {
    const assetId = computeAssetId(other.chainId, L2_NATIVE_TOKEN_VAULT_ADDRESS, other.token);
    const ntv = new ethers.Contract(
      L2_NATIVE_TOKEN_VAULT_ADDRESS,
      [...NativeTokenVaultAbi, 'function tokenAddress(bytes32 _assetId) view returns (address)'],
      receiver.provider
    );
    const wrapped = await ntv.tokenAddress(assetId);
    const bal = await new ethers.Contract(wrapped, ERC20Abi, receiver.provider).balanceOf(receiver.address);
    console.log(`[${receiver.name}] received ${ethers.formatUnits(bal, 18)} of ${other.name}'s token`);
    if (bal < amount) throw new Error(`${receiver.name} did not receive the expected amount`);
  }

  console.log('\nTwo-party atomic swap complete — neither side touched the other chain.');
}

main().catch((err) => {
  console.error('Error:', err);
  process.exit(1);
});
