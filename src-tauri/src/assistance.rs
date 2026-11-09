use crate::{filesystem, store::error, AppState, Shared};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{atomic::Ordering, Arc, Mutex},
};

#[derive(Default)]
pub struct Cache(Mutex<Option<Arc<Inventory>>>, Mutex<Option<(String, String)>>);

struct Inventory {
    scope: String,
    stamp: String,
    observed_after: String,
    directories: Vec<Directory>,
    gaps: usize,
    skipped_generated: usize,
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
        let observed_after = if is_early_stamp(stamp) {
            early_basis(&db, scope, &current)?.filter(|basis| basis.stamp == stamp)
                .map(|basis| basis.observed_after)
        } else if current["status"] == "complete" && current["finishedAt"] == stamp {
            Some(stamp.to_owned())
        } else {
            None
        }.ok_or("The inventory changed while preparing the interview. Refresh to continue.")?;
        let (low, high) = subtree_range(scope);
        let mut query = db
            .prepare(
                "SELECT q.path, coalesce(d.files,0), coalesce(d.bytes,0),
                    q.state='done' AND d.signature IS NOT NULL AND d.problem IS NULL,
                    q.state='skipped'
             FROM queue q LEFT JOIN directories d ON d.path=q.path
             WHERE q.path=?1 OR (q.path>=?2 AND q.path<?3)
             ORDER BY length(q.path), q.path COLLATE NOCASE",
            )
            .map_err(error)?;
        let rows = query
            .query_map(params![scope, low, high], |r| {
                let path: String = r.get(0)?;
                let readable: bool = r.get(3)?;
                Ok((Directory {
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
                }, r.get::<_, bool>(4)?))
            })
            .map_err(error)?;
        // A resumed inventory can have older observations beneath a newly skipped root.
        // Keep those observations in storage but exclude them from this interview.
        let mut hidden = HashSet::new();
        let mut directories = Vec::new();
        let mut skipped_generated = 0;
        for row in rows {
            let (directory, skipped) = row.map_err(error)?;
            let beneath_skip = parent(&directory.path).is_some_and(|path| hidden.contains(&path));
            if beneath_skip || skipped {
                hidden.insert(directory.path.to_lowercase());
            }
            if !beneath_skip {
                directories.push(directory);
                skipped_generated += usize::from(skipped);
            }
        }
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
             AND observed_at>=?1 AND path>=?2 AND path<?3",
            )
            .map_err(error)?;
        for row in gaps
            .query_map(
                params![scan["startedAt"].as_str().unwrap_or(""), low, high],
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
            observed_after,
            directories,
            gaps: gap_paths.len(),
            skipped_generated,
        })
    }
}

const EARLY_DIRECTORY_LIMIT: usize = 256;
const EARLY_FILE_LIMIT: u64 = 25_000;

fn subtree_range(scope: &str) -> (String, String) {
    let scope = scope.trim_end_matches('\\');
    (format!("{scope}\\"), format!("{scope}]"))
}

fn scan_contains(scan: &Value, scope: &str) -> bool {
    scan["roots"].as_array().into_iter().flatten().filter_map(Value::as_str)
        .any(|root| filesystem::under(scope, root))
}

struct EarlyBasis {
    stamp: String,
    observed_after: String,
}

pub(crate) fn is_early_stamp(stamp: &str) -> bool { stamp.starts_with("branch:") }

