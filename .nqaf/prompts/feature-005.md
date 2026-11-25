# Add an opt-in option to name the MVCC log `{name}-log` and reach it only through the IO

## Problem [feature-005]

turso finds a database's WAL through the `IO` the caller supplies, under the
name `{name}-wal`. It finds the MVCC logical log partly through `std::fs`,
under a name that is not derived the way the WAL's is:

1. **Naming.** The log is `db_path.with_extension("db-log")`. That replaces
   the db file's extension instead of appending to it, so `foo.sqlite` and
   `foo.db` both map to `foo.db-log` and collide, and `foo` maps to
   `foo.db-log` as well. A host that treats a db as `{name}` plus its
   `{name}-*` sidecars cannot find or wipe this log.
2. **Existence check.** `logical_log_exists` uses
   `std::path::Path::exists` and then `metadata().unwrap().len() > 0`.
   Under a non-filesystem IO (for example `MemoryIO`), it looks at an
   unrelated path on the real disk. If a stray file happens to be there, a
   WAL-mode open fails hard with
   `Corrupt("MVCC logical log file exists for database …, but database header indicates WAL mode…")`.
   Also, `metadata().unwrap()` panics if the file disappears between
   `exists` and `metadata`.

The log itself is already opened through the IO (`io.open_file` in
`open_mv_store`). Only its name and its existence check escape.

This change adds an option, **off by default**, that makes the MVCC log a
plain sidecar like the WAL.

