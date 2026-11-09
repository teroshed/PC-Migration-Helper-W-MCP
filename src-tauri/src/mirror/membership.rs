use super::{
    scan::{self, Scratch},
    windows::{EvidenceDirectory, Gap, Side, Stamp},
};
use crate::store;
use rusqlite::{Connection, Rows};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Witness {
    entries: u64,
    sha256: String,
}

/// The caller provides exact names sorted under SQLite BINARY collation.
/// Length framing preserves boundaries and enumeration order is immaterial.
pub(super) fn from_ordered_rows(
    rows: &mut Rows<'_>,
    cancel: &AtomicBool,
) -> Result<Witness, String> {
    let mut digest = Sha256::new();
    digest.update(b"pc-migration-directory-membership-v1\0");
    let mut entries = 0_u64;
    while let Some(row) = rows.next().map_err(store::error)? {
        scan::check_cancel(cancel)?;
        let name: String = row.get(0).map_err(store::error)?;
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        entries += 1;
    }
    Ok(Witness {
        entries,
        sha256: format!("{:x}", digest.finalize()),
    })
}

/// One reusable on-disk sort table bounds memory even for a wide directory.
pub(super) struct Checker {
    db: Connection,
    _scratch: Scratch,
}

impl Checker {
    pub(super) fn new(directory: &EvidenceDirectory) -> Result<Self, String> {
        let scratch = Scratch::create(directory)?;
        let db = Connection::open(&scratch.path).map_err(store::error)?;
        db.execute_batch(
            "PRAGMA journal_mode=OFF;
             PRAGMA synchronous=OFF;
             PRAGMA temp_store=MEMORY;
             PRAGMA cache_size=-4096;
             CREATE TABLE names(name TEXT PRIMARY KEY COLLATE BINARY);",
        )
        .map_err(store::error)?;
        Ok(Self {
            db,
            _scratch: scratch,
        })
    }

    pub(super) fn recheck(
        &mut self,
        side: &Side,
        relative: &str,
        stamp: &Stamp,
        expected: &Witness,
        cancel: &AtomicBool,
    ) -> Result<Option<Gap>, String> {
        let tx = self.db.transaction().map_err(store::error)?;
        tx.execute("DELETE FROM names", []).map_err(store::error)?;
        let mut duplicate = false;
        let observed = {
            let mut insert = tx
                .prepare_cached("INSERT OR IGNORE INTO names VALUES(?1)")
                .map_err(store::error)?;
            side.enumerate_names(relative, stamp, cancel, |name| {
                duplicate |= insert.execute([name]).map_err(store::error)? == 0;
                Ok(())
            })?
        };
        let gap = match observed {
            Err(gap) => Some(gap),
            Ok(()) if duplicate => Some(Gap::changed(
                "Directory enumeration returned a repeated name.",
            )),
            Ok(()) => {
                let mut query = tx
                    .prepare_cached("SELECT name FROM names ORDER BY name")
                    .map_err(store::error)?;
                let actual =
                    from_ordered_rows(&mut query.query([]).map_err(store::error)?, cancel)?;
                (actual != *expected).then(|| {
                    Gap::changed("Exact directory membership names changed since enumeration.")
                })
            }
        };
        tx.commit().map_err(store::error)?;
        Ok(gap)
    }
}
