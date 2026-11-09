#![windows_subsystem = "windows"]
mod assistance;
mod adaptive_review;
mod background;
mod audit;
mod bulk_mark;
mod copy_control;
mod copy_effect_ledger;
mod copy_queue;
mod filesystem;
mod gateway;
mod index;
mod manual_backup;
mod mirror;
mod mirror_service;
mod recovery_control;
mod review;
mod rollup;
mod store;
mod ui_control;
mod watcher;

use filesystem::{Entry, Volume};
use index::Index;
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    sync::{Arc, Mutex},
};
use store::{error, now, Store};
use tauri::State;

struct AppState {
    background: background::Control,
    store: Mutex<Option<Store>>,
    startup_problem: Option<String>,
    volumes: Vec<Volume>,
    index: Index,
    audit: Arc<audit::Audit>,
    watcher: Mutex<Option<watcher::WatchHandle>>,
    watcher_fault: Arc<Mutex<Option<String>>>,
    exclusions: Vec<String>,
    data_path: PathBuf,
    fixture: bool,
    runtime: PathBuf,
    gateway: Mutex<Value>,
    gateway_server: Mutex<Option<gateway::GatewayHandle>>,
    review: review::ReviewBridge,
    copy_queue: Arc<copy_queue::CopyQueue>,
    copy_control: Mutex<()>,
    copy_inspection_only: bool,
    assistance: assistance::Cache,
    adaptive: adaptive_review::Runtime,
    mirrors: mirror_service::Runtime,
}
type Shared = Arc<AppState>;

fn ensure_gateway(state: &Shared) {
    let available = state
        .store
        .lock()
        .ok()
        .is_some_and(|lock| lock.is_some());
    if !available {
        if let Ok(mut public) = state.gateway.lock() {
            *public = json!({"state":"unavailable","problem":"The local connection requires the data-file owner. A competing instance cannot replace its connection."});
        }
        return;
    }
    let Ok(mut running) = state.gateway_server.lock() else {
        return;
    };
    if running.is_some() {
        return;
    }
    match gateway::start(state, &state.runtime) {
        Ok((handle, public)) => {
            *running = Some(handle);
            if let Ok(mut status) = state.gateway.lock() {
                *status = public;
            }
        }
        Err(problem) => {
            if let Ok(mut status) = state.gateway.lock() {
                *status = json!({"state":"error","problem":format!("Local connection could not start: {problem}. Folder review is still available.")});
            }
        }
    }
}

fn ensure_watcher(state: &AppState) {
    if healthy_owner(state).is_err() {
        return;
    }
    let Ok(mut running) = state.watcher.lock() else {
        return;
    };
    if running.is_some() {
        return;
    }
    let audit = state.audit.clone();
    let fault = state.watcher_fault.clone();
    let callback = Arc::new(move |event: watcher::WatchEvent| {
        let result = match event {
            watcher::WatchEvent::Changed {
                root: _,
                path,
                kind,
            } => audit.record_watch_change(&path, kind.as_str()),
            watcher::WatchEvent::RescanRequired { root, reason }
            | watcher::WatchEvent::Unavailable { root, reason } => {
                audit.record_watch_gap(&root, &reason)
            }
        };
        if let Err(problem) = result {
            if let Ok(mut retained) = fault.lock() {
                *retained = Some(format!("Some directory-change observations could not be recorded: {problem}. A fresh checksum pass is required."));
            }
        }
    });
    let roots: Vec<String> = state
        .volumes
        .iter()
        .filter(|volume| volume.available)
        .map(|volume| volume.root.clone())
        .collect();
    let prior_observations = state.index.has_observations().unwrap_or(true) || state
        .audit
        .status()
        .is_ok_and(|status| status["phase"] != "idle")
        || document(state).is_ok_and(|doc| {
            doc["assistance"]["scope"].is_string() || doc["decisions"].as_object().is_some_and(|decisions| {
                decisions
                    .values()
                    .any(|decision| matches!(decision["status"].as_str(), Some("on-external" | "manual-confirmed")))
            })
        });
    match watcher::start(roots.clone(), state.exclusions.clone(), callback) {
        Ok(handle) => {
            *running = Some(handle);
            if prior_observations {
                for root in roots {
                    if let Err(problem) = state.audit.record_watch_gap(&root, "The app was closed or directory monitoring was reconnected. Changes during that interval were not observed; run a fresh count and checksum comparison to restore source coverage.") {
                        if let Ok(mut fault) = state.watcher_fault.lock() { *fault = Some(problem); }
                    }
                }
            }
        }
        Err(problem) => {
            if state.audit.record_watch_gap("", &problem).is_err() {
                if let Ok(mut fault) = state.watcher_fault.lock() {
                    *fault = Some(problem);
                }
            }
        }
    }
}

fn document(s: &AppState) -> Result<Value, String> {
    s.store
        .lock()
        .map_err(error)?
        .as_ref()
        .map(|s| s.doc.clone())
        .ok_or_else(|| {
            s.startup_problem
                .clone()
                .unwrap_or("Data unavailable".into())
        })
}

fn recorded_effective(s: &AppState, doc: &Value, e: &Entry) -> Value {
    recorded_effective_cached(s, doc, e, &manual_backup::ConfirmationCache::default())
}

fn recorded_effective_cached(
    s: &AppState,
    doc: &Value,
    e: &Entry,
    confirmations: &manual_backup::ConfirmationCache,
) -> Value {
    let Ok((volume, relative, key)) = filesystem::locate(&e.path, &s.volumes) else {
        return json!({"status":"attention","own":false,"inheritedFrom":null,"stale":false,"mixed":0,"originalStatus":null});
    };
    let own = &doc["decisions"][&key];
    let is_own = own.is_object() && own["noteOnly"] != true;
    let mut decision = if is_own { own } else { &Value::Null };
    let mut inherited = None;
    if !is_own {
        let mut rel = relative.clone();
        while !rel.is_empty() {
            rel = rel
                .rsplit_once('\\')
                .map(|(p, _)| p.to_string())
                .unwrap_or_default();
            let Some((volume, _)) = key.split_once('|') else {
                break;
            };
            let candidate = &doc["decisions"][format!("{volume}|{}", rel.to_lowercase())];
            if candidate.is_object() && candidate["noteOnly"] != true {
                decision = candidate;
                inherited = candidate["path"].as_str().map(String::from);
                break;
            }
        }
    }
    let original = decision["status"].as_str().unwrap_or("attention");
    let mut status = original;
    let mut stale = false;
    let verified_manual_backup = original == "manual-pending" && is_own
        && matches!(e.kind.as_str(), "folder" | "file") && e.problem.is_none()
        && s.mirrors.verifies_manual_scope(doc, &e.path, volume, s.fixture);
    if verified_manual_backup {
        status = "manual-confirmed";
    }
    if original == "on-external" {
        if inherited.is_some() {
            status = "attention";
        } else {
            stale = decision["snapshot"]["fingerprint"]
                .as_str()
                .is_some_and(|f| f != e.fingerprint)
                || decision["decidedAt"].as_str().is_some_and(|date| {
                    s.index.changed_below(&e.path, date).unwrap_or(true)
                        || s.audit.changed_below(&e.path, date).unwrap_or(true)
                        || s.watcher_fault
                            .lock()
                            .map(|fault| fault.is_some())
                            .unwrap_or(true)
                });
            if stale {
                status = "attention";
            }
        }
    }
    if original == "manual-confirmed" {
        if inherited.is_some() {
            status = "attention";
        } else if !confirmations.current(s, decision, volume) {
            stale = true;
            status = "attention";
        }
    }
    let prefix = if relative.is_empty() {
        String::new()
    } else {
        format!("{}\\", relative.to_lowercase())
    };
    let mixed = (e.kind == "folder").then(|| doc["decisions"].as_object()).flatten()
        .map(|m| {
            m.values()
                .filter(|d| {
                    d["volumeId"] == volume.id
                        && d["noteOnly"] != true
                        && d["relativePath"].as_str().is_some_and(|p| {
                            !p.eq_ignore_ascii_case(&relative)
                                && p.to_lowercase().starts_with(&prefix)
                        })
                        && d["status"].as_str() != Some(original)
                })
                .count()
        })
        .unwrap_or(0);
    json!({"status":status,"own":is_own,"inheritedFrom":inherited,"stale":stale,"mixed":mixed,
        "originalStatus":if decision.is_object(){Some(original)}else{None},
        "manualEvidence":verified_manual_backup.then_some("mirror-verified")})
}

fn effective(s: &AppState, doc: &Value, entry: &Entry) -> Value {
    let rollups = rollup::Context::new(s, doc).ok();
    effective_cached(s, doc, entry, &manual_backup::ConfirmationCache::default(), rollups.as_ref())
}

fn effective_cached(
    s: &AppState,
    doc: &Value,
    entry: &Entry,
    confirmations: &manual_backup::ConfirmationCache,
    rollups: Option<&rollup::Context<'_>>,
) -> Value {
    let mut decision = recorded_effective_cached(s, doc, entry, confirmations);
    if let Some(summary) = rollups.and_then(|context| context.derive(entry, &decision, |child| {
        recorded_effective_cached(s, doc, child, confirmations)
    })) {
        if summary["kind"] == "unanimous" {
            decision["status"] = summary["status"].clone();
            decision["mixed"] = json!(0);
        }
        decision["rollup"] = summary;
    }
    decision
}

fn review_status(decision: &Value) -> &str {
    if decision["rollup"]["kind"] == "mixed" {
        "mixed"
    } else {
        decision["status"].as_str().unwrap_or("attention")
    }
}

fn matches_review_filter(decision: &Value, filter: &str) -> bool {
    match filter {
        "all" => true,
        "mixed" => {
            review_status(decision) == "mixed" || decision["mixed"].as_u64().unwrap_or(0) > 0
        }
        filter => review_status(decision) == filter,
    }
}

