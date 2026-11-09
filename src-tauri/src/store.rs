use chrono::Utc;
use fs2::FileExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn error(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub struct Store {
    pub path: PathBuf,
    pub doc: Value,
    pub problem: Option<String>,
    fingerprint: Option<String>,
    minimum_schema: u64,
    _lock: File,
}

pub fn validate(doc: &Value) -> Result<(), String> {
    if !doc.is_object() || !matches!(doc["schemaVersion"].as_u64(), Some(1 | 2)) {
        return Err(
            "Unsupported or malformed data file. Original preserved; no writes allowed.".into(),
        );
    }
    if !doc["revision"].is_u64()
        || !doc["decisions"].is_object()
        || !doc["history"].is_array()
        || !doc["view"].is_object()
    {
        return Err("The saved document has invalid reserved fields. Original preserved.".into());
    }
    let groups = doc.get("groupOperations").map(|value| {
        value.as_object().ok_or("Grouped operation receipts have an unsupported format.")
    }).transpose()?;
    let mut group_count = 0usize;
    for event in doc["history"].as_array().unwrap() {
        if !matches!(event["type"].as_str(), Some("bulkClassify" | "undoGroup")) {
            continue;
        }
        group_count += 1;
        let matching_receipt = event["id"].as_str().and_then(|id| groups?.get(id))
            .is_some_and(|receipt| receipt["payloadHash"] == event["payloadHash"]
                && receipt["type"] == event["type"] && receipt["memberCount"] == event["memberCount"]);
        let valid_group = doc["schemaVersion"] == 2 && matching_receipt
            && event["id"].as_str().is_some_and(|id| !id.is_empty())
            && event["payloadHash"].as_str().is_some_and(|hash| {
                hash.len() == 64 && hash.bytes().all(|value| value.is_ascii_hexdigit())
            })
            && event["members"].as_array().is_some_and(|members| {
                (1..=128).contains(&members.len()) && event["memberCount"] == members.len()
            });
        if !valid_group {
            return Err("Grouped decision history has an unsupported format. Original preserved.".into());
        }
    }
    if group_count != groups.map_or(0, |receipts| receipts.len()) {
        return Err("Grouped decision history and receipts disagree. Original preserved.".into());
    }
    let view = &doc["view"];
    if !view["path"].is_string()
        || !(view["selectedPath"].is_null() || view["selectedPath"].is_string())
        || !view["search"].is_string()
        || !view["filter"].as_str().is_some_and(valid_filter)
        || !view["page"].is_u64()
        || !view["scrollTop"].is_number()
        || !view["recentPaths"]
            .as_array()
            .is_some_and(|a| a.len() <= 30 && a.iter().all(Value::is_string))
    {
        return Err(
            "The saved review position has an unsupported format. Original preserved.".into(),
        );
    }
    if view
        .get("revealPath")
        .is_some_and(|path| !(path.is_null() || path.is_string()))
    {
        return Err(
            "The saved reveal target has an unsupported format. Original preserved.".into(),
        );
    }
    if view
        .get("previewColumns")
        .is_some_and(|value| !valid_preview_columns(value))
        || view
            .get("previewScrollLeft")
            .is_some_and(|value| !valid_nonnegative_number(value))
        || view
            .get("browserColumnWidth")
            .is_some_and(|value| !valid_column_width(value))
        || view
            .get("layout")
            .is_some_and(|value| !valid_review_layout(value))
    {
        return Err(
            "The saved preview-column position has an unsupported format. Original preserved."
                .into(),
        );
    }
    if doc.get("volumes").is_some_and(|v| !v.is_object())
        || doc.get("recentOperations").is_some_and(|v| !v.is_array())
    {
        return Err(
            "Saved volume or operation metadata has an unsupported format. Original preserved."
                .into(),
        );
    }
    for decision in doc["decisions"].as_object().unwrap().values() {
        if !decision.is_object()
            || !decision["path"].is_string()
            || !decision["status"].as_str().is_some_and(valid_status)
        {
            return Err("A decision has an unknown format. Original preserved.".into());
        }
        if decision.get("notes").is_some_and(|v| !v.is_string())
            || decision.get("decidedAt").is_some_and(|v| !v.is_string())
        {
            return Err(
                "A saved note or decision date has an unsupported format. Original preserved."
                    .into(),
            );
        }
    }
    Ok(())
}

pub fn valid_status(s: &str) -> bool {
    matches!(
        s,
        "attention"
            | "migrate-now"
            | "last-moment"
            | "redownload"
            | "erase"
            | "on-external"
            | "manual-pending"
            | "manual-confirmed"
    )
}

pub fn valid_filter(s: &str) -> bool {
    matches!(s, "all" | "mixed") || valid_status(s)
}

pub fn valid_nonnegative_number(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|number| number.is_finite() && number >= 0.0)
}
pub fn valid_column_width(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|number| number.is_finite() && (240.0..=800.0).contains(&number))
}
pub fn valid_review_layout(value: &Value) -> bool {
    value.as_object().is_some_and(|fields| {
        fields.iter().all(|(key, value)| match key.as_str() {
            "drivesWidth" => value.as_f64().is_some_and(|n| (148.0..=520.0).contains(&n)),
            "decisionWidth" => value.as_f64().is_some_and(|n| (246.0..=520.0).contains(&n)),
            "drivesCollapsed" | "decisionCollapsed" => value.is_boolean(),
            "toolbar" => matches!(
                value.as_str(),
                Some("expanded" | "compact" | "path" | "search")
            ),
            "fillDepth" => value.is_null() || value.as_u64().is_some_and(|n| n <= 4096),
            _ => false,
        })
    })
}
pub fn valid_preview_columns(value: &Value) -> bool {
    value.as_array().is_some_and(|columns| {
        columns.len() <= 4096
            && columns.iter().all(|column| {
                column.is_object()
                    && column["path"].is_string()
                    && column["page"].is_u64()
                    && valid_nonnegative_number(&column["scrollTop"])
                    && column.get("width").is_none_or(valid_column_width)
            })
    })
}

