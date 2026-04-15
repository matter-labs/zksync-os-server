# Batch verification (2FA)

Batch verification, also referred to as 2FA, adds independent approval to batch commits.
Instead of trusting the main node alone, you require external nodes (ENs) to confirm that they
can reproduce a batch and sign it before the batch is treated as ready to commit.

2FA serves two different purposes depending on how it is configured:

- L2-only: a data-availability and recoverability safeguard. The main node only moves batches
  forward after independent ENs have reproduced them.
- Settlement-layer backed: if a `MultisigCommitter` is present on the settlement layer, the same
  signatures are also used by the settlement-layer commit path. This adds an execution-correctness
  check that is separate from the proof system.

## L2-only mode

L2-only mode is for data availability. In this setup:

- the main node broadcasts batch verification requests to verifier peers over the p2p network;
- selected ENs receive those requests and sign approvals from their local replay state;
- the main node requires enough EN approvals before the batch can move forward.

The value of this mode is that committed batches must already be reproducible by the participating
ENs. If the main node is lost, committed L2 data should already exist on those ENs.

## Settlement-layer-backed mode

If the chain's settlement layer uses a `MultisigCommitter` for `ValidatorTimelock`, batch
verification also has a settlement-layer-backed component.

In that case the main node reads the settlement-layer validator set and threshold on startup and
uses them for batch commit submission. This changes the behavior in two important ways:

- the settlement-layer validator set becomes the signer allowlist used for commit submission;
- the effective threshold is the higher of the local `batch_verification_threshold` and the
  settlement-layer threshold.

This mode is not primarily about data availability. It adds assurance that batch execution was
accepted by the configured validator set, independently of the proof system.

## How To Use 2FA

Use 2FA with ENs that are operationally independent from the main node. The point is not just to
run extra processes, but to require approval from separate nodes that can independently replay the
same batch.

Each participating EN should have its own signing key. The corresponding signer addresses should
match the allowlist that the main node accepts:

- in L2-only mode, that allowlist comes from `batch_verification_accepted_signers`;
- in settlement-layer-backed mode, it comes from the settlement-layer validator set.

The EN signing keys do not submit transactions themselves. The main node collects signatures and,
when settlement-layer-backed mode is active, includes them in the settlement-layer commit flow.

## Main Node Configuration

Enable and configure the main node / sequencer with these options:

- `batch_verification_server_enabled`
  Enables batch verification request collection on the main node. Without this, the main node
  does not collect EN signatures.
- `batch_verification_threshold`
  Minimum number of EN signatures required by the main node. If settlement-layer-backed mode is
  active, the effective threshold is `max(local threshold, settlement-layer threshold)`.
- `batch_verification_accepted_signers`
  Comma-separated list of accepted signer addresses for L2-only mode. These should correspond to
  the EN signing keys. If the settlement layer provides a non-empty validator set or a non-zero
  threshold through `MultisigCommitter`, that settlement-layer validator set takes precedence over
  this local list.
- `batch_verification_request_timeout`
  How long the main node waits for responses during a single signature collection attempt.
- `batch_verification_retry_delay`
  Delay between collection attempts when the main node retries.
- `batch_verification_total_timeout`
  Overall time budget for collecting enough signatures for a batch.

## 2FA EN Configuration

Each EN that participates in 2FA needs these options:

- `batch_verification_client_enabled`
  Enables the EN verifier role on the p2p network.
- `batch_verification_signing_key`
  Private key used by the EN to sign batch approvals. Its address must be present in the local
  accepted signer list for L2-only mode, or in the settlement-layer validator set for
  settlement-layer-backed mode.
