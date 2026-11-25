use std::num::NonZero;
use std::sync::Arc;

use crate::types::Text;
use crate::{
    Connection, Database, DatabaseOpts, MemoryIO, OpenFlags, SqliteDialect, Statement, Value, IO,
};

const LARGE: usize = 1 << 20;

#[test]
fn select_reads_a_bound_blob_without_copying_it() {
    let conn = open_test_connection();
    let mut stmt = conn.prepare("SELECT ?1, ?1").unwrap();

    let first = crate::alloc::vec![7u8; LARGE];
    let first_ptr = first.as_ptr();
    stmt.bind_at(first_index(), Value::Blob(first)).unwrap();
    assert_both_columns_are_blob(&mut stmt, first_ptr, 7);

    stmt.reset().unwrap();
    assert_both_columns_are_blob(&mut stmt, first_ptr, 7);

    stmt.reset().unwrap();
    let second = crate::alloc::vec![9u8; LARGE];
    let second_ptr = second.as_ptr();
    stmt.bind_at(first_index(), Value::Blob(second)).unwrap();
    assert_both_columns_are_blob(&mut stmt, second_ptr, 9);
}

#[test]
fn select_reads_a_bound_text_without_copying_it() {
    let conn = open_test_connection();
    let mut stmt = conn.prepare("SELECT ?1, ?1").unwrap();

    let first = "a".repeat(LARGE);
    let first_ptr = first.as_ptr();
    stmt.bind_at(first_index(), Value::Text(Text::new(first)))
        .unwrap();
    assert_both_columns_are_text(&mut stmt, first_ptr, 'a');

    stmt.reset().unwrap();
    assert_both_columns_are_text(&mut stmt, first_ptr, 'a');

    stmt.reset().unwrap();
    let second = "b".repeat(LARGE);
    let second_ptr = second.as_ptr();
    stmt.bind_at(first_index(), Value::Text(Text::new(second)))
        .unwrap();
    assert_both_columns_are_text(&mut stmt, second_ptr, 'b');
}

#[test]
fn integer_affinity_converts_a_shared_text_only_in_its_own_column() {
    let conn = open_test_connection();
    conn.execute("CREATE TABLE t(n INTEGER, s)").unwrap();
    let mut insert = conn.prepare("INSERT INTO t VALUES (?1, ?1)").unwrap();
    insert
        .bind_at(first_index(), Value::build_text("42".to_string()))
        .unwrap();
    insert.run_ignore_rows().unwrap();
    insert.reset().unwrap();
    insert.run_ignore_rows().unwrap();

    let rows = conn
        .prepare("SELECT typeof(n), n, typeof(s), s FROM t")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    let expected_row = vec![
        Value::build_text("integer"),
        Value::from_i64(42),
        Value::build_text("text"),
        Value::build_text("42"),
    ];
    assert_eq!(rows, vec![expected_row.clone(), expected_row]);
}

#[test]
fn clear_bindings_after_reset_makes_the_parameter_null() {
    let conn = open_test_connection();
    let mut stmt = conn.prepare("SELECT ?1").unwrap();
    stmt.bind_at(first_index(), Value::Blob(crate::alloc::vec![1u8; LARGE]))
        .unwrap();
    stmt.run_ignore_rows().unwrap();
    stmt.reset().unwrap();
    stmt.clear_bindings();

    let row = stmt
        .run_one_step_blocking(|| Ok(()), || Ok(()))
        .unwrap()
        .expect("SELECT returns one row");
    assert_eq!(row.get_value(0), &Value::Null);
}

#[test]
fn rebinding_large_blobs_inserts_each_one_intact() {
    let conn = open_test_connection();
    conn.execute("CREATE TABLE b(x BLOB)").unwrap();
    let mut insert = conn.prepare("INSERT INTO b VALUES (?1)").unwrap();
    let fills = [1u8, 2, 3, 4];
    for fill in fills {
        insert.reset().unwrap();
        insert
            .bind_at(first_index(), Value::Blob(crate::alloc::vec![fill; LARGE]))
            .unwrap();
        insert.run_ignore_rows().unwrap();
    }

    let rows = conn
        .prepare("SELECT x FROM b ORDER BY rowid")
        .unwrap()
        .run_collect_rows()
        .unwrap();
    assert_eq!(rows.len(), fills.len());
    for (row, fill) in rows.iter().zip(fills) {
        let Value::Blob(bytes) = &row[0] else {
            panic!("expected blob, got {:?}", row[0]);
        };
        assert_eq!(bytes.len(), LARGE);
        assert!(bytes.iter().all(|b| *b == fill));
    }
}

#[cfg(feature = "fts")]
#[test]
fn fts_indexes_and_matches_bound_text_parameters() {
    let conn = open_connection_with_options(DatabaseOpts::new().with_index_method(true));
    conn.execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, body TEXT)")
        .unwrap();
    conn.execute("CREATE INDEX docs_fts ON docs USING fts(body)")
        .unwrap();
    let mut insert = conn.prepare("INSERT INTO docs VALUES (1, ?1)").unwrap();
    insert
        .bind_at(
            first_index(),
            Value::build_text("shared parameter text".to_string()),
        )
        .unwrap();
    insert.run_ignore_rows().unwrap();

    let mut query = conn
        .prepare("SELECT id FROM docs WHERE fts_match(body, ?1)")
        .unwrap();
    query
        .bind_at(first_index(), Value::build_text("parameter".to_string()))
        .unwrap();
    assert_eq!(
        query.run_collect_rows().unwrap(),
        vec![vec![Value::from_i64(1)]]
    );
}

fn open_test_connection() -> Arc<Connection> {
    open_connection_with_options(DatabaseOpts::new())
}

fn open_connection_with_options(options: DatabaseOpts) -> Arc<Connection> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = Database::open_file_with_flags(
        io,
        ":memory:",
        OpenFlags::Create,
        options,
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    db.connect().unwrap()
}

fn first_index() -> NonZero<usize> {
    NonZero::new(1).unwrap()
}

fn assert_both_columns_are_blob(stmt: &mut Statement, ptr: *const u8, fill: u8) {
    let row = stmt
        .run_one_step_blocking(|| Ok(()), || Ok(()))
        .unwrap()
        .expect("SELECT returns one row");
    for column in 0..2 {
        let Value::Blob(bytes) = row.get_value(column) else {
            panic!("expected blob, got {:?}", row.get_value(column));
        };
        assert_eq!(bytes.as_ptr(), ptr, "column {column} copied the blob");
        assert_eq!(bytes.len(), LARGE);
        assert!(bytes.iter().all(|b| *b == fill));
    }
}

fn assert_both_columns_are_text(stmt: &mut Statement, ptr: *const u8, fill: char) {
    let row = stmt
        .run_one_step_blocking(|| Ok(()), || Ok(()))
        .unwrap()
        .expect("SELECT returns one row");
    for column in 0..2 {
        let Value::Text(text) = row.get_value(column) else {
            panic!("expected text, got {:?}", row.get_value(column));
        };
        assert_eq!(
            text.as_str().as_ptr(),
            ptr,
            "column {column} copied the text"
        );
        assert_eq!(text.as_str().len(), LARGE);
        assert!(text.as_str().chars().all(|c| c == fill));
    }
}
