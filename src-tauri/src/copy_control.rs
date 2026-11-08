//! Shared, explicitly authorized copy control. Journal receipts precede effects:
//! an uncertain acknowledgement must never launch the same operation twice.
use crate::{
    copy_destination, filesystem,
    store::{error, hash, now},
    AppState, MutationOrigin,
};
use serde_json::{json, Value};

fn instance(s: &AppState) -> Result<String, String> {
    s.gateway.lock().map_err(error)?["instanceId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .ok_or(
            "The app instance is not ready. Read the current connection state before copying."
                .into(),
        )
}

fn source_signature(s: &AppState) -> Result<String, String> {
    let sources: Vec<_> = s
        .volumes
        .iter()
        .map(|v| json!({"root":v.root,"id":v.id,"available":v.available}))
        .collect();
    Ok(hash(&serde_json::to_vec(&sources).map_err(error)?))
}

fn operations(doc: &Value) -> Result<Option<&serde_json::Map<String, Value>>, String> {
    match doc.get("copyOperations") {
        None => Ok(None),
        Some(Value::Object(items)) => Ok(Some(items)),
        Some(_) => Err(
            "Saved copy operation receipts have an unsupported format. Originals preserved.".into(),
        ),
    }
}

fn latest(doc: &Value) -> Result<Option<&Value>, String> {
    Ok(operations(doc)?.and_then(|items| {
        items
            .values()
            .max_by_key(|item| item["intentRevision"].as_u64().unwrap_or(0))
    }))
}

fn job_status(
    s: &AppState,
    receipt: Option<&Value>,
    current_instance: &str,
) -> Result<Value, String> {
    let current = s.copy_queue.status()?;
    let Some(receipt) = receipt else {
        return Ok(current);
    };
    if receipt["instanceId"] == current_instance && receipt["jobId"] == current["jobId"] {
        return Ok(current);
    }
    let failed = receipt["phase"] == "failed";
    let previous_session = receipt["instanceId"] != current_instance;
    Ok(json!({
        "jobId":receipt["jobId"],"mode":receipt["mode"],"scope":receipt["scope"],
        "sourceRevision":receipt["sourceRevision"],"running":false,"pauseRequested":false,
        "phase":if failed {"failed"} else if previous_session {"interrupted"} else {"uncertain"},
        "completionKnown":failed,
        "problem":if failed {receipt["error"].clone()} else {json!("This job has no current worker status. Its earlier effects may exist; inspect the destination and use an explicitly confirmed new start. Retrying the original operation never starts work.")}
    }))
}

pub fn snapshot(s: &AppState) -> Result<Value, String> {
    let _control = s.copy_control.lock().map_err(error)?;
    let current_instance = instance(s)?;
    let doc = crate::document(s)?;
    let destination = copy_destination(s);
    let mut snapshot = s.copy_queue.snapshot(&doc, &s.volumes, &destination)?;
    let receipt = latest(&doc)?;
    snapshot["revision"] = doc["revision"].clone();
    snapshot["instanceId"] = json!(current_instance);
    snapshot["sourceVolumeSignature"] = json!(source_signature(s)?);
    snapshot["sourceVolumes"] = json!(s
        .volumes
        .iter()
        .map(|v| json!({"root":v.root,"id":v.id,"available":v.available}))
        .collect::<Vec<_>>());
    snapshot["status"] = job_status(s, receipt, &current_instance)?;
    snapshot["latestOperation"] = receipt.cloned().unwrap_or(Value::Null);
    Ok(snapshot)
}

fn text<'a>(request: &'a Value, name: &str) -> Result<&'a str, String> {
    request[name]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 32768)
        .ok_or_else(|| format!("Missing or invalid {name}"))
}

/// Validate an explicit session-setting request without starting work. The UI
/// still submits a freshly fenced copy operation for each later auto-copy pass.
pub fn validate_fences(s: &AppState, request: &Value) -> Result<Value, String> {
    let _control = s.copy_control.lock().map_err(error)?;
    crate::healthy_owner(s)?;
    let current_instance = instance(s)?;
    let doc = crate::document(s)?;
    let destination = copy_destination(s);
    let mut view = checked_queue(s, request, &doc, &current_instance, &destination)?;
    view["revision"] = doc["revision"].clone();
    view["instanceId"] = json!(current_instance);
    view["sourceVolumeSignature"] = json!(source_signature(s)?);
    Ok(view)
}

