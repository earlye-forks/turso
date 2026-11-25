use super::*;
use std::path::Path;

#[test]
fn suffix_naming_keeps_log_inside_memory_io() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path_in(dir.path(), "foo.sqlite");
    let io = memory_io();

    let db = open_db(io.clone(), &db_path, true).unwrap();
    let conn = db.connect().unwrap();
    enable_mvcc_and_insert(&conn, &["a", "b"]);

    assert!(io_file_size(&io, &format!("{db_path}-log")).unwrap() > 0);
    assert_eq!(io_file_size(&io, &path_in(dir.path(), "foo.db-log")), None);
    assert_eq!(
        io_file_size(&io, &path_in(dir.path(), "foo.sqlite.db-log")),
        None
    );
    assert_dir_empty(dir.path());

    conn.close().unwrap();
    drop(conn);
    drop(db);

    let db = open_db(io, &db_path, true).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(row_values(&conn), ["a", "b"]);
    assert_dir_empty(dir.path());
}

#[test]
fn suffix_naming_gives_each_db_its_own_log() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite_path = path_in(dir.path(), "foo.sqlite");
    let db_path = path_in(dir.path(), "foo.db");
    let io = memory_io();

    let sqlite_db = open_db(io.clone(), &sqlite_path, true).unwrap();
    let sqlite_conn = sqlite_db.connect().unwrap();
    enable_mvcc_and_insert(&sqlite_conn, &["from-sqlite"]);
    let db = open_db(io.clone(), &db_path, true).unwrap();
    let conn = db.connect().unwrap();
    enable_mvcc_and_insert(&conn, &["from-db-1", "from-db-2"]);

    let sqlite_log = io_file_contents(&io, &format!("{sqlite_path}-log"));
    let db_log = io_file_contents(&io, &format!("{db_path}-log"));
    assert!(!sqlite_log.is_empty());
    assert!(!db_log.is_empty());
    assert_ne!(sqlite_log, db_log);
    assert_dir_empty(dir.path());

    sqlite_conn.close().unwrap();
    conn.close().unwrap();
    drop((sqlite_conn, sqlite_db, conn, db));

    let sqlite_conn = open_db(io.clone(), &sqlite_path, true)
        .unwrap()
        .connect()
        .unwrap();
    assert_eq!(row_values(&sqlite_conn), ["from-sqlite"]);
    let conn = open_db(io, &db_path, true).unwrap().connect().unwrap();
    assert_eq!(row_values(&conn), ["from-db-1", "from-db-2"]);
    assert_dir_empty(dir.path());
}

#[test]
fn suffix_naming_ignores_stray_log_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path_in(dir.path(), "foo.db");
    write_stray_log(dir.path());

    let db = open_db(memory_io(), &db_path, true).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(
        query_rows(&conn, "SELECT 1"),
        vec![vec![Value::from_i64(1)]]
    );
}

#[test]
fn extension_naming_fails_on_stray_log_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path_in(dir.path(), "foo.db");
    write_stray_log(dir.path());

    let error = open_db(memory_io(), &db_path, false).unwrap_err();
    assert_logical_log_corrupt_error(error);
}

#[test]
fn extension_naming_shares_one_log_between_sqlite_and_db_paths() {
    let dir = tempfile::tempdir().unwrap();
    let sqlite_path = path_in(dir.path(), "foo.sqlite");
    let db_path = path_in(dir.path(), "foo.db");

    let sqlite_db = open_db(platform_io(), &sqlite_path, false).unwrap();
    let sqlite_conn = sqlite_db.connect().unwrap();
    enable_mvcc_and_insert(&sqlite_conn, &["a"]);

    assert!(
        std::fs::metadata(path_in(dir.path(), "foo.db-log"))
            .unwrap()
            .len()
            > 0
    );
    assert!(!Path::new(&format!("{sqlite_path}-log")).exists());

    let error = open_db(platform_io(), &db_path, false).unwrap_err();
    assert_logical_log_corrupt_error(error);
}

#[test]
fn suffix_naming_probe_does_not_create_missing_log() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path_in(dir.path(), "bar.db");
    let io = memory_io();

    let db = open_db(io.clone(), &db_path, true).unwrap();
    let conn = db.connect().unwrap();
    assert_eq!(
        query_rows(&conn, "PRAGMA journal_mode"),
        vec![vec![Value::build_text("wal")]]
    );
    assert_eq!(io_file_size(&io, &path_in(dir.path(), "bar.db-log")), None);
}

