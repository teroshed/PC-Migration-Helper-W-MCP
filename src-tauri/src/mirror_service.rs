//! Native coordination for read-only mirror comparisons and exact user receipts.
use crate::{filesystem, mirror, store, watcher, AppState, MutationOrigin, Shared};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Runtime(Arc<Mutex<Live>>);

#[derive(Default)]
struct Live {
    operation: Option<String>,
    progress: Option<mirror::Progress>,
    cancel: Option<Arc<AtomicBool>>,
    worker: Option<JoinHandle<()>>,
    watcher: Option<watcher::WatchHandle>,
    verified: Option<mirror::EvidenceWindow>,
    prepared: Option<PreparedConfirmation>,
    outcome_problem: Option<String>,
}

struct PreparedConfirmation {
    revision: u64,
    membership: mirror::FrozenMembership,
    window: mirror::EvidenceWindow,
    prepared_at: Instant,
    binding: mirror::Binding,
}

pub(crate) struct ConfirmationGuard {
    binding: mirror::Binding,
    window: mirror::EvidenceWindow,
}
impl ConfirmationGuard {
    pub(crate) fn validate(&self, s: &AppState) -> Result<(), String> {
        mirror::revalidate_binding(&self.binding, environment(s)?.borrowed())?;
        s.mirrors.ensure_window(&self.window)
    }
}

impl Runtime {
    pub(crate) fn active(&self) -> Result<bool, String> {
        let live = self.0.lock().map_err(store::error)?;
        Ok(live.worker.as_ref().is_some_and(|job| !job.is_finished()))
    }

    pub(crate) fn cancel(&self) {
        if let Ok(live) = self.0.lock() {
            if let Some(cancel) = &live.cancel {
                cancel.store(true, Ordering::SeqCst);
            }
        }
    }

    // Pollable shutdown so an unresponsive reader or watcher preserves the
    // existing native exit ticket instead of extending the stop deadline.
    pub(crate) fn stopped(&self) -> Result<bool, String> {
        let mut live = self.0.lock().map_err(store::error)?;
        live.verified = None;
        live.prepared = None;
        if let Some(cancel) = &live.cancel {
            cancel.store(true, Ordering::SeqCst);
        }
        if let Some(watcher) = &live.watcher {
            if !watcher.stop_finished() {
                return Ok(false);
            }
        }
        live.watcher = None;
        if live.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Ok(false);
        }
        if let Some(worker) = live.worker.take() {
            worker.join().map_err(|_| "Mirror verification stopped unexpectedly; evidence is historical.")?;
        }
        Ok(true)
    }

    pub(crate) fn ensure_window(&self, expected: &mirror::EvidenceWindow) -> Result<(), String> {
        let live = self.0.lock().map_err(store::error)?;
        if live.verified.as_ref() != Some(expected)
            || live.watcher.as_ref().and_then(position) != Some((expected.source_sequence, expected.mirror_sequence))
        {
            return Err("Source or mirror evidence changed. Prepare the confirmation again.".into());
        }
        Ok(())
    }
}

fn text<'a>(body: &'a Value, name: &str, max: usize) -> Result<&'a str, String> {
    body[name].as_str().filter(|value| !value.is_empty() && value.len() <= max)
        .ok_or_else(|| format!("Invalid or missing {name}"))
}

fn digest<'a>(body: &'a Value, name: &str) -> Result<&'a str, String> {
    text(body, name, 64).and_then(|value| {
        if value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit()) {
            Ok(value)
        } else {
            Err(format!("Invalid {name} digest"))
        }
    })
}

fn environment(s: &AppState) -> Result<Environment, String> {
    if s.fixture {
        let source = PathBuf::from(&s.volumes.first().ok_or("No fixture volume")?.root);
        let mirror = source.parent().ok_or("Fixture root is unavailable")?.join("mirror");
        Ok(Environment::Fixture { source, mirror })
    } else {
        Ok(Environment::Live(s.volumes.clone()))
    }
}

enum Environment {
    Live(Vec<filesystem::Volume>),
    Fixture { source: PathBuf, mirror: PathBuf },
}
impl Environment {
    fn borrowed(&self) -> mirror::Environment<'_> {
        match self {
            Self::Live(volumes) => mirror::Environment::Live { review_volumes: volumes },
            Self::Fixture { source, mirror } => mirror::Environment::Fixture {
                source_root: source, mirror_root: mirror,
            },
        }
    }
}

