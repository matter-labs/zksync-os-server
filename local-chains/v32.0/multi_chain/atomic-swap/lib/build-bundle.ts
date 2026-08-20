import { ethers } from 'ethers';
import { INTEROP_BUNDLE_TUPLE } from './atomic';
import { AttributeSelectors } from './bundle-builder';
import { formatEvmV1, formatEvmV1AddressOnly, formatEvmV1WithAddress } from './address';

/**
 * Build the exact `InteropBundle` a `sendBundle` would produce — WITHOUT sending it.
 *
 * Why this exists: in a two-party flow each side must show the other what its leg actually does
 * before either commits. `sendBundle` returns only the bundle hash, and the bundle itself is
 * emitted in `InteropBundleSent`, so `eth_call` alone can't give you the bytes and
 * `eth_simulateV1` (which returns logs) may be disabled on private chains. This reconstructs the
 * struct locally, using a single read-only `eth_call` for the one part the chain owns.
 *
 * Determinism: every field is fixed by the caller's inputs except `destinationBaseTokenAssetId`,
 * which is read from this chain's L2 Bridgehub. That value is set once by `registerChain` and is
 * constant afterwards, so once both chains are registered the result is byte-identical to what
 * `sendBundle` will emit. Before registration `sendBundle` reverts with `DestinationChainNotRegistered`
 * rather than producing a different bundle — a loud failure, not a silent mismatch.
 */

const INTEROP_BUNDLE_VERSION = '0x01';
const INTEROP_CALL_VERSION = '0x01';
const L2_BRIDGEHUB_ADDRESS = '0x0000000000000000000000000000000000010002';

/** A call as handed to `sendBundle`, before the InteropCenter rewrites it. */
export interface CallStarterInput {
  /** Target on the destination chain (plain EVM address). */
  to: string;
  data: string;
  /** `interopCallValue` attribute — value delivered on the destination side. */
  interopCallValue?: bigint;
  /**
   * `indirectCall` attribute. When set, the InteropCenter calls
   * `IL2CrossChainSender(to).initiateIndirectCall(...)` and takes the REWRITTEN call from its
   * return value — so `to`/`data` in the final bundle are produced on-chain, not by the caller.
   * Token transfers via the asset router are always indirect.
   */
  indirectCall?: boolean;
  /** `indirectCall`'s message value (source-side), 0 for the asset-router path. */
  indirectCallMessageValue?: bigint;
}

export interface BuildBundleParams {
  provider: ethers.Provider;
  /** The account that will call `sendBundle` — becomes `msg.sender`, and binds the salt. */
  sender: string;
  destinationChainId: bigint | number;
  calls: CallStarterInput[];
  /** User-chosen salt for the `interopBundleSalt` attribute. */
  salt: string;
  /** Bundle attributes, all defaulting to the values an unadorned `sendBundle` produces. */
  executionAddress?: string;
  unbundlerAddress?: string;
  useFixedFee?: boolean;
  /** Override the InteropCenter address (defaults to the atomic layout's 0x…1000d). */
  interopCenter?: string;
  /**
   * Cross-check the reconstructed hash against the contract's own `sendBundle` staticCall
   * (default true). This turns a silent reconstruction bug — or an era-contracts encoding change —
   * into an immediate throw, so a wrong bundle can never be shared with a counterparty.
   * Costs one extra `eth_call` on your own chain.
   */
  verifyAgainstChain?: boolean;
  /**
   * `msg.value` to use for that cross-check. Defaults to `interopProtocolFee * callCount` plus any
   * call values; override if your chain's fee accounting differs, since a wrong value reverts with
   * `MsgValueMismatch` and would be misread as a reconstruction failure.
   */
  sendValue?: bigint;
}

export interface BuiltBundle {
  /** ABI-encoded `InteropBundle` — the bytes `executeAtomicBundle` consumes verbatim. */
  bundleData: string;
  /** `keccak256(abi.encode(sourceChainId, bundleData))`, as `InteropDataEncoding` computes it. */
  bundleHash: string;
  /** Decoded form, for inspection/logging. */
  bundle: {
    version: string;
    sourceChainId: bigint;
    destinationChainId: bigint;
    destinationBaseTokenAssetId: string;
    interopBundleSalt: string;
    calls: { version: string; shadowAccount: boolean; to: string; from: string; value: bigint; data: string }[];
    bundleAttributes: { executionAddress: string; unbundlerAddress: string; useFixedFee: boolean; salt: string };
  };
}

const BRIDGEHUB_ABI = ['function baseTokenAssetId(uint256 _chainId) view returns (bytes32)'];
const SEND_BUNDLE_ABI = [
  'function sendBundle(bytes calldata _destinationChainId, tuple(bytes to, bytes data, bytes[] callAttributes)[] calldata _callStarters, bytes[] calldata _bundleAttributes) external payable returns (bytes32)',
];
const CROSS_CHAIN_SENDER_ABI = [
  'function initiateIndirectCall(uint256 _chainId, address _originalCaller, uint256 _value, bytes calldata _data) payable returns (tuple(bytes to, bytes data, bytes[] callAttributes) interopCallStarter)',
];

