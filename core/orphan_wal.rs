//! Orphan-WAL resolution for [`OrphanWalPolicy::Discard`].
//!
//! An orphan WAL is a nonempty `{db}-wal` beside a db file that holds no
//! database (see [`EmptyDb`]). Replaying it resurrects the old database as
//! soon as page 1 is allocated, so a read-write `Discard` open truncates it
//! (and a non-empty but empty-counting db file) before the WAL is scanned.
//! Read-only opens never modify any file.

use crate::io::FileSyncType;
use crate::storage::encryption::{CipherMode, SQLITE_HEADER, TURSO_HEADER_PREFIX};
use crate::storage::pager;
use crate::storage::sqlite3_ondisk::{DatabaseHeader, PageSize, TextEncoding};
use crate::sync::Arc;
use crate::types::IOCompletions;
use crate::{
    io_yield_one, Completion, CompletionError, Database, DbHeaderReadState, EmptyDb, File,
    IOResult, LimboError, OpenFlags, OrphanWalPolicy, ReadOnlyOrphanWal, Result,
};

/// Sub state machine for [`Database::resolve_orphan_wal`], driven by the
/// `ResolvingOrphanWal` open phase.
#[derive(Default)]
pub(crate) enum OrphanWalState {
    /// Decide whether the db file counts as empty. `probed` is set once the
    /// `sync_parent_dir` probe has succeeded; the check then runs again right
    /// before anything is truncated, because the probe may have yielded.
    Check {
        probed: bool,
    },
    /// Reading page 1 to apply the [`EmptyDb::InvalidHeader`] rule.
    ReadingHeader {
        probed: bool,
        db_size: u64,
        read: DbHeaderReadState,
    },
    /// Waiting for the `IO::sync_parent_dir` support probe.
    ProbingDirSync {
        completion: Completion,
    },
    TruncatingDb {
        completion: Completion,
    },
    SyncingDb {
        completion: Completion,
    },
    /// Discard the WAL, if one of nonzero length exists.
    DiscardWal,
    TruncatingWal {
        file: Arc<dyn File>,
        completion: Completion,
    },
    SyncingWal {
        /// Held open until the sync completes.
        _file: Arc<dyn File>,
        completion: Completion,
    },
    #[default]
    Done,
}

impl OrphanWalState {
    pub(crate) fn new() -> Self {
        Self::Check { probed: false }
    }
}

/// Whether the db file counts as empty under the configured [`EmptyDb`] rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Emptiness {
    NotEmpty,
    /// Absent, 0 bytes, or (under [`EmptyDb::OneByte`]) 1 byte.
    Empty {
        db_size: u64,
    },
    /// Under [`EmptyDb::InvalidHeader`]: page 1 is not a valid header.
    InvalidHeader {
        db_size: u64,
    },
}

impl Emptiness {
    fn by_size(rule: EmptyDb, db_size: u64) -> Option<Self> {
        match (rule, db_size) {
            (_, 0) => Some(Self::Empty { db_size }),
            (EmptyDb::ZeroBytes, _) => Some(Self::NotEmpty),
            (EmptyDb::OneByte, 1) => Some(Self::Empty { db_size }),
            (EmptyDb::OneByte, _) => Some(Self::NotEmpty),
            // Page 1 is at least `PageSize::MIN` bytes: a shorter file cannot
            // hold a complete page 1, whatever its first bytes say.
            (EmptyDb::InvalidHeader, n) if n < PageSize::MIN as u64 => {
                Some(Self::InvalidHeader { db_size })
            }
            // Needs the header contents.
            (EmptyDb::InvalidHeader, _) => None,
        }
    }
}

/// Whether `page1` starts with a valid database header, for the
/// [`EmptyDb::InvalidHeader`] rule.
///
/// Accepts what `Database::header_validation` accepts for the fixed fields,
/// including schema format 0 and an unset (0) text encoding: SQLite writes
/// both only when the first table is created, and until the first checkpoint
/// such a page 1 can sit beside a WAL holding committed frames.
fn is_valid_db_header(page1: &[u8]) -> bool {
    // An encrypted db replaces the magic with a Turso header and cannot be
    // validated without the key. Never treat it as empty.
    if page1.starts_with(TURSO_HEADER_PREFIX) {
        return true;
    }
    let header: &DatabaseHeader = bytemuck::from_bytes(&page1[..DatabaseHeader::SIZE]);
    // Bytes 72..92 are "reserved for expansion" and must be zero.
    let reserved_zero = page1[72..92].iter().all(|&b| b == 0);
    let text_encoding = header.text_encoding;
    header.magic == SQLITE_HEADER
        && PageSize::new(header.page_size.get()).is_some()
        && header.max_embed_frac == 64
        && header.min_embed_frac == 32
        && header.leaf_frac == 32
        && (0..=4).contains(&header.schema_format.get())
        && matches!(
            text_encoding,
            TextEncoding::Unset
                | TextEncoding::Utf8
                | TextEncoding::Utf16Le
                | TextEncoding::Utf16Be
        )
        && reserved_zero
}