pub fn fresh(root: &str) -> Value {
    json!({"schemaVersion":1,"revision":0,"createdAt":now(),"updatedAt":now(),
        "decisions":{},"history":[],"evidence":[],"suggestions":[],"volumes":{},
        "view":{"path":root,"selectedPath":null,"search":"","filter":"all","page":0,"scrollTop":0,"recentPaths":[],"previewColumns":[],"previewScrollLeft":0,"browserColumnWidth":320},
        "provenance":{"application":"PC Migration Helper","purpose":"Read-only source review; classifications never execute migration or deletion"},
        "backupReminder":"Include this review journal in your external backup."})
}

impl Store {
    pub fn open(path: PathBuf, root: &str) -> Result<Self, String> {
        fs::create_dir_all(path.parent().ok_or("Data path has no parent")?).map_err(error)?;
        let lock_path = path.with_extension("json.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(error)?;
        lock.try_lock_exclusive().map_err(|_| "Another PC Migration Helper instance owns this data file. Close that instance normally, then reopen.".to_string())?;
        let bytes = read_optional(&path)?;
        let minimum_schema = match read_optional(&path.with_extension("json.format-v2"))? {
            None => 1,
            Some(marker) if marker == FORMAT_V2 => 2,
            Some(_) => return Err("Saved journal format fence is unreadable. Original preserved.".into()),
        };
        let mut store = Self {
            path,
            doc: fresh(root),
            problem: None,
            fingerprint: bytes.as_deref().map(hash),
            minimum_schema,
            _lock: lock,
        };
        if let Some(bytes) = bytes {
            match serde_json::from_slice::<Value>(&bytes).map_err(error).and_then(|v| { validate(&v)?; Ok(v) }) {
                Ok(doc) => {
                    store.minimum_schema = store.minimum_schema.max(doc["schemaVersion"].as_u64().unwrap());
                    store.doc = doc;
                }
                Err(e) => store.problem = Some(format!("Cannot load saved decisions: {e}. The original is untouched. You may recover the last-known-good copy below.")),
            }
        } else if store.path.with_extension("json.bak").exists() || store.minimum_schema > 1 {
            store.problem=Some("The main data file is missing, but a recovery copy exists. Recovery is required; no empty replacement was created.".into());
        } else {
            let doc = store.doc.clone();
            store.commit(doc)?;
        }
        Ok(store)
    }

    pub fn revision(&self) -> u64 {
        self.doc["revision"].as_u64().unwrap_or(0)
    }

    pub fn loaded_fingerprint(&self) -> Option<&str> {
        self.fingerprint.as_deref()
    }

    pub fn validate_recovery_document(&self, doc: &Value) -> Result<(), String> {
        validate(doc)?;
        if doc["schemaVersion"].as_u64().unwrap() < self.minimum_schema {
            return Err("Recovery would downgrade this journal format. Use a reviewed compatible backup.".into());
        }
        Ok(())
    }

    pub fn can_recover(&self) -> bool {
        self.problem.is_some() && read_optional(&self.path.with_extension("json.bak"))
            .ok().flatten().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|doc| self.validate_recovery_document(&doc).is_ok())
    }

