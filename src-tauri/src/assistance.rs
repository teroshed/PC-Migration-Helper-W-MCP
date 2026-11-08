use crate::{document, filesystem, store::error, AppState, Shared};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{atomic::Ordering, Arc, Mutex},
};

#[derive(Default)]
pub struct Cache(Mutex<Option<Arc<Inventory>>>);

struct Inventory {
    scope: String,
    stamp: String,
    directories: Vec<Directory>,
    gaps: usize,
}

struct Directory {
    path: String,
    files: u64,
    leaves: u64,
    bytes: u64,
    readable: bool,
    gap_below: bool,
    reusable: bool,
    protected: bool,
    parent_id: Option<usize>,
    children: Vec<usize>,
}

fn parent(path: &str) -> Option<String> {
    Some(Path::new(path).parent()?.to_str()?.to_lowercase())
}

fn basename(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

fn reusable_name(path: &str) -> bool {
    matches!(
        basename(path).to_ascii_lowercase().as_str(),
        "node_modules"
            | ".npm-cache"
            | ".pnpm-store"
            | "__pycache__"
            | ".pytest_cache"
            | ".ruff_cache"
    )
}

fn protected_name(path: &str) -> bool {
    matches!(
        basename(path).to_ascii_lowercase().as_str(),
        "patches" | "local-patches" | "vendor-patches" | "local-edits" | "secrets" | "credentials"
    )
}

fn prefixes<'a>(paths: impl Iterator<Item = &'a String>) -> HashSet<String> {
    let mut result = HashSet::new();
    for path in paths {
        let mut current = parent(path);
        while let Some(path) = current {
            current = parent(&path);
            if !result.insert(path) {
                break;
            }
        }
    }
    result
}

impl Inventory {
    fn load(s: &AppState, scope: &str, scan: &Value, stamp: &str) -> Result<Self, String> {
        let db = s.index.connect()?;
        let db = db.unchecked_transaction().map_err(error)?;
        let current: String = db
            .query_row("SELECT value FROM meta WHERE key='scan'", [], |r| r.get(0))
            .map_err(error)?;
        let current: Value = serde_json::from_str(&current).map_err(error)?;
        if current["status"] != "complete" || current["finishedAt"] != stamp {
            return Err(
                "The inventory changed while preparing the interview. Refresh to continue.".into(),
            );
        }
        let root = scope.trim_end_matches('\\').to_ascii_lowercase();
        let prefix = format!("{root}\\");
        let mut query = db
            .prepare(
                "SELECT q.path, coalesce(d.files,0), coalesce(d.bytes,0),
                    q.state='done' AND d.signature IS NOT NULL AND d.problem IS NULL
             FROM queue q LEFT JOIN directories d ON d.path=q.path
             WHERE rtrim(lower(q.path),char(92))=?1 OR substr(lower(q.path),1,length(?2))=?2
             ORDER BY length(q.path), q.path COLLATE NOCASE",
            )
            .map_err(error)?;
        let rows = query
            .query_map(params![root, prefix], |r| {
                let path: String = r.get(0)?;
                let readable: bool = r.get(3)?;
                Ok(Directory {
                    reusable: reusable_name(&path),
                    path,
                    files: crate::index::nonnegative(r.get(1)?, 1)?,
                    leaves: 0,
                    bytes: crate::index::nonnegative(r.get(2)?, 2)?,
                    readable,
                    gap_below: !readable,
                    protected: false,
                    parent_id: None,
                    children: Vec::new(),
                })
            })
            .map_err(error)?;
        let mut directories: Vec<_> = rows.collect::<Result<_, _>>().map_err(error)?;
        let ids: HashMap<_, _> = directories
            .iter()
            .enumerate()
            .map(|(id, directory)| (directory.path.to_lowercase(), id))
            .collect();
        if directories
            .first()
            .is_none_or(|d| !d.path.eq_ignore_ascii_case(scope))
        {
            return Err(
                "The scope was not reached by the completed inventory. Scan this folder.".into(),
            );
        }
        let mut gap_paths: HashSet<String> = directories
            .iter()
            .filter(|directory| !directory.readable)
            .map(|directory| directory.path.to_lowercase())
            .collect();
        let mut gaps = db
            .prepare(
                "SELECT path FROM directories WHERE problem IS NOT NULL
             AND observed_at>=?1 AND substr(lower(path),1,length(?2))=?2",
            )
            .map_err(error)?;
        for row in gaps
            .query_map(
                params![scan["startedAt"].as_str().unwrap_or(""), prefix],
                |row| row.get::<_, String>(0),
            )
            .map_err(error)?
        {
            gap_paths.insert(row.map_err(error)?.to_lowercase());
        }
        let ancestors = prefixes(gap_paths.iter());
        let mut inside_cache = vec![false; directories.len()];
        for (id, dir) in directories.iter_mut().enumerate() {
            let key = dir.path.to_lowercase();
            dir.gap_below |= ancestors.contains(&key) || gap_paths.contains(&key);
            dir.parent_id = parent(&dir.path).and_then(|path| ids.get(&path).copied());
            if id > 0 && dir.parent_id.is_none() {
                // A branch may have disappeared during enumeration. Its rows do not establish coverage.
                dir.readable = false;
                dir.gap_below = true;
                gap_paths.insert(key);
            }
            let parent_cache = dir.parent_id.is_some_and(|id| inside_cache[id]);
            dir.protected = parent_cache && protected_name(&dir.path);
            inside_cache[id] = parent_cache || dir.reusable;
        }
        for id in (0..directories.len()).rev() {
            let dir = &mut directories[id];
            if dir.readable {
                dir.leaves = dir.leaves.saturating_add(dir.files);
            } else {
                dir.bytes = 0;
            }
            let leaves = dir.leaves;
            let bytes = dir.bytes;
            let gap = dir.gap_below;
            if let Some(parent) = dir.parent_id {
                let owner = &mut directories[parent];
                owner.leaves = owner.leaves.saturating_add(leaves);
                owner.bytes = owner.bytes.saturating_add(bytes);
                owner.gap_below |= gap;
                owner.children.push(id);
            }
        }
        Ok(Self {
            scope: scope.to_string(),
            stamp: stamp.to_string(),
            directories,
            gaps: gap_paths.len(),
        })
    }
}