/// Returns `Err` if `completion` failed, `Ok(true)` once it has finished.
fn completion_done(completion: &Completion) -> Result<bool> {
    if let Some(err) = completion.get_error() {
        return Err(err.into());
    }
    Ok(completion.finished())
}

impl Database {
    /// Apply [`OrphanWalPolicy::Discard`] before anything reads the db header
    /// or scans the WAL.
    ///
    /// Read-write opens of an empty db truncate the db file (if non-empty) and
    /// a nonempty WAL to 0 bytes, fsyncing each, and mark this `Database` as
    /// creating the db so that `allocate_page1` makes page 1 durable before
    /// the first WAL frame. Read-only opens never modify any file.
    pub(crate) fn resolve_orphan_wal(&mut self, st: &mut OrphanWalState) -> Result<IOResult<()>> {
        let OrphanWalPolicy::Discard { empty, read_only } = self.opts.orphan_wal_policy else {
            unreachable!("resolve_orphan_wal requires OrphanWalPolicy::Discard");
        };
        let is_readonly = self.open_flags.contains(OpenFlags::ReadOnly);
        loop {
            match st {
                OrphanWalState::Check { probed } => {
                    let probed = *probed;
                    let db_size = self.db_file.size()?;
                    match Emptiness::by_size(empty, db_size) {
                        Some(emptiness) => {
                            if let Some(next) =
                                self.on_emptiness(emptiness, probed, is_readonly, read_only)?
                            {
                                *st = next;
                            } else {
                                *st = OrphanWalState::Done;
                                return Ok(IOResult::Done(()));
                            }
                        }
                        None => {
                            *st = OrphanWalState::ReadingHeader {
                                probed,
                                db_size,
                                read: DbHeaderReadState::default(),
                            };
                        }
                    }
                }
                OrphanWalState::ReadingHeader {
                    probed,
                    db_size,
                    read,
                } => {
                    let buf = crate::return_if_io!(self.read_db_header_buf(read));
                    let emptiness = if is_valid_db_header(buf.as_slice()) {
                        Emptiness::NotEmpty
                    } else {
                        Emptiness::InvalidHeader { db_size: *db_size }
                    };
                    let probed = *probed;
                    if let Some(next) =
                        self.on_emptiness(emptiness, probed, is_readonly, read_only)?
                    {
                        *st = next;
                    } else {
                        *st = OrphanWalState::Done;
                        return Ok(IOResult::Done(()));
                    }
                }
                OrphanWalState::ProbingDirSync { completion } => {
                    if !completion_done(completion)? {
                        io_yield_one!(completion.clone());
                    }
                    *st = OrphanWalState::Check { probed: true };
                }
                OrphanWalState::TruncatingDb { completion } => {
                    if !completion_done(completion)? {
                        io_yield_one!(completion.clone());
                    }
                    let c = self.db_file.sync(
                        Completion::new_sync(|_| {
                            tracing::trace!("orphan WAL: db file synced after truncation");
                        }),
                        FileSyncType::Fsync,
                    )?;
                    *st = OrphanWalState::SyncingDb { completion: c };
                }
                OrphanWalState::SyncingDb { completion } => {
                    if !completion_done(completion)? {
                        io_yield_one!(completion.clone());
                    }
                    *st = OrphanWalState::DiscardWal;
                }
                OrphanWalState::DiscardWal => {
                    let Some(file) = self.open_existing_wal()? else {
                        self.finish_discard();
                        *st = OrphanWalState::Done;
                        return Ok(IOResult::Done(()));
                    };
                    if file.size()? == 0 {
                        self.finish_discard();
                        *st = OrphanWalState::Done;
                        return Ok(IOResult::Done(()));
                    }
                    tracing::warn!(
                        "discarding orphan WAL '{}' beside empty database '{}'",
                        self.wal_path,
                        self.path
                    );
                    let c = file.truncate(
                        0,
                        Completion::new_trunc(|_| {
                            tracing::trace!("orphan WAL truncated to 0 B");
                        }),
                    )?;
                    *st = OrphanWalState::TruncatingWal {
                        file,
                        completion: c,
                    };
                }
                OrphanWalState::TruncatingWal { file, completion } => {
                    if !completion_done(completion)? {
                        io_yield_one!(completion.clone());
                    }
                    let c = file.sync(
                        Completion::new_sync(|_| {
                            tracing::trace!("orphan WAL synced after truncation");
                        }),
                        FileSyncType::Fsync,
                    )?;
                    *st = OrphanWalState::SyncingWal {
                        _file: file.clone(),
                        completion: c,
                    };
                }
                OrphanWalState::SyncingWal { completion, .. } => {
                    if !completion_done(completion)? {
                        io_yield_one!(completion.clone());
                    }
                    self.finish_discard();
                    *st = OrphanWalState::Done;
                    return Ok(IOResult::Done(()));
                }
                OrphanWalState::Done => {
                    unreachable!("resolve_orphan_wal called after completion")
                }
            }
        }
    }

