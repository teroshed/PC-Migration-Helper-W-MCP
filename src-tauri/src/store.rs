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
    _lock: File,
}

pub fn validate(doc: &Value) -> Result<(), String> {
    if !doc.is_object() || doc["schemaVersion"] != 1 {
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
        "attention" | "migrate-now" | "last-moment" | "redownload" | "erase" | "on-external"
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
        let bytes = match fs::read(&path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(error(e)),
        };
        let mut store = Self {
            path,
            doc: fresh(root),
            problem: None,
            fingerprint: bytes.as_deref().map(hash),
            _lock: lock,
        };
        if let Some(bytes) = bytes {
            match serde_json::from_slice::<Value>(&bytes).map_err(error).and_then(|v| { validate(&v)?; Ok(v) }) {
                Ok(doc) => store.doc = doc,
                Err(e) => store.problem = Some(format!("Cannot load saved decisions: {e}. The original is untouched. You may recover the last-known-good copy below.")),
            }
        } else if store.path.with_extension("json.bak").exists() {
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

    pub fn commit(&mut self, mut candidate: Value) -> Result<Value, String> {
        if let Some(e) = &self.problem {
            return Err(e.clone());
        }
        validate(&candidate)?;
        let current = match fs::read(&self.path) {
            Ok(b) => Some(b),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(error(e)),
        };
        if current.as_deref().map(hash) != self.fingerprint {
            return Err("Data changed outside this app. Your new edit was not saved. Close and reopen to load the other version; no file was overwritten.".into());
        }
        candidate["revision"] = json!(self.revision() + 1);
        candidate["updatedAt"] = json!(now());
        let bytes = serde_json::to_vec_pretty(&candidate).map_err(error)?;
        if let Some(old) = current.as_ref() {
            let parsed: Value = serde_json::from_slice(&old).map_err(error)?;
            validate(&parsed)?;
            atomic_write(&self.path.with_extension("json.bak"), &old)?;
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
        atomic_write(&self.path, &bytes)?;
        self.fingerprint = Some(hash(&bytes));
        self.doc = candidate;
        Ok(self.doc.clone())
    }

    pub fn recover(&mut self) -> Result<Value, String> {
        if self.problem.is_none() {
            return Err("Recovery is available only when the main data file cannot load.".into());
        }
        let backup = fs::read(self.path.with_extension("json.bak"))
            .map_err(|e| format!("No readable recovery copy: {e}"))?;
        let doc: Value = serde_json::from_slice(&backup).map_err(error)?;
        validate(&doc)?;
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
        atomic_write(&self.path, &backup)?;
        self.fingerprint = Some(hash(&backup));
        self.doc = doc;
        self.problem = None;
        Ok(self.doc.clone())
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
    replace(&temp, path).map_err(|e| {
        format!(
            "Save failed: {e}. Pending data retained at {}",
            temp.display()
        )
    })
}

#[cfg(windows)]
fn replace(from: &Path, to: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let a: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let b: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if unsafe {
        MoveFileExW(
            a.as_ptr(),
            b.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(error(std::io::Error::last_os_error()))
    } else {
        Ok(())
    }
}
#[cfg(not(windows))]
fn replace(from: &Path, to: &Path) -> Result<(), String> {
    fs::rename(from, to).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;
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
