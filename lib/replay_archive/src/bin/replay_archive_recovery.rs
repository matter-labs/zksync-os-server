use clap::{Parser, Subcommand};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use zksync_os_replay_archive::{
    ArchiveIdentity, FileSystemReplayArchiveReader, S3ReplayArchiveAuthMode, S3ReplayArchiveConfig,
    S3ReplayArchiveReader, download_all_replay_archive_objects, parse_age_x25519_identity,
    read_age_x25519_identity, recover_replay_records_to_rocksdb_with_optional_decryption,
};
#[cfg(feature = "gcp")]
use zksync_os_replay_archive::{
    GcpKmsClient, GcpKmsConfig, GcpKmsIdentity, GcsReplayArchiveAuthMode, GcsReplayArchiveConfig,
    GcsReplayArchiveReader,
};

#[derive(Debug, Parser)]
#[command(about = "Replay archive recovery utilities")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Download all archived replay record objects to local disk.
    Download {
        /// Root folder of the replay archive storage.
        #[cfg_attr(feature = "gcp", arg(
            long,
            conflicts_with_all = ["s3_bucket_base_url", "gcs_bucket_base_url"],
            required_unless_present_any = ["s3_bucket_base_url", "gcs_bucket_base_url"]
        ))]
        #[cfg_attr(not(feature = "gcp"), arg(
            long,
            conflicts_with_all = ["s3_bucket_base_url"],
            required_unless_present_any = ["s3_bucket_base_url"]
        ))]
        archive_root: Option<PathBuf>,
        /// S3 bucket of the replay archive storage.
        #[cfg_attr(feature = "gcp", arg(long, conflicts_with_all = ["archive_root", "gcs_bucket_base_url"]))]
        #[cfg_attr(not(feature = "gcp"), arg(long, conflicts_with_all = ["archive_root"]))]
        s3_bucket_base_url: Option<String>,
        /// Path to the S3 credentials file.
        #[arg(long, requires = "s3_bucket_base_url")]
        s3_credential_file_path: Option<PathBuf>,
        /// Use anonymous S3 access. This is only useful for public buckets.
        #[arg(
            long,
            requires = "s3_bucket_base_url",
            conflicts_with = "s3_credential_file_path"
        )]
        s3_anonymous: bool,
        /// Optional S3-compatible endpoint URL, e.g. for MinIO.
        #[arg(long, requires = "s3_bucket_base_url")]
        s3_endpoint: Option<String>,
        /// Optional S3 bucket region.
        #[arg(long, requires = "s3_bucket_base_url")]
        s3_region: Option<String>,
        /// GCS bucket of the replay archive storage.
        #[cfg(feature = "gcp")]
        #[arg(long, conflicts_with_all = ["archive_root", "s3_bucket_base_url"])]
        gcs_bucket_base_url: Option<String>,
        /// Use anonymous GCS access. This is only useful for public buckets.
        #[cfg(feature = "gcp")]
        #[arg(long, requires = "gcs_bucket_base_url")]
        gcs_anonymous: bool,
        /// Local folder where downloaded objects should be written.
        #[arg(long)]
        output_root: PathBuf,
        /// Number of archive objects fetched concurrently.
        #[arg(long, default_value_t = zksync_os_replay_archive::DEFAULT_DOWNLOAD_CONCURRENCY)]
        download_concurrency: NonZeroUsize,
    },
    /// Rebuild node replay RocksDB from downloaded replay records.
    RecoverRocksdb {
        /// Local folder containing downloaded replay records.
        #[arg(long)]
        input_root: PathBuf,
        /// Output RocksDB path for block replay WAL.
        #[arg(long)]
        replay_db_path: PathBuf,
        /// Anchor block number to recover from.
        #[arg(long)]
        anchor_block_number: u64,
        /// Canonical anchor block hash.
        #[arg(long)]
        anchor_block_hash: alloy::primitives::BlockHash,
        /// age identity file containing AGE-SECRET-KEY. If provided, records are decrypted in memory.
        #[cfg_attr(feature = "gcp", arg(long, conflicts_with_all = ["age_secret_key", "kms_key_version"]))]
        #[cfg_attr(not(feature = "gcp"), arg(long, conflicts_with_all = ["age_secret_key"]))]
        identity_file: Option<PathBuf>,
        /// age AGE-SECRET-KEY value. If provided, records are decrypted in memory.
        #[cfg_attr(feature = "gcp", arg(
            long,
            env = "REPLAY_ARCHIVE_AGE_SECRET_KEY",
            hide_env_values = true,
            conflicts_with_all = ["identity_file", "kms_key_version"]
        ))]
        #[cfg_attr(not(feature = "gcp"), arg(
            long,
            env = "REPLAY_ARCHIVE_AGE_SECRET_KEY",
            hide_env_values = true,
            conflicts_with_all = ["identity_file"]
        ))]
        age_secret_key: Option<String>,
        /// KMS key version, full resource name
        /// `projects/{project}/locations/{location}/keyRings/{key_ring}/cryptoKeys/{key}/cryptoKeyVersions/{version}`.
        /// If set, records are decrypted in memory, costing one KMS asymmetric-decrypt call per
        /// record decode.
        #[cfg(feature = "gcp")]
        #[arg(long, conflicts_with_all = ["identity_file", "age_secret_key"])]
        kms_key_version: Option<String>,
        /// Number of replay records decoded concurrently. With KMS decryption every record decode
        /// takes one KMS call, so this bounds in-flight KMS requests.
        #[arg(long, default_value_t = zksync_os_replay_archive::DEFAULT_DECRYPT_CONCURRENCY)]
        decrypt_concurrency: NonZeroUsize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Download {
            archive_root,
            s3_bucket_base_url,
            s3_credential_file_path,
            s3_anonymous,
            s3_endpoint,
            s3_region,
            #[cfg(feature = "gcp")]
            gcs_bucket_base_url,
            #[cfg(feature = "gcp")]
            gcs_anonymous,
            output_root,
            download_concurrency,
        } => {
            // clap guarantees exactly one backend is selected; GCS is only reachable when the
            // `gcp` feature is compiled in.
            #[cfg(feature = "gcp")]
            let gcs_downloaded = if let Some(gcs_bucket_base_url) = gcs_bucket_base_url {
                let reader = GcsReplayArchiveReader::new(GcsReplayArchiveConfig {
                    bucket_base_url: gcs_bucket_base_url,
                    auth_mode: if gcs_anonymous {
                        GcsReplayArchiveAuthMode::Anonymous
                    } else {
                        GcsReplayArchiveAuthMode::Authenticated
                    },
                })
                .await?;
                Some(
                    download_all_replay_archive_objects(
                        &reader,
                        &output_root,
                        download_concurrency,
                    )
                    .await?,
                )
            } else {
                None
            };
            #[cfg(not(feature = "gcp"))]
            let gcs_downloaded: Option<usize> = None;

            let downloaded = if let Some(gcs_downloaded) = gcs_downloaded {
                gcs_downloaded
            } else if let Some(archive_root) = archive_root {
                let reader = FileSystemReplayArchiveReader::new(archive_root);
                download_all_replay_archive_objects(&reader, &output_root, download_concurrency)
                    .await?
            } else {
                let auth_mode = if let Some(path) = s3_credential_file_path {
                    S3ReplayArchiveAuthMode::AuthenticatedWithCredentialFile(path)
                } else {
                    anyhow::ensure!(
                        s3_anonymous,
                        "--s3-credential-file-path is required unless --s3-anonymous is set"
                    );
                    S3ReplayArchiveAuthMode::Anonymous
                };
                let reader = S3ReplayArchiveReader::new(S3ReplayArchiveConfig {
                    bucket_base_url: s3_bucket_base_url
                        .expect("s3_bucket_base_url is required by clap"),
                    auth_mode,
                    endpoint: s3_endpoint,
                    region: s3_region,
                })
                .await;
                download_all_replay_archive_objects(&reader, &output_root, download_concurrency)
                    .await?
            };
            println!("Downloaded {downloaded} replay archive objects");
        }
        Command::RecoverRocksdb {
            input_root,
            replay_db_path,
            anchor_block_number,
            anchor_block_hash,
            identity_file,
            age_secret_key,
            #[cfg(feature = "gcp")]
            kms_key_version,
            decrypt_concurrency,
        } => {
            // A KMS identity is only reachable when the `gcp` feature is compiled in; clap
            // guarantees at most one decryption source is selected.
            #[cfg(feature = "gcp")]
            let kms_identity = if let Some(key_version) = kms_key_version {
                let client = GcpKmsClient::new(&GcpKmsConfig { key_version }).await?;
                Some(ArchiveIdentity::GcpKms(GcpKmsIdentity::new(client)))
            } else {
                None
            };
            #[cfg(not(feature = "gcp"))]
            let kms_identity: Option<ArchiveIdentity> = None;

            let identity = if let Some(kms_identity) = kms_identity {
                Some(kms_identity)
            } else if let Some(age_secret_key) = age_secret_key {
                Some(ArchiveIdentity::X25519(parse_age_x25519_identity(
                    &age_secret_key,
                )?))
            } else if let Some(identity_file) = identity_file {
                Some(ArchiveIdentity::X25519(
                    read_age_x25519_identity(&identity_file).await?,
                ))
            } else {
                None
            };
            let recovered = recover_replay_records_to_rocksdb_with_optional_decryption(
                &input_root,
                &replay_db_path,
                anchor_block_number,
                anchor_block_hash,
                identity,
                decrypt_concurrency,
            )
            .await?;
            println!("Recovered {recovered} replay records to RocksDB");
        }
    }

    Ok(())
}