fn scan_state(s: &AppState) -> Result<Value, String> {
    // Reuse the inventory controls' durable queue snapshot, including pending work.
    s.index.scan_status()
}

fn saved_paths(doc: &Value, field: &str) -> HashSet<String> {
    doc["assistance"][field]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_lowercase)
        .collect()
}

fn resolved(
    s: &AppState,
    doc: &Value,
    path: &str,
    confirmations: &crate::manual_backup::ConfirmationCache,
) -> bool {
    let Ok((volume, relative, key)) = filesystem::locate(path, &s.volumes) else {
        return false;
    };
    let own = &doc["decisions"][key];
    if own.is_object() && own["noteOnly"] != true {
        if own["status"] == "manual-confirmed" {
            return confirmations.current(s, own, volume);
        }
        if own["status"] == "on-external" {
            return filesystem::guard_path(path, &s.volumes, false).is_ok_and(|(path, _guards)| {
                let entry = filesystem::inspect(&path);
                entry.problem.is_none()
                    && crate::recorded_effective(s, doc, &entry)["status"] == "on-external"
            });
        }
        return own["status"]
            .as_str()
            .is_some_and(|status| status != "attention");
    }
    let mut rel = relative.as_str();
    while !rel.is_empty() {
        rel = rel.rsplit_once('\\').map_or("", |(left, _)| left);
        let inherited = &doc["decisions"][format!("{}|{}", volume.id, rel.to_lowercase())];
        if inherited.is_object() && inherited["noteOnly"] != true {
            return matches!(
                inherited["status"].as_str(),
                Some("migrate-now" | "last-moment" | "redownload" | "erase" | "manual-pending")
            );
        }
    }
    false
}

struct Question {
    path: String,
    kind: &'static str,
    files: u64,
    bytes: u64,
    reusable: bool,
    protected: bool,
    manual_confirmation: bool,
    manual_basis: Option<String>,
}

impl Question {
    fn directory(dir: &Directory) -> Self {
        Self {
            path: dir.path.clone(),
            kind: "folder",
            files: dir.leaves,
            bytes: dir.bytes,
            reusable: dir.reusable,
            protected: dir.protected,
            manual_confirmation: false,
            manual_basis: None,
        }
    }

