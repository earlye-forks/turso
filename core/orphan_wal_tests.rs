use super::*;
use crate::io::{FileId, FileSyncType};
use crate::storage::sqlite3_ondisk::DatabaseHeader;
use std::path::{Path, PathBuf};
#[cfg(all(
    host_shared_wal,
    any(not(target_os = "windows"), feature = "experimental_win_iocp")
))]
use std::process::Command;
use std::sync::{Mutex, OnceLock};

#[cfg(all(
    host_shared_wal,
    any(not(target_os = "windows"), feature = "experimental_win_iocp")
))]
const MULTIPROCESS_HOLD_OPEN_CHILD_TEST: &str =
    "orphan_wal_tests::multiprocess_hold_open_child_process";

const DISCARD_ZERO_BYTES: OrphanWalPolicy = OrphanWalPolicy::Discard {
    empty: EmptyDb::ZeroBytes,
    read_only: ReadOnlyOrphanWal::Ignore,
};
const DISCARD_ONE_BYTE: OrphanWalPolicy = OrphanWalPolicy::Discard {
    empty: EmptyDb::OneByte,
    read_only: ReadOnlyOrphanWal::Ignore,
};
const DISCARD_INVALID_HEADER: OrphanWalPolicy = OrphanWalPolicy::Discard {
    empty: EmptyDb::InvalidHeader,
    read_only: ReadOnlyOrphanWal::Ignore,
};
const OLD_TABLES: [&str; 3] = ["old_t", "old_u", "old_v"];

#[test]
fn replay_policy_keeps_upstream_open_behaviour() {
    let dir = tempfile::tempdir().unwrap();
    let paths = DbPaths::new(dir.path());
    place_files(&paths, None, Some(orphan_wal()));
    let io = RecordingIo::new(&paths, true);

    let db = open_db(
        io.clone(),
        &paths,
        OpenFlags::default(),
        OrphanWalPolicy::Replay,
    )
    .unwrap();
    let conn = db.connect().unwrap();
    assert!(table_names(&conn).is_empty());
    assert_eq!(io.count(IoEvent::RemoveFile), 1);
    assert_eq!(io.count(IoEvent::DbSync), 0);
    assert_eq!(io.count(IoEvent::ParentDirSync), 0);

    rollback_create_table(&conn);
    assert_eq!(file_len(&paths.db), 4096);
    assert_eq!(io.count(IoEvent::DbSync), 1);
    assert_eq!(io.count(IoEvent::ParentDirSync), 0);
    assert!(table_names(&db.connect().unwrap()).is_empty());
    drop(conn);
    drop(db);

    let db = open_db(
        platform_io(),
        &paths,
        OpenFlags::default(),
        OrphanWalPolicy::Replay,
    )
    .unwrap();
    let conn = db.connect().unwrap();
    assert!(table_names(&conn).is_empty());
    assert_eq!(integrity_check(&conn), "ok");
}

#[test]
fn zero_bytes_rule_discards_wal_beside_absent_or_zero_byte_db() {
    for db_contents in [None, Some(&[][..])] {
        let dir = tempfile::tempdir().unwrap();
        let paths = DbPaths::new(dir.path());
        place_files(&paths, db_contents, Some(orphan_wal()));
        let io = RecordingIo::new(&paths, true);

        let db = open_db(io.clone(), &paths, OpenFlags::default(), DISCARD_ZERO_BYTES).unwrap();
        assert_eq!(file_len(&paths.wal), 0);
        assert_eq!(io.count(IoEvent::RemoveFile), 0);
        let conn = db.connect().unwrap();
        assert!(table_names(&conn).is_empty());
        rollback_create_table(&conn);
        assert!(table_names(&db.connect().unwrap()).is_empty());
        drop(conn);
        drop(db);

        assert_reopens_empty(&paths, DISCARD_ZERO_BYTES);
    }
}

