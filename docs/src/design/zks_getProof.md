# `zks_getProof`

Returns a Merkle proof for a given account storage slot, verifiable against the L1 batch commitment.

## Parameters

| # | Name | Type | Description |
|---|------|------|-------------|
| 1 | `address` | `Address` | The account address. |
| 2 | `keys` | `H256[]` | Array of storage keys to prove. |
| 3 | `l1BatchNumber` | `uint64` | The L1 batch number against which the proof should be generated. The proof is for the state **after** this batch. |

## Response

```json
{
  "address": "0x...",
  "stateCommitmentPreimage": {
    "nextFreeSlot": "0x...",
    "blockNumber": "0x...",
    "last256BlockHashesBlake": "0x...",
    "lastBlockTimestamp": "0x..."
  },
  "storageProofs": [
    {
      "key": "0x...",
      "proof": { ... }
    }
  ],
  "l1VerificationData": {
    "batchNumber": 2,
    "numberOfLayer1Txs": 0,
    "priorityOperationsHash": "0x...",
    "dependencyRootsRollingHash": "0x...",
    "l2ToL1LogsRootHash": "0x...",
    "commitment": "0x..."
  }
}
```

### `address`

The account address, as provided in the request. Included in the response so the verifier can derive the flat storage key (`blake2s(address_padded32_be || key)`) without external context.

### `stateCommitmentPreimage`

The preimage fields needed to recompute the L1 state commitment from the Merkle root. These are constant per batch and shared across all storage proofs in the response.

| Field | Type | Description |
|-------|------|-------------|
| `nextFreeSlot` | `uint64` | The next available leaf index in the state tree after this batch. Part of the tree commitment. |
| `blockNumber` | `uint64` | The last L2 block number in this batch. |
| `last256BlockHashesBlake` | `H256` | `blake2s` of the concatenation of the last 256 block hashes (each as 32 bytes). |
| `lastBlockTimestamp` | `uint64` | Timestamp of the last L2 block in this batch. |

### `storageProofs[i]`

Each entry corresponds to one requested storage slot.

| Field | Type | Description |
|-------|------|-------------|
| `key` | `H256` | The storage slot (as provided in the input). The verifier derives the tree key as `blake2s(address_padded32_be || key)`. |
| `proof` | `object` | The proof object. The `type` field discriminates between existing and non-existing proofs (see below). |

The `proof` object always contains a `type` field:

- `"existing"` — the slot exists in the tree. Additional fields: `index`, `value`, `nextIndex`, `siblings`.
- `"nonExisting"` — the slot has never been written to (value is implicitly zero). Additional fields: `leftNeighbor`, `rightNeighbor`.

#### `proof` when `type` = `"existing"`

Returned when the storage slot has been written to at least once.

