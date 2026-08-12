import { ethers } from 'ethers';

/**
 * Source chain status for a bundle/message
 */
export enum SourceChainStatus {
  /** Transaction was not processed on the source chain */
  Unknown = 'Unknown',
  /** Transaction has been processed, but the bundle/message hash is not emitted there */
  Invalid = 'Invalid',
  /** Transaction's block has not been finalized by the source chain */
  SourceUnfinalized = 'SourceUnfinalized',
  /** Transaction's block has been finalized by the source chain */
  SourceFinalized = 'SourceFinalized',
}

/**
 * Destination chain status for a bundle
 */
export enum DestinationChainStatus {
  /** Root is not yet available on the chain */
  Unavailable = 'Unavailable',
  /** Bundle has not been received yet */
  Unreceived = 'Unreceived',
  /** Bundle has been verified but not executed */
  Verified = 'Verified',
  /** Bundle has been fully executed */
  FullyExecuted = 'FullyExecuted',
  /** Bundle has been unbundled (partially executed) */
  Unbundled = 'Unbundled',
}

/**
 * On-chain BundleStatus enum (matches Solidity)
 */
export enum BundleStatus {
  Unreceived = 0,
  Verified = 1,
  FullyExecuted = 2,
  Unbundled = 3,
}

/**
 * Expected root information for cross-chain verification
 */
export interface ExpectedRoot {
  rootChainId: bigint;
  batchNumber: number;
  expectedRoot: string;
}

/**
 * L2 message structure for proofs
 */
export interface L2Message {
  txNumberInBatch: number;
  sender: string;
  data: string;
}

/**
 * Message inclusion proof structure
 */
export interface MessageInclusionProof {
  chainId: bigint;
  l1BatchNumber: number;
  l2MessageIndex: number;
  message: L2Message;
  proof: string[];
}

/**
 * Information about a finalized interop message/bundle
 */
export interface InteropMessageFinalizationInfo {
  /** The expected root data for destination chain verification */
  expectedRoot: ExpectedRoot;
  /** The inclusion proof for the message */
  proof: MessageInclusionProof;
  /** The encoded bundle or message data */
  encodedData: string;
}

/**
 * Handle to track a bundle
 */
export interface BundleHandle {
  /** Hash of the bundle */
  bundleHash: string;
  /** Hash of the transaction that sent the bundle */
  txHash: string;
  /** Source chain ID */
  sourceChainId: bigint;
  /** Block number where the transaction was included */
  blockNumber: number;
}

/**
 * Information about a bundle extracted from a transaction receipt
 * Used for tracking and executing bundles
 */
export interface BundleInfo {
  /** Bundle handle for tracking */
  bundleHandle: BundleHandle;
  /** L1 message hash from L1MessageSent event */
  l1MessageHash: string;
  /** Log index of the InteropBundleSent event */
  l1LogIndex: number;
}

/**
 * Handle to track a message
 */
export interface MessageHandle {
  /** Hash of the message */
  messageHash: string;
  /** Hash of the transaction that sent the message */
  txHash: string;
  /** Source chain ID */
  sourceChainId: bigint;
  /** Block number where the transaction was included */
  blockNumber: number;
}

/**
 * Interop call starter for building bundles
 */
export interface InteropCallStarter {
  /** Target address (will be encoded to ERC-7930 format) */
  to: string;
  /** Calldata */
  data: string;
  /** Call attributes */
  callAttributes: string[];
}

/**
 * Interop call (after processing by InteropCenter)
 */
export interface InteropCall {
  version: string;
  shadowAccount: boolean;
  to: string;
  from: string;
  value: bigint;
  data: string;
}

/**
 * Bundle attributes
 */
export interface BundleAttributesStruct {
  executionAddress: string;
  unbundlerAddress: string;
}

/**
 * Full interop bundle structure
 */
export interface InteropBundle {
  version: string;
  sourceChainId: bigint;
  destinationChainId: bigint;
  interopBundleSalt: string;
  calls: InteropCall[];
  bundleAttributes: BundleAttributesStruct;
}

/**
 * Options for sending a bundle
 */
export interface SendBundleOptions {
  /** Gas limit for the transaction */
  gasLimit?: bigint;
  /** Max fee per gas */
  maxFeePerGas?: bigint;
  /** Max priority fee per gas */
  maxPriorityFeePerGas?: bigint;
  /** Gas price (legacy) */
  gasPrice?: bigint;
  /** Value to send with the transaction */
  value?: bigint;
}

/**
 * Options for executing a bundle
 */
export interface ExecuteBundleOptions {
  /** Gas limit for the transaction */
  gasLimit?: bigint;
  /** Gas price */
  gasPrice?: bigint;
}

/**
 * Options for waiting operations
 */
export interface WaitOptions {
  /** Poll interval in milliseconds */
  pollInterval?: number;
  /** Timeout in milliseconds */
  timeout?: number;
}

/**
 * Options for event searching operations
 */
export interface EventSearchOptions {
  /** Number of blocks to search per chunk (default: 1000) */
  chunkSize?: number;
  /** Maximum number of blocks to search backwards (default: 50000) */
  maxBlocksBack?: number;
}

/**
 * Provider type - uses standard ethers.Provider
 */
export type AnyProvider = ethers.Provider;

/**
 * Signer type
 */
export type AnySigner = ethers.Signer;

/**
 * Extended LogProof type that includes batch_number from the actual API response
 * The zksync RPC endpoint returns this field but standard ethers types don't include it
 */
export interface ExtendedLogProof {
  id: number;
  proof: string[];
  root: string;
  batch_number: number;
}
