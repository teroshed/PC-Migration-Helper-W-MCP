//! Derived review summaries over atomically observed directory membership.
//! No summary is a saved mark, copy authority, or proof of backup contents.
use crate::{
    filesystem::{self, Entry},
    index,
    store::error,
    AppState,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Default, Serialize, Deserialize)]
struct Counts {
    children: u64,
    leaves: u64,
    empty_folders: u64,
    undecided: u64,
    unknown_branches: u64,
    gaps: u64,
    statuses: BTreeMap<String, u64>,
    oldest: Option<String>,
}

impl Counts {
    fn unknown(gap: bool) -> Self {
        Self {
            unknown_branches: 1,
            gaps: u64::from(gap),
            ..Self::default()
        }
    }

    fn include(&mut self, other: Self) {
        self.leaves = self.leaves.saturating_add(other.leaves);
        self.empty_folders = self.empty_folders.saturating_add(other.empty_folders);
        self.undecided = self.undecided.saturating_add(other.undecided);
        self.unknown_branches = self.unknown_branches.saturating_add(other.unknown_branches);
        self.gaps = self.gaps.saturating_add(other.gaps);
        for (status, count) in other.statuses {
            let prior = self.statuses.entry(status).or_default();
            *prior = prior.saturating_add(count);
        }
        if let Some(at) = other.oldest {
            if self.oldest.as_ref().is_none_or(|current| at < *current) {
                self.oldest = Some(at);
            }
        }
    }

    fn mark(&mut self, status: Option<&str>) {
        match status {
            Some(status) => {
                let count = self.statuses.entry(status.into()).or_default();
                *count = count.saturating_add(1);
            }
            None => self.undecided = self.undecided.saturating_add(1),
        }
    }

    fn view(&self, reason: Option<&str>) -> Value {
        let coverage = self.unknown_branches == 0 && self.gaps == 0 && reason.is_none();
        let fully_marked = coverage && self.undecided == 0 && !self.statuses.is_empty();
        let kind = if !fully_marked {
            "unknown"
        } else if self.statuses.len() == 1 {
            "unanimous"
        } else {
            "mixed"
        };
        let mut view = json!({
            "kind":kind,"childCount":self.children,"leafCount":self.leaves,
            "emptyFolderCount":self.empty_folders,"undecidedCount":self.undecided,
            "unknownBranches":self.unknown_branches,"coverageGaps":self.gaps,"statusCounts":self.statuses,
            "coverageComplete":coverage,"observedAt":self.oldest,"problem":reason,
            "scope":"Observed descendant entries; derived review intent only, not backup verification or copy authority"
        });
        if kind == "unanimous" {
            view["status"] = json!(self.statuses.keys().next());
        }
        view
    }
}

fn known_status(value: &Value) -> Option<&'static str> {
    match value.as_str()? {
        "migrate-now" => Some("migrate-now"),
        "last-moment" => Some("last-moment"),
        "redownload" => Some("redownload"),
        "erase" => Some("erase"),
        "on-external" => Some("on-external"),
        "manual-pending" => Some("manual-pending"),
        "manual-confirmed" => Some("manual-confirmed"),
        _ => None,
    }
}

fn selected_assertion(record: &Value) -> bool {
    matches!(
        record["status"].as_str(),
        Some("on-external" | "manual-confirmed")
    )
}

struct Head {
    generation: String,
    fingerprint: String,
    observed_at: String,
}

/// Prepared once per native listing/details request, so a wide page does not
/// repeatedly scan all saved decisions. Invalidation keys omit view/note edits.
pub struct Context<'a> {
    state: &'a AppState,
    db: Option<Connection>,
    marks: HashMap<String, &'a Value>,
    hashes: HashMap<String, String>,
    marked_ancestors: HashSet<String>,
    assertion_ancestors: HashSet<String>,
    audit_sequences: (i64, i64),
}