fn evidence_directory(s: &AppState) -> PathBuf {
    s.runtime.join("mirror-evidence")
}

fn outcome_name(id: &str) -> String {
    format!("run-{}.json", store::hash(id.as_bytes()))
}

fn read_outcome(directory: &Path, id: &str) -> Result<Value, String> {
    let bytes = mirror::read_evidence_file(directory, &outcome_name(id), 64 * 1024)?;
    let outcome: Value = serde_json::from_slice(&bytes).map_err(store::error)?;
    if outcome["operationId"] != id {
        return Err("The saved mirror comparison outcome belongs to another operation.".into());
    }
    Ok(outcome)
}

fn current_binding(doc: &Value, id: &str) -> Result<mirror::Binding, String> {
    let binding: mirror::Binding = serde_json::from_value(doc["mirrorBindings"][id].clone())
        .map_err(|_| "The selected mirror binding does not exist or could not be loaded.")?;
    if binding.binding_id != id {
        return Err("Saved binding identity does not match.".into());
    }
    Ok(binding)
}

fn position(watcher: &watcher::WatchHandle) -> Option<(u64, u64)> {
    let source = watcher.evidence_position(0)?;
    let mirror = watcher.evidence_position(1)?;
    (source.continuous && mirror.continuous).then_some((source.sequence, mirror.sequence))
}

pub(crate) fn preview(s: &AppState, body: &Value) -> Result<Value, String> {
    let _observation = s.background.observe()?;
    let binding = mirror::bind(text(body, "source", 32_000)?, text(body, "mirror", 32_000)?,
        environment(s)?.borrowed())?;
    serde_json::to_value(binding).map_err(store::error)
}

pub(crate) fn apply_binding(s: &AppState, doc: &mut Value, op: &Value) -> Result<(), String> {
    if op["confirmed"] != true {
        return Err("Review both scope paths and explicitly confirm their binding.".into());
    }
    let binding = mirror::bind(text(op, "source", 32_000)?, text(op, "mirror", 32_000)?,
        environment(s)?.borrowed())?;
    if binding.binding_id != digest(op, "expectedBindingId")? {
        return Err("The source or mirror identity changed. Review the binding again.".into());
    }
    if doc.get("mirrorBindings").is_none() {
        doc["mirrorBindings"] = json!({});
    }
    let map = doc["mirrorBindings"].as_object_mut().ok_or("Invalid saved mirror bindings")?;
    map.insert(binding.binding_id.clone(), serde_json::to_value(binding).map_err(store::error)?);
    doc["schemaVersion"] = json!(2);
    Ok(())
}

fn validate_instance(s: &AppState, request: &Value) -> Result<String, String> {
    let instance = text(request, "expectedInstanceId", 100)?;
    if s.gateway.lock().map_err(store::error)?["instanceId"] != instance {
        return Err("The native app instance changed. Inspect the mirror state again.".into());
    }
    Ok(instance.to_owned())
}

