//! Explicit, no-replace copying of the unnamed data stream from a decision snapshot.
use crate::{
    filesystem::{self, Volume},
    store::{error, hash, now},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileBasicInfo, FileDispositionInfo, FileIdInfo, FileRenameInfo, FileStreamInfo,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, SetFileInformationByHandle, DELETE, FILE_BASIC_INFO,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_NAME_OPENED, FILE_READ_ATTRIBUTES, FILE_RENAME_INFO,
    FILE_SHARE_READ, FILE_SHARE_WRITE, VOLUME_NAME_GUID,
};

const PAGE: usize = 200;
const ISSUE_LIMIT: usize = 24;
const BUFFER_BYTES: usize = 1024 * 1024;
const PAUSED: &str = "Copy paused";
const SCOPE: &str = "Copies eligible regular files and empty folders. Each new file's unnamed data stream is checked with SHA-256. Files with additional streams, links, offline data, and app artifacts are reported as gaps. ACLs, application consistency, and restore usability are outside this copy check.";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Decision {
    #[serde(skip)]
    key: String,
    path: String,
    status: String,
    decided_at: Option<String>,
}

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    phase: String,
    job_id: Option<String>,
    mode: Option<String>,
    scope: Option<String>,
    source_revision: Option<u64>,
    planned: u64,
    copied: u64,
    skipped: u64,
    failed: u64,
    bytes_copied: u64,
    current_path: Option<String>,
    problem: Option<String>,
    issues: Vec<Value>,
}

pub struct CopyQueue {
    database: PathBuf,
    exclusions: Vec<String>,
    fixture: bool,
    progress: Mutex<Progress>,
    destination_gate: Mutex<()>,
    cancel: AtomicBool,
    worker_handle: Mutex<Option<thread::JoinHandle<()>>>,
    #[cfg(test)]
    worker_barrier: Mutex<Option<Arc<std::sync::Barrier>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ConflictPlan {
    pub(crate) source: String,
    pub(crate) original_relative: String,
    pub(crate) separate_relative: String,
    pub(crate) name: String,
    pub(crate) fingerprint: String,
}

struct ReviewedConflict {
    plan: ConflictPlan,
    _source_guards: Vec<File>,
    _destination_guards: Vec<File>,
    _original: File,
}

enum WorkPlan {
    Ordinary { mode: Mode, scope: Option<String> },
    Separate(ConflictPlan),
}

#[derive(Clone, Copy)]
enum Mode {
    Ordered,
    All,
    LastMoment,
}

impl Mode {
    fn parse(mode: &str) -> Result<Self, String> {
        match mode {
            "ordered" => Ok(Self::Ordered),
            "all" => Ok(Self::All),
            "last-moment" => Ok(Self::LastMoment),
            _ => Err("Unknown copy mode".into()),
        }
    }

    fn status(self) -> &'static str {
        match self {
            Self::Ordered | Self::All => "migrate-now",
            Self::LastMoment => "last-moment",
        }
    }
}

impl CopyQueue {
    pub fn new(database: PathBuf, exclusions: Vec<String>, fixture: bool) -> Arc<Self> {
        Arc::new(Self {
            database,
            exclusions,
            fixture,
            progress: Mutex::new(Progress {
                phase: "idle".into(),
                ..Progress::default()
            }),
            destination_gate: Mutex::new(()),
            cancel: AtomicBool::new(false),
            worker_handle: Mutex::new(None),
            #[cfg(test)]
            worker_barrier: Mutex::new(None),
        })
    }