    pub fn commit(&mut self, candidate: Value) -> Result<Value, String> {
        self.commit_guarded(candidate, || Ok(()))
    }

    pub fn commit_guarded(
        &mut self,
        mut candidate: Value,
        mut before_publish: impl FnMut() -> Result<(), String>,
    ) -> Result<Value, String> {
        if let Some(e) = &self.problem {
            return Err(e.clone());
        }
        validate(&candidate)?;
        let candidate_schema = candidate["schemaVersion"].as_u64().unwrap();
        let schema = candidate_schema.max(self.minimum_schema);
        candidate["schemaVersion"] = json!(schema);
        let current = read_optional(&self.path)?;
        if current.as_deref().map(hash) != self.fingerprint {
            return Err("Data changed outside this app. Your new edit was not saved. Close and reopen to load the other version; no file was overwritten.".into());
        }
        candidate["revision"] = json!(self.revision().checked_add(1).ok_or("Saved revision overflow")?);
        candidate["updatedAt"] = json!(now());
        let bytes = serde_json::to_vec_pretty(&candidate).map_err(error)?;
        if let Some(old) = current.as_ref() {
            let mut parsed: Value = serde_json::from_slice(old).map_err(error)?;
            validate(&parsed)?;
            let backup = self.path.with_extension("json.bak");
            if parsed["schemaVersion"].as_u64().unwrap() < schema {
                let prior_backup = read_optional(&backup)?;
                archive_pre_upgrade(&self.path, old)?;
                if let Some(prior) = &prior_backup {
                    archive_pre_upgrade(&self.path, prior)?;
                }
                let marker = self.path.with_extension("json.format-v2");
                keep_exact_or_write(&marker, FORMAT_V2)?;
                self.minimum_schema = 2;
                parsed["schemaVersion"] = json!(2);
                let converted = serde_json::to_vec_pretty(&parsed).map_err(error)?;
                let backup_hash = prior_backup.as_deref().map(hash);
                atomic_write_guarded(&backup, &converted, || {
                    require_fingerprint(&backup, backup_hash.as_deref())?;
                    require_fingerprint(&self.path, self.fingerprint.as_deref())
                })?;
            } else {
                atomic_write(&backup, old)?;
            }
        }
        // Recheck after preparing the recovery copy. App peers are locked out; this also
        // catches ordinary external-editor changes during the save preparation.
        let recheck = match fs::read(&self.path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(error(e)),
        };
        if recheck.as_deref().map(hash) != self.fingerprint {
            return Err("Data changed outside this app while preparing the save. No main-file replacement was made.".into());
        }
        atomic_write_guarded(&self.path, &bytes, || {
            require_fingerprint(&self.path, self.fingerprint.as_deref())?;
            before_publish()
        })?;
        self.fingerprint = Some(hash(&bytes));
        self.doc = candidate;
        self.minimum_schema = schema;
        Ok(self.doc.clone())
    }

    pub fn recover(&mut self) -> Result<Value, String> {
        let expected = self.fingerprint.clone();
        self.recover_checked(expected.as_deref(), None, None)
    }

    pub fn recover_prepared(
        &mut self,
        expected_original: Option<&str>,
        expected_backup: &str,
        operation_id: &str,
        receipt: Value,
    ) -> Result<Value, String> {
        self.recover_checked(expected_original, Some(expected_backup), Some((operation_id, receipt)))
    }

