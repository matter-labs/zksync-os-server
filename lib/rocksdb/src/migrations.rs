//! Schema migrations for [`RocksDB`] databases.
//!
//! Each database tracks a schema version in a reserved `__schema_meta` column family that the
//! wrapper manages outside the database's [`NamedColumnFamily`] enum. A database that has never
//! run a migration reads as version 0.
//!
//! Migrations run at open, in ascending `target_version` order, and the version is stamped after
//! each one completes. Opening a database stamped with a version higher than the binary supports
//! fails with an explicit error instead of the decode panic a format mismatch would otherwise
//! produce deep inside a read path.
//!
//! Migrations currently run in blocking mode only: the database is not served until pending
//! migrations finish. A background (resumable, concurrent with serving) mode is planned together
//! with its first consumer — the row-rewrite migration of the replay WAL unification.

use crate::db::{NamedColumnFamily, RocksDB};

/// A single schema migration for a database described by `CF`.
///
/// # Idempotency
///
/// The version is stamped only after [`Migration::run`] returns, so a crash mid-migration means
/// the whole migration reruns on the next open. Implementations MUST therefore be idempotent:
/// re-running over already-migrated rows must be a no-op.
pub trait Migration<CF: NamedColumnFamily>: Send + Sync {
    /// Schema version this migration upgrades the database to. Must be unique per database and
    /// form a strictly increasing sequence starting at 1 in the list passed to
    /// [`RocksDB::run_migrations`].
    fn target_version(&self) -> u32;

    /// Short human-readable name for logs.
    fn name(&self) -> &'static str;

    /// Applies the migration. Runs at open, before the database serves any reads or writes.
    fn run(&self, db: &RocksDB<CF>) -> anyhow::Result<()>;
}

impl<CF: NamedColumnFamily> RocksDB<CF> {
    /// Runs all migrations pending for this database and stamps the schema version after each.
    ///
    /// Consumes and returns `self` to fit the open-time builder chain. `migrations` must be
    /// sorted by strictly increasing `target_version` with no gaps from 1; a database can then
    /// upgrade from any older version, including 0 (never migrated).
    pub fn run_migrations(self, migrations: &[&dyn Migration<CF>]) -> anyhow::Result<Self> {
        for (index, migration) in migrations.iter().enumerate() {
            anyhow::ensure!(
                migration.target_version() == index as u32 + 1,
                "migration list for `{}` must have strictly increasing versions starting at 1; \
                 `{}` at position {index} targets version {}",
                CF::DB_NAME,
                migration.name(),
                migration.target_version(),
            );
        }
        let latest_version = migrations.len() as u32;
        let current_version = self.schema_version()?.unwrap_or(0);
        anyhow::ensure!(
            current_version <= latest_version,
            "database `{}` has schema version {current_version}, but this binary only supports \
             versions up to {latest_version}; downgrading below the binary that migrated this \
             database is not supported",
            CF::DB_NAME,
        );

        for migration in &migrations[current_version as usize..] {
            let started_at = std::time::Instant::now();
            tracing::info!(
                db_name = CF::DB_NAME,
                migration = migration.name(),
                target_version = migration.target_version(),
                "running schema migration"
            );
            migration.run(&self).map_err(|err| {
                err.context(format!(
                    "schema migration `{}` (to version {}) of `{}` failed",
                    migration.name(),
                    migration.target_version(),
                    CF::DB_NAME,
                ))
            })?;
            self.stamp_schema_version(migration.target_version())?;
            tracing::info!(
                db_name = CF::DB_NAME,
                migration = migration.name(),
                target_version = migration.target_version(),
                elapsed = ?started_at.elapsed(),
                "schema migration finished"
            );
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Copy, Clone, Debug)]
    enum TestColumnFamily {
        Data,
    }

    impl NamedColumnFamily for TestColumnFamily {
        const DB_NAME: &'static str = "migrations_test";
        const ALL: &'static [Self] = &[TestColumnFamily::Data];

        fn name(&self) -> &'static str {
            "data"
        }
    }

    struct MarkerMigration {
        version: u32,
        runs: AtomicUsize,
        fail: bool,
    }

    impl MarkerMigration {
        fn new(version: u32) -> Self {
            Self {
                version,
                runs: AtomicUsize::new(0),
                fail: false,
            }
        }
    }

    impl Migration<TestColumnFamily> for MarkerMigration {
        fn target_version(&self) -> u32 {
            self.version
        }

        fn name(&self) -> &'static str {
            "marker"
        }

        fn run(&self, _db: &RocksDB<TestColumnFamily>) -> anyhow::Result<()> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            anyhow::ensure!(!self.fail, "intentional failure");
            Ok(())
        }
    }

    #[test]
    fn runs_pending_migrations_once_and_stamps() {
        let dir = tempfile::tempdir().unwrap();
        let first = MarkerMigration::new(1);
        let second = MarkerMigration::new(2);

        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        assert_eq!(db.schema_version().unwrap(), None);
        let db = db.run_migrations(&[&first, &second]).unwrap();
        assert_eq!(db.schema_version().unwrap(), Some(2));
        assert_eq!(first.runs.load(Ordering::Relaxed), 1);
        assert_eq!(second.runs.load(Ordering::Relaxed), 1);
        drop(db);

        // Reopening runs nothing: both migrations are stamped.
        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        let db = db.run_migrations(&[&first, &second]).unwrap();
        assert_eq!(db.schema_version().unwrap(), Some(2));
        assert_eq!(first.runs.load(Ordering::Relaxed), 1);
        assert_eq!(second.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn applies_only_migrations_newer_than_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let first = MarkerMigration::new(1);
        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        let db = db.run_migrations(&[&first]).unwrap();
        drop(db);

        let second = MarkerMigration::new(2);
        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        db.run_migrations(&[&first, &second]).unwrap();
        assert_eq!(first.runs.load(Ordering::Relaxed), 1);
        assert_eq!(second.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn refuses_database_from_the_future() {
        let dir = tempfile::tempdir().unwrap();
        let first = MarkerMigration::new(1);
        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        let db = db.run_migrations(&[&first]).unwrap();
        drop(db);

        // A binary that only knows an empty migration list must refuse to open.
        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        let err = db.run_migrations(&[]).unwrap_err();
        assert!(
            err.to_string().contains("downgrading below"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn failed_migration_is_not_stamped_and_reruns() {
        let dir = tempfile::tempdir().unwrap();
        let mut failing = MarkerMigration::new(1);
        failing.fail = true;

        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        let err = db.run_migrations(&[&failing]).unwrap_err();
        assert!(
            format!("{err:#}").contains("intentional failure"),
            "unexpected error: {err:#}"
        );
        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        assert_eq!(db.schema_version().unwrap(), None);
        drop(db);

        // The migration reruns after the failure, i.e., it was not stamped.
        let succeeding = MarkerMigration::new(1);
        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        let db = db.run_migrations(&[&succeeding]).unwrap();
        assert_eq!(succeeding.runs.load(Ordering::Relaxed), 1);
        assert_eq!(db.schema_version().unwrap(), Some(1));
    }

    #[test]
    fn rejects_non_contiguous_versions() {
        let dir = tempfile::tempdir().unwrap();
        let wrong = MarkerMigration::new(2);
        let db = RocksDB::<TestColumnFamily>::new(dir.path()).unwrap();
        let err = db.run_migrations(&[&wrong]).unwrap_err();
        assert!(
            err.to_string().contains("strictly increasing"),
            "unexpected error: {err:#}"
        );
    }
}