    fn connect(&self) -> Result<Connection, String> {
        let db = Connection::open(&self.database).map_err(error)?;
        db.busy_timeout(Duration::from_secs(5)).map_err(error)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS receipts(
                destination TEXT, source TEXT, target TEXT, source_stamp TEXT,
                target_stamp TEXT, digest TEXT,
                PRIMARY KEY(destination,source,target));
             CREATE TABLE IF NOT EXISTS passes(
                destination TEXT, decision TEXT, decided_at TEXT, completed_at TEXT,
                PRIMARY KEY(destination,decision,decided_at));",
        )
        .map_err(error)?;
        Ok(db)
    }

    pub fn snapshot(
        &self,
        document: &Value,
        volumes: &[Volume],
        destination: &Volume,
    ) -> Result<Value, String> {
        let marks = decisions(document, volumes);
        let db = if self.database.is_file() {
            Some(self.connect()?)
        } else {
            None
        };
        let mut migrate_now = Vec::new();
        let mut last_moment = Vec::new();
        let mut migrate_now_count = 0usize;
        let mut last_moment_count = 0usize;
        let mut pending_migrate_now = 0usize;
        let mut signature = Sha256::new();
        for mark in marks
            .iter()
            .filter(|m| matches!(m.status.as_str(), "migrate-now" | "last-moment"))
        {
            let completed = match &db {
                Some(db) => db.query_row(
                    "SELECT completed_at FROM passes WHERE destination=?1 AND decision=?2 AND decided_at=?3",
                    params![destination.id, mark.key, pass_signature(mark, &marks)?],
                    |row| row.get::<_, String>(0),
                ).optional().map_err(error)?,
                None => None,
            };
            let mut item = serde_json::to_value(mark).map_err(error)?;
            item["state"] = json!(if completed.is_some() {
                "previously-checked"
            } else {
                "pending"
            });
            item["completedAt"] = json!(completed);
            if mark.status == "migrate-now" {
                migrate_now_count += 1;
                pending_migrate_now += usize::from(completed.is_none());
                if migrate_now.len() < PAGE {
                    migrate_now.push(item);
                }
            } else {
                last_moment_count += 1;
                if last_moment.len() < PAGE {
                    last_moment.push(item);
                }
            }
        }
        // Child overrides also change the scope of an existing parent mark.
        for mark in &marks {
            signature.update(serde_json::to_vec(mark).map_err(error)?);
        }
        signature.update(destination.id.as_bytes());
        Ok(json!({
            "destination": destination, "migrateNow": migrate_now, "lastMoment": last_moment,
            "migrateNowCount": migrate_now_count, "lastMomentCount": last_moment_count,
            "pendingMigrateNow": pending_migrate_now, "hasPendingMigrateNow": pending_migrate_now > 0,
            "queueSignature": format!("{:x}", signature.finalize()),
            "status": self.status()?,
        }))
    }

    pub fn status(&self) -> Result<Value, String> {
        let progress = self.progress.lock().map_err(error)?;
        let mut value = serde_json::to_value(&*progress).map_err(error)?;
        value["running"] = json!(active(&progress.phase));
        value["pauseRequested"] = json!(self.cancel.load(Ordering::SeqCst));
        value["scopeNotes"] = json!(SCOPE);
        Ok(value)
    }

    pub fn pause(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn join_stopped(&self) -> Result<(), String> {
        let mut worker = self.worker_handle.lock().map_err(error)?;
        if self.status()?["running"] == true {
            return Err("Copy coordinator is still stopping".into());
        }
        if let Some(handle) = worker.take() {
            handle
                .join()
                .map_err(|_| "Copy coordinator panicked during shutdown".to_owned())?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn hold_next_worker(&self, barrier: Arc<std::sync::Barrier>) {
        *self.worker_barrier.lock().unwrap() = Some(barrier);
    }

    pub fn pause_job(&self, expected_job_id: &str) -> Result<Value, String> {
        let progress = self.progress.lock().map_err(error)?;
        if progress.job_id.as_deref() != Some(expected_job_id) {
            return Err("The copy job changed. Inspect the current job before pausing.".into());
        }
        if !active(&progress.phase) && progress.phase != "paused" {
            return Err("This copy job has already stopped.".into());
        }
        self.cancel.store(true, Ordering::SeqCst);
        drop(progress);
        self.status()
    }

    pub fn validate_start_request(
        &self,
        volumes: &[Volume],
        destination: &Volume,
        mode: &str,
        scope: Option<&str>,
    ) -> Result<Option<String>, String> {
        let parsed_mode = Mode::parse(mode)?;
        validate_destination(destination, volumes, self.fixture)?;
        let scope = match (parsed_mode, scope) {
            (Mode::LastMoment, Some(path)) => {
                let path = filesystem::normalize(path)?;
                filesystem::identify(&path, volumes)?;
                Some(path)
            }
            (Mode::LastMoment, None) => return Err("Choose a Last Moment path explicitly".into()),
            (_, Some(_)) => return Err("This copy action uses the full Migrate now queue".into()),
            (_, None) => None,
        };
        if self.status()?["running"] == true {
            return Err("A copy pass is already running".into());
        }
        Ok(scope)
    }

    pub fn start(
        self: &Arc<Self>,
        document: Value,
        volumes: Vec<Volume>,
        destination: Volume,
        mode: &str,
        scope: Option<&str>,
    ) -> Result<Value, String> {
        let job_id = hash(format!("{}|{}|{mode}", std::process::id(), now()).as_bytes());
        self.start_with_job(document, volumes, destination, mode, scope, &job_id, None)
    }

    pub fn start_with_job(
        self: &Arc<Self>,
        document: Value,
        volumes: Vec<Volume>,
        destination: Volume,
        mode: &str,
        scope: Option<&str>,
        job_id: &str,
        launch: Option<mpsc::Receiver<()>>,
    ) -> Result<Value, String> {
        let parsed_mode = Mode::parse(mode)?;
        let scope = self.validate_start_request(&volumes, &destination, mode, scope)?;
        self.start_work(document, volumes, destination, mode, scope.clone(), job_id, launch,
            WorkPlan::Ordinary { mode: parsed_mode, scope })
    }

    pub(crate) fn preview_conflict(&self, document: &Value, volumes: &[Volume], destination: &Volume,
        source: &str, name: &str) -> Result<ConflictPlan, String> {
        self.review_conflict(document, volumes, destination, source, name).map(|review| review.plan)
    }

    pub(crate) fn start_conflict_job(self: &Arc<Self>, document: Value, volumes: Vec<Volume>,
        destination: Volume, job_id: &str, launch: mpsc::Receiver<()>, plan: ConflictPlan) -> Result<Value, String> {
        self.start_work(document, volumes, destination, "separate-file", Some(plan.source.clone()),
            job_id, Some(launch), WorkPlan::Separate(plan))
    }

    fn start_work(self: &Arc<Self>, document: Value, volumes: Vec<Volume>, destination: Volume,
        mode: &str, scope: Option<String>, job_id: &str, launch: Option<mpsc::Receiver<()>>,
        plan: WorkPlan) -> Result<Value, String> {
        let mut coordinator = self.worker_handle.lock().map_err(error)?;
        if self.status()?["running"] == true {
            return Err("A copy pass is already running".into());
        }
        if let Some(handle) = coordinator.take() {
            handle
                .join()
                .map_err(|_| "Previous copy coordinator panicked".to_owned())?;
        }
        {
            let mut progress = self.progress.lock().map_err(error)?;
            if active(&progress.phase) {
                return Err("A copy pass is already running".into());
            }
            self.cancel.store(false, Ordering::SeqCst);
            *progress = Progress {
                phase: "planning".into(),
                job_id: Some(job_id.into()),
                mode: Some(mode.into()),
                scope: scope.clone(),
                source_revision: document["revision"].as_u64(),
                ..Progress::default()
            };
        }
        let queue = self.clone();
        #[cfg(test)]
        let barrier = self.worker_barrier.lock().map_err(error)?.take();
        let spawned =
            thread::Builder::new()
                .name("migration-copy".into())
                .spawn(move || {
                    // Shared API admission is journaled while this worker waits.
                    // A lost/failed acknowledgement closes the gate without I/O.
                    if let Some(launch) = launch {
                        if launch.recv().is_err() {
                            queue.finish(Err("Copy launch acknowledgement was not saved; file copying was not started".into()));
                            return;
                        }
                    }
                    #[cfg(test)]
                    if let Some(barrier) = barrier {
                        barrier.wait();
                    }
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        match plan {
                            WorkPlan::Ordinary { mode, scope } =>
                                queue.run(&document, &volumes, &destination, mode, scope.as_deref()),
                            WorkPlan::Separate(plan) =>
                                queue.run_conflict(&document, &volumes, &destination, &plan),
                        }
                    }))
                    .unwrap_or_else(|_| Err("The copy worker stopped unexpectedly".into()));
                    queue.finish(result);
                });
        match spawned {
            Ok(handle) => *coordinator = Some(handle),
            Err(problem) => {
                self.finish(Err(error(problem)));
                return Err("Could not start the copy worker".into());
            }
        }
        self.status()
    }

    fn review_conflict(&self, document: &Value, volumes: &[Volume], destination: &Volume,
        source: &str, name: &str) -> Result<ReviewedConflict, String> {
        validate_destination(destination, volumes, self.fixture)?;
        let source = filesystem::normalize(source)?;
        if excluded(&source, &self.exclusions) {
            return Err("This file is excluded from migration copying.".into());
        }
        let (volume, original_relative, _) = filesystem::identify(&source, volumes)?;
        validate_relative(&original_relative)?;
        if name.is_empty() || name.contains(['\\','/']) {
            return Err("Enter a new filename in the same destination directory.".into());
        }
        validate_relative(name)?;
        let marks = decisions(document, volumes);
        let by_key = marks.iter().enumerate().map(|(index, mark)| (mark.key.as_str(),index)).collect();
        if nearest(&source, volumes, &by_key).is_none_or(|rank|
            !matches!(marks[rank].status.as_str(), "migrate-now" | "last-moment"))
        {
            return Err("This source file is not marked for migration.".into());
        }
        let (checked, source_guards) = guard_source(&source, volume)?;
        let input = open_read(&checked).map_err(error)?;
        let metadata = regular_file(&input)?;
        no_named_streams(&input)?;
        let source_stamp = stamp(&input, &metadata).ok_or("Could not establish the source identity")?;
        let original = Path::new(&original_relative);
        let parent = original.parent().ok_or("Missing relative parent")?;
        let original_name = original.file_name().ok_or("Choose a regular file")?;
        if original_name.to_string_lossy().to_lowercase() == name.to_lowercase() {
            return Err("The alternate filename must differ from the existing file.".into());
        }
        let (target_dir, destination_guards) = destination_directory_with_policy(
            destination, &parent.to_string_lossy(), self.fixture, false)?;
        let existing = open_read(&target_dir.join(original_name))
            .map_err(|e| format!("The original destination file cannot be reviewed: {e}"))?;
        let existing_meta = regular_file(&existing)?;
        no_named_streams(&existing)?;
        if same_file(&input, &existing) {
            return Err("Source and destination resolve to the same file.".into());
        }
        let existing_stamp = stamp(&existing, &existing_meta).ok_or("Could not establish the existing destination identity")?;
        match fs::symlink_metadata(target_dir.join(name)) {
            Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => {}
            _ => return Err("The alternate filename is already present or unavailable; it was left untouched.".into()),
        }
        let separate_relative = parent.join(name).to_string_lossy().into_owned();
        let fingerprint = hash(serde_json::to_vec(&json!([source,destination.id,original_relative,
            separate_relative,source_stamp,existing_stamp])).map_err(error)?.as_slice());
        Ok(ReviewedConflict { plan:ConflictPlan {source, original_relative, separate_relative,
            name:name.to_owned(),fingerprint}, _source_guards:source_guards,
            _destination_guards:destination_guards, _original:existing })
    }

    fn run_conflict(&self, document: &Value, volumes: &[Volume], destination: &Volume,
        expected: &ConflictPlan) -> Result<(), String> {
        self.check_cancel()?;
        let reviewed = self.review_conflict(document, volumes, destination, &expected.source, &expected.name)?;
        if reviewed.plan.fingerprint != expected.fingerprint {
            return Err("The reviewed source or collision changed before copying.".into());
        }
        {
            let mut progress = self.progress.lock().map_err(error)?;
            progress.phase = "copying".into();
            progress.planned = 1;
            progress.current_path = Some(expected.source.clone());
        }
        let db = self.connect()?;
        let job = Job { rank:0, source:expected.source.clone(), relative:expected.separate_relative.clone(),
            target:expected.separate_relative.to_uppercase(), directory:false };
        match self.copy_one_with_policy(&db, &job, volumes, destination, true)? {
            Outcome::Published(bytes) => {
                let mut progress = self.progress.lock().map_err(error)?;
                progress.copied = 1;
                progress.bytes_copied = bytes;
            }
            _ => return Err("The reviewed alternate file was not published.".into()),
        }
        drop(reviewed);
        Ok(())
    }

    fn finish(&self, result: Result<(), String>) {
        if let Ok(mut state) = self.progress.lock() {
            state.current_path = None;
            match result {
                Ok(()) if self.cancel.load(Ordering::SeqCst) => state.phase = "paused".into(),
                Ok(()) if state.failed == 0 => state.phase = "complete".into(),
                Ok(()) => state.phase = "partial".into(),
                Err(problem) if problem == PAUSED => state.phase = "paused".into(),
                Err(problem) => {
                    state.phase = "failed".into();
                    state.problem = Some(problem);
                }
            }
        }
    }

    fn issue(&self, path: &str, message: impl Into<String>) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.failed += 1;
            if progress.issues.len() < ISSUE_LIMIT {
                progress
                    .issues
                    .push(json!({"path":path,"message":message.into()}));
            }
        }
    }

    fn run(
        &self,
        document: &Value,
        volumes: &[Volume],
        destination: &Volume,
        mode: Mode,
        scope: Option<&str>,
    ) -> Result<(), String> {
        let _fixture_guards = if self.fixture {
            let (_, mut guards) = destination_directory(destination, "", true)?;
            for source in volumes {
                guards.extend(hold_directories(Path::new(&source.root), false)?);
            }
            guards
        } else {
            Vec::new()
        };
        let marks = decisions(document, volumes);
        let by_key: std::collections::HashMap<&str, usize> = marks
            .iter()
            .enumerate()
            .map(|(index, mark)| (mark.key.as_str(), index))
            .collect();
        let eligible = marks.iter().filter(|m| {
            m.status == mode.status() && scope.is_none_or(|path| filesystem::under(&m.path, path))
        });
        let mut roots = Vec::<String>::new();
        for mark in eligible.clone() {
            if roots.iter().any(|root| filesystem::under(&mark.path, root)) {
                continue;
            }
            roots.retain(|root| !filesystem::under(root, &mark.path));
            roots.push(mark.path.clone());
        }
        if let Some(path) = scope {
            if nearest(path, volumes, &by_key)
                .is_some_and(|rank| marks[rank].status == mode.status())
            {
                roots.clear();
                roots.push(path.into());
            }
        }
        if roots.is_empty() {
            return Err(format!(
                "No {} choices are eligible in this scope",
                mode.status()
            ));
        }
        let mut db = self.connect()?;
        db.execute_batch(
            "CREATE TEMP TABLE pending(source TEXT PRIMARY KEY, done INTEGER DEFAULT 0);
             CREATE INDEX temp.pending_open ON pending(done,source);
             CREATE TEMP TABLE plan(
                source TEXT PRIMARY KEY, relative TEXT, target TEXT, rank INTEGER, directory INTEGER);
             CREATE INDEX temp.plan_target ON plan(target);
             CREATE INDEX temp.plan_order ON plan(rank,target,source);
             CREATE TEMP TABLE blocked(target TEXT PRIMARY KEY);",
        ).map_err(error)?;
        for root in &roots {
            db.execute("INSERT INTO pending(source) VALUES(?1)", [root])
                .map_err(error)?;
        }
        while let Some(path) = db
            .query_row(
                "SELECT source FROM pending WHERE done=0 ORDER BY source LIMIT 1",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(error)?
        {
            self.check_cancel()?;
            let tx = db.transaction().map_err(error)?;
            tx.execute("UPDATE pending SET done=1 WHERE source=?1", [&path])
                .map_err(error)?;
            if let Err(problem) = self.plan_path(&tx, &path, volumes, &marks, &by_key, mode) {
                if problem == PAUSED {
                    return Err(problem);
                }
                self.issue(&path, problem);
            }
            tx.commit().map_err(error)?;
        }
        block_conflicts(&db)?;
        let planned = db
            .query_row("SELECT count(*) FROM plan WHERE directory=0", [], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(error)?;
        {
            let mut progress = self.progress.lock().map_err(error)?;
            progress.phase = "copying".into();
            progress.planned = planned as u64;
        }
        if matches!(mode, Mode::All) {
            self.copy_parallel(&db, volumes, destination)?;
        } else {
            self.each_job(&db, |job| {
                self.copy_job(&db, &job, volumes, destination);
                Ok(())
            })?;
        }
        self.check_cancel()?;
        if self.progress.lock().map_err(error)?.failed == 0 {
            let at = now();
            for mark in eligible {
                db.execute(
                    "INSERT OR REPLACE INTO passes VALUES(?1,?2,?3,?4)",
                    params![destination.id, mark.key, pass_signature(mark, &marks)?, at],
                )
                .map_err(error)?;
            }
        }
        Ok(())
    }

    fn each_job(
        &self,
        db: &Connection,
        mut consume: impl FnMut(Job) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut cursor = (-1i64, String::new(), String::new());
        loop {
            self.check_cancel()?;
            let batch = next_jobs(db, &cursor, 32)?;
            let Some(last) = batch.last() else {
                return Ok(());
            };
            cursor = (last.rank, last.target.clone(), last.source.clone());
            for job in batch {
                self.check_cancel()?;
                if is_blocked(db, &job.target)? {
                    self.issue(&job.source, "Multiple selected source paths need the same destination. Resolve the collision before copying.");
                } else {
                    consume(job)?;
                }
            }
        }
    }

    fn copy_parallel(
        &self,
        db: &Connection,
        volumes: &[Volume],
        destination: &Volume,
    ) -> Result<(), String> {
        const WORKERS: usize = 3;
        let connections = (0..WORKERS)
            .map(|_| self.connect())
            .collect::<Result<Vec<_>, _>>()?;
        let (send, receive) = mpsc::sync_channel::<Job>(WORKERS * 2);
        let receive = Arc::new(Mutex::new(receive));
        thread::scope(|scope| {
            for connection in connections {
                let receive = receive.clone();
                scope.spawn(move || loop {
                    let next = {
                        let Ok(receive) = receive.lock() else { return };
                        receive.recv()
                    };
                    let Ok(job) = next else { return };
                    self.copy_job(&connection, &job, volumes, destination);
                });
            }
            drop(receive);
            let result = self.each_job(db, |job| send.send(job).map_err(error));
            if result.is_err() {
                self.cancel.store(true, Ordering::SeqCst);
            }
            drop(send);
            // Scoped workers join on both success and error before a new run is allowed.
            result
        })
    }

    fn plan_path(
        &self,
        db: &Connection,
        path: &str,
        volumes: &[Volume],
        marks: &[Decision],
        by_key: &std::collections::HashMap<&str, usize>,
        mode: Mode,
    ) -> Result<(), String> {
        let rank = nearest(path, volumes, by_key)
            .filter(|&rank| marks[rank].status == mode.status())
            .ok_or("This path no longer has an eligible decision in the captured scope")?;
        if excluded(path, &self.exclusions) {
            return Err("App artifacts are outside the copy scope".into());
        }
        let (volume, relative, _) = filesystem::identify(path, volumes)?;
        validate_relative(&relative)?;
        let (guarded, guards) = guard_source(path, volume)?;
        let metadata = guards
            .last()
            .ok_or("Missing source identity handle")?
            .metadata()
            .map_err(error)?;
        eligible_metadata(&metadata)?;
        if metadata.is_dir() {
            for entry in fs::read_dir(&guarded).map_err(error)? {
                self.check_cancel()?;
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(problem) => {
                        self.issue(path, error(problem));
                        continue;
                    }
                };
                let Some(name) = entry.file_name().into_string().ok() else {
                    self.issue(
                        path,
                        "A child name cannot be represented safely on the destination",
                    );
                    continue;
                };
                let child = guarded.join(name).to_string_lossy().into_owned();
                if nearest(&child, volumes, by_key)
                    .is_some_and(|rank| marks[rank].status == mode.status())
                {
                    db.execute("INSERT OR IGNORE INTO pending(source) VALUES(?1)", [&child])
                        .map_err(error)?;
                } else {
                    // Reach explicit eligible roots beyond an intervening override
                    // without enumerating or copying the excluded branch.
                    for mark in marks.iter().filter(|m| {
                        m.status == mode.status()
                            && m.path != child
                            && filesystem::under(&m.path, &child)
                            && filesystem::under(&m.path, &volume.root)
                    }) {
                        db.execute(
                            "INSERT OR IGNORE INTO pending(source) VALUES(?1)",
                            [&mark.path],
                        )
                        .map_err(error)?;
                    }
                }
            }
        }
        if metadata.is_dir() {
            let after = guards
                .last()
                .ok_or("Missing source directory handle")?
                .metadata()
                .map_err(error)?;
            if metadata.modified().ok() != after.modified().ok() {
                return Err("Source directory changed while planning. Run another pass for complete coverage.".into());
            }
        }
        if !relative.is_empty() {
            db.execute(
                "INSERT OR IGNORE INTO plan VALUES(?1,?2,?3,?4,?5)",
                params![
                    path,
                    relative,
                    relative.to_uppercase(),
                    rank as i64,
                    metadata.is_dir()
                ],
            )
            .map_err(error)?;
        }
        Ok(())
    }

    fn check_cancel(&self) -> Result<(), String> {
        if self.cancel.load(Ordering::SeqCst) {
            Err(PAUSED.into())
        } else {
            Ok(())
        }
    }

    fn copy_job(&self, db: &Connection, job: &Job, volumes: &[Volume], destination: &Volume) {
        if let Ok(mut state) = self.progress.lock() {
            state.current_path = Some(job.source.clone());
        }
        let result = self.copy_one(db, job, volumes, destination);
        match result {
            Ok(outcome) => {
                if let Ok(mut state) = self.progress.lock() {
                    match outcome {
                        Outcome::Directory => {}
                        Outcome::Existing => state.skipped += 1,
                        Outcome::Published(bytes) => {
                            state.copied += 1;
                            state.bytes_copied += bytes;
                        }
                    }
                }
            }
            Err(problem) if problem == PAUSED => {}
            Err(problem) => self.issue(&job.source, problem),
        }
    }

    fn copy_one(
        &self,
        db: &Connection,
        job: &Job,
        volumes: &[Volume],
        destination: &Volume,
    ) -> Result<Outcome, String> {
        self.copy_one_with_policy(db, job, volumes, destination, false)
    }

    fn copy_one_with_policy(
        &self,
        db: &Connection,
        job: &Job,
        volumes: &[Volume],
        destination: &Volume,
        require_empty: bool,
    ) -> Result<Outcome, String> {
        self.check_cancel()?;
        if excluded(&job.source, &self.exclusions) {
            return Err("This entry is excluded from migration copying.".into());
        }
        validate_destination(destination, volumes, self.fixture)?;
        let (volume, _, _) = filesystem::identify(&job.source, volumes)?;
        let (source_path, source_guards) = guard_source(&job.source, volume)?;
        let source_meta = source_guards
            .last()
            .ok_or("Missing source identity handle")?
            .metadata()
            .map_err(error)?;
        eligible_metadata(&source_meta)?;
        if job.directory {
            if !source_meta.is_dir() {
                return Err("Source directory changed type after planning".into());
            }
            let _gate = self.destination_gate.lock().map_err(error)?;
            self.check_cancel()?;
            destination_directory(destination, &job.relative, self.fixture)?;
            return Ok(Outcome::Directory);
        }
        let mut source = open_read(&source_path).map_err(error)?;
        let before = regular_file(&source)?;
        let source_stamp = stamp(&source, &before);
        no_named_streams(&source)?;
        let relative = Path::new(&job.relative);
        let parent = relative
            .parent()
            .ok_or("Missing destination parent")?
            .to_string_lossy();
        let gate = self.destination_gate.lock().map_err(error)?;
        self.check_cancel()?;
        let (target_parent, _target_guards) =
            destination_directory_with_policy(destination, &parent, self.fixture, !require_empty)?;
        let strict_parent = hold_entry(&target_parent, false, true, true)?;
        let name = relative.file_name().ok_or("Missing destination filename")?;
        let target = target_parent.join(name);
        match open_read(&target) {
            Ok(_) if require_empty => return Err("The alternate file already exists. It was not changed.".into()),
            Ok(mut existing) => {
                drop(strict_parent);
                drop(gate);
                let existing_before = regular_file(&existing)?;
                no_named_streams(&existing)?;
                if same_file(&source, &existing) {
                    return Err("Source and destination resolve to the same file".into());
                }
                let target_stamp = stamp(&existing, &existing_before);
                if let (Some(source_stamp), Some(target_stamp)) = (&source_stamp, &target_stamp) {
                    let cached = db.query_row(
                        "SELECT 1 FROM receipts WHERE destination=?1 AND source=?2 AND target=?3
                         AND source_stamp=?4 AND target_stamp=?5",
                        params![destination.id, job.source, job.target, source_stamp, target_stamp],
                        |_| Ok(()),
                    ).optional().map_err(error)?;
                    if cached.is_some() {
                        return Ok(Outcome::Existing);
                    }
                }
                if before.len() != existing_before.len() {
                    return Err(
                        "Destination already contains a different file. It was left intact.".into(),
                    );
                }
                let (source_digest, bytes) = self.transfer(&mut source, None)?;
                let (target_digest, target_bytes) = self.transfer(&mut existing, None)?;
                check_stable(&source, &before, &source_stamp, bytes)?;
                check_stable(&existing, &existing_before, &target_stamp, target_bytes)?;
                if source_digest != target_digest {
                    return Err(
                        "Destination already contains a different file. It was left intact.".into(),
                    );
                }
                if let Err(problem) = remember(
                    db,
                    destination,
                    job,
                    source_stamp,
                    target_stamp,
                    &source_digest,
                ) {
                    self.issue(
                        &job.source,
                        format!(
                            "Existing file verified, but its receipt could not be saved: {problem}"
                        ),
                    );
                }
                return Ok(Outcome::Existing);
            }
            Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => {}
            Err(problem) => {
                return Err(format!(
                    "Could not safely inspect the destination: {problem}"
                ))
            }
        }
        let mut staged = StagedFile::create(&target_parent)?;
        drop(strict_parent);
        drop(gate);
        let (source_digest, bytes) = self.transfer(&mut source, Some(&mut staged.file))?;
        check_stable(&source, &before, &source_stamp, bytes)?;
        staged.file.seek(SeekFrom::Start(0)).map_err(error)?;
        let (target_digest, target_bytes) = self.transfer(&mut staged.file, None)?;
        if source_digest != target_digest || bytes != target_bytes {
            return Err("Staged copy failed SHA-256 verification".into());
        }
        if let Ok(modified) = before.modified() {
            staged
                .file
                .set_times(fs::FileTimes::new().set_modified(modified))
                .map_err(error)?;
        }
        staged.file.sync_all().map_err(error)?;
        self.check_cancel()?;
        validate_destination(destination, volumes, self.fixture)?;
        {
            let _gate = self.destination_gate.lock().map_err(error)?;
            self.check_cancel()?;
            staged.publish(&target)?;
        }
        let target_stamp = staged
            .file
            .metadata()
            .ok()
            .and_then(|m| stamp(&staged.file, &m));
        if let Err(problem) = remember(
            db,
            destination,
            job,
            source_stamp,
            target_stamp,
            &source_digest,
        ) {
            self.issue(
                &job.source,
                format!("File copied and verified, but its receipt could not be saved: {problem}"),
            );
        }
        Ok(Outcome::Published(bytes))
    }

    fn transfer(
        &self,
        input: &mut File,
        mut output: Option<&mut File>,
    ) -> Result<(String, u64), String> {
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = vec![0; BUFFER_BYTES];
        loop {
            self.check_cancel()?;
            let count = input.read(&mut buffer).map_err(error)?;
            if count == 0 {
                return Ok((format!("{:x}", hasher.finalize()), bytes));
            }
            if let Some(file) = output.as_mut() {
                file.write_all(&buffer[..count]).map_err(error)?;
            }
            hasher.update(&buffer[..count]);
            bytes += count as u64;
        }
    }
}