    /// Act on the emptiness verdict. Returns the next state, or `None` when
    /// the open proceeds without touching anything.
    fn on_emptiness(
        &mut self,
        emptiness: Emptiness,
        probed: bool,
        is_readonly: bool,
        read_only: ReadOnlyOrphanWal,
    ) -> Result<Option<OrphanWalState>> {
        let db_size = match emptiness {
            Emptiness::NotEmpty => return Ok(None),
            Emptiness::Empty { db_size } | Emptiness::InvalidHeader { db_size } => db_size,
        };

        if is_readonly {
            if let Emptiness::InvalidHeader { .. } = emptiness {
                return Err(LimboError::Corrupt(format!(
                    "database '{}' does not have a valid header and cannot be opened read-only under OrphanWalPolicy::Discard with EmptyDb::InvalidHeader",
                    self.path
                )));
            }
            match read_only {
                ReadOnlyOrphanWal::Replay => {}
                ReadOnlyOrphanWal::Ignore => {
                    self.orphan_wal_ignored = true;
                    if db_size != 0 {
                        // A 1-byte db reads as empty; nothing is written.
                        self.install_init_page_1();
                    }
                }
            }
            return Ok(None);
        }

        if !probed {
            self.check_multiprocess_discard(db_size)?;
            // This open is about to create the database, and page 1 must then
            // reach the directory durably. Refuse now rather than fall back
            // to a weaker guarantee.
            let c = match self.io.sync_parent_dir(
                &self.path,
                Completion::new_sync(|_| {
                    tracing::trace!("orphan WAL: sync_parent_dir probe done");
                }),
            ) {
                Ok(c) => c,
                Err(err @ LimboError::IoExtensionUnsupported(_)) => {
                    tracing::error!(
                        "OrphanWalPolicy::Discard cannot create database '{}': {err}",
                        self.path
                    );
                    return Err(err);
                }
                Err(err) => return Err(err),
            };
            return Ok(Some(OrphanWalState::ProbingDirSync { completion: c }));
        }

        self.creates_db_file = true;
        if db_size == 0 {
            return Ok(Some(OrphanWalState::DiscardWal));
        }
        tracing::warn!(
            "truncating database '{}' ({db_size} bytes) that holds no database",
            self.path
        );
        let c = self.db_file.truncate(
            0,
            Completion::new_trunc(|_| {
                tracing::trace!("orphan WAL: db file truncated to 0 B");
            }),
        )?;
        Ok(Some(OrphanWalState::TruncatingDb { completion: c }))
    }

    /// Open `{db}-wal` without creating or locking it. `None` if it does not
    /// exist.
    fn open_existing_wal(&self) -> Result<Option<Arc<dyn File>>> {
        let flags = (self.open_flags & !OpenFlags::Create) | OpenFlags::NoLock;
        match self.io.open_file(&self.wal_path, flags, false) {
            Ok(file) => Ok(Some(file)),
            Err(LimboError::CompletionError(CompletionError::IOError(
                std::io::ErrorKind::NotFound,
                _,
            ))) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Under experimental multiprocess WAL, only discard (or truncate the db)
    /// when no other process is attached to the `.tshm` authority: replaying
    /// would reintroduce the orphan-WAL hazard, and truncating would corrupt
    /// the peer's view. Opens the authority, which `OpenWal` then reuses.
    #[cfg(host_shared_wal)]
    fn check_multiprocess_discard(&self, db_size: u64) -> Result<()> {
        use crate::storage::shared_wal_coordination::SharedWalCoordinationOpenMode;

        if !self.opts.enable_multiprocess_wal {
            return Ok(());
        }
        let wal_size = match self.open_existing_wal()? {
            Some(file) => file.size()?,
            None => 0,
        };
        if db_size == 0 && wal_size == 0 {
            return Ok(());
        }
        let Some(authority) = self.open_shared_wal_coordination_for_open()? else {
            return Ok(());
        };
        if authority.open_mode() == SharedWalCoordinationOpenMode::MultiProcess {
            return Err(LimboError::LockingError(format!(
                "cannot discard orphan WAL '{}' ({wal_size} bytes) beside empty database '{}' ({db_size} bytes): another process is attached via '{}'",
                self.wal_path,
                self.path,
                crate::storage::wal::coordination_path_for_wal_path(&self.wal_path),
            )));
        }
        Ok(())
    }

    #[cfg(not(host_shared_wal))]
    fn check_multiprocess_discard(&self, _db_size: u64) -> Result<()> {
        Ok(())
    }

    /// The db file is now 0 bytes: make the rest of open see it as empty.
    fn finish_discard(&self) {
        if self.init_page_1.load().is_none() {
            self.install_init_page_1();
        }
    }

    fn install_init_page_1(&self) {
        let cipher_mode = self.encryption_cipher_mode.get();
        let cipher = match cipher_mode {
            CipherMode::None => None,
            ref mode => Some(mode),
        };
        self.init_page_1.store(Some(pager::default_page1(cipher)));
    }
}