fn boot(s: &AppState) -> Value {
    let mut result = match s.store.lock() {
        Ok(lock) => match lock.as_ref() {
            Some(store) => {
                json!({"document":store.doc,"volumes":s.volumes,"dataPath":s.data_path,"fixture":s.fixture,"problem":store.problem,"recoveryAvailable":store.can_recover()})
            }
            None => {
                json!({"document":store::fresh(&s.volumes[0].root),"volumes":s.volumes,"dataPath":s.data_path,"fixture":s.fixture,"problem":s.startup_problem,"recoveryAvailable":false})
            }
        },
        Err(e) => json!({"problem":error(e)}),
    };
    result["gateway"] = s
        .gateway
        .lock()
        .map(|g| g.clone())
        .unwrap_or_else(|_| json!({"state":"error","problem":"Connection status unavailable"}));
    result
}
#[tauri::command]
fn get_boot(state: State<'_, Shared>) -> Value {
    boot(state.inner())
}

#[tauri::command]
fn report_review_surface(state: State<'_, Shared>, surface: Value) -> Result<(), String> {
    state.review.report(surface, &state.volumes)
}

#[tauri::command]
fn poll_review_activity(state: State<'_, Shared>, after: u64) -> Result<Value, String> {
    state.review.poll(after)
}

#[tauri::command]
async fn plan_review_reveal(state: State<'_, Shared>, path: String) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _observation = s.background.observe()?;
        review::plan(&s, path)
    })
        .await
        .map_err(error)?
}

#[tauri::command]
async fn get_review_entry(state: State<'_, Shared>, path: String) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _observation = s.background.observe()?;
        review::entry(&s, path)
    })
        .await
        .map_err(error)?
}

fn list_folder_service(
    s: &AppState,
    path: String,
    refresh: bool,
    search: String,
    filter: String,
    page: usize,
    sample_names: bool,
) -> Result<Value, String> {
    let _observation = s.background.observe()?;
    let path = filesystem::normalize(&path)?;
    filesystem::safe(&path, &s.volumes, false).inspect_err(|e| {
        let _ = s.index.record_gap(&path, e);
    })?;
    if refresh {
        s.index.refresh(&path, &s.volumes, None, true)?;
    }
    let doc = document(&s)?;
    let mut entries = Vec::new();
    const NAME_SAMPLE_LIMIT: usize = 256;
    let mut name_sample = Vec::new();
    let mut name_stride = 1;
    let mut total = 0usize;
    let mut attention = 0usize;
    let mut counts = serde_json::Map::new();
    let confirmations = manual_backup::ConfirmationCache::default();
    let rollups = rollup::Context::new(s, &doc).ok();
    s.index.each_entry(&path, &search, |entry| {
        let result = effective_cached(&s, &doc, &entry, &confirmations, rollups.as_ref());
        let status = review_status(&result);
        let n = counts.get(status).and_then(Value::as_u64).unwrap_or(0) + 1;
        counts.insert(status.into(), json!(n));
        if status == "attention" {
            attention += 1;
        }
        if !matches_review_filter(&result, &filter) {
            return true;
        }
        if sample_names && total % name_stride == 0 {
            if name_sample.len() == NAME_SAMPLE_LIMIT {
                // Keep the sample evenly spaced over the full sorted result.
                let mut index = 0;
                name_sample.retain(|_| {
                    let keep = index % 2 == 0;
                    index += 1;
                    keep
                });
                name_stride *= 2;
            }
            name_sample.push(entry.name.clone());
        }
        if total >= page.saturating_mul(100) && entries.len() < 100 {
            let mut value = serde_json::to_value(entry).unwrap();
            value["effective"] = result;
            entries.push(value);
        }
        total += 1;
        true
    })?;
    let paths: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry["path"].as_str().map(String::from))
        .collect();
    let audit_stats = s.audit.stats(&paths)?;
    for entry in &mut entries {
        if let Some(path) = entry["path"].as_str().map(String::from) {
            entry["audit"] = audit_stats[&path].clone();
        }
    }
    let info = s.index.directory_info(&path)?;
    let mut listing = json!({"path":path,"entries":entries,"total":total,"attention":attention,"counts":counts,
        "observedAt":info.as_ref().and_then(|i|i["listingObservedAt"].as_str())});
    if sample_names && total > entries.len() {
        listing["nameFitSample"] = json!(name_sample);
    }
    Ok(listing)
}
#[tauri::command]
async fn list_folder(
    state: State<'_, Shared>,
    path: String,
    refresh: bool,
    search: String,
    filter: String,
    page: usize,
) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        list_folder_service(&s, path, refresh, search, filter, page, true)
    })
    .await
    .map_err(error)?
}