fn early_basis(db: &Connection, path: &str, scan: &Value) -> Result<Option<EarlyBasis>, String> {
    let Some(started) = scan["startedAt"].as_str() else { return Ok(None) };
    if !matches!(scan["status"].as_str(), Some("running" | "paused" | "complete"))
        || !scan_contains(scan, path)
    {
        return Ok(None);
    }
    let mut ancestor = parent(path);
    while let Some(candidate) = ancestor {
        let skipped = db.query_row("SELECT EXISTS(SELECT 1 FROM queue WHERE path=?1 AND state='skipped')",
            [&candidate], |row| row.get::<_, bool>(0)).map_err(error)?;
        if skipped { return Ok(None); }
        ancestor = parent(&candidate);
    }
    let (low, high) = subtree_range(path);
    let mut rows = db.prepare(
        "SELECT q.path, q.state, d.signature, d.problem, d.observed_at, d.files, h.generation
         FROM queue q LEFT JOIN directories d ON d.path=q.path
         LEFT JOIN observation_heads h ON h.parent=lower(q.path)
         WHERE q.path IN (
             SELECT path FROM queue WHERE path=?1
             UNION ALL SELECT path FROM (SELECT path FROM queue WHERE path>=?2 AND path<?3 ORDER BY path LIMIT ?4)
         ) ORDER BY q.path"
    ).map_err(error)?;
    let mut rows = rows.query(params![path, low, high, (EARLY_DIRECTORY_LIMIT + 1) as i64]).map_err(error)?;
    let mut missing = db.prepare(
        "SELECT EXISTS(SELECT 1 FROM directory_children c LEFT JOIN queue q ON q.path=c.path
         WHERE c.parent=?1 AND c.generation=?2 AND (q.state IS NULL OR q.state<>'done'))"
    ).map_err(error)?;
    let mut observations = Vec::new();
    let mut observed_after = None::<String>;
    let mut files = 0u64;
    let mut saw_root = false;
    while let Some(row) = rows.next().map_err(error)? {
        let dir: String = row.get(0).map_err(error)?;
        let state: String = row.get(1).map_err(error)?;
        let signature: Option<String> = row.get(2).map_err(error)?;
        let problem: Option<String> = row.get(3).map_err(error)?;
        let observed: Option<String> = row.get(4).map_err(error)?;
        let leaves: Option<i64> = row.get(5).map_err(error)?;
        let generation: Option<String> = row.get(6).map_err(error)?;
        let (Some(signature), Some(observed), Some(leaves), Some(generation)) =
            (signature, observed, leaves, generation) else { return Ok(None) };
        let Ok(leaves) = u64::try_from(leaves) else { return Ok(None) };
        files = files.saturating_add(leaves);
        if state != "done" || problem.is_some() || observed.as_str() < started
            || observations.len() == EARLY_DIRECTORY_LIMIT || files > EARLY_FILE_LIMIT
            || missing.query_row(params![dir, generation], |row| row.get::<_, bool>(0)).map_err(error)?
        {
            return Ok(None);
        }
        saw_root |= dir.eq_ignore_ascii_case(path);
        if observed_after.as_ref().is_none_or(|first| &observed < first) {
            observed_after = Some(observed);
        }
        observations.push((dir, signature));
    }
    if !saw_root { return Ok(None); }
    let evidence = serde_json::to_vec(&(started, observations)).map_err(error)?;
    Ok(observed_after.map(|observed_after| EarlyBasis {
        stamp: format!("branch:{}", crate::store::hash(&evidence)),
        observed_after,
    }))
}

fn current_early_basis(s: &AppState, path: &str) -> Result<Option<EarlyBasis>, String> {
    let db = s.index.connect()?;
    let db = db.unchecked_transaction().map_err(error)?;
    let scan: String = db.query_row("SELECT value FROM meta WHERE key='scan'", [], |row| row.get(0)).map_err(error)?;
    let scan = serde_json::from_str(&scan).map_err(error)?;
    let Some(basis) = early_basis(&db, path, &scan)? else { return Ok(None) };
    if s.watcher_fault.lock().map_err(error)?.is_some()
        || s.audit.watch_changed(path, &basis.observed_after)?
    {
        return Ok(None);
    }
    Ok(Some(basis))
}

pub(crate) fn early_branch_current(s: &AppState, path: &str, stamp: &str) -> Result<bool, String> {
    Ok(current_early_basis(s, path)?.is_some_and(|basis| basis.stamp == stamp))
}

fn early_children(s: &AppState, scope: &str) -> Result<Vec<String>, String> {
    let db = s.index.connect()?;
    let mut query = db.prepare(
        "SELECT q.path FROM directory_children c JOIN observation_heads h ON h.parent=lower(c.parent)
         JOIN queue q ON q.path=c.path WHERE c.parent=?1 AND c.generation=h.generation AND q.state='done'
         ORDER BY q.path LIMIT 128"
    ).map_err(error)?;
    let rows = query.query_map([scope], |row| row.get::<_, String>(0)).map_err(error)?;
    rows.collect::<Result<_, _>>().map_err(error)
}

