//! Copy admissions are written to a receipt ledger and a separate checksum
//! witness before a prepared worker is released. Neither file is rolled back
//! with the review journal.
use crate::store::{self, Store};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufReader, Read, Write},
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_NO_RECALL,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
};

const WITNESS_HEADER: &[u8; 8] = b"PMHCPW2\n";
const LEDGER_HEADER: &[u8; 8] = b"PMHCPL2\n";
const MAX_RECEIPT: usize = 64 * 1024;

fn witness_path(path: &Path) -> PathBuf {
    path.with_extension("json.copy-authority")
}

fn ledger_path(path: &Path) -> PathBuf {
    path.with_extension("json.copy-effects.log")
}

fn exists(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !crate::filesystem::reparse(&metadata) => Ok(true),
        Ok(_) => Err("A copy authority artifact is not a regular app-owned file.".into()),
        Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(problem) => Err(store::error(problem)),
    }
}

fn open_artifact(path: &Path, write: bool, create: bool) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(write)
        .create_new(create)
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OPEN_NO_RECALL)
        .open(path)
        .map_err(store::error)?;
    let meta = file.metadata().map_err(store::error)?;
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let read = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    if !meta.is_file() || crate::filesystem::reparse(&meta) || read == 0 || info.nNumberOfLinks != 1
    {
        return Err("The copy authority artifact is not an independent regular file.".into());
    }
    Ok(file)
}

pub(crate) struct AuthorityFiles {
    _parents: Vec<File>,
    witness: File,
    ledger: File,
}

impl AuthorityFiles {
    fn open(path: &Path, write: bool, create: bool) -> Result<Self, String> {
        let parent = path.parent().ok_or("Missing copy authority parent")?;
        let parents = crate::copy_queue::guard_copy_state_directory(parent)?;
        let witness = open_artifact(&witness_path(path), write, create)?;
        let ledger = open_artifact(&ledger_path(path), write, create)?;
        Ok(Self {
            _parents: parents,
            witness,
            ledger,
        })
    }

    fn append(&mut self, head: [u8; 32], body: &[u8]) -> Result<[u8; 32], String> {
        if !(1..=MAX_RECEIPT).contains(&body.len()) {
            return Err("A copy receipt exceeds its supported size.".into());
        }
        let size = (body.len() as u32).to_le_bytes();
        let next = extend(head, size, body);
        self.ledger.write_all(&size).map_err(store::error)?;
        self.ledger.write_all(body).map_err(store::error)?;
        self.ledger.sync_all().map_err(store::error)?;
        self.witness.write_all(&next).map_err(store::error)?;
        self.witness.sync_all().map_err(store::error)?;
        Ok(next)
    }
}

fn read_header(file: &mut File, expected: &[u8; 8]) -> Result<String, String> {
    let mut magic = [0u8; 8];
    let mut id = [0u8; 64];
    file.read_exact(&mut magic).map_err(store::error)?;
    file.read_exact(&mut id).map_err(store::error)?;
    if &magic != expected || !id.iter().all(u8::is_ascii_hexdigit) {
        return Err("A copy authority header is invalid or belongs to another format.".into());
    }
    String::from_utf8(id.to_vec()).map_err(store::error)
}

fn genesis(id: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(LEDGER_HEADER);
    hash.update(id.as_bytes());
    hash.finalize().into()
}

fn extend(head: [u8; 32], size: [u8; 4], body: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(head);
    hash.update(size);
    hash.update(body);
    hash.finalize().into()
}

fn receipt_projection(value: &Value) -> Result<Value, String> {
    let id = value["operationId"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 200)
        .ok_or("A copy operation receipt is missing its exact ID.")?;
    let hash = value["payloadHash"]
        .as_str()
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or("A copy operation receipt is missing its exact request digest.")?;
    let revision = match value["phase"].as_str() {
        Some("starting") => value["intentRevision"]
            .as_u64()
            .or_else(|| value["committedRevision"].as_u64()),
        Some("accepted" | "failed") => value["committedRevision"].as_u64(),
        _ => None,
    }
    .ok_or("A copy operation receipt is missing its durable phase or revision.")?;
    Ok(
        json!({"operationId":id,"payloadHash":hash,"committedRevision":revision,
        "action":value["action"],"jobId":value["jobId"],"phase":value["phase"],
        "instanceId":value["instanceId"],"mode":value["mode"],"sourceRevision":value["sourceRevision"],
        "destinationId":value["destinationId"],"sourceVolumeSignature":value["sourceVolumeSignature"],
        "queueSignature":value["queueSignature"],"requestedAt":value["requestedAt"]}),
    )
}

fn receipt_map<'a>(
    doc: &'a Value,
    name: &str,
) -> Result<Option<&'a serde_json::Map<String, Value>>, String> {
    doc.get(name)
        .map(|value| {
            value
                .as_object()
                .ok_or_else(|| format!("The {name} receipt collection is malformed."))
        })
        .transpose()
}

