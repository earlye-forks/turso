# Add an opt-in orphan-WAL policy: discard a leftover `-wal` beside an empty db

## Problem [feature-001]

turso replays a leftover `{db}-wal` even when the db file it belongs to is
absent or 0 bytes. The old database then comes back on the next write that
allocates page 1, with every table and row intact and
`PRAGMA integrity_check` = `ok`. A consumer that wipes a db file (but not its
sidecars) and reopens it gets the old database back, silently.

Code locations below are hints, relative to `core/` at upstream rev
`4b59a37`. Line numbers have probably drifted; search by name.

### What turso does today

- Open always loads `{path}-wal` if present (`lib.rs` ~1383, `format!("{path}-wal")`).
  The `OpenWal` phase (`lib.rs` ~1957-2022) always opens, scans and attaches
  the shared WAL via `WalFileShared::open_shared_if_exists_begin`
  (`storage/wal.rs` ~5634) and `BuildSharedWal::begin`
  (`storage/sqlite3_ondisk.rs` ~1459). A 0-byte WAL is already treated as no
  WAL (`sqlite3_ondisk.rs` ~1498).
- Validation checks the WAL only against itself (header checksum, page size,
  frame salts and cumulative checksums). Nothing compares it to the db file.
- When `db_size == 0`, `Database::new` (`lib.rs` ~715-736) installs an
  in-memory default page 1 (`init_page_1`), and header reads are served from
  it (`storage/pager.rs` ~1809). The schema reads as empty, though the WAL
  holds a full database.
- `allocate_page1` (`storage/pager.rs` ~5182-5257) writes page 1 to the **db
  file** and clears the in-memory page 1 as soon as a write transaction starts
  allocating, whether or not it commits. It waits for the write but does not
  fsync it. After that the db file is 4096 bytes and the WAL frames become
  visible: the old database is back, permanently.

### Reproduction (the regression test must reproduce this)

1. In a fresh temp dir, open `g.db` with `Database::open_file(io, path)`
   (`PlatformIO`), `connect()`, run
   `CREATE TABLE old_t(x, b); CREATE TABLE old_u(z); CREATE TABLE old_v(w);`,
   then 300 times
   `INSERT INTO old_t VALUES (i, randomblob(200)); INSERT INTO old_v VALUES (randomblob(300));`.
   While open, the WAL holds all of it (about 2.9 MB). Copy `g.db-wal` aside,
   then close.
2. Delete `g.db` and `g.db-wal`; copy the saved WAL back as `g.db-wal`. Now
   there is no db file beside a self-consistent WAL.
3. Reopen with a fresh IO. `SELECT name FROM sqlite_schema` returns no rows;
   `SELECT * FROM old_t` fails with `no such table`.
4. On that connection run `BEGIN; CREATE TABLE z(q); ROLLBACK;` (succeeds).
   `g.db` is now 4096 bytes. A new connection sees `old_t, old_u, old_v` and
   `SELECT count(*) FROM old_t` = 300.
5. Close, drop the `Database`, reopen with a fresh IO: still `old_t, old_u,
   old_v`, count 300, `integrity_check` = `ok`.

(If the first write after step 3 commits instead of rolling back, the old data
stays hidden. Whether it comes back depends only on whether page 1 was
allocated before the first commit.)

SQLite avoids this: `pagerOpenWalIfPresent` deletes the WAL when the db has
zero pages instead of opening it. This change adds an opt-in equivalent.

## API

In `core/lib.rs`, add beside the existing `enable_*` fields of `DatabaseOpts`,
with a `with_orphan_wal_policy` builder matching the existing `with_*`
builders, and re-export the types from the crate root:

```rust
pub enum OrphanWalPolicy {
    /// Default. Upstream behaviour, unchanged.
    Replay,
    /// Read-write opens discard an orphan WAL instead of replaying it.
    Discard { empty: EmptyDb, read_only: ReadOnlyOrphanWal },
}
pub enum EmptyDb { ZeroBytes, OneByte, InvalidHeader }
pub enum ReadOnlyOrphanWal { Ignore, Replay }

impl DatabaseOpts {
    pub fn with_orphan_wal_policy(mut self, policy: OrphanWalPolicy) -> Self;
}
```

`OrphanWalPolicy::default()` is `Replay`, and `DatabaseOpts::default()` uses
it. Derive what the neighbouring option types derive (`Debug`, `Clone`,
`Copy`, `PartialEq`, `Eq`, as applicable). There is deliberately no `Ignore`
for read-write opens (keeping an old WAL while writing new frames would need a
safe header reset; discarding avoids that) and no `Delete` for read-only opens
(read-only opens never modify anything). Keep the names and shape as given; a
later change may make `Discard` the default.

