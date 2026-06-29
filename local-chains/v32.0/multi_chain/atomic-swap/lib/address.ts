import { ethers } from 'ethers';
import { InteropHandlerAbi } from './abis';
import { L2_INTEROP_HANDLER_ADDRESS } from './constants';

/**
 * ERC-7930 interoperable address encoding utilities
 *
 * Format: version (2 bytes) + chainType (2 bytes) + chainRefLength (1 byte) + chainRef (variable) + addrLength (1 byte) + addr (variable)
 * For EVM: version = 0x0001, chainType = 0x0000
 */

/**
 * Convert a chain ID to its minimal bytes representation
 * @param chainId - The chain ID to convert
 * @returns The minimal bytes representation
 */
export function toChainReference(chainId: bigint): Uint8Array {
  if (chainId === 0n) {
    return new Uint8Array([0]);
  }

  const hex = chainId.toString(16);
  const paddedHex = hex.length % 2 === 0 ? hex : '0' + hex;
  return ethers.getBytes('0x' + paddedHex);
}

/**
 * Format an ERC-7930 interoperable address for EVM chain with address
 * @param chainId - The chain ID
 * @param address - The address on that chain
 * @returns The encoded ERC-7930 address
 */
export function formatEvmV1WithAddress(chainId: bigint | number, address: string): string {
  const chainReference = toChainReference(BigInt(chainId));
  return ethers.concat([
    '0x00010000', // version (0x0001) + chainType (0x0000 for EVM)
    ethers.toBeHex(chainReference.length, 1),
    chainReference,
    ethers.toBeHex(20, 1), // address length
    address,
  ]);
}

/**
 * Format an ERC-7930 interoperable address for EVM chain without address
 * Used for destination chain ID encoding
 * @param chainId - The chain ID
 * @returns The encoded ERC-7930 address without address field
 */
export function formatEvmV1(chainId: bigint | number): string {
  const chainReference = toChainReference(BigInt(chainId));
  return ethers.concat([
    '0x00010000', // version (0x0001) + chainType (0x0000 for EVM)
    ethers.toBeHex(chainReference.length, 1),
    chainReference,
    ethers.toBeHex(0, 1), // address length = 0
  ]);
}

/**
 * Format an ERC-7930 interoperable address with just address (no chain reference)
 * This is used for the 'to' field in InteropCallStarter
 * @param address - The address
 * @returns The encoded ERC-7930 address without chain reference
 */
export function formatEvmV1AddressOnly(address: string): string {
  return ethers.concat([
    '0x000100000014', // version (0x0001) + chainType (0x0000) + chainRefLength (0x00) + addrLength (0x14 = 20)
    address,
  ]);
}

/**
 * Parse an ERC-7930 interoperable address for EVM chain
 * @param encoded - The encoded ERC-7930 address
 * @returns Object with chainId and address
 */
export function parseEvmV1(encoded: string): { chainId: bigint; address: string } {
  const bytes = ethers.getBytes(encoded);

  // Check version and chain type
  if (bytes[0] !== 0x00 || bytes[1] !== 0x01 || bytes[2] !== 0x00 || bytes[3] !== 0x00) {
    throw new Error('Invalid ERC-7930 format: expected EVM v1');
  }

  const chainRefLength = bytes[4];
  const chainRefBytes = bytes.slice(5, 5 + chainRefLength);

  let chainId = 0n;
  for (let i = 0; i < chainRefBytes.length; i++) {
    chainId = (chainId << 8n) | BigInt(chainRefBytes[i]);
  }

  const addrLength = bytes[5 + chainRefLength];
  let address = ethers.ZeroAddress;

  if (addrLength === 20) {
    address = ethers.getAddress(
      ethers.hexlify(bytes.slice(6 + chainRefLength, 6 + chainRefLength + 20))
    );
  }

  return { chainId, address };
}

/**
 * Compute the asset ID for a token using NTV encoding
 * @param chainId - The chain ID where the token originates
 * @param ntvAddress - The Native Token Vault address
 * @param tokenAddress - The token address
 * @returns The computed asset ID
 */
export function computeAssetId(
  chainId: bigint | number,
  ntvAddress: string,
  tokenAddress: string
): string {
  return ethers.keccak256(
    ethers.AbiCoder.defaultAbiCoder().encode(
      ['uint256', 'address', 'address'],
      [chainId, ntvAddress, tokenAddress]
    )
  );
}

/**
 * Get the shadow account address for a user on a destination chain
 * Shadow accounts allow users from one chain to interact with contracts on another chain
 * IMPORTANT: ONLY works in the demo version.
 * @param provider - The provider for the chain where the shadow account exists
 * @param ownerChainId - The chain ID where the owner's account exists
 * @param ownerAddress - The owner's address on their origin chain
 * @returns The shadow account address on the destination chain
 */
export async function getShadowAccountAddress(
  provider: ethers.Provider,
  ownerChainId: bigint,
  ownerAddress: string
): Promise<string> {
  const interopHandler = new ethers.Contract(
    L2_INTEROP_HANDLER_ADDRESS,
    InteropHandlerAbi,
    provider
  );
  return await interopHandler.getShadowAccountAddress(ownerChainId, ownerAddress);
}