    fn recover_checked(
        &mut self,
        expected_original: Option<&str>,
        expected_backup: Option<&str>,
        receipt: Option<(&str, Value)>,
    ) -> Result<Value, String> {
        if self.problem.is_none() {
            return Err("Recovery is available only when the main data file cannot load.".into());
        }
        if self.fingerprint.as_deref() != expected_original {
            return Err("The prepared recovery does not match the original loaded by this app.".into());
        }
        let backup = fs::read(self.path.with_extension("json.bak"))
            .map_err(|e| format!("No readable recovery copy: {e}"))?;
        if expected_backup.is_some_and(|expected| expected != hash(&backup)) {
            return Err("The recovery copy changed. Inspect recovery state again before proceeding.".into());
        }
        let mut doc: Value = serde_json::from_slice(&backup).map_err(error)?;
        self.validate_recovery_document(&doc)?;
        let restored = if let Some((operation_id, mut receipt)) = receipt {
            if doc.get("recoveryOperations").is_some_and(|value| !value.is_object()) {
                return Err("Recovery receipt metadata has an unsupported format. Original preserved.".into());
            }
            if doc["recoveryOperations"].get(operation_id).is_some() {
                return Err("This operation ID is already present in the recovery copy. Inspect the current state and use a new recovery operation.".into());
            }
            if doc["copyOperations"].get(operation_id).is_some() || doc["agentOperations"].get(operation_id).is_some()
                || doc["recentOperations"].as_array().is_some_and(|items|items.iter().any(|id|id==operation_id))
                || doc["history"].as_array().is_some_and(|items|items.iter().any(|event|event["id"]==operation_id)) {
                return Err("Recovery operation ID is already used in the recovery copy".into());
            }
            let revision = doc["revision"].as_u64().unwrap().checked_add(1).ok_or("Recovery revision overflow")?;
            doc["revision"] = json!(revision);
            doc["updatedAt"] = json!(now());
            receipt["committedRevision"] = json!(revision);
            if doc.get("recoveryOperations").is_none() { doc["recoveryOperations"] = json!({}); }
            doc["recoveryOperations"][operation_id] = receipt;
            serde_json::to_vec_pretty(&doc).map_err(error)?
        } else {
            backup
        };
        let original = match fs::read(&self.path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(error(e)),
        };
        if original.as_deref().map(hash) != self.fingerprint {
            return Err("The original changed since startup. Reopen before recovering.".into());
        }
        if let Some(original) = original {
            let preserved = self.path.with_extension(format!(
                "json.recovery-original-{}",
                Utc::now().timestamp_millis()
            ));
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&preserved)
                .map_err(error)?;
            f.write_all(&original).map_err(error)?;
            f.sync_all().map_err(error)?;
        }
        atomic_write_guarded(&self.path, &restored, || {
            require_fingerprint(&self.path, self.fingerprint.as_deref())
        })?;
        self.fingerprint = Some(hash(&restored));
        self.doc = doc;
        self.minimum_schema = self.minimum_schema.max(self.doc["schemaVersion"].as_u64().unwrap());
        self.problem = None;
        Ok(self.doc.clone())
    }
}

const FORMAT_V2: &[u8] = b"{\"minimumSchemaVersion\":2}\n";

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(problem) if problem.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(problem) => Err(error(problem)),
    }
}

fn archive_pre_upgrade(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let archived = path.with_extension(format!("json.pre-v2-{}", hash(bytes)));
    keep_exact_or_write(&archived, bytes)
}

fn keep_exact_or_write(path: &Path, expected: &[u8]) -> Result<(), String> {
    if let Some(current) = read_optional(path)? {
        if current == expected {
            return Ok(());
        }
        return Err("An existing format archive differs from the original. Originals preserved.".into());
    }
    atomic_write_guarded(path, expected, || require_fingerprint(path, None))
}

#[cfg(test)]
mod format_upgrade_tests {
    use super::*;