/**
 * Resolve an indirect call the way `_processCallStarter` does: ask the target (e.g. the L2 asset
 * router) what call it would produce. `initiateIndirectCall` is state-changing and gated by
 * `onlyL2InteropCenter`, so this is an `eth_call` impersonating the InteropCenter — no state is
 * written and nothing is broadcast.
 */
async function resolveIndirectCall(
  provider: ethers.Provider,
  interopCenter: string,
  call: CallStarterInput,
  destinationChainId: bigint,
  sender: string
): Promise<{ to: string; data: string }> {
  const iface = new ethers.Interface(CROSS_CHAIN_SENDER_ABI);
  const raw = await provider.call({
    to: call.to,
    from: interopCenter, // satisfies onlyL2InteropCenter
    value: call.indirectCallMessageValue ?? 0n,
    data: iface.encodeFunctionData('initiateIndirectCall', [
      destinationChainId,
      sender,
      call.interopCallValue ?? 0n,
      call.data,
    ]),
  });
  const [starter] = iface.decodeFunctionResult('initiateIndirectCall', raw);
  // `to` comes back as an ERC-7930 interoperable address; the bundle stores the plain EVM address.
  const to = ethers.dataSlice(starter.to, ethers.dataLength(starter.to) - 20);
  return { to: ethers.getAddress(to), data: starter.data };
}

export async function buildBundle(params: BuildBundleParams): Promise<BuiltBundle> {
  const {
    provider,
    sender,
    calls,
    salt,
    executionAddress = '0x',
    unbundlerAddress = '0x',
    useFixedFee = false,
    interopCenter = '0x000000000000000000000000000000000001000d',
  } = params;

  const destinationChainId = BigInt(params.destinationChainId);
  const sourceChainId = BigInt((await provider.getNetwork()).chainId);

  // The single chain-derived field. Zero means `registerChain` has not run for this destination,
  // and `sendBundle` would revert with DestinationChainNotRegistered — fail loudly here instead.
  const destinationBaseTokenAssetId: string = await new ethers.Contract(
    L2_BRIDGEHUB_ADDRESS,
    BRIDGEHUB_ABI,
    provider
  ).baseTokenAssetId(destinationChainId);
  if (destinationBaseTokenAssetId === ethers.ZeroHash) {
    throw new Error(
      `destination chain ${destinationChainId} is not registered on chain ${sourceChainId} ` +
        `(baseTokenAssetId is zero) — run Bridgehub.chainRegistrationSender().registerChain first`
    );
  }

  const interopBundleSalt = ethers.keccak256(
    ethers.solidityPacked(['address', 'bytes32'], [sender, salt])
  );

  // InteropCenter does NOT leave an omitted unbundlerAddress empty: it defaults to the sender as an
  // ERC-7930 address on THIS chain (deliberately pinning the source chain, so a same-address clone
  // elsewhere cannot unbundle). Mirror that, or the reconstructed bundle differs in this field alone.
  const effectiveUnbundler =
    unbundlerAddress && unbundlerAddress !== '0x'
      ? unbundlerAddress
      : formatEvmV1WithAddress(sourceChainId, sender);

  const builtCalls = [];
  for (const call of calls) {
    const value = call.interopCallValue ?? 0n;
    if (call.indirectCall) {
      const { to, data } = await resolveIndirectCall(
        provider,
        interopCenter,
        call,
        destinationChainId,
        sender
      );
      // Note `from` is the indirect target (the asset router), NOT the sender.
      builtCalls.push({ version: INTEROP_CALL_VERSION, shadowAccount: false, to, from: call.to, value, data });
    } else {
      builtCalls.push({
        version: INTEROP_CALL_VERSION,
        shadowAccount: false,
        to: ethers.getAddress(call.to),
        from: ethers.getAddress(sender),
        value,
        data: call.data,
      });
    }
  }

  const bundle = {
    version: INTEROP_BUNDLE_VERSION,
    sourceChainId,
    destinationChainId,
    destinationBaseTokenAssetId,
    interopBundleSalt,
    calls: builtCalls,
    bundleAttributes: { executionAddress, unbundlerAddress: effectiveUnbundler, useFixedFee, salt },
  };

  const bundleData = ethers.AbiCoder.defaultAbiCoder().encode(
    [INTEROP_BUNDLE_TUPLE],
    [
      [
        bundle.version,
        bundle.sourceChainId,
        bundle.destinationChainId,
        bundle.destinationBaseTokenAssetId,
        bundle.interopBundleSalt,
        builtCalls.map((c) => [c.version, c.shadowAccount, c.to, c.from, c.value, c.data]),
        [executionAddress, effectiveUnbundler, useFixedFee, salt],
      ],
    ]
  );

  const bundleHash = ethers.keccak256(
    ethers.AbiCoder.defaultAbiCoder().encode(['uint256', 'bytes'], [sourceChainId, bundleData])
  );

  if (params.verifyAgainstChain !== false) {
    const onchain = await predictHashOnChain({
      provider,
      interopCenter,
      sender,
      destinationChainId,
      calls,
      salt,
      executionAddress,
      unbundlerAddress,
      useFixedFee,
      sendValue: params.sendValue,
    });
    if (onchain.toLowerCase() !== bundleHash.toLowerCase()) {
      throw new Error(
        `buildBundle reconstruction diverged from the contract: local ${bundleHash} != sendBundle ${onchain}. ` +
          `The InteropBundle encoding or _processCallStarter logic has changed — do NOT share this bundle.`
      );
    }
  }

  return { bundleData, bundleHash, bundle };
}

