//! Regression tests for `OrphanWalPolicy`: a leftover `-wal` beside an
//! absent or empty db file.

use super::*;
use crate::io::FileSyncType;
use std::path::{Path, PathBuf};

const DISCARD_RULES: [EmptyDb; 3] = [EmptyDb::ZeroBytes, EmptyDb::OneByte, EmptyDb::InvalidHeader];

fn discard(empty: EmptyDb, read_only: ReadOnlyOrphanWal) -> OrphanWalPolicy {
    OrphanWalPolicy::Discard { empty, read_only }
}

fn platform_io() -> Arc<dyn IO> {
    Arc::new(PlatformIO::new().unwrap())
}

fn open_with(
    io: Arc<dyn IO>,
    path: &Path,
    flags: OpenFlags,
    policy: OrphanWalPolicy,
) -> Result<Arc<Database>> {
    Database::open_file_with_flags(
        io,
        path.to_str().unwrap(),
        flags,
        DatabaseOpts::new().with_orphan_wal_policy(policy),
        None,
    )
}

fn open_rw(path: &Path, policy: OrphanWalPolicy) -> Result<Arc<Database>> {
    open_with(platform_io(), path, OpenFlags::default(), policy)
}

fn query_strings(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut out = Vec::new();
    stmt.run_with_row_callback(|row| {
        out.push(row.get_value(0).to_string());
        Ok(())
    })
    .unwrap();
    out
}

fn schema_names(conn: &Arc<Connection>) -> Vec<String> {
    query_strings(conn, "SELECT name FROM sqlite_schema ORDER BY name")
}

fn integrity_check(conn: &Arc<Connection>) -> Vec<String> {
    query_strings(conn, "PRAGMA integrity_check")
}

/// `BEGIN; CREATE TABLE z(q); ROLLBACK;` allocates page 1 without committing.
fn rolled_back_write(conn: &Arc<Connection>) {
    conn.execute("BEGIN").unwrap();
    conn.execute("CREATE TABLE z(q)").unwrap();
    conn.execute("ROLLBACK").unwrap();
}

fn wal_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", db_path.display()))
}

fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// Builds the old database of the reproduction and returns its WAL bytes,
/// copied while the database was still open.
fn build_old_wal(dir: &Path) -> Vec<u8> {
    let path = dir.join("source.db");
    let db = open_rw(&path, OrphanWalPolicy::Replay).unwrap();
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
    let wal = std::fs::read(wal_path(&path)).unwrap();
    assert!(wal.len() > 2_000_000, "WAL should hold the old database");
    drop(conn);
    drop(db);
    wal
}

/// Places `wal` as `{db_path}-wal`, with `db` as the db file (absent if
/// `None`).
fn place_files(db_path: &Path, db: Option<&[u8]>, wal: &[u8]) {
    let _ = std::fs::remove_file(db_path);
    if let Some(db) = db {
        std::fs::write(db_path, db).unwrap();
    }
    std::fs::write(wal_path(db_path), wal).unwrap();
}

const OLD_TABLES: [&str; 3] = ["old_t", "old_u", "old_v"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    DbWrite,
    DbSync,
    WalWrite,
    DirSync,
}

/// Wraps the platform IO, recording db/WAL writes and syncs and
/// `sync_parent_dir` calls, modelled on `SyncCountingIo` in `vdbe/vacuum.rs`.
struct CountingIo {
    inner: Arc<dyn IO>,
    db_path: String,
    wal_path: String,
    events: Arc<Mutex<Vec<Event>>>,
    dir_sync_unsupported: bool,
}

impl CountingIo {
    fn new(db_path: &Path, dir_sync_unsupported: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: platform_io(),
            db_path: db_path.to_str().unwrap().to_string(),
            wal_path: wal_path(db_path).to_str().unwrap().to_string(),
            events: Arc::new(Mutex::new(Vec::new())),
            dir_sync_unsupported,
        })
    }

    fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock())
    }
}

impl Clock for CountingIo {
    fn current_time_monotonic(&self) -> MonotonicInstant {
        self.inner.current_time_monotonic()
    }