fn active(phase: &str) -> bool {
    matches!(phase, "planning" | "copying")
}

fn decisions(document: &Value, volumes: &[Volume]) -> Vec<Decision> {
    let mut marks: Vec<_> = document["decisions"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, decision)| {
            if decision["noteOnly"] == true {
                return None;
            }
            let (volume_id, relative) = key.split_once('|')?;
            let volume = volumes.iter().find(|volume| volume.id == volume_id)?;
            let fallback = Path::new(&volume.root).join(relative);
            let fallback = filesystem::normalize(fallback.to_str()?).ok()?;
            let path = decision["path"]
                .as_str()
                .and_then(|path| filesystem::normalize(path).ok());
            let valid = path
                .as_deref()
                .and_then(|path| filesystem::locate(path, volumes).ok())
                .is_some_and(|(_, _, expected_key)| expected_key == *key)
                && decision["volumeId"]
                    .as_str()
                    .is_none_or(|id| id == volume_id);
            // A corrupt explicit record still fences inherited copy permission.
            let (path, status) = if valid {
                (
                    path.unwrap_or(fallback),
                    decision["status"].as_str().unwrap_or("attention"),
                )
            } else {
                (fallback, "attention")
            };
            Some(Decision {
                key: key.clone(),
                path,
                status: status.into(),
                decided_at: decision["decidedAt"].as_str().map(String::from),
            })
        })
        .collect();
    marks.sort_by(|a, b| {
        let date = |mark: &Decision| {
            mark.decided_at
                .as_deref()
                .and_then(|date| chrono::DateTime::parse_from_rfc3339(date).ok())
        };
        date(a).cmp(&date(b)).then_with(|| a.key.cmp(&b.key))
    });
    marks
}

