use crate::{
    filesystem::{self, Entry, Volume},
    store::{error, hash, now},
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

const MISSING: &str = "Missing or changed type since the previous observation; retained for review";
#[derive(Clone, Default)]
struct PriorityPlan {
    revision: u64,
    rules: Vec<(String, i64)>,
}

pub struct Index {
    pub path: PathBuf,
    pub running: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,
    refresh_locks: Arc<[Mutex<()>; 64]>,
    worker_handle: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    priority_plan: Arc<Mutex<PriorityPlan>>,
    priority_applied: Arc<Mutex<Option<u64>>>,
}
impl Index {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(path.parent().ok_or("Index path has no parent")?).map_err(error)?;
        let s = Self {
            path,
            running: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            refresh_locks: Arc::new(std::array::from_fn(|_| Mutex::new(()))),
            worker_handle: Arc::new(Mutex::new(None)),
            priority_plan: Arc::new(Mutex::new(PriorityPlan::default())),
            priority_applied: Arc::new(Mutex::new(None)),
        };
        let db = s.connect()?;
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS directories(path TEXT PRIMARY KEY COLLATE NOCASE,signature TEXT,changed_at TEXT,observed_at TEXT,files INTEGER,folders INTEGER,bytes INTEGER,problem TEXT);
            CREATE TABLE IF NOT EXISTS directory_children(parent TEXT COLLATE NOCASE,path TEXT COLLATE NOCASE,generation TEXT,PRIMARY KEY(parent,path));
            CREATE INDEX IF NOT EXISTS child_parent ON directory_children(parent,generation);
            CREATE TABLE IF NOT EXISTS listing_entries(parent TEXT COLLATE NOCASE,generation TEXT,path TEXT COLLATE NOCASE,name TEXT,kind TEXT,size INTEGER,modified TEXT,fingerprint TEXT,problem TEXT,PRIMARY KEY(parent,generation,path));
            CREATE INDEX IF NOT EXISTS listing_order ON listing_entries(parent,generation,kind,name COLLATE NOCASE);
            CREATE TABLE IF NOT EXISTS listing_heads(parent TEXT PRIMARY KEY COLLATE NOCASE,generation TEXT,observed_at TEXT);
            CREATE TABLE IF NOT EXISTS observation_members(parent TEXT,generation TEXT,name TEXT COLLATE BINARY,folded_name TEXT,kind TEXT,fingerprint TEXT,problem TEXT,PRIMARY KEY(parent,generation,name));
            CREATE INDEX IF NOT EXISTS observation_lookup ON observation_members(parent,generation,folded_name);
            CREATE TABLE IF NOT EXISTS observation_heads(parent TEXT PRIMARY KEY,source_path TEXT,generation TEXT,fingerprint TEXT,identity TEXT,observed_at TEXT,problem TEXT);
            CREATE TABLE IF NOT EXISTS observation_versions(parent TEXT PRIMARY KEY,sequence INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS derived_status_summaries(path TEXT PRIMARY KEY,epoch TEXT NOT NULL,counts TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS queue(path TEXT PRIMARY KEY COLLATE NOCASE,state TEXT);
            CREATE TABLE IF NOT EXISTS scan_progress(path TEXT PRIMARY KEY COLLATE NOCASE,files INTEGER NOT NULL,bytes INTEGER NOT NULL);
            DELETE FROM scan_progress;
            UPDATE queue SET state='pending' WHERE state='working';
            CREATE INDEX IF NOT EXISTS queue_state ON queue(state);
            CREATE INDEX IF NOT EXISTS queue_order ON queue(state,length(path),path);
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT);
            INSERT OR IGNORE INTO meta VALUES('scan','{}');
            INSERT OR IGNORE INTO meta VALUES('observationSequence','0');
            UPDATE meta SET value=json_set(value,'$.status','paused') WHERE key='scan' AND json_extract(value,'$.status')='running';").map_err(error)?;
        let has_priority: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('queue') WHERE name='priority')", [], |row| row.get(0)
        ).map_err(error)?;
        if !has_priority {
            db.execute("ALTER TABLE queue ADD COLUMN priority INTEGER NOT NULL DEFAULT 0", []).map_err(error)?;
        }
        db.execute_batch("CREATE TABLE IF NOT EXISTS queue_priority_rules(path TEXT PRIMARY KEY COLLATE NOCASE,defer INTEGER NOT NULL);
            CREATE INDEX IF NOT EXISTS queue_priority_order ON queue(state,priority,length(path),path);").map_err(error)?;
        Ok(s)
    }
    pub(crate) fn update_priority(&self, doc: &Value, volumes: &[Volume]) {
        let Some(revision) = doc["revision"].as_u64() else { return };
        let mut rules = Vec::new();
        if doc["inventoryDeferMarked"] != false {
            for mark in doc["decisions"].as_object().into_iter().flat_map(|marks| marks.values()) {
                if mark["noteOnly"] == true { continue; }
                let (Some(id), Some(relative), Some(status)) =
                    (mark["volumeId"].as_str(), mark["relativePath"].as_str(), mark["status"].as_str())
                else { continue };
                let Some(volume) = volumes.iter().find(|volume| volume.id == id && volume.available) else { continue };
                let path = if relative.is_empty() { volume.root.clone() } else {
                    format!("{}\\{relative}", volume.root.trim_end_matches('\\'))
                };
                if filesystem::locate(&path, volumes).is_ok() {
                    rules.push((path, i64::from(status != "attention")));
                }
            }
        }
        // Apply parent rules first. A nearer explicit decision can override them.
        rules.sort_unstable_by_key(|(path, _)| path.len());
        if let Ok(mut plan) = self.priority_plan.lock() {
            if revision >= plan.revision && plan.rules != rules {
                *plan = PriorityPlan { revision, rules };
            }
        }
    }
    fn apply_priority(&self, db: &mut Connection) -> Result<(), String> {
        let mut applied = self.priority_applied.lock().map_err(error)?;
        let plan = {
            let plan = self.priority_plan.lock().map_err(error)?;
            if *applied == Some(plan.revision) { return Ok(()) }
            plan.clone()
        };
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(error)?;
        tx.execute_batch("DELETE FROM queue_priority_rules; UPDATE queue SET priority=0 WHERE priority<>0;").map_err(error)?;
        for (path, defer) in plan.rules {
            tx.execute("INSERT OR REPLACE INTO queue_priority_rules VALUES(?1,?2)", params![path,defer]).map_err(error)?;
            let prefix = path.trim_end_matches('\\');
            tx.execute("UPDATE queue SET priority=?1 WHERE priority<>?1 AND (path=?2 OR (path>=?3 AND path<?4))",
                params![defer,path,format!("{prefix}\\"),format!("{prefix}]")]).map_err(error)?;
        }
        tx.commit().map_err(error)?;
        *applied = Some(plan.revision);
        Ok(())
    }
    pub fn connect(&self) -> Result<Connection, String> {
        let c = Connection::open(&self.path).map_err(error)?;
        c.busy_timeout(std::time::Duration::from_secs(15))
            .map_err(error)?;
        Ok(c)
    }
    pub fn join_stopped(&self) -> Result<(), String> {
        let mut worker = self.worker_handle.lock().map_err(error)?;
        if self.running.load(Ordering::SeqCst) {
            return Err("Inventory is still stopping".into());
        }
        if let Some(handle) = worker.take() {
            handle
                .join()
                .map_err(|_| "Inventory coordinator panicked during shutdown".to_owned())?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn checkpoint(&self) -> Result<(), String> {
        self.checkpoint_with_timeout(std::time::Duration::from_secs(15))
    }
    pub fn checkpoint_with_timeout(&self, timeout: std::time::Duration) -> Result<(), String> {
        if self.running.load(Ordering::SeqCst) {
            return Err("Inventory must stop before checkpointing".into());
        }
        let db = self.connect()?;
        db.busy_timeout(timeout).map_err(error)?;
        db.execute_batch("PRAGMA synchronous=FULL;")
            .map_err(error)?;
        let (busy, log, saved): (i64, i64, i64) = db
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(error)?;
        if busy != 0 || log != saved {
            return Err(
                "Inventory checkpoint is waiting for another database reader; the app remains open"
                    .into(),
            );
        }
        Ok(())
    }
    pub fn has_observations(&self) -> Result<bool, String> {
        self.connect()?
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM directories WHERE observed_at IS NOT NULL)",
                [],
                |r| r.get(0),
            )
            .map_err(error)
    }
    pub fn scan_status(&self) -> Result<Value, String> {
        let mut db = self.connect()?;
        let db = db.transaction().map_err(error)?;
        let raw: String = db
            .query_row("SELECT value FROM meta WHERE key='scan'", [], |r| r.get(0))
            .map_err(error)?;
        let mut v: Value = serde_json::from_str(&raw).map_err(error)?;
        v["pending"] = json!(db
            .query_row(
                "SELECT count(*) FROM queue WHERE state='pending'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .map_err(error)?);
        v["active"] = json!(db
            .query_row(
                "SELECT count(*) FROM queue WHERE state='working'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .map_err(error)?);
        v["currentPath"] = json!(db
            .query_row(
                "SELECT path FROM queue WHERE state='working' ORDER BY path LIMIT 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .optional()
            .map_err(error)?);
        let (files, bytes): (i64, i64) = db
            .query_row(
                "SELECT coalesce(sum(files),0),coalesce(sum(bytes),0) FROM scan_progress",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(error)?;
        v["currentFiles"] = json!(files);
        v["currentBytes"] = json!(bytes);
        v["workers"] = json!(v["workers"].as_u64().unwrap_or(0));
        // The unseen filesystem has no trustworthy denominator during discovery.
        v["totalKnown"] = json!(v["status"] == "complete");
        v["gaps"] = json!(db
            .query_row(
                "SELECT count(*) FROM directories WHERE problem IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .map_err(error)?);
        v["skippedGenerated"] = json!(db.query_row(
            "SELECT count(*) FROM queue WHERE state='skipped'",
            [], |row| row.get::<_, i64>(0),
        ).map_err(error)?);
        Ok(v)
    }
    pub fn gaps(&self) -> Result<Value, String> {
        let db = self.connect()?;
        let mut q = db.prepare(
            "SELECT path,problem,observed_at FROM (
                 SELECT path,problem,observed_at FROM directories WHERE problem IS NOT NULL
                 ORDER BY observed_at DESC
                 LIMIT 200-min(20,(SELECT count(*) FROM queue WHERE state='skipped'))
             )
             UNION ALL
             SELECT path,problem,observed_at FROM (
                 SELECT path,'Generated folder skipped by the metadata scan. Index it explicitly to include its contents.' AS problem,
                        json_extract((SELECT value FROM meta WHERE key='scan'),'$.finishedAt') AS observed_at
                 FROM queue WHERE state='skipped' ORDER BY path LIMIT 20
             )",
        ).map_err(error)?;
        let rows=q.query_map([],|r|Ok(json!({"path":r.get::<_,String>(0)?,"problem":r.get::<_,String>(1)?,"observedAt":r.get::<_,Option<String>>(2)?}))).map_err(error)?;
        Ok(Value::Array(
            rows.collect::<Result<Vec<_>, _>>().map_err(error)?,
        ))
    }
    pub(crate) fn skipped_ancestor(&self, path: &str) -> Result<bool, String> {
        let db = self.connect()?;
        let mut lookup = db.prepare("SELECT 1 FROM queue WHERE path=?1 AND state='skipped'")
            .map_err(error)?;
        for ancestor in Path::new(path).ancestors().filter_map(Path::to_str) {
            if lookup.exists([ancestor]).map_err(error)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn directory_info(&self, path: &str) -> Result<Option<Value>, String> {
        self.connect()?.query_row("SELECT observed_at,files,folders,bytes,problem,signature,(SELECT observed_at FROM listing_heads WHERE parent=?1) FROM directories WHERE path=?1",[path],|r|Ok(json!({"observedAt":r.get::<_,Option<String>>(0)?,"files":optional_count(r,1)?,"folders":optional_count(r,2)?,"bytes":optional_count(r,3)?,"problem":r.get::<_,Option<String>>(4)?,"signature":r.get::<_,Option<String>>(5)?,"listingObservedAt":r.get::<_,Option<String>>(6)?}))).optional().map_err(error)
    }
    pub fn record_gap(&self, path: &str, problem: &str) -> Result<(), String> {
        let mut db = self.connect()?;
        let tx = db.transaction().map_err(error)?;
        tx.execute("INSERT INTO directories(path,problem,observed_at,changed_at) VALUES(?1,?2,?3,?3) ON CONFLICT(path) DO UPDATE SET problem=?2,observed_at=?3,changed_at=CASE WHEN coalesce(problem,'')<>?2 THEN ?3 ELSE changed_at END",params![path,problem,now()]).map_err(error)?;
        tx.execute(
            "UPDATE observation_heads SET problem=?2 WHERE parent=?1",
            params![path.to_lowercase(), problem],
        )
        .map_err(error)?;
        touch_observations(&tx, path)?;
        tx.commit().map_err(error)?;
        Ok(())
    }
    // Only immediate children are enumerated. A staged generation becomes visible atomically.
    // Browser payloads and lightweight metadata membership are separate. Membership
    // supports derived status counts, never frozen backup or copying authority.
    pub fn refresh(
        &self,
        path: &str,
        volumes: &[Volume],
        cancel: Option<&AtomicBool>,
        cache_files: bool,
    ) -> Result<(), String> {
        // Fixed lock stripes serialize generations of the same directory without retaining a
        // growing map of every visited path. Different directories usually proceed concurrently.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.to_lowercase().hash(&mut hasher);
        let _guard = self.refresh_locks[hasher.finish() as usize % 64]
            .lock()
            .map_err(|_| "Folder refresh lock failed")?;
        let result = self.refresh_inner(path, volumes, cancel, cache_files);
        if let Err(e) = &result {
            if e != "Paused" {
                self.record_gap(path, e)?;
            }
            // Unpublished generations are never visible, even when cleanup
            // itself is interrupted; a later complete refresh can reclaim them.
            self.clean_observation_generations(&self.connect()?, path)?;
        }
        result
    }
    fn refresh_inner(
        &self,
        path: &str,
        volumes: &[Volume],
        cancel: Option<&AtomicBool>,
        cache_files: bool,
    ) -> Result<(), String> {
        self.refresh_observing(path, volumes, cancel, cache_files, || {})
    }

    fn refresh_observing(
        &self,
        path: &str,
        volumes: &[Volume],
        cancel: Option<&AtomicBool>,
        cache_files: bool,
        before_publication: impl FnOnce(),
    ) -> Result<(), String> {
        let (p, _path_guards) = filesystem::guard_path(path, volumes, false)?;
        let before = filesystem::inspect(&p);
        let identity = crate::audit::handle_identity(
            _path_guards
                .last()
                .ok_or("Missing observed directory handle")?,
        )?;
        let listing = fs::read_dir(&p).map_err(error)?;
        let generation = format!(
            "{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let mut files = 0u64;
        let mut folders = 0u64;
        let mut bytes = 0u64;
        let mut hashes = [0u8; 32];
        let mut samples = Vec::new();
        let mut errors = 0u64;
        let scanning = self.running.load(Ordering::Relaxed) && cancel.is_some();
        let mut db = self.connect()?;
        let mut chunk = Vec::with_capacity(500);
        for item in listing {
            if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                return Err("Paused".into());
            }
            let item = match item {
                Ok(i) => i,
                Err(e) => {
                    errors += 1;
                    if samples.len() < 20 {
                        samples.push(error(e));
                    }
                    continue;
                }
            };
            let e = filesystem::inspect(&item.path());
            // Order-independent full-digest metadata fingerprint; never backup content verification.
            let digest = Sha256::digest(format!("{}|{}", e.name, e.fingerprint).as_bytes());
            for (i, b) in digest.iter().enumerate() {
                hashes[i] ^= b;
            }
            match e.kind.as_str() {
                "folder" => folders += 1,
                "file" => {
                    files += 1;
                    bytes = bytes
                        .checked_add(e.size)
                        .ok_or("Directory byte count exceeds the supported integer range")?;
                }
                _ => {}
            }
            chunk.push(e);
            if chunk.len() == 500 {
                Self::flush_chunk_on(
                    &mut db,
                    path,
                    &generation,
                    &chunk,
                    cache_files,
                    scanning,
                    files,
                    bytes,
                )?;
                chunk.clear();
            }
        }
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err("Paused".into());
        }
        before_publication();
        let (current, guards) = filesystem::guard_path(path, volumes, false)?;
        if filesystem::inspect(&current).fingerprint != before.fingerprint
            || crate::audit::handle_identity(
                guards.last().ok_or("Missing current directory handle")?,
            )? != identity
        {
            return Err("Directory changed during enumeration; the previous observation was retained and a fresh enumeration is required".into());
        }
        let signature = hash(format!("{:?}|{files}|{folders}|{bytes}|{errors}", hashes).as_bytes());
        let mut problem = if errors == 0 {
            None
        } else {
            Some(format!(
                "{errors} directory enumeration errors. First {}: {}",
                samples.len(),
                samples.join("; ")
            ))
        };
        let tx = db.transaction().map_err(error)?;
        // Publish the final batch and its summary in one commit, avoiding a redundant
        // journal sync for the overwhelmingly common small-directory case.
        Self::flush_chunk_in(
            &tx,
            path,
            &generation,
            &chunk,
            cache_files,
            scanning,
            files,
            bytes,
        )?;
        let ambiguous: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM observation_members WHERE parent=?1 AND generation=?2 GROUP BY folded_name HAVING count(*)>1)",
            params![path.to_lowercase(),generation], |r|r.get(0),
        ).map_err(error)?;
        if ambiguous {
            problem = Some("Names collide under the review identity rules; membership remains unknown until this scope is resolved".into());
        }
        let observed = now();
        let (files_sql, folders_sql, bytes_sql) = (
            sqlite_count(files)?,
            sqlite_count(folders)?,
            sqlite_count(bytes)?,
        );
        tx.execute("INSERT INTO directories(path,signature,changed_at,observed_at,files,folders,bytes,problem) VALUES(?1,?2,?3,?3,?4,?5,?6,?7) ON CONFLICT(path) DO UPDATE SET changed_at=CASE WHEN signature IS NOT NULL AND signature=?2 AND coalesce(problem,'')=coalesce(?7,'') THEN changed_at ELSE ?3 END,signature=?2,observed_at=?3,files=?4,folders=?5,bytes=?6,problem=?7",params![path,signature,observed,files_sql,folders_sql,bytes_sql,problem]).map_err(error)?;
        // Absence is established only by complete parent enumeration, never by a failed/paused scan.
        if errors == 0 {
            let mut missing = tx
                .prepare("SELECT path FROM directory_children WHERE parent=?1 AND generation<>?2")
                .map_err(error)?;
            let rows = missing
                .query_map(params![path, generation], |row| row.get::<_, String>(0))
                .map_err(error)?;
            for row in rows {
                let absent = row.map_err(error)?;
                let prefix = format!("{}\\", absent.trim_end_matches('\\'));
                tx.execute("UPDATE directories SET problem=?1,observed_at=?2,changed_at=?2 WHERE (path=?3 OR substr(lower(path),1,length(?4))=lower(?4)) AND coalesce(problem,'')<>?1",params![MISSING,observed,absent,prefix]).map_err(error)?;
            }
        }
        if cache_files && errors == 0 && !ambiguous {
            tx.execute("INSERT INTO listing_heads VALUES(?1,?2,?3) ON CONFLICT(parent) DO UPDATE SET generation=?2,observed_at=?3",params![path,generation,observed]).map_err(error)?;
        }
        tx.execute("INSERT INTO observation_heads VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(parent) DO UPDATE SET source_path=?2,generation=?3,fingerprint=?4,identity=?5,observed_at=?6,problem=?7",params![path.to_lowercase(),path,generation,before.fingerprint,identity,observed,problem]).map_err(error)?;
        touch_observations(&tx, path)?;
        tx.commit().map_err(error)?;
        self.clean_observation_generations(&db, path)?;
        if (errors > 0 || ambiguous) && cache_files {
            return Err(format!(
                "Directory listing incomplete: {}. Previous complete listing retained.",
                problem.unwrap_or_default()
            ));
        }
        if cache_files {
            self.clean_old_generations(path)?;
        }
        Ok(())
    }
    #[cfg(test)]
    fn flush_chunk(
        &self,
        path: &str,
        generation: &str,
        entries: &[Entry],
        cache_files: bool,
        scanning: bool,
        files: u64,
        bytes: u64,
    ) -> Result<(), String> {
        Self::flush_chunk_on(
            &mut self.connect()?,
            path,
            generation,
            entries,
            cache_files,
            scanning,
            files,
            bytes,
        )
    }
    fn flush_chunk_on(
        db: &mut Connection,
        path: &str,
        generation: &str,
        entries: &[Entry],
        cache_files: bool,
        scanning: bool,
        files: u64,
        bytes: u64,
    ) -> Result<(), String> {
        let tx = db.transaction().map_err(error)?;
        Self::flush_chunk_in(
            &tx,
            path,
            generation,
            entries,
            cache_files,
            scanning,
            files,
            bytes,
        )?;
        tx.commit().map_err(error)
    }
    fn flush_chunk_in(
        tx: &rusqlite::Transaction<'_>,
        path: &str,
        generation: &str,
        entries: &[Entry],
        cache_files: bool,
        scanning: bool,
        files: u64,
        bytes: u64,
    ) -> Result<(), String> {
        let observed = now();
        for e in entries {
            tx.execute(
                "INSERT OR REPLACE INTO observation_members VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    path.to_lowercase(),
                    generation,
                    e.name,
                    e.name.to_lowercase(),
                    e.kind,
                    e.fingerprint,
                    e.problem
                ],
            )
            .map_err(error)?;
            if cache_files {
                tx.execute(
                    "INSERT OR REPLACE INTO listing_entries VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                    params![
                        path,
                        generation,
                        e.path,
                        e.name,
                        e.kind,
                        sqlite_count(e.size)?,
                        e.modified,
                        e.fingerprint,
                        e.problem
                    ],
                )
                .map_err(error)?;
            }
            if e.kind == "folder" || e.kind == "link" || e.problem.is_some() {
                tx.execute("INSERT INTO directory_children VALUES(?1,?2,?3) ON CONFLICT(parent,path) DO UPDATE SET generation=?3",params![path,e.path,generation]).map_err(error)?;
                tx.execute(
                    "INSERT OR IGNORE INTO directories(path,changed_at) VALUES(?1,?2)",
                    params![e.path, observed],
                )
                .map_err(error)?;
            }
            if e.kind == "folder" {
                if scanning {
                    tx.execute(
                        "INSERT OR IGNORE INTO queue(path,state,priority) VALUES(?1,'pending',
                         coalesce((SELECT defer FROM queue_priority_rules WHERE path=?1),
                                  (SELECT priority FROM queue WHERE path=?2),0))",
                        params![e.path, path],
                    )
                    .map_err(error)?;
                }
            } else if e.kind == "link" || e.problem.is_some() {
                let problem = e.problem.as_deref().unwrap_or(
                    "Reparse point/link intentionally not followed; target coverage unknown",
                );
                tx.execute("UPDATE directories SET problem=?2,observed_at=?3,changed_at=CASE WHEN coalesce(problem,'')<>?2 THEN ?3 ELSE changed_at END WHERE path=?1",params![e.path,problem,observed]).map_err(error)?;
            }
        }
        if scanning {
            tx.execute("INSERT INTO scan_progress VALUES(?1,?2,?3) ON CONFLICT(path) DO UPDATE SET files=?2,bytes=?3",params![path,sqlite_count(files)?,sqlite_count(bytes)?]).map_err(error)?;
        }
        Ok(())
    }
    fn clean_old_generations(&self, path: &str) -> Result<(), String> {
        let db = self.connect()?;
        loop {
            let removed=db.execute("DELETE FROM listing_entries WHERE rowid IN (SELECT rowid FROM listing_entries WHERE parent=?1 AND generation<>coalesce((SELECT generation FROM listing_heads WHERE parent=?1),'') LIMIT 500)",[path]).map_err(error)?;
            if removed == 0 {
                return Ok(());
            }
            std::thread::yield_now();
        }
    }
    fn clean_observation_generations(&self, db: &Connection, path: &str) -> Result<(), String> {
        loop {
            let removed=db.execute("DELETE FROM observation_members WHERE rowid IN (SELECT rowid FROM observation_members WHERE parent=?1 AND generation<>coalesce((SELECT generation FROM observation_heads WHERE parent=?1),'') LIMIT 500)",[path.to_lowercase()]).map_err(error)?;
            if removed == 0 {
                break;
            }
        }
        Ok(())
    }
    // Callback can stop early; this never allocates a complete-directory Vec.
    pub fn each_entry(
        &self,
        path: &str,
        search: &str,
        mut visit: impl FnMut(Entry) -> bool,
    ) -> Result<(), String> {
        let db = self.connect()?;
        let mut q=db.prepare("SELECT path,name,kind,size,modified,fingerprint,problem FROM listing_entries WHERE parent=?1 AND generation=(SELECT generation FROM listing_heads WHERE parent=?1) AND instr(lower(name),lower(?2))>0 ORDER BY CASE kind WHEN 'folder' THEN 0 WHEN 'link' THEN 1 ELSE 2 END,name COLLATE NOCASE").map_err(error)?;
        let rows = q
            .query_map(params![path, search], entry_row)
            .map_err(error)?;
        for row in rows {
            if !visit(row.map_err(error)?) {
                break;
            }
        }
        Ok(())
    }
    pub fn signature(&self, path: &str) -> Option<String> {
        self.connect()
            .ok()?
            .query_row(
                "SELECT signature FROM directories WHERE path=?1",
                [path],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }
    pub fn changed_below(&self, path: &str, since: &str) -> Result<bool, String> {
        let prefix = format!("{}\\", path.trim_end_matches('\\'));
        let found=self.connect()?.query_row("SELECT 1 FROM directories WHERE (path=?1 OR substr(lower(path),1,length(?2))=lower(?2)) AND changed_at>?3 LIMIT 1",params![path,prefix,since],|r|r.get::<_,i64>(0)).optional().map_err(error)?;
        Ok(found.is_some())
    }
    pub fn known_dirs(&self, search: &str) -> Result<Vec<String>, String> {
        let db = self.connect()?;
        let mut q=db.prepare("SELECT path FROM directories WHERE instr(lower(path),lower(?1))>0 ORDER BY path COLLATE NOCASE LIMIT 500").map_err(error)?;
        let rows = q.query_map([search], |r| r.get(0)).map_err(error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(error)
    }
    pub fn each_directory(&self, mut visit: impl FnMut(String) -> bool) -> Result<(), String> {
        let db = self.connect()?;
        let mut query = db
            .prepare(
                "SELECT path FROM directories WHERE problem IS NULL ORDER BY path COLLATE NOCASE",
            )
            .map_err(error)?;
        let rows = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(error)?;
        for row in rows {
            if !visit(row.map_err(error)?) {
                break;
            }
        }
        Ok(())
    }
    pub fn start(
        &self,
        roots: Vec<String>,
        volumes: Vec<Volume>,
        resume: bool,
        include_generated: bool,
    ) -> Result<(), String> {
        let mut coordinator = self.worker_handle.lock().map_err(error)?;
        if self.running.load(Ordering::SeqCst) {
            return Err("An inventory is already running".into());
        }
        if let Some(handle) = coordinator.take() {
            handle
                .join()
                .map_err(|_| "Previous inventory coordinator panicked".to_owned())?;
        }
        if !resume {
            for root in &roots {
                filesystem::identify(root, &volumes)?;
            }
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return Err("An inventory is already running.".into());
        }
        self.cancel.store(false, Ordering::Relaxed);
        let init = (|| {
            let mut db = self.connect()?;
            let tx = db.transaction().map_err(error)?;
            tx.execute("UPDATE queue SET state='pending' WHERE state='working'", [])
                .map_err(error)?;
            tx.execute("DELETE FROM scan_progress", []).map_err(error)?;
            if !resume {
                tx.execute("DELETE FROM queue", []).map_err(error)?;
                for root in &roots {
                    tx.execute("INSERT OR IGNORE INTO queue(path,state) VALUES(?1,'pending')", [root])
                        .map_err(error)?;
                }
                tx.execute("UPDATE meta SET value=?1 WHERE key='scan'",[json!({"status":"running","startedAt":now(),"roots":roots,"includeGenerated":include_generated,"processed":0,"files":0,"bytes":0,"currentFiles":0,"currentBytes":0}).to_string()]).map_err(error)?;
            } else {
                let pending: i64 = tx
                    .query_row(
                        "SELECT count(*) FROM queue WHERE state='pending'",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(error)?;
                if pending == 0 {
                    return Err(
                        "No paused inventory remains to resume. Start a new inventory instead."
                            .into(),
                    );
                }
                tx.execute("UPDATE meta SET value=json_set(value,'$.status','running','$.error',null) WHERE key='scan'",[]).map_err(error)?;
            }
            tx.commit().map_err(error)
        })();
        if let Err(e) = init {
            self.running.store(false, Ordering::SeqCst);
            return Err(e);
        }
        let worker = Self {
            path: self.path.clone(),
            running: self.running.clone(),
            cancel: self.cancel.clone(),
            refresh_locks: self.refresh_locks.clone(),
            worker_handle: self.worker_handle.clone(),
            priority_plan: self.priority_plan.clone(),
            priority_applied: self.priority_applied.clone(),
        };
        *self.priority_applied.lock().map_err(error)? = None;
        let spawn = std::thread::Builder::new().name("migration-inventory".into()).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(&volumes)))
                .unwrap_or_else(|_| Err("Inventory stopped unexpectedly; the durable queue can be resumed".into()));
            if let Err(e) = result {
                if let Ok(db) = worker.connect() {
                    let _ = db.execute_batch("UPDATE queue SET state='pending' WHERE state='working'; DELETE FROM scan_progress;");
                    let _=db.execute("UPDATE meta SET value=json_set(value,'$.status','error','$.error',?1) WHERE key='scan'",[e]);
                }
            }
            worker.running.store(false, Ordering::SeqCst);
        });
        match spawn {
            Ok(handle) => *coordinator = Some(handle),
            Err(e) => {
                self.running.store(false, Ordering::SeqCst);
                if let Ok(db) = self.connect() {
                    let _ = db.execute("UPDATE meta SET value=json_set(value,'$.status','error','$.error',?1) WHERE key='scan'", [error(&e)]);
                }
                return Err(error(e));
            }
        }
        Ok(())
    }
    fn run(&self, volumes: &[Volume]) -> Result<(), String> {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 4);
        self.run_with_workers(volumes, workers)
    }
    fn run_with_workers(&self, volumes: &[Volume], workers: usize) -> Result<(), String> {
        let workers = workers.clamp(1, 8);
        let db = self.connect()?;
        db.execute(
            "UPDATE meta SET value=json_set(value,'$.workers',?1) WHERE key='scan'",
            [workers as i64],
        )
        .map_err(error)?;
        let raw: String = db.query_row("SELECT value FROM meta WHERE key='scan'", [],
            |row| row.get(0)).map_err(error)?;
        let scan: Value = serde_json::from_str(&raw).map_err(error)?;
        let filtered_roots: Vec<&str> = if scan["includeGenerated"] == true {
            Vec::new()
        } else {
            scan["roots"].as_array().into_iter().flatten().filter_map(Value::as_str).collect()
        };
        let filtered_roots = filtered_roots.as_slice();
        // Like the audit pool, bound work and retain the durable queue. Workers claim one
        // directory at a time; no growing in-memory list or unbounded thread creation.
        let result = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for n in 0..workers {
                let handle = std::thread::Builder::new().name(format!("migration-inventory-{n}")).spawn_scoped(scope, || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run_worker(volumes, filtered_roots)))
                        .unwrap_or_else(|_| Err("Inventory worker stopped unexpectedly; the durable queue can be resumed".into()));
                    if result.is_err() { self.cancel.store(true, Ordering::SeqCst); }
                    result
                });
                match handle {
                    Ok(handle) => handles.push(handle),
                    Err(e) => {
                        self.cancel.store(true, Ordering::SeqCst);
                        return Err(error(e));
                    }
                }
            }
            let mut failure = None;
            for handle in handles {
                match handle.join() {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        failure.get_or_insert(e);
                    }
                    Err(_) => {
                        failure.get_or_insert("Inventory worker stopped unexpectedly".into());
                    }
                }
            }
            failure.map_or(Ok(()), Err)
        });
        let tx = db.unchecked_transaction().map_err(error)?;
        tx.execute("UPDATE queue SET state='pending' WHERE state='working'", [])
            .map_err(error)?;
        tx.execute("DELETE FROM scan_progress", []).map_err(error)?;
        let status = if result.is_err() {
            "error"
        } else if self.cancel.load(Ordering::Relaxed) {
            "paused"
        } else {
            "complete"
        };
        tx.execute("UPDATE meta SET value=json_set(value,'$.status',?1,'$.finishedAt',?2,'$.currentPath',null,'$.currentFiles',0,'$.currentBytes',0) WHERE key='scan'", params![status, if status == "complete" { Some(now()) } else { None }]).map_err(error)?;
        tx.commit().map_err(error)?;
        result
    }
    fn run_worker(&self, volumes: &[Volume], filtered_roots: &[&str]) -> Result<(), String> {
        let mut db = self.connect()?;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Ok(());
            }
            // IMMEDIATE avoids concurrent read-to-write upgrades and duplicate claims.
            self.apply_priority(&mut db)?;
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(error)?;
            let path: Option<String> = tx.query_row("SELECT path FROM queue WHERE state='pending' ORDER BY priority,length(path),path LIMIT 1", [], |r| r.get(0)).optional().map_err(error)?;
            let Some(path) = path else {
                let working: bool = tx
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM queue WHERE state='working')",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(error)?;
                tx.commit().map_err(error)?;
                if !working {
                    return Ok(());
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
                continue;
            };
            tx.execute("UPDATE queue SET state='working' WHERE path=?1", [&path])
                .map_err(error)?;
            tx.execute(
                "INSERT OR REPLACE INTO scan_progress VALUES(?1,0,0)",
                [&path],
            )
            .map_err(error)?;
            tx.commit().map_err(error)?;
            if let Some(root) = generated_ancestor(&path, filtered_roots, volumes) {
                let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(error)?;
                let descendant_start = format!("{root}\\");
                let descendant_end = format!("{root}]");
                tx.execute("UPDATE queue SET state='skipped-child'
                    WHERE path>=?1 AND path<?2 AND state IN ('pending','working')",
                    params![descendant_start, descendant_end]).map_err(error)?;
                tx.execute("UPDATE queue SET state='skipped' WHERE path=?1", [root])
                    .map_err(error)?;
                tx.execute("DELETE FROM scan_progress WHERE path=?1 OR (path>=?2 AND path<?3)",
                    params![root, descendant_start, descendant_end]).map_err(error)?;
                tx.commit().map_err(error)?;
                continue;
            }
            let result = self.refresh(&path, volumes, Some(&self.cancel), false);
            if result.as_ref().is_err_and(|e| e == "Paused") {
                return Ok(());
            }
            let tx = db
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(error)?;
            let finished = tx.execute(
                "UPDATE queue SET state=?2 WHERE path=?1 AND state='working'",
                params![path, if result.is_ok() { "done" } else { "gap" }],
            )
            .map_err(error)?;
            tx.execute("DELETE FROM scan_progress WHERE path=?1", [&path])
                .map_err(error)?;
            if finished != 0 {
                tx.execute("UPDATE meta SET value=json_set(value,'$.processed',coalesce(json_extract(value,'$.processed'),0)+1,'$.files',coalesce(json_extract(value,'$.files'),0)+CASE WHEN ?2 THEN coalesce((SELECT files FROM directories WHERE path=?1),0) ELSE 0 END,'$.bytes',coalesce(json_extract(value,'$.bytes'),0)+CASE WHEN ?2 THEN coalesce((SELECT bytes FROM directories WHERE path=?1),0) ELSE 0 END) WHERE key='scan'",params![path,result.is_ok()]).map_err(error)?;
            }
            tx.commit().map_err(error)?;
        }
    }
}