fn start_worker(s: &AppState, live: &mut Live, operation: &str, instance: String,
    binding: mirror::Binding, directory: PathBuf, env: Environment) -> Result<(), String> {
    let shared = s.mirrors.0.clone();
    let id = operation.to_owned();
    let cancel = Arc::new(AtomicBool::new(false));
    let work_cancel = cancel.clone();
    let worker = thread::Builder::new().name("migration-mirror-compare".into()).spawn(move || {
        let monitor = watcher::start(vec![binding.source.root.clone(), binding.mirror.root.clone()],
            Vec::new(), Arc::new(|_| {}));
        let beginning = monitor.as_ref().ok().and_then(position);
        let mut monitoring_issue = monitor.as_ref().err().cloned();
        let result = mirror::verify(&binding, env.borrowed(), &directory, &work_cancel, |progress| {
            if let Ok(mut live) = shared.lock() {
                live.progress = Some(progress.clone());
            }
        });
        let mut verified = None;
        let outcome = match result {
            Ok(receipt) => {
                if let (Some(before), Some(after)) = (beginning, monitor.as_ref().ok().and_then(position)) {
                    if before == after && !work_cancel.load(Ordering::SeqCst) {
                        verified = Some(mirror::EvidenceWindow { instance_id: instance,
                            binding_id: binding.binding_id.clone(), manifest_digest: receipt.manifest_digest.clone(),
                            source_sequence: after.0, mirror_sequence: after.1,
                            source_continuous: true, mirror_continuous: true });
                    }
                }
                if verified.is_none() && monitoring_issue.is_none() {
                    monitoring_issue = Some("The comparison is dated evidence. An uninterrupted unchanged monitoring window was not established.".into());
                }
                json!({"operationId":id,"phase":"complete","receipt":receipt,"monitoringIssue":monitoring_issue})
            }
            Err(problem) => json!({"operationId":id,
                "phase":if work_cancel.load(Ordering::SeqCst) {"canceled"} else {"error"},"problem":problem}),
        };
        let save = serde_json::to_vec_pretty(&outcome).map_err(store::error)
            .and_then(|bytes| mirror::publish_evidence_file(&directory, &outcome_name(&id), &bytes));
        if let Ok(mut live) = shared.lock() {
            live.progress = None;
            live.watcher = monitor.ok();
            live.verified = if save.is_ok() { verified } else { None };
            live.outcome_problem = save.err();
        }
    }).map_err(store::error)?;
    live.operation = Some(operation.into());
    live.cancel = Some(cancel);
    live.worker = Some(worker);
    live.progress = None;
    live.verified = None;
    live.prepared = None;
    live.outcome_problem = None;
    Ok(())
}

pub(crate) fn start(s: &AppState, request: &Value, origin: MutationOrigin) -> Result<Value, String> {
    origin.validate("mirror compare")?;
    if request["confirmed"] != true {
        return Err("Explicit user confirmation is required to compare the bound source and mirror.".into());
    }
    let operation = text(request, "operationId", 120)?;
    let binding_id = digest(request, "bindingId")?;
    let instance = validate_instance(s, request)?;
    let expected = request["expectedRevision"].as_u64().ok_or("Missing expectedRevision")?;
    let mut identity = request.clone();
    identity.as_object_mut().ok_or("Invalid comparison request")?.remove("expectedInstanceId");
    let payload = origin.payload_hash(&identity)?.unwrap_or(store::hash(
        &serde_json::to_vec(&crate::canonical_json(&json!({"origin":"manual-ui","request":identity})))
            .map_err(store::error)?));
    let _admission = s.background.start_guard()?;
    let mut owner = s.store.lock().map_err(store::error)?;
    let store = owner.as_mut().ok_or("The review journal is unavailable")?;
    if let Some(problem) = &store.problem {
        return Err(problem.clone());
    }
    if let Some(previous) = store.doc["mirrorOperations"].get(operation) {
        if previous["payloadHash"] != payload {
            return Err("This mirror operation ID already belongs to a different request.".into());
        }
        return Ok(json!({"replayed":true,"revision":store.revision(),"operation":previous}));
    }
    if crate::operation_id_used(&store.doc, operation) {
        return Err("This operation ID belongs to another saved action.".into());
    }
    if store.revision() != expected {
        return Err("The review changed. Inspect mirror state before starting again.".into());
    }
    let binding = current_binding(&store.doc, binding_id)?;
    let env = environment(s)?;
    mirror::revalidate_binding(&binding, env.borrowed())?;
    let mut live = s.mirrors.0.lock().map_err(store::error)?;
    if live.worker.as_ref().is_some_and(|job| !job.is_finished()) {
        return Err("A mirror comparison is already running.".into());
    }
    if let Some(watcher) = &live.watcher {
        if !watcher.stop_finished() {
            return Err("The prior mirror monitor is stopping. Try again when it has stopped.".into());
        }
        live.watcher = None;
    }
    if let Some(old) = live.worker.take() {
        old.join().map_err(|_| "The prior mirror comparison stopped unexpectedly.")?;
    }
    let directory = mirror::create_mirror_evidence_directory(&s.runtime, "mirror-evidence", &binding, env.borrowed())?;
    let mut candidate = store.doc.clone();
    if candidate.get("mirrorOperations").is_none() {
        candidate["mirrorOperations"] = json!({});
    }
    let receipt = json!({"operationId":operation,"payloadHash":payload,"bindingId":binding_id,
        "phase":"accepted","instanceId":instance,"startedAt":store::now(),"provenance":origin.provenance(s.fixture)});
    candidate["mirrorOperations"].as_object_mut().ok_or("Invalid mirror operation journal")?
        .insert(operation.into(), receipt.clone());
    candidate["lastMirrorOperation"] = json!(operation);
    let saved = store.commit(candidate)?;
    start_worker(s, &mut live, operation, instance, binding, directory, env).map_err(|problem|
        format!("Comparison intent was recorded but the worker could not start: {problem}. Use a new reviewed operation."))?;
    Ok(json!({"operation":receipt,"revision":saved["revision"],"replayed":false}))
}