fn nearest(
    path: &str,
    volumes: &[Volume],
    decisions: &std::collections::HashMap<&str, usize>,
) -> Option<usize> {
    let (volume, relative, _) = filesystem::locate(path, volumes).ok()?;
    let mut relative = relative.to_lowercase();
    loop {
        if let Some(rank) = decisions.get(format!("{}|{relative}", volume.id).as_str()) {
            return Some(*rank);
        }
        if relative.is_empty() {
            return None;
        }
        relative.truncate(relative.rfind('\\').unwrap_or(0));
    }
}

fn pass_signature(mark: &Decision, marks: &[Decision]) -> Result<String, String> {
    let prefix = if mark.key.ends_with('|') {
        mark.key.clone()
    } else {
        format!("{}\\", mark.key)
    };
    let mut signature = Sha256::new();
    signature.update(serde_json::to_vec(mark).map_err(error)?);
    for child in marks
        .iter()
        .filter(|candidate| candidate.key != mark.key && candidate.key.starts_with(&prefix))
    {
        signature.update(serde_json::to_vec(child).map_err(error)?);
    }
    Ok(format!("{:x}", signature.finalize()))
}

fn excluded(path: &str, exclusions: &[String]) -> bool {
    let lower = path.to_lowercase();
    exclusions.iter().any(|rule| match rule.strip_suffix('*') {
        Some(prefix) => lower.starts_with(&prefix.to_lowercase()),
        None => filesystem::under(path, rule),
    })
}

