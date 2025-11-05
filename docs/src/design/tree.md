# Tree

State is stored in a binary Merkle‑like tree. In production the logical tree depth is 64 (root at depth 0, leaves at depth 63). We use Blake2 as a tree hashing algorithm.

Optimization:
- Instead of persisting every individual leaf, we group (package) 8 consecutive leaves together.
- 8 leaves form a perfectly balanced subtree of height 3 (because 2^3 = 8).
- One such packaged subtree is stored as a single DB record.

Terminology:
- We call each 3-level chunk of the logical tree a nibble (note: this is an internal term here).
- Effective path length (number of nibbles) = ceil(64 / 3) = 22.

So:
- Logical depth: 64 levels.
- Physical traversal steps: 22 nibbles.
- Each nibble lookup loads or updates one packaged subtree (8 leaves).

Result: fewer DB reads/writes while preserving a logical depth of 64.