#[cfg(feature = "conn_raw_api")]
#[test]
fn suffix_naming_reload_after_external_restore_in_mvcc_mode() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path_in(dir.path(), "foo.sqlite");
    let io = memory_io();

    let db = open_db(io.clone(), &db_path, true).unwrap();
    let conn = db.connect().unwrap();
    enable_mvcc_and_insert(&conn, &["a", "b"]);

    conn.reload_wal_after_external_restore().unwrap();

    assert_eq!(row_values(&db.connect().unwrap()), ["a", "b"]);
    assert!(io_file_size(&io, &format!("{db_path}-log")).is_some());
    assert_eq!(io_file_size(&io, &path_in(dir.path(), "foo.db-log")), None);
    assert_dir_empty(dir.path());
}

#[cfg(feature = "conn_raw_api")]
#[test]
fn suffix_naming_reload_after_external_restore_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = path_in(dir.path(), "foo.db");
    write_stray_log(dir.path());

    let db = open_db(memory_io(), &db_path, true).unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t(x TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES ('a')").unwrap();

    conn.reload_wal_after_external_restore().unwrap();

    assert_eq!(row_values(&db.connect().unwrap()), ["a"]);
    assert!(db.get_mv_store().is_none());
}

fn path_in(dir: &Path, name: &str) -> String {
    dir.join(name).to_str().unwrap().to_string()
}

fn memory_io() -> Arc<dyn IO> {
    Arc::new(MemoryIO::new())
}

fn platform_io() -> Arc<dyn IO> {
    Arc::new(PlatformIO::new().unwrap())
}

fn open_db(io: Arc<dyn IO>, path: &str, suffix_naming: bool) -> Result<Arc<Database>> {
    Database::open_file_with_flags(
        io,
        path,
        OpenFlags::default(),
        DatabaseOpts::new().with_mvcc_log_suffix_naming(suffix_naming),
        None,
        Arc::new(SqliteDialect),
    )
}

fn enable_mvcc_and_insert(conn: &Arc<Connection>, values: &[&str]) {
    conn.execute("PRAGMA journal_mode = 'mvcc'").unwrap();
    conn.execute("CREATE TABLE t(x TEXT)").unwrap();
    for value in values {
        conn.execute(format!("INSERT INTO t VALUES ('{value}')"))
            .unwrap();
    }
}

fn row_values(conn: &Arc<Connection>) -> Vec<String> {
    query_rows(conn, "SELECT x FROM t ORDER BY x")
        .into_iter()
        .map(|row| row[0].to_text().unwrap().to_string())
        .collect()
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

fn io_file_size(io: &Arc<dyn IO>, path: &str) -> Option<u64> {
    match io.open_file(path, OpenFlags::ReadOnly, false) {
        Ok(file) => Some(file.size().unwrap()),
        Err(LimboError::CompletionError(CompletionError::IOError(
            std::io::ErrorKind::NotFound,
            _,
        ))) => None,
        Err(error) => panic!("unexpected error opening {path}: {error}"),
    }
}

fn io_file_contents(io: &Arc<dyn IO>, path: &str) -> Vec<u8> {
    let file = io.open_file(path, OpenFlags::ReadOnly, false).unwrap();
    let len = file.size().unwrap() as usize;
    let buf = Arc::new(Buffer::new_temporary(len));
    let c = file
        .pread(0, Completion::new_read(buf.clone(), |_| None))
        .unwrap();
    io.wait_for_completion(c).unwrap();
    buf.as_slice().to_vec()
}

fn write_stray_log(dir: &Path) {
    std::fs::write(dir.join("foo.db-log"), b"stray").unwrap();
}

fn assert_dir_empty(dir: &Path) {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(entries.is_empty(), "expected empty dir, found {entries:?}");
}

fn assert_logical_log_corrupt_error(error: LimboError) {
    match error {
        LimboError::Corrupt(message) => {
            assert!(
                message.starts_with("MVCC logical log file exists"),
                "unexpected message: {message}"
            );
        }
        other => panic!("expected Corrupt error, got {other:?}"),
    }
}