pub(crate) fn status(s: &AppState, request: &Value) -> Result<Value, String> {
    let doc = crate::document(s)?;
    let id = request.get("operationId").and_then(Value::as_str)
        .or_else(|| doc["lastMirrorOperation"].as_str());
    let bindings = doc["mirrorBindings"].as_object().map(|map| map.values().take(32).collect::<Vec<_>>())
        .unwrap_or_default();
    let live = s.mirrors.0.lock().map_err(store::error)?;
    let mut current = Value::Null;
    if let Some(id) = id {
        let operation = doc["mirrorOperations"].get(id).ok_or("The comparison operation does not exist.")?;
        let active = live.operation.as_deref() == Some(id)
            && live.worker.as_ref().is_some_and(|job| !job.is_finished());
        let read = if active { None } else { Some(read_outcome(&evidence_directory(s), id)) };
        let outcome = read.as_ref().and_then(|value| value.as_ref().ok());
        let verified = live.verified.as_ref().is_some_and(|window| {
            outcome.is_some_and(|result| result["receipt"]["manifestDigest"] == window.manifest_digest)
                && live.watcher.as_ref().and_then(position) == Some((window.source_sequence, window.mirror_sequence))
        });
        current = json!({"operation":operation,"active":active,"progress":if active {live.progress.as_ref()} else {None},
            "outcome":outcome,"freshness":if verified {"monitored"} else {"historical-or-unknown"},
            "outcomeProblem":if live.operation.as_deref()==Some(id) {live.outcome_problem.as_deref()} else {None},
            "interruptedOrUnavailable":!active && outcome.is_none()});
    }
    let confirmations = doc["mirrorConfirmations"].as_object()
        .map(|map| map.values().take(128).map(|entry| json!({
            "operationId":entry["operationId"],"confirmedAt":entry["confirmedAt"],
            "bindingId":entry["membership"]["bindingId"],"manifestDigest":entry["membership"]["manifestDigest"],
            "membershipDigest":entry["membership"]["membershipDigest"],"coveredFiles":entry["membership"]["coveredFiles"],
            "coveredDirectories":entry["membership"]["coveredDirectories"],
            "exceptionsCount":entry["membership"]["exceptions"].as_array().map_or(0,Vec::len),
        })).collect::<Vec<_>>()).unwrap_or_default();
    let manual_pending = doc["decisions"].as_object().into_iter().flat_map(|map| map.values())
        .filter(|mark| mark["status"] == "manual-pending" && mark["noteOnly"] != true).count();
    let deferred = doc["assistance"]["deferred"].as_array().map_or(0, Vec::len);
    let observed = &current["outcome"]["receipt"]["counts"];
    let coverage = json!({"savedManualPendingMarks":manual_pending,"deferredInterviewScopes":deferred,
        "missing":observed["missing"],"different":observed["different"],"unsupported":observed["unsupported"],
        "unreadable":observed["unreadable"],"coverageGaps":observed["coverageGaps"],
        "comparisonHistorical":current["outcome"]["phase"] == "complete" && current["freshness"] != "monitored",
        "coverageScope":"Counts describe saved review records and the selected comparison. Closing review does not establish complete backup or restore."});
    Ok(json!({"revision":doc["revision"],"bindings":bindings,"comparison":current,
        "confirmations":confirmations,"coverage":coverage,"countsAreObserved":true,
        "bindingCount":doc["mirrorBindings"].as_object().map_or(0,|items|items.len()),
        "confirmationCount":doc["mirrorConfirmations"].as_object().map_or(0,|items|items.len()),
        "artifactNote":"A saved outcome is historical evidence. Inspect its manifest before relying on individual entries."}))
}