fn details_service(s: &AppState, path: String) -> Result<Value, String> {
    let _observation = s.background.observe()?;
    let path = filesystem::normalize(&path)?;
    let (volume, relative, key) = filesystem::identify(&path, &s.volumes)?;
    let doc = document(&s)?;
    let entry = match filesystem::guard_path(&path, &s.volumes, true) {
        Ok((p, _guards)) => filesystem::inspect(&p),
        Err(e) => Entry {
            path: path.clone(),
            name: path.clone(),
            kind: "unavailable".into(),
            size: 0,
            modified: String::new(),
            fingerprint: String::new(),
            problem: Some(e),
        },
    };
    let prefix = if relative.is_empty() {
        String::new()
    } else {
        format!("{}\\", relative.to_lowercase())
    };
    let overrides: Vec<_> = doc["decisions"]
        .as_object()
        .unwrap()
        .values()
        .filter(|d| {
            d["volumeId"] == volume.id
                && d["noteOnly"] != true
                && d["relativePath"].as_str().is_some_and(|p| {
                    !p.eq_ignore_ascii_case(&relative) && p.to_lowercase().starts_with(&prefix)
                })
        })
        .take(200)
        .cloned()
        .collect();
    let history: Vec<_> = doc["history"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        // Drive letters can be reused. History follows the stable volume-relative identity;
        // legacy events without a key remain in the archive rather than being guessed onto a disk.
        .filter_map(|event| bulk_mark::history_for(event, &key))
        .take(30)
        .collect();
    Ok::<_, String>(
        json!({"path":path,"effective":effective(&s,&doc,&entry),"decision":doc["decisions"][key],"overrides":overrides,"history":history,"exists":entry.problem.is_none(),"inventory":s.index.directory_info(&path)?,"audit":s.audit.stats(&[path.clone()])?[&path]}),
    )
}
#[tauri::command]
async fn get_details(state: State<'_, Shared>, path: String) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || details_service(&s, path))
        .await
        .map_err(error)?
}

#[derive(Clone)]
enum MutationOrigin {
    ManualUi,
    Agent { basis: String, reason: String },
}
impl MutationOrigin {
    fn payload_hash(&self, operation: &Value) -> Result<Option<String>, String> {
        let Self::Agent { basis, reason } = self else {
            return Ok(None);
        };
        let request = json!({"operation":operation,"origin":{"kind":"agent-mcp","actor":"MCP agent","basis":basis,"reason":reason}});
        Ok(Some(store::hash(
            &serde_json::to_vec(&canonical_json(&request)).map_err(error)?,
        )))
    }
    fn provenance(&self, fixture: bool) -> Value {
        match self {
            Self::ManualUi => {
                json!({"kind":"manual-ui","actor":if fixture{"fixture-test"}else{"local user"},"at":now()})
            }
            Self::Agent { basis, reason } => {
                json!({"kind":"agent-mcp","actor":"MCP agent","basis":basis,"reason":reason,"at":now()})
            }
        }
    }
    fn validate(&self, operation: &str) -> Result<(), String> {
        if let Self::Agent { basis, reason } = self {
            if operation == "view" {
                return Ok(());
            }
            if !matches!(basis.as_str(), "user_instruction" | "assistant_suggestion")
                || reason.trim().is_empty()
                || reason.len() > 8000
            {
                return Err("Agent changes require a nonempty reason and basis user_instruction or assistant_suggestion".into());
            }
            if basis == "assistant_suggestion" && !matches!(operation, "classify" | "adaptiveProposal") {
                return Err("Assistant suggestions may submit a separate recommendation or model question only.".into());
            }
        }
        Ok(())
    }
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let mut names: Vec<_> = fields.keys().collect();
            names.sort();
            Value::Object(
                names
                    .into_iter()
                    .map(|name| (name.clone(), canonical_json(&fields[name])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

fn operation_id_used(doc: &Value, id: &str) -> bool {
    ["agentOperations","groupOperations","copyOperations","recoveryOperations",
        "mirrorOperations","mirrorEditOperations","copyReconciliationOperations",
        "reconciledCopyOperations"].iter()
        .any(|field| doc[*field].get(id).is_some())
        || doc["recentOperations"].as_array().is_some_and(|ids| ids.iter().any(|value| value == id))
        || doc["history"].as_array().is_some_and(|history| history.iter().any(|event| event["id"] == id))
}

fn run_mutation(
    s: &AppState,
    expected_revision: u64,
    operation_id: String,
    op: Value,
) -> Result<Value, String> {
    run_mutation_as(
        s,
        expected_revision,
        operation_id,
        op,
        MutationOrigin::ManualUi,
    )
}

fn apply_view_patch(
    s: &AppState,
    view: &mut Value,
    patch: &serde_json::Map<String, Value>,
) -> Result<(), String> {
    for (key, value) in patch {
        match key.as_str() {
            "path" => {
                filesystem::identify(value.as_str().ok_or("Invalid path")?, &s.volumes)?;
            }
            "selectedPath" | "revealPath" => {
                if !value.is_null() {
                    filesystem::identify(value.as_str().ok_or("Invalid selection")?, &s.volumes)?;
                }
            }
            "search" if value.as_str().is_some_and(|search| search.len() <= 4_000) => {}
            "filter" if value.as_str().is_some_and(store::valid_filter) => {}
            "page" if value.as_u64().is_some_and(|page| page <= 10_000_000) => {}
            "scrollTop" | "previewScrollLeft" if store::valid_nonnegative_number(value) => {}
            "recentPaths"
                if value.as_array().is_some_and(|paths| {
                    paths.len() <= 30 && paths.iter().all(Value::is_string)
                }) => {}
            "previewColumns" if store::valid_preview_columns(value) => {
                for column in value.as_array().unwrap() {
                    filesystem::identify(column["path"].as_str().unwrap(), &s.volumes)?;
                }
            }
            "browserColumnWidth" if value.is_null() || store::valid_column_width(value) => {}
            "toolbarHeight" if store::valid_toolbar_height(value) => {}
            "scanDetailsHeight" if store::valid_scan_details_height(value) => {}
            "layout" if store::valid_review_layout(value) => {}
            _ => return Err(format!("Invalid or unsupported review preference: {key}")),
        }
    }
    if !patch.contains_key("revealPath") && review::pin_context_changed(view, patch) {
        view["revealPath"] = Value::Null;
    }
    if let (Some(old), Some(new)) = (
        view["path"].as_str(),
        patch.get("path").and_then(Value::as_str),
    ) {
        if !review::same_path(old, new) && !patch.contains_key("previewColumns") {
            view["previewColumns"] = json!([]);
            view["previewScrollLeft"] = json!(0);
        }
    }
    for (key, value) in patch {
        if key == "browserColumnWidth" && value.is_null() {
            if let Some(fields) = view.as_object_mut() {
                fields.remove(key);
            }
        } else {
            view[key] = value.clone();
        }
    }
    let preview_depth = view["previewColumns"].as_array().map_or(0, Vec::len);
    if view["layout"]["fillDepth"].as_u64().is_some_and(|n| n > preview_depth as u64) {
        view["layout"]["fillDepth"] = Value::Null;
    }
    Ok(())
}

fn decision_record(
    before: &Value,
    path: &str,
    volume_id: &str,
    relative: &str,
    provenance: &Value,
) -> Value {
    if before.is_object() {
        before.clone()
    } else {
        json!({"path":path,"volumeId":volume_id,"relativePath":relative,"status":"attention",
            "noteOnly":true,"notes":"","createdAt":now(),"provenance":provenance})
    }
}

fn update_decision_snapshot(
    s: &AppState,
    decision: &mut Value,
    path: &str,
    entry: &Entry,
    status: &str,
    provenance: &Value,
) {
    decision["status"] = json!(status);
    decision["provenance"] = provenance.clone();
    decision["decidedAt"] = json!(now());
    decision.as_object_mut().expect("prepared decision").remove("noteOnly");
    decision["snapshot"] = json!({"fingerprint":entry.fingerprint,
        "directorySignature":s.index.signature(path),"observedAt":now(),
        "scope":"metadata only; no content/hash verification; descendants not exhaustively checked"});
    decision["verification"] = match status {
        "manual-confirmed" => json!({"kind":"manual-backup-assertion","assertedAt":now(),
            "sourcePath":path,"destination":null,
            "scope":"User-confirmed manual backup assertion for the selected path only; children require their own review",
            "provenance":provenance,
            "limitations":"User assertion. Destination content, checksums and restore have not been verified by this mark."}),
        "on-external" => json!({"kind":"manual-assertion","assertedAt":now(),"sourcePath":path,
            "destination":"F:\\ (exact destination not recorded)",
            "scope":"User assertion for selected path; child exceptions retained","exclusions":"Not specified",
            "provenance":provenance,
            "limitations":"Not verified by this app; no copy, full hash audit, snapshot or restore test performed"}),
        _ => Value::Null,
    };
}

fn run_mutation_as(
    s: &AppState,
    expected_revision: u64,
    operation_id: String,
    op: Value,
    origin: MutationOrigin,
) -> Result<Value, String> {
    let _admission = s.background.edit_guard()?;
    let operation_type = op["type"].as_str().ok_or("Missing operation type")?;
    origin.validate(operation_type)?;
    if operation_type == "adaptiveProposal"
        && !matches!(&origin,MutationOrigin::Agent{basis,..} if basis=="assistant_suggestion") {
        return Err("An external model proposal must use the proposal-only MCP route.".into());
    }
    if operation_id.len() > 200 || operation_id.is_empty() {
        return Err("Invalid operation identifier".into());
    }
    if operation_type == "review" && op["action"] == "reveal" {
        let prepare = s
            .store
            .lock()
            .map_err(error)?
            .as_ref()
            .is_some_and(|store| {
                store.problem.is_none()
                    && store.revision() == expected_revision
                    && store.doc["agentOperations"].get(&operation_id).is_none()
                    && store.doc["groupOperations"].get(&operation_id).is_none()
                    && !store.doc["recentOperations"]
                        .as_array()
                        .is_some_and(|ids| ids.iter().any(|id| id == &operation_id))
            });
        if prepare {
            review::prepare_reveal(s, &op["path"])?;
        }
    }
    let mut guard = s.store.lock().map_err(error)?;
    let store = guard
        .as_mut()
        .ok_or("Data file is not writable in this instance")?;
    if ["copyOperations","recoveryOperations","mirrorOperations",
        "copyReconciliationOperations","reconciledCopyOperations"].iter()
        .any(|key| store.doc[*key].get(&operation_id).is_some()) {
        return Err("This operationId belongs to a durable job or recovery receipt and cannot be reused for an edit".into());
    }
    let agent_hash = origin.payload_hash(&op)?;
    let group_hash = if matches!(operation_type, "bulkClassify" | "undoGroup") {
        Some(match &agent_hash {
            Some(hash) => hash.clone(),
            None => store::hash(&serde_json::to_vec(&canonical_json(
                &json!({"origin":"manual-ui","operation":op})
            )).map_err(error)?),
        })
    } else {
        None
    };
    let mirror_hash = if matches!(operation_type, "mirrorBind" | "mirrorConfirm") {
        Some(match &agent_hash {
            Some(hash) => hash.clone(),
            None => store::hash(&serde_json::to_vec(&canonical_json(
                &json!({"origin":"manual-ui","operation":op})
            )).map_err(error)?),
        })
    } else { None };
    if let Some(receipt) = store.doc["mirrorEditOperations"].get(&operation_id) {
        if mirror_hash.as_deref() != receipt["payloadHash"].as_str() || mirror_hash.is_none() {
            return Err("This operation ID belongs to a different saved mirror edit.".into());
        }
        return Ok(store.doc.clone());
    }
    if mirror_hash.is_some() && operation_id_used(&store.doc, &operation_id) {
        return Err("This operation ID belongs to another saved action.".into());
    }
    if let Some(receipt) = store.doc["groupOperations"].get(&operation_id) {
        if group_hash.as_deref() != receipt["payloadHash"].as_str() || group_hash.is_none() {
            return Err("This operation ID belongs to a different grouped edit. No decisions were changed.".into());
        }
        return Ok(store.doc.clone());
    }
    if group_hash.is_some() && (store.doc["history"].as_array()
        .is_some_and(|events| events.iter().any(|event| event["id"] == operation_id))
        || store.doc["recentOperations"].as_array().is_some_and(|ids| ids.iter().any(|id| id == &operation_id)))
    {
        return Err("This operation ID already belongs to another saved edit.".into());
    }
    if let Some(payload_hash) = &agent_hash {
        if store
            .doc
            .get("agentOperations")
            .is_some_and(|receipts| !receipts.is_object())
        {
            return Err(
                "Saved agent operation receipts have an unsupported format; originals preserved"
                    .into(),
            );
        }
        if let Some(receipt) = store
            .doc
            .get("agentOperations")
            .and_then(|receipts| receipts.get(&operation_id))
        {
            if receipt["payloadHash"].as_str() != Some(payload_hash.as_str()) {
                return Err("This agent operationId was already used for a different payload or origin. Nothing was changed; reconcile the saved state before choosing a new operationId.".into());
            }
            return Ok(store.doc.clone());
        }
        let legacy_id = store.doc["recentOperations"]
            .as_array()
            .is_some_and(|ids| ids.iter().any(|id| id == &operation_id))
            || store.doc["history"]
                .as_array()
                .is_some_and(|history| history.iter().any(|event| event["id"] == operation_id));
        if legacy_id {
            return Err("This operationId already exists without a durable matching payload receipt. Replay was refused; inspect the saved state before continuing.".into());
        }
    } else if store
        .doc
        .get("agentOperations")
        .and_then(|receipts| receipts.get(&operation_id))
        .is_some()
    {
        return Err(
            "This operationId belongs to an agent receipt and cannot be reused by the interface"
                .into(),
        );
    }
    if store.doc["recentOperations"]
        .as_array()
        .is_some_and(|ids| ids.iter().any(|id| id == &operation_id))
    {
        return Ok(store.doc.clone());
    }
    if expected_revision != store.revision() {
        return Err(
            "The app has newer saved state. Reload before editing; your edit was not applied."
                .into(),
        );
    }
    let mut doc = store.doc.clone();
    let provenance = origin.provenance(s.fixture);
    let is_agent = matches!(&origin, MutationOrigin::Agent { .. });
    let mut activity = None;
    let mut classification_changed = false;
    let mut group_targets = Vec::new();
    let mut mirror_guard = None;
    if operation_type == "classify" && matches!(&origin,MutationOrigin::Agent{basis,..} if basis=="assistant_suggestion") {
        let path = filesystem::normalize(op["path"].as_str().ok_or("Missing suggestion path")?)?;
        let (volume, relative, key) = filesystem::identify(&path, &s.volumes)?;
        let status = op["status"]
            .as_str()
            .filter(|status| store::valid_status(status))
            .ok_or("Unknown classification")?;
        if doc.get("suggestions").is_none() {
            doc["suggestions"] = json!([]);
        }
        let suggestions = doc["suggestions"].as_array_mut().ok_or(
            "Existing suggestions have an unsupported format; preserved without overwrite",
        )?;
        suggestions.push(json!({"id":operation_id,"key":key,"path":path,"volumeId":volume.id,"relativePath":relative,"status":status,"reason":provenance["reason"],"basis":"assistant_suggestion","createdAt":now(),"state":"proposed","provenance":provenance}));
        doc["history"].as_array_mut().unwrap().push(json!({"id":operation_id,"key":key,"volumeId":volume.id,"relativePath":relative,"type":"suggest","path":path,"status":status,"at":now(),"provenance":provenance}));
    } else {
        match op["type"].as_str().ok_or("Missing operation type")? {
            "view" | "review" => {
                let patch = if operation_type == "review" {
                    review::operation_patch(s, &doc, &op)?
                } else {
                    op["patch"].clone()
                };
                apply_view_patch(
                    s,
                    &mut doc["view"],
                    patch.as_object().ok_or("Invalid view patch")?,
                )?;
                doc["viewProvenance"] = provenance.clone();
                if is_agent && operation_type == "review" && op["action"] == "reveal" {
                    activity = patch["revealPath"]
                        .as_str()
                        .map(|path| ("reveal", path.to_string()));
                }
            }
            "assist" => assistance::update(s, &mut doc, &op)?,
            "adaptiveControl" => adaptive_review::configure(s, &mut doc, &op)?,
            "inventoryPriority" => {
                let defer = op["deferMarked"].as_bool().ok_or("Choose an inventory priority.")?;
                doc["inventoryDeferMarked"] = json!(defer);
            }
            "adaptiveProposal" => adaptive_review::submit(s, &mut doc, &op, &provenance)?,
            "mirrorBind" => mirror_service::apply_binding(s, &mut doc, &op)?,
            "mirrorConfirm" => {
                mirror_guard = Some(mirror_service::apply_confirmation(
                    s, &mut doc, &op, &operation_id, &provenance,
                )?);
            }
            "bulkClassify" => {
                let prepared = bulk_mark::prepare(s, &op)?;
                let event = bulk_mark::group_event(
                    &doc, &prepared, &operation_id,
                    group_hash.as_deref().expect("grouped request digest"), &provenance,
                    |member, before, status| {
                        let target = &member.target;
                        let mut after = decision_record(before, &target.path, &target.volume_id,
                            &target.relative_path, &provenance);
                        update_decision_snapshot(s, &mut after, &target.path, &member.entry(), status, &provenance);
                        after["updatedAt"] = json!(now());
                        after["lastEditedBy"] = provenance.clone();
                        Ok(after)
                    },
                )?;
                if is_agent {
                    activity = prepared.targets.first().map(|first| ("mark", first.target.path.clone()));
                }
                bulk_mark::apply_group(&mut doc, event);
                group_targets = prepared.targets;
                classification_changed = true;
            }
            "undoGroup" => {
                let prepared = bulk_mark::prepare_undo(s, &doc, &op)?;
                bulk_mark::apply_undo(&mut doc, &prepared, &operation_id,
                    group_hash.as_deref().expect("grouped request digest"), &provenance)?;
                group_targets = prepared.targets;
                classification_changed = true;
            }
            "classify" | "note" | "assistAnswer" => {
                let question = if operation_type == "assistAnswer" {
                    Some(assistance::validate_question(s, &doc, &op)?)
                } else {
                    None
                };
                let path =
                    filesystem::normalize(op["path"].as_str().ok_or("Missing decision path")?)?;
                let (volume, relative, key) = filesystem::identify(&path, &s.volumes)?;
                let before = doc["decisions"][&key].clone();
                let mut after = decision_record(&before, &path, &volume.id, &relative, &provenance);
                if operation_type != "note" {
                    classification_changed = true;
                    let status = op["status"]
                        .as_str()
                        .filter(|s| store::valid_status(s))
                        .ok_or("Unknown classification")?;
                    if question.is_some() && status == "attention" {
                        return Err("Use Defer to leave this interview question unanswered.".into());
                    }
                    // A failed source check happens before this operation can save anything.
                    let reject_path = |problem: String| {
                        if operation_type == "classify" {
                            format!("Mark rejected before save: {problem}")
                        } else {
                            problem
                        }
                    };
                    let (checked, _guards) = filesystem::guard_path(&path, &s.volumes, question.is_none())
                        .map_err(reject_path)?;
                    let entry = filesystem::inspect(&checked);
                    if entry.problem.is_some()
                        || question.as_ref().is_some_and(|q| q["kind"] != entry.kind)
                    {
                        return Err(reject_path(entry.problem.unwrap_or_else(||
                            format!("{path} changed before it could be marked."))));
                    }
                    if is_agent {
                        activity = Some(("mark", path.clone()));
                    }
                    update_decision_snapshot(s, &mut after, &path, &entry, status, &provenance);
                } else {
                    let notes = op["notes"].as_str().ok_or("Invalid notes")?;
                    if notes.len() > 1_000_000 {
                        return Err("Notes exceed the supported size".into());
                    }
                    after["notes"] = json!(notes);
                    after["notesProvenance"] = provenance.clone();
                }
                after["updatedAt"] = json!(now());
                after["lastEditedBy"] = provenance.clone();
                doc["decisions"][&key] = after.clone();
                let mut event = json!({"id":operation_id,"type":if operation_type == "note" {"note"} else {"classify"},"path":path,"key":key,"at":now(),"before":before,"after":after,"provenance":provenance});
                if let Some(question) = question {
                    adaptive_review::record_answer(&mut doc, &question, op["status"].as_str().unwrap_or("answered"))?;
                    event["assistanceQuestion"] = question;
                }
                doc["history"].as_array_mut().unwrap().push(event);
            }
            "undo" => {
                let position = doc["history"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .rposition(|h| {
                        matches!(h["type"].as_str(), Some("classify" | "note" | "bulkClassify"))
                            && h["undoneAt"].is_null()
                    })
                    .ok_or("There is no classification or note change to undo")?;
                let original = doc["history"][position].clone();
                if original["type"] == "bulkClassify" {
                    return Err("The latest edit is a grouped mark. Review the group before using grouped Undo.".into());
                }
                classification_changed = original["type"] == "classify";
                let key = original["key"]
                    .as_str()
                    .ok_or("History entry cannot be undone")?;
                let before = doc["decisions"][key].clone();
                if original["before"].is_null() {
                    doc["decisions"].as_object_mut().unwrap().remove(key);
                } else {
                    doc["decisions"][key] = original["before"].clone();
                }
                doc["history"][position]["undoneAt"] = json!(now());
                doc["history"][position]["undoneBy"] = provenance.clone();
                doc["history"].as_array_mut().unwrap().push(json!({"id":operation_id,"type":"undo","path":original["path"],"key":key,"at":now(),"undoes":original["id"],"before":before,"after":original["before"],"provenance":provenance}));
            }
            _ => return Err("Unsupported operation".into()),
        }
    }
    if doc.get("volumes").is_none() {
        doc["volumes"] = json!({});
    }
    for v in &s.volumes {
        if doc["volumes"].get(&v.id).is_none() {
            doc["volumes"][&v.id] = json!({});
        }
        let observed = serde_json::to_value(v).map_err(error)?;
        if let (Some(existing), Some(fields)) =
            (doc["volumes"][&v.id].as_object_mut(), observed.as_object())
        {
            for (k, value) in fields {
                existing.insert(k.clone(), value.clone());
            }
        }
    }
    if !doc["recentOperations"].is_array() {
        doc["recentOperations"] = json!([]);
    }
    let ids = doc["recentOperations"].as_array_mut().unwrap();
    ids.push(json!(operation_id));
    if ids.len() > 256 {
        ids.remove(0);
    }
    if let Some(payload_hash) = mirror_hash {
        if doc.get("mirrorEditOperations").is_none() {
            doc["mirrorEditOperations"] = json!({});
        }
        doc["mirrorEditOperations"].as_object_mut().ok_or("Invalid saved mirror edit receipts")?
            .insert(operation_id.clone(), json!({"payloadHash":payload_hash,"type":operation_type,
                "recordedAt":now(),"provenance":provenance}));
    }
    if let Some(payload_hash) = agent_hash {
        if doc.get("agentOperations").is_none() {
            doc["agentOperations"] = json!({});
        }
        let next_revision = store
            .revision()
            .checked_add(1)
            .ok_or("Saved revision exceeds the supported range")?;
        let outcome = doc["history"].as_array().and_then(|history| {
            history
                .iter()
                .rev()
                .find(|event| event["id"] == operation_id)
        });
        let mut receipt = json!({"operationId":operation_id,"payloadHash":payload_hash,"committedRevision":next_revision,
            "operationType":operation_type,"recordedAt":now(),"path":op.get("path"),
            "key":outcome.and_then(|event|event.get("key")),"provenance":provenance});
        if operation_type == "review" {
            receipt["action"] = op["action"].clone();
        }
        if group_hash.is_some() {
            receipt["memberCount"] = outcome.map(|event| event["memberCount"].clone()).unwrap_or(Value::Null);
        }
        doc["agentOperations"]
            .as_object_mut()
            .ok_or("Invalid agent operation receipt map")?
            .insert(operation_id, receipt);
    }
    let saved = store.commit_guarded(doc, || {
        bulk_mark::ensure_current(&group_targets)?;
        if let Some(guard) = &mirror_guard {
            guard.validate(s)?;
        }
        Ok(())
    })?;
    if classification_changed {
        // A running pass owns the decision snapshot from its start. Stop further
        // queued work after an intent change, so the next pass uses the new marks.
        s.copy_queue.pause();
    }
    if classification_changed || operation_type == "inventoryPriority" {
        s.index.update_priority(&saved, &s.volumes);
    }
    if let Some((kind, path)) = activity {
        s.review.activity(kind, &path, saved["revision"].as_u64());
    }
    Ok(saved)
}
#[tauri::command]
async fn mutate(
    state: State<'_, Shared>,
    expected_revision: u64,
    operation_id: String,
    operation: Value,
) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        run_mutation(&s, expected_revision, operation_id, operation)
    })
    .await
    .map_err(error)?
}
#[tauri::command]
async fn recover(state: State<'_, Shared>) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || recovery_control::recover_ui(&s))
    .await
    .map_err(error)?
}

fn status_service(s: &AppState) -> Result<Value, String> {
    Ok(
        json!({"scan":s.index.scan_status()?,"gaps":s.index.gaps()?,"decisionCount":document(s)?["decisions"].as_object().map(|m|m.len()).unwrap_or(0),
            "mirrorEvidenceStamp":s.mirrors.matching_stamp()}),
    )
}

fn healthy_owner(s: &AppState) -> Result<(), String> {
    let guard = s.store.lock().map_err(error)?;
    let store = guard
        .as_ref()
        .ok_or("This instance does not own the review data file")?;
    if let Some(problem) = &store.problem {
        return Err(problem.clone());
    }
    Ok(())
}

fn copy_destination(s: &AppState) -> Volume {
    copy_queue::destination(if s.fixture {
        s.data_path.parent()
    } else {
        None
    })
}

#[tauri::command]
async fn copy_queue_snapshot(state: State<'_, Shared>) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || copy_control::snapshot(&s))
    .await
    .map_err(error)?
}

#[tauri::command]
async fn control_copy_queue(
    state: State<'_, Shared>,
    request: Value,
) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || copy_control::control(&s, &request, MutationOrigin::ManualUi))
    .await
    .map_err(error)?
}

fn audit_status_service(s: &AppState) -> Result<Value, String> {
    // Retain missed-event coverage in memory if its database write fails. A later
    // successful poll persists a root-wide gap before clearing this warning.
    let observation = s.background.observe().ok();
    let mut fault = s.watcher_fault.lock().map_err(error)?;
    if let Some(problem) = fault.as_ref() {
        if observation.is_some() && s.audit.record_watch_gap("", problem).is_ok() {
            *fault = None;
        }
    }
    let mut status = s.audit.status()?;
    if let Some(problem) = fault.as_ref() {
        status["problem"] = json!(problem);
        status["watcherProblem"] = json!(problem);
        status["coverageGaps"] = json!(status["coverageGaps"].as_u64().unwrap_or(0) + 1);
        status["complete"] = json!(false);
    }
    Ok(status)
}

fn audit_control_service(
    s: &AppState,
    action: &str,
    scope: &str,
    path: Option<&str>,
) -> Result<Value, String> {
    let _start = if action != "pause" { Some(s.background.start_guard()?) } else { None };
    healthy_owner(s)?;
    match action {
        "pause" => s.audit.pause(),
        "resume" => s.audit.start(Vec::new(), s.volumes.clone(), true, 4)?,
        "start" => {
            let roots = match scope {
                "all" => s
                    .volumes
                    .iter()
                    .filter(|volume| volume.available)
                    .map(|volume| volume.root.clone())
                    .collect(),
                "here" => {
                    let path = filesystem::normalize(path.ok_or("Choose a folder to check")?)?;
                    filesystem::safe(&path, &s.volumes, false)?;
                    vec![path]
                }
                _ => return Err("Unknown checksum scan scope".into()),
            };
            s.audit.start(roots, s.volumes.clone(), false, 4)?;
        }
        _ => return Err("Unknown checksum scan action".into()),
    }
    drop(_start);
    audit_status_service(s)
}

fn audit_notifications_service(s: &AppState, limit: usize, offset: usize) -> Result<Value, String> {
    if !(1..=100).contains(&limit) || offset > 10_000 {
        return Err("Notification page is outside the supported range".into());
    }
    let mut result = s.audit.notifications(limit, offset)?;
    if let Some(items) = result["items"].as_array_mut() {
        for item in items {
            if let Some(id) = item["id"].as_i64() {
                item["id"] = json!(id.to_string());
            }
            // A notification belongs to this volume namespace. A removed entry opens
            // its nearest surviving parent, while the original path stays in the record.
            let original_path = item["path"].as_str().map(String::from);
            let mut destination = original_path.as_deref().map(PathBuf::from);
            item["originalPath"] = json!(original_path);
            item["path"] = Value::Null;
            item["navigable"] = json!(false);
            while let Some(candidate) = destination {
                let candidate_text = candidate.to_string_lossy().to_string();
                if filesystem::identify(&candidate_text, &s.volumes).is_err() {
                    break;
                }
                if filesystem::safe(&candidate_text, &s.volumes, false).is_ok() {
                    item["path"] = json!(candidate_text);
                    item["navigable"] = json!(true);
                    break;
                }
                destination = candidate.parent().map(PathBuf::from);
            }
        }
    }
    Ok(result)
}

fn audit_read_service(s: &AppState, ids: &[String], all: bool) -> Result<Value, String> {
    let observation = s.background.observe()?;
    healthy_owner(s)?;
    if ids.len() > 1000 {
        return Err("Too many notification IDs".into());
    }
    let parsed: Vec<i64> = ids
        .iter()
        .map(|id| {
            id.parse::<i64>()
                .map_err(|_| "Invalid notification ID".to_string())
        })
        .collect::<Result<_, _>>()?;
    if parsed.iter().any(|id| *id < 1) {
        return Err("Invalid notification ID".into());
    }
    s.audit.mark_read(&parsed, all)?;
    drop(observation);
    audit_status_service(s)
}

#[tauri::command]
async fn audit_status(state: State<'_, Shared>) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || audit_status_service(&s))
        .await
        .map_err(error)?
}

#[tauri::command]
async fn audit_control(
    state: State<'_, Shared>,
    action: String,
    scope: String,
    path: Option<String>,
) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        audit_control_service(&s, &action, &scope, path.as_deref())
    })
    .await
    .map_err(error)?
}

