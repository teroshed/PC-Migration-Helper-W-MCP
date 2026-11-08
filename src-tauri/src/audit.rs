//! Persistent read-only source audit. SQLite is a derived baseline, never the decision journal.
//! File payloads are streamed through a bounded SHA-256 pool. Sizes count directory entries,
//! including each hardlink occurrence; unnamed data streams only, no backup/restore claim.
use crate::{
    filesystem::{self, Volume},
    store::{error, now},
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::Duration,
};

const CHUNK: usize = 500;
const BUFFER: usize = 1024 * 1024;
const SCOPE_NOTE:&str="SHA-256 covers the unnamed file data stream only. Alternate data streams, ACLs, directory allocation, restore usability and application-consistent snapshots are outside this audit. Allocated bytes count each directory entry; hardlinks are not deduplicated.";

pub struct Audit {
    path: PathBuf,
    namespace: String,
    exclusions: Vec<String>,
    volumes: Mutex<Vec<Volume>>,
    running: AtomicBool,
    cancel: AtomicBool,
    stats_cache: Mutex<HashMap<String, (i64, Value)>>,
}
#[derive(Clone)]
struct Observation {
    path: String,
    kind: String,
    logical: i64,
    allocated: Option<i64>,
    identity: String,
    mtime: String,
    problem: Option<String>,
}
struct HashJob {
    id: i64,
    path: String,
}
struct HashResult {
    id: i64,
    digest: Option<String>,
    logical: i64,
    allocated: Option<i64>,
    identity: String,
    mtime: String,
    problem: Option<String>,
    cancelled: bool,
}

struct HashBaseline {
    parent: String,
    digest: Option<String>,
    logical: i64,
    allocated: Option<i64>,
    first_run: i64,
    identity: String,
    mtime: String,
}