/// Create both authorities before saving the first new copy intent. If an
/// interrupted bootstrap leaves only one file or a partial frame, copying is
/// disabled until the original authority can be restored.
pub(crate) fn initialize(store: &Store) -> Result<String, String> {
    if assess(store, 0, 0)?.is_some() {
        return Err("Existing copy authority cannot be initialized again.".into());
    }
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(store::error)?;
    let id = store::hash(&random);
    let mut files = AuthorityFiles::open(&store.path, true, true)?;
    files
        .witness
        .write_all(WITNESS_HEADER)
        .map_err(store::error)?;
    files
        .witness
        .write_all(id.as_bytes())
        .map_err(store::error)?;
    files.witness.sync_all().map_err(store::error)?;
    files
        .ledger
        .write_all(LEDGER_HEADER)
        .map_err(store::error)?;
    files
        .ledger
        .write_all(id.as_bytes())
        .map_err(store::error)?;
    files.ledger.sync_all().map_err(store::error)?;
    let mut head = genesis(&id);
    if let Some(items) = receipt_map(&store.doc, "copyOperations")? {
        for (key, receipt) in items {
            let projection = receipt_projection(receipt)?;
            if projection["operationId"] != *key {
                return Err("An existing copy receipt does not match its ID.".into());
            }
            let body = serde_json::to_vec(&projection).map_err(store::error)?;
            head = files.append(head, &body)?;
        }
    }
    Ok(id)
}

/// Returns held handles so neither authority can be replaced between its
/// verification, the journal acknowledgement, and worker release.
pub(crate) fn record_admission(store: &Store, receipt: &Value) -> Result<AuthorityFiles, String> {
    let mut files = AuthorityFiles::open(&store.path, true, false)?;
    let view = inspect(store, &mut files, 0, 0)?;
    if !view.ready(store) {
        return Err("The independent copy effect history requires review.".into());
    }
    let projection = receipt_projection(receipt)?;
    let id = projection["operationId"].as_str().unwrap();
    if view.ids.contains(id) {
        return Err("The independent copy operation ID was already recorded.".into());
    }
    let body = serde_json::to_vec(&projection).map_err(store::error)?;
    files.append(view.head, &body)?;
    Ok(files)
}

struct Assessment {
    id: String,
    latest_revision: u64,
    head: [u8; 32],
    ids: HashSet<String>,
    missing_count: usize,
    missing: Vec<Value>,
}

