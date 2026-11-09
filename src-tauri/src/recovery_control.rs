//! Explicit, fingerprint-fenced recovery using the same Store operation for UI and RPC.
use crate::{canonical_json, document, store::{error, hash, now, Store}, AppState, MutationOrigin, Shared};
use serde_json::{json, Value};
use std::fs;

fn bytes(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(error(e)),
    }
}

fn snapshot_locked(s: &AppState, store: &Store, instance: &Value) -> Result<Value, String> {
    let original = bytes(&store.path)?;
    let original_hash = original.as_deref().map(hash);
    let backup = bytes(&store.path.with_extension("json.bak"))?;
    let backup_hash = backup.as_deref().map(hash);
    let parsed = backup.as_deref().map(|value| {
        serde_json::from_slice::<Value>(value).map_err(error).and_then(|doc| {
            store.validate_recovery_document(&doc)?;
            Ok(doc)
        })
    });
    let matches_loaded = original_hash.as_deref() == store.loaded_fingerprint();
    let backup_problem = parsed.as_ref().and_then(|result| result.as_ref().err()).cloned();
    let summary = parsed.as_ref().and_then(|result| result.as_ref().ok()).map(|doc| json!({
        "revision":doc["revision"],
        "schemaVersion":doc["schemaVersion"],
        "decisionCount":doc["decisions"].as_object().map_or(0,|items|items.len()),
        "historyCount":doc["history"].as_array().map_or(0,|items|items.len()),
        "updatedAt":doc["updatedAt"],
    }));
    let latest = store.doc["recoveryOperations"].as_object().into_iter().flat_map(|items|items.values())
        .max_by_key(|item|item["recordedAt"].as_str().unwrap_or(""));
    Ok(json!({"api":crate::gateway::api_capabilities(),"instanceId":instance,"dataPath":s.data_path,"problem":store.problem,
        "available":store.problem.is_some() && matches_loaded && summary.is_some(),
        "originalSha256":original_hash,"backupSha256":backup_hash,"originalMatchesLoaded":matches_loaded,
        "backup":summary,"backupProblem":backup_problem,"revision":store.revision(),"latestOperation":latest,
        "scope":"Restores the last-known-good journal, preserving the displaced original and recording recovery provenance. Never copies or deletes source data."}))
}

pub fn snapshot(s: &AppState) -> Result<Value, String> {
    let instance = s.gateway.lock().map_err(error)?["instanceId"].clone();
    let locked = s.store.lock().map_err(error)?;
    snapshot_locked(s, locked.as_ref().ok_or("This instance does not own the journal")?, &instance)
}

fn fingerprint(value: &Value, nullable: bool) -> Result<Option<String>, String> {
    if nullable && value.is_null() { return Ok(None); }
    let value = value.as_str().filter(|text|text.len()==64 && text.bytes().all(|b|b.is_ascii_hexdigit()))
        .ok_or("Recovery fingerprints must be SHA-256 values from get_recovery_state")?;
    Ok(Some(value.to_ascii_lowercase()))
}

pub fn control(s: &Shared, request: &Value, origin: MutationOrigin) -> Result<Value, String> {
    origin.validate("recover")?;
    if request["confirmed"] != true { return Err("Explicit recovery confirmation is required".into()); }
    let operation_id = request["operationId"].as_str().filter(|id| (8..=120).contains(&id.len()) && !id.trim().is_empty())
        .ok_or("Recovery needs a stable operation ID of 8 to 120 bytes")?;
    let instance = s.gateway.lock().map_err(error)?["instanceId"].clone();
    if !instance.is_string() || request["expectedInstanceId"] != instance {
        return Err("The app instance changed. Inspect recovery state before continuing.".into());
    }
    let original = fingerprint(&request["expectedOriginalSha256"], true)?;
    let backup = fingerprint(&request["expectedBackupSha256"], false)?.unwrap();
    let identity = match &origin {
        MutationOrigin::ManualUi => json!({"kind":"manual-ui"}),
        MutationOrigin::Agent{basis,reason} => json!({"kind":"agent-mcp","basis":basis,"reason":reason}),
    };
    let payload = json!({"originalSha256":original,"backupSha256":backup,"confirmed":true,"actor":identity});
    let payload_hash = hash(&serde_json::to_vec(&canonical_json(&payload)).map_err(error)?);
    let _admission = s.background.edit_guard()?;
    let mut locked = s.store.lock().map_err(error)?;
    let store = locked.as_mut().ok_or("This instance does not own the journal")?;
    if let Some(receipt) = store.doc["recoveryOperations"].get(operation_id) {
        if receipt["payloadHash"] != payload_hash { return Err("Recovery operation ID was already used with a different request".into()); }
        return Ok(json!({"receipt":receipt,"revision":store.revision(),"replayed":true,
            "recoveryState":snapshot_locked(s,store,&instance)?}));
    }
    if crate::operation_id_used(&store.doc, operation_id) {
        return Err("Recovery operation ID already belongs to another application operation".into());
    }
    let receipt = json!({"operationId":operation_id,"payloadHash":payload_hash,"phase":"applied",
        "recordedAt":now(),"originalSha256":original,"backupSha256":backup,"provenance":origin.provenance(s.fixture)});
    let recovered = store.recover_prepared(original.as_deref(), &backup, operation_id, receipt)?;
    let receipt = recovered["recoveryOperations"][operation_id].clone();
    let revision = store.revision();
    drop(locked);
    crate::ensure_gateway(s);
    crate::ensure_watcher(s);
    Ok(json!({"receipt":receipt,"revision":revision,"replayed":false,"recoveryState":snapshot(s)?}))
}

