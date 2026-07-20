# Replay Archive

`zksync_os_replay_archive` stores cold-storage copies of block replay records. The archive is an
extra safety layer for cases where local node storage is lost or corrupted: replay records are
written outside the node RocksDB path and can later be used to rebuild the node replay WAL.

The archive stores replay records only. It does not store batch metadata. Batch information can be
recovered from L1 committed batch range events once block replay records are available.

## Storage Layout

All nodes and process restarts write to one shared namespace. Replay records use:

```text
<block_number>/<block_hash>
```

For the filesystem backend, the full path is:

```text
<archive_root>/<block_number>/<block_hash>
```

S3 and GCS use the same `<block_number>/<block_hash>` value as the object key.

The object value is the replay record payload only. There is no wrapper, batch number, block range,
or extra archive metadata in the object body.

Implementations of `ReplayArchiveStorage` must provide idempotent, append-only writes:

- `contains_object` checks whether a key is already present.
- `put_object_if_absent` creates a missing key and treats an existing key as success.
- Existing archive data must never be overwritten, even with identical bytes.

## Write Path

The node constructs a `ReplayArchiver` from the configured backend and starts
`ReplayArchiveComponent`.

`ReplayArchivingWriteReplay` writes records to replay storage and sends `(block_hash, ReplayRecord)`
to the component through a bounded channel. The actual archive write happens asynchronously in the
component. If the queue is full, backpressure is applied to replay storage writes.

The current queue size is `REPLAY_ARCHIVE_QUEUE_SIZE`.

### Concurrent Writes

`ReplayArchiveComponent` processes up to `MAX_PARALLEL_OBJECT_PUTS` archive operations
concurrently. The same storage contract handles races between those operations and writers in
other nodes.

For each replay record, the archiver first checks whether `<block_number>/<block_hash>` already
exists. If it does, the write succeeds immediately without serializing or encrypting the payload.
This check is an optimization; it is not the concurrency guarantee.

If the key is missing, the archiver builds the payload and performs an atomic conditional create:

- GCS uses `if_generation_match(0)`.
- S3 uses `If-None-Match: *`.
- The filesystem writes a complete temporary file and hard-links it to the final path.

When several nodes race, exactly one conditional create publishes the object. The other writers
observe that the key already exists and also return success. A restart follows the same path and
normally stops at the initial existence check. Errors other than an already-existing key are
propagated and fail the archive component.

First-writer-wins applies to the exact `(block_number, block_hash)` pair. Different block hashes at
the same height are separate objects. If writers provide different replay records for the same key,
the first stored payload is retained; writers do not download, decrypt, or compare the existing
payload.

## Implementations

Current archive implementations:

- `FileSystemReplayArchiveStorage`: conditional object creation on local disk.
- `FileSystemReplayArchiver`: filesystem archiver that stores plaintext JSON replay records.
- `S3ReplayArchiveStorage`: conditional object creation in S3 or an S3-compatible service.
- `GcsReplayArchiveStorage`: conditional object creation in Google Cloud Storage.
- `AgeEncryptedReplayArchiver`: wrapper that JSON-encodes replay records and encrypts them with
  age before storing them in any `ReplayArchiveStorage`. Supports X25519 recipients and GCP KMS
  asymmetric keys.

Current reader implementation:

- `FileSystemReplayArchiveReader`: lists archive objects from the filesystem layout.
- `S3ReplayArchiveReader`: lists archive objects from S3.
- `GcsReplayArchiveReader`: lists archive objects from Google Cloud Storage.

Other storage backends should implement:

- `ReplayArchiveStorage` for node-side conditional-create/check operations.
- `ReplayArchiveStorageReader` for recovery-side object listing.

## Encryption

Encrypted archives use the age format with one of two recipient types. GCP KMS is the primary
mode for our deployments; age X25519 is available as a KMS-independent alternative.

With GCP KMS, the node is configured with the resource name of an `ASYMMETRIC_DECRYPT` key version
using an `RSA_DECRYPT_OAEP_*_SHA256` algorithm:

```text
projects/../locations/../keyRings/../cryptoKeys/../cryptoKeyVersions/..
```

The node fetches the public key once at startup (requiring only
`cloudkms.cryptoKeyVersions.viewPublicKey`) and wraps the per-record age file key locally with
RSA-OAEP; no private key material exists outside KMS. During recovery, unwrapping a record's file
key takes one KMS `AsymmetricDecrypt` call (requiring
`cloudkms.cryptoKeyVersions.useToDecrypt`), so key access can be revoked and audited. Recovery
currently decodes each archived record once during the canonical chain walk and again when writing
to RocksDB, so budget roughly two `AsymmetricDecrypt` calls per stored record.
Note that KMS-encrypted objects use a custom age stanza and can only be decrypted by the recovery
tool, not by the stock `age` CLI.

The key version resource name is embedded in the age header of every archived object, so it can be
recovered from the archive itself even if the node configuration is lost:

```console
$ head -c 300 <downloaded_object> | strings | head -2
age-encryption.org/v1
-> gcp-kms-rsa-oaep projects/../locations/../keyRings/../cryptoKeys/../cryptoKeyVersions/..
```

With age X25519, the node needs only the public recipient key:

```text
age1...
```

The private identity should be stored separately and used only during recovery:

```text
AGE-SECRET-KEY-...
```

Encryption is randomized, so archive presence checks verify object existence only. They do not
re-encrypt a replay record and compare bytes.

## Recovery

Recovery has two steps.

First, download all archive objects into a local recovery layout:

```text
<output_root>/<block_number>/<block_hash>
```

Second, rebuild the node replay RocksDB from a canonical anchor:

```text
anchor = (latest_block_number, latest_block_hash)
```

The anchor must come from a trusted source, e.g. `eth_getBlockByNumber("latest")` on a healthy
replica, or a block explorer. When testing recovery (rather than responding to actual data loss),
the highest `<block_number>/<block_hash>` in the downloaded layout can be used as the anchor: it
is the latest record the archive contains.

If the archive was encrypted, recovery decrypts downloaded objects in memory when a GCP KMS key
version (`--kms-key-version`) or an age identity (`--identity-file` / `--age-secret-key`) is
provided. Decrypted replay records are not written to disk.

The recovery logic starts from the anchor, reads the replay record for that block, extracts the
previous block hash from the replay record, and walks backward until block `0`. It then writes the
canonical chain into RocksDB from genesis upward using the node replay storage format.

## CLI

The recovery utility binary is `replay_archive_recovery`.

Download archive objects:

```bash
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  download \
  --archive-root ./db/replay_archive \
  --output-root ./replay_archive_downloaded
```

Download archive objects from S3:

```bash
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  download \
  --s3-bucket-base-url my-replay-archive \
  --s3-credential-file-path ./s3-credentials \
  --s3-region us-east-2 \
  --output-root ./replay_archive_downloaded
```

Download archive objects from GCS using Application Default Credentials. The caller needs
`storage.objects.list` and `storage.objects.get` on the bucket:

```bash
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  download \
  --gcs-bucket-base-url my-replay-archive \
  --output-root ./replay_archive_downloaded
```

On GKE, ADC uses Workload Identity without additional configuration. For external workload
identity federation, point ADC at the external-account configuration before starting the process:

```bash
GOOGLE_APPLICATION_CREDENTIALS=./wif-credentials.json \
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  download \
  --gcs-bucket-base-url my-replay-archive \
  --output-root ./replay_archive_downloaded
```

For local testing, initialize local ADC with `gcloud auth application-default login`.

Rebuild replay RocksDB from a KMS-encrypted archive (the primary mode for our deployments). The
caller needs `cloudkms.cryptoKeyVersions.useToDecrypt` on the key version. ADC requires no
credential CLI flags:

```bash
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  recover-rocksdb \
  --input-root ./replay_archive_downloaded \
  --replay-db-path ./db/block_replay_wal \
  --anchor-block-number 123 \
  --anchor-block-hash 0x... \
  --kms-key-version projects/../locations/../keyRings/../cryptoKeys/../cryptoKeyVersions/..
```

KMS uses the same ADC configuration as GCS. Every record decode costs one KMS
`AsymmetricDecrypt` call, and recovery decodes records during the chain walk and again while
writing to RocksDB (roughly two calls per stored record); `--decrypt-concurrency`
(default 32) bounds the number of in-flight KMS requests.

Rebuild replay RocksDB from an unencrypted archive:

```bash
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  recover-rocksdb \
  --input-root ./replay_archive_downloaded \
  --replay-db-path ./db/block_replay_wal \
  --anchor-block-number 123 \
  --anchor-block-hash 0x...
```

For age-X25519-encrypted archives, pass the age identity file to `recover-rocksdb`:

```bash
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  recover-rocksdb \
  --input-root ./replay_archive_downloaded \
  --replay-db-path ./db/block_replay_wal \
  --anchor-block-number 123 \
  --anchor-block-hash 0x... \
  --identity-file ./replay-archive.key
```

Alternatively, provide the `AGE-SECRET-KEY-...` value directly through
`REPLAY_ARCHIVE_AGE_SECRET_KEY`:

```bash
REPLAY_ARCHIVE_AGE_SECRET_KEY='AGE-SECRET-KEY-...' \
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  recover-rocksdb \
  --input-root ./replay_archive_downloaded \
  --replay-db-path ./db/block_replay_wal \
  --anchor-block-number 123 \
  --anchor-block-hash 0x...
```

`--replay-db-path` must point to the `block_replay_wal` RocksDB directory, not the parent node
storage directory.

## Node Configuration

Replay archiving is configured by `ReplayArchiveConfig`.

Default:

```yaml
replay_archive:
  type: Noop
```

Filesystem archive with age encryption:

```yaml
replay_archive:
  type: FileSystem
  root_path: ./db/replay_archive
  encryption:
    type: AgeX25519
    recipient: age1...
```

S3 archive with age encryption:

```yaml
replay_archive:
  type: S3WithCredentialFile
  bucket_base_url: my-replay-archive
  s3_credential_file_path: ./s3-credentials
  endpoint: null
  region: us-east-2
  encryption:
    type: AgeX25519
    recipient: age1...
```

The S3 backend follows the old object-store initialization path: credentials are loaded from the
configured credentials file, `endpoint` overrides S3 API endpoint for S3-compatible
providers, and `region` is used as the first region provider before falling back to the SDK
defaults and then `auto`.

GCS archive with GCP KMS encryption (the primary mode for our deployments):

```yaml
replay_archive:
  type: Gcs
  bucket_base_url: my-replay-archive
  encryption:
    type: GcpKms
    kms_key_version: projects/../locations/../keyRings/../cryptoKeys/../cryptoKeyVersions/..
```

Both GCS and KMS use the Google Cloud client libraries' Application Default Credentials chain.
This discovers GKE Workload Identity automatically, reads an external workload identity federation
configuration from `GOOGLE_APPLICATION_CREDENTIALS`, or uses local credentials created by
`gcloud auth application-default login`. The node only ever uses the KMS public key, so its
identity needs `cloudkms.cryptoKeyVersions.viewPublicKey` and should not be granted `useToDecrypt`.
