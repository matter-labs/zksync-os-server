# Replay Archive

`zksync_os_replay_archive` stores cold-storage copies of block replay records. The archive is an
extra safety layer for cases where local node storage is lost or corrupted: replay records are
written outside the node RocksDB path and can later be used to rebuild the node replay WAL.

The archive stores replay records only. It does not store batch metadata. Batch information can be
recovered from L1 committed batch range events once block replay records are available.

## Storage Layout

Every node process creates one session. The session name is:

```text
<timestamp_millis>-<node_id>
```

Replay records are stored under:

```text
<session>/<block_number>/<block_hash>
```

For the filesystem backend, the full path is:

```text
<archive_root>/<timestamp_millis>-<node_id>/<block_number>/<block_hash>
```

The object value is the replay record payload only. There is no wrapper, batch number, block range,
or extra archive metadata in the object body.

Implementations of `ReplayArchiveStorage` must be append-only:

- `init` must fail if the session already exists.
- `append_object` must fail if the object key already exists.
- Existing archive data must never be overwritten, even with identical bytes.

## Write Path

The node constructs a `ReplayArchiver` from the configured backend and starts
`ReplayArchiveComponent`.

`ReplayArchivingWriteReplay` writes records to replay storage and sends `(block_hash, ReplayRecord)`
to the component through a bounded channel. The actual archive write happens asynchronously in the
component. If the queue is full, backpressure is applied to replay storage writes.

The current queue size is `REPLAY_ARCHIVE_QUEUE_SIZE`.

## Implementations

Current archive implementations:

- `FileSystemReplayArchiveStorage`: append-only object storage on local disk.
- `FileSystemReplayArchiver`: filesystem archiver that stores plaintext JSON replay records.
- `AgeEncryptedReplayArchiver`: wrapper that JSON-encodes replay records and encrypts them with
  age X25519 before storing them in any `ReplayArchiveStorage`.

Current reader implementation:

- `FileSystemReplayArchiveReader`: lists archive objects from the filesystem layout.

Other storage backends, such as S3, should implement:

- `ReplayArchiveStorage` for node-side append/check operations.
- `ReplayArchiveStorageReader` for recovery-side object listing.

## Encryption

Encrypted archives use age X25519.

The node needs only the public recipient key:

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
<output_root>/<block_number>/<block_hash>/<session>
```

Second, rebuild the node replay RocksDB from a canonical anchor:

```text
anchor = (latest_block_number, latest_block_hash)
```

If the archive was encrypted, recovery decrypts downloaded objects in memory when an age identity
file is provided. Decrypted replay records are not written to disk.

The recovery logic starts from the anchor, reads the replay record for that block, extracts the
previous block hash from the replay record, and walks backward until block `0`. It then writes the
canonical chain into RocksDB from genesis upward using the node replay storage format.

If several sessions contain the same `(block_number, block_hash)`, recovery verifies that the
session copies agree before writing the record.

## CLI

The recovery utility binary is `replay_archive_recovery`.

Download archive objects:

```bash
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  download \
  --archive-root ./db/replay_archive \
  --output-root ./replay_archive_downloaded
```

Rebuild replay RocksDB:

```bash
cargo run -p zksync_os_replay_archive --bin replay_archive_recovery -- \
  recover-rocksdb \
  --input-root ./replay_archive_downloaded \
  --replay-db-path ./db/block_replay_wal \
  --anchor-block-number 123 \
  --anchor-block-hash 0x...
```

For encrypted archives, pass the age identity file to `recover-rocksdb`:

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