## Behaviour

Terms: an **empty db** is a db file that counts as holding no database under
the configured `EmptyDb` rule. An **orphan WAL** is a `{db}-wal` of nonzero
length beside an empty db.

### `EmptyDb` rules (each includes the ones before it)

- `ZeroBytes`: the db file is absent or 0 bytes — exactly when `init_page_1`
  is installed today.
- `OneByte`: also a 1-byte file (SQLite's unix VFS reports a 1-byte file as
  0 bytes; this copies it).
- `InvalidHeader`: also any file whose page 1 is not a valid header: shorter
  than 100 bytes, or failing any of: magic `"SQLite format 3\0"`; page size a
  power of two in 512..=32768, or the value 1 (meaning 65536); payload
  fractions 64/32/32; schema format 1-4; text encoding 1-3; reserved bytes
  72-91 all zero. Page 1 is the only self-identifying page, so "no valid
  pages" and "invalid page 1" are the same rule. Reuse existing header
  parsing/validation code where it exists rather than duplicating it.

### Read-write open under `Discard`

When the db file counts as empty:

1. If the db file is non-empty (1 byte, or an invalid header), truncate it to
   0 and fsync it, so the open proceeds exactly as for a 0-byte db
   (`init_page_1` installed). For `InvalidHeader` this happens whether or not
   a WAL exists: this rule deliberately wipes a corrupt db. Because
   `Database::new` reads `db_size` before this point, make sure the rest of
   open sees the db as size 0 (install `init_page_1`), not the stale size.
2. If `{db}-wal` exists with nonzero length, **discard** it before the
   `OpenWal` scan: truncate it to 0 with `File::truncate` and then `File::sync`
   it, modelled on checkpoint `TRUNCATE` (`storage/wal.rs` ~5084-5120).
   **Do not** use `IO::remove_file` (it silently does nothing on the browser
   IO) and do not use `std::fs`. All file operations go through the supplied
   `IO`, since hosts may supply non-filesystem IOs. The file stays in place;
   a 0-byte WAL is already "no WAL".
3. Re-check the empty rule against the db file's current size/contents
   immediately before truncating anything, because `db_size` was read earlier
   in `Database::new`. If it no longer counts as empty, replay as usual.
4. Either truncation order converges: a crash between the db and WAL
   truncations leaves a state the next open finishes.

Concurrency guards:

- **Legacy mode** (no `enable_multiprocess_wal`): no extra check. The
  exclusive lock on the db file already excludes other processes.
- **Multiprocess mode** (`enable_multiprocess_wal`, db opened `NoLock`,
  `lib.rs` ~840 and ~2541-2564): only discard when the `{db}-tshm`
  coordination authority, opened inside `OpenWal` (`lib.rs` ~1976), reports
  `SharedWalCoordinationOpenMode::Exclusive`
  (`storage/shared_wal_coordination.rs` `detect_open_mode`). If it reports
  `MultiProcess` and a discard or db truncation would otherwise happen, **fail
  the open with a clear error** naming the orphan WAL and the attached peer.
  Replaying would reintroduce the hazard; truncating would corrupt the peer's
  view. This means the discard decision must be made after the authority is
  opened but before the WAL is scanned/attached. If the db is not empty,
  multiprocess opens are unaffected.
