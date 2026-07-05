# Consensus

zksync-os-server can run as one validator of a **BFT committee** instead of as a
single sequencer: several operators, none of whom trust each other, jointly produce
one chain. A block becomes part of the chain only after a quorum of validators has
independently re-executed it and signed off, and once it does, it is final — no
committee member, including whoever proposed it, can take it back.

This section is written for engineers working on the node. It assumes no prior
consensus background and stays at the level of principles and contracts — the things
that hold regardless of which knob or module was touched last. Where an exhaustive,
current inventory matters (config fields, test names, rule lists), the documentation
points into the code instead of copying it; the code is the source of truth, and the
module documentation there is written to be read.

| Chapter | The question it answers |
| --- | --- |
| [What and why](intro.md) | What BFT consensus buys, how the Simplex protocol works, and what the commonware library provides |
| [How it is wired in](integration.md) | Where consensus meets the node: the one-trait seam, speculative state, validity rules, and a block's life |
| [How it is tested](testing.md) | Why deterministic simulation is the primary test surface, what each layer proves, and how to work with the harness |
| [Running it](enabling.md) | Enabling consensus on a new chain, migrating an existing chain into a committee, and rolling back |

The one-paragraph summary of the whole design: consensus runs in-process on its own
thread, and decides *which* block comes next; everything about *what* a block is —
execution, storage, settlement, external-node sync — is the node's existing
machinery, unchanged. The leader of each round executes a block and proposes it;
every other validator re-executes it and compares outcomes **before** voting, so a
dishonest proposer cannot get a bad block finalized — it can only waste its own
turn. Only finalized blocks ever reach disk.
