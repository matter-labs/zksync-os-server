### What’s stored and how it’s queried
- Each state write is stored in RocksDB under a composite key: hashed_key(32 bytes) || block_number(8 bytes, big‑endian) in the Data CF (here and below `||` stands for concatenation).
- All versions for the same logical key are contiguous and ordered by block number lexicographically.
- Read at block B performs a reverse seek from end = hashed_key || B, then takes at most a single item.
- Preimages are a simple KV store: hash -> bytes (no per-block versioning).

### Per‑operation complexity and storage accesses

#### Read: value of key K at block B
- Steps our code performs
    - Build end = K || B and get a reverse iterator on Data CF, then fetch the first item (iter.next()).
    - If the item’s 32‑byte prefix matches K, return its value; else return None.
- KV entries iterated: at most 1
- Edge cases:
    - No writes for K ≤ B: the first reverse item has a different prefix (or iterator is exhausted) ⇒ immediate None.
    - Empty blocks don’t change cost; we still land directly on the latest version ≤ B.

#### Write: apply block B with W writes
- We collapse duplicates per key in memory (“last write wins”), leaving U unique keys (U ≤ W).
- Batch contents:
    - U Puts into Data CF at K || B (one per unique key).
    - 1 Put into Meta CF for latest_block.
- KV writes: U + 1, batched in a single write

#### Preimages
- get_preimage(hash): 1 Get ⇒ one seek (O(log N) internal) and at most one KV read (O(1) KV scan).
- add(preimages): one batch with 1 Put per preimage (no reads).

### Comparison to the previous mixed in‑memory + Rocks approach
- Previous: read could scan multiple in‑memory diffs backward (worst‑case proportional to retained diffs) before hitting base RocksDB.
- New: read always resolves with a single reverse seek and returns after at most one KV item — independent of number of historical updates or empty blocks.

### Space / amplification / constants
- Space: stores all historical versions (one row per unique key update). That’s the trade‑off for O(1) KV scan reads.
- Write amplification: typical of LSMs; background compactions don’t change API‑visible complexity (still 1 batch per block).
- Constants: big‑endian block numbers ensure correct order; prefix locality makes iterator .next() sufficient.

### Optional optimizations (engine‑level)
- Ensure Bloom filters / block cache are enabled for Data CF to reduce I/O on seeks.
- Consider prefix extractor set to 32‑byte hashed_key to optimize prefix iteration and filters.

### TL;DR
- Read(key at block): at most 1 KV record examined; 1 engine seek (O(1) KV scan, O(log N) engine work).
- Write(block): one KV row per unique key updated + one meta row, in a single batch; no reads.
- Preimages: single Get per read; batched Puts per write.