#[tauri::command]
async fn audit_notifications(
    state: State<'_, Shared>,
    limit: usize,
    offset: usize,
) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || audit_notifications_service(&s, limit, offset))
        .await
        .map_err(error)?
}

#[tauri::command]
async fn audit_read_notifications(
    state: State<'_, Shared>,
    ids: Option<Vec<String>>,
    all: Option<bool>,
) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        audit_read_service(&s, &ids.unwrap_or_default(), all.unwrap_or(false))
    })
    .await
    .map_err(error)?
}
#[tauri::command]
async fn get_status(state: State<'_, Shared>) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || status_service(&s))
        .await
        .map_err(error)?
}

fn scan_service(s: &AppState, mode: String, path: String, include_generated: bool) -> Result<Value, String> {
    if include_generated && !matches!(mode.as_str(), "all" | "current") {
        return Err("Choose whether to include generated folders when starting an inventory.".into());
    }
    let _start = if mode != "pause" { Some(s.background.start_guard()?) } else { None };
    {
        let guard = s.store.lock().map_err(error)?;
        let store = guard
            .as_ref()
            .ok_or("This instance does not own the data file")?;
        if let Some(e) = &store.problem {
            return Err(e.clone());
        }
        if mode != "pause" {
            s.index.update_priority(&store.doc, &s.volumes);
        }
    }
    match mode.as_str() {
        "pause" => {
            s.index.cancel.store(true, Ordering::Relaxed);
        }
        "resume" => s.index.start(Vec::new(), s.volumes.clone(), true, false)?,
        "all" => s.index.start(
            s.volumes
                .iter()
                .filter(|v| v.available)
                .map(|v| v.root.clone())
                .collect(),
            s.volumes.clone(),
            false,
            include_generated,
        )?,
        "current" => s.index.start(
            vec![filesystem::normalize(&path)?],
            s.volumes.clone(),
            false,
            include_generated,
        )?,
        _ => return Err("Unknown inventory action".into()),
    }
    s.index.scan_status()
}
#[tauri::command]
async fn scan(state: State<'_, Shared>, mode: String, path: String, include_generated: Option<bool>) -> Result<Value, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || scan_service(&s, mode, path, include_generated.unwrap_or(false)))
        .await
        .map_err(error)?
}

