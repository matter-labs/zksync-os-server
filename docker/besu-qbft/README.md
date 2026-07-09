# ZKsync OS on a private Besu QBFT L1 (docker compose)

One-command dev stack that reproduces the manual Besu flow:

```
genesis-init ──> besu-node-1..4 (QBFT, chainId 31337) ──> deployer (one-shot) ──> server
                 L1 JSON-RPC :8545                        zk-deployer:            zksync-os-server
                                                            bootstrap             built from LOCAL sources,
                                                            apply                 --config local-chains/local_dev.yaml
                                                            server-config         --config /deployment/server.yaml
```

```bash
cd docker/besu-qbft
docker compose up --build
```

First run: image builds (the deployer image compiles `zk-deployer` and bakes the
era-contracts forge artifacts; the server image compiles the local workspace),
then the ecosystem deployment broadcasts for **~5–7 minutes** at 2s blocks.
Subsequent `up`s reuse images and the deployer exits early when the deployment
is still live.

## Endpoints

| Service          | URL                     |
| ---------------- | ----------------------- |
| L1 JSON-RPC      | http://localhost:8545   |
| L2 JSON-RPC (HTTP+WS), chain id 6565 | http://localhost:3050 |
| Prover API       | http://localhost:3124   |
| Prometheus       | http://localhost:3312   |

## State model — what survives what

Everything is **ephemeral by design** (no RocksDB/Besu persistence):

| Action | Effect |
| ------ | ------ |
| `docker compose restart server` | Server restarts against the same L1/L2 state (this is also the remedy for the known Besu QBFT `finalized`-tag stall). |
| `docker compose down && docker compose up` | Brand-new L1 from (re-derived) genesis + full ecosystem redeploy. The `deployment` volume survives `down`, but the deployer detects that its recorded bridgehub has no code on the fresh L1, wipes the volume and redeploys. `-v` on `down` just does the wipe eagerly. |
| Recreating the server container (e.g. after `--build`) | Requires a fresh L1 too (`down && up --build`): an empty server DB cannot resume against an L1 that already has committed batches. |

## Editing things

| You changed | To apply |
| ----------- | -------- |
| `besu/qbftConfigFile.json` (forks, alloc, chainId, gas limit, QBFT params) | `docker compose down && docker compose up` — `genesis-init` re-derives `genesis.json` on every `up`; nothing is pre-generated. |
| `local-chains/local_dev.yaml` (bind-mounted) | `docker compose restart server` |
| zksync-os-server sources | `docker compose down && docker compose up --build` |
| `deployer/intent.yaml` (chain id, DA mode) | `docker compose build deployer && docker compose down && docker compose up` |
| zk-deployer / era-contracts revision | `ZK_ITESTS_REF=<branch-or-tag> docker compose build deployer` (era-contracts rev follows the branch's `Cargo.lock` pin automatically) |
| Validator **count** (the one manual regen) | see below |

### Regenerating validator material

Only needed when changing the number of validators (or rotating their keys) —
`extraData.txt` + `besu/keys/` encode the validator set and nothing else:

```bash
cd docker/besu-qbft/besu
# set blockchain.nodes.count in qbftConfigFile.json, then:
docker run --rm -v "$PWD:/work" hyperledger/besu:26.6.1 operator generate-blockchain-config \
  --config-file=/work/qbftConfigFile.json --to=/work/networkFiles --private-key-file-name=key
# rearrange networkFiles/keys/<addr>/ into keys/node-N/, copy the new extraData
# from networkFiles/genesis.json into extraData.txt, update the bootnode enode
# (keys/node-1/key.pub) in docker-compose.yaml, add/remove besu-node-N services.
rm -rf networkFiles
```

## Accounts (public test keys — never use with real funds)

The genesis prefunds the standard Besu-docs dev accounts (see the `comment`
fields in `besu/qbftConfigFile.json`), including the deployer key
(`0x627306...ef57`, overridable via `DEPLOYER_PK`), two zksync-os-scripts rich
accounts, and pre-deploys the Foundry deterministic CREATE2 factory. The
committed QBFT validator keys under `besu/keys/` are likewise dev-only
material for this throwaway network.

## Version pins

- Besu: `hyperledger/besu:26.6.1` (compose)
- zk-deployer: `ZK_ITESTS_REF` build arg, default `main` (the clone layer is cached — rebuild with `--no-cache deployer` to pick up a moved branch)
- era-contracts: automatically the rev pinned by zk-deployer's `Cargo.lock`
- Foundry `v1.5.1` (era-contracts CI pin), Node 20 — deployer image build args

## Known issues / gotchas

- **Historical-state queries / archive mode**: on every start the server
  locates the diamond proxy's deployment block by binary-searching
  `eth_getCode` over `[0, head]`, so the RPC node must answer state queries at
  arbitrary old blocks. Validated on Besu 26.6.1: with BONSAI (the default),
  any state query (`eth_getCode`/`eth_getBalance`/…) more than
  `--bonsai-historical-block-limit` (default 512, hard minimum 512) blocks
  behind head returns a spec-violating `{"result": null}` — not an error, not
  `"0x"` (same for blocks beyond head). The server deserializes that null into
  a genesis-loading panic (`Failed to load genesis upgrade transaction ...
  invalid type: null`), so *any* server (re)start more than 512 blocks after
  chain registration fails against a BONSAI L1. The validators therefore run
  `--data-storage-format=FOREST` (archive, every depth resolvable); do the
  same (or use an archive RPC node) for any manually-run Besu a server points
  at. Note: trie-log pruning is irrelevant here — `--bonsai-limit-trie-logs-enabled`
  is rejected with `--sync-mode=FULL` (the only mode for private QBFT); the
  512-block window gates access regardless.

