use crate::{document, effective, filesystem, store, AppState, Entry};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::{collections::VecDeque, path::Path, sync::Mutex};

const ACTIVITY_CAPACITY: usize = 256;
const MAX_COLUMNS: usize = 48;
const MAX_VISIBLE_ROWS: usize = 512;
const MAX_SURFACE_BYTES: usize = 384 * 1024;
const MAX_PATH_BYTES: usize = 32_000;
const PAGE_SIZE: usize = 100;
const SURFACE_MAX_AGE_MS: i64 = 15_000;

#[derive(Default)]
pub(crate) struct ReviewBridge {
    inner: Mutex<ReviewState>,
    pub(crate) ui: crate::ui_control::UiControl,
}

#[derive(Default)]
struct ReviewState {
    sequence: u64,
    activity: VecDeque<Value>,
    surface: Option<Value>,
}

impl ReviewBridge {
    pub(crate) fn activity(&self, kind: &str, path: &str, revision: Option<u64>) {
        if !matches!(kind, "read" | "mark" | "reveal")
            || path.is_empty()
            || path.len() > MAX_PATH_BYTES
        {
            return;
        }
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        let Some(sequence) = state.sequence.checked_add(1) else {
            return;
        };
        state.sequence = sequence;
        if state.activity.len() == ACTIVITY_CAPACITY {
            state.activity.pop_front();
        }
        let mut entry = json!({"sequence":sequence,"kind":kind,"path":path,"atMs":timestamp()});
        if let Some(revision) = revision {
            entry["revision"] = json!(revision);
        }
        state.activity.push_back(entry);
    }

    pub(crate) fn poll(&self, after: u64) -> Result<Value, String> {
        let state = self.inner.lock().map_err(store::error)?;
        let oldest = state.activity.front().and_then(|v| v["sequence"].as_u64());
        let items: Vec<_> = state
            .activity
            .iter()
            .filter(|v| v["sequence"].as_u64().is_some_and(|seq| seq > after))
            .collect();
        Ok(json!({"sequence":state.sequence,"oldestSequence":oldest,"items":items}))
    }

    pub(crate) fn surface(&self) -> Result<Value, String> {
        let state = self.inner.lock().map_err(store::error)?;
        Ok(state.surface.clone().unwrap_or(Value::Null))
    }

    pub(crate) fn report(
        &self,
        value: Value,
        volumes: &[filesystem::Volume],
    ) -> Result<(), String> {
        let surface: ReviewSurface =
            serde_json::from_value(value).map_err(|_| "Invalid rendered review surface")?;
        surface.validate(volumes)?;
        let mut value = serde_json::to_value(surface).map_err(store::error)?;
        value["reportedAtMs"] = json!(timestamp());
        if serde_json::to_vec(&value).map_err(store::error)?.len() > MAX_SURFACE_BYTES {
            return Err("Rendered review surface exceeds 384 KiB".into());
        }
        self.inner.lock().map_err(store::error)?.surface = Some(value);
        Ok(())
    }
}