#[tauri::command]
async fn search_dirs(state: State<'_, Shared>, search: String) -> Result<Vec<String>, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || s.index.known_dirs(&search))
        .await
        .map_err(error)?
}
fn next_attention_service(
    s: &AppState,
    path: String,
    after: Option<String>,
) -> Result<Option<Value>, String> {
    let _observation = s.background.observe()?;
    filesystem::identify(&path, &s.volumes)?;
    let doc = document(&s)?;
    let confirmations = manual_backup::ConfirmationCache::default();
    let rollups = rollup::Context::new(s, &doc).ok();
    let mut first = None;
    let mut next = None;
    let mut passed = after.is_none();
    let mut position = 0usize;
    s.index.each_entry(&path, "", |e| {
        let result = effective_cached(s, &doc, &e, &confirmations, rollups.as_ref());
        if review_status(&result) == "attention" {
            if first.is_none() && after.as_ref() != Some(&e.path) {
                first = Some((e.path.clone(), position / 100));
            }
            if passed && after.as_ref() != Some(&e.path) {
                next = Some((e.path.clone(), position / 100));
                return false;
            }
        }
        if after.as_ref() == Some(&e.path) {
            passed = true;
        }
        position += 1;
        true
    })?;
    if let Some((selected, page)) = next.or(first) {
        return Ok(Some(
            json!({"path":path,"selectedPath":selected,"page":page}),
        ));
    }
    let mut candidate = None;
    s.index.each_directory(|dir| {
        if dir == path || after.as_ref() == Some(&dir) {
            return true;
        }
        if filesystem::safe(&dir, &s.volumes, false).is_err() {
            return true;
        }
        let e = filesystem::inspect(std::path::Path::new(&dir));
        let result = effective_cached(s, &doc, &e, &confirmations, rollups.as_ref());
        if review_status(&result) == "attention" {
            let parent = std::path::Path::new(&dir)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(dir.clone());
            candidate = Some(json!({"path":parent,"selectedPath":dir}));
            return false;
        }
        true
    })?;
    Ok(candidate)
}
fn assisted_review_service(s: &AppState) -> Result<Value, String> {
    let _observation = s.background.observe()?;
    assistance::snapshot(s, &document(s)?)
}
#[tauri::command]
async fn next_attention(
    state: State<'_, Shared>,
    path: String,
    after: Option<String>,
) -> Result<Option<Value>, String> {
    let s = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || next_attention_service(&s, path, after))
        .await
        .map_err(error)?
}