    fn display(self, s: &AppState, doc: &Value, scope: &str, stamp: &str) -> Value {
        let name = basename(&self.path);
        let (prompt, reason) = if self.manual_confirmation {
            (format!("Have you completed the manual backup of {name}?"),
             "Confirm only after your manual copy is complete. This records your assertion for the selected path. Children still need their own review. Destination checksums and a restore test remain separate evidence.")
        } else if self.protected {
            (format!("Keep {name} separately from its parent?"),
             "This branch might contain local changes or private data. A separate keep decision protects it if you later choose to recreate the parent.")
        } else if self.reusable {
            (format!("Keep {name} unless you can rebuild it?"),
             "The name suggests generated data. Choose To Redownload only if the retained inputs, lockfile and local patches can reproduce what you need.")
        } else if self.kind == "folder" {
            (format!("Keep {name} with the final migration?"),
             "One decision can cover this branch. Review children if some need a different decision.")
        } else {
            (format!("Keep {name} with the final migration?"),
             "This file needs its own decision. The local suggestion is to keep it if you still need it.")
        };
        let key = filesystem::locate(&self.path, &s.volumes)
            .ok()
            .map(|(_, _, key)| key);
        let suggestion = doc["suggestions"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .find(|value| {
                !self.manual_confirmation
                    && value["state"] == "proposed"
                    && key
                        .as_ref()
                        .is_some_and(|key| value["key"].as_str() == Some(key))
                    && matches!(
                        value["status"].as_str(),
                        Some(
                            "migrate-now"
                                | "last-moment"
                                | "redownload"
                                | "manual-pending"
                                | "manual-confirmed"
                        )
                    )
                    && value["reason"]
                        .as_str()
                        .is_some_and(|reason| !reason.trim().is_empty())
                    && value["createdAt"]
                        .as_str()
                        .is_some_and(|date| date >= stamp)
            });
        let agent_status = suggestion.and_then(|v| v["status"].as_str());
        let proposed = if self.manual_confirmation {
            "manual-confirmed"
        } else {
            agent_status
                .filter(|status| matches!(*status, "migrate-now" | "manual-pending"))
                .unwrap_or("last-moment")
        };
        let evidence: String = suggestion
            .and_then(|v| v["reason"].as_str())
            .unwrap_or(reason)
            .chars()
            .take(1200)
            .collect();
        let source_fingerprint = filesystem::guard_path(&self.path, &s.volumes, false)
            .ok()
            .map(|(path, _guards)| filesystem::inspect(&path).fingerprint);
        let id = crate::store::hash(
            format!(
                "{stamp}|{scope}|{}|{}|{proposed}|{agent_status:?}|{evidence}|{}|{}|{}|{:?}|{source_fingerprint:?}",
                self.path.to_lowercase(),
                self.kind,
                self.files,
                self.bytes,
                self.manual_confirmation,
                self.manual_basis
            )
            .as_bytes(),
        );
        json!({"id":id,"path":self.path,"kind":self.kind,"files":self.files,"bytes":self.bytes,
            "prompt":prompt,"status":proposed,"agentStatus":agent_status,"reason":evidence,
            "source":if suggestion.is_some() {"codex"} else {"local"},"manualConfirmation":self.manual_confirmation,
            "sourceFingerprint":source_fingerprint})
    }
}

fn pick_question(
    s: &AppState,
    doc: &Value,
    inventory: &Inventory,
) -> Result<(Option<Question>, usize), String> {
    let split = saved_paths(doc, "split");
    let deferred = saved_paths(doc, "deferred");
    let blocked_ancestors = prefixes(split.iter().chain(&deferred));
    let mut candidates = Vec::new();
    let mut file_parents = Vec::new();
    let mut nested_exceptions = Vec::new();
    let mut protected_exceptions = Vec::new();
    let (volume, _, _) = filesystem::locate(&inventory.scope, &s.volumes)?;
    let volume_root = inventory.scope.eq_ignore_ascii_case(&volume.root);
    let file_overrides: HashSet<_> = doc["decisions"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
        .filter(|d| {
            d["volumeId"] == volume.id && d["noteOnly"] != true && d["status"] == "attention"
        })
        .filter_map(|d| {
            let relative = d["relativePath"].as_str()?;
            parent(&format!(
                "{}\\{relative}",
                volume.root.trim_end_matches('\\')
            ))
        })
        .collect();
    let mut reachable = vec![0];
    let confirmations = crate::manual_backup::ConfirmationCache::default();
    let mut unresolved = vec![0u64; inventory.directories.len()];
    while let Some(id) = reachable.pop() {
        let dir = &inventory.directories[id];
        let key = dir.path.to_lowercase();
        if deferred.contains(&key) {
            continue;
        }
        // Look past decided parents: an explicit attention override can still be unresolved.
        reachable.extend(&dir.children);
        if !dir.readable {
            continue;
        }
        if resolved(s, doc, &dir.path, &confirmations) {
            let (_, _, decision_key) = filesystem::locate(&dir.path, &s.volumes)?;
            if dir.files > 0
                && (file_overrides.contains(&key)
                    || matches!(
                        doc["decisions"][decision_key]["status"].as_str(),
                        Some("on-external" | "manual-confirmed")
                    ))
            {
                file_parents.push(dir);
            }
            continue;
        }
        unresolved[id] = dir.files;
        if (id == 0 && volume_root)
            || dir.gap_below
            || split.contains(&key)
            || blocked_ancestors.contains(&key)
        {
            if dir.files > 0 {
                file_parents.push(dir);
            }
            continue;
        }
        if dir.protected {
            protected_exceptions.push(id);
        } else if dir.reusable {
            nested_exceptions.push(id);
        } else {
            candidates.push(id);
        }
    }
    for (id, dir) in inventory.directories.iter().enumerate().rev() {
        if let Some(parent) = dir.parent_id {
            unresolved[parent] = unresolved[parent].saturating_add(unresolved[id]);
        }
    }
    // Do not group a parent before offering likely exclusions anywhere beneath it.
    let choose = |ids: Vec<usize>| -> Option<Question> {
        ids.into_iter()
            .max_by(|&left, &right| {
                let a = &inventory.directories[left];
                let b = &inventory.directories[right];
                unresolved[left]
                    .cmp(&unresolved[right])
                    .then(a.bytes.cmp(&b.bytes))
                    .then_with(|| b.path.cmp(&a.path))
            })
            .map(|id| Question::directory(&inventory.directories[id]))
    };
    let branch = choose(protected_exceptions)
        .or_else(|| choose(nested_exceptions))
        .or_else(|| choose(candidates));
    if branch.is_some() {
        return Ok((branch, 0));
    }
    let db = s.index.connect()?;
    let mut unavailable = 0;
    file_parents.sort_by(|a, b| a.path.cmp(&b.path));
    for dir in file_parents {
        let observed: Option<String> = db
            .query_row(
                "SELECT observed_at FROM listing_heads WHERE parent=?1",
                [&dir.path],
                |r| r.get(0),
            )
            .optional()
            .map_err(error)?;
        if observed.is_none_or(|time| time.as_str() < inventory.stamp.as_str())
            && s.index.refresh(&dir.path, &s.volumes, None, true).is_err()
        {
            unavailable += 1;
            continue;
        }
        let mut candidate = None;
        s.index.each_entry(&dir.path, "", |entry| {
            if entry.kind == "file"
                && entry.problem.is_none()
                && !deferred.contains(&entry.path.to_lowercase())
                && !resolved(s, doc, &entry.path, &confirmations)
            {
                candidate = Some(Question {
                    path: entry.path,
                    kind: "file",
                    files: 1,
                    bytes: entry.size,
                    reusable: false,
                    protected: false,
                    manual_confirmation: false,
                    manual_basis: None,
                });
                return false;
            }
            true
        })?;
        if candidate.is_some() {
            return Ok((candidate, unavailable));
        }
    }
    Ok((None, unavailable))
}

fn pending_manual(
    s: &AppState,
    doc: &Value,
    scope: &str,
    volume: &filesystem::Volume,
) -> Vec<Value> {
    let mut pending: Vec<_> = doc["decisions"]
        .as_object()
        .into_iter()
        .flat_map(|marks| marks.values())
        .filter(|decision| {
            decision["status"] == "manual-pending"
                && decision["noteOnly"] != true
                && decision["volumeId"] == volume.id
                && decision["relativePath"].as_str().is_some_and(|relative| {
                    let path = std::path::Path::new(&volume.root).join(relative);
                    filesystem::under(&path.to_string_lossy(), scope)
                })
        })
        .cloned()
        .collect();
    if let Ok((_, relative, key)) = filesystem::locate(scope, &s.volumes) {
        let own = &doc["decisions"][key];
        if !own.is_object() || own["noteOnly"] == true {
            let mut ancestor = relative.as_str();
            while !ancestor.is_empty() {
                ancestor = ancestor.rsplit_once('\\').map_or("", |(left, _)| left);
                let decision =
                    &doc["decisions"][format!("{}|{}", volume.id, ancestor.to_lowercase())];
                if decision.is_object() && decision["noteOnly"] != true {
                    if decision["status"] == "manual-pending" {
                        let mut scoped = decision.clone();
                        scoped["path"] = json!(scope);
                        scoped["relativePath"] = json!(relative);
                        pending.push(scoped);
                    }
                    break;
                }
            }
        }
    }
    pending
}

fn pending_question(
    s: &AppState,
    doc: &Value,
    inventory: &Inventory,
    pending: &[Value],
) -> Option<Question> {
    let deferred = saved_paths(doc, "deferred");
    for decision in pending {
        let volume = s
            .volumes
            .iter()
            .find(|volume| decision["volumeId"] == volume.id)?;
        let relative = decision["relativePath"].as_str()?;
        let path = std::path::Path::new(&volume.root)
            .join(relative)
            .to_string_lossy()
            .into_owned();
        if deferred
            .iter()
            .any(|skipped| filesystem::under(&path, skipped))
        {
            continue;
        }
        if s.index
            .changed_below(&path, &inventory.stamp)
            .unwrap_or(true)
            || s.audit
                .changed_below(&path, &inventory.stamp)
                .unwrap_or(true)
        {
            continue;
        }
        let Ok((source, _guards)) = filesystem::guard_path(&path, &s.volumes, false) else {
            continue;
        };
        let entry = filesystem::inspect(&source);
        if entry.problem.is_some() || !matches!(entry.kind.as_str(), "folder" | "file") {
            continue;
        }
        let summary = inventory
            .directories
            .iter()
            .find(|dir| dir.path.eq_ignore_ascii_case(&path));
        if entry.kind == "folder" && summary.is_none_or(|dir| dir.gap_below) {
            continue;
        }
        return Some(Question {
            path,
            kind: if entry.kind == "folder" {
                "folder"
            } else {
                "file"
            },
            files: summary.map_or(1, |dir| dir.leaves),
            bytes: summary.map_or(entry.size, |dir| dir.bytes),
            reusable: false,
            protected: false,
            manual_confirmation: true,
            manual_basis: Some(crate::store::hash(
                format!("{decision}|{}", entry.fingerprint).as_bytes(),
            )),
        });
    }
    None
}

pub fn snapshot(s: &AppState, doc: &Value) -> Result<Value, String> {
    let Some(scope) = doc["assistance"]["scope"].as_str() else {
        return Ok(json!({"phase":"idle","revision":doc["revision"]}));
    };
    let (volume, _, _) = filesystem::identify(scope, &s.volumes)?;
    if doc["assistance"]["volumeId"] != volume.id {
        return Err("The interview scope is on a different volume. Choose a new scope.".into());
    }
    let pending = pending_manual(s, doc, scope, volume);
    let scan = scan_state(s)?;
    let scoped = scan["roots"].as_array().is_some_and(|roots| {
        roots
            .iter()
            .filter_map(Value::as_str)
            .any(|root| filesystem::under(scope, root))
    });
    let mut view = json!({"phase":"needs-scan","scope":scope,"revision":doc["revision"],
        "deferred":saved_paths(doc,"deferred").len(),"manualPending":pending.len(),"scan":scan});
    if scan["status"] == "running" || s.index.running.load(Ordering::SeqCst) {
        view["phase"] = json!(if scoped { "scanning" } else { "other-scan" });
        return Ok(view);
    }
    if scoped && scan["status"] == "paused" {
        view["phase"] = json!("paused");
        return Ok(view);
    }
    if !scoped || scan["status"] != "complete" {
        return Ok(view);
    }
    let stamp = scan["finishedAt"]
        .as_str()
        .ok_or("Inventory completion time is missing")?;
    let monitoring_failed = s
        .watcher_fault
        .lock()
        .map(|fault| fault.is_some())
        .unwrap_or(true);
    if monitoring_failed
        || s.index.changed_below(scope, stamp)?
        || s.audit.changed_below(scope, stamp)?
    {
        view["scan"]["error"] = json!("Source observations changed or monitoring was interrupted after this inventory. Scan this folder again before continuing the interview.");
        return Ok(view);
    }
    let inventory = {
        let mut cache = s.assistance.0.lock().map_err(error)?;
        if cache
            .as_ref()
            .is_none_or(|cached| cached.stamp != stamp || cached.scope != scope)
        {
            *cache = Some(Arc::new(Inventory::load(s, scope, &scan, stamp)?));
        }
        cache
            .as_ref()
            .cloned()
            .ok_or("Inventory snapshot unavailable")?
    };
    let (question, unavailable) = pick_question(s, doc, &inventory)?;
    let question = question.or_else(|| pending_question(s, doc, &inventory, &pending));
    view["phase"] = json!(if question.is_some() {
        "question"
    } else {
        "complete"
    });
    view["question"] = question
        .map(|question| question.display(s, doc, scope, stamp))
        .unwrap_or(Value::Null);
    view["files"] = json!(inventory.directories[0].leaves);
    view["gaps"] = json!(inventory.gaps + unavailable);
    Ok(view)
}

pub fn validate_question(s: &AppState, doc: &Value, op: &Value) -> Result<Value, String> {
    let current = snapshot(s, doc)?;
    let question = current["question"].clone();
    if !question.is_object() || question["id"] != op["questionId"] || question["path"] != op["path"]
    {
        return Err(
            "The interview question changed. Read the current question before applying a decision."
                .into(),
        );
    }
    Ok(question)
}

pub fn update(s: &AppState, doc: &mut Value, op: &Value) -> Result<(), String> {
    match op["action"].as_str() {
        Some("scope") => {
            let path =
                filesystem::normalize(op["path"].as_str().ok_or("Missing interview scope")?)?;
            let (volume, _, _) = filesystem::identify(&path, &s.volumes)?;
            filesystem::safe(&path, &s.volumes, false)?;
            if !doc["assistance"]["scope"]
                .as_str()
                .is_some_and(|scope| scope.eq_ignore_ascii_case(&path))
                || doc["assistance"]["volumeId"] != volume.id
            {
                doc["assistance"] =
                    json!({"scope":path,"volumeId":volume.id,"split":[],"deferred":[]});
            }
        }
        Some("split" | "defer") => {
            let question = validate_question(s, doc, op)?;
            let action = op["action"].as_str().unwrap_or_default();
            if action == "split"
                && (question["kind"] != "folder" || question["manualConfirmation"] == true)
            {
                return Err("Use child decisions in the folder explorer to set manual backup exceptions before confirming this scope.".into());
            }
            let field = if action == "split" {
                "split"
            } else {
                "deferred"
            };
            let items = doc["assistance"][field]
                .as_array_mut()
                .ok_or("The interview state needs recovery")?;
            items.push(question["path"].clone());
        }
        Some("restore") => {
            if doc["assistance"]["scope"].is_string() {
                doc["assistance"]["deferred"] = json!([]);
            }
        }
        _ => return Err("Unknown assistance action".into()),
    }
    Ok(())
}

#[tauri::command]
pub async fn get_assisted_review(state: tauri::State<'_, Shared>) -> Result<Value, String> {
    let app = state.inner().clone();
    tauri::async_runtime::spawn_blocking(move || snapshot(&app, &document(&app)?))
        .await
        .map_err(error)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_observations_fence_prepared_questions_until_a_fresh_inventory() {
        let s = fixture();
        let leaf = write(&s, "Work\\Child\\change.txt");
        scope_and_inventory(&s, "Work");
        let question = current(&s)["question"].clone();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&leaf, "changed source").unwrap();
        s.audit.record_watch_change(&leaf, "modified").unwrap();
        let stale = current(&s);
        assert_eq!(stale["phase"], "needs-scan");
        assert!(stale["scan"]["error"]
            .as_str()
            .unwrap()
            .contains("Source observations"));
        assert!(edit(&s, "answer-old-source", answer(&question, "last-moment")).is_err());
    }

    #[test]
    fn question_identifier_changes_when_target_metadata_changes_without_an_observation() {
        let s = fixture();
        let leaf = write(&s, "Work\\Child\\change.txt");
        scope_and_inventory(&s, "Work\\Child");
        let folder = current(&s)["question"].clone();
        edit(&s, "split-folder", op(&folder, "split")).unwrap();
        let file = current(&s)["question"].clone();
        assert_eq!(file["kind"], "file");
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&leaf, "a larger changed source").unwrap();
        let refreshed = current(&s)["question"].clone();
        assert_ne!(file["id"], refreshed["id"]);
        assert!(edit(&s, "old-file-answer", answer(&file, "last-moment")).is_err());
    }