struct WorkerPool<'a, J, R> {
    jobs: Option<mpsc::SyncSender<J>>,
    output: Option<mpsc::Receiver<R>>,
    handles: Vec<thread::JoinHandle<()>>,
    cancel: &'a AtomicBool,
    successful: bool,
}
impl<J, R> Drop for WorkerPool<'_, J, R> {
    fn drop(&mut self) {
        if !self.successful {
            self.cancel.store(true, Ordering::SeqCst);
        }
        drop(self.jobs.take());
        drop(self.output.take());
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Audit {
    pub fn open(
        path: PathBuf,
        namespace: String,
        exclusions: Vec<String>,
    ) -> Result<Arc<Self>, String> {
        fs::create_dir_all(
            path.parent()
                .ok_or("Audit database needs a parent directory")?,
        )
        .map_err(error)?;
        let audit = Arc::new(Self {
            path,
            namespace,
            exclusions,
            volumes: Mutex::new(Vec::new()),
            running: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            stats_cache: Mutex::new(HashMap::new()),
        });
        let db = audit.connect()?;
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT);
            CREATE TABLE IF NOT EXISTS runs(id INTEGER PRIMARY KEY,phase TEXT,roots TEXT,started_at TEXT,completed_at TEXT,workers INTEGER,baseline INTEGER,current_path TEXT,error TEXT,
              discovered INTEGER DEFAULT 0,counted INTEGER DEFAULT 0,hashed INTEGER DEFAULT 0,failed INTEGER DEFAULT 0,logical INTEGER DEFAULT 0,allocated INTEGER DEFAULT 0,unknown INTEGER DEFAULT 0,gaps INTEGER DEFAULT 0);
            CREATE TABLE IF NOT EXISTS dirs(path TEXT PRIMARY KEY COLLATE NOCASE,parent TEXT COLLATE NOCASE,run INTEGER,state TEXT,problem TEXT,observed_at TEXT,changed_at TEXT,
              discovered INTEGER DEFAULT 0,counted INTEGER DEFAULT 0,hashed INTEGER DEFAULT 0,failed INTEGER DEFAULT 0,logical INTEGER DEFAULT 0,allocated INTEGER DEFAULT 0,unknown INTEGER DEFAULT 0,gaps INTEGER DEFAULT 0);
            CREATE INDEX IF NOT EXISTS dirs_run_state ON dirs(run,state);
            CREATE INDEX IF NOT EXISTS dirs_parent ON dirs(parent);
            CREATE TABLE IF NOT EXISTS files(id INTEGER PRIMARY KEY,path TEXT UNIQUE COLLATE NOCASE,parent TEXT COLLATE NOCASE,kind TEXT,run INTEGER,state TEXT,
              logical INTEGER,allocated INTEGER,identity TEXT,mtime TEXT,digest TEXT,verified_at TEXT,changed_at TEXT,first_run INTEGER,problem TEXT);
            CREATE INDEX IF NOT EXISTS files_jobs ON files(run,state,id);
            CREATE INDEX IF NOT EXISTS files_parent ON files(parent,run);
            CREATE INDEX IF NOT EXISTS files_changes ON files(changed_at,path);
            CREATE TABLE IF NOT EXISTS notifications(id INTEGER PRIMARY KEY,event_key TEXT UNIQUE,kind TEXT,title TEXT,message TEXT,path TEXT,created_at TEXT,read_at TEXT,change_count INTEGER DEFAULT 1);
            CREATE INDEX IF NOT EXISTS notifications_unread ON notifications(read_at,id);
            CREATE INDEX IF NOT EXISTS notifications_recent ON notifications(created_at DESC,id DESC);
            CREATE TABLE IF NOT EXISTS watch_changes(path TEXT PRIMARY KEY COLLATE NOCASE,changed_at TEXT,kind TEXT);
            UPDATE runs SET phase='paused' WHERE phase IN('discovering','hashing');
            UPDATE dirs SET state='pending' WHERE state='working';
            UPDATE files SET state='pending' WHERE state='working';").map_err(error)?;
        for table in ["files", "dirs"] {
            let mut q = db
                .prepare(&format!("PRAGMA table_info({table})"))
                .map_err(error)?;
            let names = q
                .query_map([], |r| r.get::<_, String>(1))
                .map_err(error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(error)?;
            if !names.iter().any(|n| n == "seen_token") {
                db.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN seen_token TEXT DEFAULT ''"),
                    [],
                )
                .map_err(error)?;
            }
        }
        db.execute(
            "INSERT OR IGNORE INTO meta VALUES('changeSequence','0')",
            [],
        )
        .map_err(error)?;
        db.execute("INSERT OR IGNORE INTO meta VALUES('updatedAt',?1)", [now()])
            .map_err(error)?;
        let existing: Option<String> = db
            .query_row("SELECT value FROM meta WHERE key='namespace'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(error)?;
        if existing.as_ref().is_some_and(|v| v != &audit.namespace) {
            return Err("Audit database belongs to different source volumes. Use a separate namespace; the previous baseline was preserved.".into());
        }
        db.execute(
            "INSERT OR IGNORE INTO meta VALUES('namespace',?1)",
            [&audit.namespace],
        )
        .map_err(error)?;
        Ok(audit)
    }
    fn connect(&self) -> Result<Connection, String> {
        let db = Connection::open(&self.path).map_err(error)?;
        db.busy_timeout(Duration::from_secs(15)).map_err(error)?;
        db.execute_batch("PRAGMA synchronous=NORMAL;")
            .map_err(error)?;
        Ok(db)
    }
    pub fn set_volumes(&self, volumes: Vec<Volume>) -> Result<(), String> {
        *self.volumes.lock().map_err(error)? = volumes;
        Ok(())
    }
    fn volumes(&self) -> Result<Vec<Volume>, String> {
        Ok(self.volumes.lock().map_err(error)?.clone())
    }
    pub fn pause(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
    pub fn start(
        self: &Arc<Self>,
        roots: Vec<String>,
        volumes: Vec<Volume>,
        resume: bool,
        workers: usize,
    ) -> Result<(), String> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Err("A size and checksum audit is already running.".into());
        }
        self.cancel.store(false, Ordering::SeqCst);
        let init = (|| {
            self.set_volumes(volumes.clone())?;
            let mut db = self.connect()?;
            if resume {
                let run: Option<(i64, String, String)> = db
                    .query_row(
                        "SELECT id,roots,phase FROM runs ORDER BY id DESC LIMIT 1",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )
                    .optional()
                    .map_err(error)?;
                let (id, raw, phase) = run.ok_or("There is no audit to resume")?;
                if !matches!(phase.as_str(), "paused" | "error") {
                    return Err("Only a paused or failed audit can be resumed; start a new comparison after completion".into());
                }
                let saved: Vec<String> = serde_json::from_str(&raw).map_err(error)?;
                for p in &saved {
                    filesystem::identify(p, &volumes)?;
                }
                db.execute(
                    "UPDATE files SET state='pending' WHERE run=?1 AND state='working'",
                    [id],
                )
                .map_err(error)?;
                db.execute(
                    "UPDATE dirs SET state='pending' WHERE run=?1 AND state='working'",
                    [id],
                )
                .map_err(error)?;
                db.execute("UPDATE runs SET phase=CASE WHEN EXISTS(SELECT 1 FROM dirs WHERE run=?1 AND state='pending') THEN 'discovering' ELSE 'hashing' END,error=NULL,workers=?2 WHERE id=?1",params![id,workers.clamp(1,8) as i64]).map_err(error)?;
                touch(&db)?;
                return Ok(id);
            }
            if roots.is_empty() {
                return Err("Select at least one review root".into());
            }
            let mut selected = Vec::<String>::new();
            for root in roots {
                let p = filesystem::normalize(&root)?;
                filesystem::identify(&p, &volumes)?;
                if selected.iter().any(|old| filesystem::under(&p, old)) {
                    continue;
                }
                selected.retain(|old| !filesystem::under(old, &p));
                selected.push(p);
            }
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(error)?;
            let prior: i64 = tx
                .query_row(
                    "SELECT count(*) FROM runs WHERE phase='complete'",
                    [],
                    |r| r.get(0),
                )
                .map_err(error)?;
            tx.execute("INSERT INTO runs(phase,roots,started_at,workers,baseline) VALUES('discovering',?1,?2,?3,?4)",params![serde_json::to_string(&selected).map_err(error)?,now(),workers.clamp(1,8) as i64,prior==0]).map_err(error)?;
            let id = tx.last_insert_rowid();
            for root in &selected {
                tx.execute("INSERT INTO dirs(path,parent,run,state,changed_at) VALUES(?1,'',?2,'pending',?3) ON CONFLICT(path) DO UPDATE SET run=?2,state='pending',problem=NULL,discovered=0,counted=0,hashed=0,failed=0,logical=0,allocated=0,unknown=0,gaps=0",params![root,id,now()]).map_err(error)?;
            }
            touch(&tx)?;
            tx.commit().map_err(error)?;
            Ok(id)
        })();
        let id = match init {
            Ok(id) => id,
            Err(e) => {
                self.running.store(false, Ordering::SeqCst);
                return Err(e);
            }
        };
        let audit = self.clone();
        if let Err(e) = thread::Builder::new()
            .name("migration-audit-manager".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    audit.run(id, volumes, workers.clamp(1, 8))
                }));
                let failure = match outcome {
                    Ok(Err(e)) => Some(e),
                    Err(_) => Some(
                        "The audit worker stopped unexpectedly. Its durable queue can be resumed."
                            .into(),
                    ),
                    _ => None,
                };
                if let Some(failure) = failure {
                    if let Ok(db) = audit.connect() {
                        let _ = db.execute(
                            "UPDATE runs SET phase='error',error=?2 WHERE id=?1",
                            params![id, failure],
                        );
                    }
                }
                audit.running.store(false, Ordering::SeqCst);
            })
        {
            self.running.store(false, Ordering::SeqCst);
            return Err(error(e));
        }
        Ok(())
    }
    fn run(self: &Arc<Self>, run: i64, volumes: Vec<Volume>, workers: usize) -> Result<(), String> {
        let db = self.connect()?;
        self.discover_parallel(run, &volumes, workers)?;
        if self.cancel.load(Ordering::Relaxed) {
            return self.finish_paused(run);
        }
        db.execute(
            "UPDATE runs SET phase='hashing',current_path='' WHERE id=?1",
            [run],
        )
        .map_err(error)?;
        self.hash_pending(run, &volumes, workers)?;
        if self.cancel.load(Ordering::Relaxed) {
            return self.finish_paused(run);
        }
        db.execute(
            "UPDATE runs SET phase='complete',completed_at=?2,current_path='' WHERE id=?1",
            params![run, now()],
        )
        .map_err(error)?;
        touch(&db)?;
        let baseline: bool = db
            .query_row("SELECT baseline FROM runs WHERE id=?1", [run], |r| r.get(0))
            .map_err(error)?;
        let status = self.status()?;
        let title = if baseline {
            "First source checksum baseline recorded"
        } else {
            "Source checksum comparison finished"
        };
        let msg=format!("{} files discovered; {} stable SHA-256 reads; {} file gaps and {} coverage gaps. This is source verification, not backup or restore verification. {}",status["discoveredLeaves"],status["hashedLeaves"],status["failedLeaves"],status["coverageGaps"],SCOPE_NOTE);
        self.notify(
            &format!("complete|{run}"),
            if baseline {
                "baseline"
            } else {
                "audit-complete"
            },
            title,
            &msg,
            "",
            1,
        )?;
        Ok(())
    }
    fn finish_paused(&self, run: i64) -> Result<(), String> {
        let db = self.connect()?;
        db.execute(
            "UPDATE dirs SET state='pending' WHERE run=?1 AND state='working'",
            [run],
        )
        .map_err(error)?;
        db.execute(
            "UPDATE files SET state='pending' WHERE run=?1 AND state='working'",
            [run],
        )
        .map_err(error)?;
        db.execute("UPDATE runs SET phase='paused' WHERE id=?1", [run])
            .map_err(error)?;
        touch(&db)?;
        Ok(())
    }
    fn disk_guard(&self) -> Result<(), String> {
        let free = fs2::available_space(self.path.parent().ok_or("Audit path has no parent")?)
            .map_err(error)?;
        if free < 2_000_000_000 {
            self.cancel.store(true, Ordering::SeqCst);
            self.notify("audit-disk-space", "coverage-gap", "Audit paused to preserve disk space", "The audit database drive has less than 2 GB free. Existing results and the durable queue are retained; free space before resuming.", "", 1)?;
        }
        Ok(())
    }
    fn discover_parallel(
        self: &Arc<Self>,
        run: i64,
        volumes: &[Volume],
        workers: usize,
    ) -> Result<(), String> {
        let (jobs, receiver) = mpsc::sync_channel::<String>(workers * 2);
        let receiver = Arc::new(Mutex::new(receiver));
        let (results, output) = mpsc::sync_channel::<(String, Result<(), String>)>(workers * 2);
        let mut pool = WorkerPool {
            jobs: Some(jobs),
            output: Some(output),
            handles: Vec::new(),
            cancel: &self.cancel,
            successful: false,
        };
        for n in 0..workers {
            let input = receiver.clone();
            let output = results.clone();
            let audit = self.clone();
            let volumes = volumes.to_vec();
            pool.handles.push(
                thread::Builder::new()
                    .name(format!("migration-discovery-{n}"))
                    .spawn(move || loop {
                        let next = {
                            let Ok(input) = input.lock() else {
                                return;
                            };
                            input.recv()
                        };
                        let Ok(path) = next else {
                            return;
                        };
                        let result = audit.discover(run, &path, &volumes);
                        if output.send((path, result)).is_err() {
                            return;
                        }
                    })
                    .map_err(error)?,
            );
        }
        drop(results);
        let db = self.connect()?;
        let mut active = 0usize;
        loop {
            self.disk_guard()?;
            if !self.cancel.load(Ordering::Relaxed) {
                while active < workers * 2 {
                    let next:Option<String>=db.query_row("SELECT path FROM dirs WHERE run=?1 AND state='pending' ORDER BY rowid LIMIT 1",[run],|r|r.get(0)).optional().map_err(error)?;
                    let Some(path) = next else {
                        break;
                    };
                    db.execute("UPDATE dirs SET state='working' WHERE path=?1", [&path])
                        .map_err(error)?;
                    db.execute(
                        "UPDATE runs SET phase='discovering',current_path=?2 WHERE id=?1",
                        params![run, path],
                    )
                    .map_err(error)?;
                    pool.jobs.as_ref().unwrap().send(path).map_err(error)?;
                    active += 1;
                }
            }
            if active == 0 {
                break;
            }
            let (path, result) = pool.output.as_ref().unwrap().recv().map_err(error)?;
            active -= 1;
            if let Err(e) = result {
                if e != "Paused" {
                    self.directory_gap(run, &path, &e)?;
                }
            }
        }
        pool.successful = true;
        drop(pool);
        Ok(())
    }
    fn excluded(&self, path: &str) -> bool {
        excluded(path, &self.exclusions)
    }
    fn discover(&self, run: i64, path: &str, volumes: &[Volume]) -> Result<(), String> {
        if self.excluded(path) {
            return self.directory_gap(run,path,"Operational exclusion: active app/journal/runtime/build files are outside this audit; retain them in the eventual external backup.");
        }
        let (p, guards) = filesystem::guard_path(path, volumes, false)?;
        let before = guards
            .last()
            .ok_or("No directory identity handle")?
            .metadata()
            .map_err(error)?;
        if !before.is_dir() {
            return self.directory_gap(
                run,
                path,
                "The review root is not a directory; select its parent to audit loose files.",
            );
        }
        let db = self.connect()?;
        db.execute("UPDATE dirs SET state='working' WHERE path=?1", [path])
            .map_err(error)?;
        let token = chrono::Utc::now()
            .timestamp_nanos_opt()
            .unwrap_or_default()
            .to_string();
        let mut chunk = Vec::with_capacity(CHUNK);
        let mut listing_errors = Vec::new();
        for item in fs::read_dir(&p).map_err(error)? {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("Paused".into());
            }
            match item {
                Ok(item) => {
                    let child = item.path().to_string_lossy().to_string();
                    let observed = observe(&child, path, volumes, self.excluded(&child), true);
                    chunk.push(observed);
                    if chunk.len() == CHUNK {
                        self.store_discovery(run, path, &chunk, &token)?;
                        chunk.clear();
                    }
                }
                Err(e) => {
                    if listing_errors.len() < 20 {
                        listing_errors.push(error(e));
                    }
                }
            }
        }
        self.store_discovery(run, path, &chunk, &token)?;
        let after = guards.last().unwrap().metadata().map_err(error)?;
        if stamp(&before) != stamp(&after) {
            listing_errors.push(
                "Directory changed while being enumerated; absence cannot be established".into(),
            );
        }
        if !listing_errors.is_empty() {
            return self.directory_gap(
                run,
                path,
                &format!("Incomplete enumeration: {}", listing_errors.join("; ")),
            );
        }
        self.finish_directory(run, path, &token)?;
        Ok(())
    }
    fn store_discovery(
        &self,
        run: i64,
        parent: &str,
        items: &[Observation],
        token: &str,
    ) -> Result<(), String> {
        self.disk_guard()?;
        if self.cancel.load(Ordering::Relaxed) {
            return Err("Paused".into());
        }
        if items.is_empty() {
            return Ok(());
        }
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        let mut discovered = 0i64;
        let mut counted = 0i64;
        let mut failed = 0i64;
        let mut logical = 0i64;
        let mut allocated = 0i64;
        let mut unknown = 0i64;
        let mut gaps = 0i64;
        for e in items {
            if e.kind == "folder" || e.kind == "gap-folder" {
                tx.execute("INSERT INTO dirs(path,parent,run,state,changed_at) VALUES(?1,?2,?3,'pending',?4) ON CONFLICT(path) DO UPDATE SET parent=?2,run=?3,state=CASE WHEN run=?3 THEN state ELSE 'pending' END,discovered=CASE WHEN run=?3 THEN discovered ELSE 0 END,counted=CASE WHEN run=?3 THEN counted ELSE 0 END,hashed=CASE WHEN run=?3 THEN hashed ELSE 0 END,failed=CASE WHEN run=?3 THEN failed ELSE 0 END,logical=CASE WHEN run=?3 THEN logical ELSE 0 END,allocated=CASE WHEN run=?3 THEN allocated ELSE 0 END,unknown=CASE WHEN run=?3 THEN unknown ELSE 0 END,gaps=CASE WHEN run=?3 THEN gaps ELSE 0 END",params![e.path,parent,run,now()]).map_err(error)?;
                tx.execute(
                    "UPDATE dirs SET seen_token=?2 WHERE path=?1",
                    params![e.path, token],
                )
                .map_err(error)?;
                if e.kind == "gap-folder" {
                    let previous: String = tx
                        .query_row("SELECT state FROM dirs WHERE path=?1", [&e.path], |r| {
                            r.get(0)
                        })
                        .map_err(error)?;
                    if previous != "gap" {
                        gaps += 1;
                    }
                    tx.execute("UPDATE dirs SET state='gap',problem=?2,observed_at=?3,gaps=1 WHERE path=?1",params![e.path,e.problem,now()]).map_err(error)?;
                }
                continue;
            }
            let prior: Option<(i64, i64, Option<i64>, String, Option<String>, String)> = tx
                .query_row(
                    "SELECT run,logical,allocated,identity,problem,state FROM files WHERE path=?1",
                    [&e.path],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                        ))
                    },
                )
                .optional()
                .map_err(error)?;
            let same = prior.as_ref().is_some_and(|p| p.0 == run);
            if same {
                tx.execute(
                    "UPDATE files SET seen_token=?2 WHERE path=?1",
                    params![e.path, token],
                )
                .map_err(error)?;
                continue;
            }
            discovered += 1;
            if e.kind == "file" {
                counted += 1;
                logical += e.logical;
                allocated += e.allocated.unwrap_or(0);
                if e.allocated.is_none() {
                    unknown += 1;
                }
            }
            let state = if e.problem.is_some() {
                failed += 1;
                gaps += 1;
                "gap"
            } else {
                "pending"
            };
            tx.execute("INSERT INTO files(path,parent,kind,run,state,logical,allocated,identity,mtime,changed_at,first_run,problem) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?4,?11) ON CONFLICT(path) DO UPDATE SET parent=?2,kind=?3,run=?4,state=?5,logical=?6,allocated=?7,identity=?8,mtime=?9,problem=?11,changed_at=CASE WHEN identity<>?8 OR mtime<>?9 OR logical<>?6 OR kind<>?3 THEN ?10 ELSE changed_at END",params![e.path,parent,e.kind,run,state,e.logical,e.allocated,e.identity,e.mtime,now(),e.problem]).map_err(error)?;
            tx.execute(
                "UPDATE files SET seen_token=?2 WHERE path=?1",
                params![e.path, token],
            )
            .map_err(error)?;
        }
        let direct_gaps = items
            .iter()
            .filter(|e| e.kind != "gap-folder" && e.kind != "folder" && e.problem.is_some())
            .count() as i64;
        tx.execute("UPDATE dirs SET discovered=discovered+?2,counted=counted+?3,failed=failed+?4,logical=logical+?5,allocated=allocated+?6,unknown=unknown+?7,gaps=gaps+?8 WHERE path=?1",params![parent,discovered,counted,failed,logical,allocated,unknown,direct_gaps]).map_err(error)?;
        tx.execute("UPDATE runs SET discovered=discovered+?2,counted=counted+?3,failed=failed+?4,logical=logical+?5,allocated=allocated+?6,unknown=unknown+?7,gaps=gaps+?8 WHERE id=?1",params![run,discovered,counted,failed,logical,allocated,unknown,gaps]).map_err(error)?;
        touch(&tx)?;
        tx.commit().map_err(error)
    }
    fn finish_directory(&self, run: i64, path: &str, token: &str) -> Result<(), String> {
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        let at = now();
        let missing: i64 = tx
            .query_row(
                "SELECT count(*) FROM files WHERE parent=?1 AND coalesce(seen_token,'')<>?2 AND state<>'missing'",
                params![path, token],
                |r| r.get(0),
            )
            .map_err(error)?;
        tx.execute("UPDATE files SET state='missing',problem='Absent from a complete directory enumeration',changed_at=?3 WHERE parent=?1 AND coalesce(seen_token,'')<>?2 AND state<>'missing'",params![path,token,at]).map_err(error)?;
        let mut removed = Vec::new();
        {
            let mut q = tx
                .prepare("SELECT path FROM dirs WHERE parent=?1 AND coalesce(seen_token,'')<>?2 AND state<>'missing'")
                .map_err(error)?;
            let rows = q
                .query_map(params![path, token], |r| r.get::<_, String>(0))
                .map_err(error)?;
            for row in rows {
                removed.push(row.map_err(error)?);
            }
        }
        for absent in &removed {
            let (low, high) = prefix_range(absent);
            tx.execute("UPDATE dirs SET state='missing',problem='Directory absent from complete parent enumeration',changed_at=?3 WHERE path=?1 OR (path>=?2 COLLATE NOCASE AND path<?4 COLLATE NOCASE)",params![absent,low,at,high]).map_err(error)?;
            tx.execute("UPDATE files SET state='missing',problem='Ancestor absent from complete parent enumeration',changed_at=?3 WHERE path>=?1 COLLATE NOCASE AND path<?2 COLLATE NOCASE",params![low,high,at]).map_err(error)?;
        }
        tx.execute(
            "UPDATE dirs SET state='done',problem=NULL,observed_at=?2 WHERE path=?1",
            params![path, at],
        )
        .map_err(error)?;
        let old:[i64;8]=tx.query_row("SELECT discovered,counted,hashed,failed,logical,allocated,unknown,gaps FROM dirs WHERE path=?1",[path],|r|Ok([r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?])).map_err(error)?;
        let counts:[i64;8]=tx.query_row("SELECT count(*),coalesce(sum(kind='file'),0),coalesce(sum(state='hashed'),0),coalesce(sum(state='gap'),0),coalesce(sum(CASE WHEN kind='file' THEN logical ELSE 0 END),0),coalesce(sum(CASE WHEN kind='file' THEN coalesce(allocated,0) ELSE 0 END),0),coalesce(sum(kind='file' AND allocated IS NULL),0),coalesce(sum(state='gap'),0) FROM files WHERE parent=?1 AND run=?2 AND state<>'missing'",params![path,run],|r|Ok([r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?])).map_err(error)?;
        let mut delta = [0i64; 8];
        for i in 0..8 {
            delta[i] = counts[i] - old[i];
        }
        tx.execute("UPDATE dirs SET discovered=?2,counted=?3,hashed=?4,failed=?5,logical=?6,allocated=?7,unknown=?8,gaps=?9 WHERE path=?1",params![path,counts[0],counts[1],counts[2],counts[3],counts[4],counts[5],counts[6],counts[7]]).map_err(error)?;
        tx.execute("UPDATE runs SET discovered=discovered+?2,counted=counted+?3,hashed=hashed+?4,failed=failed+?5,logical=logical+?6,allocated=allocated+?7,unknown=unknown+?8,gaps=gaps+?9 WHERE id=?1",params![run,delta[0],delta[1],delta[2],delta[3],delta[4],delta[5],delta[6],delta[7]]).map_err(error)?;
        touch(&tx)?;
        let baseline: bool = tx
            .query_row("SELECT baseline FROM runs WHERE id=?1", [run], |r| r.get(0))
            .map_err(error)?;
        tx.commit().map_err(error)?;
        if !baseline && missing + removed.len() as i64 > 0 {
            self.notify(&format!("removed|{run}|{path}"),"verified-change","Source entries removed or moved", "Entries were absent from a complete stable parent enumeration. Their previous checksums remain retained in the audit baseline.",path,missing+removed.len() as i64)?;
        }
        Ok(())
    }
    fn directory_gap(&self, run: i64, path: &str, problem: &str) -> Result<(), String> {
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        let prior: Option<String> = tx
            .query_row("SELECT state FROM dirs WHERE path=?1", [path], |r| r.get(0))
            .optional()
            .map_err(error)?;
        tx.execute("INSERT INTO dirs(path,parent,run,state,problem,observed_at,changed_at,gaps) VALUES(?1,'',?2,'gap',?3,?4,?4,1) ON CONFLICT(path) DO UPDATE SET state='gap',problem=?3,observed_at=?4,changed_at=CASE WHEN coalesce(problem,'')<>?3 THEN ?4 ELSE changed_at END,gaps=CASE WHEN state='gap' THEN gaps ELSE gaps+1 END",params![path,run,problem,now()]).map_err(error)?;
        if prior.as_deref() != Some("gap") {
            tx.execute("UPDATE runs SET gaps=gaps+1 WHERE id=?1", [run])
                .map_err(error)?;
        }
        tx.commit().map_err(error)
    }
    fn hash_pending(
        self: &Arc<Self>,
        run: i64,
        volumes: &[Volume],
        workers: usize,
    ) -> Result<(), String> {
        let (jobs, receiver) = mpsc::sync_channel::<HashJob>(workers * 2);
        let receiver = Arc::new(Mutex::new(receiver));
        let (results, output) = mpsc::sync_channel::<HashResult>(workers * 2);
        let mut pool = WorkerPool {
            jobs: Some(jobs),
            output: Some(output),
            handles: Vec::new(),
            cancel: &self.cancel,
            successful: false,
        };
        for n in 0..workers {
            let input = receiver.clone();
            let output = results.clone();
            let audit = self.clone();
            let volumes = volumes.to_vec();
            pool.handles.push(
                thread::Builder::new()
                    .name(format!("migration-sha256-{n}"))
                    .spawn(move || loop {
                        let job = {
                            let Ok(input) = input.lock() else {
                                return;
                            };
                            input.recv()
                        };
                        let Ok(job) = job else {
                            return;
                        };
                        let result = hash_file(job, &volumes, &audit.cancel);
                        if output.send(result).is_err() {
                            return;
                        }
                    })
                    .map_err(error)?,
            );
        }
        drop(results);
        let mut active = 0usize;
        let mut db = self.connect()?;
        loop {
            self.disk_guard()?;
            if !self.cancel.load(Ordering::Relaxed) {
                let claimed = {
                    let tx = db
                        .transaction_with_behavior(TransactionBehavior::Immediate)
                        .map_err(error)?;
                    let mut jobs_to_send = Vec::with_capacity(workers * 2 - active);
                    {
                        let mut q=tx.prepare("SELECT id,path FROM files WHERE run=?1 AND state='pending' ORDER BY id LIMIT ?2").map_err(error)?;
                        for row in q
                            .query_map(params![run, (workers * 2 - active) as i64], |r| {
                                Ok(HashJob {
                                    id: r.get(0)?,
                                    path: r.get(1)?,
                                })
                            })
                            .map_err(error)?
                        {
                            jobs_to_send.push(row.map_err(error)?);
                        }
                    }
                    for job in &jobs_to_send {
                        tx.execute("UPDATE files SET state='working' WHERE id=?1", [job.id])
                            .map_err(error)?;
                    }
                    if let Some(last) = jobs_to_send.last() {
                        tx.execute(
                            "UPDATE runs SET current_path=?2 WHERE id=?1",
                            params![run, last.path],
                        )
                        .map_err(error)?;
                    }
                    tx.commit().map_err(error)?;
                    jobs_to_send
                };
                for job in claimed {
                    pool.jobs.as_ref().unwrap().send(job).map_err(error)?;
                    active += 1;
                }
            }
            if active == 0 {
                break;
            }
            let first = pool.output.as_ref().unwrap().recv().map_err(error)?;
            active -= 1;
            let mut batch = vec![first];
            while let Ok(result) = pool.output.as_ref().unwrap().try_recv() {
                active -= 1;
                batch.push(result);
            }
            self.store_hashes(run, &batch)?;
        }
        pool.successful = true;
        drop(pool);
        Ok(())
    }
    fn store_hashes(&self, run: i64, results: &[HashResult]) -> Result<(), String> {
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        let at = now();
        let baseline: bool = tx
            .query_row("SELECT baseline FROM runs WHERE id=?1", [run], |r| r.get(0))
            .map_err(error)?;
        let mut per_parent = HashMap::<String, [i64; 7]>::new();
        let mut changes = HashMap::<String, i64>::new();
        for result in results {
            if result.cancelled {
                tx.execute("UPDATE files SET state='pending' WHERE id=?1", [result.id])
                    .map_err(error)?;
                continue;
            }
            let old = tx
                .query_row(
                    "SELECT parent,digest,logical,allocated,first_run,identity,mtime FROM files WHERE id=?1",
                    [result.id],
                    |r| Ok(HashBaseline {
                        parent: r.get(0)?,
                        digest: r.get(1)?,
                        logical: r.get(2)?,
                        allocated: r.get(3)?,
                        first_run: r.get(4)?,
                        identity: r.get(5)?,
                        mtime: r.get(6)?,
                    }),
                )
                .map_err(error)?;
            let values = per_parent.entry(old.parent.clone()).or_default();
            if let Some(digest) = &result.digest {
                // Discovery already records additions. A first stable digest enriches
                // that observation; only changed content or metadata advances staleness.
                let changed = old.digest.as_ref().is_some_and(|old| old != digest)
                    || old.identity != result.identity
                    || old.mtime != result.mtime
                    || old.logical != result.logical;
                let newly_observed = old.digest.is_none() && old.first_run == run;
                values[0] += 1;
                values[2] += result.logical - old.logical;
                values[3] += result.allocated.unwrap_or(0) - old.allocated.unwrap_or(0);
                values[4] += (result.allocated.is_none() as i64) - (old.allocated.is_none() as i64);
                tx.execute("UPDATE files SET state='hashed',digest=?2,verified_at=?3,problem=NULL,logical=?4,allocated=?5,identity=?6,mtime=?7,changed_at=CASE WHEN ?8 THEN ?3 ELSE changed_at END WHERE id=?1",params![result.id,digest,at,result.logical,result.allocated,result.identity,result.mtime,changed]).map_err(error)?;
                if changed {
                    tx.execute(
                        "UPDATE dirs SET changed_at=?2 WHERE path=?1",
                        params![old.parent, at],
                    )
                    .map_err(error)?;
                }
                if (changed || newly_observed) && !baseline {
                    *changes.entry(old.parent).or_default() += 1;
                }
            } else {
                values[1] += 1;
                values[5] += 1;
                tx.execute(
                    "UPDATE files SET state='gap',problem=?2,changed_at=?3 WHERE id=?1",
                    params![result.id, result.problem, at],
                )
                .map_err(error)?;
            }
        }
        let mut total = [0i64; 7];
        for (parent, v) in &per_parent {
            for (i, n) in v.iter().enumerate() {
                total[i] += n;
            }
            tx.execute("UPDATE dirs SET hashed=hashed+?2,failed=failed+?3,logical=logical+?4,allocated=allocated+?5,unknown=unknown+?6,gaps=gaps+?7 WHERE path=?1",params![parent,v[0],v[1],v[2],v[3],v[4],v[5]]).map_err(error)?;
        }
        tx.execute("UPDATE runs SET hashed=hashed+?2,failed=failed+?3,logical=logical+?4,allocated=allocated+?5,unknown=unknown+?6,gaps=gaps+?7 WHERE id=?1",params![run,total[0],total[1],total[2],total[3],total[4],total[5]]).map_err(error)?;
        touch(&tx)?;
        tx.commit().map_err(error)?;
        for (parent, count) in changes {
            self.notify(&format!("changed|{run}|{parent}"),"verified-change","Source content changed or was newly observed","Full stable SHA-256 reads differ from the previous recorded baseline, or files were newly observed. First observation is not proof of when a file was created. Review prior migration decisions; no backup verification is implied.",&parent,count)?;
        }
        Ok(())
    }
    pub fn status(&self) -> Result<Value, String> {
        let db = self.connect()?;
        let row:Option<Value>=db.query_row("SELECT id,phase,started_at,completed_at,workers,current_path,error,discovered,counted,hashed,failed,logical,allocated,unknown,gaps,roots FROM runs ORDER BY id DESC LIMIT 1",[],|r|Ok(json!({"runId":r.get::<_,i64>(0)?,"phase":r.get::<_,String>(1)?,"startedAt":r.get::<_,Option<String>>(2)?,"completedAt":r.get::<_,Option<String>>(3)?,"workers":r.get::<_,i64>(4)?,"currentPath":r.get::<_,Option<String>>(5)?,"error":r.get::<_,Option<String>>(6)?,"discoveredLeaves":r.get::<_,i64>(7)?,"countedLeaves":r.get::<_,i64>(8)?,"hashedLeaves":r.get::<_,i64>(9)?,"failedLeaves":r.get::<_,i64>(10)?,"logicalBytes":r.get::<_,i64>(11)?,"allocatedBytes":r.get::<_,i64>(12)?,"allocationUnknown":r.get::<_,i64>(13)?,"coverageGaps":r.get::<_,i64>(14)?,"roots":serde_json::from_str::<Value>(&r.get::<_,String>(15)?).unwrap_or(Value::Null)}))).optional().map_err(error)?;
        let mut value=row.unwrap_or_else(||json!({"phase":"idle","discoveredLeaves":0,"countedLeaves":0,"hashedLeaves":0,"failedLeaves":0,"logicalBytes":0,"allocatedBytes":0,"allocationUnknown":0,"coverageGaps":0,"workers":4,"currentPath":null,"startedAt":null,"completedAt":null}));
        let phase = value["phase"].as_str().unwrap_or("idle").to_string();
        let watcher_gaps:i64=db.query_row("SELECT count(*) FROM watch_changes WHERE kind='coverage-gap' AND changed_at>coalesce((SELECT started_at FROM runs ORDER BY id DESC LIMIT 1),'')",[],|r|r.get(0)).map_err(error)?;
        value["sourceCoverageGaps"] = value["coverageGaps"].clone();
        value["watcherCoverageGaps"] = json!(watcher_gaps);
        value["coverageGaps"] = json!(value["coverageGaps"].as_i64().unwrap_or(0) + watcher_gaps);
        value["reconciliationRequired"] = json!(watcher_gaps > 0);
        if watcher_gaps > 0 {
            value["problem"]=json!("An unobserved live-watcher interval requires a fresh full audit of the affected scope. Resuming saved work alone does not refresh previously completed trees.");
        }

        value["totalKnown"]=json!(matches!(phase.as_str(),"hashing"|"complete") || phase=="paused" && db.query_row("SELECT count(*) FROM dirs WHERE run=(SELECT max(id) FROM runs) AND state IN('pending','working')",[],|r|r.get::<_,i64>(0)).map_err(error)?==0);
        value["complete"] =
            json!(phase == "complete" && value["coverageGaps"] == 0 && value["failedLeaves"] == 0);
        value["notificationsUnread"] = json!(db
            .query_row(
                "SELECT count(*) FROM notifications WHERE read_at IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .map_err(error)?);
        value["updatedAt"] = json!(db
            .query_row("SELECT value FROM meta WHERE key='updatedAt'", [], |r| {
                r.get::<_, String>(0)
            })
            .map_err(error)?);
        value["changeSequence"] = json!(sequence(&db)?);
        let mut samples = Vec::new();
        {
            let mut q=db.prepare("SELECT path,problem,'folder',observed_at FROM dirs WHERE run=(SELECT max(id) FROM runs) AND state='gap' LIMIT 50").map_err(error)?;
            for row in q.query_map([],|r|Ok(json!({"path":r.get::<_,String>(0)?,"problem":r.get::<_,Option<String>>(1)?,"kind":r.get::<_,String>(2)?,"observedAt":r.get::<_,Option<String>>(3)?}))).map_err(error)?{samples.push(row.map_err(error)?);}
        }
        if samples.len() < 50 {
            let mut q=db.prepare("SELECT path,problem,kind,changed_at FROM files WHERE run=(SELECT max(id) FROM runs) AND state='gap' LIMIT ?1").map_err(error)?;
            for row in q.query_map([(50-samples.len()) as i64],|r|Ok(json!({"path":r.get::<_,String>(0)?,"problem":r.get::<_,Option<String>>(1)?,"kind":r.get::<_,String>(2)?,"observedAt":r.get::<_,Option<String>>(3)?}))).map_err(error)?{samples.push(row.map_err(error)?);}
        }
        value["gapSamples"] = json!(samples);
        value["scopeNotes"] = json!(SCOPE_NOTE);
        value["exclusions"] = json!(self.exclusions);
        value["watcherVerification"]=json!("Filesystem events signal pending changes; only a full source audit establishes recorded checksums.");
        Ok(value)
    }
    pub fn stats(&self, paths: &[String]) -> Result<Value, String> {
        let mut map = serde_json::Map::new();
        for path in paths.iter().take(500) {
            map.insert(path.clone(), self.stats_for(path)?);
        }
        Ok(Value::Object(map))
    }
    pub fn stats_for(&self, path: &str) -> Result<Value, String> {
        let volumes = self.volumes()?;
        let path = filesystem::normalize(path)?;
        filesystem::identify(&path, &volumes)?;
        let db = self.connect()?;
        let live = fs::symlink_metadata(&path);
        let missing_file = live.is_err()
            && db
                .query_row("SELECT 1 FROM files WHERE path=?1", [&path], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()
                .map_err(error)?
                .is_some();
        if live.as_ref().is_ok_and(|m| !m.is_dir()) || missing_file {
            let e = observe(
                &path,
                &parent_path(&path),
                &volumes,
                self.excluded(&path),
                false,
            );
            let saved:Option<(String,Option<String>,Option<String>,String,String,Option<String>)>=db.query_row("SELECT state,digest,verified_at,identity,mtime,problem FROM files WHERE path=?1",[&path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(error)?;
            let watched = saved
                .as_ref()
                .and_then(|v| v.2.as_deref())
                .map(|at| self.watch_changed(&path, at))
                .transpose()?
                .unwrap_or(false);
            let valid = !watched
                && saved.as_ref().is_some_and(|v| {
                    v.0 == "hashed" && v.3 == e.identity && v.4 == e.mtime && e.problem.is_none()
                });
            return Ok(
                json!({"path":path,"kind":e.kind,"discoveredLeaves":1,"countedLeaves":(e.kind=="file") as i32,"hashedLeaves":valid as i32,"failedLeaves":e.problem.is_some() as i32,"logicalBytes":e.logical,"allocatedBytes":e.allocated.unwrap_or(0),"allocationUnknown":e.allocated.is_none() as i32,"totalKnown":true,"complete":valid,"checksum":saved.as_ref().and_then(|v|v.1.clone()),"sha256":saved.as_ref().and_then(|v|v.1.clone()),"verifiedAt":saved.as_ref().and_then(|v|v.2.clone()),"cached":!valid && saved.as_ref().is_some_and(|v|v.1.is_some()),"phase":if valid{"complete"}else if e.problem.is_some(){"error"}else{"idle"},"hashState":if valid{"verified"}else if e.problem.is_some(){"gap"}else{"pending"},"problem":e.problem,"scopeNotes":SCOPE_NOTE}),
            );
        }
        let run: Option<i64> = db
            .query_row("SELECT run FROM dirs WHERE path=?1", [&path], |r| r.get(0))
            .optional()
            .map_err(error)?;
        let Some(run) = run else {
            return Ok(
                json!({"path":path,"kind":"folder","discoveredLeaves":0,"countedLeaves":0,"hashedLeaves":0,"failedLeaves":0,"logicalBytes":0,"allocatedBytes":0,"allocationUnknown":0,"coverageGaps":0,"totalKnown":false,"complete":false,"phase":"idle","hashState":"not-scanned","scopeNotes":SCOPE_NOTE}),
            );
        };
        let epoch = sequence(&db)?;
        if let Some((seen, cached)) = self.stats_cache.lock().map_err(error)?.get(&path) {
            if *seen == epoch {
                return Ok(cached.clone());
            }
        }
        let (low, high) = prefix_range(&path);
        let mut value=db.query_row("SELECT coalesce(sum(discovered),0),coalesce(sum(counted),0),coalesce(sum(hashed),0),coalesce(sum(failed),0),coalesce(sum(logical),0),coalesce(sum(allocated),0),coalesce(sum(unknown),0),coalesce(sum(gaps),0),sum(CASE WHEN state IN('pending','working') THEN 1 ELSE 0 END),min(observed_at),max(changed_at) FROM dirs WHERE state<>'missing' AND (path=?1 OR (path>=?2 COLLATE NOCASE AND path<?3 COLLATE NOCASE))",params![path,low,high],|r|Ok(json!({"path":path,"kind":"folder","discoveredLeaves":r.get::<_,i64>(0)?,"countedLeaves":r.get::<_,i64>(1)?,"hashedLeaves":r.get::<_,i64>(2)?,"failedLeaves":r.get::<_,i64>(3)?,"logicalBytes":r.get::<_,i64>(4)?,"allocatedBytes":r.get::<_,i64>(5)?,"allocationUnknown":r.get::<_,i64>(6)?,"coverageGaps":r.get::<_,i64>(7)?,"totalKnown":r.get::<_,Option<i64>>(8)?.unwrap_or(0)==0,"observedAt":r.get::<_,Option<String>>(9)?,"lastSourceChangeAt":r.get::<_,Option<String>>(10)?}))).map_err(error)?;
        value["complete"] = json!(
            value["totalKnown"] == true
                && value["hashedLeaves"].as_i64().unwrap_or(0)
                    + value["failedLeaves"].as_i64().unwrap_or(0)
                    == value["discoveredLeaves"].as_i64().unwrap_or(0)
                && value["coverageGaps"] == 0
                && value["failedLeaves"] == 0
        );
        value["phase"] = json!(db
            .query_row("SELECT phase FROM runs WHERE id=?1", [run], |r| r
                .get::<_, String>(0))
            .map_err(error)?);
        let pending_change = value["observedAt"]
            .as_str()
            .map(|at| self.watch_changed(&path, at))
            .transpose()?
            .unwrap_or(false);
        if value["coverageGaps"].as_i64().unwrap_or(0) > 0 || pending_change || live.is_err() {
            value["problem"] = json!(if live.is_err() {
                "Source path is missing or inaccessible; previous audit results are retained"
            } else if pending_change {
                "Source changes pending a fresh count/checksum pass"
            } else {
                "Recursive coverage has unreadable, excluded, unstable or reparse locations; totals are partial"
            });
            value["totalKnown"] = json!(false);
            value["complete"] = json!(false);
            value["cached"] = json!(true);
        }
        value["hashState"] = json!(if value["complete"] == true {
            "verified"
        } else {
            "partial"
        });
        value["scopeNotes"] = json!(SCOPE_NOTE);
        {
            let mut cache = self.stats_cache.lock().map_err(error)?;
            if cache.len() > 1000 {
                cache.clear();
            }
            cache.insert(path.clone(), (epoch, value.clone()));
        }
        Ok(value)
    }
    pub fn changed_below(&self, path: &str, since: &str) -> Result<bool, String> {
        let db = self.connect()?;
        let (low, high) = prefix_range(path);
        for table in ["files", "dirs", "watch_changes"] {
            let sql=format!("SELECT 1 FROM {table} WHERE changed_at>?1 AND (path=?2 OR (path>=?3 COLLATE NOCASE AND path<?4 COLLATE NOCASE)) LIMIT 1");
            if db
                .query_row(&sql, params![since, path, low, high], |r| {
                    r.get::<_, i64>(0)
                })
                .optional()
                .map_err(error)?
                .is_some()
            {
                return Ok(true);
            }
        }
        self.watch_changed(path, since)
    }
    pub fn notifications(&self, limit: usize, offset: usize) -> Result<Value, String> {
        let db = self.connect()?;
        let mut q=db.prepare("SELECT id,kind,title,message,path,created_at,read_at,change_count FROM notifications ORDER BY created_at DESC,id DESC LIMIT ?1 OFFSET ?2").map_err(error)?;
        let rows=q.query_map(params![limit.clamp(1,100) as i64,offset.min(10000) as i64],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"kind":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"message":r.get::<_,String>(3)?,"path":r.get::<_,String>(4)?,"createdAt":r.get::<_,String>(5)?,"readAt":r.get::<_,Option<String>>(6)?,"changeCount":r.get::<_,i64>(7)?}))).map_err(error)?;
        let items = rows.collect::<Result<Vec<_>, _>>().map_err(error)?;
        Ok(
            json!({"items":items,"total":db.query_row("SELECT count(*) FROM notifications",[],|r|r.get::<_,i64>(0)).map_err(error)?,"unread":db.query_row("SELECT count(*) FROM notifications WHERE read_at IS NULL",[],|r|r.get::<_,i64>(0)).map_err(error)?}),
        )
    }
    pub fn mark_read(&self, ids: &[i64], all: bool) -> Result<(), String> {
        let mut db = self.connect()?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(error)?;
        if all {
            tx.execute(
                "UPDATE notifications SET read_at=?1 WHERE read_at IS NULL",
                [now()],
            )
            .map_err(error)?;
        } else {
            for id in ids.iter().take(1000) {
                tx.execute(
                    "UPDATE notifications SET read_at=?2 WHERE id=?1 AND read_at IS NULL",
                    params![id, now()],
                )
                .map_err(error)?;
            }
        }
        touch(&tx)?;
        tx.commit().map_err(error)
    }
    fn notify(
        &self,
        key: &str,
        kind: &str,
        title: &str,
        message: &str,
        path: &str,
        count: i64,
    ) -> Result<(), String> {
        self.connect()?.execute("INSERT INTO notifications(event_key,kind,title,message,path,created_at,change_count) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(event_key) DO UPDATE SET message=?4,created_at=?6,read_at=NULL,change_count=change_count+?7",params![key,kind,title,message,path,now(),count]).map_err(error)?;
        Ok(())
    }
    pub fn record_watch_change(&self, path: &str, kind: &str) -> Result<(), String> {
        if self.excluded(path) {
            return Ok(());
        }
        let volumes = self.volumes()?;
        filesystem::identify(path, &volumes)?;
        self.connect()?.execute("INSERT INTO watch_changes VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET changed_at=?2,kind=?3",params![path,now(),kind]).map_err(error)?;
        touch(&self.connect()?)?;
        let parent = parent_path(path);
        self.notify(&format!("watch|{parent}"),"source-change","Source changes need checksum verification","Filesystem change events were observed. These are pending verification, not proof of changed content or a completed backup.",&parent,1)
    }
    pub fn record_watch_gap(&self, root: &str, reason: &str) -> Result<(), String> {
        let roots = if root.is_empty() {
            self.volumes()?
                .into_iter()
                .map(|v| v.root)
                .collect::<Vec<_>>()
        } else {
            vec![root.to_string()]
        };
        let db = self.connect()?;
        for path in &roots {
            db.execute("INSERT INTO watch_changes VALUES(?1,?2,'coverage-gap') ON CONFLICT(path) DO UPDATE SET changed_at=?2,kind='coverage-gap'",params![path,now()]).map_err(error)?;
            self.notify(
                &format!("watch-gap|{path}"),
                "coverage-gap",
                "Live change coverage needs a rescan",
                reason,
                path,
                1,
            )?;
        }
        touch(&db)?;
        Ok(())
    }
    fn watch_changed(&self, path: &str, since: &str) -> Result<bool, String> {
        let db = self.connect()?;
        let (low, high) = prefix_range(path);
        if db.query_row("SELECT 1 FROM watch_changes WHERE changed_at>?1 AND (path=?2 OR (path>=?3 COLLATE NOCASE AND path<?4 COLLATE NOCASE)) LIMIT 1",params![since,path,low,high],|r|r.get::<_,i64>(0)).optional().map_err(error)?.is_some(){return Ok(true);}
        let mut q = db
            .prepare("SELECT path FROM watch_changes WHERE changed_at>?1 AND kind='coverage-gap'")
            .map_err(error)?;
        for row in q
            .query_map([since], |r| r.get::<_, String>(0))
            .map_err(error)?
        {
            if filesystem::under(path, &row.map_err(error)?) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn sequence(db: &Connection) -> Result<i64, String> {
    db.query_row(
        "SELECT cast(value AS INTEGER) FROM meta WHERE key='changeSequence'",
        [],
        |r| r.get(0),
    )
    .map_err(error)
}
fn touch(db: &Connection) -> Result<(), String> {
    db.execute(
        "UPDATE meta SET value=cast(value AS INTEGER)+1 WHERE key='changeSequence'",
        [],
    )
    .map_err(error)?;
    db.execute("UPDATE meta SET value=?1 WHERE key='updatedAt'", [now()])
        .map_err(error)?;
    Ok(())
}

fn prefix_range(path: &str) -> (String, String) {
    let low = format!("{}\\", path.trim_end_matches('\\'));
    let high = format!("{}\u{10ffff}", low);
    (low, high)
}
fn parent_path(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}
fn excluded(path: &str, exclusions: &[String]) -> bool {
    let path = path.to_lowercase();
    exclusions.iter().any(|p| {
        let p = p.to_lowercase();
        if let Some(prefix) = p.strip_suffix('*') {
            path.starts_with(prefix)
        } else {
            filesystem::under(&path, &p)
        }
    })
}
fn stamp(metadata: &fs::Metadata) -> String {
    use std::os::windows::fs::MetadataExt;
    format!(
        "{}|{}|{}",
        metadata.len(),
        metadata.last_write_time(),
        metadata.creation_time()
    )
}
fn handle_identity(file: &File) -> Result<String, String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdInfo, GetFileInformationByHandleEx, FILE_ID_INFO,
    };
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(format!(
            "Cannot read stable file identity: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(format!(
        "{:016x}-{:02x?}",
        info.VolumeSerialNumber, info.FileId.Identifier
    ))
}
fn allocation(file: &File, metadata: &fs::Metadata) -> Option<i64> {
    use std::os::windows::{fs::MetadataExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileCompressionInfo, FileStandardInfo, GetFileInformationByHandleEx, FILE_COMPRESSION_INFO,
        FILE_STANDARD_INFO,
    };
    unsafe {
        if metadata.file_attributes() & (0x200 | 0x800) != 0 {
            let mut info: FILE_COMPRESSION_INFO = std::mem::zeroed();
            if GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileCompressionInfo,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<FILE_COMPRESSION_INFO>() as u32,
            ) != 0
                && info.CompressedFileSize >= 0
            {
                return Some(info.CompressedFileSize);
            }
        }
        let mut info: FILE_STANDARD_INFO = std::mem::zeroed();
        if GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        ) != 0
            && info.AllocationSize >= 0
        {
            Some(info.AllocationSize)
        } else {
            None
        }
    }
}
fn open_file(path: &Path, read: bool) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN,
        FILE_GENERIC_READ, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    fs::OpenOptions::new()
        .access_mode(if read {
            FILE_GENERIC_READ
        } else {
            FILE_READ_ATTRIBUTES
        })
        .share_mode(if read {
            FILE_SHARE_READ
        } else {
            FILE_SHARE_READ | FILE_SHARE_WRITE
        })
        .custom_flags(
            FILE_FLAG_OPEN_REPARSE_POINT
                | FILE_FLAG_OPEN_NO_RECALL
                | if read { FILE_FLAG_SEQUENTIAL_SCAN } else { 0 },
        )
        .open(path)
        .map_err(error)
}
fn observe(
    path: &str,
    _parent: &str,
    volumes: &[Volume],
    exclude: bool,
    parent_guarded: bool,
) -> Observation {
    let mut e = Observation {
        path: path.into(),
        kind: "unavailable".into(),
        logical: 0,
        allocated: None,
        identity: String::new(),
        mtime: String::new(),
        problem: None,
    };
    let result = (|| {
        use std::os::windows::fs::MetadataExt;
        let initial = fs::symlink_metadata(path).map_err(error)?;
        if exclude {
            e.kind = if initial.is_dir() {
                "gap-folder"
            } else {
                "excluded"
            }
            .into();
            return Err("Operational scope exclusion: active app/journal/runtime/build files were not audited".into());
        }
        if filesystem::reparse(&initial) {
            e.kind = if initial.is_dir() {
                "gap-folder"
            } else {
                "link"
            }
            .into();
            return Err("Reparse point or cloud placeholder not followed; target and hydration coverage unknown".into());
        }
        if initial.is_dir() {
            e.kind = "folder".into();
            return Ok(());
        }
        e.kind = "file".into();
        e.logical = i64::try_from(initial.len()).map_err(error)?;
        if initial.file_attributes() & (0x1000 | 0x400000 | 0x40000) != 0 {
            return Err("Offline/recall source skipped to avoid triggering cloud hydration".into());
        }
        let (p, _guards) = if parent_guarded {
            (PathBuf::from(path), Vec::new())
        } else {
            filesystem::guard_path(path, volumes, false)?
        };
        let file = open_file(&p, false)?;
        let m = file.metadata().map_err(error)?;
        if filesystem::reparse(&m) || !m.is_file() {
            return Err("Source type changed while its metadata was opened".into());
        }
        e.logical = i64::try_from(m.len()).map_err(error)?;
        e.identity = handle_identity(&file)?;
        e.mtime = stamp(&m);
        e.allocated = allocation(&file, &m);
        Ok::<_, String>(())
    })();
    if let Err(problem) = result {
        e.problem = Some(problem);
    }
    e
}
fn hash_file(job: HashJob, volumes: &[Volume], cancel: &AtomicBool) -> HashResult {
    let mut result = HashResult {
        id: job.id,
        digest: None,
        logical: 0,
        allocated: None,
        identity: String::new(),
        mtime: String::new(),
        problem: None,
        cancelled: false,
    };
    let outcome = (|| {
        if cancel.load(Ordering::Relaxed) {
            return Err("Paused".into());
        }
        let (p, _guards) = filesystem::guard_path(&job.path, volumes, false)?;
        let mut file = open_file(&p, true)?;
        let before = file.metadata().map_err(error)?;
        use std::os::windows::fs::MetadataExt;
        if !before.is_file()
            || filesystem::reparse(&before)
            || before.file_attributes() & (0x1000 | 0x400000 | 0x40000) != 0
        {
            return Err("Not an eligible locally available regular source file".into());
        }
        let identity = handle_identity(&file)?;
        let before_stamp = stamp(&before);
        let mut sha = Sha256::new();
        let mut buffer = vec![0u8; BUFFER];
        let mut bytes = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("Paused".into());
            }
            let n = file.read(&mut buffer).map_err(error)?;
            if n == 0 {
                break;
            }
            bytes = bytes
                .checked_add(n as u64)
                .ok_or("File read exceeded supported size")?;
            sha.update(&buffer[..n]);
        }
        let after = file.metadata().map_err(error)?;
        if identity != handle_identity(&file)?
            || before_stamp != stamp(&after)
            || bytes != before.len()
        {
            return Err("Source identity, size or modification time changed during hashing; digest rejected".into());
        }
        result.logical = i64::try_from(after.len()).map_err(error)?;
        result.allocated = allocation(&file, &after);
        result.identity = identity;
        result.mtime = before_stamp;
        result.digest = Some(format!("{:x}", sha.finalize()));
        Ok::<_, String>(())
    })();
    if let Err(e) = outcome {
        if e == "Paused" {
            result.cancelled = true;
        } else {
            result.problem = Some(e);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Arc<Audit>, PathBuf, Vec<Volume>) {
        let root = std::env::temp_dir().join(format!(
            "migration-audit-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let source = root.join("source");
        fs::create_dir_all(source.join("nested")).unwrap();
        let v = filesystem::volumes(Some(&source));
        let a = Audit::open(root.join("audit.sqlite"), "fixture".into(), vec![]).unwrap();
        a.set_volumes(v.clone()).unwrap();
        (a, source, v)
    }
    fn wait(a: &Audit) {
        for _ in 0..1000 {
            if !a.is_running() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("audit did not finish");
    }

    const OBSERVED_BEFORE_CONFIRMATION: &str = "2000-01-01T00:00:00Z";
    const CONFIRMED_AFTER_DISCOVERY: &str = "2001-01-01T00:00:00Z";

    struct PendingHashFixture {
        audit: Arc<Audit>,
        file: PathBuf,
        volumes: Vec<Volume>,
        run: i64,
        id: i64,
    }

    impl PendingHashFixture {
        fn hash(&self) -> HashResult {
            hash_file(
                HashJob {
                    id: self.id,
                    path: self.file.to_string_lossy().into_owned(),
                },
                &self.volumes,
                &AtomicBool::new(false),
            )
        }

        fn current_since_confirmation(&self) -> bool {
            !self
                .audit
                .changed_below(&self.file.to_string_lossy(), CONFIRMED_AFTER_DISCOVERY)
                .unwrap()
        }
    }

    fn pending_hash_fixture() -> PendingHashFixture {
        let (audit, parent, volumes) = fixture();
        let file = parent.join("first.txt");
        fs::write(&file, b"abc").unwrap();
        let db = audit.connect().unwrap();
        db.execute(
            "INSERT INTO runs(phase,roots,started_at,workers,baseline) VALUES('hashing',?1,?2,1,0)",
            params![
                json!([parent.to_string_lossy()]).to_string(),
                OBSERVED_BEFORE_CONFIRMATION
            ],
        )
        .unwrap();
        let run = db.last_insert_rowid();
        db.execute(
            "INSERT INTO dirs(path,parent,run,state,changed_at) VALUES(?1,'',?2,'done',?3)",
            params![parent.to_string_lossy(), run, OBSERVED_BEFORE_CONFIRMATION],
        )
        .unwrap();
        let observation = observe(
            &file.to_string_lossy(),
            &parent.to_string_lossy(),
            &volumes,
            false,
            false,
        );
        assert!(observation.problem.is_none());
        audit
            .store_discovery(run, &parent.to_string_lossy(), &[observation], "fixture")
            .unwrap();
        // Fix the discovery time before the simulated assertion, independently
        // of scheduler timing between discovery and the first content read.
        db.execute(
            "UPDATE files SET changed_at=?1",
            [OBSERVED_BEFORE_CONFIRMATION],
        )
        .unwrap();
        let id = db
            .query_row(
                "SELECT id FROM files WHERE path=?1",
                [file.to_string_lossy().to_string()],
                |r| r.get(0),
            )
            .unwrap();
        PendingHashFixture {
            audit,
            file,
            volumes,
            run,
            id,
        }
    }

    #[test]
    fn first_hash_enriches_stable_metadata_without_invalidating_confirmation() {
        let f = pending_hash_fixture();
        assert!(f.current_since_confirmation());
        let result = f.hash();
        assert!(result.digest.is_some());
        f.audit.store_hashes(f.run, &[result]).unwrap();
        assert!(f.current_since_confirmation());
        assert!(!f
            .audit
            .changed_below(
                &f.file.parent().unwrap().to_string_lossy(),
                CONFIRMED_AFTER_DISCOVERY
            )
            .unwrap());
        let db = f.audit.connect().unwrap();
        let saved: (String, String, String) = db
            .query_row(
                "SELECT digest,verified_at,changed_at FROM files WHERE id=?1",
                [f.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            saved.0,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(saved.1.as_str() > CONFIRMED_AFTER_DISCOVERY);
        assert_eq!(saved.2, OBSERVED_BEFORE_CONFIRMATION);
        // First observations still produce a notification without claiming a
        // source change happened after the manual assertion.
        assert_eq!(f.audit.notifications(100, 0).unwrap()["total"], 1);
    }

    #[test]
    fn first_hash_with_changed_metadata_still_invalidates_confirmation() {
        for field in ["identity", "mtime", "logical"] {
            let f = pending_hash_fixture();
            let mut result = f.hash();
            match field {
                "identity" => result.identity.push_str("-replacement"),
                "mtime" => result.mtime.push_str("-modified"),
                "logical" => result.logical += 1,
                _ => unreachable!(),
            }
            f.audit.store_hashes(f.run, &[result]).unwrap();
            assert!(!f.current_since_confirmation(), "{field}");
            let changed: String = f
                .audit
                .connect()
                .unwrap()
                .query_row(
                    "SELECT changed_at FROM dirs WHERE path=?1",
                    [f.file.parent().unwrap().to_string_lossy().to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(changed.as_str() > CONFIRMED_AFTER_DISCOVERY, "{field}");
        }
    }

    #[test]
    fn changed_existing_digest_invalidates_even_with_identical_metadata() {
        use std::fs::FileTimes;
        let f = pending_hash_fixture();
        let first = f.hash();
        let old_digest = first.digest.clone();
        let old_identity = first.identity.clone();
        let old_mtime = first.mtime.clone();
        let old_logical = first.logical;
        f.audit.store_hashes(f.run, &[first]).unwrap();
        let modified = fs::metadata(&f.file).unwrap().modified().unwrap();
        fs::write(&f.file, b"xyz").unwrap();
        File::options()
            .write(true)
            .open(&f.file)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        let second = f.hash();
        assert_eq!(second.identity, old_identity);
        assert_eq!(second.mtime, old_mtime);
        assert_eq!(second.logical, old_logical);
        assert_ne!(second.digest, old_digest);
        f.audit.store_hashes(f.run, &[second]).unwrap();
        assert!(!f.current_since_confirmation());
    }

    #[test]
    fn new_file_discovery_stays_stale_after_first_hash_and_notifies() {
        let f = pending_hash_fixture();
        let parent = f.file.parent().unwrap();
        let added = parent.join("added.txt");
        fs::write(&added, b"new").unwrap();
        let observation = observe(
            &added.to_string_lossy(),
            &parent.to_string_lossy(),
            &f.volumes,
            false,
            false,
        );
        f.audit
            .store_discovery(f.run, &parent.to_string_lossy(), &[observation], "new-file")
            .unwrap();
        let db = f.audit.connect().unwrap();
        let (id, discovered_at): (i64, String) = db
            .query_row(
                "SELECT id,changed_at FROM files WHERE path=?1",
                [added.to_string_lossy().to_string()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(discovered_at.as_str() > CONFIRMED_AFTER_DISCOVERY);
        let result = hash_file(
            HashJob {
                id,
                path: added.to_string_lossy().into_owned(),
            },
            &f.volumes,
            &AtomicBool::new(false),
        );
        f.audit.store_hashes(f.run, &[result]).unwrap();
        let hashed_at: String = db
            .query_row("SELECT changed_at FROM files WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(hashed_at, discovered_at);
        assert!(f
            .audit
            .changed_below(&parent.to_string_lossy(), CONFIRMED_AFTER_DISCOVERY)
            .unwrap());
        assert_eq!(f.audit.notifications(100, 0).unwrap()["total"], 1);
    }

    #[test]
    fn first_hash_failure_remains_an_invalidating_gap() {
        let f = pending_hash_fixture();
        let mut result = f.hash();
        result.digest = None;
        result.problem = Some("Synthetic source became unreadable before hashing".into());
        f.audit.store_hashes(f.run, &[result]).unwrap();
        assert!(!f.current_since_confirmation());
        let saved: (String, String) = f
            .audit
            .connect()
            .unwrap()
            .query_row("SELECT state,problem FROM files WHERE id=?1", [f.id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(saved.0, "gap");
        assert!(saved.1.contains("unreadable"));
    }

    #[test]
    fn baseline_hashes_aggregate_and_persists() {
        let (a, p, v) = fixture();
        fs::write(p.join("a.txt"), b"abc").unwrap();
        fs::write(p.join("nested").join("b.txt"), b"hello").unwrap();
        let live = a.stats_for(&p.join("a.txt").to_string_lossy()).unwrap();
        assert_eq!(live["logicalBytes"], 3);
        assert_eq!(live["allocationUnknown"], 0);
        a.start(vec![p.to_string_lossy().into()], v, false, 4)
            .unwrap();
        wait(&a);
        let s = a.status().unwrap();
        assert_eq!(s["phase"], "complete");
        assert_eq!(s["discoveredLeaves"], 2);
        assert_eq!(s["hashedLeaves"], 2);
        assert_eq!(s["logicalBytes"], 8);
        let f = a.stats_for(&p.join("a.txt").to_string_lossy()).unwrap();
        assert_eq!(
            f["checksum"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let dir = a.stats_for(&p.to_string_lossy()).unwrap();
        assert_eq!(dir["hashedLeaves"], 2);
        assert_eq!(a.notifications(100, 0).unwrap()["total"], 1);
        a.mark_read(&[], true).unwrap();
        assert_eq!(a.notifications(100, 0).unwrap()["unread"], 0);
        let db = a.path.clone();
        drop(a);
        let reopened = Audit::open(db, "fixture".into(), vec![]).unwrap();
        assert_eq!(reopened.status().unwrap()["hashedLeaves"], 2);
        assert_eq!(reopened.notifications(100, 0).unwrap()["unread"], 0);
    }
    #[test]
    fn rescan_detects_same_size_and_mtime_content_additions_and_removals() {
        use std::fs::FileTimes;
        let (a, p, v) = fixture();
        let f = p.join("a.txt");
        fs::write(&f, b"abc").unwrap();
        let modified = fs::metadata(&f).unwrap().modified().unwrap();
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 2)
            .unwrap();
        wait(&a);
        let since = now();
        thread::sleep(Duration::from_millis(5));
        fs::write(&f, b"xyz").unwrap();
        File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_times(FileTimes::new().set_modified(modified))
            .unwrap();
        fs::write(p.join("new.txt"), b"new").unwrap();
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 2)
            .unwrap();
        wait(&a);
        assert_eq!(a.status().unwrap()["hashedLeaves"], 2);
        assert!(a.changed_below(&p.to_string_lossy(), &since).unwrap());
        assert!(a.notifications(100, 0).unwrap()["total"].as_i64().unwrap() > 1);
        fs::remove_file(p.join("new.txt")).unwrap();
        a.start(vec![p.to_string_lossy().into()], v, false, 2)
            .unwrap();
        wait(&a);
        assert_eq!(a.status().unwrap()["discoveredLeaves"], 1);
        let db = a.connect().unwrap();
        assert_eq!(
            db.query_row(
                "SELECT state FROM files WHERE path=?1",
                [p.join("new.txt").to_string_lossy().to_string()],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "missing"
        );
    }
    #[test]
    fn excluded_self_paths_and_watcher_changes_are_explicit() {
        let (a, p, v) = fixture();
        fs::write(p.join("data.json"), b"active").unwrap();
        let db = a.path.clone();
        drop(a);
        let a = Audit::open(
            db,
            "fixture".into(),
            vec![p.join("data.*").to_string_lossy().into()],
        )
        .unwrap();
        a.set_volumes(v.clone()).unwrap();
        a.start(vec![p.to_string_lossy().into()], v, false, 1)
            .unwrap();
        wait(&a);
        assert_eq!(a.status().unwrap()["coverageGaps"], 1);
        assert_eq!(a.status().unwrap()["hashedLeaves"], 0);
        let since = now();
        thread::sleep(Duration::from_millis(2));
        a.record_watch_change(&p.join("new.txt").to_string_lossy(), "modified")
            .unwrap();
        assert!(a.changed_below(&p.to_string_lossy(), &since).unwrap());
        a.record_watch_change(&p.join("data.json").to_string_lossy(), "modified")
            .unwrap();
        assert_eq!(a.notifications(100, 0).unwrap()["total"], 2);
    }
    #[test]
    fn queued_pause_and_resume_are_durable() {
        let (a, p, v) = fixture();
        for n in 0..40 {
            fs::write(p.join(format!("{n}.txt")), vec![1u8; 65536]).unwrap();
        }
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 2)
            .unwrap();
        a.pause();
        wait(&a);
        assert_eq!(a.status().unwrap()["phase"], "paused");
        let db = a.path.clone();
        drop(a);
        let a = Audit::open(db, "fixture".into(), vec![]).unwrap();
        a.start(Vec::new(), v, true, 2).unwrap();
        wait(&a);
        assert_eq!(a.status().unwrap()["hashedLeaves"], 40);
    }
    #[test]
    fn watcher_invalidates_cached_file_and_folder_hashes_and_coverage_gaps() {
        let (a, p, v) = fixture();
        let file = p.join("a.txt");
        fs::write(&file, b"abc").unwrap();
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 2)
            .unwrap();
        wait(&a);
        assert_eq!(
            a.stats_for(&file.to_string_lossy()).unwrap()["complete"],
            true
        );
        assert_eq!(a.stats_for(&p.to_string_lossy()).unwrap()["complete"], true);
        let epoch = a.status().unwrap()["changeSequence"].as_i64().unwrap();
        thread::sleep(Duration::from_millis(3));
        a.record_watch_change(&file.to_string_lossy(), "modified")
            .unwrap();
        let file_stats = a.stats_for(&file.to_string_lossy()).unwrap();
        assert_eq!(file_stats["complete"], false);
        assert_eq!(file_stats["hashedLeaves"], 0);
        assert_eq!(file_stats["cached"], true);
        assert!(file_stats["sha256"].is_string());
        let dir = a.stats_for(&p.to_string_lossy()).unwrap();
        assert_eq!(dir["totalKnown"], false);
        assert!(dir["problem"].is_string());
        assert!(a.status().unwrap()["changeSequence"].as_i64().unwrap() > epoch);
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 2)
            .unwrap();
        wait(&a);
        assert_eq!(
            a.stats_for(&file.to_string_lossy()).unwrap()["complete"],
            true
        );
        let since = now();
        thread::sleep(Duration::from_millis(3));
        a.record_watch_gap("", "Watcher buffer overflow; fresh audit required")
            .unwrap();
        assert!(a.changed_below(&file.to_string_lossy(), &since).unwrap());
        assert_eq!(
            a.stats_for(&file.to_string_lossy()).unwrap()["complete"],
            false
        );
        assert!(a.start(Vec::new(), v, true, 2).is_err());
    }
    #[test]
    fn excluded_subtree_is_gap_not_a_false_deletion_and_empty_directory_completes() {
        let (a, p, v) = fixture();
        fs::write(p.join("nested").join("keep.txt"), b"keep").unwrap();
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 2)
            .unwrap();
        wait(&a);
        let db = a.path.clone();
        drop(a);
        let a = Audit::open(
            db,
            "fixture".into(),
            vec![p.join("nested").to_string_lossy().into()],
        )
        .unwrap();
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 2)
            .unwrap();
        wait(&a);
        let root = a.stats_for(&p.to_string_lossy()).unwrap();
        assert_eq!(root["totalKnown"], false);
        assert_eq!(root["complete"], false);
        assert!(root["problem"].is_string());
        let state: String = a
            .connect()
            .unwrap()
            .query_row(
                "SELECT state FROM files WHERE path=?1",
                [p.join("nested")
                    .join("keep.txt")
                    .to_string_lossy()
                    .to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(state, "missing");
        let empty = p.join("empty");
        fs::create_dir(&empty).unwrap();
        a.start(vec![empty.to_string_lossy().into()], v, false, 2)
            .unwrap();
        wait(&a);
        let s = a.stats_for(&empty.to_string_lossy()).unwrap();
        assert_eq!(s["complete"], true);
        assert_eq!(s["totalKnown"], true);
        assert_eq!(s["hashedLeaves"], 0);
    }
    #[test]
    fn sparse_and_hardlink_allocation_uses_native_storage_measurements() {
        use std::io::{Seek, SeekFrom, Write};
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::IO::DeviceIoControl;
        let (a, p, v) = fixture();
        let sparse = p.join("sparse.bin");
        let mut file = File::options()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&sparse)
            .unwrap();
        let mut bytes = 0;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                0x900c4,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                0,
                &mut bytes,
                std::ptr::null_mut(),
            )
        };
        assert_ne!(ok, 0, "fixture filesystem must support sparse files");
        file.seek(SeekFrom::Start(8 * 1024 * 1024)).unwrap();
        file.write_all(&[1]).unwrap();
        drop(file);
        fs::hard_link(&sparse, p.join("same-hardlink.bin")).unwrap();
        let stats = a.stats_for(&sparse.to_string_lossy()).unwrap();
        assert_eq!(stats["allocationUnknown"], 0);
        assert!(
            stats["allocatedBytes"].as_i64().unwrap() < stats["logicalBytes"].as_i64().unwrap()
        );
        a.start(vec![p.to_string_lossy().into()], v, false, 2)
            .unwrap();
        wait(&a);
        let s = a.status().unwrap();
        assert_eq!(s["hashedLeaves"], 2);
        assert_eq!(s["logicalBytes"], 2 * (8 * 1024 * 1024 + 1));
        assert_eq!(
            s["allocatedBytes"].as_i64().unwrap(),
            stats["allocatedBytes"].as_i64().unwrap() * 2
        );
    }
    #[test]
    fn resumed_directory_reconciles_files_seen_before_interruption() {
        let (a, p, v) = fixture();
        let gone = p.join("gone.txt");
        fs::write(&gone, b"gone").unwrap();
        let db = a.connect().unwrap();
        db.execute(
            "INSERT INTO runs(phase,roots,started_at,workers,baseline) VALUES('paused',?1,?2,1,1)",
            params![json!([p.to_string_lossy()]).to_string(), now()],
        )
        .unwrap();
        let run = db.last_insert_rowid();
        db.execute(
            "INSERT INTO dirs(path,parent,run,state,changed_at) VALUES(?1,'',?2,'pending',?3)",
            params![p.to_string_lossy().to_string(), run, now()],
        )
        .unwrap();
        let observation = observe(
            &gone.to_string_lossy(),
            &p.to_string_lossy(),
            &v,
            false,
            false,
        );
        a.store_discovery(
            run,
            &p.to_string_lossy(),
            &[observation],
            "interrupted-token",
        )
        .unwrap();
        fs::remove_file(&gone).unwrap();
        a.start(Vec::new(), v, true, 2).unwrap();
        wait(&a);
        assert_eq!(a.status().unwrap()["discoveredLeaves"], 0);
        assert_eq!(a.status().unwrap()["hashedLeaves"], 0);
        let state: String = db
            .query_row(
                "SELECT state FROM files WHERE path=?1",
                [gone.to_string_lossy().to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "missing");
    }
    #[test]
    fn partial_scope_rescan_keeps_ancestor_recursive_totals() {
        let (a, p, v) = fixture();
        fs::write(p.join("root.txt"), b"abc").unwrap();
        fs::write(p.join("nested").join("child.txt"), b"hello").unwrap();
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 4)
            .unwrap();
        wait(&a);
        assert_eq!(
            a.stats_for(&p.to_string_lossy()).unwrap()["logicalBytes"],
            8
        );
        a.start(vec![p.join("nested").to_string_lossy().into()], v, false, 4)
            .unwrap();
        wait(&a);
        let parent = a.stats_for(&p.to_string_lossy()).unwrap();
        assert_eq!(parent["logicalBytes"], 8);
        assert_eq!(parent["hashedLeaves"], 2);
        assert_eq!(parent["complete"], true);
    }
    #[test]
    fn concurrent_discovery_notifications_and_polling_do_not_lock_out_the_writer() {
        let (a, p, v) = fixture();
        for dir in 0..8 {
            let d = p.join(format!("dir-{dir}"));
            fs::create_dir(&d).unwrap();
            for f in 0..25 {
                fs::write(d.join(format!("{f}.txt")), b"payload").unwrap();
            }
        }
        let stop = Arc::new(AtomicBool::new(false));
        let observer = a.clone();
        let flag = stop.clone();
        let root = p.to_string_lossy().to_string();
        let polling = thread::spawn(move || {
            while !flag.load(Ordering::Relaxed) {
                observer.status().unwrap();
                observer.stats_for(&root).unwrap();
                observer
                    .record_watch_change(&format!("{root}\\pending-watch.txt"), "modified")
                    .unwrap();
                observer.notifications(10, 0).unwrap();
                thread::sleep(Duration::from_millis(2));
            }
        });
        for _ in 0..3 {
            a.start(vec![p.to_string_lossy().into()], v.clone(), false, 4)
                .unwrap();
            wait(&a);
            assert_eq!(a.status().unwrap()["phase"], "complete");
            assert_eq!(a.status().unwrap()["hashedLeaves"], 200);
        }
        stop.store(true, Ordering::Relaxed);
        polling.join().unwrap();
    }
    #[test]
    fn failed_pool_scope_joins_workers_before_returning() {
        let cancel = AtomicBool::new(false);
        let finished = Arc::new(AtomicBool::new(false));
        let (jobs, input) = mpsc::sync_channel::<()>(1);
        let (results, output) = mpsc::sync_channel::<()>(1);
        let marker = finished.clone();
        let handle = thread::spawn(move || {
            let _ = input.recv();
            let _ = results.send(());
            marker.store(true, Ordering::SeqCst);
        });
        {
            let _failed_scope = WorkerPool {
                jobs: Some(jobs),
                output: Some(output),
                handles: vec![handle],
                cancel: &cancel,
                successful: false,
            };
        }
        assert!(cancel.load(Ordering::SeqCst));
        assert!(finished.load(Ordering::SeqCst));
    }
    #[test]
    fn busy_source_file_is_a_gap_until_a_new_full_pass() {
        let (a, p, v) = fixture();
        let path = p.join("busy.txt");
        fs::write(&path, b"in use").unwrap();
        let writing = File::options().write(true).open(&path).unwrap();
        a.start(vec![p.to_string_lossy().into()], v.clone(), false, 2)
            .unwrap();
        wait(&a);
        assert_eq!(a.status().unwrap()["failedLeaves"], 1);
        assert_eq!(a.status().unwrap()["complete"], false);
        drop(writing);
        a.start(vec![p.to_string_lossy().into()], v, false, 2)
            .unwrap();
        wait(&a);
        assert_eq!(a.status().unwrap()["hashedLeaves"], 1);
        assert_eq!(a.status().unwrap()["failedLeaves"], 0);
    }
    #[test]
    fn repeated_old_notification_returns_to_the_latest_list_without_changing_its_id() {
        let (a, _, _) = fixture();
        a.notify("old", "source-change", "Old path", "First change", "old", 1)
            .unwrap();
        let id = a.notifications(1, 0).unwrap()["items"][0]["id"].clone();
        thread::sleep(Duration::from_millis(3));
        a.notify("new", "source-change", "New path", "New change", "new", 1)
            .unwrap();
        assert_ne!(a.notifications(1, 0).unwrap()["items"][0]["id"], id);
        a.mark_read(&[id.as_i64().unwrap()], false).unwrap();
        thread::sleep(Duration::from_millis(3));
        a.notify(
            "old",
            "source-change",
            "Old path",
            "Another change",
            "old",
            1,
        )
        .unwrap();
        let latest = a.notifications(1, 0).unwrap();
        assert_eq!(latest["items"][0]["id"], id);
        assert_eq!(latest["items"][0]["changeCount"], 2);
        assert!(latest["items"][0]["readAt"].is_null());
    }
}
