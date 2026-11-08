//! Preparation for exact, atomic classification groups. Selection is a UI
//! concern; this operation only accepts the targets explicitly reviewed.
use crate::{filesystem, store, AppState};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, fs::File, os::windows::io::AsRawHandle, path::PathBuf};
use windows_sys::Win32::Storage::FileSystem::{
    FileCaseSensitiveInfo, GetFileInformationByHandleEx, FILE_CASE_SENSITIVE_INFO,
};

pub const MAX_TARGETS: usize = 128;
const MAX_HISTORY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub path: String,
    pub volume_id: String,
    pub relative_path: String,
    pub kind: String,
    pub fingerprint: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    #[serde(rename = "type")]
    operation_type: String,
    targets: Vec<Target>,
    status: String,
    confirmed: bool,
    acknowledge_queue_effects: bool,
    #[serde(default)]
    acknowledge_folder_effects: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UndoRequest {
    #[serde(rename = "type")]
    operation_type: String,
    group_id: String,
    confirmed: bool,
    acknowledge_queue_effects: bool,
}

pub struct PreparedTarget {
    pub target: Target,
    pub key: String,
    checked: PathBuf,
    _guards: Vec<File>,
}

impl PreparedTarget {
    fn ensure_current(&self) -> Result<(), String> {
        let observed = filesystem::inspect(&self.checked);
        if observed.problem.is_some()
            || observed.kind != self.target.kind
            || observed.fingerprint != self.target.fingerprint
        {
            return Err("the source changed since it was reviewed".into());
        }
        Ok(())
    }

    pub fn describe(&self) -> Value {
        json!({
            "path": self.target.path, "volumeId": self.target.volume_id,
            "relativePath": self.target.relative_path, "kind": self.target.kind,
            "fingerprint": self.target.fingerprint
        })
    }

    pub fn entry(&self) -> filesystem::Entry {
        filesystem::inspect(&self.checked)
    }
}

pub fn ensure_current(targets: &[PreparedTarget]) -> Result<(), String> {
    for target in targets {
        target.ensure_current().map_err(|reason|
            format!("{}: {reason}. The grouped edit was not saved.", target.target.path))?;
    }
    Ok(())
}

pub fn is_group_event(event: &Value) -> bool {
    matches!(event["type"].as_str(), Some("bulkClassify" | "undoGroup"))
}

pub fn history_record_id(event: &Value, key: &str) -> Option<String> {
    Some(format!("group:{}:{}", store::hash(event["id"].as_str()?.as_bytes()),
        store::hash(key.as_bytes())))
}

pub fn history_for(event: &Value, key: &str) -> Option<Value> {
    if !is_group_event(event) {
        return (event["key"] == key).then(|| event.clone());
    }
    let member = event["members"].as_array()?.iter().find(|member| member["key"] == key)?;
    let record_id = history_record_id(event, key)?;
    let mut projection = json!({
        "id":event["id"],"type":event["type"],"at":event["at"],"memberCount":event["memberCount"],
        "recordId":record_id,
        "path":member["target"]["path"],"key":member["key"],
        "before":member["before"],"after":member["after"],"provenance":event["provenance"]
    });
    for key in ["undoneAt", "undoneBy", "undoes", "status"] {
        if let Some(value) = event.get(key) {
            projection[key] = value.clone();
        }
    }
    Some(projection)
}

pub fn history_by_record_id(doc: &Value, record_id: &str) -> Option<Value> {
    let (group, member) = record_id.strip_prefix("group:")?.split_once(':')?;
    if group.len() != 64 || member.len() != 64 {
        return None;
    }
    let event = doc["history"].as_array()?.iter().rev().find(|event| {
        is_group_event(event) && event["id"].as_str().is_some_and(|id| store::hash(id.as_bytes()) == group)
    })?;
    let key = event["members"].as_array()?.iter().find_map(|item| {
        item["key"].as_str().filter(|key| store::hash(key.as_bytes()) == member)
    })?;
    history_for(event, key)
}

