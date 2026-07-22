# Atomic-swap demo driver (v32.0 multi_chain)

Self-contained TypeScript driver for an all-or-nothing cross-chain **atomic swap**
between the two L1-settling chains in the `../` preset (A = 6565 @ :3050,
B = 6566 @ :3051). No cross-repo dependency: `lib/` vendors the six interop-SDK
modules the driver needs (they depend only on `ethers`).

See [../ATOMIC_SWAP.md](../ATOMIC_SWAP.md) for what the swap does step by step.

## Prerequisites

Bring up the preset first (from the repo root):

```bash
./run_local.sh ./local-chains/v32.0/multi_chain
# wait until :3050 / :3051 answer eth_chainId (0x19a5 / 0x19a6)
```

Then register the two chains for interop once per anvil session (see
[../ATOMIC_SWAP.md](../ATOMIC_SWAP.md) step 2).

## Install + run

```bash
cd local-chains/v32.0/multi_chain/atomic-swap
npm install
PRIVATE_KEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110 \
L2_RPC_URL=http://127.0.0.1:3050 \
L2_RPC_URL_SECOND=http://127.0.0.1:3051 \
L1_RPC_URL=http://127.0.0.1:8545 \
  npm run atomic-swap
```

The private key above is the standard ZKsync rich L2 account
(`0x36615Cf349d7F6344891B1e7CA7C72883F5dc049`), funded on both chains by the deploy.

## Environment variables

| Var                 | Meaning                                  | Default                  |
|---------------------|------------------------------------------|--------------------------|
| `PRIVATE_KEY`       | funded L2 key (source + dest)            | — (required)             |
| `L2_RPC_URL`        | chain A (source) RPC                     | — (required)             |
| `L2_RPC_URL_SECOND` | chain B (destination) RPC                | — (required)             |
| `L1_RPC_URL`        | L1 (anvil) RPC, for interop-root waits   | `http://127.0.0.1:8545`  |
| `ATOMIC_DEADLINE_TS`    | flow deadline (SL L1 timestamp)      | `l1Now + 24h`            |

Expected success line: `Atomic swap complete: both legs executed atomically.`
(both bundles reach `bundleStatus = 2` / FullyExecuted, both wrapped tokens mint).

## Layout

```
atomic-swap/
├── atomic-swap-3chains.ts   # the driver (imports from ./lib)
├── lib/                     # vendored interop-SDK subset (ethers-only)
│   ├── index.ts             # barrel re-exporting the symbols the driver uses
│   ├── atomic.ts            # atomic ABIs, IMT engine, proof helpers
│   ├── bundle-builder.ts
│   ├── address.ts
│   ├── constants.ts
│   ├── abis.ts
│   └── types.ts
├── package.json
└── tsconfig.json
```