fn checked_queue(
    s: &AppState,
    request: &Value,
    doc: &Value,
    current_instance: &str,
    destination: &filesystem::Volume,
) -> Result<Value, String> {
    if request["confirmed"] != true {
        return Err("Explicit confirmation is required for copy control.".into());
    }
    if text(request, "expectedInstanceId")? != current_instance {
        return Err(
            "The native app instance changed. Reconnect and reconcile before copy control.".into(),
        );
    }
    if request["expectedRevision"].as_u64() != doc["revision"].as_u64() {
        return Err(
            "The review changed. Inspect the current copy queue before trying again.".into(),
        );
    }
    let queue = s.copy_queue.snapshot(doc, &s.volumes, destination)?;
    if queue["queueSignature"] != text(request, "expectedQueueSignature")? {
        return Err(
            "The copy queue changed. Inspect the current queue before trying again.".into(),
        );
    }
    if source_signature(s)? != text(request, "expectedSourceVolumeSignature")? {
        return Err("The configured source volumes changed. Inspect the current queue.".into());
    }
    if destination.id != text(request, "expectedDestinationId")? {
        return Err("The copy destination changed. Check the drive before continuing.".into());
    }
    Ok(queue)
}

fn response(
    s: &AppState,
    receipt: Value,
    revision: u64,
    replayed: bool,
    current_instance: &str,
) -> Result<Value, String> {
    let status = job_status(s, Some(&receipt), current_instance)?;
    Ok(json!({"receipt":receipt,"revision":revision,"replayed":replayed,"status":status}))
}

pub fn control(s: &AppState, request: &Value, origin: MutationOrigin) -> Result<Value, String> {
    control_inner(s, request, origin, || {})
}