impl<'a> Context<'a> {
    pub fn new(state: &'a AppState, doc: &'a Value) -> Result<Self, String> {
        let mut marks = HashMap::new();
        let mut digests: HashMap<String, Sha256> = HashMap::new();
        let mut marked_ancestors = HashSet::new();
        let mut assertion_ancestors = HashSet::new();
        if let Some(decisions) = doc["decisions"].as_object() {
            for (key, record) in decisions {
                if record["noteOnly"] == true {
                    continue;
                }
                let Some((volume_id, relative)) = key.split_once('|') else {
                    continue;
                };
                let Some(volume) = state.volumes.iter().find(|v| v.id == volume_id) else {
                    continue;
                };
                let raw = Path::new(&volume.root).join(relative);
                let Ok(path) = filesystem::normalize(&raw.to_string_lossy()) else {
                    continue;
                };
                if !filesystem::under(&path, &volume.root) {
                    continue;
                }
                let folded = path.to_lowercase();
                marks.insert(folded.clone(), record);
                let semantic = serde_json::to_vec(&json!([
                    key,
                    record["status"],
                    record["decidedAt"],
                    record["snapshot"]["fingerprint"]
                ]))
                .map_err(error)?;
                for (distance, ancestor) in Path::new(&folded)
                    .ancestors()
                    .take_while(|p| p.has_root())
                    .enumerate()
                {
                    let name = ancestor.to_string_lossy().into_owned();
                    digests.entry(name.clone()).or_default().update(&semantic);
                    if distance > 0 && known_status(&record["status"]).is_some() {
                        marked_ancestors.insert(name.clone());
                    }
                    if selected_assertion(record) {
                        assertion_ancestors.insert(name);
                    }
                }
            }
        }
        let hashes = digests
            .into_iter()
            .map(|(path, digest)| (path, format!("{:x}", digest.finalize())))
            .collect();
        let (db, audit_sequences) = if marked_ancestors.is_empty() {
            (None, (0, 0))
        } else {
            let db = state.index.connect()?;
            // These rows are disposable derived cache, never a durable source checkpoint.
            db.execute_batch("PRAGMA synchronous=NORMAL;
                CREATE TEMP TABLE rollup_work(path TEXT PRIMARY KEY, source_path TEXT NOT NULL, parent TEXT, depth INTEGER NOT NULL,
                    phase INTEGER NOT NULL DEFAULT 0, counts TEXT, epoch TEXT);
                CREATE INDEX temp.rollup_work_depth ON rollup_work(depth DESC,path);").map_err(error)?;
            (Some(db), state.audit.evidence_sequences()?)
        };
        Ok(Self {
            state,
            db,
            marks,
            hashes,
            marked_ancestors,
            assertion_ancestors,
            audit_sequences,
        })
    }

    fn nearest(&self, folded: &str) -> Option<&'a Value> {
        Path::new(folded)
            .ancestors()
            .take_while(|p| p.has_root())
            .find_map(|p| self.marks.get(p.to_string_lossy().as_ref()).copied())
    }

    fn relevant_audit_sequence(&self, path: &str, pair: (i64, i64)) -> i64 {
        if self.assertion_ancestors.contains(path) {
            pair.0
        } else {
            pair.1
        }
    }

    fn epoch(&self, db: &Connection, folded: &str) -> Result<String, String> {
        let observed = index::observation_version(db, folded)?;
        let ancestor = Path::new(folded)
            .parent()
            .and_then(|path| self.nearest(&path.to_string_lossy()));
        // Scope-local marks and the closest inherited barrier are sufficient.
        // An unrelated sibling's note, mark or observation does not churn this key.
        let inherited = ancestor
            .map(|r| json!([r["status"], r["decidedAt"], r["snapshot"]["fingerprint"]]))
            .unwrap_or(Value::Null);
        Ok(format!(
            "{}|{observed}|{}|{inherited}",
            self.hashes.get(folded).map(String::as_str).unwrap_or(""),
            self.relevant_audit_sequence(folded, self.audit_sequences)
        ))
    }