/**
 * The contract's own answer for this bundle's hash: `sendBundle` via `eth_call`, nothing sent.
 * Ground truth for the reconstruction — the same thing the demo driver's `predictBundleHash` does,
 * expressed at the raw level so `from` can be set without holding the sender's key.
 */
export async function predictHashOnChain(params: {
  provider: ethers.Provider;
  interopCenter: string;
  sender: string;
  destinationChainId: bigint;
  calls: CallStarterInput[];
  salt: string;
  executionAddress?: string;
  unbundlerAddress?: string;
  useFixedFee?: boolean;
  sendValue?: bigint;
}): Promise<string> {
  const iface = new ethers.Interface(SEND_BUNDLE_ABI);

  const callStarters = params.calls.map((c) => {
    const attrs: string[] = [];
    if (c.indirectCall) {
      attrs.push(
        AttributeSelectors.indirectCall +
          ethers.AbiCoder.defaultAbiCoder()
            .encode(['uint256'], [c.indirectCallMessageValue ?? 0n])
            .substring(2)
      );
    }
    if ((c.interopCallValue ?? 0n) !== 0n) {
      attrs.push(
        AttributeSelectors.interopCallValue +
          ethers.AbiCoder.defaultAbiCoder().encode(['uint256'], [c.interopCallValue]).substring(2)
      );
    }
    return { to: formatEvmV1AddressOnly(c.to), data: c.data, callAttributes: attrs };
  });

  let value = params.sendValue;
  if (value === undefined) {
    const fee: bigint = await new ethers.Contract(
      params.interopCenter,
      ['function interopProtocolFee() view returns (uint256)'],
      params.provider
    ).interopProtocolFee();
    value =
      fee * BigInt(params.calls.length) +
      params.calls.reduce((acc, c) => acc + (c.interopCallValue ?? 0n) + (c.indirectCallMessageValue ?? 0n), 0n);
  }

  const raw = await params.provider.call({
    to: params.interopCenter,
    from: params.sender,
    value,
    data: iface.encodeFunctionData('sendBundle', [
      formatEvmV1(params.destinationChainId),
      callStarters.map((s) => [s.to, s.data, s.callAttributes]),
      bundleAttributesFor({
        salt: params.salt,
        executionAddress: params.executionAddress,
        unbundlerAddress: params.unbundlerAddress,
        useFixedFee: params.useFixedFee,
      }),
    ]),
  });
  const [hash] = iface.decodeFunctionResult('sendBundle', raw);
  return hash;
}

/**
 * The attributes to pass to `sendBundle` so it produces exactly the bundle `buildBundle` returned.
 * Keep these in lockstep: any divergence changes the bundle hash and breaks the agreed flowId.
 */
export function bundleAttributesFor(params: {
  salt: string;
  executionAddress?: string;
  unbundlerAddress?: string;
  useFixedFee?: boolean;
}): string[] {
  const attrs: string[] = [];
  if (params.executionAddress && params.executionAddress !== '0x') {
    attrs.push(
      AttributeSelectors.executionAddress +
        ethers.AbiCoder.defaultAbiCoder()
          .encode(['bytes'], [formatEvmV1AddressOnly(params.executionAddress)])
          .substring(2)
    );
  }
  if (params.unbundlerAddress && params.unbundlerAddress !== '0x') {
    attrs.push(
      AttributeSelectors.unbundlerAddress +
        ethers.AbiCoder.defaultAbiCoder()
          .encode(['bytes'], [formatEvmV1AddressOnly(params.unbundlerAddress)])
          .substring(2)
    );
  }
  if (params.useFixedFee) {
    attrs.push(
      ethers.id('useFixedFee(bool)').substring(0, 10) +
        ethers.AbiCoder.defaultAbiCoder().encode(['bool'], [true]).substring(2)
    );
  }
  attrs.push(
    ethers.id('interopBundleSalt(bytes32)').substring(0, 10) +
      ethers.AbiCoder.defaultAbiCoder().encode(['bytes32'], [params.salt]).substring(2)
  );
  return attrs;
}