    #[test]
    fn pending_manual_backups_need_an_explicit_confirmation_and_survive_deferral() {
        let s = fixture();
        let leaf = write(&s, "Work\\Child\\keep.txt");
        let work = Path::new(&s.volumes[0].root)
            .join("Work")
            .to_string_lossy()
            .into_owned();
        edit(
            &s,
            "manual-intent",
            json!({"type":"classify","path":work,"status":"manual-pending"}),
        )
        .unwrap();
        scope_and_inventory(&s, "Work");
        let next = current(&s);
        assert_eq!(next["manualPending"], 1);
        assert_eq!(next["question"]["manualConfirmation"], true);
        assert_eq!(next["question"]["status"], "manual-confirmed");
        let question = next["question"].clone();
        edit(&s, "later", op(&question, "defer")).unwrap();
        let later = current(&s);
        assert_eq!(later["phase"], "complete");
        assert_eq!(later["manualPending"], 1);
        assert_eq!(later["deferred"], 1);
        edit(
            &s,
            "bring-back",
            json!({"type":"assist","action":"restore"}),
        )
        .unwrap();

        let revision = document(&s).unwrap()["revision"].as_u64().unwrap();
        run_mutation_as(
            &s,
            revision,
            "agent-confirmation-proposal".into(),
            json!({"type":"classify","path":work,"status":"manual-confirmed"}),
            MutationOrigin::Agent {
                basis: "assistant_suggestion".into(),
                reason: "Synthetic recommendation only".into(),
            },
        )
        .unwrap();
        let doc = document(&s).unwrap();
        let (_, _, key) = filesystem::locate(&work, &s.volumes).unwrap();
        assert_eq!(doc["decisions"][&key]["status"], "manual-pending");
        assert_eq!(current(&s)["question"]["id"], question["id"]);

        edit(
            &s,
            "user-confirms-manual",
            answer(&question, "manual-confirmed"),
        )
        .unwrap();
        let done = current(&s);
        assert_eq!(done["phase"], "question");
        assert_eq!(done["manualPending"], 0);
        assert_eq!(
            crate::details_service(&s, leaf).unwrap()["effective"]["status"],
            "attention"
        );
    }

