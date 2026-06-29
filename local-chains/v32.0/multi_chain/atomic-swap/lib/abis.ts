/**
 * ABI definitions for interop contracts
 */

import { ethers } from "ethers";

export const InteropCenterAbi = [
  'function sendBundle(bytes calldata _destinationChainId, tuple(bytes to, bytes data, bytes[] callAttributes)[] calldata _callStarters, bytes[] calldata _bundleAttributes) external payable returns (bytes32)',
  'function sendMessage(bytes calldata recipient, bytes calldata payload, bytes[] calldata attributes) external payable returns (bytes32)',
  'event InteropBundleSent(bytes32 l2l1MsgHash, bytes32 interopBundleHash, tuple(bytes1 version, uint256 sourceChainId, uint256 destinationChainId, bytes32 interopBundleSalt, tuple(bytes1 version, bool shadowAccount, address to, address from, uint256 value, bytes data)[] calls, tuple(bytes executionAddress, bytes unbundlerAddress) bundleAttributes) interopBundle)',
];

export const InteropHandlerAbi = [
  'function executeBundle(bytes memory _bundle, tuple(uint256 chainId, uint256 l1BatchNumber, uint256 l2MessageIndex, tuple(uint16 txNumberInBatch, address sender, bytes data) message, bytes32[] proof) memory _proof) external',
  'function verifyBundle(bytes memory _bundle, tuple(uint256 chainId, uint256 l1BatchNumber, uint256 l2MessageIndex, tuple(uint16 txNumberInBatch, address sender, bytes data) message, bytes32[] proof) memory _proof) external',
  'function unbundleBundle(uint256 _sourceChainId, bytes memory _bundle, uint8[] calldata _providedCallStatus) external',
  'function bundleStatus(bytes32 bundleHash) view returns (uint8)',
  'function getShadowAccountAddress(uint256 _ownerChainId, address _ownerAddress) view returns (address)',
  'event BundleExecuted(bytes32 indexed bundleHash)',
  'event BundleVerified(bytes32 indexed bundleHash)',
  'event BundleUnbundled(bytes32 indexed bundleHash)',
];

export const InteropRootStorageAbi = [
  'function interopRoots(uint256 chainId, uint256 batchNumber) view returns (bytes32)',
];

export const L1MessengerAbi = [
  'function sendToL1(bytes calldata _message) external returns (bytes32)',
  'event L1MessageSent(address indexed _sender, bytes32 indexed _hash, bytes _message)',
];

export const MessageVerificationAbi = [
  'function proveL2MessageInclusionShared(uint256 _chainId, uint256 _blockOrBatchNumber, uint256 _index, tuple(uint16 txNumberInBatch, address sender, bytes data) _message, bytes32[] calldata _proof) external view returns (bool)',
];

export const NativeTokenVaultAbi = [
  'function assetId(address _tokenAddress) view returns (bytes32)',
  'function tokenAddress(bytes32 _assetId) view returns (address)',
  'function ensureTokenIsRegistered(address _nativeToken) returns (bytes32)',
] as const;

export const ERC20Abi = [
  'function balanceOf(address account) view returns (uint256)',
  'function transfer(address to, uint256 value) returns (bool)',
  'function approve(address spender, uint256 value) returns (bool)',
  'function name() view returns (string)',
  'function symbol() view returns (string)',
  'function decimals() view returns (uint8)',
];

export const interopBundleSentInterface = new ethers.Interface(InteropCenterAbi);
export const l1MessengerInterface = new ethers.Interface(L1MessengerAbi);