fn parse_target(s: &AppState, mut target: Target) -> Result<PreparedTarget, String> {
    if target.path.len() > 32_000
        || target.relative_path.len() > 32_000
        || target.volume_id.len() > 256
        || !matches!(target.kind.as_str(), "folder" | "file")
        || target.fingerprint.len() != 64
        || !target.fingerprint.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err("invalid reviewed file or folder snapshot".into());
    }
    target.path = filesystem::normalize(&target.path)?;
    if target.path[3..].contains(r"\\") || target.path.contains('\0') {
        return Err("ambiguous Windows path".into());
    }
    let (volume, relative, key) = filesystem::identify(&target.path, &s.volumes)?;
    if volume.id != target.volume_id || relative.to_lowercase() != target.relative_path.to_lowercase() {
        return Err("the reviewed volume or relative location changed".into());
    }
    target.relative_path = relative;
    let (checked, guards) = filesystem::guard_path(&target.path, &s.volumes, false)?;
    let canonical = std::fs::canonicalize(&checked).map_err(store::error)?;
    let canonical = canonical.to_str().ok_or("the exact path cannot be represented")?;
    let canonical = canonical.strip_prefix(r"\\?\").unwrap_or(canonical);
    if canonical.to_lowercase() != target.path.to_lowercase() {
        return Err("the path is an alias for a different reviewed identity".into());
    }
    // Stored decision identities fold case. Refuse scopes where two distinct
    // Windows names can share that identity, rather than changing the other row.
    for handle in &guards {
        if handle.metadata().map_err(store::error)?.is_dir() {
            let mut info = FILE_CASE_SENSITIVE_INFO::default();
            let ok = unsafe {
                GetFileInformationByHandleEx(handle.as_raw_handle().cast(), FileCaseSensitiveInfo,
                    (&mut info as *mut FILE_CASE_SENSITIVE_INFO).cast(),
                    std::mem::size_of::<FILE_CASE_SENSITIVE_INFO>() as u32)
            };
            if ok == 0 {
                return Err(format!("cannot verify the directory's path identity: {}", std::io::Error::last_os_error()));
            }
            if info.Flags != 0 {
                return Err("case-sensitive folders need a separate review; folded decision keys are ambiguous here".into());
            }
        }
    }
    let member = PreparedTarget { target, key, checked, _guards: guards };
    member.ensure_current()?;
    Ok(member)
}

fn prepare_targets(s: &AppState, targets: Vec<Target>) -> Result<Vec<PreparedTarget>, String> {
    if !(1..=MAX_TARGETS).contains(&targets.len()) {
        return Err(format!("Choose between 1 and {MAX_TARGETS} reviewed targets."));
    }
    let root = targets[0].volume_id.clone();
    let mut accepted = Vec::with_capacity(targets.len());
    let mut keys = HashSet::with_capacity(targets.len());
    let mut failures = Vec::new();
    for (index, target) in targets.into_iter().enumerate() {
        let path = target.path.chars().take(300).collect::<String>();
        let prepared = if target.volume_id == root {
            parse_target(s, target)
        } else {
            Err("target is outside the reviewed selection root".into())
        };
        match prepared {
            Ok(target) if keys.insert(target.key.clone()) => accepted.push(target),
            Ok(_) => failures.push(format!("{}. {path}: duplicate target", index + 1)),
            Err(reason) => failures.push(format!("{}. {path}: {reason}", index + 1)),
        }
    }
    if !failures.is_empty() {
        let omitted = failures.len().saturating_sub(12);
        failures.truncate(12);
        return Err(format!("No decisions were changed. {}{}", failures.join("\n"),
            if omitted == 0 { String::new() } else { format!("\n…and {omitted} more invalid targets.") }));
    }
    Ok(accepted)
}

pub struct PreparedGroup {
    pub status: String,
    pub targets: Vec<PreparedTarget>,
}

pub fn prepare(s: &AppState, op: &Value) -> Result<PreparedGroup, String> {
    let request: Request = serde_json::from_value(op.clone()).map_err(store::error)?;
    if request.operation_type != "bulkClassify" || !request.confirmed || !request.acknowledge_queue_effects {
        return Err("Review the exact selection and confirm the grouped mark.".into());
    }
    if !store::valid_status(&request.status) {
        return Err("Unknown classification".into());
    }
    if request.targets.iter().any(|target| target.kind == "folder") && !request.acknowledge_folder_effects {
        return Err("A selected folder can affect unmarked descendants. Review folder effects before applying.".into());
    }
    let targets = prepare_targets(s, request.targets)?;
    Ok(PreparedGroup { status: request.status, targets })
}