| Field | Type | Description |
|-------|------|-------------|
| `type` | `string` | `"existing"` |
| `index` | `uint64` | The leaf index in the tree. |
| `value` | `H256` | The storage value. |
| `nextIndex` | `uint64` | The linked-list pointer to the next leaf (by key order). |
| `siblings` | `H256[]` | The Merkle path (see [Siblings](#siblings) below). |

The leaf key used in the tree is not included explicitly — the verifier derives it as `blake2s(address_padded32_be || key)` from the `address` and `key` fields in the response.

#### `proof` when `type` = `"nonExisting"`

Returned when the storage slot has never been written to (value is implicitly zero). Proves non-membership by showing two consecutive leaves in the key-sorted linked list that bracket the queried key.

| Field | Type | Description |
|-------|------|-------------|
| `type` | `string` | `"nonExisting"` |
| `leftNeighbor` | `LeafWithProof` | The leaf with the largest key smaller than the queried key. |
| `rightNeighbor` | `LeafWithProof` | The leaf with the smallest key larger than the queried key. `leftNeighbor.nextIndex` must equal `rightNeighbor.index`. |

#### `LeafWithProof`

Used within non-existing proofs to represent a neighbor leaf and its Merkle path.

| Field | Type | Description |
|-------|------|-------------|
| `index` | `uint64` | The leaf index in the tree. |
| `leafKey` | `H256` | The leaf's key (the `blake2s` derived flat storage key). |
| `value` | `H256` | The leaf's value. |
| `nextIndex` | `uint64` | The linked-list pointer to the next leaf. |
| `siblings` | `H256[]` | The Merkle path (see [Siblings](#siblings) below). |

### `l1VerificationData`

The remaining fields of `StoredBatchInfo` that, together with the state commitment derived from the proof, allow the caller to reconstruct the full struct and verify it against L1:

```solidity
struct StoredBatchInfo {
    uint64  batchNumber;                  // l1VerificationData
    bytes32 batchHash;                    // = stateCommitment (derived from proof)
    uint64  indexRepeatedStorageChanges;   // always 0 (ZKsync OS)
    uint256 numberOfLayer1Txs;            // l1VerificationData
    bytes32 priorityOperationsHash;       // l1VerificationData
    bytes32 dependencyRootsRollingHash;   // l1VerificationData
    bytes32 l2ToL1LogsRootHash;           // l1VerificationData
    uint256 timestamp;                    // always 0 (ZKsync OS)
    bytes32 commitment;                   // l1VerificationData
}
```

| Field | Type | Description |
|-------|------|-------------|
| `batchNumber` | `uint64` | The L1 batch number. |
| `numberOfLayer1Txs` | `uint256` | Number of priority (L1 → L2) transactions in this batch. |
| `priorityOperationsHash` | `H256` | Rolling hash of priority operations. |
| `dependencyRootsRollingHash` | `H256` | Rolling hash of dependency roots. |
| `l2ToL1LogsRootHash` | `H256` | Root hash of L2 → L1 log Merkle tree. |
| `commitment` | `H256` | Batch auxiliary commitment. |

Two fields of `StoredBatchInfo` are fixed constants in ZKsync OS and therefore omitted from the response: `indexRepeatedStorageChanges` is always `0` and `timestamp` is always `0`.

## Tree Structure

The state tree is a fixed-depth (64) binary Merkle tree using Blake2s-256 as the hash function. Leaves are allocated left-to-right by insertion order and linked together in a sorted linked list by key.

### Key derivation

The flat storage key for a slot is derived as:

```
flat_key = blake2s(address_padded32_be || storage_key)
```

where `address_padded32_be` is the 20-byte address zero-padded on the left to 32 bytes.

### Leaf hashing

```
leaf_hash = blake2s(key || value || next_index_le8)
```

where `key` and `value` are 32 bytes each, and `next_index_le8` is the `next` pointer encoded as 8 bytes little-endian.

An empty (unoccupied) leaf has `key = 0`, `value = 0`, `next = 0`.

### Node hashing

```
node_hash = blake2s(left_child_hash || right_child_hash)
```

### Siblings

The `siblings` array is an ordered list of sibling hashes forming the Merkle path from leaf to root.

**Order.** `siblings[0]` is the sibling at the leaf level (depth 64). Subsequent entries move toward the root. A full (uncompressed) path has 64 entries, with the last entry being the sibling at depth 1 (one level below the root). At each level, if the current index is even the node is a left child; if odd it is a right child. The index is halved (integer division) after each level.

**Empty subtree compression.** The tree has depth 64 but is sparsely populated — most subtrees are entirely empty. The hash of an empty subtree at each level is deterministic:

```
emptyHash[0] = blake2s(0x00{32} || 0x00{32} || 0x00{8})    // empty leaf hash (72 zero bytes)
emptyHash[i] = blake2s(emptyHash[i-1] || emptyHash[i-1])    // for i = 1..63
```

If trailing siblings (toward the root) are equal to the corresponding `emptyHash` for that level, they are omitted. The verifier reconstructs them: if `siblings` has fewer than 64 entries, the missing entries at positions `len(siblings)` through `63` are filled with `emptyHash[len(siblings)]`, `emptyHash[len(siblings)+1]`, etc.

For example, if a leaf is at index 5 in a tree with 100 occupied leaves, siblings at levels ~7 and above will all be empty subtree hashes, so the array will contain only ~7 entries instead of 64.

## Verification

```coq
deriveFlatKey (address, storageKey) → H256 :=
    blake2s(leftPad32(address) || storageKey)

hashLeaf (leafKey, value, nextIndex) → H256 :=
    blake2s(leafKey || value || nextIndex.to_le_bytes(8))

emptyHash (0) → H256 := blake2s(0x00{72})
emptyHash (i) → H256 := blake2s(emptyHash(i-1) || emptyHash(i-1))

padSiblings (siblings) → H256[64] :=
    siblings ++ [emptyHash(i) for i in len(siblings)..63]

walkMerklePath (leafHash, index, siblings) → H256 :=
    fullPath ← padSiblings(siblings)
    current ← leafHash
    idx ← index
    for sibling in fullPath:
        current ← if even(idx) then blake2s(current || sibling)
                                else blake2s(sibling || current)
        idx ← idx / 2
    assert idx = 0
    current

verifyExistingProof (address, storageProof) → (H256, H256) :=
    let flatKey = deriveFlatKey(address, storageProof.key) in
    let p = storageProof.proof in
    let stateRoot = walkMerklePath(hashLeaf(flatKey, p.value, p.nextIndex),
                                   p.index, p.siblings) in
    (stateRoot, p.value)

verifyNonExistingProof (address, storageProof) → (H256, H256) :=
    let flatKey = deriveFlatKey(address, storageProof.key) in
    let left = storageProof.proof.leftNeighbor in
    let right = storageProof.proof.rightNeighbor in
    let leftRoot = walkMerklePath(hashLeaf(left.leafKey, left.value, left.nextIndex),
                                  left.index, left.siblings) in
    let rightRoot = walkMerklePath(hashLeaf(right.leafKey, right.value, right.nextIndex),
                                   right.index, right.siblings) in
    assert leftRoot = rightRoot
    assert left.leafKey < flatKey < right.leafKey
    assert left.nextIndex = right.index
    (leftRoot, 0x00{32})

computeStateCommitment (stateRoot, preimage) → H256 :=
    blake2s(
        stateRoot
        || preimage.nextFreeSlot.to_be_bytes(8)
        || preimage.blockNumber.to_be_bytes(8)
        || preimage.last256BlockHashesBlake
        || preimage.lastBlockTimestamp.to_be_bytes(8)
    )
```

### Full verification

A client verifies a storage proof end-to-end in three steps:

1. **Verify the Merkle proof** — walk `storageProofs` to recover the tree root, hash it with `stateCommitmentPreimage` to get the `stateCommitment`.

2. **Reconstruct `StoredBatchInfo`** — place `stateCommitment` into the `batchHash` field, fill the remaining fields from `l1VerificationData`, set `indexRepeatedStorageChanges = 0` and `timestamp = 0`.

3. **Compare against L1** — compute `keccak256(abi.encode(StoredBatchInfo))` and compare with the hash fetched from L1 by calling `storedBatchHash(batchNumber)` on the diamond proxy contract. This is a single `eth_call`, no event scanning required.

If the hashes match, the storage values are proven to be part of the state committed on L1.

```coq
verify (response, onChainHash) :=
    -- onChainHash = diamondProxy.storedBatchHash(batchNumber)  (fetched by caller via eth_call)

    -- Step 1: verify each Merkle proof and collect the tree root
    let stateRoots = []
    forall storageProof in response.storageProofs:
        let (stateRoot, value) =
            match storageProof.proof.type with
            | "existing"    => verifyExistingProof(response.address, storageProof)
            | "nonExisting" => verifyNonExistingProof(response.address, storageProof)
        in
        stateRoots.append(stateRoot)

    -- All proofs must agree on the same tree root
    assert all elements of stateRoots are equal
    let stateRoot = stateRoots[0]

    -- Step 2: compute state commitment from tree root + preimage
    let stateCommitment = computeStateCommitment(stateRoot, response.stateCommitmentPreimage)

    -- Step 3: reconstruct StoredBatchInfo and check against L1
    let storedBatchInfo = StoredBatchInfo {
        batchNumber:                 response.l1VerificationData.batchNumber,
        batchHash:                   stateCommitment,
        indexRepeatedStorageChanges: 0,
        numberOfLayer1Txs:           response.l1VerificationData.numberOfLayer1Txs,
        priorityOperationsHash:      response.l1VerificationData.priorityOperationsHash,
        dependencyRootsRollingHash:  response.l1VerificationData.dependencyRootsRollingHash,
        l2ToL1LogsRootHash:          response.l1VerificationData.l2ToL1LogsRootHash,
        timestamp:                   0,
        commitment:                  response.l1VerificationData.commitment,
    }

    let computedHash = keccak256(abi.encode(storedBatchInfo))
    assert computedHash = onChainHash
```

Where `onChainHash` is obtained by the caller via `diamondProxy.storedBatchHash(batchNumber)` — a single `eth_call`, no event scanning required.

Alternatively, the caller can obtain `onChainHash` by scanning `BlockCommit` events emitted by the diamond proxy for the relevant batch number.
