import { ethers } from 'ethers';
import { formatEvmV1, formatEvmV1AddressOnly, formatEvmV1WithAddress, computeAssetId } from './address';
import { InteropCall, InteropCallStarter } from './types';
import { NEW_ENCODING_VERSION, L2_NATIVE_TOKEN_VAULT_ADDRESS, L2_ASSET_ROUTER_ADDRESS } from './constants';

/**
 * Attribute selectors for ERC-7786
 */
export const AttributeSelectors = {
  interopCallValue: ethers.id('interopCallValue(uint256)').substring(0, 10),
  indirectCall: ethers.id('indirectCall(uint256)').substring(0, 10),
  executionAddress: ethers.id('executionAddress(bytes)').substring(0, 10),
  unbundlerAddress: ethers.id('unbundlerAddress(bytes)').substring(0, 10),
  shadowAccount: ethers.id('shadowAccount()').substring(0, 10),
};

/**
 * Builder class for creating interop call starters
 */
export class CallBuilder {
  private _to: string;
  private _data: string;
  private _attributes: string[] = [];

  /**
   * Create a new CallBuilder
   * @param to - Target address on the destination chain
   * @param data - Calldata to send
   */
  constructor(to: string, data: string) {
    this._to = to;
    this._data = data;
  }

  /**
   * Set the interop call value (amount of base token to send with the call)
   * @param value - The value to send
   * @returns this for chaining
   */
  withValue(value: bigint): CallBuilder {
    const encoded = ethers.AbiCoder.defaultAbiCoder().encode(['uint256'], [value]);
    this._attributes.push(AttributeSelectors.interopCallValue + encoded.substring(2));
    return this;
  }

  /**
   * Mark this call as an indirect call (e.g., for bridge operations)
   * @param messageValue - The message value for the indirect call
   * @returns this for chaining
   */
  asIndirectCall(messageValue: bigint = 0n): CallBuilder {
    const encoded = ethers.AbiCoder.defaultAbiCoder().encode(['uint256'], [messageValue]);
    this._attributes.push(AttributeSelectors.indirectCall + encoded.substring(2));
    return this;
  }

  /**
   * Mark this call to be executed via a shadow account on the destination chain.
   * Shadow accounts are deterministic contracts that represent the sender's cross-chain identity,
   * enabling execution on contracts that don't natively support ERC-7786.
   *
   * IMPORTANT: This feature is only available in the demo version.
   * @returns this for chaining
   */
  withShadowAccount(): CallBuilder {
    // shadowAccount() takes no parameters - it's just the selector
    this._attributes.push(AttributeSelectors.shadowAccount);
    return this;
  }

  /**
   * Build the call starter
   * @returns The InteropCallStarter object
   */
  build(): InteropCallStarter {
    return {
      to: formatEvmV1AddressOnly(this._to),
      data: this._data,
      callAttributes: this._attributes,
    };
  }

  /**
   * Create a call for transferring tokens via the Asset Router
   * This is a convenience method that encapsulates:
   * - Computing the asset ID from chain ID and token address
   * - Building the bridge calldata
   * - Creating a InteropCallStarter with indirect call
   *
   * @param sourceChainId - The source chain ID where the token originates
   * @param tokenAddress - The token address on the source chain
   * @param amount - Amount to transfer
   * @param receiver - Receiver address on destination chain
   * @returns An InteropCallStarter configured for token transfer
   *
   * @example
   * ```typescript
   * const call = CallBuilder.tokenTransfer(
   *   sourceChainId,
   *   tokenAddress,
   *   ethers.parseUnits('100', 18),
   *   recipientAddress
   * );
   *
   * const bundle = new BundleBuilder(destChainId)
   *   .addCall(call)
   *   .withUnbundler(unbundlerAddress);
   * ```
   */
  static tokenTransfer(
    sourceChainId: bigint | number,
    tokenAddress: string,
    amount: bigint,
    receiver: string
  ): InteropCallStarter {
    // Compute the asset ID
    const assetId = computeAssetId(
      sourceChainId,
      L2_NATIVE_TOKEN_VAULT_ADDRESS,
      tokenAddress
    );

    // Build the bridge calldata
    const burnData = ethers.AbiCoder.defaultAbiCoder().encode(
      ['uint256', 'address', 'address'],
      [amount, receiver, ethers.ZeroAddress]
    );
    const calldata = ethers.concat([
      NEW_ENCODING_VERSION,
      ethers.AbiCoder.defaultAbiCoder().encode(['bytes32', 'bytes'], [assetId, burnData]),
    ]);

    // Create and return the CallBuilder configured as an indirect call
    return (new CallBuilder(L2_ASSET_ROUTER_ADDRESS, calldata).asIndirectCall(0n)).build();
  }
}

/**
 * Builder class for creating interop bundles
 */
export class BundleBuilder {
  private _destinationChainId: bigint;
  private _calls: InteropCallStarter[] = [];
  private _bundleAttributes: string[] = [];

  /**
   * Create a new BundleBuilder
   * @param destinationChainId - The destination chain ID
   */
  constructor(destinationChainId: bigint | number) {
    this._destinationChainId = BigInt(destinationChainId);
  }

  /**
   * Add a call to the bundle
   * @param call - The call starter to add
   * @returns this for chaining
   */
  addCall(call: InteropCallStarter): BundleBuilder {
    this._calls.push(call);
    return this;
  }