pub fn group_event(
    doc: &Value,
    group: &PreparedGroup,
    operation_id: &str,
    payload_hash: &str,
    provenance: &Value,
    mut classify: impl FnMut(&PreparedTarget, &Value, &str) -> Result<Value, String>,
) -> Result<Value, String> {
    let mut members = Vec::with_capacity(group.targets.len());
    let mut bytes = 0usize;
    for member in &group.targets {
        let before = &doc["decisions"][&member.key];
        let after = classify(member, before, &group.status)?;
        let item = json!({"key":member.key,"target":member.describe(),"before":before,"after":after});
        bytes = bytes.saturating_add(serde_json::to_vec(&item).map_err(store::error)?.len());
        if bytes > MAX_HISTORY_BYTES {
            return Err("These decisions exceed the safe grouped history size. Apply smaller reviewed selections.".into());
        }
        members.push(item);
    }
    for member in &group.targets {
        member.ensure_current().map_err(|reason| format!("{}: {reason}. No decisions were changed.", member.target.path))?;
    }
    Ok(json!({"id":operation_id,"type":"bulkClassify","status":group.status,"at":store::now(),
        "members":members,"memberCount":members.len(),"payloadHash":payload_hash,"provenance":provenance}))
}

pub struct PreparedUndo {
    pub position: usize,
    pub original: Value,
    pub targets: Vec<PreparedTarget>,
}

pub fn prepare_undo(s: &AppState, doc: &Value, op: &Value) -> Result<PreparedUndo, String> {
    let request: UndoRequest = serde_json::from_value(op.clone()).map_err(store::error)?;
    if request.operation_type != "undoGroup" || !request.confirmed || !request.acknowledge_queue_effects || request.group_id.is_empty() {
        return Err("Confirm Undo for the reviewed group.".into());
    }
    let history = doc["history"].as_array().ok_or("Invalid decision history")?;
    let position = history.iter().rposition(|event| event["id"] == request.group_id
        && event["type"] == "bulkClassify" && event["undoneAt"].is_null())
        .ok_or("This group does not exist or has already been undone.")?;
    let original = &history[position];
    let members = original["members"].as_array().filter(|members| (1..=MAX_TARGETS).contains(&members.len()))
        .ok_or("The saved group has invalid membership.")?;
    let targets = members.iter().map(|member| serde_json::from_value::<Target>(member["target"].clone()))
        .collect::<Result<Vec<_>, _>>().map_err(store::error)?;
    let targets = prepare_targets(s, targets)?;
    for (member, target) in members.iter().zip(&targets) {
        if member["key"] != target.key || doc["decisions"][&target.key] != member["after"]
            || !(member["before"].is_object() || member["before"].is_null())
        {
            return Err(format!("{} changed after the grouped mark. No member was undone; review later edits first.", target.target.path));
        }
    }
    Ok(PreparedUndo { position, original: original.clone(), targets })
}

pub fn apply_group(doc: &mut Value, event: Value) {
    for member in event["members"].as_array().expect("prepared group") {
        let key = member["key"].as_str().expect("prepared decision key");
        doc["decisions"][key] = member["after"].clone();
    }
    doc["schemaVersion"] = json!(2);
    append_group_event(doc, event);
}

fn append_group_event(doc: &mut Value, event: Value) {
    if doc.get("groupOperations").is_none() {
        doc["groupOperations"] = json!({});
    }
    let id = event["id"].as_str().expect("prepared group ID");
    doc["groupOperations"][id] = json!({"payloadHash":event["payloadHash"],
        "type":event["type"],"memberCount":event["memberCount"]});
    doc["history"].as_array_mut().expect("validated history").push(event);
}