#[test]
fn zero_bytes_rule_keeps_wal_beside_one_byte_db() {
    let dir = tempfile::tempdir().unwrap();
    let paths = DbPaths::new(dir.path());
    place_files(&paths, Some(&[0]), Some(orphan_wal()));

    let result = open_db(
        platform_io(),
        &paths,
        OpenFlags::default(),
        DISCARD_ZERO_BYTES,
    );
    drop(result);
    assert_eq!(std::fs::read(&paths.db).unwrap(), vec![0]);
    assert_eq!(std::fs::read(&paths.wal).unwrap(), orphan_wal());
}

#[test]
fn one_byte_rule_discards_wal_beside_one_byte_db() {
    let dir = tempfile::tempdir().unwrap();
    let paths = DbPaths::new(dir.path());
    place_files(&paths, Some(&[0]), Some(orphan_wal()));

    let db = open_db(
        platform_io(),
        &paths,
        OpenFlags::default(),
        DISCARD_ONE_BYTE,
    )
    .unwrap();
    assert_eq!(file_len(&paths.db), 0);
    assert_eq!(file_len(&paths.wal), 0);
    let conn = db.connect().unwrap();
    assert!(table_names(&conn).is_empty());
    rollback_create_table(&conn);
    drop(conn);
    drop(db);

    assert_reopens_empty(&paths, DISCARD_ONE_BYTE);
}

#[test]
fn invalid_header_rule_truncates_zeroed_db_with_or_without_wal() {
    for wal in [Some(orphan_wal()), None] {
        let dir = tempfile::tempdir().unwrap();
        let paths = DbPaths::new(dir.path());
        place_files(&paths, Some(&[0; 4096]), wal);

        let db = open_db(
            platform_io(),
            &paths,
            OpenFlags::default(),
            DISCARD_INVALID_HEADER,
        )
        .unwrap();
        assert_eq!(file_len(&paths.db), 0);
        assert_eq!(file_len(&paths.wal), 0);
        let conn = db.connect().unwrap();
        assert!(table_names(&conn).is_empty());
        drop(conn);
        drop(db);

        assert_reopens_empty(&paths, DISCARD_INVALID_HEADER);
    }
}

#[test]
fn invalid_header_rule_treats_db_shorter_than_512_bytes_as_invalid() {
    let mut short_db = vec![0u8; 200];
    short_db[..DatabaseHeader::SIZE]
        .copy_from_slice(bytemuck::bytes_of(&DatabaseHeader::default()));

    let dir = tempfile::tempdir().unwrap();
    let paths = DbPaths::new(dir.path());
    place_files(&paths, Some(&short_db), Some(orphan_wal()));
    let result = open_db(
        platform_io(),
        &paths,
        OpenFlags::default(),
        DISCARD_ONE_BYTE,
    );
    drop(result);
    assert_eq!(std::fs::read(&paths.db).unwrap(), short_db);
    assert_eq!(std::fs::read(&paths.wal).unwrap(), orphan_wal());

    let err = open_db(
        platform_io(),
        &paths,
        OpenFlags::ReadOnly,
        DISCARD_INVALID_HEADER,
    )
    .expect_err("a read-only open of a db shorter than 512 bytes must fail");
    assert!(matches!(err, LimboError::Corrupt(_)), "{err:?}");
    assert_eq!(std::fs::read(&paths.db).unwrap(), short_db);
    assert_eq!(std::fs::read(&paths.wal).unwrap(), orphan_wal());

    let db = open_db(
        platform_io(),
        &paths,
        OpenFlags::default(),
        DISCARD_INVALID_HEADER,
    )
    .unwrap();
    assert_eq!(file_len(&paths.db), 0);
    assert_eq!(file_len(&paths.wal), 0);
    assert!(table_names(&db.connect().unwrap()).is_empty());
}