  /**
   * Add a simple call to the bundle
   * @param to - Target address
   * @param data - Calldata
   * @returns this for chaining
   */
  addSimpleCall(to: string, data: string): BundleBuilder {
    this._calls.push(new CallBuilder(to, data).build());
    return this;
  }

  /**
   * Add a call with value to the bundle
   * @param to - Target address
   * @param data - Calldata
   * @param value - Value to send
   * @returns this for chaining
   */
  addCallWithValue(to: string, data: string, value: bigint): BundleBuilder {
    this._calls.push(new CallBuilder(to, data).withValue(value).build());
    return this;
  }

  /**
   * Add a call that executes via shadow account on the destination chain.
   * Shadow accounts enable calling contracts that don't support ERC-7786 natively.
   *
   * IMPORTANT: This feature is only available in the demo version.
   * @param to - Target address
   * @param data - Calldata
   * @param value - Optional value to send
   * @returns this for chaining
   */
  addShadowAccountCall(to: string, data: string, value?: bigint): BundleBuilder {
    const builder = new CallBuilder(to, data).withShadowAccount();
    if (value !== undefined && value > 0n) {
      builder.withValue(value);
    }
    this._calls.push(builder.build());
    return this;
  }

  /**
   * Set the unbundler address (who can unbundle the bundle on destination)
   * @param address - The unbundler address
   * @param chainId - Optional chain ID (if not set, any chain id is allowed)
   * @returns this for chaining
   */
  withUnbundler(address: string, chainId?: bigint | number): BundleBuilder {
    const encoded = chainId
      ? formatEvmV1WithAddress(chainId, address)
      : formatEvmV1AddressOnly(address);
    const attributeEncoded = ethers.AbiCoder.defaultAbiCoder().encode(['bytes'], [encoded]);
    this._bundleAttributes.push(AttributeSelectors.unbundlerAddress + attributeEncoded.substring(2));
    return this;
  }

  /**
   * Set the execution address (who can execute the bundle on destination)
   * @param address - The execution address
   * @param chainId - Optional chain ID (if not set, uses current chain ID)
   * @returns this for chaining
   */
  withExecutor(address: string, chainId?: bigint | number): BundleBuilder {
    const encoded = chainId
      ? formatEvmV1WithAddress(chainId, address)
      : formatEvmV1AddressOnly(address);
    const attributeEncoded = ethers.AbiCoder.defaultAbiCoder().encode(['bytes'], [encoded]);
    this._bundleAttributes.push(AttributeSelectors.executionAddress + attributeEncoded.substring(2));
    return this;
  }

  /**
   * Get the encoded destination chain ID
   * @returns The ERC-7930 encoded destination chain ID
   */
  getEncodedDestination(): string {
    return formatEvmV1(this._destinationChainId);
  }

  /**
   * Get the calls array
   * @returns The array of call starters
   */
  getCalls(): InteropCallStarter[] {
    return this._calls;
  }

  /**
   * Get the bundle attributes array
   * @returns The array of bundle attributes
   */
  getBundleAttributes(): string[] {
    return this._bundleAttributes;
  }
}

/**
 * Helper to encode bridge burn data for asset transfers
 * @param amount - Amount to transfer
 * @param receiver - Receiver address
 * @param maybeTokenAddress - Optional token address (use ZeroAddress if not needed)
 * @returns Encoded burn data
 */
export function encodeBridgeBurnData(
  amount: bigint,
  receiver: string,
  maybeTokenAddress: string = ethers.ZeroAddress
): string {
  return ethers.AbiCoder.defaultAbiCoder().encode(
    ['uint256', 'address', 'address'],
    [amount, receiver, maybeTokenAddress]
  );
}

/**
 * Helper to encode asset router bridgehub deposit data
 * @param assetId - The asset ID
 * @param transferData - The transfer data (from encodeBridgeBurnData)
 * @returns Encoded deposit data
 */
export function encodeAssetRouterDepositData(assetId: string, transferData: string): string {
  return ethers.concat([
    NEW_ENCODING_VERSION,
    ethers.AbiCoder.defaultAbiCoder().encode(['bytes32', 'bytes'], [assetId, transferData]),
  ]);
}

/**
 * Build calldata for a token bridge transfer via interop
 * @param assetId - The asset ID of the token
 * @param amount - Amount to transfer
 * @param receiver - Receiver address on destination chain
 * @param maybeTokenAddress - Optional token address
 * @returns The encoded calldata for the bridge
 */
export function buildBridgeCalldata(
  assetId: string,
  amount: bigint,
  receiver: string,
  maybeTokenAddress: string = ethers.ZeroAddress
): string {
  const burnData = encodeBridgeBurnData(amount, receiver, maybeTokenAddress);
  return encodeAssetRouterDepositData(assetId, burnData);
}

/**
 * Create a bundle for transferring tokens via the asset router
 * @param destinationChainId - Destination chain ID
 * @param assetId - Asset ID of the token
 * @param amount - Amount to transfer
 * @param receiver - Receiver address
 * @param unbundler - Unbundler address
 * @returns A configured BundleBuilder
 */
export function createTokenTransferBundle(
  destinationChainId: bigint | number,
  assetId: string,
  amount: bigint,
  receiver: string,
  unbundler: string
): BundleBuilder {
  const calldata = buildBridgeCalldata(assetId, amount, receiver);

  const call = new CallBuilder(L2_ASSET_ROUTER_ADDRESS, calldata).asIndirectCall(0n).build();

  return new BundleBuilder(destinationChainId).addCall(call).withUnbundler(unbundler);
}