fn validate_relative(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    for name in path.split('\\') {
        let stem = name
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end()
            .to_uppercase();
        let reserved_port = ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        if name.is_empty()
            || name.ends_with([' ', '.'])
            || matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            )
            || reserved_port
            || name.chars().any(|c| c < ' ' || "<>:\"/|?*".contains(c))
        {
            return Err(format!("Unsupported destination filename: {name}"));
        }
    }
    Ok(())
}

pub fn destination(fixture_root: Option<&Path>) -> Volume {
    let root = fixture_root
        .map(|path| path.join("destination"))
        .unwrap_or_else(|| PathBuf::from(r"F:\"));
    let root = root.to_string_lossy().into_owned();
    let result = if fixture_root.is_some() {
        fixture_path(&root)
            .and_then(|_| hold_directories(Path::new(&root), false))
            .and_then(|guards| {
                let file = guards.last().ok_or("Missing fixture destination handle")?;
                let identity =
                    file_id(file).ok_or("Fixture destination needs a stable file identity")?;
                Ok((
                    format!("copy-fixture:{identity}"),
                    "Test destination".into(),
                ))
            })
    } else {
        volume_id(&root)
    };
    match result {
        Ok((id, label)) => Volume {
            root,
            id,
            label,
            available: true,
            problem: None,
        },
        Err(problem) => Volume {
            root: root.clone(),
            id: format!("unavailable-{root}"),
            label: root,
            available: false,
            problem: Some(problem),
        },
    }
}

fn fixture_path(path: &str) -> Result<String, String> {
    filesystem::normalize(path)
}

fn volume_id(root: &str) -> Result<(String, String), String> {
    let name = wide(Path::new(root));
    let mut guid = [0u16; 128];
    let mut label = [0u16; 261];
    let mut serial = 0;
    unsafe {
        if GetVolumeNameForVolumeMountPointW(name.as_ptr(), guid.as_mut_ptr(), guid.len() as u32)
            == 0
            || GetVolumeInformationW(
                guid.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            ) == 0
        {
            return Err(format!(
                "Destination volume is unavailable: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    let decode = |value: &[u16]| {
        String::from_utf16_lossy(
            &value[..value.iter().position(|&n| n == 0).unwrap_or(value.len())],
        )
    };
    Ok((format!("{}:{serial:08x}", decode(&guid)), decode(&label)))
}

pub(crate) fn verify_source_volume(volume: &Volume, guards: &[File]) -> Result<(), String> {
    if volume.id == "fixture-volume" {
        return Ok(());
    }
    let root = guards.first().ok_or("Missing source volume handle")?;
    let (expected, _) = volume
        .id
        .rsplit_once(':')
        .ok_or("Invalid source volume identity")?;
    let mut name = [0u16; 128];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            root.as_raw_handle(),
            name.as_mut_ptr(),
            name.len() as u32,
            VOLUME_NAME_GUID | FILE_NAME_OPENED,
        )
    } as usize;
    if length == 0 || length >= name.len() {
        return Err("Could not confirm the opened source volume identity".into());
    }
    let opened = String::from_utf16(&name[..length]).map_err(error)?;
    if !opened
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(expected.trim_end_matches('\\'))
    {
        return Err("The source volume changed while its path was being opened".into());
    }
    Ok(())
}

fn validate_destination(dest: &Volume, sources: &[Volume], fixture: bool) -> Result<(), String> {
    if !dest.available {
        return Err(dest
            .problem
            .clone()
            .unwrap_or_else(|| "Destination is unavailable".into()));
    }
    if fixture {
        fixture_path(&dest.root)?;
        let root = Path::new(&dest.root);
        let parent = root.parent().ok_or("Invalid fixture destination")?;
        if root.file_name().and_then(|s| s.to_str()) != Some("destination")
            || sources.is_empty()
            || sources.iter().any(|source| {
                source.id != "fixture-volume" || Path::new(&source.root) != parent.join("source")
            })
            || destination(Some(parent)).id != dest.id
        {
            return Err(
                "Fixture copying requires the unchanged sibling source and destination".into(),
            );
        }
        return Ok(());
    }
    if !dest.root.eq_ignore_ascii_case(r"F:\") {
        return Err("Choose the configured F: destination".into());
    }
    let (current, _) = volume_id(&dest.root)?;
    let guid = |id: &str| id.rsplit_once(':').map(|(guid, _)| guid.to_lowercase());
    if current != dest.id
        || sources
            .iter()
            .any(|source| guid(&source.id) == guid(&dest.id))
    {
        return Err("Destination changed or points to a source volume. Copy stopped.".into());
    }
    Ok(())
}

fn destination_directory(
    dest: &Volume,
    relative: &str,
    fixture: bool,
) -> Result<(PathBuf, Vec<File>), String> {
    destination_directory_with_policy(dest, relative, fixture, true)
}

fn destination_directory_with_policy(
    dest: &Volume,
    relative: &str,
    fixture: bool,
    create: bool,
) -> Result<(PathBuf, Vec<File>), String> {
    validate_relative(relative)?;
    let root = if fixture {
        PathBuf::from(&dest.root)
    } else {
        let (guid, _) = dest
            .id
            .rsplit_once(':')
            .ok_or("Invalid destination volume identity")?;
        if !guid.starts_with(r"\\?\Volume{") || !guid.ends_with("}\\") {
            return Err("Invalid destination volume identifier".into());
        }
        PathBuf::from(guid)
    };
    let mut guards = hold_directories(&root, false)?;
    if fixture {
        let current = guards
            .last()
            .and_then(file_id)
            .map(|id| format!("copy-fixture:{id}"));
        if current.as_deref() != Some(&dest.id) {
            return Err("The fixture destination directory was replaced".into());
        }
    }
    let mut target = root;
    if !relative.is_empty() {
        for component in relative.split('\\') {
            let _strict_parent = hold_entry(&target, false, true, true)?;
            target.push(component);
            guards.push(hold_directory(&target, create)?);
        }
    }
    Ok((target, guards))
}

fn hold_directories(path: &Path, create: bool) -> Result<Vec<File>, String> {
    let parents: Vec<_> = path.ancestors().take_while(|p| p.has_root()).collect();
    let mut guards = Vec::with_capacity(parents.len());
    let mut previous = None;
    for path in parents.into_iter().rev() {
        let _strict_parent = previous
            .map(|parent| hold_entry(parent, false, true, true))
            .transpose()?;
        guards.push(hold_directory(path, create)?);
        previous = Some(path);
    }
    Ok(guards)
}

pub(crate) fn guard_copy_state_directory(path: &Path) -> Result<Vec<File>, String> {
    if !path.is_absolute() {
        return Err("Copy effect history requires an absolute app data directory.".into());
    }
    hold_directories(path, false)
}

fn hold_directory(path: &Path, create: bool) -> Result<File, String> {
    hold_entry(path, create, true, false)
}

fn guard_source(path: &str, volume: &Volume) -> Result<(PathBuf, Vec<File>), String> {
    let path = PathBuf::from(filesystem::normalize(path)?);
    let parts: Vec<_> = path.ancestors().take_while(|p| p.has_root()).collect();
    let guards = parts
        .into_iter()
        .rev()
        .map(|part| hold_entry(part, false, part != path, true))
        .collect::<Result<Vec<_>, _>>()?;
    verify_source_volume(volume, &guards)?;
    Ok((path, guards))
}

fn hold_entry(path: &Path, create: bool, directory: bool, read_only: bool) -> Result<File, String> {
    let open = || {
        OpenOptions::new()
            // Directory-list (or file-data read) access activates sharing checks.
            // Attribute-only handles do not reliably fence writes and renames.
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES)
            .share_mode(if read_only {
                FILE_SHARE_READ
            } else {
                FILE_SHARE_READ | FILE_SHARE_WRITE
            })
            .custom_flags(
                FILE_FLAG_BACKUP_SEMANTICS
                    | FILE_FLAG_OPEN_REPARSE_POINT
                    | FILE_FLAG_OPEN_NO_RECALL,
            )
            .open(path)
    };
    let file = match open() {
        Ok(file) => file,
        Err(problem) if create && problem.kind() == std::io::ErrorKind::NotFound => {
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(problem) if problem.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(problem) => return Err(format!("{}: {problem}", path.display())),
            }
            open().map_err(error)?
        }
        Err(problem) => return Err(format!("{}: {problem}", path.display())),
    };
    let metadata = file.metadata().map_err(error)?;
    eligible_metadata(&metadata)?;
    if directory && !metadata.is_dir() {
        return Err(format!(
            "Path ancestor is not a directory: {}",
            path.display()
        ));
    }
    Ok(file)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn open_read(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .access_mode(FILE_GENERIC_READ)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(
            FILE_FLAG_OPEN_NO_RECALL | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN,
        )
        .open(path)
}

fn eligible_metadata(metadata: &fs::Metadata) -> Result<(), String> {
    use std::os::windows::fs::MetadataExt;
    if filesystem::reparse(metadata)
        || metadata.file_attributes() & (0x1000 | 0x40000 | 0x400000) != 0
    {
        return Err("Links, reparse paths and offline data are outside the copy scope".into());
    }
    Ok(())
}

fn regular_file(file: &File) -> Result<fs::Metadata, String> {
    let metadata = file.metadata().map_err(error)?;
    eligible_metadata(&metadata)?;
    if !metadata.is_file() {
        return Err("Source or destination is not a regular file".into());
    }
    Ok(metadata)
}

fn file_id(file: &File) -> Option<String> {
    let mut info = FILE_ID_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    };
    (ok != 0).then(|| {
        format!(
            "{:016x}-{:02x?}",
            info.VolumeSerialNumber, info.FileId.Identifier
        )
    })
}

fn same_file(source: &File, destination: &File) -> bool {
    file_id(source).is_some_and(|source| Some(source) == file_id(destination))
}

fn stamp(file: &File, metadata: &fs::Metadata) -> Option<String> {
    let identity = file_id(file)?;
    let mut info = FILE_BASIC_INFO::default();
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&mut info as *mut FILE_BASIC_INFO).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    };
    (ok != 0 && info.ChangeTime != 0).then(|| {
        format!(
            "{identity}|{}|{}|{}|{}",
            metadata.len(),
            info.CreationTime,
            info.LastWriteTime,
            info.ChangeTime
        )
    })
}