fn runtime_path_arg(args: &[String], name: &str) -> Option<PathBuf> {
    let index = args.iter().position(|arg| arg == name)?;
    let path = args
        .get(index + 1)
        .and_then(|value| filesystem::normalize(value).ok())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    if path.is_none() {
        eprintln!("{name} requires an absolute local Windows path.");
        std::process::exit(2);
    }
    path
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let compact_window = args.iter().any(|arg| arg == "--compact-window");
    let copy_inspection_only = args.iter().any(|arg| arg == "--copy-inspection-only");
    let fixture = runtime_path_arg(&args, "--fixture-root");
    let data_directory = runtime_path_arg(&args, "--data-dir");
    let journal = runtime_path_arg(&args, "--journal-file");
    if (option_env!("MIGRATION_MASTER_FIXTURE_ONLY") == Some("1") && fixture.is_none())
        || fixture.as_ref().is_some_and(|path| !path.join("source").is_dir())
        || (fixture.is_some() && (data_directory.is_some() || journal.is_some()))
        || journal.as_ref().is_some_and(|path| !path.is_file())
    {
        eprintln!("Use --fixture-root with a source directory for isolated runs. For regular runs, --data-dir sets the runtime directory and --journal-file selects an existing review journal.");
        std::process::exit(2);
    }
    let executable = std::env::current_exe().unwrap_or_else(|problem| {
        eprintln!("Cannot locate the application executable: {problem}");
        std::process::exit(2);
    });
    let base = executable.parent().unwrap_or_else(|| {
        eprintln!("Cannot locate the application directory.");
        std::process::exit(2);
    });
    let runtime = fixture
        .as_ref()
        .map(|path| path.join("runtime"))
        .or(data_directory)
        .unwrap_or_else(|| base.join("runtime"));
    let data_path = fixture
        .as_ref()
        .map(|path| path.join("data.json"))
        .or(journal)
        .unwrap_or_else(|| runtime.join("migration_master_data.json"));
    let roots = filesystem::volumes(fixture.as_ref().map(|p| p.join("source")).as_deref());
    let opened = Store::open(data_path.clone(), &roots[0].root);
    let (store, problem) = match opened {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e)),
    };
    let namespace = store::hash(
        roots
            .iter()
            .map(|v| v.id.as_str())
            .collect::<Vec<_>>()
            .join("|")
            .as_bytes(),
    );
    let cache_runtime = if store.is_some() {
        runtime.clone()
    } else {
        runtime.join(format!("blocked-instance-{}", std::process::id()))
    };
    let index = Index::open(cache_runtime.join(format!("inventory-{}.sqlite", &namespace[..12])))
        .expect("Cannot open inventory cache");
    if let Some(journal) = &store {
        index.update_priority(&journal.doc, &roots);
    }
    let exclusions = vec![
        cache_runtime.to_string_lossy().to_string(),
        data_path.with_extension("*").to_string_lossy().to_string(),
        executable.to_string_lossy().to_string(),
    ];
    let audit = audit::Audit::open(
        cache_runtime.join(format!("audit-{}.sqlite", &namespace[..12])),
        namespace,
        exclusions.clone(),
    )
    .expect("Cannot open checksum index");
    audit
        .set_volumes(roots.clone())
        .expect("Cannot configure source volume identities");
    let copy_queue = copy_queue::CopyQueue::new(
        cache_runtime.join("copy-receipts.sqlite"),
        exclusions.clone(),
        fixture.is_some(),
    );
    let state = Arc::new(AppState {
        background: background::Control::new(args.iter().any(|arg| matches!(arg.as_str(), "--background" | "--fixture-tray"))),
        store: Mutex::new(store),
        startup_problem: problem,
        volumes: roots,
        index,
        audit,
        watcher: Mutex::new(None),
        watcher_fault: Arc::new(Mutex::new(None)),
        exclusions,
        data_path,
        fixture: fixture.is_some(),
        runtime: runtime.clone(),
        gateway: Mutex::new(json!({"state":"unavailable","problem":"Connection has not started"})),
        gateway_server: Mutex::new(None),
        review: review::ReviewBridge::default(),
        assistance: assistance::Cache::default(),
        adaptive: adaptive_review::Runtime::default(),
        mirrors: mirror_service::Runtime::default(),
        copy_queue,
        copy_control: Mutex::new(()),
        copy_inspection_only,
    });
    tauri::Builder::default()
        .manage(state.clone())
        .on_window_event(|_window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // The frontend obtains an exit ticket, flushes buffered edits,
                // then invokes the shared drain. Explicit tray mode may hide.
                api.prevent_close();
            }
        })
        .setup(move |app| {
            ensure_gateway(&state);
            ensure_watcher(&state);
            let title = if fixture.is_some() {
                "PC Migration Helper — Test fixture"
            } else {
                "PC Migration Helper"
            };
            let builder = tauri::WebviewWindowBuilder::from_config(
                app.handle(),
                &app.config().app.windows[0],
            )?
            .title(title)
            .data_directory(cache_runtime.join("webview"));
            let builder = if compact_window {
                builder.inner_size(940.0, 640.0)
            } else {
                builder
            };
            builder.build()?;
            background::setup(app.handle(), &state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            background::background_status,
            background::background_begin_exit,
            background::background_hide,
            background::background_request_exit,
            background::background_cancel_exit,
            background::background_confirm_exit,
            ui_control::report_ui_state,
            ui_control::poll_ui_commands,
            ui_control::validate_auto_copy,
            ui_control::begin_ui_session,
            get_boot,
            assistance::get_assisted_review,
            adaptive_review::get_adaptive_question_packet,
            mirror_service::preview_mirror_binding,
            mirror_service::mirror_comparison_status,
            mirror_service::start_mirror_comparison,
            mirror_service::cancel_mirror_comparison,
            mirror_service::read_mirror_evidence,
            mirror_service::read_mirror_confirmation,
            mirror_service::prepare_mirror_confirmation,
            copy_queue_snapshot,
            control_copy_queue,
            copy_control::preview_copy_conflict,
            copy_control::reconcile_copy_history,
            copy_control::read_copy_effect_history,
            report_review_surface,
            poll_review_activity,
            plan_review_reveal,
            get_review_entry,
            list_folder,
            get_details,
            mutate,
            recover,
            get_status,
            scan,
            search_dirs,
            next_attention,
            audit_status,
            audit_control,
            audit_notifications,
            audit_read_notifications
        ])
        .run(tauri::generate_context!())
        .expect("PC Migration Helper could not start");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changing_copy_intent_requests_pause_before_further_snapshot_work() {
        let s = fixture();
        let work = PathBuf::from(&s.volumes[0].root).join("Work");
        for index in 0..1_000 {
            std::fs::write(work.join(format!("file-{index:04}.txt")), "synthetic bytes").unwrap();
        }
        let path = work.to_string_lossy().into_owned();
        apply(&s, "copy-planned", json!({"type":"classify","path":path,"status":"migrate-now"}));
        std::fs::create_dir_all(s.data_path.parent().unwrap().join("destination")).unwrap();
        let destination = copy_destination(&s);
        s.copy_queue.start(document(&s).unwrap(), s.volumes.clone(), destination, "all", None).unwrap();
        apply(&s, "switch-to-manual", json!({"type":"classify","path":path,"status":"manual-pending"}));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = s.copy_queue.status().unwrap();
            if status["running"] == false {
                assert_eq!(status["phase"], "paused", "{status}");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "{status}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(details_service(&s, path).unwrap()["decision"]["status"], "manual-pending");
    }

    #[test]
    fn manual_backup_intent_confirmation_exceptions_and_undo_are_distinct() {
        let s = fixture();
        let work = PathBuf::from(&s.volumes[0].root).join("Work");
        let ordinary = work.join("ordinary.txt");
        let pending_child = work.join("Child");
        let private = work.join("Private");
        std::fs::create_dir_all(&private).unwrap();
        std::fs::write(&ordinary, "original").unwrap();
        std::fs::write(private.join("secret.txt"), "private").unwrap();
        let work = work.to_string_lossy().into_owned();
        let pending_child = pending_child.to_string_lossy().into_owned();
        let private = private.to_string_lossy().into_owned();
        apply(&s, "manual-plan", json!({"type":"classify","path":work,"status":"manual-pending"}));
        let pending = details_service(&s, ordinary.to_string_lossy().into_owned()).unwrap();
        assert_eq!(pending["effective"]["status"], "manual-pending");
        assert_eq!(pending["effective"]["inheritedFrom"], work);
        assert!(pending["decision"].is_null());
        apply(&s, "narrow-pending", json!({"type":"classify","path":pending_child,"status":"manual-pending"}));
        apply(&s, "private-copy", json!({"type":"classify","path":private,"status":"last-moment"}));

        apply(&s, "owner-confirms", json!({"type":"classify","path":work,"status":"manual-confirmed"}));
        let confirmed = details_service(&s, work.clone()).unwrap();
        assert_eq!(confirmed["decision"]["verification"]["kind"], "manual-backup-assertion");
        assert_eq!(confirmed["decision"]["verification"]["destination"], Value::Null);
        assert_eq!(details_service(&s, ordinary.to_string_lossy().into_owned()).unwrap()["effective"]["status"], "attention");
        assert_eq!(details_service(&s, pending_child).unwrap()["effective"]["status"], "manual-pending");
        assert_eq!(details_service(&s, private).unwrap()["effective"]["status"], "last-moment");

        apply(&s, "undo-owner-confirmation", json!({"type":"undo"}));
        let restored = details_service(&s, work).unwrap();
        assert_eq!(restored["decision"]["status"], "manual-pending");
        assert!(restored["decision"]["verification"].is_null());
    }

    #[test]
    fn source_changes_invalidate_selected_manual_confirmation_and_new_leaves_stay_unconfirmed() {
        let s = fixture();
        apply(&s, "redownload-ancestor", json!({"type":"classify","path":s.volumes[0].root,"status":"redownload"}));
        let work = PathBuf::from(&s.volumes[0].root).join("Work");
        let child = work.join("Child");
        let leaf = child.join("original.txt");
        std::fs::write(&leaf, "before").unwrap();
        let work = work.to_string_lossy().into_owned();
        apply(&s, "manual-scope-confirmed", json!({"type":"classify","path":work,"status":"manual-confirmed"}));
        let leaf_path = leaf.to_string_lossy().into_owned();
        assert_eq!(details_service(&s, work.clone()).unwrap()["effective"]["status"], "manual-confirmed");
        assert_eq!(details_service(&s, leaf_path.clone()).unwrap()["effective"]["status"], "attention");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&leaf, "after!").unwrap();
        s.audit.record_watch_change(&leaf_path, "modified").unwrap();
        let changed = details_service(&s, work.clone()).unwrap();
        assert_eq!(changed["effective"]["status"], "attention");
        assert_eq!(changed["effective"]["originalStatus"], "manual-confirmed");
        assert_eq!(changed["effective"]["stale"], true);
        assert_eq!(changed["effective"]["own"], true);
        assert!(changed["effective"]["inheritedFrom"].is_null());
        assert_eq!(details_service(&s, work.clone()).unwrap()["decision"]["status"], "manual-confirmed");

        apply(&s, "owner-reconfirms", json!({"type":"classify","path":work,"status":"manual-confirmed"}));
        assert_eq!(details_service(&s, work.clone()).unwrap()["effective"]["status"], "manual-confirmed");
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(child.join("new.txt"), "new original").unwrap();
        s.audit.record_watch_change(&child.join("new.txt").to_string_lossy(), "created").unwrap();
        assert_eq!(details_service(&s, work).unwrap()["effective"]["status"], "attention");
        let newly_added = details_service(&s, child.join("new.txt").to_string_lossy().into_owned()).unwrap();
        assert_eq!(newly_added["effective"]["status"], "attention");
    }

    #[test]
    fn manual_marks_never_enter_automated_copy_and_override_parent_copy_intent() {
        let s = fixture();
        let source = PathBuf::from(&s.volumes[0].root);
        let work = source.join("Work");
        let now = work.join("automatic.txt");
        let pending = work.join("manual-pending.txt");
        let confirmed = work.join("manual-confirmed.txt");
        for file in [&now, &pending, &confirmed] {
            std::fs::write(file, "source bytes").unwrap();
        }
        let scope = work.to_string_lossy().into_owned();
        apply(&s, "auto-parent", json!({"type":"classify","path":scope,"status":"migrate-now"}));
        apply(&s, "pending-file", json!({"type":"classify","path":pending.to_string_lossy(),"status":"manual-pending"}));
        apply(&s, "confirmed-file", json!({"type":"classify","path":confirmed.to_string_lossy(),"status":"manual-confirmed"}));
        let destination_path = s.data_path.parent().unwrap().join("destination");
        std::fs::create_dir_all(&destination_path).unwrap();
        let destination = copy_destination(&s);
        let doc = document(&s).unwrap();
        let queue = s.copy_queue.snapshot(&doc, &s.volumes, &destination).unwrap();
        assert_eq!(queue["migrateNowCount"], 1);
        assert_eq!(queue["lastMomentCount"], 0);
        s.copy_queue.start(doc, s.volumes.clone(), destination, "all", None).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = s.copy_queue.status().unwrap();
            if status["running"] == false {
                assert_eq!(status["phase"], "complete", "{status}");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "{status}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(destination_path.join("Work").join("automatic.txt").is_file());
        assert!(!destination_path.join("Work").join("manual-pending.txt").exists());
        assert!(!destination_path.join("Work").join("manual-confirmed.txt").exists());
        assert!(pending.is_file() && confirmed.is_file());
    }

    #[test]
    fn manual_child_rollups_and_journal_reopen_preserve_assertion_provenance() {
        let s = fixture();
        let parent = PathBuf::from(&s.volumes[0].root).join("Work").join("Group");
        std::fs::create_dir_all(&parent).unwrap();
        for name in ["one.txt", "two.txt"] {
            let path = parent.join(name);
            std::fs::write(&path, name).unwrap();
            apply(&s, &format!("pending-{name}"), json!({"type":"classify","path":path.to_string_lossy(),"status":"manual-pending"}));
        }
        let path = parent.to_string_lossy().into_owned();
        let unobserved = details_service(&s, path.clone()).unwrap();
        assert_eq!(unobserved["effective"]["rollup"]["kind"], "unknown");
        s.index.refresh(&path, &s.volumes, None, false).unwrap();
        let rolled = details_service(&s, path.clone()).unwrap();
        assert_eq!(rolled["effective"]["rollup"]["kind"], "unanimous");
        assert_eq!(rolled["effective"]["status"], "manual-pending");
        for name in ["one.txt", "two.txt"] {
            apply(&s, &format!("confirm-{name}"), json!({"type":"classify","path":parent.join(name).to_string_lossy(),"status":"manual-confirmed"}));
        }
        let rolled = details_service(&s, path).unwrap();
        assert_eq!(rolled["effective"]["status"], "manual-confirmed");
        assert_eq!(rolled["effective"]["rollup"]["kind"], "unanimous");
        let expected = document(&s).unwrap();
        let journal = s.data_path.clone();
        let root = s.volumes[0].root.clone();
        drop(s);
        let reopened = Store::open(journal, &root).unwrap();
        assert_eq!(reopened.doc["decisions"], expected["decisions"]);
        assert_eq!(reopened.doc["history"], expected["history"]);
    }
    fn wait_audit(s: &AppState) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let status = s.audit.status().unwrap();
            if status["phase"] == "complete" && !s.audit.is_running() {
                return status;
            }
            assert_ne!(status["phase"], "error", "{status}");
            assert!(
                std::time::Instant::now() < deadline,
                "Audit did not complete: {status}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn clearing_a_child_override_does_not_inherit_selected_parent_confirmation() {
        let s=fixture();
        let parent=PathBuf::from(&s.volumes[0].root).join("Work");
        let child=parent.join("Child");
        apply(&s,"parent-manual-confirm",json!({"type":"classify","path":parent.to_string_lossy(),"status":"manual-confirmed"}));
        apply(&s,"child-keep-override",json!({"type":"classify","path":child.to_string_lossy(),"status":"last-moment"}));
        assert_eq!(details_service(&s,child.to_string_lossy().into_owned()).unwrap()["effective"]["status"],"last-moment");
        apply(&s,"undo-child-override",json!({"type":"undo"}));
        let child=details_service(&s,child.to_string_lossy().into_owned()).unwrap();
        assert!(child["decision"].is_null());
        assert_eq!(child["effective"]["status"],"attention");
        assert_eq!(child["effective"]["originalStatus"],"manual-confirmed");
    }

    #[test]
    fn undo_preserves_stale_confirmation_barrier_after_source_metadata_changed() {
        use std::fs::FileTimes;
        let s=fixture();
        let path=PathBuf::from(&s.volumes[0].root).join("Work").join("original.txt");
        std::fs::write(&path,b"original").unwrap();
        let initial=std::fs::metadata(&path).unwrap().modified().unwrap();
        apply(&s,"confirm-before-change",json!({"type":"classify","path":path.to_string_lossy(),"status":"manual-confirmed"}));
        apply(&s,"change-intent-before-undo",json!({"type":"classify","path":path.to_string_lossy(),"status":"last-moment"}));
        std::fs::File::options().write(true).open(&path).unwrap().set_times(FileTimes::new().set_modified(initial+std::time::Duration::from_secs(5))).unwrap();
        apply(&s,"undo-after-source-change",json!({"type":"undo"}));
        let result=details_service(&s,path.to_string_lossy().into_owned()).unwrap();
        assert_eq!(result["decision"]["status"],"manual-confirmed");
        assert_eq!(result["effective"]["status"],"attention");
        assert_eq!(result["effective"]["stale"],true);
    }

    #[test]
    fn checksum_changes_return_manual_external_assertions_to_attention() {
        let s = fixture();
        let parent = PathBuf::from(&s.volumes[0].root).join("Work");
        let leaf = parent.join("content.txt");
        std::fs::write(&leaf, b"before").unwrap();
        audit_control_service(&s, "start", "all", None).unwrap();
        wait_audit(&s);
        let parent = parent.to_string_lossy().to_string();
        apply(
            &s,
            "external-after-baseline",
            json!({"type":"classify","path":parent,"status":"on-external"}),
        );
        audit_control_service(&s, "start", "all", None).unwrap();
        wait_audit(&s);
        assert_eq!(
            details_service(&s, parent.clone()).unwrap()["effective"]["status"],
            "on-external",
            "An unchanged verification pass must preserve the earlier manual assertion"
        );
        let original_time = std::fs::metadata(&leaf).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&leaf, b"after!").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&leaf)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_time))
            .unwrap();
        audit_control_service(&s, "start", "all", None).unwrap();
        wait_audit(&s);
        let details = details_service(&s, parent).unwrap();
        assert_eq!(details["effective"]["status"], "attention");
        assert_eq!(details["effective"]["stale"], true);
        assert_eq!(details["decision"]["status"], "on-external");
        assert!(details["audit"]["allocatedBytes"].as_u64().is_some());
    }

    #[test]
    fn audit_controls_and_notification_pagination_are_bounded() {
        let s = fixture();
        assert!(audit_control_service(&s, "start", "here", Some("Z:\\outside")).is_err());
        assert!(audit_control_service(&s, "unknown", "all", None).is_err());
        assert!(audit_notifications_service(&s, 101, 0).is_err());
        assert!(audit_notifications_service(&s, 1, 10_001).is_err());
        assert!(audit_read_service(&s, &["invalid".into()], false).is_err());
    }

    #[test]
    fn a_retained_watcher_write_failure_cannot_leave_an_external_claim_fresh() {
        let s = fixture();
        let parent = PathBuf::from(&s.volumes[0].root)
            .join("Work")
            .to_string_lossy()
            .to_string();
        apply(
            &s,
            "external-before-watcher-gap",
            json!({"type":"classify","path":parent,"status":"on-external"}),
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        *s.watcher_fault.lock().unwrap() = Some("Fixture: an event could not be persisted".into());
        assert_eq!(
            details_service(&s, parent.clone()).unwrap()["effective"]["status"],
            "attention"
        );
        audit_status_service(&s).unwrap();
        assert!(s.watcher_fault.lock().unwrap().is_none());
        assert_eq!(
            details_service(&s, parent).unwrap()["effective"]["status"],
            "attention",
            "The persisted root-wide coverage gap must survive clearing the in-memory failure"
        );
    }
    pub(super) fn fixture() -> AppState {
        let root = std::env::temp_dir().join(format!(
            "migration-native-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let source = root.join("source");
        std::fs::create_dir_all(source.join("Work").join("Child")).unwrap();
        let volumes = filesystem::volumes(Some(&source));
        let data_path = root.join("data.json");
        let audit = audit::Audit::open(
            root.join("audit.sqlite"),
            "test-fixture".into(),
            vec![root.join("runtime").to_string_lossy().to_string()],
        )
        .unwrap();
        audit.set_volumes(volumes.clone()).unwrap();
        AppState {
            background: background::Control::new(false),
            store: Mutex::new(Some(
                Store::open(data_path.clone(), &volumes[0].root).unwrap(),
            )),
            startup_problem: None,
            volumes,
            index: Index::open(root.join("index.sqlite")).unwrap(),
            audit,
            watcher: Mutex::new(None),
            watcher_fault: Arc::new(Mutex::new(None)),
            exclusions: Vec::new(),
            data_path,
            fixture: true,
            runtime: root.join("runtime"),
            gateway: Mutex::new(json!({"state":"running","instanceId":"fixture-native-instance"})),
            gateway_server: Mutex::new(None),
            review: review::ReviewBridge::default(),
            assistance: assistance::Cache::default(),
            adaptive: adaptive_review::Runtime::default(),
            mirrors: mirror_service::Runtime::default(),
            copy_queue: copy_queue::CopyQueue::new(
                root.join("copy-receipts.sqlite"),
                Vec::new(),
                true,
            ),
            copy_control: Mutex::new(()),
            copy_inspection_only: false,
        }
    }
    fn apply(s: &AppState, id: &str, op: Value) -> Value {
        run_mutation(
            s,
            document(s).unwrap()["revision"].as_u64().unwrap(),
            id.into(),
            op,
        )
        .unwrap()
    }
    #[test]
    fn inheritance_notes_exceptions_and_undo() {
        let s = fixture();
        let parent = format!("{}\\Work", s.volumes[0].root);
        let child = format!("{parent}\\Child");
        apply(
            &s,
            "parent",
            json!({"type":"classify","path":parent,"status":"last-moment"}),
        );
        apply(
            &s,
            "note",
            json!({"type":"note","path":child,"notes":"Keep live export"}),
        );
        let e = filesystem::inspect(std::path::Path::new(&child));
        let result = effective(&s, &document(&s).unwrap(), &e);
        assert_eq!(result["status"], "last-moment");
        assert_eq!(result["own"], false);
        apply(
            &s,
            "child",
            json!({"type":"classify","path":child,"status":"redownload"}),
        );
        assert_eq!(
            effective(
                &s,
                &document(&s).unwrap(),
                &filesystem::inspect(std::path::Path::new(&parent))
            )["mixed"],
            1
        );
        apply(&s, "undo", json!({"type":"undo"}));
        assert_eq!(
            effective(&s, &document(&s).unwrap(), &e)["status"],
            "last-moment"
        );
        assert_eq!(
            document(&s).unwrap()["history"].as_array().unwrap().len(),
            4
        );
    }
    #[test]
    fn backup_assertions_never_inherit_and_new_content_needs_attention() {
        let s = fixture();
        let parent = format!("{}\\Work", s.volumes[0].root);
        let child = format!("{parent}\\Child");
        s.index.refresh(&parent, &s.volumes, None, true).unwrap();
        let d = apply(
            &s,
            "claim",
            json!({"type":"classify","path":parent,"status":"on-external"}),
        );
        let key = filesystem::identify(&parent, &s.volumes).unwrap().2;
        assert_eq!(
            d["decisions"][&key]["verification"]["kind"],
            "manual-assertion"
        );
        assert_eq!(
            effective(&s, &d, &filesystem::inspect(std::path::Path::new(&child)))["status"],
            "attention"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(
            std::path::Path::new(&parent).join("new-file.txt"),
            b"fixture only",
        )
        .unwrap();
        s.index.refresh(&parent, &s.volumes, None, true).unwrap();
        let result = effective(&s, &d, &filesystem::inspect(std::path::Path::new(&parent)));
        assert_eq!(result["status"], "attention");
        assert_eq!(result["stale"], true);
    }
    #[test]
    fn idempotence_volume_metadata_and_conflict_preservation() {
        let s = fixture();
        let path = format!("{}\\Work", s.volumes[0].root);
        let op = json!({"type":"classify","path":path,"status":"migrate-now"});
        {
            let mut guard = s.store.lock().unwrap();
            let store = guard.as_mut().unwrap();
            let mut d = store.doc.clone();
            d["volumes"][&s.volumes[0].id] = json!({"futureProvenance":"retained"});
            store.commit(d).unwrap();
        }
        let result = apply(&s, "same-operation", op.clone());
        assert_eq!(
            result["volumes"][&s.volumes[0].id]["futureProvenance"],
            "retained"
        );
        assert_eq!(
            run_mutation(&s, 0, "same-operation".into(), op).unwrap()["revision"],
            result["revision"]
        );
        assert!(run_mutation(&s, 0, "different-operation".into(), json!({"type":"undo"})).is_err());
    }
    #[test]
    fn preview_columns_persist_without_a_fixed_depth_and_reject_escaped_paths() {
        let s = fixture();
        let mut path = s.volumes[0].root.clone();
        let mut columns = Vec::new();
        for number in 0..12 {
            path.push_str(&format!("\\Preview-{number}"));
            columns.push(json!({"path":path,"page":number,"scrollTop":number as f64*21.5,"width":320,"futureColumnField":"preserved"}));
        }
        let patch = json!({"previewColumns":columns,"previewScrollLeft":225.5,
            "browserColumnWidth":360,"toolbarHeight":111,"scanDetailsHeight":77});
        let saved = apply(&s, "preview-columns", json!({"type":"view","patch":patch}));
        assert_eq!(
            saved["view"]["previewColumns"].as_array().unwrap().len(),
            12
        );
        let revision = saved["revision"].as_u64().unwrap();
        assert!(run_mutation(&s,revision,"bad-preview-path".into(),json!({"type":"view","patch":{"previewColumns":[{"path":"Z:\\outside","page":0,"scrollTop":0}]}})).is_err());
        assert!(run_mutation(
            &s,
            revision,
            "bad-preview-width".into(),
            json!({"type":"view","patch":{"browserColumnWidth":239}})
        )
        .is_err());
        assert!(run_mutation(
            &s,
            revision,
            "bad-preview-scroll".into(),
            json!({"type":"view","patch":{"previewScrollLeft":-1}})
        )
        .is_err());
        assert_eq!(document(&s).unwrap()["revision"], revision);
        let data = s.data_path.clone();
        let root = s.volumes[0].root.clone();
        drop(s);
        let reopened = Store::open(data, &root).unwrap();
        assert_eq!(
            reopened.doc["view"]["previewColumns"],
            patch["previewColumns"]
        );
        assert_eq!(reopened.doc["view"]["previewScrollLeft"], 225.5);
        assert_eq!(reopened.doc["view"]["browserColumnWidth"], 360);
        assert_eq!(reopened.doc["view"]["toolbarHeight"], 111);
        assert_eq!(reopened.doc["view"]["scanDetailsHeight"], 77);
    }
}