pub fn apply_undo(
    doc: &mut Value,
    prepared: &PreparedUndo,
    operation_id: &str,
    payload_hash: &str,
    provenance: &Value,
) -> Result<(), String> {
    for target in &prepared.targets {
        target.ensure_current().map_err(|reason| format!("{}: {reason}. No decisions were undone.", target.target.path))?;
    }
    let decisions = doc["decisions"].as_object_mut().expect("validated decisions");
    let mut members = Vec::new();
    for member in prepared.original["members"].as_array().expect("prepared group") {
        let key = member["key"].as_str().expect("prepared decision key");
        if member["before"].is_null() {
            decisions.remove(key);
        } else {
            decisions.insert(key.to_string(), member["before"].clone());
        }
        members.push(json!({"key":key,"target":member["target"],"before":member["after"],"after":member["before"]}));
    }
    let at = store::now();
    doc["history"][prepared.position]["undoneAt"] = json!(at);
    doc["history"][prepared.position]["undoneBy"] = provenance.clone();
    append_group_event(doc, json!({
        "id":operation_id,"type":"undoGroup","undoes":prepared.original["id"],"at":at,
        "members":members,"memberCount":members.len(),"payloadHash":payload_hash,"provenance":provenance
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{document, run_mutation, Store};
    use std::{fs, path::Path};

    fn read_target(s: &AppState, path: &Path) -> Value {
        let entry = filesystem::inspect(path);
        let (volume, relative, _) = filesystem::identify(&entry.path, &s.volumes).unwrap();
        json!({"path":entry.path,"volumeId":volume.id,"relativePath":relative,
            "kind":entry.kind,"fingerprint":entry.fingerprint})
    }

    fn revision(s: &AppState) -> u64 {
        document(s).unwrap()["revision"].as_u64().unwrap()
    }

    fn set(s: &AppState, id: &str, op: Value) -> Result<Value, String> {
        run_mutation(s, revision(s), id.into(), op)
    }

    fn group(targets: Vec<Value>) -> Value {
        json!({"type":"bulkClassify","targets":targets,"status":"migrate-now",
            "confirmed":true,"acknowledgeFolderEffects":true,"acknowledgeQueueEffects":true})
    }

    fn undo(id: &str) -> Value {
        json!({"type":"undoGroup","groupId":id,"confirmed":true,"acknowledgeQueueEffects":true})
    }

    #[test]
    fn overlap_is_atomic_and_does_not_change_unselected_exceptions() {
        let s = crate::tests::fixture();
        let folder = Path::new(&s.volumes[0].root).join("group");
        let selected = folder.join("selected.txt");
        let exception = folder.join("exception.txt");
        fs::create_dir(&folder).unwrap();
        fs::write(&selected, b"selected").unwrap();
        fs::write(&exception, b"exception").unwrap();
        set(&s, "child-exception", json!({"type":"classify","path":exception,"status":"last-moment"})).unwrap();
        let before = document(&s).unwrap();
        let op = group(vec![read_target(&s, &folder), read_target(&s, &selected)]);
        let saved = set(&s, "group-review", op.clone()).unwrap();
        assert_eq!(saved["schemaVersion"], 2);
        assert_eq!(saved["revision"], before["revision"].as_u64().unwrap() + 1);
        let history = saved["history"].as_array().unwrap();
        assert_eq!(history.len(), before["history"].as_array().unwrap().len() + 1);
        assert_eq!(history.last().unwrap()["memberCount"], 2);
        let keys = [&folder, &selected, &exception].map(|path|
            filesystem::identify(&path.to_string_lossy(), &s.volumes).unwrap().2);
        assert_eq!(saved["decisions"][&keys[0]]["status"], "migrate-now");
        assert_eq!(saved["decisions"][&keys[1]]["status"], "migrate-now");
        assert_eq!(saved["decisions"][&keys[2]], before["decisions"][&keys[2]]);

        {
            let mut owner = s.store.lock().unwrap();
            let mut current = owner.take().unwrap();
            let mut doc = current.doc.clone();
            doc["recentOperations"] = json!([]);
            current.commit(doc).unwrap();
            drop(current);
            *owner = Some(Store::open(s.data_path.clone(), &s.volumes[0].root).unwrap());
        }
        let previous = document(&s).unwrap();
        assert_eq!(run_mutation(&s, 0, "group-review".into(), op.clone()).unwrap(), previous);
        let mut changed = op;
        changed["status"] = json!("erase");
        assert!(set(&s, "group-review", changed).is_err());
        let restored = set(&s, "undo-group-review", undo("group-review")).unwrap();
        assert_eq!(restored["decisions"], before["decisions"]);
        assert_eq!(restored["history"].as_array().unwrap().last().unwrap()["memberCount"], 2);
        assert!(set(&s, "undo-group-again", undo("group-review")).is_err());
    }

    #[test]
    fn changed_target_and_later_note_each_refuse_the_whole_operation() {
        let s = crate::tests::fixture();
        let root = Path::new(&s.volumes[0].root);
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        fs::write(&a, b"first").unwrap();
        fs::write(&b, b"second").unwrap();
        let op = group(vec![read_target(&s, &a), read_target(&s, &b)]);
        fs::write(&b, b"changed length fixture").unwrap();
        let before = document(&s).unwrap();
        assert!(set(&s, "stale-group", op).unwrap_err().contains("changed since it was reviewed"));
        assert_eq!(document(&s).unwrap(), before);
        let duplicate = read_target(&s, &a);
        assert!(set(&s, "duplicate-target", group(vec![duplicate.clone(), duplicate])).is_err());
        assert_eq!(document(&s).unwrap(), before);
        set(&s, "fresh-group", group(vec![read_target(&s, &a), read_target(&s, &b)])).unwrap();
        set(&s, "later-note", json!({"type":"note","path":b,"notes":"keep this note"})).unwrap();
        let after = document(&s).unwrap();
        assert!(set(&s, "undo-old-group", undo("fresh-group")).unwrap_err().contains("changed after"));
        assert_eq!(document(&s).unwrap(), after);
    }
}
