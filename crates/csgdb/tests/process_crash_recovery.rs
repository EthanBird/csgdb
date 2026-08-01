use csgdb::{Collection, CollectionCrud, Database, MigrationStatus, SchemaRegistration};
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

const WORKER_STAGE: &str = "CSGDB_TEST_CRASH_STAGE";
const WORKER_DATABASE: &str = "CSGDB_TEST_CRASH_DATABASE";
const DATABASE_SECRET: &str = "process-crash-recovery-secret";
const CRASH_MARKER: &str = "CSGDB_CRASH_POINT_REACHED:";
const PLAINTEXT_MARKER: &str = "abrupt-recovery-plaintext-marker";

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Collection)]
#[csgdb(collection = "agent.crash-memory", table = "crash_memory", version = 1)]
struct CrashMemoryV1 {
    #[csgdb(id = "agent.crash-memory.id", column = "id", primary_key)]
    id: i64,
    #[csgdb(id = "agent.crash-memory.text", column = "text")]
    text: String,
}

#[derive(Clone, Debug, PartialEq, Collection)]
#[csgdb(
    collection = "agent.crash-memory",
    table = "crash_memory",
    version = 2,
    index(
        id = "agent.crash-memory.score",
        name = "idx_crash_memory_score",
        field = "agent.crash-memory.score"
    )
)]
struct CrashMemoryV2 {
    #[csgdb(id = "agent.crash-memory.id", column = "id", primary_key)]
    id: i64,
    #[csgdb(id = "agent.crash-memory.text", column = "text")]
    text: String,
    #[csgdb(id = "agent.crash-memory.score", column = "score")]
    score: Option<f64>,
}

struct TestDatabasePath {
    path: PathBuf,
}