pub(crate) fn cancel(s: &AppState, request: &Value, origin: MutationOrigin) -> Result<Value, String> {
    origin.validate("mirror cancel")?;
    validate_instance(s, request)?;
    let _admission = s.background.edit_guard()?;
    let id = text(request, "operationId", 120)?;
    let doc = crate::document(s)?;
    if doc["revision"] != request["expectedRevision"] {
        return Err("The review changed. Inspect mirror status before canceling.".into());
    }
    let live = s.mirrors.0.lock().map_err(store::error)?;
    if live.operation.as_deref() != Some(id) {
        return Err("This is not the current mirror comparison.".into());
    }
    let active = live.worker.as_ref().is_some_and(|job| !job.is_finished());
    if active {
        if let Some(cancel) = &live.cancel {
            cancel.store(true, Ordering::SeqCst);
        }
    }
    Ok(json!({"operationId":id,"cancelRequested":active,"running":active}))
}

pub(crate) fn prepare_confirmation(s: &AppState, request: &Value) -> Result<Value, String> {
    let _observation = s.background.observe()?;
    let instance = validate_instance(s, request)?;
    let id = text(request, "operationId", 120)?;
    let revision = request["expectedRevision"].as_u64().ok_or("Missing expectedRevision")?;
    let doc = crate::document(s)?;
    if doc["revision"] != revision {
        return Err("The saved review changed. Inspect it before preparing confirmation.".into());
    }
    let binding_id = doc["mirrorOperations"][id]["bindingId"].as_str().ok_or("Comparison not found")?;
    let binding = current_binding(&doc, binding_id)?;
    let exceptions = request["exceptions"].as_array().filter(|items| items.len() <= 1_000)
        .ok_or("Review a bounded list of explicit exceptions")?.iter()
        .map(|item| item.as_str().map(str::to_owned).ok_or("Exceptions must be relative paths"))
        .collect::<Result<Vec<_>, _>>()?;
    let directory = evidence_directory(s);
    let outcome = read_outcome(&directory, id)?;
    let receipt: mirror::Receipt = serde_json::from_value(outcome["receipt"].clone()).map_err(store::error)?;
    let (window, cancel) = {
        let live = s.mirrors.0.lock().map_err(store::error)?;
        let window = live.verified.as_ref().filter(|window| window.instance_id == instance
            && window.binding_id == binding_id && window.manifest_digest == receipt.manifest_digest)
            .ok_or("This comparison is historical or its monitoring is unavailable. Run a fresh comparison.")?;
        let cancel = live.cancel.as_ref().cloned().ok_or("The comparison instance stopped.")?;
        (window.clone(), cancel)
    };
    s.mirrors.ensure_window(&window)?;
    let membership = mirror::prepare_membership(&directory, &binding, &receipt, &exceptions)?;
    mirror::revalidate_membership(&directory, &binding, &receipt, environment(s)?.borrowed(),
        &membership, &cancel)?;
    if crate::document(s)?["revision"] != revision {
        return Err("The saved review changed during validation. Prepare again.".into());
    }
    s.mirrors.ensure_window(&window)?;
    let response = json!({"revision":revision,"membership":membership,"operationId":id,"validForMs":60_000});
    s.mirrors.0.lock().map_err(store::error)?.prepared = Some(PreparedConfirmation {
        revision, membership, window, prepared_at: Instant::now(), binding,
    });
    Ok(response)
}

pub(crate) fn apply_confirmation(s: &AppState, doc: &mut Value, op: &Value,
    operation_id: &str, provenance: &Value) -> Result<ConfirmationGuard, String> {
    if op["confirmed"] != true {
        return Err("Review the frozen membership and confirm the manual backup explicitly.".into());
    }
    let digest = digest(op, "membershipDigest")?;
    let live = s.mirrors.0.lock().map_err(store::error)?;
    let prepared = live.prepared.as_ref().filter(|item| {
        item.membership.membership_digest == digest && doc["revision"] == item.revision
            && item.prepared_at.elapsed() <= Duration::from_secs(60)
            && live.verified.as_ref() == Some(&item.window)
            && live.watcher.as_ref().and_then(position)
                == Some((item.window.source_sequence, item.window.mirror_sequence))
    }).ok_or("The prepared membership changed or expired. Review it again.")?;
    if doc.get("mirrorConfirmations").is_none() {
        doc["mirrorConfirmations"] = json!({});
    }
    let records = doc["mirrorConfirmations"].as_object_mut().ok_or("Invalid saved confirmation records")?;
    records.insert(operation_id.to_owned(), json!({"operationId":operation_id,
        "membership":prepared.membership,"confirmedAt":store::now(),
        "provenance":provenance,"evidenceWindow":prepared.window,
        "scope":"Exact observed members only; no source deletion, restoration proof, or inherited confirmation"}));
    doc["schemaVersion"] = json!(2);
    Ok(ConfirmationGuard { binding: prepared.binding.clone(), window: prepared.window.clone() })
}