    fn current_head(
        &self,
        db: &Connection,
        folded: &str,
        requested: &str,
    ) -> Result<Option<Head>, String> {
        let row: Option<(String,String,String,String,String,Option<String>)> = db.query_row(
            "SELECT source_path,generation,fingerprint,identity,observed_at,problem FROM observation_heads WHERE parent=?1",
            [folded], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)),
        ).optional().map_err(error)?;
        let Some((source, generation, fingerprint, identity, observed_at, problem)) = row else {
            return Ok(None);
        };
        if let Some(problem) = problem {
            return Err(problem);
        }
        if source.to_lowercase() != folded || requested.to_lowercase() != folded {
            return Err("Observed directory identity is ambiguous".into());
        }
        // Resolve the actual requested spelling. A case-sensitive directory can
        // otherwise reuse the folded key of a different, still-existing sibling.
        let (path, guards) = filesystem::guard_path(requested, &self.state.volumes, false)?;
        let current = filesystem::inspect(&path);
        if current.kind != "folder"
            || current.problem.is_some()
            || current.fingerprint != fingerprint
            || crate::audit::handle_identity(
                guards
                    .last()
                    .ok_or("Observed directory handle unavailable")?,
            )? != identity
        {
            return Err("Directory membership is stale; refresh its metadata observation".into());
        }
        Ok(Some(Head {
            generation,
            fingerprint,
            observed_at,
        }))
    }

    fn cached(&self, db: &Connection, folded: &str, epoch: &str) -> Result<Option<Counts>, String> {
        let row: Option<String> = db
            .query_row(
                "SELECT counts FROM derived_status_summaries WHERE path=?1 AND epoch=?2",
                params![folded, epoch],
                |r| r.get(0),
            )
            .optional()
            .map_err(error)?;
        row.map(|raw| serde_json::from_str(&raw).map_err(error))
            .transpose()
    }

    fn choose(
        &self,
        path: &Path,
        kind: &str,
        fingerprint: &str,
        inherited: Option<&Value>,
        effective: &impl Fn(&Entry) -> Value,
    ) -> Option<&'static str> {
        let folded = path.to_string_lossy().to_lowercase();
        let own = self.marks.get(&folded).copied();
        let record = own.or(inherited)?;
        if selected_assertion(record) {
            // Selected-path assertions never cover a descendant by inheritance.
            own?;
            let live = filesystem::inspect(path);
            if live.problem.is_some() || live.kind != kind || live.fingerprint != fingerprint {
                return None;
            }
            return known_status(&effective(&live)["status"]);
        }
        known_status(&record["status"])
    }

    fn direct(
        &self,
        db: &Connection,
        folded: &str,
        path: &str,
        depth: i64,
        effective: &impl Fn(&Entry) -> Value,
    ) -> Result<Counts, String> {
        let head = match self.current_head(db, folded, path) {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(Counts::unknown(false)),
            Err(_) => return Ok(Counts::unknown(true)),
        };
        let mut counts = Counts {
            oldest: Some(head.observed_at),
            ..Counts::default()
        };
        let ambiguous:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM observation_members WHERE parent=?1 AND generation=?2 GROUP BY folded_name HAVING count(*)>1)",
            params![folded,head.generation],|r|r.get(0)).map_err(error)?;
        if ambiguous {
            counts.include(Counts::unknown(true));
            return Ok(counts);
        }
        let inherited = self.nearest(folded);
        let mut members=db.prepare_cached("SELECT name,kind,fingerprint,problem FROM observation_members WHERE parent=?1 AND generation=?2 ORDER BY name").map_err(error)?;
        let mut rows = members
            .query(params![folded, head.generation])
            .map_err(error)?;
        let mut enqueue = db
            .prepare_cached(
                "INSERT INTO rollup_work(path,source_path,parent,depth) VALUES(?1,?2,?3,?4)",
            )
            .map_err(error)?;
        while let Some(row) = rows.next().map_err(error)? {
            let name: String = row.get(0).map_err(error)?;
            let kind: String = row.get(1).map_err(error)?;
            let fingerprint: String = row.get(2).map_err(error)?;
            let problem: Option<String> = row.get(3).map_err(error)?;
            counts.children = counts.children.saturating_add(1);
            if problem.is_some()
                || name.is_empty()
                || matches!(name.as_str(), "." | "..")
                || name.contains(['\\', '/', ':', '\0'])
            {
                counts.include(Counts::unknown(true));
                continue;
            }
            let child = PathBuf::from(path).join(name);
            match kind.as_str() {
                "file" => {
                    counts.leaves = counts.leaves.saturating_add(1);
                    counts.mark(self.choose(&child, "file", &fingerprint, inherited, effective));
                }
                "folder" => {
                    let child_path = child.to_string_lossy();
                    enqueue
                        .execute(params![
                            child_path.to_lowercase(),
                            child_path,
                            folded,
                            depth + 1
                        ])
                        .map_err(error)?;
                }
                _ => counts.include(Counts::unknown(true)),
            }
        }
        if counts.children == 0 {
            counts.empty_folders = 1;
            counts.mark(self.choose(
                Path::new(path),
                "folder",
                &head.fingerprint,
                inherited,
                effective,
            ));
        }
        Ok(counts)
    }

    /// Deepest-first disk-backed work frames combine children into parents.
    /// At most one directory's streaming rows and small histograms are resident.
    fn build(
        &self,
        db: &Connection,
        path: &str,
        effective: &impl Fn(&Entry) -> Value,
    ) -> Result<Counts, String> {
        db.execute("DELETE FROM rollup_work", []).map_err(error)?;
        db.execute(
            "INSERT INTO rollup_work(path,source_path,parent,depth) VALUES(?1,?2,NULL,0)",
            params![path.to_lowercase(), path],
        )
        .map_err(error)?;
        loop {
            let next:(String,String,Option<String>,i64,i64,Option<String>,Option<String>)=db.query_row(
                "SELECT path,source_path,parent,depth,phase,counts,epoch FROM rollup_work ORDER BY depth DESC,path LIMIT 1",
                [],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).map_err(error)?;
            let (folded, source, parent, depth, phase, raw, saved_epoch) = next;
            if phase == 0 {
                let epoch = self.epoch(db, &folded)?;
                let cached = self.cached(db, &folded, &epoch)?;
                let counts = match cached {
                    Some(counts)
                        if self
                            .current_head(db, &folded, &source)
                            .is_ok_and(|head| head.is_some()) =>
                    {
                        counts
                    }
                    _ => self.direct(db, &folded, &source, depth, effective)?,
                };
                db.execute(
                    "UPDATE rollup_work SET phase=1,counts=?2,epoch=?3 WHERE path=?1",
                    params![
                        folded,
                        serde_json::to_string(&counts).map_err(error)?,
                        epoch
                    ],
                )
                .map_err(error)?;
                continue;
            }
            let mut counts: Counts =
                serde_json::from_str(raw.as_deref().ok_or("Rollup counts unavailable")?)
                    .map_err(error)?;
            let epoch = saved_epoch.ok_or("Rollup observation epoch unavailable")?;
            if self.epoch(db, &folded)? != epoch {
                counts.include(Counts::unknown(false));
            } else {
                db.execute(
                    "INSERT OR REPLACE INTO derived_status_summaries VALUES(?1,?2,?3)",
                    params![
                        folded,
                        epoch,
                        serde_json::to_string(&counts).map_err(error)?
                    ],
                )
                .map_err(error)?;
            }
            db.execute("DELETE FROM rollup_work WHERE path=?1", [&folded])
                .map_err(error)?;
            if let Some(parent) = parent {
                let raw: String = db
                    .query_row(
                        "SELECT counts FROM rollup_work WHERE path=?1",
                        [&parent],
                        |r| r.get(0),
                    )
                    .map_err(error)?;
                let mut combined: Counts = serde_json::from_str(&raw).map_err(error)?;
                combined.include(counts);
                db.execute(
                    "UPDATE rollup_work SET counts=?2 WHERE path=?1",
                    params![parent, serde_json::to_string(&combined).map_err(error)?],
                )
                .map_err(error)?;
            } else {
                return Ok(counts);
            }
        }
    }

    pub fn derive(
        &self,
        entry: &Entry,
        base: &Value,
        effective: impl Fn(&Entry) -> Value,
    ) -> Option<Value> {
        let folded = entry.path.to_lowercase();
        if entry.kind != "folder"
            || entry.problem.is_some()
            || base["own"] == true
            || base["status"] != "attention"
            || !base["inheritedFrom"].is_null()
            || !self.marked_ancestors.contains(&folded)
        {
            return None;
        }
        let db = self.db.as_ref()?;
        let result = (|| -> Result<Value, String> {
            if self.state.watcher_fault.lock().map_err(error)?.is_some() {
                return Ok(
                    Counts::unknown(true).view(Some("Source monitoring needs reconciliation"))
                );
            }
            let epoch = self.epoch(db, &folded)?;
            let mut counts = match self.current_head(db, &folded, &entry.path) {
                Ok(Some(_)) => match self.cached(db, &folded, &epoch)? {
                    Some(c) => c,
                    None => self.build(db, &entry.path, &effective)?,
                },
                Ok(None) => {
                    return Ok(Counts::unknown(false)
                        .view(Some("Observe this folder before deriving child decisions")))
                }
                Err(problem) => return Ok(Counts::unknown(true).view(Some(&problem))),
            };
            let evidence_changed = self
                .relevant_audit_sequence(&folded, self.state.audit.evidence_sequences()?)
                != self.relevant_audit_sequence(&folded, self.audit_sequences)
                || self.epoch(db, &folded)? != epoch
                || self.state.watcher_fault.lock().map_err(error)?.is_some();
            let stale = counts.oldest.as_ref().is_some_and(|at| {
                self.state
                    .audit
                    .changed_below(&entry.path, at)
                    .unwrap_or(true)
            });
            if evidence_changed || stale {
                counts.unknown_branches = counts.unknown_branches.saturating_add(1);
                return Ok(counts.view(Some(
                    "Source observations changed; refresh the affected membership",
                )));
            }
            Ok(counts.view(None))
        })();
        Some(result.unwrap_or_else(|problem| Counts::unknown(true).view(Some(&problem))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::Ordering,
        time::{Duration, Instant},
    };

    fn explicit(s: &AppState, doc: &mut Value, path: &Path, status: &str) {
        let path = path.to_string_lossy().into_owned();
        let (volume, relative, key) = filesystem::locate(&path, &s.volumes).unwrap();
        doc["decisions"][key] = json!({"path":path,"relativePath":relative,"volumeId":volume.id,"status":status,"decidedAt":crate::store::now()});
    }
    fn summary(s: &AppState, doc: &Value, path: &Path) -> Value {
        let entry = filesystem::inspect(path);
        Context::new(s, doc)
            .unwrap()
            .derive(
                &entry,
                &crate::recorded_effective(s, doc, &entry),
                |child| crate::recorded_effective(s, doc, child),
            )
            .unwrap()
    }

    #[test]
    fn observed_rollups_exceed_old_width_and_depth_and_require_actual_membership() {
        let s = crate::tests::fixture();
        let root = Path::new(&s.volumes[0].root).join("WideDeep");
        fs::create_dir_all(&root).unwrap();
        let mut doc = crate::document(&s).unwrap();
        for n in 0..2050 {
            let path = root.join(format!("leaf-{n:04}.txt"));
            fs::write(&path, b"synthetic").unwrap();
            explicit(&s, &mut doc, &path, "migrate-now");
        }
        let mut deepest = root.clone();
        for _ in 0..70 {
            deepest.push("x");
        }
        fs::create_dir_all(&deepest).unwrap();
        let last = deepest.join("deep.txt");
        fs::write(&last, b"synthetic deep leaf").unwrap();
        explicit(&s, &mut doc, &last, "migrate-now");
        assert_eq!(summary(&s, &doc, &root)["kind"], "unknown");
        s.index
            .start(
                vec![root.to_string_lossy().into_owned()],
                s.volumes.clone(),
                false,
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while s.index.running.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "Wide/deep inventory did not complete"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
        s.index.join_stopped().unwrap();
        let scan = s.index.scan_status().unwrap();
        assert_eq!(scan["status"], "complete", "{scan}");
        assert_eq!(scan["files"], 2051, "{scan}");
        let cold_started = Instant::now();
        let known = summary(&s, &doc, &root);
        let cold_ms = cold_started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(known["kind"], "unanimous", "{known}");
        assert_eq!(known["statusCounts"]["migrate-now"], 2051);
        assert_eq!(known["coverageComplete"], true);
        let warm_ms: Vec<_> = (0..5)
            .map(|_| {
                let started = Instant::now();
                assert_eq!(summary(&s, &doc, &root), known);
                started.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        eprintln!("ROLLUP_BENCH leaves=2051 depth=70 cold_ms={cold_ms:.2} warm_ms={warm_ms:.2?}");
        let mut mixed = doc.clone();
        explicit(&s, &mut mixed, &last, "last-moment");
        let different = summary(&s, &mixed, &root);
        assert_eq!(different["kind"], "mixed", "{different}");
        assert_eq!(different["statusCounts"]["last-moment"], 1);
        // A removed marked file and a new unmarked file leave the same count.
        // Joining the real member names, not merely totals, must catch it.
        fs::remove_file(root.join("leaf-0000.txt")).unwrap();
        fs::write(root.join("new-unmarked.txt"), b"synthetic replacement").unwrap();
        s.index
            .refresh(&root.to_string_lossy(), &s.volumes, None, false)
            .unwrap();
        let replaced = summary(&s, &doc, &root);
        assert_eq!(replaced["kind"], "unknown", "{replaced}");
        assert_eq!(replaced["leafCount"], 2051);
        assert_eq!(replaced["undecidedCount"], 1);
        assert_eq!(replaced["statusCounts"]["migrate-now"], 2050);
        let (_, _, root_key) = filesystem::locate(&root.to_string_lossy(), &s.volumes).unwrap();
        assert!(doc["decisions"].get(root_key).is_none());
        assert_eq!(crate::document(&s).unwrap()["decisions"], json!({}));
    }

    #[test]
    fn selected_folder_assertion_never_counts_its_descendant_as_confirmed() {
        let s = crate::tests::fixture();
        let parent = Path::new(&s.volumes[0].root).join("Work");
        let child = parent.join("Child");
        fs::write(
            child.join("new-leaf.txt"),
            b"not covered by folder assertion",
        )
        .unwrap();
        s.index
            .refresh(&parent.to_string_lossy(), &s.volumes, None, false)
            .unwrap();
        s.index
            .refresh(&child.to_string_lossy(), &s.volumes, None, false)
            .unwrap();
        let doc = crate::run_mutation(
            &s,
            crate::document(&s).unwrap()["revision"].as_u64().unwrap(),
            "selected-folder-assertion".into(),
            json!({"type":"classify","path":child.to_string_lossy(),"status":"manual-confirmed"}),
        )
        .unwrap();
        let observed = summary(&s, &doc, &parent);
        assert_eq!(observed["kind"], "unknown", "{observed}");
        assert_eq!(observed["undecidedCount"], 1);
        assert!(observed["statusCounts"]["manual-confirmed"].is_null());
        assert_eq!(
            crate::recorded_effective(&s, &doc, &filesystem::inspect(&child))["status"],
            "manual-confirmed"
        );
    }

    #[test]
    fn view_saves_and_unrelated_observations_do_not_churn_scope_cache() {
        let s = crate::tests::fixture();
        let root = Path::new(&s.volumes[0].root).join("Work").join("Child");
        let sibling = Path::new(&s.volumes[0].root).join("Elsewhere");
        fs::create_dir(&sibling).unwrap();
        let leaf = root.join("leaf.txt");
        fs::write(&leaf, b"known").unwrap();
        s.index
            .refresh(&root.to_string_lossy(), &s.volumes, None, false)
            .unwrap();
        let mut doc = crate::document(&s).unwrap();
        explicit(&s, &mut doc, &leaf, "migrate-now");
        let first = summary(&s, &doc, &root);
        assert_eq!(first["kind"], "unanimous");
        let db = s.index.connect().unwrap();
        let epoch = || -> String {
            db.query_row(
                "SELECT epoch FROM derived_status_summaries WHERE path=?1",
                [root.to_string_lossy().to_lowercase()],
                |r| r.get(0),
            )
            .unwrap()
        };
        let before = epoch();
        doc["revision"] = json!(999);
        doc["view"]["search"] = json!("view only");
        s.index
            .refresh(&sibling.to_string_lossy(), &s.volumes, None, false)
            .unwrap();
        let elsewhere = sibling.join("unrelated.txt");
        fs::write(&elsewhere, b"unrelated").unwrap();
        explicit(&s, &mut doc, &elsewhere, "last-moment");
        assert_eq!(summary(&s, &doc, &root), first);
        assert_eq!(epoch(), before);
        s.audit
            .record_watch_change(&leaf.to_string_lossy(), "modified")
            .unwrap();
        let stale = summary(&s, &doc, &root);
        assert_eq!(stale["kind"], "unknown", "{stale}");
        assert_eq!(stale["coverageComplete"], false);
    }
}
