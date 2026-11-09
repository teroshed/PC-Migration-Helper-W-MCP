//! Independent record of copy admissions. This authority stays with the
//! installation when a journal/backup pair is restored.
use crate::store::{self, Store};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::{
    fs::{self,File,OpenOptions}, io::{Read,Write},
    ops::{Deref,DerefMut},
    os::windows::{fs::OpenOptionsExt,io::AsRawHandle},
    path::{Path,PathBuf}, time::Duration,
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle,BY_HANDLE_FILE_INFORMATION,FILE_FLAG_OPEN_NO_RECALL,
    FILE_FLAG_OPEN_REPARSE_POINT,FILE_SHARE_READ,FILE_SHARE_WRITE,
};

const MARKER_BYTES: &[u8] = b"PC Migration Helper copy effect authority v1\n";

fn marker(path: &Path) -> PathBuf { path.with_extension("json.copy-authority") }
fn database(path: &Path) -> PathBuf { path.with_extension("json.copy-effects.sqlite") }

fn regular(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !crate::filesystem::reparse(&metadata) => Ok(true),
        Ok(_) => Err("A copy authority artifact is not a regular app-owned file.".into()),
        Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(problem) => Err(store::error(problem)),
    }
}

fn marker_state(path: &Path) -> Result<bool, String> {
    let path = marker(path);
    if !regular(&path)? { return Ok(false); }
    let _parents = crate::copy_queue::guard_copy_state_directory(path.parent().ok_or("Missing copy history parent")?)?;
    let file = open_artifact(&path,false)?;
    let mut contents = Vec::new();
    file.take(MARKER_BYTES.len() as u64+1).read_to_end(&mut contents).map_err(store::error)?;
    if contents != MARKER_BYTES {
        return Err("The independent copy authority marker is missing or damaged.".into());
    }
    Ok(true)
}

fn open_artifact(path: &Path, create: bool) -> Result<File,String> {
    let file = OpenOptions::new().read(true).write(create).create_new(create)
        .share_mode(FILE_SHARE_READ|FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT|FILE_FLAG_OPEN_NO_RECALL)
        .open(path).map_err(store::error)?;
    let meta = file.metadata().map_err(store::error)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let read = unsafe {GetFileInformationByHandle(file.as_raw_handle(),&mut info)};
    if !meta.is_file() || crate::filesystem::reparse(&meta) || read==0 || info.nNumberOfLinks!=1 {
        return Err("The copy authority artifact is not an independent regular file.".into());
    }
    Ok(file)
}

struct LedgerConnection {
    db: Connection,
    _parent: Vec<File>,
    _artifact: File,
}
impl Deref for LedgerConnection {
    type Target = Connection;
    fn deref(&self)->&Connection { &self.db }
}
impl DerefMut for LedgerConnection {
    fn deref_mut(&mut self)->&mut Connection { &mut self.db }
}

fn connect(path: &Path, write: bool, create: bool) -> Result<LedgerConnection, String> {
    let file = database(path);
    let parent = crate::copy_queue::guard_copy_state_directory(file.parent().ok_or("Missing copy history parent")?)?;
    let artifact = open_artifact(&file,create).map_err(|cause|
        format!("The independent copy effect ledger is unavailable: {cause}"))?;
    let flags = if write { OpenFlags::SQLITE_OPEN_READ_WRITE } else { OpenFlags::SQLITE_OPEN_READ_ONLY };
    let db = Connection::open_with_flags(file, flags).map_err(store::error)?;
    db.busy_timeout(Duration::from_secs(2)).map_err(store::error)?;
    if write { db.execute_batch("PRAGMA synchronous=FULL").map_err(store::error)?; }
    Ok(LedgerConnection { db,_parent:parent,_artifact:artifact })
}

fn identity(db: &Connection) -> Result<String, String> {
    let id = db.query_row("SELECT id FROM lineage WHERE slot=1 AND schema=1", [], |row| row.get::<_,String>(0))
        .map_err(store::error)?;
    if id.len() != 64 || !id.bytes().all(|value| value.is_ascii_hexdigit()) {
        return Err("The copy effect ledger identity is invalid.".into());
    }
    Ok(id)
}

fn ensure_marker(path: &Path) -> Result<(), String> {
    if marker_state(path)? { return Ok(()); }
    let marker = marker(path);
    let _parents = crate::copy_queue::guard_copy_state_directory(marker.parent().ok_or("Missing copy history parent")?)?;
    let mut file = open_artifact(&marker,true)?;
    file.write_all(MARKER_BYTES).map_err(store::error)?;
    file.sync_all().map_err(store::error)
}