impl Assessment {
    fn digest(&self) -> String {
        self.head.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn ready(&self, store: &Store) -> bool {
        let acknowledged = store.doc["copyLedgerReconciliation"]["acknowledgedThrough"]
            .as_u64()
            .is_some_and(|revision| revision >= self.latest_revision);
        store.doc["copyLedgerId"] == self.id
            && self.missing_count == 0
            && (store.revision() >= self.latest_revision || acknowledged)
    }
}

fn exact_projection(saved: Option<&Value>, independent: &Value) -> bool {
    saved
        .and_then(|value| receipt_projection(value).ok())
        .as_ref()
        == Some(independent)
}

fn inspect(
    store: &Store,
    files: &mut AuthorityFiles,
    offset: usize,
    limit: usize,
) -> Result<Assessment, String> {
    let id = read_header(&mut files.ledger, LEDGER_HEADER)?;
    if read_header(&mut files.witness, WITNESS_HEADER)? != id
        || (!store.doc["copyLedgerId"].is_null() && store.doc["copyLedgerId"] != id)
    {
        return Err("The journal and copy authority artifacts have different lineages.".into());
    }
    let saved = receipt_map(&store.doc, "copyOperations")?;
    let reconciled = receipt_map(&store.doc, "reconciledCopyOperations")?;
    let head = genesis(&id);
    let mut output = Assessment {
        id,
        latest_revision: 0,
        head,
        ids: HashSet::new(),
        missing_count: 0,
        missing: Vec::new(),
    };
    let mut ledger = BufReader::new(&mut files.ledger);
    let mut witness = BufReader::new(&mut files.witness);
    let mut size = [0u8; 4];
    let mut body = Vec::new();
    while ledger.read(&mut size[..1]).map_err(store::error)? != 0 {
        ledger.read_exact(&mut size[1..]).map_err(store::error)?;
        let length = u32::from_le_bytes(size) as usize;
        if !(1..=MAX_RECEIPT).contains(&length) {
            return Err("An independent copy receipt exceeds its supported size.".into());
        }
        body.resize(length, 0);
        ledger.read_exact(&mut body).map_err(store::error)?;
        output.head = extend(output.head, size, &body);
        let mut checksum = [0u8; 32];
        if witness.read_exact(&mut checksum).is_err() || checksum != output.head {
            return Err(
                "The copy effect ledger and its independent checksum witness disagree.".into(),
            );
        }
        let value: Value = serde_json::from_slice(&body).map_err(store::error)?;
        let checked = receipt_projection(&value)?;
        let key = checked["operationId"].as_str().unwrap();
        if checked != value || !output.ids.insert(key.to_owned()) {
            return Err("A saved copy effect receipt is invalid or duplicated.".into());
        }
        output.latest_revision = output
            .latest_revision
            .max(value["committedRevision"].as_u64().unwrap());
        let stored = saved.and_then(|items| items.get(key));
        let tombstone = reconciled.and_then(|items| items.get(key));
        if !exact_projection(tombstone, &value) && !exact_projection(stored, &value) {
            if output.missing_count >= offset && output.missing.len() < limit {
                output.missing.push(value);
            }
            output.missing_count += 1;
        }
    }
    if witness.read(&mut size[..1]).map_err(store::error)? != 0 {
        return Err(
            "The copy effect ledger is older than its independent checksum witness.".into(),
        );
    }
    // A starting intent can legitimately precede its independent admission.
    // A finalized or reconciled ID cannot be absent from a matching ledger.
    if saved.is_some_and(|items| {
        items
            .iter()
            .any(|(key, value)| value["phase"] != "starting" && !output.ids.contains(key))
    }) || reconciled.is_some_and(|items| items.keys().any(|key| !output.ids.contains(key)))
    {
        return Err("The review journal contains finalized copy receipts absent from the independent authority.".into());
    }
    Ok(output)
}

fn assess(store: &Store, offset: usize, limit: usize) -> Result<Option<Assessment>, String> {
    let witness = exists(&witness_path(&store.path))?;
    let ledger = exists(&ledger_path(&store.path))?;
    if !witness
        && !ledger
        && store.doc["copyLedgerId"].is_null()
        && !exists(&store.path.with_extension("json.copy-effects.sqlite"))?
    {
        return Ok(None);
    }
    if !witness || !ledger {
        return Err(
            "The independent copy effect history is incomplete. Copy admission is blocked.".into(),
        );
    }
    let mut files = AuthorityFiles::open(&store.path, false, false)?;
    inspect(store, &mut files, offset, limit).map(Some)
}

pub(crate) fn status(store: &Store) -> Value {
    match assess(store, 0, 32) {
        Ok(None) => json!({"phase":"uninitialized","ready":true,"historyCount":0}),
        Ok(Some(value)) => {
            let ready = value.ready(store);
            json!({"phase":if ready {"ready"} else {"review-required"},"ready":ready,"ledgerId":value.id,
                "ledgerDigest":value.digest(),"highestEffectRevision":value.latest_revision,"historyCount":value.ids.len(),
                "unreconciledCount":value.missing_count,"unreconciled":value.missing,
                "problem":if ready {None} else {Some("Newer or different copy effect history requires review before any further copying.")}})
        }
        Err(problem) => json!({"phase":"unavailable","ready":false,"problem":problem}),
    }
}

pub(crate) fn page(
    store: &Store,
    offset: usize,
    limit: usize,
    expected_digest: &str,
) -> Result<Value, String> {
    let value = assess(store, offset, limit)?.ok_or("No independent copy history is available.")?;
    let digest = value.digest();
    if digest != expected_digest {
        return Err(
            "The independent copy history changed. Read the current queue before continuing."
                .into(),
        );
    }
    let next = offset.saturating_add(value.missing.len());
    Ok(json!({"entries":value.missing,"total":value.missing_count,
        "nextOffset":(next<value.missing_count).then_some(next),"ledgerDigest":digest}))
}

pub(crate) fn reconcile(
    store: &Store,
    doc: &mut Value,
    expected_digest: &str,
    expected_destination: &str,
    provenance: Value,
) -> Result<(), String> {
    let value =
        assess(store, 0, usize::MAX)?.ok_or("No independent copy authority was established.")?;
    if value.digest() != expected_digest {
        return Err("The independent copy effect history changed. Review it again.".into());
    }
    if value
        .missing
        .iter()
        .any(|entry| entry["destinationId"] != expected_destination)
    {
        return Err("Unreconciled copy effects belong to a different destination. Reconnect the original reviewed drive.".into());
    }
    if doc.get("reconciledCopyOperations").is_none() {
        doc["reconciledCopyOperations"] = json!({});
    }
    let records = doc["reconciledCopyOperations"]
        .as_object_mut()
        .ok_or("Invalid reconciled receipt collection.")?;
    for receipt in value.missing {
        let id = receipt["operationId"].as_str().unwrap().to_owned();
        records.insert(id, receipt);
    }
    doc["schemaVersion"] = json!(2);
    doc["copyLedgerId"] = json!(value.id);
    doc["copyLedgerReconciliation"] = json!({"ledgerDigest":expected_digest,"acknowledgedThrough":value.latest_revision,
        "recordedAt":store::now(),"provenance":provenance,
        "scope":"Old copy IDs remain fenced. Reconciliation does not assert that destination files are complete."});
    Ok(())
}
