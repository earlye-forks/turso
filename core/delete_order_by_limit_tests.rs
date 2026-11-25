use std::sync::Arc;

use crate::{Connection, Database, DatabaseOpts, MemoryIO, OpenFlags, SqliteDialect, Value, IO};

#[test]
fn delete_with_order_by_and_limit_is_refused_with_an_error_naming_order_by() {
    let conn = open_connection_with_events();

    let message = delete_error_message(
        &conn,
        "DELETE FROM events WHERE delete_after < 500 ORDER BY delete_after, id LIMIT 3",
    );

    assert!(message.contains("ORDER BY"), "{message}");
    assert!(
        message.contains("ORDER BY clause is not supported in DELETE"),
        "{message}"
    );
    assert_all_events_remain(&conn);
}

#[test]
fn delete_with_limit_is_refused_with_an_error_naming_limit() {
    let conn = open_connection_with_events();

    let message =
        delete_error_message(&conn, "DELETE FROM events WHERE delete_after < 500 LIMIT 3");

    assert!(
        message.contains("LIMIT clause is not supported in DELETE"),
        "{message}"
    );
    assert_all_events_remain(&conn);
}

#[test]
fn delete_with_order_by_after_returning_is_refused_with_an_error_naming_order_by() {
    let conn = open_connection_with_events();

    let message = delete_error_message(
        &conn,
        "DELETE FROM events WHERE delete_after < 500 RETURNING id ORDER BY id LIMIT 3",
    );

    assert!(
        message.contains("ORDER BY clause is not supported in DELETE"),
        "{message}"
    );
    assert_all_events_remain(&conn);
}

fn open_connection_with_events() -> Arc<Connection> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = Database::open_file_with_flags(
        io,
        ":memory:",
        OpenFlags::Create,
        DatabaseOpts::new(),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE events(id INTEGER PRIMARY KEY, delete_after INTEGER)")
        .unwrap();
    conn.execute("INSERT INTO events VALUES (1, 400), (2, 100), (3, 300), (4, 200), (5, 900)")
        .unwrap();
    conn
}

fn delete_error_message(conn: &Arc<Connection>, sql: &str) -> String {
    let err = conn
        .execute(sql)
        .expect_err("DELETE with ORDER BY or LIMIT must be refused");
    format!("{err:?}")
}

fn assert_all_events_remain(conn: &Arc<Connection>) {
    let rows = conn
        .prepare("SELECT count(*) FROM events")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert_eq!(rows, vec![vec![Value::from_i64(5)]]);
}