fn timestamp() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewSurface {
    path: String,
    selected_path: Option<String>,
    focused_path: Option<String>,
    search: String,
    filter: String,
    page: u64,
    scroll_top: f64,
    preview_scroll_left: f64,
    preview_columns: Vec<ReportedPreview>,
    preview_columns_truncated: Option<bool>,
    columns: Vec<ReportedColumn>,
    follow_ai: bool,
    following: Option<bool>,
    reveal_path: Option<String>,
    applied_revision: Option<u64>,
    loading: Option<bool>,
    search_mode: Option<String>,
    window_focused: Option<bool>,
    visibility_state: Option<String>,
    dialog_open: Option<bool>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportedPreview {
    path: String,
    page: u64,
    scroll_top: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<f64>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportedColumn {
    path: String,
    depth: u64,
    page: u64,
    scroll_top: f64,
    total: u64,
    visible_rows: Vec<ReportedRow>,
}

#[derive(Deserialize, Serialize)]
struct ReportedRow {
    path: String,
    name: String,
    kind: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pinned: Option<bool>,
}

impl ReviewSurface {
    fn validate(&self, volumes: &[filesystem::Volume]) -> Result<(), String> {
        let path_in_scope = |path: &str| -> Result<(), String> {
            if path.len() > MAX_PATH_BYTES {
                return Err("Rendered path is too long".into());
            }
            filesystem::locate(path, volumes)?;
            Ok(())
        };
        path_in_scope(&self.path)?;
        for path in [&self.selected_path, &self.focused_path, &self.reveal_path]
            .into_iter()
            .flatten()
        {
            path_in_scope(path)?;
        }
        let position_ok = |n: f64| n.is_finite() && n >= 0.0;
        if self.search.len() > 4_000
            || !store::valid_filter(&self.filter)
            || self.page > 10_000_000
            || !position_ok(self.scroll_top)
            || !position_ok(self.preview_scroll_left)
            || self.columns.len() > MAX_COLUMNS
            || self.preview_columns.len() > MAX_COLUMNS
            || self.search_mode.as_ref().is_some_and(|s| s.len() > 64)
            || self.visibility_state.as_ref().is_some_and(|s| s.len() > 32)
        {
            return Err("Rendered review surface exceeds supported limits".into());
        }
        let mut row_count = 0;
        for column in &self.columns {
            path_in_scope(&column.path)?;
            row_count += column.visible_rows.len();
            if column.depth > 4096
                || column.page > 10_000_000
                || !position_ok(column.scroll_top)
                || column.visible_rows.len() > PAGE_SIZE + 1
                || row_count > MAX_VISIBLE_ROWS
            {
                return Err("Rendered column exceeds supported limits".into());
            }
            for row in &column.visible_rows {
                path_in_scope(&row.path)?;
                if row.name.len() > 4_000
                    || row.kind.len() > 64
                    || !(store::valid_status(&row.status) || matches!(row.status.as_str(), "unknown" | "mixed"))
                {
                    return Err("Invalid rendered review row".into());
                }
            }
        }
        for column in &self.preview_columns {
            path_in_scope(&column.path)?;
            if column.page > 10_000_000
                || !position_ok(column.scroll_top)
                || column.width.is_some_and(|w| !(240.0..=800.0).contains(&w))
            {
                return Err("Invalid rendered preview position".into());
            }
        }
        Ok(())
    }
}

pub(crate) fn same_path(left: &str, right: &str) -> bool {
    let canonical = |path: &str| {
        path.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    canonical(left) == canonical(right)
}

fn parent_path(path: &str) -> Option<String> {
    Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
}

fn text_path(value: &Value) -> Result<String, String> {
    let path = value.as_str().ok_or("A review path is required")?;
    if path.len() > MAX_PATH_BYTES {
        return Err("Review path exceeds the supported length".into());
    }
    filesystem::normalize(path)
}

fn open_folder(s: &AppState, path: &str) -> Result<(), String> {
    let (_, guards) = filesystem::guard_path(path, &s.volumes, false)?;
    let handle = guards
        .last()
        .ok_or("Review folder identity is unavailable")?;
    if !handle.metadata().map_err(store::error)?.is_dir() {
        return Err("Open a folder to create a review column".into());
    }
    Ok(())
}

fn inspect_target(s: &AppState, path: &str) -> Result<Entry, String> {
    let (checked, _guards) = filesystem::guard_path(path, &s.volumes, true)?;
    let entry = filesystem::inspect(&checked);
    if let Some(problem) = &entry.problem {
        return Err(problem.clone());
    }
    Ok(entry)
}

pub(crate) fn entry(s: &AppState, path: String) -> Result<Value, String> {
    let path = text_path(&json!(path))?;
    let entry = inspect_target(s, &path)?;
    let doc = document(s)?;
    let mut row = serde_json::to_value(&entry).map_err(store::error)?;
    row["effective"] = effective(s, &doc, &entry);
    // Link rows can be presented without inspecting the target of the link.
    row["audit"] = if entry.kind == "link" {
        Value::Null
    } else {
        s.audit.stats_for(&path)?
    };
    Ok(row)
}

fn previews(view: &Value) -> Vec<Value> {
    view["previewColumns"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

pub(crate) fn pin_context_changed(view: &Value, patch: &Map<String, Value>) -> bool {
    for key in ["path", "selectedPath", "search", "filter", "page"] {
        if patch.get(key).is_some_and(|value| value != &view[key]) {
            return true;
        }
    }
    if let Some(columns) = patch.get("previewColumns").and_then(Value::as_array) {
        let old = previews(view);
        return old.len() != columns.len()
            || old
                .iter()
                .zip(columns)
                .any(|(a, b)| a["path"] != b["path"] || a["page"] != b["page"]);
    }
    false
}

fn depth(value: Option<&Value>) -> Result<usize, String> {
    let Some(value) = value else {
        return Ok(0);
    };
    value
        .as_u64()
        .filter(|n| *n <= 4096)
        .map(|n| n as usize)
        .ok_or_else(|| "Invalid review column depth".into())
}

fn open_main_patch(view: &Value, path: &str) -> Value {
    if view["path"]
        .as_str()
        .is_some_and(|old| same_path(old, path))
    {
        return json!({});
    }
    let mut recent = vec![json!(path)];
    if let Some(previous) = view["recentPaths"].as_array() {
        recent.extend(
            previous
                .iter()
                .filter(|v| v.as_str().is_some_and(|p| !same_path(p, path)))
                .take(29)
                .cloned(),
        );
    }
    json!({"path":path,"selectedPath":null,"page":0,"scrollTop":0,"recentPaths":recent,
        "previewColumns":[],"previewScrollLeft":0})
}

fn reveal_container(s: &AppState, path: &str) -> Result<Option<String>, String> {
    let (volume, _, _) = filesystem::identify(path, &s.volumes)?;
    if same_path(path, &volume.root) {
        return Ok(None);
    }
    let parent = parent_path(path).ok_or("The review target has no parent folder")?;
    open_folder(s, &parent)?;
    Ok(Some(parent))
}

/// Populate an absent listing outside the journal lock. Existing ordered listings are
/// reused to match the rows the app already renders.
pub(crate) fn prepare_reveal(s: &AppState, target: &Value) -> Result<(), String> {
    let path = text_path(target)?;
    inspect_target(s, &path)?;
    let Some(parent) = reveal_container(s, &path)? else {
        return Ok(());
    };
    if !s
        .index
        .directory_info(&parent)?
        .is_some_and(|info| info["listingObservedAt"].is_string())
    {
        s.index.refresh(&parent, &s.volumes, None, true)?;
    }
    Ok(())
}

fn reveal_patch(s: &AppState, doc: &Value, view: &Value, target: &Value) -> Result<Value, String> {
    let path = text_path(target)?;
    inspect_target(s, &path)?;
    let Some(parent) = reveal_container(s, &path)? else {
        let mut patch = open_main_patch(view, &path);
        patch["selectedPath"] = json!(path);
        patch["revealPath"] = json!(path);
        return Ok(patch);
    };
    let main_matches = view["path"]
        .as_str()
        .is_some_and(|main| same_path(main, &parent));
    let mut columns = previews(view);
    let preview = columns.iter().position(|column| {
        column["path"]
            .as_str()
            .is_some_and(|p| same_path(p, &parent))
    });
    let (search, filter) = if main_matches || preview.is_none() {
        (
            view["search"].as_str().unwrap_or(""),
            view["filter"].as_str().unwrap_or("all"),
        )
    } else {
        ("", "all")
    };
    let mut row_index = 0usize;
    let mut target_page = None;
    s.index.each_entry(&parent, search, |entry| {
        if filter != "all" {
            let result = effective(s, doc, &entry);
            if !crate::matches_review_filter(&result, filter) {
                return true;
            }
        }
        if same_path(&entry.path, &path) {
            target_page = Some(row_index / PAGE_SIZE);
            return false;
        }
        row_index += 1;
        true
    })?;
    let mut patch = if main_matches {
        json!({"scrollTop":0})
    } else if let Some(index) = preview {
        if let Some(page) = target_page {
            columns[index]["page"] = json!(page);
        }
        columns[index]["scrollTop"] = json!(0);
        json!({"previewColumns":columns})
    } else {
        open_main_patch(view, &parent)
    };
    if (main_matches || preview.is_none()) && target_page.is_some() {
        patch["page"] = json!(target_page);
    }
    patch["selectedPath"] = json!(path);
    patch["revealPath"] = json!(path);
    let target_depth = if main_matches { 0 } else { preview.map_or(0, |index| index + 1) };
    if view["layout"]["fillDepth"].as_u64().is_some_and(|depth| depth != target_depth as u64) {
        patch["layout"] = view["layout"].clone();
        patch["layout"]["fillDepth"] = Value::Null;
    }
    Ok(patch)
}

fn follow_view(s: &AppState, doc: &Value, allow_legacy_report: bool) -> Result<Value, String> {
    let mut view = doc["view"].clone();
    let surface = s.review.surface()?;
    let age = surface["reportedAtMs"]
        .as_i64()
        .map(|ms| timestamp().saturating_sub(ms));
    if surface["followAi"] == true
        && (surface["following"] == true || allow_legacy_report && surface["following"].is_null())
        && surface["appliedRevision"] == doc["revision"]
        && surface["loading"] != true
        && surface["previewColumnsTruncated"] != true
        && age.is_some_and(|ms| (0..=SURFACE_MAX_AGE_MS).contains(&ms))
    {
        for key in [
            "path",
            "selectedPath",
            "search",
            "filter",
            "page",
            "scrollTop",
            "previewScrollLeft",
            "revealPath",
        ] {
            view[key] = surface[key].clone();
        }
        let saved_columns = previews(&view);
        let columns: Vec<Value> = previews(&surface)
            .into_iter()
            .map(|shown| {
                let saved = saved_columns.iter().find(|saved| {
                    let displayed = shown["path"].as_str().unwrap_or("");
                    saved["path"]
                        .as_str()
                        .is_some_and(|path| same_path(path, displayed))
                });
                let mut column = saved.cloned().unwrap_or_else(|| json!({}));
                if let Some(fields) = shown.as_object() {
                    for (name, value) in fields {
                        column[name] = value.clone();
                    }
                }
                column
            })
            .collect();
        view["previewColumns"] = json!(columns);
    }
    Ok(view)
}

pub(crate) fn plan(s: &AppState, path: String) -> Result<Value, String> {
    let doc = document(s)?;
    let view = follow_view(s, &doc, true)?;
    let target = json!(path);
    prepare_reveal(s, &target)?;
    let patch = reveal_patch(s, &doc, &view, &target)?;
    Ok(json!({"patch":patch,"revealPath":patch["revealPath"],"revision":doc["revision"]}))
}

pub(crate) fn operation_patch(s: &AppState, doc: &Value, op: &Value) -> Result<Value, String> {
    let view = follow_view(s, doc, false)?;
    let mut action = action_patch(s, doc, &view, op)?;
    if op["action"] == "openPreview" && view["layout"]["fillDepth"].is_u64() {
        action["layout"] = view["layout"].clone();
        action["layout"]["fillDepth"] = Value::Null;
    }
    let fields = action.as_object().ok_or("Invalid review action result")?;
    let mut patch = Map::new();
    if let Some(base) = view.as_object() {
        for (key, value) in base {
            if doc["view"].get(key) != Some(value) {
                patch.insert(key.clone(), value.clone());
            }
        }
    }
    if !fields.contains_key("revealPath") && pin_context_changed(&view, fields) {
        patch.insert("revealPath".into(), Value::Null);
    }
    patch.extend(fields.clone());
    Ok(Value::Object(patch))
}

fn action_patch(s: &AppState, doc: &Value, view: &Value, op: &Value) -> Result<Value, String> {
    match op["action"].as_str().ok_or("A review action is required")? {
        "openMain" => {
            let path = text_path(&op["path"])?;
            open_folder(s, &path)?;
            let mut patch = open_main_patch(view, &path);
            if let Some(selection) = op.get("selectedPath") {
                patch["selectedPath"] = if selection.is_null() {
                    Value::Null
                } else {
                    let path = text_path(selection)?;
                    inspect_target(s, &path)?;
                    json!(path)
                };
            }
            patch["revealPath"] = Value::Null;
            Ok(patch)
        }
        "openPreview" => {
            let from = depth(op.get("depth"))?;
            let path = text_path(&op["path"])?;
            let mut columns = previews(view);
            let parent = if from == 0 {
                view["path"].as_str()
            } else {
                columns
                    .get(from - 1)
                    .and_then(|column| column["path"].as_str())
            }
            .ok_or("The parent review column is not open")?;
            if !parent_path(&path).is_some_and(|p| same_path(&p, parent)) {
                return Err("A preview must be a direct child of its parent column".into());
            }
            open_folder(s, &path)?;
            if !columns
                .get(from)
                .and_then(|column| column["path"].as_str())
                .is_some_and(|p| same_path(p, &path))
            {
                columns.truncate(from);
                columns.push(json!({"path":path,"page":0,"scrollTop":0}));
            }
            Ok(json!({"previewColumns":columns,"selectedPath":path,"revealPath":null}))
        }
        "closePreview" => {
            let closing = depth(op.get("depth"))?;
            let mut columns = previews(view);
            let index = closing
                .checked_sub(1)
                .ok_or("The main review column cannot be closed")?;
            let removed = columns
                .get(index)
                .and_then(|column| column["path"].as_str())
                .ok_or("The preview column is not open")?;
            let selection = if view["selectedPath"]
                .as_str()
                .is_some_and(|p| filesystem::under(p, removed))
            {
                json!(removed)
            } else {
                view["selectedPath"].clone()
            };
            columns.truncate(index);
            let mut patch =
                json!({"previewColumns":columns,"selectedPath":selection,"revealPath":null});
            if index == 0 {
                patch["previewScrollLeft"] = json!(0);
            }
            Ok(patch)
        }
        "reveal" => reveal_patch(s, doc, view, &op["path"]),
        "select" => {
            let path = text_path(&op["path"])?;
            inspect_target(s, &path)?;
            Ok(json!({"selectedPath":path,"revealPath":null}))
        }
        "setView" => view_settings_patch(view, op),
        "setLayout" => layout_settings_patch(view, op),
        _ => Err("Unknown review action".into()),
    }
}

fn layout_settings_patch(view: &Value, op: &Value) -> Result<Value, String> {
    let mut patch = Map::new();
    if let Some(settings) = op.get("layout") {
        if !crate::store::valid_review_layout(settings) {
            return Err("Invalid review layout settings".into());
        }
        let mut layout = view["layout"].as_object().cloned().unwrap_or_default();
        let fields = settings.as_object().ok_or("Invalid review layout settings")?;
        if fields.get("fillDepth").and_then(Value::as_u64)
            .is_some_and(|n| n > previews(view).len() as u64)
        {
            return Err("The folder column to expand is not open".into());
        }
        layout.extend(fields.clone());
        patch.insert("layout".into(), Value::Object(layout));
    }
    if let Some(width) = op.get("width") {
        if !width.as_f64().is_some_and(|n| (280.0..=800.0).contains(&n)) {
            return Err("Folder column width must be between 280 and 800 pixels".into());
        }
        let depth = depth(op.get("depth"))?;
        if depth == 0 {
            patch.insert("browserColumnWidth".into(), width.clone());
        } else {
            let mut columns = previews(view);
            let column = columns.get_mut(depth - 1).ok_or("The preview column is not open")?;
            column["width"] = width.clone();
            patch.insert("previewColumns".into(), json!(columns));
        }
    }
    if patch.is_empty() {
        return Err("Provide a sidebar, toolbar, fill, or width setting".into());
    }
    Ok(Value::Object(patch))
}

fn view_settings_patch(view: &Value, op: &Value) -> Result<Value, String> {
    let depth = depth(op.get("depth"))?;
    if op
        .get("page")
        .is_some_and(|p| !p.as_u64().is_some_and(|n| n <= 10_000_000))
    {
        return Err("Review page is outside the supported range".into());
    }
    let mut patch = Map::new();
    if depth == 0 {
        for key in ["search", "filter", "page", "scrollTop"] {
            if let Some(value) = op.get(key) {
                patch.insert(key.into(), value.clone());
            }
        }
        let criteria_changed = ["search", "filter"]
            .iter()
            .any(|key| patch.get(*key).is_some_and(|value| value != &view[*key]));
        if criteria_changed {
            patch.entry("page").or_insert(json!(0));
            patch.entry("scrollTop").or_insert(json!(0));
        }
    } else {
        if op.get("search").is_some() || op.get("filter").is_some() {
            return Err("Search and classification filters apply to the main review column".into());
        }
        let mut columns = previews(view);
        let column = columns
            .get_mut(depth - 1)
            .ok_or("The preview column is not open")?;
        let mut edited = false;
        for key in ["page", "scrollTop"] {
            if let Some(value) = op.get(key) {
                column[key] = value.clone();
                edited = true;
            }
        }
        if edited {
            patch.insert("previewColumns".into(), json!(columns));
        }
    }
    if let Some(value) = op.get("previewScrollLeft") {
        patch.insert("previewScrollLeft".into(), value.clone());
    }
    if patch.is_empty() {
        return Err("Provide at least one review setting to change".into());
    }
    Ok(Value::Object(patch))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{run_mutation, run_mutation_as, MutationOrigin};

    fn mutate(s: &AppState, id: &str, operation: Value) -> Value {
        let revision = document(s).unwrap()["revision"].as_u64().unwrap();
        run_mutation(s, revision, id.into(), operation).unwrap()
    }

    fn surface(path: &str, revision: u64) -> Value {
        json!({"path":path,"selectedPath":null,"focusedPath":null,"search":"","filter":"all",
            "page":0,"scrollTop":0,"previewScrollLeft":0,"previewColumns":[],
            "columns":[{"path":path,"depth":0,"page":0,"scrollTop":0,"total":0,"visibleRows":[]}],
            "followAi":true,"following":true,"appliedRevision":revision,"loading":false})
    }

    #[test]
    fn activity_is_bounded_and_ordered_and_reports_preserve_exact_paths() {
        let s = crate::tests::fixture();
        let path = format!("{}\\Work", s.volumes[0].root);
        for _ in 0..300 {
            s.review.activity("read", &path, None);
        }
        let result = s.review.poll(0).unwrap();
        assert_eq!(result["sequence"], 300);
        assert_eq!(result["oldestSequence"], 45);
        assert_eq!(result["items"].as_array().unwrap().len(), ACTIVITY_CAPACITY);
        let after = s.review.poll(299).unwrap();
        assert_eq!(after["items"][0]["path"], path);
        assert_eq!(after["items"][0]["sequence"], 300);
        assert!(after["items"][0]["atMs"].is_i64());
        let revision = document(&s).unwrap()["revision"].as_u64().unwrap();
        let mut report = surface(&path, revision);
        report["columns"][0]["visibleRows"] = json!([{
            "path":format!("{path}\\Child"),"name":"Child","kind":"folder","status":"unknown"
        }]);
        s.review.report(report.clone(), &s.volumes).unwrap();
        let accepted = s.review.surface().unwrap();
        assert_eq!(
            accepted["columns"][0]["visibleRows"][0]["path"],
            format!("{path}\\Child")
        );
        assert!(accepted["reportedAtMs"].is_i64());
        report["columns"][0]["visibleRows"][0]["path"] = json!(r"Z:\outside");
        assert!(s.review.report(report, &s.volumes).is_err());
        assert_eq!(s.review.surface().unwrap(), accepted);
    }

    #[test]
    fn preview_depth_uses_the_direct_parent_and_branch_close_preserves_other_view_settings() {
        let s = crate::tests::fixture();
        let root = &s.volumes[0].root;
        let work = format!("{root}\\Work");
        let child = format!("{work}\\Child");
        mutate(
            &s,
            "preview-one",
            json!({"type":"review","action":"openPreview","path":work,"depth":0}),
        );
        mutate(
            &s,
            "preview-two",
            json!({"type":"review","action":"openPreview","path":child,"depth":1}),
        );
        let doc = mutate(
            &s,
            "same-main",
            json!({"type":"review","action":"openMain","path":root}),
        );
        assert_eq!(doc["view"]["previewColumns"].as_array().unwrap().len(), 2);
        let doc = mutate(
            &s,
            "preview-settings",
            json!({"type":"review","action":"setView","depth":2,"page":3,"scrollTop":18.5}),
        );
        assert_eq!(doc["view"]["previewColumns"][1]["page"], 3);
        assert_eq!(doc["view"]["previewColumns"][1]["scrollTop"], 18.5);
        assert_eq!(doc["view"]["page"], 0);
        let revision = doc["revision"].as_u64().unwrap();
        assert!(run_mutation(
            &s,
            revision,
            "wrong-depth".into(),
            json!({"type":"review","action":"openPreview","path":child,"depth":0})
        )
        .is_err());
        assert_eq!(document(&s).unwrap()["revision"], revision);
        let doc = mutate(
            &s,
            "close-suffix",
            json!({"type":"review","action":"closePreview","depth":1}),
        );
        assert_eq!(doc["view"]["previewColumns"], json!([]));
        assert_eq!(doc["view"]["selectedPath"], work);
    }

    #[test]
    fn reveal_finds_the_sorted_page_or_keeps_a_filtered_target_as_a_pin() {
        let s = crate::tests::fixture();
        let work = format!("{}\\Work", s.volumes[0].root);
        for i in 0..103 {
            std::fs::write(format!("{work}\\file-{i:03}.txt"), b"fixture").unwrap();
        }
        let target = format!("{work}\\file-102.txt");
        let doc = mutate(
            &s,
            "search-fixture-files",
            json!({"type":"view","patch":{"path":work,"search":"file-","filter":"all"}}),
        );
        let reveal = plan(&s, target.clone()).unwrap();
        assert_eq!(reveal["patch"]["page"], 1);
        assert_eq!(reveal["revision"], doc["revision"]);
        assert_eq!(document(&s).unwrap()["revision"], doc["revision"]);
        let doc = mutate(
            &s,
            "change-filter",
            json!({"type":"review","action":"setView","filter":"erase","page":2}),
        );
        assert_eq!(doc["view"]["page"], 2);
        let reveal = plan(&s, target.clone()).unwrap();
        assert!(reveal["patch"].get("filter").is_none());
        assert!(reveal["patch"].get("page").is_none());
        assert_eq!(reveal["patch"]["revealPath"], target);
        let row = entry(&s, target.clone()).unwrap();
        assert_eq!(row["path"], target);
        assert_eq!(row["effective"]["status"], "attention");
        let doc = mutate(
            &s,
            "persist-pinned-target",
            json!({"type":"review","action":"reveal","path":target}),
        );
        assert_eq!(doc["view"]["filter"], "erase");
        let doc = mutate(
            &s,
            "persist-scroll",
            json!({"type":"view","patch":{"scrollTop":20}}),
        );
        assert_eq!(doc["view"]["revealPath"], target);
        let doc = mutate(
            &s,
            "clear-by-ordinary-selection",
            json!({"type":"view","patch":{"selectedPath":null}}),
        );
        assert!(doc["view"]["revealPath"].is_null());
    }

    #[test]
    fn durable_operations_use_current_follow_surface_and_emit_activity_once_after_commit() {
        let s = crate::tests::fixture();
        let work = format!("{}\\Work", s.volumes[0].root);
        let child = format!("{work}\\Child");
        let revision = document(&s).unwrap()["revision"].as_u64().unwrap();
        let mut report = surface(&work, revision);
        report["previewColumns"] = json!([{"path":child,"page":0,"scrollTop":0}]);
        s.review.report(report, &s.volumes).unwrap();
        let doc = mutate(
            &s,
            "act-on-followed-folder",
            json!({"type":"review","action":"setView","filter":"erase"}),
        );
        assert_eq!(doc["view"]["path"], work);
        assert_eq!(doc["view"]["filter"], "erase");
        assert_eq!(doc["view"]["previewColumns"][0]["path"], child);
        let origin = MutationOrigin::Agent {
            basis: "user_instruction".into(),
            reason: "Fixture review".into(),
        };
        let reveal = json!({"type":"review","action":"reveal","path":child});
        let old_revision = doc["revision"].as_u64().unwrap();
        let saved = run_mutation_as(
            &s,
            old_revision,
            "durable-reveal".into(),
            reveal.clone(),
            origin.clone(),
        )
        .unwrap();
        assert_eq!(
            run_mutation_as(
                &s,
                old_revision,
                "durable-reveal".into(),
                reveal,
                origin.clone()
            )
            .unwrap()["revision"],
            saved["revision"]
        );
        let activities = s.review.poll(0).unwrap();
        assert_eq!(activities["sequence"], 1);
        assert_eq!(activities["items"][0]["kind"], "reveal");
        assert_eq!(activities["items"][0]["path"], child);
        assert_eq!(activities["items"][0]["revision"], saved["revision"]);
        assert!(run_mutation_as(
            &s,
            old_revision,
            "stale-mark".into(),
            json!({"type":"classify","path":child,"status":"erase"}),
            origin.clone()
        )
        .is_err());
        assert_eq!(s.review.poll(0).unwrap()["sequence"], 1);
        let saved = run_mutation_as(
            &s,
            saved["revision"].as_u64().unwrap(),
            "new-mark".into(),
            json!({"type":"classify","path":child,"status":"erase"}),
            origin,
        )
        .unwrap();
        let activity = s.review.poll(1).unwrap();
        assert_eq!(activity["items"][0]["kind"], "mark");
        assert_eq!(activity["items"][0]["revision"], saved["revision"]);
        assert_eq!(activity["items"][0]["path"], child);
    }
}