fn early_questions(s: &AppState, doc: &Value, scope: &str, scan: &Value,
    slate: &mut Vec<Value>, limit: usize, view: &mut Value) -> Result<(), String> {
    let pinned = s.assistance.1.lock().map_err(error)?.clone();
    let mut candidates = vec![scope.to_owned()];
    candidates.extend(early_children(s, scope)?);
    if let Some((owner, path)) = pinned {
        if owner == scope && filesystem::under(&path, scope) {
            candidates.retain(|candidate| !candidate.eq_ignore_ascii_case(&path));
            candidates.insert(0, path);
        }
    }
    let confirmations = crate::manual_backup::ConfirmationCache::default();
    let overrides = saved_paths(doc, "split");
    for path in candidates {
        if crate::adaptive_review::active(doc)
            && s.runtime.to_str().is_some_and(|runtime| filesystem::under(runtime, &path))
        {
            continue;
        }
        if resolved(s, doc, &path, &confirmations) && !overrides.contains(&path.to_lowercase()) {
            continue;
        }
        let Some(basis) = current_early_basis(s, &path)? else { continue };
        let inventory = Inventory::load(s, &path, scan, &basis.stamp)?;
        if inventory.directories[0].gap_below || inventory.gaps != 0 { continue; }
        let (questions, unavailable) = pick_question(s, doc, &inventory, limit)?;
        if questions.is_empty() || unavailable != 0 || !early_branch_current(s, &path, &basis.stamp)? {
            continue;
        }
        slate.extend(questions.into_iter().map(|question| {
            let mut displayed = question.display(s, doc, scope, &basis.stamp, &inventory.observed_after);
            displayed["earlyBranch"] = json!(path);
            displayed
        }));
        *s.assistance.1.lock().map_err(error)? = Some((scope.to_owned(), path.clone()));
        view["phase"] = json!("question");
        view["question"] = slate[0].clone();
        view["earlyBranch"] = json!(path);
        view["earlyStamp"] = json!(basis.stamp);
        view["files"] = json!(inventory.directories[0].leaves);
        view["skippedGenerated"] = json!(0);
        break;
    }
    Ok(())
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

    fn display(self, s: &AppState, doc: &Value, scope: &str, stamp: &str, observed_after: &str) -> Value {
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
                        .is_some_and(|date| date >= observed_after)
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
        let child_exceptions = doc["decisions"].as_object().into_iter().flat_map(|marks| marks.values())
            .filter(|mark| mark["noteOnly"] != true && mark["path"].as_str().is_some_and(|path|
                !path.eq_ignore_ascii_case(&self.path) && filesystem::under(path, &self.path))).count();
        json!({"id":id,"path":self.path,"kind":self.kind,"files":self.files,"bytes":self.bytes,
            "prompt":prompt,"status":proposed,"agentStatus":agent_status,"reason":evidence,
            "source":if suggestion.is_some() {"codex"} else {"local"},"manualConfirmation":self.manual_confirmation,
            "protected":self.protected,"rebuildableHint":self.reusable,"exceptions":child_exceptions,"coverageGaps":0,
            "sourceFingerprint":source_fingerprint})
    }
}

fn pick_question(
    s: &AppState,
    doc: &Value,
    inventory: &Inventory,
    limit: usize,
) -> Result<(Vec<Question>, usize), String> {
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
    let choose = |mut ids: Vec<usize>| -> Vec<Question> {
        ids.sort_unstable_by(|&left, &right| {
            let a = &inventory.directories[left];
            let b = &inventory.directories[right];
            unresolved[right].cmp(&unresolved[left]).then(b.bytes.cmp(&a.bytes))
                .then_with(|| a.path.cmp(&b.path))
        });
        ids.into_iter().take(limit).map(|id| Question::directory(&inventory.directories[id])).collect()
    };
    let branch = if !protected_exceptions.is_empty() { choose(protected_exceptions) }
        else if !nested_exceptions.is_empty() { choose(nested_exceptions) }
        else { choose(candidates) };
    if !branch.is_empty() {
        return Ok((branch, 0));
    }
    let db = s.index.connect()?;
    let mut unavailable = 0;
    let mut choices = Vec::new();
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
        if observed.is_none_or(|time| time.as_str() < inventory.observed_after.as_str())
            && s.index.refresh(&dir.path, &s.volumes, None, true).is_err()
        {
            unavailable += 1;
            continue;
        }
        s.index.each_entry(&dir.path, "", |entry| {
            if entry.kind == "file"
                && entry.problem.is_none()
                && !deferred.contains(&entry.path.to_lowercase())
                && !resolved(s, doc, &entry.path, &confirmations)
            {
                choices.push(Question {
                    path: entry.path,
                    kind: "file",
                    files: 1,
                    bytes: entry.size,
                    reusable: false,
                    protected: false,
                    manual_confirmation: false,
                    manual_basis: None,
                });
                return choices.len() < limit;
            }
            true
        })?;
        if choices.len() == limit {
            break;
        }
    }
    Ok((choices, unavailable))
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
                    let path = path.to_string_lossy();
                    filesystem::under(&path, scope) && !(s.mirrors.verifies_manual_scope(doc, &path, volume, s.fixture)
                        && filesystem::guard_path(&path, &s.volumes, false).is_ok_and(|(path, _guards)| {
                            let entry = filesystem::inspect(&path);
                            matches!(entry.kind.as_str(), "folder" | "file") && entry.problem.is_none()
                        }))
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