#[test]
fn valid_db_with_wal_replays_under_every_empty_db_rule() {
    let source = tempfile::tempdir().unwrap();
    let source_paths = DbPaths::new(source.path());
    let (db_bytes, wal_bytes) = {
        let db = open_db(
            platform_io(),
            &source_paths,
            OpenFlags::default(),
            OrphanWalPolicy::Replay,
        )
        .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE kept(x)").unwrap();
        conn.execute("INSERT INTO kept VALUES (1), (2), (3)")
            .unwrap();
        (
            std::fs::read(&source_paths.db).unwrap(),
            std::fs::read(&source_paths.wal).unwrap(),
        )
    };
    assert!(!wal_bytes.is_empty());

    for policy in [DISCARD_ZERO_BYTES, DISCARD_ONE_BYTE, DISCARD_INVALID_HEADER] {
        let dir = tempfile::tempdir().unwrap();
        let paths = DbPaths::new(dir.path());
        place_files(&paths, Some(&db_bytes), Some(&wal_bytes));

        let db = open_db(platform_io(), &paths, OpenFlags::default(), policy).unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(table_names(&conn), vec!["kept".to_string()], "{policy:?}");
        assert_eq!(query_i64(&conn, "SELECT count(*) FROM kept"), 3);
        assert_eq!(integrity_check(&conn), "ok");
    }
}

#[test]
fn read_only_ignore_reads_empty_and_leaves_files_untouched() {
    for (db_contents, empty) in [(&[][..], EmptyDb::ZeroBytes), (&[0][..], EmptyDb::OneByte)] {
        let dir = tempfile::tempdir().unwrap();
        let paths = DbPaths::new(dir.path());
        place_files(&paths, Some(db_contents), Some(orphan_wal()));
        let policy = OrphanWalPolicy::Discard {
            empty,
            read_only: ReadOnlyOrphanWal::Ignore,
        };

        let db = open_db(platform_io(), &paths, OpenFlags::ReadOnly, policy).unwrap();
        let conn = db.connect().unwrap();
        assert!(table_names(&conn).is_empty(), "{empty:?}");
        drop(conn);
        drop(db);

        assert_eq!(std::fs::read(&paths.db).unwrap(), db_contents);
        assert_eq!(std::fs::read(&paths.wal).unwrap(), orphan_wal());
    }
}

#[test]
fn read_only_replay_matches_replay_policy() {
    let discard_read_only_replay = OrphanWalPolicy::Discard {
        empty: EmptyDb::ZeroBytes,
        read_only: ReadOnlyOrphanWal::Replay,
    };
    let outcomes: Vec<_> = [OrphanWalPolicy::Replay, discard_read_only_replay]
        .into_iter()
        .map(|policy| {
            let dir = tempfile::tempdir().unwrap();
            let paths = DbPaths::new(dir.path());
            place_files(&paths, Some(&[]), Some(orphan_wal()));
            let db = open_db(platform_io(), &paths, OpenFlags::ReadOnly, policy).unwrap();
            let tables = table_names(&db.connect().unwrap());
            drop(db);
            (
                tables,
                std::fs::read(&paths.db).unwrap(),
                std::fs::read(&paths.wal).unwrap() == orphan_wal(),
            )
        })
        .collect();
    assert_eq!(outcomes[0], outcomes[1]);
}

#[test]
fn read_only_open_of_invalid_header_db_fails_without_touching_files() {
    for read_only in [ReadOnlyOrphanWal::Ignore, ReadOnlyOrphanWal::Replay] {
        for db_contents in [&[0; 4096][..], &[7; 50][..], &[0][..]] {
            let dir = tempfile::tempdir().unwrap();
            let paths = DbPaths::new(dir.path());
            place_files(&paths, Some(db_contents), Some(orphan_wal()));
            let policy = OrphanWalPolicy::Discard {
                empty: EmptyDb::InvalidHeader,
                read_only,
            };

            let err = open_db(platform_io(), &paths, OpenFlags::ReadOnly, policy)
                .expect_err("a read-only open of a db with an invalid header must fail");
            assert!(
                matches!(err, LimboError::Corrupt(ref msg) if msg.contains("valid database header")),
                "{err:?}"
            );
            assert_eq!(std::fs::read(&paths.db).unwrap(), db_contents);
            assert_eq!(std::fs::read(&paths.wal).unwrap(), orphan_wal());
        }
    }
}