- **Registry-bypassed opens** (`open_with_flags_bypass_registry*`,
  `lib.rs` ~1291 and ~1325) let a second `Database` in the same process open
  the same file; fcntl locks don't exclude the same process. The in-process
  `DATABASE_MANAGER` registry is otherwise safe (it returns the existing
  `Database`, so `OpenWal` doesn't run again). Either confirm by reading the
  code that discard is safe for bypassed opens (the suspected risk is only a
  zero-page window, since page 1 reaches the db file before any WAL frame),
  or make `Discard` fail such opens with a clear error. State which you did,
  and why, in the commit message.

### Read-only open under `Discard`

Read-only opens **never modify any file**, under any setting.

- If the db counts as empty under `ZeroBytes`/`OneByte` rules, follow
  `read_only`:
  - `Ignore`: do not scan or attach the WAL; leave the file untouched. The
    db reads as empty (treat a 1-byte db as size 0 for `init_page_1`
    purposes, without writing to it).
  - `Replay`: exactly what upstream does today.
- Under `InvalidHeader`, a read-only open of a db whose header is invalid
  (including a 1..99-byte file) **fails** with a clear error, whatever
  `read_only` is set to, and modifies nothing. An absent or 0-byte db
  follows `read_only` as above.

### Page-1 durability under `Discard`

Under any `Discard` value, `allocate_page1` must, after writing page 1 to the
db file and before the first WAL frame is written:

- fsync the db file (`File::sync`), and
- if this `Database` created the db file (it did not exist before this
  open), call `IO::sync_parent_dir(db_path, ..)` and wait for it.

Without this, a power loss after the first commit can leave a legitimate WAL
of committed frames beside a db that is absent, 0 bytes, or has a zeroed
page 1, and `Discard` would throw that WAL away. `InvalidHeader` in
particular depends on it: with page 1 durable before any WAL frame, an invalid
page 1 can never be hiding committed frames. Cost: one or two fsyncs per db
creation.

Also under `Discard`, on a read-write open where the db counts as empty
(so a db is about to be created), probe `sync_parent_dir` support (call it
on the db path, or add a cheap check that returns the same answer) and
**fail the open with a clear error if it returns the unsupported error**
(see below). Never fall back to a weaker guarantee. Don't probe when the db
is not empty: nothing is being created, so the directory sync isn't needed,
and an open of an existing db should not pay a directory fsync.

### `Replay`

Under `Replay` (the default) behaviour is byte-for-byte unchanged: no new file
operations, no extra fsyncs, no `sync_parent_dir` calls.

## New required IO method: `sync_parent_dir`

Add to the `IO` trait in `core/io/mod.rs` a **required** method (no default
body):

```rust
/// Make the directory entry for `path` durable by fsyncing its parent
/// directory. Returns an unsupported error if this IO cannot provide that.
fn sync_parent_dir(&self, path: &str, c: Completion) -> Result<Completion>;
```

It is Completion-based like `File::sync`; synchronous impls may complete `c`
before returning. For impls that cannot honour it, add one dedicated,
matchable error variant to `LimboError` (e.g.
`LimboError::IoExtensionUnsupported(&'static str)`, naming the method) if no
existing variant fits; `Discard` checks for exactly that variant.

Implement it for **every** `impl IO` in the tree. Find them all with a search
such as `grep -rnE "impl(<.*>)? +[A-Za-z_:]*IO +for " --include=*.rs .` —
include tests, simulators, the sync engine and the bindings, and any impl
added upstream since this was written. Known impls at `4b59a37`:

| impl | location | behaviour |
|---|---|---|
| `UnixIO` | `core/io/unix.rs` | open the parent dir, fsync it |
| `UringIO` | `core/io/io_uring.rs` | open the parent dir, fsync it (sync or via the ring) |
| `GenericIO` | `core/io/generic.rs` | on unix: open parent dir, fsync; elsewhere: no-op `Ok` |
| `SparseLinuxIo` | `sync/engine/src/sparse_io.rs` | open the parent dir, fsync it |
| `MemoryIO`, `MemoryYieldIO` (+ test `StepGuardedIO`) | `core/io/memory.rs`, `core/io/memory_yield.rs` | no-op `Ok` (nothing survives a crash) |
| `WindowsIO`, `WindowsIOCP` | `core/io/windows.rs`, `core/io/win_iocp.rs` | no-op `Ok` (NTFS journals metadata) — **confirm this** and note the conclusion in a code comment; if a directory flush is actually needed, implement it |
| `VfsMod` | `core/io/vfs.rs` | unsupported error (the C-ABI extension has no slot for it) |
| `Opfs` | `bindings/javascript/src/browser.rs` | unsupported error |
| `SyncCountingIo` (test) | `core/vdbe/vacuum.rs` | delegate to inner |
| simulator / fuzzer / test IOs | `testing/simulator/runner/io.rs`, `testing/simulator/runner/memory/io.rs`, `testing/concurrent-simulator/io.rs`, `testing/differential-oracle/fuzzer/memory/io.rs`, `tests/integration/common.rs`, `tests/integration/queued_io.rs` | delegate to the wrapped IO, or no-op `Ok` for pure in-memory ones |

"Parent dir" of a bare filename is the current directory. An empty parent
means `.`.

## ADR

Add this file to the repo verbatim as
`docs/adr/0001-io-extensions-are-required-trait-methods.md` (create
`docs/adr/`):

````markdown
---
status: accepted
---

# IO extensions are required methods on `IO`

The fork needs capabilities that upstream turso's `IO` trait doesn't
have. The first is `sync_parent_dir`, which the orphan-WAL discard
needs: it makes a newly created db file durable before the first WAL
frame is written. We add each such **IO extension** as a **required**
method on `IO`, not as a provided method with a default. The
behaviour of every in-tree impl is known. A compile error then forces
every other impl, including out-of-tree ones, to decide what the
capability means for it, rather than silently inheriting a default.
An impl that can't honestly provide the capability (such as `VfsMod`
or browser `Opfs` for directory sync) returns an unsupported error,
and the feature that needs the capability refuses to run rather than
falling back to a weaker guarantee.

## Considered Options

