use crate::filesystem::{self, Entry, Volume};
use serde_json::{json, Value};
use std::fs;

const MAX_ENTRIES: usize = 2_000;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy)]
enum Marks {
    One(&'static str),
    Mixed,
}

impl Marks {
    fn combine(self, other: Self) -> Self {
        match (self, other) {
            (Self::One(left), Self::One(right)) if left == right => self,
            _ => Self::Mixed,
        }
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

fn has_marked_descendant(doc: &Value, path: &str, volumes: &[Volume]) -> bool {
    let Ok((volume, relative, _)) = filesystem::locate(path, volumes) else {
        return false;
    };
    let prefix = if relative.is_empty() {
        String::new()
    } else {
        format!("{}\\", relative.to_lowercase())
    };
    doc["decisions"].as_object().is_some_and(|decisions| {
        decisions.values().any(|decision| {
            decision["volumeId"] == volume.id
                && decision["noteOnly"] != true
                && known_status(&decision["status"]).is_some()
                && decision["relativePath"].as_str().is_some_and(|candidate| {
                    !candidate.eq_ignore_ascii_case(&relative)
                        && candidate.to_lowercase().starts_with(&prefix)
                })
        })
    })
}

fn children(
    path: &str,
    volumes: &[Volume],
    doc: &Value,
    effective: &impl Fn(&Entry) -> Value,
    remaining: &mut usize,
    depth: usize,
) -> Option<(Marks, usize)> {
    if depth >= MAX_DEPTH {
        return None;
    }
    let (path, guards) = filesystem::guard_path(path, volumes, false).ok()?;
    let directory = guards.last()?;
    let before = directory.metadata().ok()?.modified().ok()?;
    let mut marks: Option<Marks> = None;
    let mut count = 0;
    for item in fs::read_dir(path).ok()? {
        *remaining = remaining.checked_sub(1)?;
        let entry = filesystem::inspect(&item.ok()?.path());
        if entry.problem.is_some() {
            return None;
        }
        let observed = effective(&entry);
        let status = known_status(&observed["status"]);
        let nested = entry.kind == "folder"
            && ((status.is_none() && observed["own"] != true)
                || (status.is_some() && observed["mixed"].as_u64().unwrap_or(0) > 0))
            && has_marked_descendant(doc, &entry.path, volumes);
        let child = if nested {
            children(&entry.path, volumes, doc, effective, remaining, depth + 1)?.0
        } else {
            Marks::One(status?)
        };
        marks = Some(marks.map_or(child, |prior| prior.combine(child)));
        count += 1;
    }
    if before != directory.metadata().ok()?.modified().ok()? {
        return None;
    }
    Some((marks?, count))
}

/// A read-only summary of the currently observed children. It never changes
/// the journal, inheritance rules or the explicit marks used for copying.
pub fn derive(
    entry: &Entry,
    base: &Value,
    doc: &Value,
    volumes: &[Volume],
    effective: impl Fn(&Entry) -> Value,
) -> Option<Value> {
    if entry.kind != "folder"
        || entry.problem.is_some()
        || base["own"] == true
        || base["status"] != "attention"
        || !base["inheritedFrom"].is_null()
        || !has_marked_descendant(doc, &entry.path, volumes)
    {
        return None;
    }
    let mut remaining = MAX_ENTRIES;
    let (marks, count) = children(&entry.path, volumes, doc, &effective, &mut remaining, 0)?;
    Some(match marks {
        Marks::One(status) => json!({"kind":"unanimous","status":status,"childCount":count}),
        Marks::Mixed => json!({"kind":"mixed","childCount":count}),
    })
}
