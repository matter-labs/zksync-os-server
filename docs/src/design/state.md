# State

The global state is a set of key/value pairs:
- key = keccak(address, slot)
- value = a single 32-byte word (U256)

All such pairs are stored (committed) in the Merkle tree (see tree.md).

## Account metadata (AccountProperties)
Account-related data (balance, nonce, deployed bytecode hash, etc.) is grouped into an AccountProperties struct. We do NOT store every field directly in the tree. Instead:
- We hash the AccountProperties struct.
- That hash (a single U256) is what appears in the Merkle tree.
- The full struct is retrievable from a separate preimage store.

## Special address: ACCOUNT_STORAGE (0x8003)
We reserve the synthetic address 0x8003 to map account addresses to their AccountProperties hash. Concretely:
value at key = keccak(0x8003, user_address) = hash(AccountProperties(user_address))

## Example: fetching the nonce for address 0x1234
1. Compute key = keccak(0x8003, 0x1234)
2. Read the U256 value H from the Merkle tree at that key
3. Look up preimage(H) to get AccountProperties
4. Take the nonce field from that struct

This indirection:
- Keeps the Merkle tree smaller (one leaf per account metadata bundle)
- Avoids multiple leaf updates when several account fields change at once.


## Bytecodes

We track two related things:
1. What the outside world sees (the deployed / observable bytecode).
2. An internal, enriched form that adds execution helpers (artifacts).

### Terminology
- Observable (deployed) bytecode:
    The exact bytes you get from an RPC call like eth_getCode or cast code.
- Observable bytecode hash (observable_bytecode_hash):
    keccak256(observable bytecode). This matches Ethereum conventions.
- Internal extended representation:
    observable bytecode
    + padding (if any, e.g. to align)
    + artifacts (pre‑computed data used to speed execution, e.g. jumpdest map).
- Internal bytecode hash (bytecode_hash):
    blake2 hash of the full extended representation above. The extended blob itself lives in the preimage store; only the blake hash is stored in AccountProperties.

### Stored fields in AccountProperties
- bytecode_hash (Bytes32):
    blake2 hash of `[observable bytecode | padding | artifacts]`.
- unpadded_code_len (u32):
    Length (in bytes) of the original observable bytecode, before any internal padding or artifacts.
- artifacts_len (u32):
    Length (in bytes) of the artifacts segment appended after padding.
- observable_bytecode_hash (Bytes32):
    keccak256 of the observable (deployed) bytecode.
- observable_bytecode_len (u32):
    Length of the observable (deployed) bytecode. (Currently mirrors unpadded_code_len; kept explicitly for clarity / future evolution.)

### Why two hashes?
- keccak (observable_bytecode_hash) is what external tooling expects and can independently recompute.
- blake (bytecode_hash) commits to the richer internal representation the node actually executes against (including acceleration data), avoiding recomputing artifacts on every access.

### Lookup workflow (simplified)
1. From AccountProperties get:
     - bytecode_hash → fetch extended blob via preimage store.
     - observable_bytecode_hash → verify against externally visible code if needed.
2. Use lengths (unpadded_code_len, artifacts_len) to slice:
     [0 .. unpadded_code_len) → observable code
     [end of padding .. end) → artifacts

This separation keeps the Merkle tree lean while enabling fast execution.