#[test]
fn discard_syncs_db_file_and_parent_dir_before_first_wal_write() {
    let dir = tempfile::tempdir().unwrap();
    let paths = DbPaths::new(dir.path());
    place_files(&paths, None, Some(orphan_wal()));
    let io = RecordingIo::new(&paths, true);

    let db = open_db(io.clone(), &paths, OpenFlags::default(), DISCARD_ZERO_BYTES).unwrap();
    let conn = db.connect().unwrap();
    io.clear();
    conn.execute("CREATE TABLE t(x)").unwrap();

    let events = io.events();
    let first_wal_write = events
        .iter()
        .position(|event| *event == IoEvent::WalWrite)
        .expect("the commit must write the WAL");
    assert_eq!(
        &events[..first_wal_write],
        &[IoEvent::DbWrite, IoEvent::DbSync, IoEvent::ParentDirSync]
    );
}

#[test]
fn discard_fails_open_of_empty_db_when_io_cannot_sync_parent_dir() {
    let dir = tempfile::tempdir().unwrap();
    let empty_paths = DbPaths::new(dir.path());
    let io = RecordingIo::new(&empty_paths, false);

    let err = open_db(
        io.clone(),
        &empty_paths,
        OpenFlags::default(),
        DISCARD_ZERO_BYTES,
    )
    .expect_err("Discard must refuse an IO that cannot sync the parent directory");
    assert!(
        matches!(err, LimboError::InvalidArgument(ref msg) if msg.contains("sync_parent_dir")),
        "{err:?}"
    );

    let filled_paths = DbPaths {
        db: dir.path().join("filled.db"),
        wal: dir.path().join("filled.db-wal"),
    };
    {
        let db = open_db(
            io.clone(),
            &filled_paths,
            OpenFlags::default(),
            OrphanWalPolicy::Replay,
        )
        .unwrap();
        db.connect()
            .unwrap()
            .execute("CREATE TABLE kept(x)")
            .unwrap();
    }
    let db = open_db(io, &filled_paths, OpenFlags::default(), DISCARD_ZERO_BYTES).unwrap();
    assert_eq!(
        table_names(&db.connect().unwrap()),
        vec!["kept".to_string()]
    );
}

#[test]
fn discard_fails_registry_bypassing_open_without_touching_files() {
    let dir = tempfile::tempdir().unwrap();
    let paths = DbPaths::new(dir.path());
    place_files(&paths, Some(&[]), Some(orphan_wal()));
    let io = platform_io();
    let path = paths.db.to_str().unwrap();

    for policy in [DISCARD_ZERO_BYTES, DISCARD_ONE_BYTE, DISCARD_INVALID_HEADER] {
        let err = open_bypassing_registry(io.clone(), path, policy)
            .expect_err("Discard must refuse an open that skips the registry");
        assert!(
            matches!(err, LimboError::InvalidArgument(ref msg) if msg.contains("registry")),
            "{err:?}"
        );
        assert_eq!(file_len(&paths.db), 0);
        assert_eq!(std::fs::read(&paths.wal).unwrap(), orphan_wal());
    }

    let db = open_bypassing_registry(io, path, OrphanWalPolicy::Replay).unwrap();
    assert!(table_names(&db.connect().unwrap()).is_empty());
}

