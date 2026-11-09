use super::{
    manifest,
    membership::{self, Checker, Witness},
    windows::{EvidenceDirectory, Gap, Kind, Side, Stamp},
    Binding, Digest, Progress, Receipt, Record, Verdict,
};
use crate::store;
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
};
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

const WORKERS: usize = 4;
const FILE_BATCH: usize = WORKERS * 8;
const HASH_BUFFER: usize = 1024 * 1024;

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<(), String> {
    if cancel.load(Ordering::Acquire) {
        return Err("Mirror verification cancelled before publishing an evidence receipt.".into());
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
struct Member {
    relative: String,
    stamp: Stamp,
    membership: Option<Witness>,
}

#[derive(Deserialize, Serialize)]
struct Work {
    relative: String,
    source: Option<Member>,
    mirror: Option<Member>,
}

impl Work {
    fn result(&self, verdict: Verdict, directory: bool) -> Record {
        Record {
            relative: self.relative.clone(),
            source_relative: self.source.as_ref().map(|m| m.relative.clone()),
            mirror_relative: self.mirror.as_ref().map(|m| m.relative.clone()),
            verdict,
            directory,
            coverage_gap: false,
            source: None,
            mirror: None,
            source_stamp: self.source.as_ref().map(|m| m.stamp.clone()),
            mirror_stamp: self.mirror.as_ref().map(|m| m.stamp.clone()),
            source_membership: self.source.as_ref().and_then(|m| m.membership.clone()),
            mirror_membership: self.mirror.as_ref().and_then(|m| m.membership.clone()),
            problem: None,
            source_gap: None,
            mirror_gap: None,
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Observed {
    relative: String,
    stamp: Option<Stamp>,
    gap: Option<Gap>,
}

impl Observed {
    fn member(self) -> Result<Member, String> {
        Ok(Member {
            relative: self.relative,
            stamp: self.stamp.ok_or("Observed entry metadata missing")?,
            membership: None,
        })
    }
}

fn encode<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(store::error)
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(value).map_err(store::error)
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.into()
    } else {
        format!("{parent}\\{name}")
    }
}

fn set_gaps(record: &mut Record, source: Option<Gap>, mirror: Option<Gap>) {
    record.coverage_gap = true;
    let verdicts = [
        source.as_ref().map(|gap| gap.verdict),
        mirror.as_ref().map(|gap| gap.verdict),
    ];
    record.verdict = if verdicts.contains(&Some(Verdict::ChangedDuringRead)) {
        Verdict::ChangedDuringRead
    } else if verdicts.contains(&Some(Verdict::Unreadable)) {
        Verdict::Unreadable
    } else {
        Verdict::Unsupported
    };
    record.source_gap = source;
    record.mirror_gap = mirror;
}

fn store_result(db: &Connection, progress: &mut Progress, record: &Record) -> Result<(), String> {
    db.execute(
        "INSERT INTO entries(relative,body) VALUES(?1,?2)",
        params![record.relative, encode(record)?],
    )
    .map_err(store::error)?;
    progress.counts.record(record.verdict);
    if record.coverage_gap {
        progress.counts.coverage_gaps += 1;
    }
    for digest in [&record.source, &record.mirror].into_iter().flatten() {
        progress.checked_files += 1;
        progress.checked_bytes = progress
            .checked_bytes
            .checked_add(digest.bytes)
            .ok_or("The comparison exceeds the supported byte counter.")?;
    }
    Ok(())
}

fn save_work(
    db: &Connection,
    progress: &mut Progress,
    work: &Work,
    directory: bool,
) -> Result<(), String> {
    db.execute(
        "INSERT INTO queue(directory,body) VALUES(?1,?2)",
        params![directory, encode(work)?],
    )
    .map_err(store::error)?;
    if directory {
        progress.queued_directories += 1;
    } else {
        progress.queued_files += 1;
    }
    Ok(())
}

fn take_work(db: &Transaction<'_>, directory: bool, limit: usize) -> Result<Vec<Work>, String> {
    let mut query = db
        .prepare("SELECT id,body FROM queue WHERE directory=?1 ORDER BY id LIMIT ?2")
        .map_err(store::error)?;
    let mut rows = query
        .query(params![directory, limit as i64])
        .map_err(store::error)?;
    let mut work = Vec::new();
    while let Some(row) = rows.next().map_err(store::error)? {
        let id: i64 = row.get(0).map_err(store::error)?;
        let raw: String = row.get(1).map_err(store::error)?;
        work.push((id, decode(&raw)?));
    }
    drop(rows);
    work.into_iter()
        .map(|(id, work)| {
            db.execute("DELETE FROM queue WHERE id=?1", [id])
                .map_err(store::error)?;
            Ok(work)
        })
        .collect()
}

fn observe_side(
    db: &Transaction<'_>,
    side: &Side,
    member: Option<&Member>,
    cancel: &AtomicBool,
    role: i64,
) -> Result<Option<Gap>, String> {
    let Some(member) = member else {
        return Ok(None);
    };
    let mut insert = db
        .prepare_cached("INSERT OR IGNORE INTO seen(role,folded,name,body) VALUES(?1,?2,?3,?4)")
        .map_err(store::error)?;
    let mut duplicate = false;
    let result = side.enumerate(&member.relative, &member.stamp, cancel, |name, result| {
        let (stamp, gap) = match result {
            Ok(stamp) => (Some(stamp), None),
            Err(gap) => (None, Some(gap)),
        };
        let observed = Observed {
            relative: join(&member.relative, &name),
            stamp,
            gap,
        };
        duplicate |= insert
            .execute(params![role, name.to_lowercase(), name, encode(&observed)?])
            .map_err(store::error)?
            == 0;
        Ok(())
    })?;
    Ok(result.err().or_else(|| {
        duplicate.then(|| Gap::changed("Directory enumeration returned a repeated name."))
    }))
}

fn directory_witness(db: &Connection, role: i64, cancel: &AtomicBool) -> Result<Witness, String> {
    let mut names = db
        .prepare_cached("SELECT name FROM seen WHERE role=?1 ORDER BY name COLLATE BINARY")
        .map_err(store::error)?;
    let mut rows = names.query([role]).map_err(store::error)?;
    membership::from_ordered_rows(&mut rows, cancel)
}

fn record_ambiguity(
    db: &Transaction<'_>,
    progress: &mut Progress,
    directory: &Work,
    folded: &str,
) -> Result<(), String> {
    let mut query = db
        .prepare_cached("SELECT role,name,body FROM seen WHERE folded=?1 ORDER BY role,name")
        .map_err(store::error)?;
    let mut rows = query.query([folded]).map_err(store::error)?;
    while let Some(row) = rows.next().map_err(store::error)? {
        let role: i64 = row.get(0).map_err(store::error)?;
        let name: String = row.get(1).map_err(store::error)?;
        let observed: Observed = decode(&row.get::<_, String>(2).map_err(store::error)?)?;
        let work = Work {
            relative: join(&directory.relative, &name),
            source: None,
            mirror: None,
        };
        let mut result = work.result(Verdict::Unsupported, false);
        if role == 0 {
            result.source_relative = Some(observed.relative);
        } else {
            result.mirror_relative = Some(observed.relative);
        }
        result.coverage_gap = true;
        result.problem = Some(
            "This name collides under the review path rules. These entries were not paired.".into(),
        );
        store_result(db, progress, &result)?;
    }
    Ok(())
}

fn merge_members(
    db: &Transaction<'_>,
    directory: &Work,
    progress: &mut Progress,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut query = db
        .prepare_cached(
            "SELECT folded, count(CASE WHEN role=0 THEN 1 END), count(CASE WHEN role=1 THEN 1 END),
                max(CASE WHEN role=0 THEN name END), max(CASE WHEN role=1 THEN name END),
                max(CASE WHEN role=0 THEN body END), max(CASE WHEN role=1 THEN body END)
         FROM seen GROUP BY folded ORDER BY folded",
        )
        .map_err(store::error)?;
    let mut rows = query.query([]).map_err(store::error)?;
    while let Some(row) = rows.next().map_err(store::error)? {
        check_cancel(cancel)?;
        let source_count: i64 = row.get(1).map_err(store::error)?;
        let mirror_count: i64 = row.get(2).map_err(store::error)?;
        if source_count > 1 || mirror_count > 1 {
            let folded: String = row.get(0).map_err(store::error)?;
            record_ambiguity(db, progress, directory, &folded)?;
            continue;
        }
        let source_name: Option<String> = row.get(3).map_err(store::error)?;
        let mirror_name: Option<String> = row.get(4).map_err(store::error)?;
        let source = row
            .get::<_, Option<String>>(5)
            .map_err(store::error)?
            .as_deref()
            .map(decode::<Observed>)
            .transpose()?;
        let mirror = row
            .get::<_, Option<String>>(6)
            .map_err(store::error)?
            .as_deref()
            .map(decode::<Observed>)
            .transpose()?;
        let name = source_name
            .as_deref()
            .or(mirror_name.as_deref())
            .ok_or("Observed member name missing")?;
        let relative = join(&directory.relative, name);
        let directory_type = [&source, &mirror]
            .iter()
            .filter_map(|entry| entry.as_ref())
            .any(|entry| {
                entry
                    .stamp
                    .as_ref()
                    .is_some_and(|stamp| stamp.kind() == Kind::Directory)
            });
        let mut result = Work {
            relative: relative.clone(),
            source: None,
            mirror: None,
        }
        .result(Verdict::Unsupported, directory_type);
        result.source_relative = source.as_ref().map(|member| member.relative.clone());
        result.mirror_relative = mirror.as_ref().map(|member| member.relative.clone());
        let case_mismatch = source_name
            .as_ref()
            .zip(mirror_name.as_ref())
            .is_some_and(|(left, right)| left != right);
        if case_mismatch {
            result.coverage_gap = true;
            result.problem = Some("Relative components have different spelling. Exact source path preservation could not be established.".into());
            store_result(db, progress, &result)?;
        } else if [&source, &mirror]
            .iter()
            .filter_map(|entry| entry.as_ref())
            .any(|entry| entry.gap.is_some())
        {
            set_gaps(
                &mut result,
                source.and_then(|member| member.gap),
                mirror.and_then(|member| member.gap),
            );
            store_result(db, progress, &result)?;
        } else {
            let source = source.map(Observed::member).transpose()?;
            let mirror = mirror.map(Observed::member).transpose()?;
            if source
                .as_ref()
                .zip(mirror.as_ref())
                .is_some_and(|(left, right)| left.stamp.kind() != right.stamp.kind())
            {
                result.verdict = Verdict::Different;
                result.coverage_gap = true;
                result.problem = Some("The same relative name is a file on one side and a directory on the other. Its descendants remain uncovered.".into());
                store_result(db, progress, &result)?;
            } else {
                save_work(
                    db,
                    progress,
                    &Work {
                        relative,
                        source,
                        mirror,
                    },
                    directory_type,
                )?;
            }
        }
    }
    Ok(())
}

fn compare_directory(
    db: &Transaction<'_>,
    mut work: Work,
    source: &Side,
    mirror: &Side,
    cancel: &AtomicBool,
    progress: &mut Progress,
) -> Result<(), String> {
    db.execute("DELETE FROM seen", []).map_err(store::error)?;
    let source_gap = observe_side(db, source, work.source.as_ref(), cancel, 0)?;
    let mirror_gap = observe_side(db, mirror, work.mirror.as_ref(), cancel, 1)?;
    if source_gap.is_some() || mirror_gap.is_some() {
        // If either existing parent cannot be read, absence of its members
        // is unknown. Never turn the readable peer's entries into missing ones.
        let mut record = work.result(Verdict::Directory, true);
        set_gaps(&mut record, source_gap, mirror_gap);
        return store_result(db, progress, &record);
    }
    if let Some(member) = &mut work.source {
        member.membership = Some(directory_witness(db, 0, cancel)?);
    }
    if let Some(member) = &mut work.mirror {
        member.membership = Some(directory_witness(db, 1, cancel)?);
    }
    let verdict = match (&work.source, &work.mirror) {
        (Some(_), Some(_)) => Verdict::Directory,
        (Some(_), None) => Verdict::Missing,
        (None, Some(_)) => Verdict::Extra,
        (None, None) => return Err("Directory comparison lacks both sides.".into()),
    };
    merge_members(db, &work, progress, cancel)?;
    db.execute(
        "INSERT INTO checked_directories(body) VALUES(?1)",
        [encode(&work)?],
    )
    .map_err(store::error)?;
    store_result(db, progress, &work.result(verdict, true))
}

fn hash_member(
    side: &Side,
    member: Option<&Member>,
    cancel: &AtomicBool,
    buffer: &mut [u8],
) -> Result<(Option<Digest>, Option<Gap>), String> {
    let Some(member) = member else {
        return Ok((None, None));
    };
    match side.hash(&member.relative, &member.stamp, cancel, buffer)? {
        Ok(digest) => Ok((Some(digest), None)),
        Err(gap) => Ok((None, Some(gap))),
    }
}

fn compare_file(
    work: &Work,
    source: &Side,
    mirror: &Side,
    cancel: &AtomicBool,
    buffer: &mut [u8],
) -> Result<Record, String> {
    let (source_digest, source_gap) = hash_member(source, work.source.as_ref(), cancel, buffer)?;
    let (mirror_digest, mirror_gap) = hash_member(mirror, work.mirror.as_ref(), cancel, buffer)?;
    let mut record = work.result(Verdict::Different, false);
    record.source = source_digest;
    record.mirror = mirror_digest;
    if source_gap.is_some() || mirror_gap.is_some() {
        set_gaps(&mut record, source_gap, mirror_gap);
    } else {
        record.verdict = match (&record.source, &record.mirror) {
            (Some(left), Some(right)) if left == right => Verdict::Equal,
            (Some(_), Some(_)) => Verdict::Different,
            (Some(_), None) => Verdict::Missing,
            (None, Some(_)) => Verdict::Extra,
            (None, None) => return Err("File comparison lacks both sides.".into()),
        };
    }
    Ok(record)
}

struct HashPool {
    queue: mpsc::SyncSender<(usize, Work)>,
    results: mpsc::Receiver<(usize, Result<Record, String>)>,
}

impl HashPool {
    fn with<T>(
        source: &Side,
        mirror: &Side,
        cancel: &AtomicBool,
        run: impl FnOnce(&Self) -> Result<T, String>,
    ) -> Result<T, String> {
        std::thread::scope(|scope| {
            let (send_work, pending) = mpsc::sync_channel::<(usize, Work)>(FILE_BATCH);
            let (send_result, results) = mpsc::channel();
            let pending = Arc::new(Mutex::new(pending));
            let mut workers = Vec::with_capacity(WORKERS);
            for _ in 0..WORKERS {
                let pending = pending.clone();
                let send_result = send_result.clone();
                workers.push(scope.spawn(move || {
                    let mut buffer = vec![0; HASH_BUFFER];
                    loop {
                        let Ok(job) = pending.lock().map(|queue| queue.recv()) else {
                            return;
                        };
                        let Ok((position, work)) = job else { return };
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            compare_file(&work, source, mirror, cancel, &mut buffer)
                        }))
                        .unwrap_or_else(|_| {
                            Err("A mirror checksum worker stopped unexpectedly.".into())
                        });
                        if send_result.send((position, result)).is_err() {
                            return;
                        }
                    }
                }));
            }
            drop(send_result);
            drop(pending);
            let pool = Self {
                queue: send_work,
                results,
            };
            let result = run(&pool);
            drop(pool);
            for worker in workers {
                worker
                    .join()
                    .map_err(|_| "A mirror checksum worker stopped unexpectedly.")?;
            }
            result
        })
    }

    fn compare(&self, batch: Vec<Work>) -> Result<Vec<Record>, String> {
        let length = batch.len();
        for (position, work) in batch.into_iter().enumerate() {
            self.queue
                .send((position, work))
                .map_err(|_| "Mirror checksum workers stopped before the batch was accepted.")?;
        }
        let mut completed = Vec::with_capacity(length);
        for _ in 0..length {
            let (position, record) = self
                .results
                .recv()
                .map_err(|_| "Mirror checksum workers stopped before the batch finished.")?;
            completed.push((position, record?));
        }
        completed.sort_unstable_by_key(|(position, _)| *position);
        Ok(completed.into_iter().map(|(_, record)| record).collect())
    }
}