fn generated_ancestor<'a>(path: &'a str, roots: &[&str], volumes: &[Volume]) -> Option<&'a str> {
    const GENERATED_NAMES: &[&str] = &[
        "node_modules", ".npm-cache", ".pnpm-store", "__pycache__", ".pytest_cache",
        ".ruff_cache", ".next", ".nuxt", ".svelte-kit",
    ];
    let scope = roots.iter().find(|scope| filesystem::under(path, scope))?;
    let mut generated = None;
    for ancestor in Path::new(path).ancestors() {
        let Some(raw) = ancestor.to_str() else { break };
        if !filesystem::under(raw, scope) {
            break;
        }
        let name = ancestor.file_name().and_then(|name| name.to_str()).unwrap_or("");
        let named = GENERATED_NAMES.iter().any(|generated| name.eq_ignore_ascii_case(generated));
        let cmake = name.eq_ignore_ascii_case("build");
        let cargo = name.eq_ignore_ascii_case("target");
        let npm_cache = name.eq_ignore_ascii_case("_cacache");
        if !(named || cmake || cargo || npm_cache) {
            continue;
        }
        let Ok((directory, _guards)) = filesystem::guard_path(raw, volumes, false) else {
            continue;
        };
        let file = |name: &str| fs::symlink_metadata(directory.join(name))
            .is_ok_and(|meta| meta.is_file() && !filesystem::reparse(&meta));
        let dir = |name: &str| fs::symlink_metadata(directory.join(name))
            .is_ok_and(|meta| meta.is_dir() && !filesystem::reparse(&meta));
        if (named && directory.is_dir())
            || (cmake && file("CMakeCache.txt") && dir("CMakeFiles"))
            || (cargo && file("CACHEDIR.TAG") && file(".rustc_info.json"))
            || (npm_cache && dir("content-v2") && dir("index-v5"))
        {
            generated = Some(raw);
        }
    }
    generated
}
fn touch_observations(db: &Connection, path: &str) -> Result<(), String> {
    db.execute(
        "UPDATE meta SET value=cast(value AS INTEGER)+1 WHERE key='observationSequence'",
        [],
    )
    .map_err(error)?;
    let seq: i64 = db
        .query_row(
            "SELECT cast(value AS INTEGER) FROM meta WHERE key='observationSequence'",
            [],
            |r| r.get(0),
        )
        .map_err(error)?;
    let mut write=db.prepare_cached("INSERT INTO observation_versions VALUES(?1,?2) ON CONFLICT(parent) DO UPDATE SET sequence=?2").map_err(error)?;
    for ancestor in std::path::Path::new(path)
        .ancestors()
        .take_while(|p| p.has_root())
    {
        write
            .execute(params![ancestor.to_string_lossy().to_lowercase(), seq])
            .map_err(error)?;
    }
    Ok(())
}
pub(crate) fn observation_version(db: &Connection, path: &str) -> Result<i64, String> {
    db.query_row(
        "SELECT coalesce((SELECT sequence FROM observation_versions WHERE parent=?1),0)",
        [path.to_lowercase()],
        |r| r.get(0),
    )
    .map_err(error)
}