    #[test]
    fn interrupted_promotion_keeps_the_primary_and_exact_legacy_pair() {
        let base = std::env::temp_dir().join(format!(
            "migration-journal-upgrade-{}-{}",
            std::process::id(), Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let path = base.join("data.json");
        let mut store = Store::open(path.clone(), "E:\\").unwrap();
        store.commit(store.doc.clone()).unwrap();
        let primary = fs::read(&path).unwrap();
        let backup_path = path.with_extension("json.bak");
        let backup = fs::read(&backup_path).unwrap();
        let mut candidate = store.doc.clone();
        candidate["schemaVersion"] = json!(2);
        candidate["futureField"] = json!({"preserved":true});
        let fault = store.commit_guarded(candidate.clone(), || Err("fixture pre-publication fault".into()));
        assert!(fault.unwrap_err().contains("fixture pre-publication fault"));
        assert_eq!(fs::read(&path).unwrap(), primary);
        assert_eq!(fs::read(path.with_extension(format!("json.pre-v2-{}", hash(&primary)))).unwrap(), primary);
        assert_eq!(fs::read(path.with_extension(format!("json.pre-v2-{}", hash(&backup)))).unwrap(), backup);
        assert_eq!(serde_json::from_slice::<Value>(&fs::read(&backup_path).unwrap()).unwrap()["schemaVersion"], 2);
        drop(store);

        let mut store = Store::open(path.clone(), "E:\\").unwrap();
        let upgraded = store.commit(candidate).unwrap();
        assert_eq!(upgraded["schemaVersion"], 2);
        assert_eq!(upgraded["futureField"]["preserved"], true);
        let pair = [fs::read(&path).unwrap(), fs::read(&backup_path).unwrap()];
        for version in pair {
            let doc: Value = serde_json::from_slice(&version).unwrap();
            assert_ne!(doc["schemaVersion"], 1); // Legacy validation rejects both.
            validate(&doc).unwrap();
        }
        drop(store);

        fs::write(&path, b"fixture corruption").unwrap();
        fs::write(&backup_path, backup).unwrap();
        let mut store = Store::open(path.clone(), "E:\\").unwrap();
        assert!(store.problem.is_some());
        assert!(store.recover().unwrap_err().contains("downgrade"));
        assert_eq!(fs::read(path).unwrap(), b"fixture corruption");
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    atomic_write_guarded(path, bytes, || Ok(()))
}

fn require_fingerprint(path: &Path, expected: Option<&str>) -> Result<(), String> {
    let current = match fs::read(path) {
        Ok(bytes) => Some(hash(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(error(e)),
    };
    if current.as_deref() != expected {
        return Err("Data changed outside this app during save replacement. No main-file replacement was made.".into());
    }
    Ok(())
}

fn atomic_write_guarded(
    path: &Path,
    bytes: &[u8],
    mut before_replace: impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    let temp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .map_err(error)?;
    file.write_all(bytes).map_err(error)?;
    file.sync_all().map_err(error)?;
    drop(file);
    replace(&temp, path, &mut before_replace).map_err(|e| {
        format!(
            "Save failed: {e}. Pending data retained at {}",
            temp.display()
        )
    })
}

#[cfg(windows)]
fn replace(
    from: &Path,
    to: &Path,
    before_replace: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // A scanner or another reader may briefly omit FILE_SHARE_DELETE. Retry only
    // these known noncommitted lock failures, with the same synced payload.
    // The caller's guard runs again before every attempt to protect external edits.
    const BACKOFF_MS: [u64; 8] = [10, 20, 40, 80, 160, 160, 160, 160];
    for attempt in 0..=BACKOFF_MS.len() {
        before_replace()?;
        if unsafe {
            MoveFileExW(
                a.as_ptr(),
                b.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } != 0
        {
            return Ok(());
        }
        let problem = std::io::Error::last_os_error();
        let transient = matches!(problem.raw_os_error(), Some(32 | 33))
            || (problem.raw_os_error() == Some(5)
                && (has_delete_sharing_conflict(from) || has_delete_sharing_conflict(to)));
        if !transient || attempt == BACKOFF_MS.len() {
            return Err(error(problem));
        }
        std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS[attempt]));
    }
    unreachable!("replacement returns on success or the final attempt")
}

#[cfg(windows)]
fn has_delete_sharing_conflict(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    // MoveFileEx can map a denied delete-sharing request to ACCESS_DENIED.
    // Probe that specific access without mutating the file; ACL/readonly failures
    // remain permanent errors rather than being treated as sharing conflicts.
    OpenOptions::new()
        .read(true)
        .access_mode(DELETE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
        .err()
        .is_some_and(|problem| matches!(problem.raw_os_error(), Some(32 | 33)))
}
#[cfg(not(windows))]
fn replace(
    from: &Path,
    to: &Path,
    before_replace: &mut impl FnMut() -> Result<(), String>,
) -> Result<(), String> {
    before_replace()?;
    fs::rename(from, to).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn hold_without_delete_sharing(path: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
        OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(path)
            .unwrap()
    }

    #[cfg(windows)]
    fn pending_files(path: &Path) -> Vec<PathBuf> {
        let prefix = format!("{}.tmp-", path.file_stem().unwrap().to_string_lossy());
        fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(Result::unwrap)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .map(|entry| entry.path())
            .collect()
    }

    #[cfg(windows)]
    fn wait_for_pending_file(path: &Path) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while pending_files(path).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "Save never staged its primary payload"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[cfg(windows)]
    #[test]
    fn transient_primary_sharing_lock_releases_without_replaying_the_commit() {
        let path = fixture("transient-lock");
        let mut store = Store::open(path.clone(), r"E:\").unwrap();
        let before = fs::read(&path).unwrap();
        let revision = store.revision();
        let reader = hold_without_delete_sharing(&path);
        let locked_path = path.clone();
        let release = std::thread::spawn(move || {
            wait_for_pending_file(&locked_path);
            std::thread::sleep(std::time::Duration::from_millis(60));
            drop(reader);
        });
        let mut candidate = store.doc.clone();
        candidate["retainedPayload"] = json!("new user edit");
        let saved = store.commit(candidate).unwrap();
        release.join().unwrap();
        assert_eq!(saved["revision"], revision + 1);
        assert_eq!(saved["retainedPayload"], "new user edit");
        assert_eq!(fs::read(path.with_extension("json.bak")).unwrap(), before);
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&path).unwrap()).unwrap(),
            saved
        );
        assert!(pending_files(&path).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn external_edit_during_replacement_lock_aborts_and_retains_candidate() {
        let path = fixture("locked-external-edit");
        let mut store = Store::open(path.clone(), r"E:\").unwrap();
        let before = store.doc.clone();
        let mut external = before.clone();
        external["externalOwner"] = json!("changed while replacement was blocked");
        let external_bytes = serde_json::to_vec_pretty(&external).unwrap();
        let reader = hold_without_delete_sharing(&path);
        let locked_path = path.clone();
        let written = external_bytes.clone();
        let edit = std::thread::spawn(move || {
            wait_for_pending_file(&locked_path);
            fs::write(&locked_path, &written).unwrap();
        });
        let mut candidate = before.clone();
        candidate["pendingUserEdit"] = json!("retain me");
        // Keep replacement blocked until the commit has observed the edit.
        // Releasing from the editor would race the final fingerprint-to-rename gap.
        let result = store.commit(candidate);
        drop(reader);
        edit.join().unwrap();
        let failed = result.unwrap_err();
        assert!(failed.contains("outside this app"), "{failed}");
        assert!(failed.contains("Pending data retained"), "{failed}");
        assert_eq!(store.doc, before);
        assert_eq!(fs::read(&path).unwrap(), external_bytes);
        let pending = pending_files(&path);
        assert_eq!(pending.len(), 1);
        let payload: Value = serde_json::from_slice(&fs::read(&pending[0]).unwrap()).unwrap();
        assert_eq!(payload["pendingUserEdit"], "retain me");
        assert!(payload.get("externalOwner").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn persistent_primary_sharing_lock_has_a_bound_and_keeps_original_and_pending_bytes() {
        let path = fixture("persistent-lock");
        let mut store = Store::open(path.clone(), r"E:\").unwrap();
        let before = fs::read(&path).unwrap();
        let doc = store.doc.clone();
        let reader = hold_without_delete_sharing(&path);
        let mut candidate = doc.clone();
        candidate["pendingUserEdit"] = json!("keep after lock failure");
        let started = std::time::Instant::now();
        let failed = store.commit(candidate).unwrap_err();
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(failed.contains("Pending data retained"), "{failed}");
        assert_eq!(store.doc, doc);
        assert_eq!(fs::read(&path).unwrap(), before);
        let pending = pending_files(&path);
        assert_eq!(pending.len(), 1);
        let payload: Value = serde_json::from_slice(&fs::read(&pending[0]).unwrap()).unwrap();
        assert_eq!(payload["pendingUserEdit"], "keep after lock failure");
        drop(reader);
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    fn fixture(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "migration-master-{label}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&p).unwrap();
        p.join("data.json")
    }
    #[test]
    fn preserves_unknown_fields_and_reopens() {
        let p = fixture("roundtrip");
        {
            let mut s = Store::open(p.clone(), r"E:\").unwrap();
            let mut d = s.doc.clone();
            d["futureField"] = json!({"kept":true});
            s.commit(d).unwrap();
        }
        let s = Store::open(p, r"E:\").unwrap();
        assert_eq!(s.doc["futureField"]["kept"], true);
    }
    #[test]
    fn rejects_competing_process_and_external_edits() {
        let p = fixture("conflict");
        let mut s = Store::open(p.clone(), r"E:\").unwrap();
        assert!(Store::open(p.clone(), r"E:\").is_err());
        fs::write(&p, b"external edit").unwrap();
        assert!(s.commit(s.doc.clone()).unwrap_err().contains("outside"));
        assert_eq!(fs::read(&p).unwrap(), b"external edit");
    }
    #[test]
    fn recovers_without_destroying_malformed_original() {
        let p = fixture("recovery");
        {
            let mut s = Store::open(p.clone(), r"E:\").unwrap();
            s.commit(s.doc.clone()).unwrap();
        }
        fs::write(&p, b"{broken original").unwrap();
        let mut s = Store::open(p.clone(), r"E:\").unwrap();
        assert!(s.problem.is_some());
        s.recover().unwrap();
        assert!(s.problem.is_none());
        assert!(fs::read_dir(p.parent().unwrap())
            .unwrap()
            .flatten()
            .any(|e| e
                .file_name()
                .to_string_lossy()
                .contains("recovery-original")));
    }
    #[test]
    fn failed_write_leaves_document_unchanged() {
        let p = fixture("write-failure");
        let mut s = Store::open(p.clone(), r"E:\").unwrap();
        let before = fs::read(&p).unwrap();
        fs::create_dir(p.with_extension("json.bak")).unwrap();
        assert!(s.commit(s.doc.clone()).is_err());
        assert_eq!(fs::read(&p).unwrap(), before);
    }
    #[test]
    fn missing_main_requires_recovery_when_backup_exists() {
        let p = fixture("missing-primary");
        {
            let mut s = Store::open(p.clone(), r"E:\").unwrap();
            let mut d = s.doc.clone();
            d["retained"] = json!(true);
            s.commit(d).unwrap();
            s.commit(s.doc.clone()).unwrap();
        }
        fs::rename(&p, p.with_extension("json.displaced-fixture")).unwrap();
        let mut s = Store::open(p.clone(), r"E:\").unwrap();
        assert!(s.problem.is_some());
        assert!(!p.exists());
        s.recover().unwrap();
        assert_eq!(s.doc["retained"], true);
    }
    #[test]
    fn optional_preview_fields_preserve_legacy_documents_and_reject_invalid_values() {
        let mut legacy = fresh(r"E:\");
        for key in ["previewColumns", "previewScrollLeft", "browserColumnWidth"] {
            legacy["view"].as_object_mut().unwrap().remove(key);
        }
        assert!(validate(&legacy).is_ok());
        legacy["view"]["previewColumns"] =
            json!([{"path":"E:\\Folder","page":0,"scrollTop":0,"width":800,"future":"retained"}]);
        assert!(validate(&legacy).is_ok());
        legacy["view"]["previewColumns"][0]["width"] = json!(801);
        assert!(validate(&legacy).is_err());
        legacy["view"]["previewColumns"][0]["width"] = json!(320);
        legacy["view"]["previewColumns"][0]["scrollTop"] = json!(-1);
        assert!(validate(&legacy).is_err());
    }
}