fn control_inner(
    s: &AppState,
    request: &Value,
    origin: MutationOrigin,
    after_effect: impl FnOnce(),
) -> Result<Value, String> {
    let fields = request
        .as_object()
        .ok_or("Copy control requires an object")?;
    if fields.keys().any(|key| {
        !matches!(
            key.as_str(),
            "action"
                | "operationId"
                | "expectedInstanceId"
                | "expectedRevision"
                | "expectedQueueSignature"
                | "expectedSourceVolumeSignature"
                | "expectedDestinationId"
                | "confirmed"
                | "mode"
                | "scope"
                | "expectedJobId"
        )
    }) {
        return Err("Unsupported copy control field".into());
    }
    origin.validate("copy")?;
    if matches!(&origin, MutationOrigin::Agent { basis, .. } if basis != "user_instruction") {
        return Err("Copy control requires an explicit user instruction; assistant suggestions cannot start or pause copying.".into());
    }
    if request["confirmed"] != true {
        return Err("Explicit confirmation is required for copy control.".into());
    }
    let action = text(request, "action")?;
    if !matches!(action, "start" | "resume" | "pause") {
        return Err("Unknown copy control action".into());
    }
    let _admission = if action == "pause" { s.background.edit_guard()? } else { s.background.start_guard()? };
    let operation_id = text(request, "operationId")?;
    if operation_id.len() > 200 {
        return Err("Invalid operation identifier".into());
    }
    let expected_revision = request["expectedRevision"]
        .as_u64()
        .ok_or("Missing expectedRevision")?;
    let expected_queue = text(request, "expectedQueueSignature")?;
    let expected_sources = text(request, "expectedSourceVolumeSignature")?;
    let expected_destination = text(request, "expectedDestinationId")?;
    let expected_instance = text(request, "expectedInstanceId")?;
    let mut payload = request.clone();
    payload
        .as_object_mut()
        .unwrap()
        .remove("expectedInstanceId");
    let actor = match &origin {
        MutationOrigin::ManualUi => json!({"kind":"manual-ui"}),
        MutationOrigin::Agent { basis, reason } => {
            json!({"kind":"agent-mcp","basis":basis,"reason":reason})
        }
    };
    let payload_hash = hash(
        &serde_json::to_vec(&crate::canonical_json(
            &json!({"request":payload,"origin":actor}),
        ))
        .map_err(error)?,
    );
    // Always take control before Store. Classification takes Store only and its
    // safety pause never takes this gate, avoiding an inverted lock dependency.
    let _control = s.copy_control.lock().map_err(error)?;
    let current_instance = instance(s)?;
    if expected_instance != current_instance {
        return Err(
            "The native app instance changed. Reconnect and reconcile before copy control.".into(),
        );
    }
    let mut owner = s.store.lock().map_err(error)?;
    let store = owner
        .as_mut()
        .ok_or("This instance does not own the review data file")?;
    if let Some(problem) = &store.problem {
        return Err(problem.clone());
    }
    if let Some(saved) = operations(&store.doc)?.and_then(|items| items.get(operation_id)) {
        if saved["payloadHash"] != payload_hash {
            return Err(
                "This copy operationId was already used for a different payload or origin.".into(),
            );
        }
        return response(s, saved.clone(), store.revision(), true, &current_instance);
    }
    if store.doc["agentOperations"].get(operation_id).is_some()
        || store.doc["recoveryOperations"].get(operation_id).is_some()
        || store.doc["recentOperations"]
            .as_array()
            .is_some_and(|items| items.iter().any(|id| id == operation_id))
        || store.doc["history"]
            .as_array()
            .is_some_and(|items| items.iter().any(|event| event["id"] == operation_id))
    {
        return Err("This operationId already belongs to another saved action. Choose an unused identifier after reconciling state.".into());
    }
    let destination = copy_destination(s);
    checked_queue(s, request, &store.doc, &current_instance, &destination)?;
    let current_job = s.copy_queue.status()?;
    let previous_job = if action == "start" {
        None
    } else {
        Some(text(request, "expectedJobId")?)
    };
    if previous_job.is_some_and(|id| current_job["jobId"].as_str() != Some(id)) {
        return Err("The copy job changed. Inspect its current status before continuing.".into());
    }
    let (mode, scope) = if action == "start" {
        if request.get("expectedJobId").is_some_and(|v| !v.is_null()) {
            return Err("A new start cannot target an earlier job".into());
        }
        let mode = text(request, "mode")?.to_owned();
        let scope = request
            .get("scope")
            .filter(|v| !v.is_null())
            .map(|v| v.as_str().ok_or("scope must be a path or null"))
            .transpose()?;
        for volume in &s.volumes {
            filesystem::identify(&volume.root, &s.volumes)?;
        }
        let scope = s
            .copy_queue
            .validate_start_request(&s.volumes, &destination, &mode, scope)?;
        (Some(mode), scope)
    } else {
        if request.get("mode").is_some_and(|v| !v.is_null())
            || request.get("scope").is_some_and(|v| !v.is_null())
        {
            return Err("Pause and resume use the selected job's mode and scope".into());
        }
        if action == "resume" {
            if current_job["phase"] != "paused" || current_job["running"] == true {
                return Err("Only a stopped, paused job from this app session can resume. Use a new start after a restart.".into());
            }
            for volume in &s.volumes {
                filesystem::identify(&volume.root, &s.volumes)?;
            }
            let mode = current_job["mode"]
                .as_str()
                .ok_or("Paused job mode unavailable")?
                .to_owned();
            let scope = s.copy_queue.validate_start_request(
                &s.volumes,
                &destination,
                &mode,
                current_job["scope"].as_str(),
            )?;
            (Some(mode), scope)
        } else {
            if current_job["running"] != true && current_job["phase"] != "paused" {
                return Err("This copy job has already stopped.".into());
            }
            (
                current_job["mode"].as_str().map(str::to_owned),
                current_job["scope"].as_str().map(str::to_owned),
            )
        }
    };
    let job_id = if action == "pause" {
        previous_job.unwrap().to_owned()
    } else {
        hash(format!("{current_instance}|{operation_id}").as_bytes())
    };
    let mut receipt = json!({"operationId":operation_id,"payloadHash":payload_hash,"action":action,
        "jobId":job_id,"previousJobId":previous_job,"instanceId":current_instance,
        "sourceRevision":if action=="pause" {current_job["sourceRevision"].clone()} else {json!(expected_revision)},
        "reviewedRevision":expected_revision,"intentRevision":store.revision()+1,
        "phase":"starting","requestedAt":now(),"mode":mode,"scope":scope,
        "queueSignature":expected_queue,"sourceVolumeSignature":expected_sources,
        "destinationId":expected_destination,"provenance":origin.provenance(s.fixture)});
    let source_document = store.doc.clone();
    let mut saved = source_document.clone();
    if saved.get("copyOperations").is_none() {
        saved["copyOperations"] = json!({});
    }
    saved["copyOperations"][operation_id] = receipt.clone();
    store.commit(saved)?;
    // From this point a persisted intent exists. The prepared worker waits for
    // its accepted receipt, so journal replacement cannot race source fencing.
    // Losing either acknowledgement never permits replay to start another job.
    let (release, launch) = std::sync::mpsc::channel();
    let effect = if action == "pause" {
        s.copy_queue.pause_job(&job_id)
    } else {
        s.copy_queue.start_with_job(
            source_document,
            s.volumes.clone(),
            destination,
            mode.as_deref().unwrap(),
            scope.as_deref(),
            &job_id,
            Some(launch),
        )
    };
    let release_worker = effect.is_ok() && action != "pause";
    after_effect();
    match effect {
        Ok(status) => {
            receipt["phase"] = json!("accepted");
            receipt["acceptedStatus"] = status;
        }
        Err(problem) => {
            receipt["phase"] = json!("failed");
            receipt["error"] = json!(problem);
        }
    }
    receipt["committedRevision"] = json!(store.revision() + 1);
    let mut saved = store.doc.clone();
    saved["copyOperations"][operation_id] = receipt.clone();
    if let Err(problem) = store.commit(saved) {
        return Err(format!("Copy intent {operation_id} was saved and may have taken effect, but its acknowledgement could not be saved: {problem}. Inspect the operation; retrying its original identifier will not start it again."));
    }
    if release_worker {
        release.send(()).map_err(|_| "The accepted copy job lost its prepared worker. Inspect the saved receipt; retrying its identifier will not launch it again.".to_owned())?;
    }
    response(s, receipt, store.revision(), false, &current_instance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{copy_queue::CopyQueue, store::Store};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{Arc, Barrier},
        thread,
        time::{Duration, Instant},
    };

    fn fixture() -> AppState {
        let s = crate::tests::fixture();
        fs::create_dir(s.data_path.parent().unwrap().join("destination")).unwrap();
        s
    }

    fn file(s: &AppState, relative: &str) -> PathBuf {
        let path = Path::new(&s.volumes[0].root).join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"fixture payload").unwrap();
        path
    }

    fn mark(s: &AppState, path: &Path, status: &str, id: &str) {
        crate::run_mutation(
            s,
            crate::document(s).unwrap()["revision"].as_u64().unwrap(),
            id.into(),
            json!({"type":"classify","path":path.to_string_lossy(),"status":status}),
        )
        .unwrap();
    }

    fn request(s: &AppState, id: &str, action: &str) -> Value {
        let view = snapshot(s).unwrap();
        let mut request = json!({"action":action,"operationId":id,"confirmed":true,
            "expectedInstanceId":view["instanceId"],"expectedRevision":view["revision"],
            "expectedQueueSignature":view["queueSignature"],"expectedSourceVolumeSignature":view["sourceVolumeSignature"],
            "expectedDestinationId":view["destination"]["id"]});
        if action == "start" {
            request["mode"] = json!("all");
        } else {
            request["expectedJobId"] = view["status"]["jobId"].clone();
        }
        request
    }

    fn wait(s: &AppState) -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = s.copy_queue.status().unwrap();
            if status["running"] != true {
                return status;
            }
            assert!(Instant::now() < deadline, "Copy did not stop: {status}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn restart(s: &mut AppState) {
        let old = s.store.get_mut().unwrap().take();
        drop(old);
        *s.store.get_mut().unwrap() =
            Some(Store::open(s.data_path.clone(), &s.volumes[0].root).unwrap());
        s.copy_queue = CopyQueue::new(
            s.data_path.parent().unwrap().join("copy-receipts.sqlite"),
            Vec::new(),
            true,
        );
        *s.gateway.get_mut().unwrap() =
            json!({"state":"running","instanceId":"replacement-fixture-instance"});
    }

    #[test]
    fn copy_fences_confirmation_and_agent_authorization_fail_without_effects() {
        let s = fixture();
        let path = file(&s, "Work\\keep.txt");
        mark(&s, &path, "migrate-now", "mark");
        let baseline = crate::document(&s).unwrap();
        let prepared = request(&s, "copy", "start");
        for field in [
            "expectedInstanceId",
            "expectedQueueSignature",
            "expectedSourceVolumeSignature",
            "expectedDestinationId",
        ] {
            let mut wrong = prepared.clone();
            wrong[field] = json!("stale");
            assert!(
                control(&s, &wrong, MutationOrigin::ManualUi).is_err(),
                "{field}"
            );
        }
        for (field, value) in [
            ("expectedRevision", json!(999999)),
            ("confirmed", json!(false)),
            ("action", json!("delete")),
        ] {
            let mut wrong = prepared.clone();
            wrong[field] = value;
            assert!(
                control(&s, &wrong, MutationOrigin::ManualUi).is_err(),
                "{field}"
            );
        }
        assert!(control(
            &s,
            &prepared,
            MutationOrigin::Agent {
                basis: "assistant_suggestion".into(),
                reason: "Unapproved fixture proposal".into()
            }
        )
        .is_err());
        assert_eq!(crate::document(&s).unwrap(), baseline);
        assert_eq!(s.copy_queue.status().unwrap()["phase"], "idle");
        assert!(!s
            .data_path
            .parent()
            .unwrap()
            .join("destination\\Work\\keep.txt")
            .exists());
        assert!(validate_fences(&s, &prepared).is_ok());
        let mut saved = crate::document(&s).unwrap();
        saved["recoveryOperations"] = json!({"recovery-id":{"operationId":"recovery-id"}});
        s.store
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .commit(saved)
            .unwrap();
        let collision = request(&s, "recovery-id", "start");
        assert!(control(&s, &collision, MutationOrigin::ManualUi)
            .unwrap_err()
            .contains("another saved action"));
        assert_eq!(s.copy_queue.status().unwrap()["phase"], "idle");
    }

    #[test]
    fn destination_replacement_and_source_configuration_changes_refuse_prepared_start() {
        let mut s = fixture();
        let path = file(&s, "Work\\keep.txt");
        mark(&s, &path, "migrate-now", "mark");
        let prepared = request(&s, "prepared", "start");
        let baseline = crate::document(&s).unwrap();
        let parent = s.data_path.parent().unwrap();
        fs::rename(parent.join("destination"), parent.join("old-destination")).unwrap();
        fs::create_dir(parent.join("destination")).unwrap();
        assert!(control(&s, &prepared, MutationOrigin::ManualUi).is_err());
        let fresh = request(&s, "source-replaced", "start");
        s.volumes[0].id = "changed-source-volume".into();
        assert!(control(&s, &fresh, MutationOrigin::ManualUi).is_err());
        assert_eq!(crate::document(&s).unwrap(), baseline);
        assert_eq!(s.copy_queue.status().unwrap()["phase"], "idle");
    }

    #[test]
    fn durable_copy_retry_reopens_without_relaunch_or_payload_reuse() {
        let mut s = fixture();
        let path = file(&s, "Work\\keep.txt");
        mark(&s, path.parent().unwrap(), "migrate-now", "mark");
        let mut prepared = request(&s, "durable-copy", "start");
        let source_revision = prepared["expectedRevision"].clone();
        let accepted = control_inner(&s, &prepared, MutationOrigin::ManualUi, || {
            assert_eq!(s.copy_queue.status().unwrap()["phase"], "planning");
            assert!(!s
                .data_path
                .parent()
                .unwrap()
                .join("destination\\Work\\keep.txt")
                .exists());
        })
        .unwrap();
        assert_eq!(accepted["receipt"]["phase"], "accepted");
        assert_eq!(accepted["receipt"]["sourceRevision"], source_revision);
        assert_eq!(
            accepted["revision"].as_u64(),
            source_revision.as_u64().map(|r| r + 2)
        );
        let finished = wait(&s);
        assert_eq!(finished["phase"], "complete", "{finished}");
        let first_job = s.copy_queue.status().unwrap()["jobId"].clone();
        let added = file(&s, "Work\\added-after-first-pass.txt");
        let retried = control(&s, &prepared, MutationOrigin::ManualUi).unwrap();
        assert_eq!(retried["receipt"], accepted["receipt"]);
        assert_eq!(retried["replayed"], true);
        assert_eq!(s.copy_queue.status().unwrap()["jobId"], first_job);
        restart(&mut s);
        prepared["expectedInstanceId"] = json!("replacement-fixture-instance");
        let reopened = control(&s, &prepared, MutationOrigin::ManualUi).unwrap();
        assert_eq!(reopened["receipt"], accepted["receipt"]);
        assert_eq!(reopened["status"]["phase"], "interrupted");
        assert_eq!(reopened["status"]["completionKnown"], false);
        assert_eq!(snapshot(&s).unwrap()["status"]["phase"], "interrupted");
        assert_eq!(s.copy_queue.status().unwrap()["phase"], "idle");
        let resume_after_restart = request(&s, "resume-after-restart", "resume");
        assert!(control(&s, &resume_after_restart, MutationOrigin::ManualUi).is_err());
        assert!(!s
            .data_path
            .parent()
            .unwrap()
            .join("destination\\Work")
            .join(added.file_name().unwrap())
            .exists());
        let mut different = prepared.clone();
        different["mode"] = json!("ordered");
        assert!(control(&s, &different, MutationOrigin::ManualUi).is_err());
        assert!(crate::run_mutation(
            &s,
            crate::document(&s).unwrap()["revision"].as_u64().unwrap(),
            "durable-copy".into(),
            json!({"type":"view","patch":{"search":"reuse"}})
        )
        .is_err());
        let fresh = request(&s, "explicit-new-start", "start");
        control(&s, &fresh, MutationOrigin::ManualUi).unwrap();
        assert_eq!(wait(&s)["phase"], "complete");
        assert!(s
            .data_path
            .parent()
            .unwrap()
            .join("destination\\Work")
            .join(added.file_name().unwrap())
            .is_file());
    }

    #[test]
    fn all_copies_only_now_and_last_moment_requires_its_own_explicit_scope() {
        let s = fixture();
        let now = file(&s, "Desktop\\Coding\\Now\\keep.txt");
        let later = file(&s, "Desktop\\Coding\\Later\\late.txt");
        let pending = file(&s, "Desktop\\Coding\\Now\\pending.txt");
        let confirmed = file(&s, "Desktop\\Coding\\Now\\confirmed.txt");
        mark(
            &s,
            now.parent().unwrap().parent().unwrap(),
            "migrate-now",
            "now",
        );
        mark(&s, later.parent().unwrap(), "last-moment", "later");
        mark(&s, &pending, "manual-pending", "pending");
        mark(&s, &confirmed, "manual-confirmed", "confirmed");
        let prepared = request(&s, "copy-now", "start");
        let result = control(
            &s,
            &prepared,
            MutationOrigin::Agent {
                basis: "user_instruction".into(),
                reason: "The user explicitly requested copying the fixture Migrate now queue"
                    .into(),
            },
        )
        .unwrap();
        assert_eq!(result["receipt"]["phase"], "accepted");
        assert_eq!(wait(&s)["phase"], "complete");
        let target = s
            .data_path
            .parent()
            .unwrap()
            .join("destination\\Desktop\\Coding");
        assert_eq!(
            fs::read(target.join("Now\\keep.txt")).unwrap(),
            b"fixture payload"
        );
        assert!(!target.join("Now\\pending.txt").exists());
        assert!(!target.join("Now\\confirmed.txt").exists());
        assert!(!target.join("Later\\late.txt").exists());
        let mut late = request(&s, "copy-later", "start");
        late["mode"] = json!("last-moment");
        assert!(control(&s, &late, MutationOrigin::ManualUi).is_err());
        late["scope"] = json!(later.parent().unwrap().to_string_lossy());
        control(&s, &late, MutationOrigin::ManualUi).unwrap();
        assert_eq!(wait(&s)["phase"], "complete");
        assert!(target.join("Later\\late.txt").is_file());
        assert!(!s
            .data_path
            .parent()
            .unwrap()
            .join("destination\\late.txt")
            .exists());
    }

    #[test]
    fn concurrent_starts_have_one_durable_winner_and_one_job() {
        let s = Arc::new(fixture());
        let path = file(&s, "Work\\keep.txt");
        mark(&s, &path, "migrate-now", "mark");
        let first = request(&s, "concurrent-first", "start");
        let mut second = first.clone();
        second["operationId"] = json!("concurrent-second");
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for prepared in [first, second] {
            let s = s.clone();
            let barrier = barrier.clone();
            workers.push(thread::spawn(move || {
                barrier.wait();
                control(&s, &prepared, MutationOrigin::ManualUi)
            }));
        }
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            operations(&crate::document(&s).unwrap())
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(wait(&s)["phase"], "complete");
    }

    #[test]
    fn pause_is_a_request_and_resume_requires_the_same_paused_session_job() {
        let s = fixture();
        let path = file(&s, "Work\\keep.txt");
        mark(&s, &path, "migrate-now", "mark");
        let barrier = Arc::new(Barrier::new(2));
        s.copy_queue.hold_next_worker(barrier.clone());
        let started =
            control(&s, &request(&s, "start", "start"), MutationOrigin::ManualUi).unwrap();
        let first_job = started["receipt"]["jobId"].clone();
        let mut pause = request(&s, "pause", "pause");
        let mut wrong = pause.clone();
        wrong["expectedJobId"] = json!("another-job");
        assert!(control(&s, &wrong, MutationOrigin::ManualUi).is_err());
        let paused = control(&s, &pause, MutationOrigin::ManualUi).unwrap();
        assert_eq!(paused["status"]["pauseRequested"], true);
        assert_eq!(paused["status"]["running"], true);
        assert_ne!(paused["status"]["phase"], "paused");
        barrier.wait();
        assert_eq!(wait(&s)["phase"], "paused");
        let mut resume = request(&s, "resume", "resume");
        let resumed = control(&s, &resume, MutationOrigin::ManualUi).unwrap();
        assert_ne!(resumed["receipt"]["jobId"], first_job);
        assert_eq!(resumed["receipt"]["previousJobId"], first_job);
        assert_eq!(wait(&s)["phase"], "complete");
        // An uncertain retry of the old pause must not pause the newer job.
        let replay = control(&s, &pause, MutationOrigin::ManualUi).unwrap();
        assert_eq!(replay["replayed"], true);
        assert_eq!(s.copy_queue.status().unwrap()["phase"], "complete");
        pause["operationId"] = json!("new-old-pause");
        pause["expectedRevision"] = crate::document(&s).unwrap()["revision"].clone();
        assert!(control(&s, &pause, MutationOrigin::ManualUi).is_err());
        resume["operationId"] = json!("resume-complete");
        resume["expectedRevision"] = crate::document(&s).unwrap()["revision"].clone();
        resume["expectedJobId"] = resumed["receipt"]["jobId"].clone();
        assert!(control(&s, &resume, MutationOrigin::ManualUi).is_err());
    }

    #[test]
    fn acknowledgement_save_failure_preserves_uncertain_intent_and_never_replays_effect() {
        let mut s = fixture();
        let path = file(&s, "Work\\keep.txt");
        mark(&s, path.parent().unwrap(), "migrate-now", "mark");
        let mut prepared = request(&s, "uncertain-copy", "start");
        let error = control_inner(&s, &prepared, MutationOrigin::ManualUi, || {
            let mut external: Value =
                serde_json::from_slice(&fs::read(&s.data_path).unwrap()).unwrap();
            assert_eq!(
                external["copyOperations"]["uncertain-copy"]["phase"],
                "starting"
            );
            external["externalFixtureMarker"] = json!(true);
            fs::write(&s.data_path, serde_json::to_vec_pretty(&external).unwrap()).unwrap();
        })
        .unwrap_err();
        assert!(error.contains("may have taken effect"));
        assert_eq!(wait(&s)["phase"], "failed");
        assert!(!s
            .data_path
            .parent()
            .unwrap()
            .join("destination\\Work\\keep.txt")
            .exists());
        file(&s, "Work\\must-not-replay.txt");
        restart(&mut s);
        prepared["expectedInstanceId"] = json!("replacement-fixture-instance");
        let replay = control(&s, &prepared, MutationOrigin::ManualUi).unwrap();
        assert_eq!(replay["receipt"]["phase"], "starting");
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["status"]["completionKnown"], false);
        assert_eq!(s.copy_queue.status().unwrap()["phase"], "idle");
        assert_eq!(crate::document(&s).unwrap()["externalFixtureMarker"], true);
        assert!(!s
            .data_path
            .parent()
            .unwrap()
            .join("destination\\Work\\must-not-replay.txt")
            .exists());
    }
}