    fn current_time_wall_clock(&self) -> WallClockInstant {
        self.inner.current_time_wall_clock()
    }
}

impl IO for CountingIo {
    fn open_file(&self, path: &str, flags: OpenFlags, direct: bool) -> Result<Arc<dyn File>> {
        let inner = self.inner.open_file(path, flags, direct)?;
        Ok(Arc::new(CountingFile {
            inner,
            is_db: path == self.db_path,
            tracked: path == self.db_path || path == self.wal_path,
            events: self.events.clone(),
        }))
    }

    fn remove_file(&self, path: &str) -> Result<()> {
        self.inner.remove_file(path)
    }

    fn sync_parent_dir(&self, path: &str, c: Completion) -> Result<Completion> {
        if self.dir_sync_unsupported {
            return Err(LimboError::IoExtensionUnsupported("sync_parent_dir"));
        }
        self.events.lock().push(Event::DirSync);
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

    fn file_id(&self, path: &str) -> Result<io::FileId> {
        self.inner.file_id(path)
    }
}

struct CountingFile {
    inner: Arc<dyn File>,
    is_db: bool,
    tracked: bool,
    events: Arc<Mutex<Vec<Event>>>,
}

impl CountingFile {
    fn record_write(&self) {
        if self.tracked {
            self.events.lock().push(if self.is_db {
                Event::DbWrite
            } else {
                Event::WalWrite
            });
        }
    }
}

impl File for CountingFile {
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
        if self.tracked && self.is_db {
            self.events.lock().push(Event::DbSync);
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

fn count(events: &[Event], event: Event) -> usize {
    events.iter().filter(|e| **e == event).count()
}

#[test]
fn replay_resurrects_orphan_wal_without_extra_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let old_wal = build_old_wal(dir.path());
    let path = dir.path().join("g.db");
    place_files(&path, None, &old_wal);

    let io = CountingIo::new(&path, false);
    let db = open_with(
        io.clone(),
        &path,
        OpenFlags::default(),
        OrphanWalPolicy::Replay,
    )
    .unwrap();
    let conn = db.connect().unwrap();
    assert!(schema_names(&conn).is_empty());
    assert!(conn.prepare("SELECT * FROM old_t").is_err());
    rolled_back_write(&conn);
    assert_eq!(file_len(&path), Some(4096));

    let events = io.take_events();
    assert_eq!(count(&events, Event::DirSync), 0, "{events:?}");
    assert_eq!(count(&events, Event::DbSync), 0, "{events:?}");
    assert_eq!(file_len(&wal_path(&path)), Some(old_wal.len() as u64));

    let conn2 = db.connect().unwrap();
    assert_eq!(schema_names(&conn2), OLD_TABLES);
    assert_eq!(query_strings(&conn2, "SELECT count(*) FROM old_t"), ["300"]);
    drop(conn2);
    drop(conn);
    drop(db);

    let db = open_rw(&path, OrphanWalPolicy::Replay).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(schema_names(&conn), OLD_TABLES);
    assert_eq!(query_strings(&conn, "SELECT count(*) FROM old_t"), ["300"]);
    assert_eq!(integrity_check(&conn), ["ok"]);
}

/// Opens `path` under `policy`, checks the schema is empty, allocates page 1
/// without committing, reopens and checks the db is still empty.
fn assert_discarded(path: &Path, policy: OrphanWalPolicy) {
    let db = open_rw(path, policy).unwrap();
    assert_eq!(file_len(&wal_path(path)), Some(0), "WAL must be discarded");
    let conn = db.connect().unwrap();
    assert!(schema_names(&conn).is_empty());
    rolled_back_write(&conn);
    let conn2 = db.connect().unwrap();
    assert!(schema_names(&conn2).is_empty());
    drop(conn2);
    drop(conn);
    drop(db);

    let db = open_rw(path, policy).unwrap();
    let conn = db.connect().unwrap();
    assert!(schema_names(&conn).is_empty());
    assert_eq!(integrity_check(&conn), ["ok"]);
}

#[test]
fn discard_zero_bytes_discards_orphan_wal_beside_absent_or_empty_db() {
    let dir = tempfile::tempdir().unwrap();
    let old_wal = build_old_wal(dir.path());
    let policy = discard(EmptyDb::ZeroBytes, ReadOnlyOrphanWal::Ignore);

    let absent = dir.path().join("absent.db");
    place_files(&absent, None, &old_wal);
    assert_discarded(&absent, policy);

    let empty = dir.path().join("empty.db");
    place_files(&empty, Some(&[]), &old_wal);
    assert_discarded(&empty, policy);

    // A 1-byte db is not empty under `ZeroBytes`: the WAL is left alone.
    let one = dir.path().join("one.db");
    place_files(&one, Some(&[0]), &old_wal);
    let _ = open_rw(&one, policy);
    assert_eq!(std::fs::read(wal_path(&one)).unwrap(), old_wal);
    assert_eq!(file_len(&one), Some(1));
}

#[test]
fn discard_one_byte_treats_one_byte_db_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    let old_wal = build_old_wal(dir.path());
    let path = dir.path().join("one.db");
    place_files(&path, Some(&[0]), &old_wal);
    let policy = discard(EmptyDb::OneByte, ReadOnlyOrphanWal::Ignore);
    {
        let _db = open_rw(&path, policy).unwrap();
        assert_eq!(file_len(&path), Some(0), "1-byte db must be truncated");
    }
    assert_discarded(&path, policy);
}

#[test]
fn discard_invalid_header_truncates_invalid_db_with_and_without_wal() {
    let dir = tempfile::tempdir().unwrap();
    let old_wal = build_old_wal(dir.path());
    let policy = discard(EmptyDb::InvalidHeader, ReadOnlyOrphanWal::Ignore);
    let zeros = vec![0u8; 4096];

    let with_wal = dir.path().join("with_wal.db");
    place_files(&with_wal, Some(&zeros), &old_wal);
    {
        let db = open_rw(&with_wal, policy).unwrap();
        assert_eq!(file_len(&with_wal), Some(0));
        assert_eq!(file_len(&wal_path(&with_wal)), Some(0));
        let conn = db.connect().unwrap();
        assert!(schema_names(&conn).is_empty());
    }
    assert_discarded(&with_wal, policy);

    let no_wal = dir.path().join("no_wal.db");
    std::fs::write(&no_wal, &zeros).unwrap();
    let db = open_rw(&no_wal, policy).unwrap();
    assert_eq!(file_len(&no_wal), Some(0));
    let conn = db.connect().unwrap();
    assert!(schema_names(&conn).is_empty());
    conn.execute("CREATE TABLE t(x)").unwrap();
    assert_eq!(schema_names(&conn), ["t"]);
    assert_eq!(integrity_check(&conn), ["ok"]);
}

#[test]
fn discard_replays_wal_of_valid_db_under_every_rule() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.db");
    let (db_bytes, wal_bytes) = {
        let db = open_rw(&source, OrphanWalPolicy::Replay).unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t(x)").unwrap();
        for i in 0..10 {
            conn.execute(format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        (
            std::fs::read(&source).unwrap(),
            std::fs::read(wal_path(&source)).unwrap(),
        )
    };
    assert!(!db_bytes.is_empty());
    assert!(!wal_bytes.is_empty());

    for rule in DISCARD_RULES {
        let path = dir.path().join(format!("{rule:?}.db"));
        place_files(&path, Some(&db_bytes), &wal_bytes);
        let db = open_rw(&path, discard(rule, ReadOnlyOrphanWal::Ignore)).unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(schema_names(&conn), ["t"], "{rule:?}");
        assert_eq!(query_strings(&conn, "SELECT count(*) FROM t"), ["10"]);
        assert_eq!(integrity_check(&conn), ["ok"]);
    }
}

#[test]
fn discard_read_only_never_modifies_files() {
    let dir = tempfile::tempdir().unwrap();
    let old_wal = build_old_wal(dir.path());
    let ro = OpenFlags::ReadOnly;

    // Ignore: empty schema, WAL untouched.
    for (name, db, rule) in [
        ("zero.db", &[][..], EmptyDb::ZeroBytes),
        ("one.db", &[0u8][..], EmptyDb::OneByte),
    ] {
        let path = dir.path().join(name);
        place_files(&path, Some(db), &old_wal);
        let db_handle = open_with(
            platform_io(),
            &path,
            ro,
            discard(rule, ReadOnlyOrphanWal::Ignore),
        )
        .unwrap();
        let conn = db_handle.connect().unwrap();
        assert!(schema_names(&conn).is_empty(), "{name}");
        drop(conn);
        drop(db_handle);
        assert_eq!(std::fs::read(&path).unwrap(), db, "{name}");
        assert_eq!(std::fs::read(wal_path(&path)).unwrap(), old_wal, "{name}");
    }

    // Replay: same as upstream (the default policy).
    let path = dir.path().join("replay.db");
    place_files(&path, Some(&[]), &old_wal);
    let upstream = {
        let db = open_with(platform_io(), &path, ro, OrphanWalPolicy::Replay).unwrap();
        let conn = db.connect().unwrap();
        schema_names(&conn)
    };
    let replayed = {
        let db = open_with(
            platform_io(),
            &path,
            ro,
            discard(EmptyDb::ZeroBytes, ReadOnlyOrphanWal::Replay),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        schema_names(&conn)
    };
    assert_eq!(replayed, upstream);
    assert_eq!(file_len(&path), Some(0));
    assert_eq!(std::fs::read(wal_path(&path)).unwrap(), old_wal);

    // InvalidHeader: an invalid header fails the open, whatever `read_only`.
    for read_only in [ReadOnlyOrphanWal::Ignore, ReadOnlyOrphanWal::Replay] {
        for (name, db) in [("zeros.db", vec![0u8; 4096]), ("short.db", vec![7u8; 50])] {
            let path = dir.path().join(name);
            place_files(&path, Some(&db), &old_wal);
            let err = open_with(
                platform_io(),
                &path,
                ro,
                discard(EmptyDb::InvalidHeader, read_only),
            )
            .unwrap_err();
            assert!(err.to_string().contains("valid header"), "{err}");
            assert_eq!(std::fs::read(&path).unwrap(), db);
            assert_eq!(std::fs::read(wal_path(&path)).unwrap(), old_wal);
        }
    }
}

#[test]
fn discard_makes_page1_durable_before_first_wal_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("durable.db");
    let io = CountingIo::new(&path, false);
    let db = open_with(
        io.clone(),
        &path,
        OpenFlags::default(),
        discard(EmptyDb::ZeroBytes, ReadOnlyOrphanWal::Ignore),
    )
    .unwrap();
    let open_events = io.take_events();
    assert_eq!(open_events, [Event::DirSync], "open probes sync_parent_dir");

    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t(x)").unwrap();
    let events = io.take_events();
    let first_wal_write = events
        .iter()
        .position(|e| *e == Event::WalWrite)
        .expect("commit must write the WAL");
    assert_eq!(
        &events[..first_wal_write],
        [Event::DbWrite, Event::DbSync, Event::DirSync],
        "page 1 must be written, synced and its directory entry synced before any WAL frame: {events:?}"
    );
}

#[test]
fn discard_fails_open_of_empty_db_when_sync_parent_dir_unsupported() {
    let dir = tempfile::tempdir().unwrap();
    let policy = discard(EmptyDb::ZeroBytes, ReadOnlyOrphanWal::Ignore);

    let empty = dir.path().join("empty.db");
    let io = CountingIo::new(&empty, true);
    let err = open_with(io, &empty, OpenFlags::default(), policy).unwrap_err();
    assert!(
        matches!(err, LimboError::IoExtensionUnsupported("sync_parent_dir")),
        "{err:?}"
    );

    let existing = dir.path().join("existing.db");
    {
        let db = open_rw(&existing, OrphanWalPolicy::Replay).unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t(x)").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
    }
    let io = CountingIo::new(&existing, true);
    let db = open_with(io, &existing, OpenFlags::default(), policy).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(query_strings(&conn, "SELECT x FROM t"), ["1"]);
    conn.execute("INSERT INTO t VALUES (2)").unwrap();
}

#[test]
fn discard_rejects_registry_bypassed_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bypass.db");
    let path_str = path.to_str().unwrap();
    let io = platform_io();
    let file = io.open_file(path_str, OpenFlags::default(), false).unwrap();
    let mut state = OpenDbAsyncState::new();
    let err = Database::open_with_flags_bypass_registry_async(
        &mut state,
        io,
        path_str,
        None,
        Arc::new(DatabaseFile::new(file)),
        OpenFlags::default(),
        DatabaseOpts::new()
            .with_orphan_wal_policy(discard(EmptyDb::ZeroBytes, ReadOnlyOrphanWal::Ignore)),
        None,
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("registry-bypassed"), "{err}");
}

#[cfg(all(host_shared_wal, unix))]
const MULTIPROCESS_HOLD_CHILD_TEST: &str =
    "orphan_wal_tests::multiprocess_orphan_wal_child_process";

#[cfg(all(host_shared_wal, unix))]
fn open_multiprocess(path: &Path, policy: OrphanWalPolicy) -> Result<Arc<Database>> {
    Database::open_file_with_flags(
        platform_io(),
        path.to_str().unwrap(),
        OpenFlags::default(),
        DatabaseOpts::new()
            .with_multiprocess_wal(true)
            .with_orphan_wal_policy(policy),
        None,
    )
}

#[cfg(all(host_shared_wal, unix))]
fn wait_for_file(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(all(host_shared_wal, unix))]
#[test]
fn discard_under_multiprocess_wal_fails_when_another_process_is_attached() {
    let dir = tempfile::tempdir().unwrap();
    let old_wal = build_old_wal(dir.path());
    let path = dir.path().join("mp.db");
    place_files(&path, None, &old_wal);
    let ready = dir.path().join("ready");
    let release = dir.path().join("release");

    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg(MULTIPROCESS_HOLD_CHILD_TEST)
        .arg("--exact")
        .arg("--nocapture")
        .env("TURSO_ORPHAN_WAL_DB_PATH", &path)
        .env("TURSO_ORPHAN_WAL_READY_FILE", &ready)
        .env("TURSO_ORPHAN_WAL_RELEASE_FILE", &release)
        .spawn()
        .unwrap();
    wait_for_file(&ready);

    let policy = discard(EmptyDb::ZeroBytes, ReadOnlyOrphanWal::Ignore);
    let result = open_multiprocess(&path, policy);
    std::fs::write(&release, b"release").unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "child failed: {status:?}");

    let err = result.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("orphan WAL"), "{msg}");
    assert!(msg.contains("another process is attached"), "{msg}");
    assert!(msg.contains(wal_path(&path).to_str().unwrap()), "{msg}");
    assert_eq!(std::fs::read(wal_path(&path)).unwrap(), old_wal);

    // With no peer attached the authority is exclusive and discard proceeds.
    let db = open_multiprocess(&path, policy).unwrap();
    assert_eq!(file_len(&wal_path(&path)), Some(0));
    let conn = db.connect().unwrap();
    assert!(schema_names(&conn).is_empty());
}

#[cfg(all(host_shared_wal, unix))]
#[test]
fn multiprocess_orphan_wal_child_process() {
    let Some(db_path) = std::env::var_os("TURSO_ORPHAN_WAL_DB_PATH") else {
        return;
    };
    let ready = std::env::var_os("TURSO_ORPHAN_WAL_READY_FILE").unwrap();
    let release = std::env::var_os("TURSO_ORPHAN_WAL_RELEASE_FILE").unwrap();
    let _db = open_multiprocess(Path::new(&db_path), OrphanWalPolicy::Replay).unwrap();
    std::fs::write(&ready, b"ready").unwrap();
    wait_for_file(Path::new(&release));
}
