# Refuse `DELETE ... ORDER BY` and `DELETE ... LIMIT` with an error that names the clause

## Problem [feature-006]

A caller that runs a bounded, ordered delete such as

```sql
DELETE FROM events WHERE delete_after < 500 ORDER BY delete_after, id LIMIT 3
```

needs to know whether the statement was refused because DELETE does not
support ORDER BY / LIMIT, or because it is broken for some other reason.
Silently deleting an unordered set of rows would be wrong, so the statement
must be refused before anything runs, and the error must say why.

Upstream commit `1e21c2beb` ("core, sqlite/parser: reject LIMIT and ORDER BY
on DELETE and UPDATE") removed the `order_by` and `limit` fields from
`Stmt::Delete` and `Stmt::Update`, so the parser no longer consumes those
clauses. The statement is still refused, but only by the generic
end-of-statement check:

```
LexerError(ParseUnexpectedToken { ..., got: TK_ORDER, expected: [TK_SEMI], token_text: "ORDER", ... })
```

Neither its `Display` (`near "ORDER": syntax error`) nor its `Debug` says
"ORDER BY", so the caller cannot tell it apart from any other syntax error.
Before that commit, the translator refused the statement with
`ORDER BY clause is not supported in DELETE`.

This change makes the parser refuse ORDER BY and LIMIT on DELETE with an
error that names the clause. It does **not** add support for them.

Code locations below are hints, relative to fork rev `066f662` (upstream
`8cc059b` with this fork's earlier prompts applied). Line numbers have
probably drifted; search by name.

### What turso does today

- `sqlite/parser/src/parser.rs` ~4759-4773: `parse_delete_without_cte`
  parses `DELETE FROM <fullname> [INDEXED ...] [WHERE ...] [RETURNING ...]`
  and returns. A following `ORDER` or `LIMIT` token is left for the
  caller, which fails with `ParseUnexpectedToken` (expected `;`).
- `sqlite/parser/src/ast.rs` ~219-230: `Stmt::Delete` has no `order_by`
  or `limit` fields. Do not add them back.
- `sqlite/parser/src/error.rs` ~97-99: `Error::Custom(String)`, whose
  `Display` is the string itself.
- `core/error.rs` ~33: parser errors reach `turso_core` callers as
  `LimboError::LexerError(Box<turso_parser::error::Error>)`, so the
  `Custom` string appears in both `format!("{err}")` and
  `format!("{err:?}")`.
- Existing tests that already expect DELETE/UPDATE with ORDER BY / LIMIT to
  fail, and must keep passing unchanged:
  - `sqlite/parser/src/parser.rs`
    `test_delete_and_update_reject_limit_and_order_by` (only checks
    `is_err()`).
  - `sqlite/conformance/sqlite-sqltests/delete-update-limit-rejected.sqltest`,
    which expects errors matching `near "LIMIT": syntax error` and
    `near "ORDER": syntax error` (a case-insensitive regex match, not an
    exact match).
  - `testing/cli_tests/cli_test_cases.py`
    `test_update_delete_reject_limit`, which checks that the output
    contains `syntax error`.

## Behaviour

In `parse_delete_without_cte`, after the optional WHERE clause and the
optional RETURNING clause have been parsed, if the next token is `TK_ORDER`
or `TK_LIMIT`, return `Error::Custom` with:

- for `ORDER`:
  `near "ORDER": syntax error: ORDER BY clause is not supported in DELETE`
- for `LIMIT`:
  `near "LIMIT": syntax error: LIMIT clause is not supported in DELETE`

The `near "X": syntax error` prefix keeps the SQLite-compatible text that
the existing sqltest and CLI tests match. The suffix is the message that
turso used before `1e21c2beb`, and it names the clause.

Put the check in one small helper below `parse_delete_without_cte`, and
call it once, right after `parse_returning()`. `parse_where` and
`parse_returning` consume nothing when their clause is absent, so that one
call sees an `ORDER` or `LIMIT` that follows the table name, the WHERE
clause, or the RETURNING clause. Do not change UPDATE: it keeps the generic
error. Do not change the AST, the translator, or any existing test.

## Regression tests

Add `#[cfg(test)] mod delete_order_by_limit_tests;` in `core/lib.rs`, in
module order (after `dbpage`), in a new file
`core/delete_order_by_limit_tests.rs`. Open a database with
`Database::open_file_with_flags(Arc::new(MemoryIO::new()), ":memory:",
OpenFlags::Create, DatabaseOpts::new(), None, Arc::new(SqliteDialect))`, as
`param_sharing_tests::open_connection_with_options` does. Create
`events(id INTEGER PRIMARY KEY, delete_after INTEGER)` and insert five rows,
four of them with `delete_after < 500`. Each test runs a statement through
`Connection::execute`, expects an error, and checks the error's
`format!("{err:?}")`; afterwards, `SELECT count(*) FROM events` is still 5.

1. `DELETE FROM events WHERE delete_after < 500 ORDER BY delete_after, id LIMIT 3`:
   the message contains `ORDER BY` and
   `ORDER BY clause is not supported in DELETE`.
2. `DELETE FROM events WHERE delete_after < 500 LIMIT 3`: the message
   contains `LIMIT clause is not supported in DELETE`.
3. `DELETE FROM events WHERE delete_after < 500 RETURNING id ORDER BY id LIMIT 3`:
   the message contains `ORDER BY clause is not supported in DELETE`.
4. `DELETE FROM events WHERE delete_after < 500 RETURNING id LIMIT 3`:
   the message contains `LIMIT clause is not supported in DELETE`.
5. `DELETE FROM events ORDER BY id LIMIT 1` (no WHERE): the message
   contains `ORDER BY clause is not supported in DELETE`.

All five tests must fail without the parser change (the `Debug` output is
then a `ParseUnexpectedToken`, which contains neither message).

## Constraints

- Do not implement ORDER BY or LIMIT on DELETE.
- Do not make any other changes. Keep the diff focused, follow turso's
  existing code style and naming, and do no unrelated refactors.
- Run `cargo fmt`, `cargo clippy -p turso_parser -p turso_core
  --all-targets -- --deny=warnings`,
  `cargo test -p turso_core --lib -- delete_order_by_limit_tests`,
  `cargo test -p turso_parser`, and the sqltest
  `sqlite/conformance/sqlite-sqltests/delete-update-limit-rejected.sqltest`
  (`cargo run --manifest-path testing/sqltest/Cargo.toml --bin sqltest --
  run <file> --backend rust`). Do not run the full `make test`.

## Update README

In the "Fork Features" subsection of the "About This Fork" section of
`README.md`, add a bullet: `DELETE ... ORDER BY` and `DELETE ... LIMIT` are
still refused at parse time and delete nothing, as upstream, but the error
names the clause (`near "ORDER": syntax error: ORDER BY clause is not
supported in DELETE`, and the same for LIMIT), where upstream reports only
`near "ORDER": syntax error` or `near "LIMIT": syntax error`.