#[cfg(feature = "conn_raw_api")]
#[test]
fn discard_fails_do_open_before_creating_db_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = DbPaths::new(dir.path());

    let err = Database::do_open(
        platform_io(),
        paths.db.to_str().unwrap(),
        OpenOptions::new(Arc::new(SqliteDialect))
            .db_opts(DatabaseOpts::new().with_orphan_wal_policy(DISCARD_ZERO_BYTES)),
    )
    .expect_err("Discard must refuse an open that skips the registry");
    assert!(
        matches!(err, LimboError::InvalidArgument(ref msg) if msg.contains("registry")),
        "{err:?}"
    );
    assert!(!paths.db.exists());
    assert!(!paths.wal.exists());
}

#[cfg(all(
    host_shared_wal,
    any(not(target_os = "windows"), feature = "experimental_win_iocp")
))]
#[test]
fn multiprocess_discard_fails_when_another_process_is_attached() {
    let dir = tempfile::tempdir().unwrap();
    let paths = DbPaths::new(dir.path());
    place_files(&paths, Some(&[]), None);
    let ready_file = dir.path().join("child-ready");
    let release_file = dir.path().join("child-release");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg(MULTIPROCESS_HOLD_OPEN_CHILD_TEST)
        .arg("--exact")
        .arg("--nocapture")
        .env("TURSO_ORPHAN_WAL_DB_PATH", &paths.db)
        .env("TURSO_ORPHAN_WAL_READY_FILE", &ready_file)
        .env("TURSO_ORPHAN_WAL_RELEASE_FILE", &release_file)
        .spawn()
        .unwrap();
    wait_for_file(&ready_file);
    std::fs::write(&paths.wal, orphan_wal()).unwrap();

    let result = Database::open_file_with_flags(
        platform_io(),
        paths.db.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new()
            .with_multiprocess_wal(true)
            .with_orphan_wal_policy(DISCARD_ZERO_BYTES),
        None,
        Arc::new(SqliteDialect),
    );

    std::fs::write(&release_file, b"release").unwrap();
    assert!(child.wait().unwrap().success());
    let err = result.expect_err("Discard must not touch files another process is attached to");
    assert!(
        matches!(err, LimboError::LockingError(ref msg) if msg.contains("orphan WAL")),
        "{err:?}"
    );
    assert_eq!(file_len(&paths.db), 0);
    assert_eq!(std::fs::read(&paths.wal).unwrap(), orphan_wal());
}

#[cfg(all(
    host_shared_wal,
    any(not(target_os = "windows"), feature = "experimental_win_iocp")
))]
#[test]
fn multiprocess_hold_open_child_process() {
    let Some(db_path) = std::env::var_os("TURSO_ORPHAN_WAL_DB_PATH") else {
        return;
    };
    let ready_file = PathBuf::from(std::env::var_os("TURSO_ORPHAN_WAL_READY_FILE").unwrap());
    let release_file = PathBuf::from(std::env::var_os("TURSO_ORPHAN_WAL_RELEASE_FILE").unwrap());

    let db = Database::open_file_with_flags(
        platform_io(),
        db_path.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new().with_multiprocess_wal(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();

    std::fs::write(&ready_file, b"ready").unwrap();
    wait_for_file(&release_file);
    drop(db);
}

struct DbPaths {
    db: PathBuf,
    wal: PathBuf,
}

impl DbPaths {
    fn new(dir: &Path) -> Self {
        Self {
            db: dir.join("g.db"),
            wal: dir.join("g.db-wal"),
        }
    }
}

fn orphan_wal() -> &'static [u8] {
    static ORPHAN_WAL: OnceLock<Vec<u8>> = OnceLock::new();
    ORPHAN_WAL.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let paths = DbPaths::new(dir.path());
        let db = Database::open_file(
            platform_io(),
            paths.db.to_str().unwrap(),
            Arc::new(SqliteDialect),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE old_t(x, b)").unwrap();
        conn.execute("CREATE TABLE old_u(z)").unwrap();
        conn.execute("CREATE TABLE old_v(w)").unwrap();
        for i in 0..300 {
            conn.execute(format!("INSERT INTO old_t VALUES ({i}, randomblob(200))"))
                .unwrap();
            conn.execute("INSERT INTO old_v VALUES (randomblob(300))")
                .unwrap();
        }
        assert_eq!(table_names(&conn), OLD_TABLES);
        let wal = std::fs::read(&paths.wal).unwrap();
        assert!(wal.len() > 2_000_000, "WAL holds {} bytes", wal.len());
        wal
    })
}