fn check_stable(
    file: &File,
    before: &fs::Metadata,
    initial: &Option<String>,
    bytes: u64,
) -> Result<(), String> {
    let after = regular_file(file)?;
    if bytes != before.len()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || *initial != stamp(file, &after)
    {
        return Err("File changed while being read. Copy verification failed.".into());
    }
    Ok(())
}

fn no_named_streams(file: &File) -> Result<(), String> {
    // The DWORD-aligned buffer need only hold the ordinary unnamed entry.
    // A larger stream list fails safely instead of silently dropping data.
    let mut buffer = [0u64; 128];
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStreamInfo,
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(&buffer) as u32,
        )
    };
    if ok == 0 {
        let problem = std::io::Error::last_os_error();
        if matches!(problem.raw_os_error(), Some(1 | 50)) {
            // The filesystem reports that it does not support stream enumeration.
            return Ok(());
        }
        return Err(format!(
            "Could not establish file stream coverage: {problem}"
        ));
    }
    let next = buffer[0] as u32;
    let name_bytes = (buffer[0] >> 32) as usize;
    let name = "::$DATA".encode_utf16().collect::<Vec<_>>();
    let words =
        unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u16>().add(12), name.len()) };
    if next != 0 || name_bytes != name.len() * 2 || words != name {
        return Err("Additional file data streams require separate migration review".into());
    }
    Ok(())
}

