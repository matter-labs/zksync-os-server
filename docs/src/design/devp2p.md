# devp2p / `zks` Protocol

The `lib/network` crate integrates ZKsync OS-specific peer-to-peer traffic into the node's
devp2p / RLPx network stack.

Its purpose is not to replace the node's general networking. Instead, it adds two subprotocols
multiplexed over the same RLPx connection:

1. `zks/<version>` — replay streaming from a main node to an external node
2. `zks_2fa/1` — batch-verification request / response exchange between a main node and
   verifier-capable external nodes

## High-level model

At runtime, each node is configured locally as either:

- a `MainNode`
- an `ExternalNode`

That local role determines how the node behaves on each negotiated connection:

- the main node serves replay data over `zks` and receives `VerifyBatchResult` over `zks_2fa`
- the external node requests replay data over `zks` and, if configured as a verifier, advertises
  `zks_2fa` and authenticates on it

There is currently no explicit "remote role negotiation" on top of devp2p. The local node chooses
its behavior from config and then expects the remote peer to behave compatibly on the negotiated
connections.

## Supported versions

- `zks/5` — the current (and only) production version. Replay-only: `GetBlockReplays` (0x00) and
  `BlockReplays` (0x01, batched via `max_blocks_per_message`), using the v3 replay record
  encoding.
- `zks/0` — a bare-bones version kept in-tree for tests only (records carry just the block
  number); it is never registered in production.
- `zks/1`–`zks/4` are retired. `zks/3` and `zks/4` carried the verifier messages inline at message
  IDs 0x02–0x06; those IDs are reserved and must not be reused without bumping the protocol
  version.
- `zks_2fa/1` — hosts the verifier handshake (`VerifierRoleRequest` 0x00, `VerifierChallenge`
  0x01, `VerifierAuth` 0x02) and batch verification (`VerifyBatch` 0x03, `VerifyBatchResult`
  0x04). Only verifier-configured ENs advertise it; the main node always does.

## Module split

- `service.rs`
  Owns the network manager, registers the `zks` and `zks_2fa` protocol handlers, consumes protocol
  events, tracks peer sessions, and dispatches `VerifyBatch` requests to eligible peers over their
  `zks_2fa` connections.
- `protocol/`
  Contains the `zks` RLPx subprotocol implementation itself.
  - `handler.rs`: bridges reth's protocol hooks into per-connection tasks
  - `mn.rs`: main-node side of one `zks` connection
  - `en.rs`: external-node side of one `zks` connection
  - `events.rs`: protocol events
  - `handler_shared_state.rs`: shared handler runtime state such as the active-connection limit
- `twofa/`
  Contains the `zks_2fa` RLPx subprotocol: verifier handshake and batch verification transport,
  plus the registry of live `zks_2fa` connections used for dispatch.
- `session.rs`
  Tracks higher-level peer session facts derived from protocol events, such as replay progress and
  verifier authorization state.
- `wire/`
  Defines the wire messages carried over devp2p (shared payload types are reused by `zks_2fa`).

## Replay flow

Replay is the `zks` protocol's sole responsibility.

1. The EN negotiates a `zks/<version>` capability over devp2p.
2. The EN sends `GetBlockReplays`, including how many records the MN may batch per response.
3. The MN streams `BlockReplays`, batching up to the requested number of records per message.
4. The EN forwards received replay records into its local pipeline.

Replay record encoding is versioned separately from the protocol version (`wire/replays/v*.rs`);
`zks/5` pins the v3 record encoding.

## Batch verification flow

Batch verification lives in the standalone `zks_2fa` subprotocol. Peers are correlated across the
two subprotocols by their devp2p `PeerId`.

1. An EN that is configured as a verifier advertises `zks_2fa` and sends `VerifierRoleRequest`.
2. The MN replies with `VerifierChallenge`.
3. The EN signs the challenge and sends `VerifierAuth`.
4. The MN emits authorization events and tracks verifier eligibility for that peer session.
5. When the MN wants external verification for a batch, `service.rs` selects eligible peers and
   sends `VerifyBatch` over their live `zks_2fa` connections.
6. The EN-side verifier validates the request and sends back `VerifyBatchResult`.
7. The MN forwards those results into the batch-verification pipeline, which validates request ids,
   signatures, and signer membership before counting them.

### Sequence

```mermaid
sequenceDiagram
    participant EN as External Node
    participant MN as Main Node
    participant VS as Verifier Service
    participant BV as Batch Verification Pipeline

    Note over EN,MN: devp2p negotiates zks/5 (+ zks_2fa/1 for verifier ENs)

    alt EN is verifier-capable (zks_2fa)
        EN->>MN: VerifierRoleRequest
        MN->>EN: VerifierChallenge
        EN->>MN: VerifierAuth
        Note over MN: Peer session marked authorized if signer is accepted
    end

    EN->>MN: GetBlockReplays (zks)
    loop replay stream
        MN->>EN: BlockReplays (zks)
        Note over EN: replay records forwarded into local pipeline
    end

    Note over MN,BV: Main node decides a batch needs external verification
    BV->>MN: VerifyBatch request
    MN->>EN: VerifyBatch (zks_2fa)
    EN->>VS: forward request
    VS-->>EN: Approved / Refused
    EN->>MN: VerifyBatchResult (zks_2fa)
    MN->>BV: forward result
    Note over BV: request id, signature, and signer membership are validated here
```

## Why session tracking exists

The network service needs more than "is this peer connected right now?"

For verification dispatch, it needs to know whether a peer:

- requested replay
- has been sent replay far enough to verify a given batch
- successfully authenticated as a verifier on the current connection

That derived state is kept in `PeerSessionStore`, fed by events from both subprotocols. Live send
handles stay in the `zks_2fa` connection registry. Dispatch joins those two views:

- `PeerSessionStore` answers "who is eligible?"
- `Zks2faConnectionRegistry` answers "how do I send to them right now?"