pub fn recover_ui(s: &Shared) -> Result<Value, String> {
    let reviewed = snapshot(s)?;
    if reviewed["available"] != true { return Err("No current valid recovery copy is available. Inspect the recovery state.".into()); }
    let request = json!({"operationId":format!("ui-recovery-{}-{}",std::process::id(),chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)),
        "expectedInstanceId":reviewed["instanceId"],"expectedOriginalSha256":reviewed["originalSha256"],
        "expectedBackupSha256":reviewed["backupSha256"],"confirmed":true});
    control(s, &request, MutationOrigin::ManualUi)?;
    document(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn broken() -> Shared {
        let s = Arc::new(crate::tests::fixture());
        let mut backup = document(&s).unwrap();
        backup["retainedUnknown"] = json!({"future":"preserved"});
        fs::write(s.data_path.with_extension("json.bak"),serde_json::to_vec(&backup).unwrap()).unwrap();
        drop(s.store.lock().unwrap().take());
        fs::write(&s.data_path,b"{malformed original").unwrap();
        *s.store.lock().unwrap()=Some(Store::open(s.data_path.clone(),&s.volumes[0].root).unwrap());
        s
    }
    fn request(s:&Shared,id:&str)->Value {
        let state=snapshot(s).unwrap();
        json!({"operationId":id,"expectedInstanceId":state["instanceId"],
            "expectedOriginalSha256":state["originalSha256"],"expectedBackupSha256":state["backupSha256"],"confirmed":true})
    }
    fn agent()->MutationOrigin { MutationOrigin::Agent{basis:"user_instruction".into(),reason:"Explicit synthetic journal recovery".into()} }

    #[test]
    fn recovery_is_fenced_preserves_original_and_replays_without_restoring_twice() {
        let s=broken();
        assert_eq!(snapshot(&s).unwrap()["available"],true);
        let mut req=request(&s,"recovery-fixture-once");
        let result=control(&s,&req,agent()).unwrap();
        assert_eq!(result["receipt"]["phase"],"applied");
        assert_eq!(document(&s).unwrap()["retainedUnknown"]["future"],"preserved");
        let saved=fs::read(&s.data_path).unwrap();
        req["expectedInstanceId"]=snapshot(&s).unwrap()["instanceId"].clone();
        let again=control(&s,&req,agent()).unwrap();
        assert_eq!(again["replayed"],true);
        assert_eq!(fs::read(&s.data_path).unwrap(),saved);
        let originals:Vec<_>=fs::read_dir(s.data_path.parent().unwrap()).unwrap().flatten()
            .filter(|entry|entry.file_name().to_string_lossy().contains("recovery-original-")).collect();
        assert_eq!(originals.len(),1);
        assert_eq!(fs::read(originals[0].path()).unwrap(),b"{malformed original");
        let mut changed=req.clone(); changed["expectedBackupSha256"]=json!("0".repeat(64));
        assert!(control(&s,&changed,agent()).unwrap_err().contains("different request"));
    }

    #[test]
    fn recovery_refuses_missing_authority_stale_instance_and_changed_backup_or_original() {
        let s=broken(); let req=request(&s,"recovery-stale-fixture");
        let before=fs::read(&s.data_path).unwrap();
        let mut unconfirmed=req.clone();unconfirmed["confirmed"]=json!(false);
        assert!(control(&s,&unconfirmed,agent()).is_err());
        assert!(control(&s,&req,MutationOrigin::Agent{basis:"assistant_suggestion".into(),reason:"proposal".into()}).is_err());
        let mut stale=req.clone();stale["expectedInstanceId"]=json!("old-app-instance-value");
        assert!(control(&s,&stale,agent()).is_err());
        fs::write(s.data_path.with_extension("json.bak"),b"{}").unwrap();
        assert!(control(&s,&req,agent()).unwrap_err().contains("recovery copy changed"));
        assert_eq!(fs::read(&s.data_path).unwrap(),before);
        let s=broken();let req=request(&s,"recovery-original-changed");
        fs::write(&s.data_path,b"different external bytes").unwrap();
        assert!(control(&s,&req,agent()).unwrap_err().contains("original changed"));
        assert_eq!(fs::read(&s.data_path).unwrap(),b"different external bytes");
    }

    #[test]
    fn recovery_receipt_survives_store_reopen_and_new_instance() {
        let s=broken();let mut req=request(&s,"recovery-reopened-fixture");
        control(&s,&req,agent()).unwrap();
        drop(s.store.lock().unwrap().take());
        *s.store.lock().unwrap()=Some(Store::open(s.data_path.clone(),&s.volumes[0].root).unwrap());
        s.gateway.lock().unwrap()["instanceId"]=json!("replacement-instance-value");
        req["expectedInstanceId"]=json!("replacement-instance-value");
        assert_eq!(control(&s,&req,agent()).unwrap()["replayed"],true);
    }
}