#[cfg(test)]
mod membership_tests {
    use super::*;

    fn generation(db: &Connection, path: &str) -> String {
        db.query_row(
            "SELECT generation FROM observation_heads WHERE parent=?1",
            [path.to_lowercase()],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn staged_members_are_not_published_by_a_paused_refresh() {
        let s = crate::tests::fixture();
        let dir = PathBuf::from(&s.volumes[0].root).join("Work").join("Child");
        let file = dir.join("observed.txt");
        fs::write(&file, b"known member").unwrap();
        let path = dir.to_string_lossy().into_owned();
        s.index.refresh(&path, &s.volumes, None, false).unwrap();
        let db = s.index.connect().unwrap();
        let published = generation(&db, &path);
        let mut staged = filesystem::inspect(&file);
        staged.name = "staged-only.txt".into();
        staged.path = dir.join(&staged.name).to_string_lossy().into_owned();
        s.index
            .flush_chunk(&path, "test-unpublished", &[staged], false, false, 1, 1)
            .unwrap();
        let visible: Vec<String> = {
            let mut q=db.prepare("SELECT name FROM observation_members WHERE parent=?1 AND generation=(SELECT generation FROM observation_heads WHERE parent=?1)").unwrap();
            q.query_map([path.to_lowercase()], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(visible, vec!["observed.txt"]);
        let cancelled = AtomicBool::new(true);
        assert_eq!(
            s.index
                .refresh(&path, &s.volumes, Some(&cancelled), false)
                .unwrap_err(),
            "Paused"
        );
        assert_eq!(generation(&db, &path), published);
        let remaining: i64 = db
            .query_row(
                "SELECT count(*) FROM observation_members WHERE parent=?1",
                [path.to_lowercase()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn directory_changed_before_publication_keeps_previous_membership() {
        use std::{
            fs::{FileTimes, OpenOptions},
            os::windows::fs::OpenOptionsExt,
            time::Duration,
        };
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_BACKUP_SEMANTICS, FILE_WRITE_ATTRIBUTES,
        };
        let s = crate::tests::fixture();
        let dir = PathBuf::from(&s.volumes[0].root).join("Work").join("Child");
        fs::write(dir.join("before.txt"), b"original").unwrap();
        let path = dir.to_string_lossy().into_owned();
        s.index.refresh(&path, &s.volumes, None, false).unwrap();
        let db = s.index.connect().unwrap();
        let published = generation(&db, &path);
        let before = fs::metadata(&dir).unwrap().modified().unwrap();
        let failure = s
            .index
            .refresh_observing(&path, &s.volumes, None, false, || {
                fs::write(dir.join("added-during-enumeration.txt"), b"new member").unwrap();
                OpenOptions::new()
                    .access_mode(FILE_WRITE_ATTRIBUTES)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                    .open(&dir)
                    .unwrap()
                    .set_times(FileTimes::new().set_modified(before + Duration::from_secs(2)))
                    .unwrap();
            })
            .unwrap_err();
        assert!(failure.contains("changed during enumeration"), "{failure}");
        assert_eq!(generation(&db, &path), published);
        s.index.refresh(&path, &s.volumes, None, false).unwrap();
        assert_ne!(generation(&db, &path), published);
        let visible:i64=db.query_row("SELECT count(*) FROM observation_members WHERE parent=?1 AND generation=(SELECT generation FROM observation_heads WHERE parent=?1)",[path.to_lowercase()],|r|r.get(0)).unwrap();
        assert_eq!(visible, 2);
    }
}
fn sqlite_count(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| {
        "An indexed count exceeds SQLite's supported integer range; coverage remains incomplete"
            .into()
    })
}
pub(super) fn nonnegative(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(e),
        )
    })
}
fn optional_count(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|value| nonnegative(value, index))
        .transpose()
}
fn entry_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        path: r.get(0)?,
        name: r.get(1)?,
        kind: r.get(2)?,
        size: nonnegative(r.get::<_, i64>(3)?, 3)?,
        modified: r.get(4)?,
        fingerprint: r.get(5)?,
        problem: r.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn marked_folders_are_deferred_and_explicit_attention_restores_normal_priority() {
        let s = crate::tests::fixture();
        let root = &s.volumes[0].root;
        let marked = format!("{root}\\Work");
        let exception = format!("{marked}\\Child");
        let unmarked = format!("{root}\\Other");
        let volume = &s.volumes[0].id;
        let mut db = s.index.connect().unwrap();
        for path in [&marked, &exception, &unmarked] {
            db.execute("INSERT INTO queue(path,state) VALUES(?1,'pending')", [path]).unwrap();
        }
        let mut policy = serde_json::json!({"revision":1,"decisions":{
            "marked":{"volumeId":volume,"relativePath":"Work","status":"last-moment"},
            "exception":{"volumeId":volume,"relativePath":"Work\\Child","status":"attention"}
        }});
        s.index.update_priority(&policy, &s.volumes);
        s.index.apply_priority(&mut db).unwrap();
        let priority = |db: &rusqlite::Connection, path: &str| -> i64 {
            db.query_row("SELECT priority FROM queue WHERE path=?1", [path], |row| row.get(0)).unwrap()
        };
        assert_eq!(priority(&db, &marked), 1);
        assert_eq!(priority(&db, &exception), 0);
        assert_eq!(priority(&db, &unmarked), 0);
        policy["revision"] = serde_json::json!(2);
        policy["inventoryDeferMarked"] = serde_json::json!(false);
        s.index.update_priority(&policy, &s.volumes);
        s.index.apply_priority(&mut db).unwrap();
        assert_eq!(priority(&db, &marked), 0);
        let pending: i64 = db.query_row("SELECT count(*) FROM queue WHERE state='pending'", [], |row| row.get(0)).unwrap();
        assert_eq!(pending, 3);
    }