pub(crate) fn evidence_page(s: &AppState, request: &Value) -> Result<Value, String> {
    let _observation = s.background.observe()?;
    let id = text(request, "operationId", 120)?;
    let offset = request["offset"].as_u64().ok_or("Invalid evidence offset")?;
    let limit = request["limit"].as_u64().filter(|size| (1..=32).contains(size))
        .ok_or("Review between 1 and 32 evidence entries at a time")?;
    let problems_only = request["problemsOnly"].as_bool().ok_or("Choose the evidence filter")?;
    let doc = crate::document(s)?;
    let binding_id = doc["mirrorOperations"][id]["bindingId"].as_str().ok_or("Comparison not found")?;
    let binding = current_binding(&doc, binding_id)?;
    let directory = evidence_directory(s);
    let outcome = read_outcome(&directory, id)?;
    let receipt: mirror::Receipt = serde_json::from_value(outcome["receipt"].clone()).map_err(store::error)?;
    let page = mirror::read_evidence_page(&directory, &binding, &receipt, offset, limit as usize, problems_only)?;
    Ok(json!({"revision":doc["revision"],"receipt":receipt,"page":page}))
}

pub(crate) fn read_confirmation(s: &AppState, request: &Value) -> Result<Value, String> {
    let id = text(request, "operationId", 200)?;
    let offset = request["offset"].as_u64().filter(|value| *value <= 1_000)
        .ok_or("Invalid confirmation offset")? as usize;
    let limit = request["limit"].as_u64().filter(|value| (1..=32).contains(value))
        .ok_or("Review between 1 and 32 exceptions at a time")? as usize;
    let doc = crate::document(s)?;
    let record = doc["mirrorConfirmations"].get(id).ok_or("The saved confirmation does not exist.")?;
    let membership = &record["membership"];
    let items = membership["exceptions"].as_array().ok_or("Invalid confirmation exceptions")?;
    let exceptions: Vec<_> = items.iter().skip(offset).take(limit).collect();
    let next = offset.saturating_add(exceptions.len());
    Ok(json!({"revision":doc["revision"],"confirmation":{"operationId":id,"confirmedAt":record["confirmedAt"],
        "membership":{"bindingId":membership["bindingId"],"manifestDigest":membership["manifestDigest"],
            "membershipDigest":membership["membershipDigest"],"coveredFiles":membership["coveredFiles"],
            "coveredDirectories":membership["coveredDirectories"]},"historical":true},
        "exceptions":exceptions,"total":items.len(),"nextOffset":(next<items.len()).then_some(next)}))
}

#[tauri::command]
pub(crate) async fn preview_mirror_binding(state: tauri::State<'_, Shared>, request: Value) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || preview(&s, &request)).await.map_err(store::error)?
}

#[tauri::command]
pub(crate) async fn mirror_comparison_status(state: tauri::State<'_, Shared>, request: Value) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || status(&s, &request)).await.map_err(store::error)?
}

#[tauri::command]
pub(crate) async fn start_mirror_comparison(state: tauri::State<'_, Shared>, request: Value) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || start(&s, &request, MutationOrigin::ManualUi)).await.map_err(store::error)?
}

#[tauri::command]
pub(crate) async fn cancel_mirror_comparison(state: tauri::State<'_, Shared>, request: Value) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || cancel(&s, &request, MutationOrigin::ManualUi)).await.map_err(store::error)?
}

#[tauri::command]
pub(crate) async fn prepare_mirror_confirmation(state: tauri::State<'_, Shared>, request: Value) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || prepare_confirmation(&s, &request)).await.map_err(store::error)?
}

#[tauri::command]
pub(crate) async fn read_mirror_evidence(state: tauri::State<'_, Shared>, request: Value) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || evidence_page(&s, &request)).await.map_err(store::error)?
}