fn place_files(paths: &DbPaths, db_contents: Option<&[u8]>, wal_contents: Option<&[u8]>) {
    for (path, contents) in [(&paths.db, db_contents), (&paths.wal, wal_contents)] {
        match contents {
            Some(contents) => std::fs::write(path, contents).unwrap(),
            None => assert!(!path.exists()),
        }
    }
}

fn platform_io() -> Arc<dyn IO> {
    Arc::new(PlatformIO::new().unwrap())
}

fn open_db(
    io: Arc<dyn IO>,
    paths: &DbPaths,
    flags: OpenFlags,
    policy: OrphanWalPolicy,
) -> Result<Arc<Database>> {
    Database::open_file_with_flags(
        io,
        paths.db.to_str().unwrap(),
        flags,
        DatabaseOpts::new().with_orphan_wal_policy(policy),
        None,
        Arc::new(SqliteDialect),
    )
}

fn open_bypassing_registry(
    io: Arc<dyn IO>,
    path: &str,
    policy: OrphanWalPolicy,
) -> Result<Arc<Database>> {
    let file = io.open_file(path, OpenFlags::default(), true)?;
    let options = OpenOptions::new(Arc::new(SqliteDialect))
        .storage(Arc::new(crate::storage::database::DatabaseFile::new(file)))
        .db_opts(DatabaseOpts::new().with_orphan_wal_policy(policy));
    let mut state = OpenDbAsyncState::new();
    loop {
        match Database::do_open_async(&mut state, io.clone(), path, &options)? {
            IOResult::Done(db) => return Ok(db),
            IOResult::IO(completion) => completion.wait(&*io)?,
        }
    }
}

fn rollback_create_table(conn: &Arc<Connection>) {
    conn.execute("BEGIN").unwrap();
    conn.execute("CREATE TABLE z(q)").unwrap();
    conn.execute("ROLLBACK").unwrap();
}

fn assert_reopens_empty(paths: &DbPaths, policy: OrphanWalPolicy) {
    let db = open_db(platform_io(), paths, OpenFlags::default(), policy).unwrap();
    let conn = db.connect().unwrap();
    assert!(table_names(&conn).is_empty());
    assert_eq!(integrity_check(&conn), "ok");
}

fn table_names(conn: &Arc<Connection>) -> Vec<String> {
    query_rows(conn, "SELECT name FROM sqlite_schema ORDER BY name")
        .into_iter()
        .map(|row| row[0].to_text().unwrap().to_string())
        .collect()
}

fn integrity_check(conn: &Arc<Connection>) -> String {
    query_rows(conn, "PRAGMA integrity_check")[0][0]
        .to_text()
        .unwrap()
        .to_string()
}

fn query_i64(conn: &Arc<Connection>, sql: &str) -> i64 {
    match query_rows(conn, sql)[0][0] {
        Value::Numeric(Numeric::Integer(value)) => value,
        ref other => panic!("expected an integer from {sql}, got {other:?}"),
    }
}

fn query_rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut rows = Vec::new();
    stmt.run_with_row_callback(|row| {
        rows.push(row.get_values().cloned().collect());
        Ok(())
    })
    .unwrap();
    rows
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