struct Job {
    rank: i64,
    source: String,
    relative: String,
    target: String,
    directory: bool,
}

fn next_jobs(
    db: &Connection,
    after: &(i64, String, String),
    workers: usize,
) -> Result<Vec<Job>, String> {
    let mut query = db
        .prepare_cached(
            "SELECT rank,source,relative,target,directory FROM plan
         WHERE (rank,target,source)>(?1,?2,?3) ORDER BY rank,target,source LIMIT ?4",
        )
        .map_err(error)?;
    let jobs = query
        .query_map(params![after.0, after.1, after.2, workers as i64], |row| {
            Ok(Job {
                rank: row.get(0)?,
                source: row.get(1)?,
                relative: row.get(2)?,
                target: row.get(3)?,
                directory: row.get(4)?,
            })
        })
        .map_err(error)?;
    jobs.collect::<Result<Vec<_>, _>>().map_err(error)
}

enum Outcome {
    Directory,
    Existing,
    Published(u64),
}

fn is_blocked(db: &Connection, target: &str) -> Result<bool, String> {
    let mut candidate = target;
    loop {
        let exists = db
            .query_row("SELECT 1 FROM blocked WHERE target=?1", [candidate], |_| {
                Ok(())
            })
            .optional()
            .map_err(error)?
            .is_some();
        if exists {
            return Ok(true);
        }
        let Some((parent, _)) = candidate.rsplit_once('\\') else {
            return Ok(false);
        };
        candidate = parent;
    }
}

fn block_conflicts(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        "INSERT OR IGNORE INTO blocked SELECT target FROM plan GROUP BY target
            HAVING count(*)>1 AND sum(directory=0)>0;
         INSERT OR IGNORE INTO blocked SELECT file.target FROM plan file
            WHERE file.directory=0 AND EXISTS(
              SELECT 1 FROM plan child
              WHERE child.target >= file.target||char(92) AND child.target < file.target||']');",
    )
    .map_err(error)
}

fn remember(
    db: &Connection,
    destination: &Volume,
    job: &Job,
    source: Option<String>,
    target: Option<String>,
    digest: &str,
) -> Result<(), String> {
    if let (Some(source), Some(target)) = (source, target) {
        db.execute(
            "INSERT OR REPLACE INTO receipts VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                destination.id,
                job.source,
                job.target,
                source,
                target,
                digest
            ],
        )
        .map_err(error)?;
    }
    Ok(())
}

struct StagedFile {
    file: File,
    published: bool,
}