    #[test]
    fn inherited_manual_intent_is_accounted_for_inside_a_narrower_interview_scope() {
        let s = fixture();
        write(&s, "Work\\Child\\keep.txt");
        let work = Path::new(&s.volumes[0].root)
            .join("Work")
            .to_string_lossy()
            .into_owned();
        edit(
            &s,
            "manual-parent-intent",
            json!({"type":"classify","path":work,"status":"manual-pending"}),
        )
        .unwrap();
        scope_and_inventory(&s, "Work\\Child");
        let next = current(&s);
        assert_eq!(next["manualPending"], 1);
        assert_eq!(next["question"]["manualConfirmation"], true);
        assert!(next["question"]["path"]
            .as_str()
            .unwrap()
            .ends_with("\\Child"));
    }
    use crate::{run_mutation, run_mutation_as, tests::fixture, MutationOrigin};
    use std::{
        fs, thread,
        time::{Duration, Instant},
    };

    fn edit(s: &AppState, id: &str, operation: Value) -> Result<Value, String> {
        let revision = document(s)?["revision"].as_u64().unwrap();
        run_mutation(s, revision, id.into(), operation)
    }

    fn current(s: &AppState) -> Value {
        snapshot(s, &document(s).unwrap()).unwrap()
    }

    fn op(question: &Value, action: &str) -> Value {
        json!({"type":"assist","action":action,"path":question["path"],"questionId":question["id"]})
    }