impl TestDatabasePath {
    fn new() -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "csgdb-process-crash-{}-{sequence}.db",
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
#[ignore = "invoked by abrupt_process_exit_recovers_transactions_and_migrations"]
fn process_crash_worker() {
    let Ok(stage) = std::env::var(WORKER_STAGE) else {
        return;
    };
    let path = std::env::var_os(WORKER_DATABASE).expect("worker database path");
    let mut database =
        Database::open_with_passphrase(path, DATABASE_SECRET).expect("worker opens database");

    match stage.as_str() {
        "uncommitted-write" => {
            let transaction = database.transaction().expect("begin write");
            transaction
                .insert(&CrashMemoryV1 {
                    id: 2,
                    text: "must be rolled back".to_owned(),
                })
                .expect("insert uncommitted row");
            abort_at(&stage);
        }
        "mid-migration" => {
            let _ = database.migrate_collection::<CrashMemoryV1, CrashMemoryV2, _>(
                "add-score-v2",
                |transaction| {
                    transaction.execute_batch(
                        "ALTER TABLE crash_memory ADD COLUMN score REAL;
                         UPDATE crash_memory SET text = 'must be rolled back' WHERE id = 1",
                    )?;
                    abort_at(&stage)
                },
            );
            unreachable!("mid-migration worker must abort");
        }
        "after-migration-commit" => {
            let status = database
                .migrate_collection::<CrashMemoryV1, CrashMemoryV2, _>(
                    "add-score-v2",
                    |transaction| {
                        transaction.execute_batch(
                            "ALTER TABLE crash_memory ADD COLUMN score REAL;
                             UPDATE crash_memory SET score = 0.95 WHERE id = 1",
                        )
                    },
                )
                .expect("commit migration");
            assert_eq!(status, MigrationStatus::Applied);
            abort_at(&stage);
        }
        _ => panic!("unknown crash worker stage"),
    }
}

#[test]
fn abrupt_process_exit_recovers_transactions_and_migrations() {
    let path = TestDatabasePath::new();
    let mut database =
        Database::open_with_passphrase(path.path(), DATABASE_SECRET).expect("create database");
    database
        .register_collection::<CrashMemoryV1>()
        .expect("register v1");
    database
        .insert(&CrashMemoryV1 {
            id: 1,
            text: PLAINTEXT_MARKER.to_owned(),
        })
        .expect("insert committed baseline");
    database.close().expect("close baseline");

    assert_worker_aborted(
        &run_worker(path.path(), "uncommitted-write"),
        "uncommitted-write",
    );
    let database = reopen_v1(path.path());
    assert_eq!(database.get::<CrashMemoryV1>(&2).expect("read row 2"), None);
    assert_integrity(&database);
    database.close().expect("close after write recovery");

    assert_worker_aborted(&run_worker(path.path(), "mid-migration"), "mid-migration");
    let database = reopen_v1(path.path());
    assert_eq!(
        database
            .get::<CrashMemoryV1>(&1)
            .expect("read baseline")
            .expect("baseline row")
            .text,
        PLAINTEXT_MARKER
    );
    assert_eq!(
        database
            .query_i64("SELECT count(*) FROM __csgdb_migration")
            .expect("migration history"),
        0
    );
    assert_integrity(&database);
    database.close().expect("close after migration rollback");

    assert_worker_aborted(
        &run_worker(path.path(), "after-migration-commit"),
        "after-migration-commit",
    );
    let mut database =
        Database::open_with_passphrase(path.path(), DATABASE_SECRET).expect("reopen v2");
    assert_eq!(
        database
            .register_collection::<CrashMemoryV2>()
            .expect("validate committed v2"),
        SchemaRegistration::AlreadyRegistered
    );
    assert_eq!(
        database
            .get::<CrashMemoryV2>(&1)
            .expect("read v2")
            .expect("migrated row")
            .score,
        Some(0.95)
    );
    assert_eq!(
        database
            .query_i64("SELECT count(*) FROM __csgdb_migration")
            .expect("migration history"),
        1
    );
    assert_eq!(
        database
            .migrate_collection::<CrashMemoryV1, CrashMemoryV2, _>("add-score-v2", |_| panic!(
                "committed migration callback must not repeat"
            ),)
            .expect("idempotent recovery"),
        MigrationStatus::AlreadyApplied
    );
    assert_integrity(&database);
    database.close().expect("close v2");
    assert_encrypted_files_hide_marker(path.path(), PLAINTEXT_MARKER.as_bytes());
}

fn reopen_v1(path: &Path) -> Database {
    let mut database = Database::open_with_passphrase(path, DATABASE_SECRET).expect("reopen v1");
    assert_eq!(
        database
            .register_collection::<CrashMemoryV1>()
            .expect("validate v1"),
        SchemaRegistration::AlreadyRegistered
    );
    database
}

fn run_worker(path: &Path, stage: &str) -> Output {
    Command::new(std::env::current_exe().expect("current test executable"))
        .arg("--exact")
        .arg("process_crash_worker")
        .arg("--ignored")
        .arg("--nocapture")
        .env(WORKER_STAGE, stage)
        .env(WORKER_DATABASE, path)
        .output()
        .expect("run crash worker")
}

fn assert_worker_aborted(output: &Output, stage: &str) {
    assert!(!output.status.success(), "crash worker unexpectedly exited");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("{CRASH_MARKER}{stage}")),
        "worker failed before reaching crash point; stdout={stdout:?}, stderr={:?}",
        String::from_utf8_lossy(&output.stderr),
    );
}

fn abort_at(stage: &str) -> ! {
    println!("{CRASH_MARKER}{stage}");
    io::stdout().flush().expect("flush crash marker");
    std::process::abort()
}

fn assert_integrity(database: &Database) {
    let mut statement = database
        .prepare("PRAGMA integrity_check")
        .expect("prepare integrity");
    let mut rows = statement.query(&[]).expect("run integrity check");
    let row = rows.next_row().expect("read integrity row").expect("row");
    assert_eq!(row.get_text(0).expect("integrity result"), "ok");
    assert!(rows.next_row().expect("finish integrity rows").is_none());
}

fn assert_encrypted_files_hide_marker(path: &Path, marker: &[u8]) {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.to_string_lossy())),
        PathBuf::from(format!("{}-shm", path.to_string_lossy())),
    ] {
        let Ok(bytes) = fs::read(&candidate) else {
            continue;
        };
        assert!(
            !bytes.windows(marker.len()).any(|window| window == marker),
            "plaintext marker leaked into {}",
            candidate.display(),
        );
    }
}