    use super::*;
    fn fixture(label: &str) -> (Index, Vec<Volume>, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "migration-index-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let source = base.join("source");
        fs::create_dir_all(&source).unwrap();
        let index = Index::open(base.join("index.sqlite")).unwrap();
        let volumes = filesystem::volumes(Some(&source));
        (index, volumes, source)
    }
    fn path(p: &Path) -> String {
        p.to_string_lossy().into()
    }
    #[test]
    fn streaming_and_cancel_preserve_complete_listing() {
        let (i, v, p) = fixture("stream");
        for n in 0..1205 {
            fs::write(p.join(format!("file-{n:04}")), b"x").unwrap();
        }
        i.refresh(&path(&p), &v, None, true).unwrap();
        let mut count = 0;
        i.each_entry(&path(&p), "", |_| {
            count += 1;
            count < 100
        })
        .unwrap();
        assert_eq!(count, 100);
        fs::write(p.join("new-file"), b"new").unwrap();
        // Simulate an interrupted refresh that already committed a staging chunk.
        i.flush_chunk(
            &path(&p),
            "unpublished-fixture",
            &[filesystem::inspect(&p.join("new-file"))],
            true,
            false,
            1,
            3,
        )
        .unwrap();
        let cancel = AtomicBool::new(true);
        assert_eq!(
            i.refresh(&path(&p), &v, Some(&cancel), true).unwrap_err(),
            "Paused"
        );
        let mut count = 0;
        i.each_entry(&path(&p), "", |_| {
            count += 1;
            true
        })
        .unwrap();
        assert_eq!(count, 1205);
        i.refresh(&path(&p), &v, None, true).unwrap();
        let mut count = 0;
        i.each_entry(&path(&p), "", |_| {
            count += 1;
            true
        })
        .unwrap();
        assert_eq!(count, 1206);
    }
    #[test]
    fn rescan_records_missing_dirs() {
        let (i, v, p) = fixture("rescan");
        let child = p.join("child");
        fs::create_dir(&child).unwrap();
        i.refresh(&path(&p), &v, None, true).unwrap();
        let before = i.signature(&path(&p));
        // Removal is restricted to this test-created empty directory.
        fs::remove_dir(&child).unwrap();
        i.refresh(&path(&p), &v, None, true).unwrap();
        assert_ne!(i.signature(&path(&p)), before);
        assert!(i.directory_info(&path(&child)).unwrap().unwrap()["problem"]
            .as_str()
            .unwrap()
            .contains("Missing"));
    }
    #[test]
    fn resume_queue_keeps_files_as_summaries() {
        let (i, v, p) = fixture("resume");
        fs::create_dir(p.join("child")).unwrap();
        fs::write(p.join("loose"), b"fixture").unwrap();
        let db = i.connect().unwrap();
        db.execute("INSERT INTO queue(path,state) VALUES(?1,'pending')", [path(&p)])
            .unwrap();
        db.execute(
            "UPDATE meta SET value=?1 WHERE key='scan'",
            [json!({"status":"paused","processed":0,"files":0,"bytes":0}).to_string()],
        )
        .unwrap();
        i.running.store(true, Ordering::Relaxed);
        i.run(&v).unwrap();
        i.running.store(false, Ordering::Relaxed);
        let s = i.scan_status().unwrap();
        assert_eq!(s["status"], "complete");
        assert_eq!(s["processed"], 2);
        assert_eq!(s["files"], 1);
        assert_eq!(s["pending"], 0);
        assert_eq!(
            db.query_row("SELECT count(*) FROM listing_entries", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn invalid_start_preserves_the_resumable_queue() {
        let (i, v, p) = fixture("invalid-start");
        let db = i.connect().unwrap();
        db.execute("INSERT INTO queue(path,state) VALUES(?1,'pending')", [path(&p)])
            .unwrap();
        assert!(i
            .start(vec![r"Z:\outside-review-root".into()], v, false, false)
            .is_err());
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM queue WHERE state='pending'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert!(!i.running.load(Ordering::Relaxed));
    }
    #[test]
    fn npm_cache_requires_its_layout_and_can_be_included_explicitly() {
        let (index, volumes, source) = fixture("npm-cache-policy");
        let npm = source.join(".caches").join("npm");
        let real_cache = npm.join("_cacache");
        let lookalike = source.join("ordinary").join("_cacache");
        for directory in [real_cache.join("content-v2").join("sha512"), real_cache.join("index-v5"),
            lookalike.join("content-v2")] {
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("example"), b"fixture").unwrap();
        }
        fs::write(npm.join("notes.txt"), b"user fixture").unwrap();
        for (include_generated, files, skipped) in [(false, 2, 1), (true, 4, 0)] {
            index.start(vec![path(&source)], volumes.clone(), false, include_generated).unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            while index.running.load(Ordering::SeqCst) {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let status = index.scan_status().unwrap();
            assert_eq!(status["status"], "complete");
            assert_eq!(status["files"], files);
            assert_eq!(status["skippedGenerated"], skipped);
            assert!(!index.skipped_ancestor(&path(&npm)).unwrap());
            assert!(!index.skipped_ancestor(&path(&lookalike)).unwrap());
            assert_eq!(index.skipped_ancestor(&path(&real_cache)).unwrap(), !include_generated);
        }
    }

    #[test]
    fn legacy_inventory_resume_skips_queued_generated_descendants_and_keeps_other_work() {
        let (index, volumes, source) = fixture("generated-resume");
        let generated = source.join("NODE_MODULES");
        let retained = generated.join("already-done");
        let queued = generated.join("queued");
        let ordinary = source.join("node_modules-backup");
        for folder in [&retained, &queued, &ordinary] {
            fs::create_dir_all(folder).unwrap();
            fs::write(folder.join("file"), b"fixture").unwrap();
        }
        let db = index.connect().unwrap();
        db.execute("UPDATE meta SET value=?1 WHERE key='scan'", [json!({
            "status":"paused","roots":[path(&source)],"startedAt":now(),"processed":3,"files":1
        }).to_string()]).unwrap();
        for (folder, state) in [(&source, "done"), (&generated, "done"), (&retained, "done"),
            (&queued, "pending"), (&ordinary, "pending")] {
            db.execute("INSERT INTO queue(path,state) VALUES(?1,?2)", params![path(folder),state]).unwrap();
        }
        index.start(Vec::new(), volumes, true, false).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while index.running.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let state = |folder: &Path| db.query_row("SELECT state FROM queue WHERE path=?1",
            [path(folder)], |row| row.get::<_, String>(0)).unwrap();
        assert_eq!(state(&generated), "skipped");
        assert_eq!(state(&retained), "done");
        assert_eq!(state(&queued), "skipped-child");
        assert_eq!(state(&ordinary), "done");
        assert!(index.skipped_ancestor(&path(&retained)).unwrap());
        assert!(!index.skipped_ancestor(&path(&ordinary)).unwrap());
        let status = index.scan_status().unwrap();
        assert_eq!(status["status"], "complete");
        assert_eq!(status["processed"], 4);
        assert_eq!(status["files"], 2);
        assert_eq!(status["pending"], 0);
        assert_eq!(status["skippedGenerated"], 1);
        assert!(index.gaps().unwrap().as_array().unwrap().iter().any(|gap|
            gap["path"] == path(&generated) && gap["problem"].as_str().unwrap().contains("Generated folder skipped")));
    }
    #[cfg(windows)]
    #[test]
    fn junction_cycle_is_a_visible_gap_and_is_not_followed() {
        use std::os::windows::process::CommandExt;
        let (i, v, p) = fixture("junction");
        let junction = p.join("cycle");
        let output = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J", &path(&junction), &path(&p)])
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Fixture junction could not be created: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        i.refresh(&path(&p), &v, None, true).unwrap();
        let mut entries = Vec::new();
        i.each_entry(&path(&p), "", |e| {
            entries.push(e);
            true
        })
        .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "link");
        assert!(filesystem::safe(&path(&junction), &v, false).is_err());
        assert!(
            i.directory_info(&path(&junction)).unwrap().unwrap()["problem"]
                .as_str()
                .unwrap()
                .contains("not followed")
        );
        assert_eq!(i.scan_status().unwrap()["pending"], 0);
    }
    #[test]
    #[ignore = "Synthetic fixture benchmark; never scans live data"]
    fn inventory_synthetic_benchmark() {
        let (i, v, p) = fixture("benchmark");
        for d in 0..160 {
            let child = p.join(format!("dir-{d:04}"));
            fs::create_dir(&child).unwrap();
            for f in 0..40 {
                fs::write(child.join(format!("file-{f:04}")), b"synthetic").unwrap();
            }
        }
        for workers in [1, 4] {
            for attempt in 0..3 {
                let i = Index::open(
                    i.path
                        .with_file_name(format!("bench-{workers}-{attempt}.sqlite")),
                )
                .unwrap();
                let db = i.connect().unwrap();
                db.execute("INSERT INTO queue(path,state) VALUES(?1,'pending')", [path(&p)])
                    .unwrap();
                i.running.store(true, Ordering::Relaxed);
                let started = std::time::Instant::now();
                i.run_with_workers(&v, workers).unwrap();
                let elapsed_ms = started.elapsed().as_millis();
                i.running.store(false, Ordering::Relaxed);
                i.checkpoint().unwrap();
                let size = fs::metadata(&i.path).unwrap().len();
                let members: i64 = db
                    .query_row("SELECT count(*) FROM observation_members", [], |r| r.get(0))
                    .unwrap();
                let heads: i64 = db
                    .query_row("SELECT count(*) FROM observation_heads", [], |r| r.get(0))
                    .unwrap();
                let listings: i64 = db
                    .query_row("SELECT count(*) FROM listing_entries", [], |r| r.get(0))
                    .unwrap();
                assert_eq!((members, heads, listings), (6560, 161, 0));
                eprintln!(
                    "INVENTORY_BENCH workers={workers} attempt={attempt} elapsed_ms={elapsed_ms} checkpointed_index_bytes={size} members={members} heads={heads} listing_rows={listings} status={}",
                    i.scan_status().unwrap()
                );
                assert_eq!(i.scan_status().unwrap()["processed"], 161);
                assert_eq!(i.scan_status().unwrap()["files"], 6400);
            }
        }
    }
    #[test]
    fn interrupted_claim_is_recovered_without_double_counting() {
        let (i, v, p) = fixture("crash-recovery");
        fs::write(p.join("file"), b"fixture").unwrap();
        let db = i.connect().unwrap();
        db.execute("INSERT INTO queue(path,state) VALUES(?1,'working')", [path(&p)])
            .unwrap();
        db.execute("INSERT INTO scan_progress VALUES(?1,999,999)", [path(&p)])
            .unwrap();
        db.execute(
            "UPDATE meta SET value=?1 WHERE key='scan'",
            [json!({"status":"running","processed":0,"files":0}).to_string()],
        )
        .unwrap();
        drop(db);
        let database = i.path.clone();
        drop(i);
        let i = Index::open(database).unwrap();
        let status = i.scan_status().unwrap();
        assert_eq!(status["status"], "paused");
        assert_eq!(status["pending"], 1);
        assert_eq!(status["active"], 0);
        assert_eq!(status["currentFiles"], 0);
        i.running.store(true, Ordering::Relaxed);
        i.run_with_workers(&v, 4).unwrap();
        let status = i.scan_status().unwrap();
        assert_eq!(status["processed"], 1);
        assert_eq!(status["files"], 1);
        assert_eq!(status["active"], 0);
        assert_eq!(status["pending"], 0);
    }
    #[test]
    fn parallel_pause_drains_claims_and_resume_counts_each_directory_once() {
        let (i, v, p) = fixture("parallel-pause");
        for n in 0..48 {
            let child = p.join(format!("dir-{n}"));
            fs::create_dir(&child).unwrap();
            for f in 0..32 {
                fs::write(child.join(format!("file-{f}")), b"x").unwrap();
            }
        }
        let db = i.connect().unwrap();
        db.execute("INSERT INTO queue(path,state) VALUES(?1,'pending')", [path(&p)])
            .unwrap();
        i.running.store(true, Ordering::Relaxed);
        std::thread::scope(|scope| {
            let scan = scope.spawn(|| i.run_with_workers(&v, 4));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                let status = i.scan_status().unwrap();
                assert!(status["active"].as_u64().unwrap() <= 4);
                if status["active"].as_u64().unwrap() >= 2 {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "workers never overlapped: {status}"
                );
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            i.cancel.store(true, Ordering::Relaxed);
            scan.join().unwrap().unwrap();
        });
        let status = i.scan_status().unwrap();
        assert_eq!(status["status"], "paused");
        assert_eq!(status["active"], 0);
        assert_eq!(status["currentFiles"], 0);
        assert!(status["pending"].as_u64().unwrap() > 0);
        assert_eq!(status["totalKnown"], false);
        i.cancel.store(false, Ordering::Relaxed);
        i.run_with_workers(&v, 4).unwrap();
        let status = i.scan_status().unwrap();
        assert_eq!(status["status"], "complete");
        assert_eq!(status["processed"], 49);
        assert_eq!(status["files"], 1536);
        assert_eq!(status["gaps"], 0);
        assert_eq!(status["active"], 0);
        assert_eq!(status["pending"], 0);
        assert_eq!(
            db.query_row("SELECT count(*) FROM listing_entries", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}