#[cfg(all(
    host_shared_wal,
    any(not(target_os = "windows"), feature = "experimental_win_iocp")
))]
fn wait_for_file(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IoEvent {
    DbWrite,
    DbSync,
    WalWrite,
    ParentDirSync,
    RemoveFile,
}

struct RecordingIo {
    inner: Arc<dyn IO>,
    db_path: String,
    wal_path: String,
    supports_parent_dir_sync: bool,
    events: Arc<Mutex<Vec<IoEvent>>>,
}

impl RecordingIo {
    fn new(paths: &DbPaths, supports_parent_dir_sync: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: platform_io(),
            db_path: paths.db.to_str().unwrap().to_string(),
            wal_path: paths.wal.to_str().unwrap().to_string(),
            supports_parent_dir_sync,
            events: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn events(&self) -> Vec<IoEvent> {
        self.events.lock().unwrap().clone()
    }

    fn count(&self, event: IoEvent) -> usize {
        self.events().into_iter().filter(|e| *e == event).count()
    }

    fn clear(&self) {
        self.events.lock().unwrap().clear();
    }
}

impl Clock for RecordingIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.inner.current_time_monotonic()
    }

    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.inner.current_time_wall_clock()
    }
}

impl IO for RecordingIo {
    fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
        let inner = self.inner.open_file(path, flags, direct)?;
        let kind = if path == self.db_path {
            RecordedFile::Db
        } else if path == self.wal_path {
            RecordedFile::Wal
        } else {
            RecordedFile::Other
        };
        Ok(Arc::new(RecordingFile {
            inner,
            kind,
            events: self.events.clone(),
        }))
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.events.lock().unwrap().push(IoEvent::RemoveFile);
        self.inner.remove_file(path)
    }

    fn sync_parent_dir(&self, path: &str, c: Completion) -> Result<Completion> {
        if !self.supports_parent_dir_sync {
            return Err(LimboError::IoExtensionUnsupported("sync_parent_dir"));
        }
        self.events.lock().unwrap().push(IoEvent::ParentDirSync);
        self.inner.sync_parent_dir(path, c)
    }

    fn step(&self) -> Result<()> {
        self.inner.step()
    }

    fn drain_completions(&self, completions: &[Completion]) -> Result<()> {
        self.inner.drain_completions(completions)
    }

    fn cancel(&self, completions: &[Completion]) -> Result<()> {
        self.inner.cancel(completions)
    }

    fn file_id(&self, path: &str) -> Result<FileId> {
        self.inner.file_id(path)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordedFile {
    Db,
    Wal,
    Other,
}

struct RecordingFile {
    inner: Arc<dyn File>,
    kind: RecordedFile,
    events: Arc<Mutex<Vec<IoEvent>>>,
}

impl RecordingFile {
    fn record_write(&self) {
        let event = match self.kind {
            RecordedFile::Db => IoEvent::DbWrite,
            RecordedFile::Wal => IoEvent::WalWrite,
            RecordedFile::Other => return,
        };
        self.events.lock().unwrap().push(event);
    }
}

impl File for RecordingFile {
    fn lock_file(&self, exclusive: bool) -> Result<()> {
        self.inner.lock_file(exclusive)
    }

    fn unlock_file(&self) -> Result<()> {
        self.inner.unlock_file()
    }

    fn pread(&self, pos: u64, c: Completion) -> Result<Completion> {
        self.inner.pread(pos, c)
    }

    fn pwrite(&self, pos: u64, buffer: Arc<Buffer>, c: Completion) -> Result<Completion> {
        self.record_write();
        self.inner.pwrite(pos, buffer, c)
    }

    fn pwritev(&self, pos: u64, buffers: Vec<Arc<Buffer>>, c: Completion) -> Result<Completion> {
        self.record_write();
        self.inner.pwritev(pos, buffers, c)
    }

    fn sync(&self, c: Completion, sync_type: FileSyncType) -> Result<Completion> {
        if self.kind == RecordedFile::Db {
            self.events.lock().unwrap().push(IoEvent::DbSync);
        }
        self.inner.sync(c, sync_type)
    }

    fn size(&self) -> Result<u64> {
        self.inner.size()
    }

    fn truncate(&self, len: u64, c: Completion) -> Result<Completion> {
        self.inner.truncate(len, c)
    }
}
