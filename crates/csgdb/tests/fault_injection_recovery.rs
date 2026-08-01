#![cfg(feature = "fault-injection")]

use csgdb::{
    CheckpointMode, Database, ErrorCode, FaultRule, FaultSession, FaultTarget, KeySource,
    SecretString,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const DATABASE_SECRET: &str = "deterministic-fault-recovery-secret";
const BASELINE_MARKER: &str = "committed-agent-memory-marker";
static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

struct TestDatabasePath {
    path: PathBuf,
}

impl TestDatabasePath {
    fn new(label: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "csgdb-fault-{label}-{}-{sequence}.db",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDatabasePath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let base = self.path.to_string_lossy();
        let _ = fs::remove_file(format!("{base}-wal"));
        let _ = fs::remove_file(format!("{base}-shm"));
    }
}

#[test]
fn partial_wal_write_does_not_commit_or_damage_baseline() {
    let path = TestDatabasePath::new("partial-wal-write");
    let session = FaultSession::start().expect("start fault session");
    let mut database = open_with_fault_vfs(path.path(), &session);
    initialize(&database);

    let transaction = database.transaction().expect("begin transaction");
    transaction
        .execute_batch("INSERT INTO memory(id, body) VALUES (2, 'must-not-commit')")
        .expect("stage uncommitted row");
    session.arm(
        FaultRule::partial_write(FaultTarget::WriteAheadLog, 1, 7)
            .expect("valid partial-write rule"),
    );
    let error = transaction.commit().expect_err("commit must fail");
    assert_eq!(error.code(), ErrorCode::StorageWriteFailed);
    assert_eq!(session.stats().injections, 1);
    drop(database);
    session.disarm();

    assert_recovered(path.path(), 1);
    assert_encrypted_files_hide_marker(path.path(), BASELINE_MARKER.as_bytes());
}

#[test]
fn wal_sync_failure_does_not_commit_or_damage_baseline() {
    let path = TestDatabasePath::new("wal-sync");
    let session = FaultSession::start().expect("start fault session");
    let mut database = open_with_fault_vfs(path.path(), &session);
    initialize(&database);
    database
        .execute_batch("PRAGMA synchronous = FULL")
        .expect("enable full synchronization");

    let transaction = database.transaction().expect("begin transaction");
    transaction
        .execute_batch("INSERT INTO memory(id, body) VALUES (2, 'must-not-commit')")
        .expect("stage uncommitted row");
    session.arm(FaultRule::sync_error(FaultTarget::WriteAheadLog, 1).expect("valid sync rule"));
    let error = transaction.commit().expect_err("commit sync must fail");
    assert_eq!(error.code(), ErrorCode::StorageSyncFailed);
    assert_eq!(session.stats().injections, 1);
    drop(database);
    session.disarm();

    assert_recovered(path.path(), 1);
}

#[test]
fn failed_main_file_checkpoint_preserves_committed_wal_data() {
    let path = TestDatabasePath::new("checkpoint-write");
    let session = FaultSession::start().expect("start fault session");
    let database = open_with_fault_vfs(path.path(), &session);
    initialize(&database);
    database
        .execute_batch("INSERT INTO memory(id, body) VALUES (2, 'committed-before-checkpoint')")
        .expect("commit second row to WAL");

    session.arm(FaultRule::write_error(FaultTarget::MainDatabase, 1).expect("valid write rule"));
    let error = database
        .checkpoint(CheckpointMode::Full)
        .expect_err("checkpoint write must fail");
    assert_eq!(error.code(), ErrorCode::StorageWriteFailed);
    assert_eq!(session.stats().injections, 1);
    drop(database);
    session.disarm();

    assert_recovered(path.path(), 2);
}

#[test]
fn failed_wal_truncate_preserves_committed_data() {
    let path = TestDatabasePath::new("wal-truncate");
    let session = FaultSession::start().expect("start fault session");
    let database = open_with_fault_vfs(path.path(), &session);
    initialize(&database);
    database
        .execute_batch("INSERT INTO memory(id, body) VALUES (2, 'committed-before-truncate')")
        .expect("commit second row to WAL");

    session.arm(
        FaultRule::truncate_error(FaultTarget::WriteAheadLog, 1).expect("valid truncate rule"),
    );
    let error = database
        .checkpoint(CheckpointMode::Truncate)
        .expect_err("WAL truncate must fail");
    assert_eq!(error.code(), ErrorCode::StorageTruncateFailed);
    assert_eq!(session.stats().injections, 1);
    drop(database);
    session.disarm();

    assert_recovered(path.path(), 2);
}

fn open_with_fault_vfs(path: &Path, session: &FaultSession) -> Database {
    Database::builder(path)
        .key(KeySource::Passphrase(SecretString::new(DATABASE_SECRET)))
        .vfs(session.vfs_name())
        .open()
        .expect("open encrypted database through fault VFS")
}

fn open_normally(path: &Path) -> Database {
    Database::open_with_passphrase(path, DATABASE_SECRET).expect("reopen encrypted database")
}

fn initialize(database: &Database) {
    database
        .set_wal_autocheckpoint(0)
        .expect("disable auto-checkpoint");
    database
        .execute_batch(
            "CREATE TABLE memory(id INTEGER PRIMARY KEY, body TEXT NOT NULL);
             INSERT INTO memory(id, body) VALUES (1, 'committed-agent-memory-marker')",
        )
        .expect("create committed baseline");
}

fn assert_recovered(path: &Path, expected_rows: i64) {
    let database = open_normally(path);
    assert_eq!(
        database
            .query_i64("SELECT count(*) FROM memory")
            .expect("count recovered rows"),
        expected_rows
    );
    assert_eq!(
        database
            .query_i64("SELECT count(*) FROM memory WHERE id = 1 AND body = 'committed-agent-memory-marker'")
            .expect("read committed baseline"),
        1
    );
    let mut statement = database
        .prepare("PRAGMA integrity_check")
        .expect("prepare integrity check");
    let mut rows = statement.query(&[]).expect("run integrity check");
    let row = rows
        .next_row()
        .expect("read integrity result")
        .expect("row");
    assert_eq!(row.get_text(0).expect("integrity text"), "ok");
    assert!(rows.next_row().expect("finish integrity rows").is_none());
    drop(rows);
    drop(statement);
    database.close().expect("close recovered database");
}

fn assert_encrypted_files_hide_marker(path: &Path, marker: &[u8]) {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.to_string_lossy())),
    ] {
        let Ok(bytes) = fs::read(candidate) else {
            continue;
        };
        assert!(
            !bytes.windows(marker.len()).any(|window| window == marker),
            "encrypted storage leaked a plaintext marker"
        );
    }
}