- **A provided method with an `Unsupported` default**, like the
  existing `supports_shared_wal_coordination()`. Rejected: an
  out-of-tree IO would pick up `Unsupported` without anyone deciding
  it, and the first sign would be a runtime refusal. Upstream is more
  likely to accept this shape, so it may come back if the extension
  is proposed upstream.
- **A single interface reached by a hop**
  (`fn dir_sync(&self) -> Option<&dyn DirSync>`). Rejected: this pays
  off only for a group of related capabilities behind one interface,
  and there is one method. The hop itself still has to go on `IO`,
  either with a default (the silent opt-out again) or required (the
  same breaking change plus an extra trait).
- **COM-style `query_interface(&self, iid) -> Option<..>`** covering
  many unrelated optional interfaces. Rejected for now: we expect
  neither many related extensions nor many unrelated ones.

## Consequences

- This fork is maintained by re-implementing each fix from scratch
  against fresh upstream, so the cost of a required method is that
  each re-apply touches every `impl IO` in the tree, including any
  that upstream has added since. It does not create merge conflicts.
- **Revisit when the extension set grows.** Reconsider
  `query_interface`, or a single hop interface, once the fork carries
  two or more IO extensions, or once one of them needs a group of
  related methods. The current set is listed under "Fork Features" in
  `README.md`.
- This ADR was added alongside the change that introduced the first
  IO extension. It is maintained in the fork's NQAF prompt history;
  change it there, not by editing this copy.
````

## Regression tests

Add tests in `core` (e.g. a new `#[cfg(test)] mod orphan_wal_tests;` in
`core/lib.rs`, following `core/multiprocess_tests.rs`). Use temp dirs and a
real platform IO where file sizes matter; build the orphan WAL as in the
Reproduction. Cover every setting:

1. **`Replay` control:** Reproduction steps 3-5 still resurrect the old db,
   and a counting IO shows no extra db syncs and no `sync_parent_dir` calls
   compared with today.
2. **`ZeroBytes`:** orphan WAL beside an absent db, and beside a 0-byte db:
   schema empty; after `BEGIN; CREATE TABLE z(q); ROLLBACK;` and a reopen, the
   db is still empty and `integrity_check` = `ok`. A 1-byte db is **not**
   treated as empty (the WAL is not discarded).
3. **`OneByte`:** the 1-byte case is treated as empty too.
4. **`InvalidHeader`, read-write:** a 4096-byte all-zero db plus a WAL: both
   files end up truncated to 0 and the open succeeds with an empty db. The
   same all-zero db with **no** WAL is also truncated and opens empty.
5. **Negative control:** a valid db with its WAL replays as usual under every
   `EmptyDb` rule.
6. **Read-only:** `Ignore` gives an empty schema and the WAL's size and bytes
   are unchanged. `Replay` matches upstream. Under `InvalidHeader`, a
   read-only open of an invalid-header db fails and no file is modified.
7. **Durability:** under `Discard`, a counting IO (copy the `SyncCountingIo`
   pattern in `core/vdbe/vacuum.rs`, and also count `sync_parent_dir` and WAL
   writes) sees a db-file sync and a `sync_parent_dir` after `allocate_page1`
   and before the first WAL write. Power loss is not simulated.
8. **Unsupported IO:** a read-write `Discard` open of an empty db, on an IO
   whose `sync_parent_dir` returns the unsupported error, fails at open. The
   same IO opens a non-empty db normally.
9. **Multiprocess:** with `enable_multiprocess_wal`, `Discard` fails at open
   when another process is attached (the `.tshm` authority reports
   `MultiProcess`). Use a real child process, as the existing tests in
   `core/multiprocess_tests.rs` do (re-exec `current_exe` with a filter).

## Constraints

- Do not break the default open path (`Replay`) or multiprocess WAL mode.
  Existing tests for both must still pass.
- Keep the diff focused on this change. Follow turso's existing code style,
  error handling and naming. No unrelated refactors.
- Run `cargo fmt`, `cargo clippy` on the touched crates, and the relevant
  `cargo test` targets for `core` (the new tests, `multiprocess_tests`, the
  WAL/pager tests, `vacuum`) plus `cargo check` for every crate whose
  `impl IO` you changed (sync engine, simulators, integration tests, JS
  bindings where buildable). Do not run the full `make test`.

## Update README

Add a "Fork Features" subsection to the "About This Fork" section of
`README.md` (added by feature-000) if not present, with a bullet for this
change: the `OrphanWalPolicy` option (default `Replay`), what `Discard` does,
and the new required `IO::sync_parent_dir` method (an **IO extension**; see
`docs/adr/0001-io-extensions-are-required-trait-methods.md`), noting that
out-of-tree `IO` impls must now implement it.