fn receipt_projection(value: &Value) -> Result<Value, String> {
    let id = value["operationId"].as_str().filter(|id| !id.is_empty() && id.len() <= 200)
        .ok_or("A copy operation receipt is missing its exact ID.")?;
    let hash = value["payloadHash"].as_str().filter(|hash|
        hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("A copy operation receipt is missing its exact request digest.")?;
    let revision = value["committedRevision"].as_u64().or_else(|| value["intentRevision"].as_u64())
        .ok_or("A copy operation receipt is missing its durable revision.")?;
    Ok(json!({"operationId":id,"payloadHash":hash,"committedRevision":revision,
        "action":value["action"],"jobId":value["jobId"],"phase":value["phase"],
        "instanceId":value["instanceId"],"mode":value["mode"],"sourceRevision":value["sourceRevision"],
        "destinationId":value["destinationId"],"sourceVolumeSignature":value["sourceVolumeSignature"],
        "queueSignature":value["queueSignature"],"requestedAt":value["requestedAt"]}))
}

fn insert(db: &Connection, receipt: &Value) -> Result<(), String> {
    let value = receipt_projection(receipt)?;
    let id = value["operationId"].as_str().unwrap();
    let body = serde_json::to_string(&value).map_err(store::error)?;
    let prior = db.query_row("SELECT receipt FROM admissions WHERE operation_id=?1",[id], |row| row.get::<_,String>(0))
        .optional().map_err(store::error)?;
    if let Some(prior) = prior {
        if prior == body { return Ok(()); }
        return Err("The independent copy operation ID belongs to a different recorded receipt.".into());
    }
    db.execute("INSERT INTO admissions VALUES(?1,?2)", params![id,body]).map_err(store::error)?;
    db.execute("UPDATE lineage SET admitted=admitted+1 WHERE slot=1",[]).map_err(store::error)?;
    Ok(())
}

/// Establish an independent ledger before the first effect under this build.
/// A crash between marker, database and journal leaves admission blocked.
pub(crate) fn initialize(store: &Store) -> Result<String, String> {
    if marker_state(&store.path)? || regular(&database(&store.path))? {
        return Err("Existing copy authority must be reconciled; it cannot be initialized again.".into());
    }
    ensure_marker(&store.path)?;
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(store::error)?;
    let id = store::hash(&random);
    let mut db = connect(&store.path, true, true)?;
    let tx = db.transaction().map_err(store::error)?;
    tx.execute_batch("CREATE TABLE lineage(slot INTEGER PRIMARY KEY CHECK(slot=1), id TEXT NOT NULL, schema INTEGER NOT NULL,
        admitted INTEGER NOT NULL CHECK(admitted>=0));
        CREATE TABLE admissions(operation_id TEXT PRIMARY KEY, receipt TEXT NOT NULL) WITHOUT ROWID;")
        .map_err(store::error)?;
    tx.execute("INSERT INTO lineage VALUES(1,?1,1,0)",[&id]).map_err(store::error)?;
    if let Some(items) = store.doc.get("copyOperations") {
        let items = items.as_object().ok_or("The existing copy receipt collection is malformed.")?;
        for (key, receipt) in items {
            if receipt["operationId"] != *key { return Err("An existing copy receipt does not match its ID.".into()); }
            insert(&tx, receipt)?;
        }
    }
    tx.commit().map_err(store::error)?;
    Ok(id)
}

pub(crate) fn record_admission(store: &Store, receipt: &Value) -> Result<(), String> {
    if !marker_state(&store.path)? { return Err("The copy authority marker is missing.".into()); }
    let mut db = connect(&store.path, true, false)?;
    if store.doc["copyLedgerId"] != identity(&db)? {
        return Err("The journal and copy ledger have different lineages.".into());
    }
    let tx = db.transaction().map_err(store::error)?;
    insert(&tx, receipt)?;
    tx.commit().map_err(store::error)
}

struct Assessment {
    id: String,
    latest_revision: u64,
    digest: String,
    count: usize,
    missing_count: usize,
    missing: Vec<Value>,
}

fn assess(store: &Store, offset: usize, limit: usize) -> Result<Option<Assessment>, String> {
    let marker = marker_state(&store.path)?;
    let db_exists = regular(&database(&store.path))?;
    if !marker && !db_exists && store.doc["copyLedgerId"].is_null() { return Ok(None); }
    if !marker || !db_exists {
        return Err("The independent copy effect history is incomplete. Copy admission is blocked.".into());
    }
    let db = connect(&store.path, false, false)?;
    let id = identity(&db)?;
    let admitted = db.query_row("SELECT admitted FROM lineage WHERE slot=1",[],|row| row.get::<_,i64>(0))
        .map_err(store::error)?;
    let mut output = Assessment { id, latest_revision:0,digest:String::new(),count:0,missing_count:0,missing:Vec::new() };
    let mut query = db.prepare("SELECT operation_id,length(receipt),receipt FROM admissions ORDER BY operation_id").map_err(store::error)?;
    let mut rows = query.query([]).map_err(store::error)?;
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(output.id.as_bytes());
    while let Some(row) = rows.next().map_err(store::error)? {
        let key: String = row.get(0).map_err(store::error)?;
        let size: i64 = row.get(1).map_err(store::error)?;
        if !(1..=64*1024).contains(&size) { return Err("An independent copy receipt exceeds its supported size.".into()); }
        let body: String = row.get(2).map_err(store::error)?;
        let value: Value = serde_json::from_str(&body).map_err(store::error)?;
        let checked = receipt_projection(&value)?;
        if key != checked["operationId"] || checked != value {
            return Err("A saved copy ledger receipt is invalid. Copy admission is blocked.".into());
        }
        output.latest_revision = output.latest_revision.max(value["committedRevision"].as_u64().unwrap_or(0));
        output.count += 1;
        digest.update((body.len() as u64).to_le_bytes());
        digest.update(body.as_bytes());
        let stored = &store.doc["copyOperations"][&key];
        let tombstone = &store.doc["reconciledCopyOperations"][&key];
        if stored["payloadHash"] != value["payloadHash"] && tombstone["payloadHash"] != value["payloadHash"] {
            if output.missing_count >= offset && output.missing.len() < limit {
                output.missing.push(value);
            }
            output.missing_count += 1;
        }
    }
    output.digest = format!("{:x}",digest.finalize());
    if usize::try_from(admitted).ok() != Some(output.count) {
        return Err("Independent copy history has an incomplete receipt set. Copy admission is blocked.".into());
    }
    Ok(Some(output))
}

pub(crate) fn status(store: &Store) -> Value {
    match assess(store, 0, 32) {
        Ok(None) => json!({"phase":"uninitialized","ready":true,"historyCount":0}),
        Ok(Some(value)) => {
            let acknowledged = store.doc["copyLedgerReconciliation"]["acknowledgedThrough"]
                .as_u64().is_some_and(|revision| revision >= value.latest_revision);
            let ready = store.doc["copyLedgerId"] == value.id && value.missing_count == 0
                && (store.revision() >= value.latest_revision || acknowledged);
            json!({"phase":if ready {"ready"} else {"review-required"},"ready":ready,"ledgerId":value.id,
                "ledgerDigest":value.digest,"highestEffectRevision":value.latest_revision,"historyCount":value.count,
                "unreconciledCount":value.missing_count,"unreconciled":value.missing,
                "problem":if ready {None} else {Some("Newer or different copy effect history requires review before any further copying.")}})
        }
        Err(problem) => json!({"phase":"unavailable","ready":false,"problem":problem}),
    }
}

pub(crate) fn page(store: &Store, offset: usize, limit: usize, expected_digest: &str) -> Result<Value, String> {
    let value = assess(store, offset, limit)?.ok_or("No independent copy history is available.")?;
    if value.digest != expected_digest {
        return Err("The independent copy history changed. Read the current queue before continuing.".into());
    }
    let next = offset.saturating_add(value.missing.len());
    Ok(json!({"entries":value.missing,"total":value.missing_count,
        "nextOffset":(next<value.missing_count).then_some(next),"ledgerDigest":value.digest}))
}

pub(crate) fn reconcile(store: &Store, doc: &mut Value, expected_digest: &str,
    expected_destination: &str, provenance: Value) -> Result<(), String> {
    let value = assess(store, 0, usize::MAX)?.ok_or("No independent copy authority was established.")?;
    if value.digest != expected_digest {
        return Err("The independent copy effect history changed. Review it again.".into());
    }
    if value.missing.iter().any(|entry| entry["destinationId"] != expected_destination) {
        return Err("Unreconciled copy effects belong to a different destination. Reconnect the original reviewed drive.".into());
    }
    if doc.get("reconciledCopyOperations").is_none() { doc["reconciledCopyOperations"] = json!({}); }
    let records = doc["reconciledCopyOperations"].as_object_mut().ok_or("Invalid reconciled receipt collection.")?;
    for receipt in value.missing {
        let id = receipt["operationId"].as_str().unwrap().to_owned();
        records.insert(id,receipt);
    }
    doc["schemaVersion"] = json!(2);
    doc["copyLedgerId"] = json!(value.id);
    doc["copyLedgerReconciliation"] = json!({"ledgerDigest":value.digest,"acknowledgedThrough":value.latest_revision,
        "recordedAt":store::now(),
        "provenance":provenance,"scope":"Old copy IDs remain fenced. Reconciliation does not assert that destination files are complete."});
    Ok(())
}