impl StagedFile {
    fn create(parent: &Path) -> Result<Self, String> {
        let mut nonce = [0u8; 24];
        getrandom::fill(&mut nonce).map_err(error)?;
        let path = parent.join(format!(".migration-master-{}.part", hash(&nonce)));
        let file = OpenOptions::new()
            .write(true)
            .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE)
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .create_new(true)
            .open(path)
            .map_err(error)?;
        Ok(Self {
            file,
            published: false,
        })
    }

    fn publish(&mut self, target: &Path) -> Result<(), String> {
        // Keep ownership of the staging handle throughout the no-replace rename.
        // The verified destination ancestors remain held while Windows resolves it.
        let text = target.to_str().ok_or("Invalid destination name")?;
        let name = if text.starts_with(r"\\?\") {
            wide(target)
        } else {
            wide(Path::new(&format!(r"\\?\{text}")))
        };
        let header = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
        let byte_count = header + name.len() * 2;
        let mut aligned = vec![0usize; byte_count.div_ceil(std::mem::size_of::<usize>())];
        let info = aligned.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        unsafe {
            (*info).FileNameLength = ((name.len() - 1) * 2) as u32;
            std::ptr::copy_nonoverlapping(
                name.as_ptr(),
                std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
                name.len(),
            );
            if SetFileInformationByHandle(
                self.file.as_raw_handle(),
                FileRenameInfo,
                info.cast(),
                byte_count as u32,
            ) == 0
            {
                return Err(format!(
                    "Could not publish without replacing an existing file: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        self.published = true;
        Ok(())
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if !self.published {
            let delete = FILE_DISPOSITION_INFO { DeleteFile: true };
            unsafe {
                SetFileInformationByHandle(
                    self.file.as_raw_handle(),
                    FileDispositionInfo,
                    (&delete as *const FILE_DISPOSITION_INFO).cast(),
                    std::mem::size_of_val(&delete) as u32,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture() -> (PathBuf, Vec<Volume>, Volume, Arc<CopyQueue>) {
        let root = std::env::temp_dir().join(format!(
            "migration-copy-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fixture_path(&root.to_string_lossy()).unwrap();
        fs::create_dir_all(root.join("source")).unwrap();
        fs::create_dir(root.join("destination")).unwrap();
        let sources = filesystem::volumes(Some(&root.join("source")));
        let target = destination(Some(&root));
        assert!(target.available, "{target:?}");
        let queue = CopyQueue::new(root.join("receipts.sqlite"), Vec::new(), true);
        (root, sources, target, queue)
    }

    fn mark(
        document: &mut Value,
        volumes: &[Volume],
        path: &Path,
        status: &str,
        time: &str,
        note_only: bool,
    ) {
        let path = path.to_string_lossy();
        let (volume, _, key) = filesystem::locate(&path, volumes).unwrap();
        document["decisions"][key] = json!({
            "path":path, "volumeId":volume.id, "status":status, "decidedAt":time, "noteOnly":note_only,
        });
    }

    fn wait(queue: &CopyQueue) -> Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let status = queue.status().unwrap();
            if status["running"] == false {
                return status;
            }
            assert!(Instant::now() < deadline, "{status}");
            thread::sleep(Duration::from_millis(15));
        }
    }

    #[test]
    fn copies_full_relative_paths_follows_overrides_and_rechecks_existing_content() {
        let (root, volumes, destination, queue) = fixture();
        let source = root.join(r"source\Documents\Projects");
        let target = root.join(r"destination\Documents\Projects");
        let limited = source.join("Later");
        let selected_again = limited.join("Again");
        fs::create_dir_all(&selected_again).unwrap();
        fs::create_dir(source.join("Empty")).unwrap();
        fs::write(source.join("keep.txt"), b"source").unwrap();
        fs::write(source.join("peer.txt"), b"peer").unwrap();
        fs::write(limited.join("only-later.txt"), b"later").unwrap();
        fs::write(selected_again.join("keep-too.txt"), b"again").unwrap();
        let mut doc = json!({"revision":7,"decisions":{}});
        let old = "2026-10-31T00:00:00Z";
        let new = "2026-11-01T00:00:00Z";
        mark(
            &mut doc,
            &volumes,
            &selected_again,
            "migrate-now",
            new,
            false,
        );
        mark(&mut doc, &volumes, &source, "migrate-now", old, false);
        mark(&mut doc, &volumes, &limited, "last-moment", old, false);
        mark(
            &mut doc,
            &volumes,
            &source.join("keep.txt"),
            "erase",
            old,
            true,
        );
        let initial = queue.snapshot(&doc, &volumes, &destination).unwrap();
        assert_eq!(initial["migrateNowCount"], 2);
        assert_eq!(
            initial["migrateNow"][0]["path"],
            source.to_string_lossy().as_ref()
        );
        assert_eq!(initial["pendingMigrateNow"], 2);

        queue
            .start(
                doc.clone(),
                volumes.clone(),
                destination.clone(),
                "all",
                None,
            )
            .unwrap();
        let status = wait(&queue);
        assert_eq!(status["phase"], "complete", "{status}");
        assert_eq!(status["copied"], 3, "{status}");
        assert!(target.join("Empty").is_dir());
        assert!(!target.join(r"Later\only-later.txt").exists());
        assert_eq!(fs::read(target.join("keep.txt")).unwrap(), b"source");
        assert_eq!(fs::read(target.join("peer.txt")).unwrap(), b"peer");
        assert_eq!(
            fs::read(target.join(r"Later\Again\keep-too.txt")).unwrap(),
            b"again"
        );
        let checked = queue.snapshot(&doc, &volumes, &destination).unwrap();
        assert_eq!(checked["pendingMigrateNow"], 0);
        assert_eq!(checked["queueSignature"], initial["queueSignature"]);

        let other = source.join("Other");
        mark(&mut doc, &volumes, &other, "redownload", new, false);
        let changed = queue.snapshot(&doc, &volumes, &destination).unwrap();
        assert_eq!(changed["pendingMigrateNow"], 1);
        assert_ne!(changed["queueSignature"], initial["queueSignature"]);

        queue
            .start(
                doc.clone(),
                volumes.clone(),
                destination.clone(),
                "ordered",
                None,
            )
            .unwrap();
        let status = wait(&queue);
        assert_eq!(status["phase"], "complete", "{status}");
        assert_eq!(status["skipped"], 3, "{status}");
        fs::write(target.join("keep.txt"), b"edited").unwrap();
        queue
            .start(
                doc.clone(),
                volumes.clone(),
                destination.clone(),
                "ordered",
                None,
            )
            .unwrap();
        let status = wait(&queue);
        assert_eq!(status["phase"], "partial", "{status}");
        assert_eq!(status["failed"], 1, "{status}");
        assert_eq!(fs::read(target.join("keep.txt")).unwrap(), b"edited");

        queue
            .start(doc, volumes, destination, "last-moment", limited.to_str())
            .unwrap();
        let status = wait(&queue);
        assert_eq!(status["phase"], "complete", "{status}");
        assert_eq!(status["copied"], 1, "{status}");
        assert_eq!(
            fs::read(target.join(r"Later\only-later.txt")).unwrap(),
            b"later"
        );
    }

    #[test]
    fn preflight_blocks_exact_file_and_ancestor_collisions() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            r"CREATE TABLE plan(source TEXT, target TEXT, directory INTEGER);
             CREATE TABLE blocked(target TEXT PRIMARY KEY);
             INSERT INTO plan VALUES
                ('C:\Same.txt','SAME.TXT',0), ('E:\same.txt','SAME.TXT',0),
                ('C:\Tree','TREE',0), ('E:\Tree\Child','TREE\CHILD',0),
                ('C:\Shared','SHARED',1), ('E:\Shared','SHARED',1);",
        )
        .unwrap();
        block_conflicts(&db).unwrap();
        for target in ["SAME.TXT", "TREE", r"TREE\CHILD"] {
            assert!(is_blocked(&db, target).unwrap(), "{target}");
        }
        assert!(!is_blocked(&db, "SHARED").unwrap());
        assert!(!is_blocked(&db, "TREE2").unwrap());
    }

    #[test]
    fn no_replace_publish_keeps_racing_destination_and_cleans_owned_temp() {
        let (root, _, _, _) = fixture();
        let parent = root.join("destination");
        let _guard = hold_directory(&parent, false).unwrap();
        let mut stage = StagedFile::create(&parent).unwrap();
        stage.file.write_all(b"staged").unwrap();
        fs::write(parent.join("raced.txt"), b"existing").unwrap();
        assert!(stage.publish(&parent.join("raced.txt")).is_err());
        drop(stage);
        assert_eq!(fs::read(parent.join("raced.txt")).unwrap(), b"existing");
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
    }

    #[test]
    fn changed_destination_identity_refuses_start() {
        let (root, volumes, destination, queue) = fixture();
        let path = root.join("destination");
        fs::rename(&path, root.join("old-destination")).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(queue
            .start(
                json!({"decisions":{}}),
                volumes,
                destination,
                "ordered",
                None
            )
            .is_err());
        assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    }

    #[test]
    fn volume_guid_paths_publish_inside_the_same_fixture() {
        let (root, _, _, _) = fixture();
        let drive = root.ancestors().last().and_then(Path::to_str).unwrap();
        let (id, label) = volume_id(drive).unwrap();
        let (guid, _) = id.rsplit_once(':').unwrap();
        let parent = root.join("destination");
        let pinned = Path::new(guid).join(&parent.to_string_lossy()[3..]);
        let guards = hold_directories(&pinned, false).unwrap();
        verify_source_volume(
            &Volume {
                root: drive.into(),
                id,
                label,
                available: true,
                problem: None,
            },
            &guards,
        )
        .unwrap();
        let mut stage = StagedFile::create(&pinned).unwrap();
        stage.file.write_all(b"pinned").unwrap();
        stage.publish(&pinned.join("guid.txt")).unwrap();
        drop(stage);
        assert_eq!(fs::read(parent.join("guid.txt")).unwrap(), b"pinned");
    }

    #[test]
    fn open_destination_ancestor_cannot_be_renamed() {
        let (root, _, _, _) = fixture();
        let path = root.join("destination");
        let renamed = root.join("renamed-destination");
        let guard = hold_directory(&path, false).unwrap();
        assert!(fs::rename(&path, &renamed).is_err());
        drop(guard);
        fs::rename(&path, &renamed).unwrap();
    }
}