fn snapshot_inner(s: &AppState, doc: &Value, slate: &mut Vec<Value>, limit: usize) -> Result<Value, String> {
    let Some(scope) = doc["assistance"]["scope"].as_str() else {
        return Ok(json!({"phase":"idle","revision":doc["revision"],"scan":scan_state(s)?}));
    };
    let (volume, _, _) = filesystem::identify(scope, &s.volumes)?;
    if doc["assistance"]["volumeId"] != volume.id {
        return Err("The interview scope is on a different volume. Choose a new scope.".into());
    }
    let pending = pending_manual(s, doc, scope, volume);
    let scan = scan_state(s)?;
    let scoped = scan_contains(&scan, scope);
    let mut view = json!({"phase":"needs-scan","scope":scope,"revision":doc["revision"],
        "deferred":saved_paths(doc,"deferred").len(),"manualPending":pending.len(),"scan":scan});
    if scan["status"] == "running" || s.index.running.load(Ordering::SeqCst) {
        view["phase"] = json!(if scoped { "scanning" } else { "other-scan" });
        if scoped { early_questions(s, doc, scope, &scan, slate, limit, &mut view)?; }
        return Ok(view);
    }
    if scoped && scan["status"] == "paused" {
        view["phase"] = json!("paused");
        early_questions(s, doc, scope, &scan, slate, limit, &mut view)?;
        return Ok(view);
    }
    if !scoped || scan["status"] != "complete" {
        return Ok(view);
    }
    if crate::adaptive_review::active(doc) && scan["skippedGenerated"].as_u64().unwrap_or(0) > 0 {
        early_questions(s, doc, scope, &scan, slate, limit, &mut view)?;
        if !slate.is_empty() { return Ok(view); }
    }
    if scan["skippedGenerated"].as_u64().unwrap_or(0) > 0 && s.index.skipped_ancestor(scope)? {
        view["scan"]["error"] = json!("This folder was skipped as generated. Scan including generated folders to review it.");
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
    let (mut questions, unavailable) = pick_question(s, doc, &inventory, limit)?;
    if questions.is_empty() {
        questions.extend(pending_question(s, doc, &inventory, &pending));
    }
    slate.extend(questions.into_iter().map(|question| question.display(s, doc, scope, stamp, stamp)));
    view["phase"] = json!(if !slate.is_empty() {
        "question"
    } else {
        "complete"
    });
    view["question"] = slate.first().cloned().unwrap_or(Value::Null);
    view["files"] = json!(inventory.directories[0].leaves);
    view["gaps"] = json!(inventory.gaps + unavailable);
    view["skippedGenerated"] = json!(inventory.skipped_generated);
    Ok(view)
}

pub(crate) fn local_slate(s: &AppState, doc: &Value, limit: usize) -> Result<(Value,Vec<Value>), String> {
    let mut slate = Vec::new();
    let view = snapshot_inner(s, doc, &mut slate, limit.clamp(1,12))?;
    Ok((view,slate))
}

pub fn snapshot(s: &AppState, doc: &Value) -> Result<Value, String> {
    let (mut view, _) = local_slate(s, doc, 1)?;
    view["deferMarked"] = json!(doc["inventoryDeferMarked"] != false);
    crate::adaptive_review::decorate(s, doc, &mut view)?;
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
    if question["adaptive"] == true {
        crate::adaptive_review::validate_saved_question(s, doc, op)?;
    }
    if question["earlyBranch"].is_string() && op["type"] == "assistAnswer"
        && matches!(op["status"].as_str(), Some("redownload" | "erase" | "on-external" | "manual-confirmed"))
    {
        return Err("Use a keep or manual backup decision for a question offered during inventory.".into());
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
            crate::adaptive_review::scope_changed(doc, &path);
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
            crate::adaptive_review::record_answer(doc, &question, action)?;
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
    tauri::async_runtime::spawn_blocking(move || crate::assisted_review_service(&app))
        .await
        .map_err(error)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document;

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
        run_inventory(s, scope, true);
    }

    fn run_inventory(s: &AppState, scope: String, include_generated: bool) {
        s.index.start(vec![scope], s.volumes.clone(), false, include_generated).unwrap();
        let started = Instant::now();
        while s.index.running.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(15));
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(scan_state(s).unwrap()["status"], "complete");
    }

    #[test]
    fn completed_branch_is_reviewable_while_another_branch_is_active() {
        let s = fixture();
        write(&s, r"Ready\keep.txt");
        write(&s, r"Work\Child\pending.txt");
        let root = &s.volumes[0].root;
        let ready = format!("{root}\\Ready");
        let work = format!("{root}\\Work");
        edit(&s, "early-scope", json!({"type":"assist","action":"scope","path":root})).unwrap();
        run_inventory(&s, root.clone(), false);
        let db = s.index.connect().unwrap();
        db.execute("UPDATE queue SET state='working' WHERE path=?1", [&work]).unwrap();
        db.execute("UPDATE meta SET value=json_set(value,'$.status','running','$.finishedAt',null) WHERE key='scan'", []).unwrap();
        let first = current(&s);
        assert_eq!(first["scan"]["status"], "running");
        assert_eq!(first["phase"], "question", "{first}");
        assert_eq!(first["question"]["path"], ready);
        assert!(edit(&s, "unsafe-early-answer", answer(&first["question"], "erase")).is_err());

        db.execute("UPDATE queue SET state='done' WHERE path=?1", [&work]).unwrap();
        assert_eq!(current(&s)["question"]["id"], first["question"]["id"]);
        thread::sleep(Duration::from_millis(3));
        s.audit.record_watch_change(&format!("{ready}\\keep.txt"), "modified").unwrap();
        assert!(edit(&s, "stale-early-answer", answer(&first["question"], "last-moment")).is_err());
        assert!(document(&s).unwrap()["decisions"].as_object().unwrap().is_empty());
    }

    #[test]
    fn inaccessible_mark_is_rejected_before_save_without_changing_other_marks() {
        let s = fixture();
        let missing = format!("{}\\Missing", s.volumes[0].root);
        let work = format!("{}\\Work", s.volumes[0].root);
        let before = document(&s).unwrap();
        let error = edit(&s, "missing-mark", json!({"type":"classify","path":missing,"status":"last-moment"})).unwrap_err();
        assert!(error.starts_with("Mark rejected before save: "), "{error}");
        assert_eq!(document(&s).unwrap(), before);
        let saved = edit(&s, "reachable-mark", json!({"type":"classify","path":work,"status":"last-moment"})).unwrap();
        assert_eq!(saved["decisions"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn default_inventory_keeps_generated_branches_out_of_questions_until_requested() {
        let s = fixture();
        for file in [
            r"Work\node_modules\dependency\package.js",
            r"Work\build\handwritten.txt",
            r"Work\app\build\CMakeCache.txt",
            r"Work\app\build\CMakeFiles\generated.txt",
            r"Work\app\target\CACHEDIR.TAG",
            r"Work\app\target\.rustc_info.json",
            r"Work\app\.next\chunk.js",
        ] {
            write(&s, file);
        }
        let scope = Path::new(&s.volumes[0].root).join("Work").to_string_lossy().into_owned();
        edit(&s, "filtered-scope", json!({"type":"assist","action":"scope","path":scope})).unwrap();
        run_inventory(&s, scope.clone(), false);
        let filtered = current(&s);
        assert_eq!(filtered["scan"]["skippedGenerated"], 4);
        assert_eq!(filtered["skippedGenerated"], 4);
        assert_eq!(filtered["gaps"], 4);
        assert_eq!(filtered["files"], 1);
        let handwritten = Path::new(&scope).join("build").to_string_lossy().into_owned();
        assert_eq!(filtered["question"]["path"], handwritten);

        let generated = Path::new(&scope).join("node_modules").join("dependency")
            .to_string_lossy().into_owned();
        edit(&s, "generated-scope", json!({"type":"assist","action":"scope","path":generated})).unwrap();
        assert_eq!(current(&s)["phase"], "needs-scan");
        run_inventory(&s, generated, true);
        let included = current(&s);
        assert_eq!(included["scan"]["includeGenerated"], true);
        assert_eq!(included["skippedGenerated"], 0);
        assert_eq!(included["files"], 1);
        assert_eq!(included["phase"], "question");
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
