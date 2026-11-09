//! Frozen source metadata for the interview safety evaluator. No mirror,
//! backup verification, or file-content read is needed for this evidence.
use super::{
    manifest,
    membership::{self, Checker, Witness},
    scan,
    windows::{self, Gap, Kind, Side, Stamp},
    Endpoint,
};
use crate::{filesystem::Volume, store};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

const FORMAT: &str = "pc-migration-source-membership";
const PREFIX: &str = "source";
const SCHEMA: u32 = 1;

#[derive(Clone, Copy)]
pub(crate) enum SourceEnvironment<'a> {
    Live { review_volumes: &'a [Volume] },
    Fixture { source_root: &'a Path },
}

impl SourceEnvironment<'_> {
    fn open(self, path: &str) -> Result<(Endpoint, Side), String> {
        match self {
            Self::Live { review_volumes } => {
                windows::open_source_scope(path, Some(review_volumes), None)
            }
            Self::Fixture { source_root } => {
                windows::open_source_scope(path, None, Some(source_root))
            }
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourceCounts {
    pub(crate) files: u64,
    pub(crate) directories: u64,
    pub(crate) gaps: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceProgress {
    pub(crate) phase: &'static str,
    pub(crate) counts: SourceCounts,
    pub(crate) queued_directories: u64,
    pub(crate) total_known: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourceReceipt {
    pub(crate) schema: u32,
    pub(crate) endpoint: Endpoint,
    pub(crate) manifest_digest: String,
    pub(crate) counts: SourceCounts,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourceCandidate {
    pub(crate) relative: String,
    pub(crate) kind: String,
    pub(crate) fingerprint: String,
    pub(crate) files: u64,
    pub(crate) directories: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Header {
    format: String,
    schema: u32,
    endpoint: Endpoint,
    counts: SourceCounts,
    started_at: String,
    finished_at: String,
}

impl Header {
    fn receipt(&self, manifest_digest: String) -> SourceReceipt {
        SourceReceipt {
            schema: self.schema,
            endpoint: self.endpoint.clone(),
            manifest_digest,
            counts: self.counts.clone(),
            started_at: self.started_at.clone(),
            finished_at: self.finished_at.clone(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Row {
    relative: String,
    stamp: Option<Stamp>,
    gap: Option<Gap>,
    membership: Option<Witness>,
}

impl Row {
    fn observe(relative: String, result: Result<Stamp, Gap>) -> Self {
        match result {
            Ok(stamp) => Self {
                relative,
                stamp: Some(stamp),
                gap: None,
                membership: None,
            },
            Err(gap) => Self {
                relative,
                stamp: None,
                gap: Some(gap),
                membership: None,
            },
        }
    }

    fn count(&self, counts: &mut SourceCounts) -> Result<(), String> {
        match (&self.stamp, &self.gap) {
            (Some(stamp), None) if stamp.kind() == Kind::File => counts.files += 1,
            (Some(_), None) if self.membership.is_some() => counts.directories += 1,
            (None, Some(_)) => counts.gaps += 1,
            _ => return Err("Frozen source observation has inconsistent metadata.".into()),
        }
        Ok(())
    }
}

fn json(value: &impl Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(store::error)
}

fn put(db: &Connection, row: &Row) -> Result<(), String> {
    let directory = row
        .stamp
        .as_ref()
        .is_some_and(|stamp| stamp.kind() == Kind::Directory);
    db.execute(
        "INSERT OR REPLACE INTO source_members VALUES(?1,?2,?3)",
        params![row.relative, directory, json(row)?],
    )
    .map_err(store::error)?;
    Ok(())
}

fn queue(db: &Connection, relative: &str, stamp: &Stamp) -> Result<(), String> {
    db.execute(
        "INSERT INTO source_pending(relative,stamp) VALUES(?1,?2)",
        params![relative, json(stamp)?],
    )
    .map_err(store::error)?;
    Ok(())
}

fn observe_directory(
    db: &Transaction<'_>,
    side: &Side,
    relative: &str,
    stamp: Stamp,
    cancel: &AtomicBool,
    progress: &mut SourceProgress,
) -> Result<(), String> {
    db.execute("DELETE FROM source_staged", [])
        .map_err(store::error)?;
    let mut insert = db
        .prepare_cached("INSERT OR IGNORE INTO source_staged VALUES(?1,?2,?3)")
        .map_err(store::error)?;
    let mut duplicate = false;
    let observed = side.enumerate(relative, &stamp, cancel, |name, mut result| {
        let child = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}\\{name}")
        };
        if let Ok(stamp) = &result {
            if stamp.kind() == Kind::File {
                if let Err(gap) = side.recheck_file(&child, stamp) {
                    result = Err(gap);
                }
            }
        }
        duplicate |= insert
            .execute(params![
                name.to_lowercase(),
                name,
                json(&Row::observe(child, result))?
            ])
            .map_err(store::error)?
            == 0;
        Ok(())
    })?;
    let observed = observed.and_then(|()| {
        if duplicate {
            Err(Gap::changed(
                "Directory enumeration returned a repeated name.",
            ))
        } else {
            Ok(stamp)
        }
    });
    let mut directory = Row::observe(relative.to_owned(), observed);
    if directory.gap.is_none() {
        let mut names = db
            .prepare_cached("SELECT name FROM source_staged ORDER BY name COLLATE BINARY")
            .map_err(store::error)?;
        directory.membership = Some(membership::from_ordered_rows(
            &mut names.query([]).map_err(store::error)?,
            cancel,
        )?);
    }
    directory.count(&mut progress.counts)?;
    put(db, &directory)?;
    if directory.gap.is_some() {
        return Ok(());
    }
    let mut query = db
        .prepare_cached(
            "SELECT entry.body, EXISTS(SELECT 1 FROM source_staged other
                WHERE other.folded=entry.folded AND other.name<>entry.name)
             FROM source_staged entry ORDER BY entry.folded,entry.name",
        )
        .map_err(store::error)?;
    let mut rows = query.query([]).map_err(store::error)?;
    while let Some(row) = rows.next().map_err(store::error)? {
        scan::check_cancel(cancel)?;
        let mut member: Row = serde_json::from_str(&row.get::<_, String>(0).map_err(store::error)?)
            .map_err(store::error)?;
        if row.get::<_, bool>(1).map_err(store::error)? {
            member.stamp = None;
            member.gap = Some(Gap::unsupported(
                "Observed names collide under the review identity rules.",
            ));
        }
        if let Some(stamp) = member
            .stamp
            .as_ref()
            .filter(|stamp| stamp.kind() == Kind::Directory)
        {
            queue(db, &member.relative, stamp)?;
            progress.queued_directories += 1;
        } else {
            member.count(&mut progress.counts)?;
            put(db, &member)?;
        }
    }
    Ok(())
}

fn recheck_directories(
    db: &Connection,
    side: &Side,
    checker: &mut Checker,
    cancel: &AtomicBool,
    counts: &mut SourceCounts,
) -> Result<(), String> {
    let mut query = db
        .prepare("SELECT body FROM source_members WHERE directory=1 ORDER BY relative")
        .map_err(store::error)?;
    let mut rows = query.query([]).map_err(store::error)?;
    let mut update = db
        .prepare("INSERT INTO source_changed VALUES(?1,0,?2)")
        .map_err(store::error)?;
    while let Some(row) = rows.next().map_err(store::error)? {
        scan::check_cancel(cancel)?;
        let original: Row = serde_json::from_str(&row.get::<_, String>(0).map_err(store::error)?)
            .map_err(store::error)?;
        let Some(stamp) = original
            .stamp
            .as_ref()
            .filter(|stamp| stamp.kind() == Kind::Directory)
        else {
            continue;
        };
        let witness = original
            .membership
            .as_ref()
            .ok_or("The observed directory membership is missing.")?;
        if let Some(gap) = checker.recheck(side, &original.relative, stamp, witness, cancel)? {
            let record = Row::observe(original.relative, Err(gap));
            update
                .execute(params![record.relative, json(&record)?])
                .map_err(store::error)?;
            counts.directories -= 1;
            counts.gaps += 1;
        }
    }
    drop(rows);
    db.execute(
        "INSERT OR REPLACE INTO source_members SELECT * FROM source_changed",
        [],
    )
    .map_err(store::error)?;
    Ok(())
}

pub(crate) fn freeze_source(
    path: &str,
    environment: SourceEnvironment<'_>,
    directory: &Path,
    cancel: &AtomicBool,
    mut report: impl FnMut(&SourceProgress),
) -> Result<SourceReceipt, String> {
    scan::check_cancel(cancel)?;
    let started_at = store::now();
    let (endpoint, side) = environment.open(path)?;
    let directory = windows::guard_outside(directory, &[&side])?;
    let scratch = scan::Scratch::create(&directory)?;
    let mut db = Connection::open(&scratch.path).map_err(store::error)?;
    db.execute_batch(
        "PRAGMA journal_mode=OFF;
         PRAGMA synchronous=OFF;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-8192;
         CREATE TABLE source_pending(id INTEGER PRIMARY KEY,relative TEXT UNIQUE,stamp TEXT);
         CREATE TABLE source_members(relative TEXT PRIMARY KEY,directory INTEGER,body TEXT);
         CREATE INDEX source_directory ON source_members(directory,relative);
         CREATE TABLE source_changed(relative TEXT PRIMARY KEY,directory INTEGER,body TEXT);
         CREATE TABLE source_staged(folded TEXT,name TEXT,body TEXT,PRIMARY KEY(folded,name));
         CREATE INDEX source_staged_exact ON source_staged(name);",
    )
    .map_err(store::error)?;
    queue(&db, "", &side.root_stamp())?;
    let mut progress = SourceProgress {
        phase: "observing",
        queued_directories: 1,
        ..SourceProgress::default()
    };
    report(&progress);
    while progress.queued_directories > 0 {
        scan::check_cancel(cancel)?;
        let tx = db.transaction().map_err(store::error)?;
        let (id, relative, raw): (i64, String, String) = tx
            .query_row(
                "SELECT id,relative,stamp FROM source_pending ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(store::error)?
            .ok_or("Source observation queue is incomplete.")?;
        tx.execute("DELETE FROM source_pending WHERE id=?1", [id])
            .map_err(store::error)?;
        progress.queued_directories -= 1;
        let stamp = serde_json::from_str(&raw).map_err(store::error)?;
        observe_directory(&tx, &side, &relative, stamp, cancel, &mut progress)?;
        tx.commit().map_err(store::error)?;
        report(&progress);
    }
    progress.phase = "checking-membership";
    report(&progress);
    let mut checker = Checker::new(&directory)?;
    recheck_directories(&db, &side, &mut checker, cancel, &mut progress.counts)?;
    if environment.open(&endpoint.root)?.0 != endpoint {
        return Err("The source volume or scope changed before freezing membership.".into());
    }
    let header = Header {
        format: FORMAT.into(),
        schema: SCHEMA,
        endpoint,
        started_at,
        finished_at: store::now(),
        counts: progress.counts.clone(),
    };
    let mut query = db
        .prepare("SELECT body FROM source_members ORDER BY relative")
        .map_err(store::error)?;
    let mut rows = query.query([]).map_err(store::error)?;
    progress.phase = "publishing";
    report(&progress);
    let digest = manifest::publish_lines(
        &directory,
        PREFIX,
        json(&header)?.as_bytes(),
        cancel,
        || {
            rows.next()
                .map_err(store::error)?
                .map(|row| row.get(0).map_err(store::error))
                .transpose()
        },
    )?;
    let receipt = header.receipt(digest);
    visit(&directory, &receipt, cancel, |_, _| Ok(()))?;
    progress.phase = "complete";
    progress.total_known = receipt.counts.gaps == 0;
    report(&progress);
    Ok(receipt)
}

pub(crate) fn create_source_evidence_directory(
    parent: &Path,
    name: &str,
    scope: &str,
    environment: SourceEnvironment<'_>,
) -> Result<PathBuf, String> {
    let (_, side) = environment.open(scope)?;
    windows::create_evidence_directory(parent, name, &[&side])
}

fn visit(
    directory: &windows::EvidenceDirectory,
    receipt: &SourceReceipt,
    cancel: &AtomicBool,
    mut member: impl FnMut(&Row, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut counts = SourceCounts::default();
    manifest::visit_lines(
        directory,
        PREFIX,
        &receipt.manifest_digest,
        |line| {
            let header: Header = serde_json::from_slice(line).map_err(store::error)?;
            if header.format != FORMAT
                || header.schema != SCHEMA
                || header.receipt(receipt.manifest_digest.clone()) != *receipt
            {
                return Err("The frozen source manifest does not match the saved receipt.".into());
            }
            Ok(())
        },
        |line| {
            scan::check_cancel(cancel)?;
            let row: Row = serde_json::from_slice(line).map_err(store::error)?;
            row.count(&mut counts)?;
            member(&row, line)
        },
    )?;
    if counts != receipt.counts {
        return Err("The frozen source manifest counts changed.".into());
    }
    Ok(())
}

fn under(relative: &str, scope: &str) -> bool {
    scope.is_empty()
        || relative == scope
        || relative
            .strip_prefix(scope)
            .is_some_and(|tail| tail.starts_with('\\'))
}

/// Check an exact frozen subtree, including the path from the bound root and
/// every observed descendant. The caller still fences the scan/revision and
/// live watcher around the final proposal or user decision.
pub(crate) fn revalidate_source_scope(
    directory: &Path,
    receipt: &SourceReceipt,
    environment: SourceEnvironment<'_>,
    relative: &str,
    cancel: &AtomicBool,
) -> Result<SourceCandidate, String> {
    scan::check_cancel(cancel)?;
    if !relative.is_empty() && !windows::relative_supported(relative) {
        return Err("Select a supported exact relative source path.".into());
    }
    let (endpoint, side) = environment.open(&receipt.endpoint.root)?;
    if endpoint != receipt.endpoint {
        return Err("The frozen source volume or scope has changed.".into());
    }
    let directory = windows::guard_outside(directory, &[&side])?;
    let mut checker = Checker::new(&directory)?;
    let mut digest = Sha256::new();
    digest.update(
        serde_json::to_vec(&(FORMAT, &receipt.manifest_digest, relative)).map_err(store::error)?,
    );
    let mut counts = SourceCounts::default();
    let mut kind = None;
    visit(&directory, receipt, cancel, |member, line| {
        let selected = under(&member.relative, relative);
        if !selected && !under(relative, &member.relative) {
            return Ok(());
        }
        let stamp = member.stamp.as_ref().ok_or_else(|| {
            let description = member
                .gap
                .as_ref()
                .map(|gap| gap.message.as_str())
                .unwrap_or("Coverage is unknown.");
            format!(
                "Source coverage is unresolved at {}. {description}",
                member.relative
            )
        })?;
        let gap = if stamp.kind() == Kind::Directory {
            let witness = member
                .membership
                .as_ref()
                .ok_or("The frozen directory membership is missing.")?;
            checker.recheck(&side, &member.relative, stamp, witness, cancel)?
        } else {
            side.recheck_file(&member.relative, stamp).err()
        };
        if let Some(gap) = gap {
            return Err(format!(
                "Source observation changed at {}. {}",
                member.relative, gap.message
            ));
        }
        if member.relative == relative {
            kind = Some(if stamp.kind() == Kind::File {
                "file"
            } else {
                "folder"
            });
        }
        if selected {
            member.count(&mut counts)?;
            digest.update(line);
        }
        Ok(())
    })?;
    let kind = kind.ok_or("The selected source path is absent from the frozen membership.")?;
    scan::check_cancel(cancel)?;
    if environment.open(&receipt.endpoint.root)?.0 != endpoint {
        return Err("The frozen source volume or scope changed during revalidation.".into());
    }
    Ok(SourceCandidate {
        relative: relative.into(),
        kind: kind.into(),
        fingerprint: format!("{:x}", digest.finalize()),
        files: counts.files,
        directories: counts.directories,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf, sync::atomic::Ordering};

    struct Fixture {
        source: PathBuf,
        evidence: PathBuf,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "migration-frozen-source-{}",
                manifest::nonce().unwrap()
            ));
            let fixture = Self {
                source: root.join("source"),
                evidence: root.join("evidence"),
                root,
            };
            fs::create_dir_all(&fixture.source).unwrap();
            fs::create_dir_all(&fixture.evidence).unwrap();
            fixture
        }
        fn env(&self) -> SourceEnvironment<'_> {
            SourceEnvironment::Fixture {
                source_root: &self.source,
            }
        }
        fn write(&self, relative: &str, contents: &[u8]) -> PathBuf {
            let path = self.source.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }
        fn freeze(&self) -> SourceReceipt {
            freeze_source(
                self.source.to_str().unwrap(),
                self.env(),
                &self.evidence,
                &AtomicBool::new(false),
                |_| {},
            )
            .unwrap()
        }
        fn candidate(
            &self,
            receipt: &SourceReceipt,
            relative: &str,
        ) -> Result<SourceCandidate, String> {
            revalidate_source_scope(
                &self.evidence,
                receipt,
                self.env(),
                relative,
                &AtomicBool::new(false),
            )
        }
    }

    #[test]
    fn frozen_metadata_has_exact_membership_without_a_mirror() {
        let fixture = Fixture::new();
        let path = fixture.write(r"Project\work.txt", b"keep");
        fixture.write(r"Other\note.txt", b"also keep");
        let scope = fixture.source.to_str().unwrap();
        assert!(create_source_evidence_directory(
            &fixture.source,
            "must-not-create",
            scope,
            fixture.env()
        )
        .is_err());
        assert!(!fixture.source.join("must-not-create").exists());
        assert_eq!(
            create_source_evidence_directory(&fixture.root, "evidence", scope, fixture.env())
                .unwrap(),
            fixture.evidence
        );
        let receipt = fixture.freeze();
        assert_eq!(
            (
                receipt.counts.files,
                receipt.counts.directories,
                receipt.counts.gaps
            ),
            (2, 3, 0)
        );
        assert!(!fixture.root.join("mirror").exists());
        let scope = fixture.candidate(&receipt, "Project").unwrap();
        assert_eq!(
            (scope.kind.as_str(), scope.files, scope.directories),
            ("folder", 1, 1)
        );
        assert_eq!(fixture.candidate(&receipt, "Project").unwrap(), scope);
        let leaf = fixture.candidate(&receipt, r"Project\work.txt").unwrap();
        assert_eq!(leaf.kind, "file");
        assert_ne!(leaf.fingerprint, scope.fingerprint);
        assert!(fixture
            .candidate(&receipt, r"Project\unseen.txt")
            .unwrap_err()
            .contains("absent"));
        assert!(fixture.candidate(&receipt, r"project\work.txt").is_err());

        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, b"edit").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert!(fixture
            .candidate(&receipt, r"Project\work.txt")
            .unwrap_err()
            .contains("changed"));
        fixture.write(r"Project\new.txt", b"new");
        assert!(fixture.candidate(&receipt, "Project").is_err());
        assert!(fixture.candidate(&receipt, r"Other\note.txt").is_ok());
    }

    #[test]
    fn source_coverage_gap_blocks_ancestor_and_keeps_known_sibling() {
        let fixture = Fixture::new();
        let unsupported = fixture.write(r"Gap\streams.txt", b"default stream");
        fs::write(
            format!("{}:extra", unsupported.display()),
            b"alternate stream",
        )
        .unwrap();
        fixture.write(r"Known\item.txt", b"eligible file");
        let receipt = fixture.freeze();
        assert_eq!(receipt.counts.gaps, 1);
        assert!(fixture
            .candidate(&receipt, "")
            .unwrap_err()
            .contains("unresolved"));
        assert!(fixture
            .candidate(&receipt, "Gap")
            .unwrap_err()
            .contains("unresolved"));
        assert_eq!(fixture.candidate(&receipt, "Known").unwrap().files, 1);
    }

    #[test]
    fn cancellation_and_invalid_sidecar_cannot_supply_a_frozen_candidate() {
        let fixture = Fixture::new();
        fixture.write(r"Project\work.txt", b"frozen metadata");
        let cancel = AtomicBool::new(false);
        let result = freeze_source(
            fixture.source.to_str().unwrap(),
            fixture.env(),
            &fixture.evidence,
            &cancel,
            |progress| {
                if progress.counts.directories > 0 {
                    cancel.store(true, Ordering::Release);
                }
            },
        );
        assert!(result.unwrap_err().contains("cancelled"));
        assert_eq!(fs::read_dir(&fixture.evidence).unwrap().count(), 0);
        let receipt = fixture.freeze();
        let sidecar = fixture
            .evidence
            .join(format!("source-{}.ndjson", receipt.manifest_digest));
        let mut bytes = fs::read(&sidecar).unwrap();
        assert_eq!(store::hash(&bytes), receipt.manifest_digest);
        bytes.pop();
        fs::write(sidecar, bytes).unwrap();
        assert!(fixture.candidate(&receipt, "Project").is_err());
    }
}