fn recheck_directories(
    db: &Connection,
    source: &Side,
    mirror: &Side,
    checker: &mut Checker,
    cancel: &AtomicBool,
    progress: &mut Progress,
) -> Result<(), String> {
    let mut query = db
        .prepare("SELECT body FROM checked_directories ORDER BY id")
        .map_err(store::error)?;
    let mut rows = query.query([]).map_err(store::error)?;
    while let Some(row) = rows.next().map_err(store::error)? {
        check_cancel(cancel)?;
        let work: Work = decode(&row.get::<_, String>(0).map_err(store::error)?)?;
        let source_gap = recheck_member(checker, source, work.source.as_ref(), cancel)?;
        let mirror_gap = recheck_member(checker, mirror, work.mirror.as_ref(), cancel)?;
        if source_gap.is_some() || mirror_gap.is_some() {
            let mut record = work.result(Verdict::ChangedDuringRead, true);
            set_gaps(&mut record, source_gap, mirror_gap);
            store_result(db, progress, &record)?;
        }
    }
    Ok(())
}

fn recheck_member(
    checker: &mut Checker,
    side: &Side,
    member: Option<&Member>,
    cancel: &AtomicBool,
) -> Result<Option<Gap>, String> {
    let Some(member) = member else {
        return Ok(None);
    };
    let witness = member
        .membership
        .as_ref()
        .ok_or("Observed directory membership missing")?;
    checker.recheck(side, &member.relative, &member.stamp, witness, cancel)
}