#[tauri::command]
pub(crate) async fn read_mirror_confirmation(state: tauri::State<'_, Shared>, request: Value) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || read_confirmation(&s, &request)).await.map_err(store::error)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn wait(mut check: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(8);
        while !check() {
            assert!(Instant::now() < until, "mirror fixture did not settle");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn mirror_compare_and_confirm_are_dated_read_only_and_require_current_membership() {
        let mut s = crate::tests::fixture();
        let source = PathBuf::from(&s.volumes[0].root).join("Work");
        let mirror_root = s.data_path.parent().unwrap().join("mirror");
        let mirror = mirror_root.join("Work");
        fs::create_dir_all(mirror.join("Child")).unwrap();
        fs::create_dir_all(&s.runtime).unwrap();
        let source_file = source.join("notes.txt");
        let mirror_file = mirror.join("notes.txt");
        fs::write(&source_file, b"fixture source").unwrap();
        fs::write(&mirror_file, b"fixture source").unwrap();
        let proposed = preview(&s, &json!({"source":source,"mirror":mirror})).unwrap();
        assert_ne!(proposed["source"]["volumeId"], proposed["mirror"]["volumeId"]);
        let initial = crate::document(&s).unwrap();
        let bound = crate::run_mutation(&s, initial["revision"].as_u64().unwrap(), "bind-fixture".into(),
            json!({"type":"mirrorBind","source":source,"mirror":mirror,"expectedBindingId":proposed["bindingId"],
                "confirmed":true})).unwrap();
        let instance = s.gateway.lock().unwrap()["instanceId"].clone();
        let request = json!({"operationId":"verify-fixture","bindingId":proposed["bindingId"],
            "expectedInstanceId":instance,"expectedRevision":bound["revision"],"confirmed":true});
        let accepted = start(&s, &request, MutationOrigin::ManualUi).unwrap();
        assert_eq!(accepted["replayed"], false);
        assert_eq!(start(&s, &request, MutationOrigin::ManualUi).unwrap()["replayed"], true);
        wait(|| !s.mirrors.active().unwrap());
        let proof = status(&s, &json!({})).unwrap();
        let comparison = &proof["comparison"];
        assert_eq!(comparison["outcome"]["receipt"]["sourceMatches"], true, "{comparison}");
        assert_eq!(comparison["freshness"], "monitored", "{comparison}");
        let records = evidence_page(&s, &json!({"operationId":"verify-fixture","offset":0,
            "limit":8,"problemsOnly":false})).unwrap();
        assert!(records["page"]["entries"].as_array().unwrap().len() >= 2);
        let prepare = json!({"operationId":"verify-fixture","expectedInstanceId":instance,
            "expectedRevision":proof["revision"],"exceptions":[]});
        let prepared = prepare_confirmation(&s, &prepare).unwrap();
        let confirm = json!({"type":"mirrorConfirm","membershipDigest":prepared["membership"]["membershipDigest"],
            "confirmed":true});
        let confirmed = crate::run_mutation(&s, proof["revision"].as_u64().unwrap(), "confirm-fixture".into(),
            confirm.clone()).unwrap();
        assert_eq!(confirmed["mirrorConfirmations"]["confirm-fixture"]["membership"]["coveredFiles"], 1);
        assert_eq!(confirmed["decisions"], initial["decisions"]);
        assert_eq!(crate::run_mutation(&s, 0, "confirm-fixture".into(), confirm).unwrap(), confirmed);
        assert_eq!(fs::read(&source_file).unwrap(), b"fixture source");
        assert_eq!(fs::read(&mirror_file).unwrap(), b"fixture source");

        fs::write(source.join("new-original.txt"), b"new fixture member").unwrap();
        wait(|| status(&s, &json!({})).unwrap()["comparison"]["freshness"] != "monitored");
        let mut updated_prepare = prepare;
        updated_prepare["expectedRevision"] = confirmed["revision"].clone();
        assert!(prepare_confirmation(&s, &updated_prepare).is_err());
        let exit = crate::background::begin_exit(&s).unwrap();
        crate::background::drain_exit(&s, exit["ticket"].as_u64().unwrap(), Duration::from_secs(3)).unwrap();
        s.mirrors = Runtime::default();
        assert_eq!(status(&s, &json!({})).unwrap()["comparison"]["freshness"], "historical-or-unknown");
    }
}
