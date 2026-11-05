use crate::{
    filesystem::{self, Entry, Volume},
    store::{error, hash, now},
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

const MISSING: &str = "Missing or changed type since the previous observation; retained for review";
pub struct Index {
    pub path: PathBuf,
    pub running: Arc<AtomicBool>,
    pub cancel: Arc<AtomicBool>,
    refresh_locks: Arc<[Mutex<()>; 64]>,
}
impl Index {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(path.parent().ok_or("Index path has no parent")?).map_err(error)?;
        let s = Self {
            path,
            running: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            refresh_locks: Arc::new(std::array::from_fn(|_| Mutex::new(()))),
        };
        let db = s.connect()?;
        db.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS directories(path TEXT PRIMARY KEY COLLATE NOCASE,signature TEXT,changed_at TEXT,observed_at TEXT,files INTEGER,folders INTEGER,bytes INTEGER,problem TEXT);
            CREATE TABLE IF NOT EXISTS directory_children(parent TEXT COLLATE NOCASE,path TEXT COLLATE NOCASE,generation TEXT,PRIMARY KEY(parent,path));
            CREATE INDEX IF NOT EXISTS child_parent ON directory_children(parent,generation);
            CREATE TABLE IF NOT EXISTS listing_entries(parent TEXT COLLATE NOCASE,generation TEXT,path TEXT COLLATE NOCASE,name TEXT,kind TEXT,size INTEGER,modified TEXT,fingerprint TEXT,problem TEXT,PRIMARY KEY(parent,generation,path));
            CREATE INDEX IF NOT EXISTS listing_order ON listing_entries(parent,generation,kind,name COLLATE NOCASE);
            CREATE TABLE IF NOT EXISTS listing_heads(parent TEXT PRIMARY KEY COLLATE NOCASE,generation TEXT,observed_at TEXT);
            CREATE TABLE IF NOT EXISTS queue(path TEXT PRIMARY KEY COLLATE NOCASE,state TEXT);
            CREATE INDEX IF NOT EXISTS queue_state ON queue(state);
            CREATE INDEX IF NOT EXISTS queue_order ON queue(state,length(path),path);
            CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT);
            INSERT OR IGNORE INTO meta VALUES('scan','{}');
            UPDATE meta SET value=json_set(value,'$.status','paused') WHERE key='scan' AND json_extract(value,'$.status')='running';").map_err(error)?;
        Ok(s)
    }
    pub fn connect(&self) -> Result<Connection, String> {
        let c = Connection::open(&self.path).map_err(error)?;
        c.busy_timeout(std::time::Duration::from_secs(15))
            .map_err(error)?;
        Ok(c)
    }
    pub fn scan_status(&self) -> Result<Value, String> {
        let db = self.connect()?;
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
        v["gaps"] = json!(db
            .query_row(
                "SELECT count(*) FROM directories WHERE problem IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .map_err(error)?);
        Ok(v)
    }
    pub fn gaps(&self) -> Result<Value, String> {
        let db = self.connect()?;
        let mut q=db.prepare("SELECT path,problem,observed_at FROM directories WHERE problem IS NOT NULL ORDER BY observed_at DESC LIMIT 200").map_err(error)?;
        let rows=q.query_map([],|r|Ok(json!({"path":r.get::<_,String>(0)?,"problem":r.get::<_,String>(1)?,"observedAt":r.get::<_,Option<String>>(2)?}))).map_err(error)?;
        Ok(Value::Array(
            rows.collect::<Result<Vec<_>, _>>().map_err(error)?,
        ))
    }
    pub fn directory_info(&self, path: &str) -> Result<Option<Value>, String> {
        self.connect()?.query_row("SELECT observed_at,files,folders,bytes,problem,signature,(SELECT observed_at FROM listing_heads WHERE parent=?1) FROM directories WHERE path=?1",[path],|r|Ok(json!({"observedAt":r.get::<_,Option<String>>(0)?,"files":optional_count(r,1)?,"folders":optional_count(r,2)?,"bytes":optional_count(r,3)?,"problem":r.get::<_,Option<String>>(4)?,"signature":r.get::<_,Option<String>>(5)?,"listingObservedAt":r.get::<_,Option<String>>(6)?}))).optional().map_err(error)
    }
    pub fn record_gap(&self, path: &str, problem: &str) -> Result<(), String> {
        self.connect()?.execute("INSERT INTO directories(path,problem,observed_at,changed_at) VALUES(?1,?2,?3,?3) ON CONFLICT(path) DO UPDATE SET problem=?2,observed_at=?3,changed_at=CASE WHEN coalesce(problem,'')<>?2 THEN ?3 ELSE changed_at END",params![path,problem,now()]).map_err(error)?;
        Ok(())
    }
    // Only immediate children are enumerated. A staged generation becomes visible atomically.
    // Background inventory persists directory summaries, never every file entry.
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
        let (p, _path_guards) = filesystem::guard_path(path, volumes, false)?;
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
                self.flush_chunk(
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
        self.flush_chunk(
            path,
            &generation,
            &chunk,
            cache_files,
            scanning,
            files,
            bytes,
        )?;
        if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Err("Paused".into());
        }
        let signature = hash(format!("{:?}|{files}|{folders}|{bytes}|{errors}", hashes).as_bytes());
        let problem = if errors == 0 {
            None
        } else {
            Some(format!(
                "{errors} directory enumeration errors. First {}: {}",
                samples.len(),
                samples.join("; ")
            ))
        };
        let mut db = self.connect()?;
        let tx = db.transaction().map_err(error)?;
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
        if cache_files && errors == 0 {
            tx.execute("INSERT INTO listing_heads VALUES(?1,?2,?3) ON CONFLICT(parent) DO UPDATE SET generation=?2,observed_at=?3",params![path,generation,observed]).map_err(error)?;
        }
        tx.commit().map_err(error)?;
        if errors > 0 && cache_files {
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
        let mut db = self.connect()?;
        let tx = db.transaction().map_err(error)?;
        let observed = now();
        for e in entries {
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
                        "INSERT OR IGNORE INTO queue(path,state) VALUES(?1,'pending')",
                        [&e.path],
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
            tx.execute("UPDATE meta SET value=json_set(value,'$.currentFiles',?1,'$.currentBytes',?2) WHERE key='scan'",params![sqlite_count(files)?,sqlite_count(bytes)?]).map_err(error)?;
        }
        tx.commit().map_err(error)
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
    ) -> Result<(), String> {
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
            if !resume {
                tx.execute("DELETE FROM queue", []).map_err(error)?;
                for root in &roots {
                    tx.execute("INSERT OR IGNORE INTO queue VALUES(?1,'pending')", [root])
                        .map_err(error)?;
                }
                tx.execute("UPDATE meta SET value=?1 WHERE key='scan'",[json!({"status":"running","startedAt":now(),"roots":roots,"processed":0,"files":0,"bytes":0,"currentFiles":0,"currentBytes":0}).to_string()]).map_err(error)?;
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
        };
        std::thread::spawn(move || {
            let result = worker.run(&volumes);
            if let Err(e) = result {
                if let Ok(db) = worker.connect() {
                    let _=db.execute("UPDATE meta SET value=json_set(value,'$.status','error','$.error',?1) WHERE key='scan'",[e]);
                }
            }
            worker.running.store(false, Ordering::SeqCst);
        });
        Ok(())
    }
    fn run(&self, volumes: &[Volume]) -> Result<(), String> {
        let db = self.connect()?;
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                db.execute(
                    "UPDATE meta SET value=json_set(value,'$.status','paused') WHERE key='scan'",
                    [],
                )
                .map_err(error)?;
                return Ok(());
            }
            let path:Option<String>=db.query_row("SELECT path FROM queue WHERE state='pending' ORDER BY length(path),path LIMIT 1",[],|r|r.get(0)).optional().map_err(error)?;
            let Some(path) = path else {
                db.execute("UPDATE meta SET value=json_set(value,'$.status','complete','$.finishedAt',?1,'$.currentPath',null,'$.currentFiles',0,'$.currentBytes',0) WHERE key='scan'",[now()]).map_err(error)?;
                return Ok(());
            };
            db.execute("UPDATE meta SET value=json_set(value,'$.currentPath',?1,'$.currentFiles',0,'$.currentBytes',0) WHERE key='scan'",[&path]).map_err(error)?;
            let result = self.refresh(&path, volumes, Some(&self.cancel), false);
            if result.as_ref().is_err_and(|e| e == "Paused") {
                continue;
            }
            let tx = db.unchecked_transaction().map_err(error)?;
            tx.execute(
                "UPDATE queue SET state=?2 WHERE path=?1",
                params![path, if result.is_ok() { "done" } else { "gap" }],
            )
            .map_err(error)?;
            tx.execute("UPDATE meta SET value=json_set(value,'$.processed',coalesce(json_extract(value,'$.processed'),0)+1,'$.files',coalesce(json_extract(value,'$.files'),0)+CASE WHEN ?2 THEN coalesce((SELECT files FROM directories WHERE path=?1),0) ELSE 0 END,'$.bytes',coalesce(json_extract(value,'$.bytes'),0)+CASE WHEN ?2 THEN coalesce((SELECT bytes FROM directories WHERE path=?1),0) ELSE 0 END) WHERE key='scan'",params![path,result.is_ok()]).map_err(error)?;
            tx.commit().map_err(error)?;
        }
    }
}
fn sqlite_count(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| {
        "An indexed count exceeds SQLite's supported integer range; coverage remains incomplete"
            .into()
    })
}
fn nonnegative(value: i64, index: usize) -> rusqlite::Result<u64> {
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
    use super::*;
    use std::path::Path;
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
        db.execute("INSERT INTO queue VALUES(?1,'pending')", [path(&p)])
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
        db.execute("INSERT INTO queue VALUES(?1,'pending')", [path(&p)])
            .unwrap();
        assert!(i
            .start(vec![r"Z:\outside-review-root".into()], v, false)
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
}