pub(super) struct Scratch {
    pub(super) path: PathBuf,
    guard: Option<File>,
}

impl Scratch {
    pub(super) fn create(directory: &EvidenceDirectory) -> Result<Self, String> {
        let file = directory
            .path
            .join(format!(".mirror-work-{}.sqlite", manifest::nonce()?));
        let guard = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&file)
            .map_err(store::error)?;
        Ok(Self {
            path: file,
            guard: Some(guard),
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        drop(self.guard.take());
        let _ = fs::remove_file(&self.path);
    }
}

pub(super) fn run(
    binding: &Binding,
    source: &Side,
    mirror: &Side,
    directory: &EvidenceDirectory,
    cancel: &AtomicBool,
    started_at: String,
    mut report: impl FnMut(&Progress),
    rebind: impl FnOnce() -> Result<(), String>,
) -> Result<Receipt, String> {
    check_cancel(cancel)?;
    let scratch = Scratch::create(directory)?;
    let mut db = Connection::open(&scratch.path).map_err(store::error)?;
    // Scratch state is disposable and creates no recovery sidecars. The
    // indexed traversals do not need large SQLite temporary sorts.
    db.execute_batch(
        "PRAGMA journal_mode=OFF;
         PRAGMA synchronous=OFF;
         PRAGMA temp_store=MEMORY;
         PRAGMA cache_size=-8192;
         CREATE TABLE queue(id INTEGER PRIMARY KEY,directory INTEGER,body TEXT);
         CREATE INDEX queue_type ON queue(directory,id);
         CREATE TABLE entries(id INTEGER PRIMARY KEY,relative TEXT,body TEXT);
         CREATE INDEX entry_order ON entries(relative,id);
         CREATE TABLE checked_directories(id INTEGER PRIMARY KEY,body TEXT);
         CREATE TABLE seen(role INTEGER,folded TEXT,name TEXT,body TEXT,PRIMARY KEY(folded,role,name));
         CREATE INDEX seen_exact ON seen(role,name);",
    ).map_err(store::error)?;
    let mut progress = Progress {
        phase: "comparing",
        ..Progress::default()
    };
    let root = Work {
        relative: String::new(),
        source: Some(Member {
            relative: String::new(),
            stamp: source.root_stamp(),
            membership: None,
        }),
        mirror: Some(Member {
            relative: String::new(),
            stamp: mirror.root_stamp(),
            membership: None,
        }),
    };
    save_work(&db, &mut progress, &root, true)?;
    report(&progress);
    HashPool::with(source, mirror, cancel, |pool| {
        while progress.queued_directories > 0 || progress.queued_files > 0 {
            check_cancel(cancel)?;
            if progress.queued_directories > 0 {
                let tx = db.transaction().map_err(store::error)?;
                let work = take_work(&tx, true, 1)?
                    .pop()
                    .ok_or("Missing queued directory")?;
                progress.queued_directories -= 1;
                compare_directory(&tx, work, source, mirror, cancel, &mut progress)?;
                tx.commit().map_err(store::error)?;
                report(&progress);
            }
            if progress.queued_files > 0 {
                let tx = db.transaction().map_err(store::error)?;
                let batch = take_work(&tx, false, FILE_BATCH)?;
                tx.commit().map_err(store::error)?;
                if batch.is_empty() {
                    return Err("Missing queued files".into());
                }
                let records = pool.compare(batch)?;
                let tx = db.transaction().map_err(store::error)?;
                for record in &records {
                    store_result(&tx, &mut progress, record)?;
                }
                tx.commit().map_err(store::error)?;
                progress.queued_files -= records.len() as u64;
                report(&progress);
            }
        }
        Ok(())
    })?;
    progress.phase = "checking-membership";
    report(&progress);
    let mut checker = Checker::new(directory)?;
    recheck_directories(&db, source, mirror, &mut checker, cancel, &mut progress)?;
    check_cancel(cancel)?;
    rebind()?;
    progress.phase = "publishing";
    report(&progress);
    let mut query = db
        .prepare("SELECT body FROM entries ORDER BY relative COLLATE BINARY,id")
        .map_err(store::error)?;
    let mut rows = query.query([]).map_err(store::error)?;
    let receipt = manifest::publish(directory, binding, &progress, started_at, cancel, || {
        rows.next()
            .map_err(store::error)?
            .map(|row| row.get(0).map_err(store::error))
            .transpose()
    })?;
    progress.phase = "complete";
    progress.total_known = receipt.coverage_complete;
    report(&progress);
    Ok(receipt)
}