Code locations below are hints, relative to `core/` at fork rev `1ccaf5c`
(upstream `8cc059b` with this fork's earlier prompts applied). Line numbers
have probably drifted; search by name.

### What turso does today

- `storage/journal_mode.rs` ~59-63: `logical_log_exists(db_path)`, the
  `std::fs` check described above.
- `storage/journal_mode.rs` ~65-110: `open_mv_store(io, db_path, …)`. When no
  custom `DurableStorage` is supplied, it derives
  `db_path.with_extension("db-log")` (~90) and opens it with
  `io.open_file(string_path, flags, false)` (~95).
- `database.rs` ~1999-2000: `logical_log_exists(&self.path)` runs on every
  open, in the `HeaderValidationState::Start` step. Its result feeds the
  `Corrupt` error at ~2181-2185 when the header says WAL mode.
- `database.rs` ~2678: the second `logical_log_exists` caller, in
  `reload_wal_after_external_restore` (`conn_raw_api` feature).
- `open_mv_store` has four callers. They do not all pass the same path:
  - `database.rs` ~2363-2374 (open path): passes
    `self.get_database_canonical_path()`.
  - `database.rs` ~2680 (`reload_wal_after_external_restore`): passes
    `self.path`.
  - `vdbe/execute.rs` ~19028-19041 (`PRAGMA journal_mode = 'mvcc'`,
    `OpJournalModeSubState::Finalize`): passes
    `program.connection.get_database_canonical_path()`.
  - `connection.rs` ~3588 (ATTACH of a fresh db into an MVCC connection):
    passes `init.db.path`.
- `get_database_canonical_path` (`database.rs` ~2617-2628) calls
  `std::fs::canonicalize` and falls back to the raw path if it fails, or
  returns `""` for in-memory dbs.
- The WAL is named `format!("{path}-wal")` from the path as passed
  (`database.rs` ~1021 and ~1583), unless `OpenOptions::wal_path` overrides
  it (~251, ~293; honoured only by `Database::do_open*`).
- `mvcc/persistent_storage/logical_log.rs` ~9-10: the module doc comment
  says commits append to `.db-log` and checkpoint truncates `.db-log`.

## API

In `core/database.rs`, add a field beside the existing `enable_*` fields of
`DatabaseOpts` (~71-86), and a builder beside the existing `with_*` builders
(~99-162), matching their shape:

```rust
pub struct DatabaseOpts {
    // ...
    pub enable_mvcc_log_suffix_naming: bool,
    // ...
}

impl DatabaseOpts {
    pub fn with_mvcc_log_suffix_naming(mut self, enable: bool) -> Self;
}
```

The default is `false` (`DatabaseOpts` derives `Default`). Keep the names as
given.

## Behaviour

### Option off (the default)

Naming and behaviour are **byte-for-byte unchanged**. That includes the
`with_extension("db-log")` naming, the `std::fs` existence check in
`logical_log_exists`, and the path each `open_mv_store` caller passes today
(canonical in two places, raw in two). Other code and existing tests rely on
the `.db-log` name; do not touch them.

### Option on

1. **Suffix naming.** The log path is `format!("{name}-log")`, where `name`
   is the same string the default WAL name suffixes: the database path **as
   passed** to open (`Database::path`), **not** the canonicalized path from
   `get_database_canonical_path`. The log must
   sit beside the WAL as a sidecar of the same `name`, inside the same IO,
   and `std::fs::canonicalize` is meaningless for a non-filesystem IO. All
   four `open_mv_store` callers use this name when the option is on,
   including the two that pass the canonical path today. An
   `OpenOptions::wal_path` override does not change the log name; the log is
   always derived from the db path. For in-memory paths the same rule
   applies (for example `:memory:-log`, inside that db's IO).
2. **Existence check through the IO.** `logical_log_exists` must not use
   `std::fs` at all. Open the log path through the database's `IO` with
   `OpenFlags::ReadOnly` (no `Create`):
   - An open error of
     `LimboError::CompletionError(CompletionError::IOError(std::io::ErrorKind::NotFound, _))`
     means the log is absent: return `false`. This is the error both
     `UnixIO::open_file` (via `io_error`, `error.rs` ~297, `io/unix.rs`
     ~50-73) and `MemoryIO::open_file` (`io/memory.rs` ~59-67) return for a
     missing file opened without `Create`.
   - Otherwise, the log exists if `File::size()` (`io/mod.rs` ~199) is
     `> 0`.
   - Any other error, from the open or from `size()`, propagates. So the
     function returns `Result<bool>` under the option (or always; your
     choice, as long as behaviour with the option off is unchanged). No
     `unwrap`.
   - Use `ReadOnly`, not the database's own open flags, so that the probe
     takes no lock: on `UnixIO`, a read-write open without `NoLock` takes
     an exclusive `fcntl` lock (`io/unix.rs` ~67-71). Separately, on POSIX,
     closing *any* descriptor to a file drops every `fcntl` lock the
     process holds on it, whatever flags it was opened with. So the probe
     must not run while this process has the log open: drop the probe file
     before `open_mv_store` opens the log for real. In
     `reload_wal_after_external_restore` the probe runs only when
     `mvcc_enabled()` is false (the `||` short-circuits), so no store has
     the log open there; keep that ordering.
3. **Both `logical_log_exists` callers** honour the option: the one on
   every open (`database.rs` ~2000) and the one in
   `reload_wal_after_external_restore` (~2678).

The option affects only the default file-backed MVCC storage. When a custom
`DurableStorage` is supplied, `open_mv_store` still does not open a log
file. The existence check still runs, with the name and mechanism that the
option selects.

How you thread the option is up to you. One way: a helper such as
`journal_mode::logical_log_path(db_path, canonical_path, suffix_naming) ->
String`, and an extra parameter (the resolved log path, or the flag) on
`logical_log_exists` and `open_mv_store`. Each caller already has the
`Database` and so its `opts`. Keep the off path calling exactly what it
calls today.

### Doc comment

Update the module doc comment in `mvcc/persistent_storage/logical_log.rs`
(~9-10). It should say that the log is named `<db stem>.db-log` (the db
path with its extension replaced) by default, and `{db path}-log`, opened
and existence-checked only through the database's `IO`, when
`DatabaseOpts::with_mvcc_log_suffix_naming(true)` is set.

## Out of scope: do not change

Two other places reach `std::fs` on the MVCC open path. Leave both exactly
as they are:

- **`get_database_canonical_path`** and its `std::fs::canonicalize` call.
  Under the option, the log name no longer depends on it, but other code
  (for example `PRAGMA database_list`) still uses it.
- **The process-wide `Database` registry**, keyed by `(dev, ino)` from
  `std::fs::metadata` (`io/mod.rs` `get_file_id` ~94-130,
  `database.rs` `lookup_in_registry` ~1105). It may return an
  already-open `Database` with the IO that opened it first. That is
  accepted behaviour.

## Regression tests

Add a new `#[cfg(all(test, feature = "fs"))] mod mvcc_log_suffix_tests;` in
`core/lib.rs`, beside `orphan_wal_tests` (~38), in a new file
`core/mvcc_log_suffix_tests.rs`. Open dbs with
`Database::open_file_with_flags(io, path, OpenFlags::default(),
DatabaseOpts::new().with_mvcc_log_suffix_naming(..), None,
Arc::new(SqliteDialect))`, as `orphan_wal_tests::open_db` does. Turn MVCC on
with `PRAGMA journal_mode = 'mvcc'` (see `MvccTestDb::new` in
`mvcc/database/tests.rs`), then create a table and insert rows so that the
log holds at least one committed frame. Each test uses its own temp dir, and
db paths inside it, so that "nothing on disk" can be checked by listing
that dir.

1. **Option on, `MemoryIO`, log inside the IO.** Use a `MemoryIO` and the
   path `{tmp}/foo.sqlite`. After the MVCC writes,
   `io.open_file("{tmp}/foo.sqlite-log", OpenFlags::ReadOnly, false)`
   succeeds with `size() > 0`. Neither `{tmp}/foo.db-log` nor
   `{tmp}/foo.sqlite.db-log` exists in the IO. The temp dir on real disk is
   still empty. Close, reopen the same path on the same `MemoryIO` with the
   option on: the rows are there. (Close may checkpoint and truncate the
   log; that is fine. If it does not, this reopen also exercises recovery
   from `{name}-log`.)
2. **Option on, distinct logs.** On one `MemoryIO`, open `{tmp}/foo.sqlite`
   and `{tmp}/foo.db`, put each in MVCC mode, and write different rows to
   each. `{tmp}/foo.sqlite-log` and `{tmp}/foo.db-log` both exist in the IO
   and are different files (different contents). After reopening each, each
   db sees only its own rows. The temp dir on real disk is still empty.
3. **Option on, a stray on-disk file is ignored.** Write a non-empty file
   at `{tmp}/foo.db-log` on the real disk. Open `{tmp}/foo.db` on a
   `MemoryIO` (WAL mode, no MVCC) with the option on, connect, and run a
   query: the open succeeds. (For `foo.db` the suffix name and the upstream
   name are the same string, `foo.db-log`; the open succeeds only because
   the probe looks inside the `MemoryIO`, not on disk.)
4. **Option off, the stray file still fails (shows test 3 is meaningful).**
   The same setup as test 3 with the option off fails at open with the
   `Corrupt("MVCC logical log file exists …")` error.
5. **Control: option off, `PlatformIO`, upstream naming.** In a temp dir,
   open `{tmp}/foo.sqlite` on `PlatformIO` with the option off and put it in
   MVCC mode with writes. The log is on disk at `{tmp}/foo.db-log`
   (`with_extension("db-log")`) and is non-empty, and `{tmp}/foo.sqlite-log`
   does not exist. While that database is still open (closing it may
   checkpoint and truncate the log to 0), open a fresh `{tmp}/foo.db`
   (WAL mode) on a fresh `PlatformIO` with the option off: it fails with the
   same `Corrupt` error, because `foo.sqlite` and `foo.db` share
   `foo.db-log`.
6. **Option on, a missing log does not error.** A fresh `{tmp}/bar.db` on
   `MemoryIO` with the option on opens in WAL mode with no log present, and
   nothing named `{tmp}/bar.db-log` is created in the IO by the existence
   check.
7. **Option on, external-restore reload** (gate it with
   `#[cfg(feature = "conn_raw_api")]`, because `conn_raw_api` is not a
   default feature). Two cases:
   - **MVCC mode** (covers the reload's `open_mv_store` naming). Set up as
     in test 1, then call `Connection::reload_wal_after_external_restore()`.
     It succeeds, the rows are still visible, `{tmp}/foo.sqlite-log` is
     still the only log in the IO (no `{tmp}/foo.db-log` was created), and
     the temp dir on real disk is still empty.
   - **WAL mode** (covers the reload's `logical_log_exists` call, which the
     MVCC case skips because `mvcc_enabled()` short-circuits it). Write a
     non-empty stray `{tmp}/foo.db-log` on the real disk. Open `{tmp}/foo.db`
     on a `MemoryIO` with the option on, in WAL mode with a table and a
     row, and call `reload_wal_after_external_restore()`. It succeeds, the
     row is visible, and the database still has no MVCC store
     (`get_mv_store()` is `None`), because the probe looked inside the IO
     and found no log.

## Constraints

- Option off: no behaviour change of any kind. Existing tests, including
  those that build `with_extension("db-log")` paths in
  `mvcc/database/tests.rs`, must pass unchanged.
- Do not change the canonicalize call or the registry (see "Out of
  scope").
- Do not make any other changes. Keep the diff focused, follow turso's
  existing code style, error handling and naming, and do no unrelated
  refactors.
- Run `cargo fmt`, `cargo clippy` on `turso_core`, and
  `cargo test -p turso_core --lib -- mvcc_log_suffix_tests orphan_wal_tests mvcc`,
  and `cargo test -p turso_core --lib --features conn_raw_api -- mvcc_log_suffix_tests`
  (test 7 compiles only with that feature). Do not run the full `make test`.

## Update README

In the "Fork Features" subsection of the "About This Fork" section of
`README.md`, add a bullet: `DatabaseOpts::with_mvcc_log_suffix_naming`
(default off). When on, the MVCC logical log is named `{path}-log`, using
the same `{path}` as the default WAL name `{path}-wal`, instead of
`with_extension("db-log")`, and it is opened and
existence-checked only through the supplied `IO`. When off, naming and
behaviour match upstream.