    fn answer(question: &Value, status: &str) -> Value {
        json!({"type":"assistAnswer","path":question["path"],"questionId":question["id"],"status":status})
    }

    fn write(s: &AppState, relative: &str) -> String {
        let path = Path::new(&s.volumes[0].root).join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "fixture bytes").unwrap();
        path.to_string_lossy().into_owned()
    }

    fn scope_and_inventory(s: &AppState, relative: &str) {
        let scope = Path::new(&s.volumes[0].root)
            .join(relative)
            .to_string_lossy()
            .into_owned();
        edit(
            s,
            "scope",
            json!({"type":"assist","action":"scope","path":scope}),
        )
        .unwrap();
        assert_eq!(current(s)["phase"], "needs-scan");
        s.index
            .start(vec![scope], s.volumes.clone(), false)
            .unwrap();
        let started = Instant::now();
        while s.index.running.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(15));
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(scan_state(s).unwrap()["status"], "complete");
    }

    #[test]
    fn preserve_nested_exception_and_require_an_explicit_override_for_redownload() {
        let s = fixture();
        let patch = write(&s, "Work\\Project\\node_modules\\patches\\unique.patch");
        write(&s, "Work\\Project\\node_modules\\Installed\\dep.js");
        scope_and_inventory(&s, "Work");
        let first = current(&s)["question"].clone();
        assert!(first["path"].as_str().unwrap().ends_with("\\patches"));
        assert_eq!(first["status"], "last-moment");
        edit(&s, "keep-patch", answer(&first, "last-moment")).unwrap();

        let cache = current(&s)["question"].clone();
        let cache_path = cache["path"].as_str().unwrap();
        assert!(cache_path.ends_with("\\node_modules"));
        let revision = document(&s).unwrap()["revision"].as_u64().unwrap();
        run_mutation_as(
            &s,
            revision,
            "codex-suggestion".into(),
            json!({"type":"classify","path":cache_path,"status":"redownload"}),
            MutationOrigin::Agent {
                basis: "assistant_suggestion".into(),
                reason: "Fixture metadata indicates an install recipe; verify local patches."
                    .into(),
            },
        )
        .unwrap();
        let refreshed = current(&s)["question"].clone();
        assert_eq!(refreshed["agentStatus"], "redownload");
        assert_eq!(refreshed["status"], "last-moment");
        assert_ne!(refreshed["id"], cache["id"]);
        assert!(edit(&s, "stale-answer", answer(&cache, "redownload")).is_err());

        let updated = edit(&s, "owner-confirmed", answer(&refreshed, "redownload")).unwrap();
        let entry = filesystem::inspect(Path::new(&patch));
        assert_eq!(
            crate::recorded_effective(&s, &updated, &entry)["status"],
            "last-moment"
        );
        assert_eq!(
            updated["history"].as_array().unwrap().last().unwrap()["assistanceQuestion"]["id"],
            refreshed["id"]
        );
        assert!(Path::new(&patch).is_file());
        let undone = edit(&s, "undo-cache", json!({"type":"undo"})).unwrap();
        let (_, _, key) = filesystem::locate(cache_path, &s.volumes).unwrap();
        assert!(undone["decisions"].get(key).is_none());
        assert_eq!(current(&s)["question"]["path"], cache_path);
    }

    #[test]
    fn deferred_exception_blocks_ancestor_grouping_and_can_be_restored() {
        let s = fixture();
        write(&s, "Work\\Project\\node_modules\\patches\\unique.patch");
        write(&s, "Work\\Project\\node_modules\\Installed\\dep.js");
        scope_and_inventory(&s, "Work");
        let first = current(&s)["question"].clone();
        edit(&s, "defer-patches", op(&first, "defer")).unwrap();
        let next = current(&s);
        assert_eq!(next["deferred"], 1);
        assert!(next["question"]["path"]
            .as_str()
            .unwrap()
            .ends_with("\\Installed"));
        edit(
            &s,
            "restore-deferred",
            json!({"type":"assist","action":"restore"}),
        )
        .unwrap();
        assert_eq!(current(&s)["question"]["id"], first["id"]);
    }

    #[test]
    fn split_reviews_files_and_does_not_group_an_inventory_gap() {
        let s = fixture();
        let loose = write(&s, "Work\\Project\\unique.txt");
        let hidden = write(&s, "Work\\Project\\Unknown\\hidden.txt");
        scope_and_inventory(&s, "Work");
        let db = s.index.connect().unwrap();
        let unknown = Path::new(&hidden).parent().unwrap().to_str().unwrap();
        db.execute(
            "UPDATE directories SET problem='synthetic inaccessible folder' WHERE path=?1",
            [unknown],
        )
        .unwrap();
        let view = current(&s);
        assert_eq!(view["gaps"], 1);
        let first = view["question"].clone();
        assert!(first["path"].as_str().unwrap().ends_with("\\Child"));
        edit(&s, "keep-child", answer(&first, "last-moment")).unwrap();
        assert_eq!(current(&s)["question"]["path"], loose);
        edit(
            &s,
            "keep-loose",
            answer(&current(&s)["question"], "migrate-now"),
        )
        .unwrap();
        let done = current(&s);
        assert_eq!(done["phase"], "complete");
        assert_eq!(done["gaps"], 1);
        assert_eq!(done["files"], 1);
    }

    #[test]
    fn choosing_review_children_removes_the_parent_question() {
        let s = fixture();
        let file = write(&s, "Work\\unique.txt");
        scope_and_inventory(&s, "Work");
        let first = current(&s)["question"].clone();
        assert!(first["path"].as_str().unwrap().ends_with("\\Work"));
        edit(&s, "split-scope", op(&first, "split")).unwrap();
        let child = current(&s)["question"].clone();
        assert!(child["path"].as_str().unwrap().ends_with("\\Child"));
        edit(&s, "keep-child", answer(&child, "last-moment")).unwrap();
        assert_eq!(current(&s)["question"]["path"], file);
    }
}
