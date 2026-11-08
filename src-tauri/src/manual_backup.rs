use crate::filesystem::Volume;
use crate::{filesystem, AppState};
use serde_json::Value;
use std::{cell::RefCell, collections::HashMap, path::PathBuf};

/// Results are shared only during one read/planning operation, so later watcher
/// events or source observations are always checked by the next operation.
#[derive(Default)]
pub struct ConfirmationCache(RefCell<HashMap<String, bool>>);

impl ConfirmationCache {
    pub fn current(&self, s: &AppState, decision: &Value, volume: &Volume) -> bool {
        let Some(relative) = decision["relativePath"].as_str() else {
            return false;
        };
        let Some(asserted_at) = decision["decidedAt"].as_str() else {
            return false;
        };
        let Some(fingerprint) = decision["snapshot"]["fingerprint"].as_str() else {
            return false;
        };
        if decision["volumeId"] != volume.id {
            return false;
        }
        let key = format!(
            "{}|{}|{asserted_at}|{fingerprint}",
            volume.id,
            relative.to_lowercase()
        );
        if let Some(current) = self.0.borrow().get(&key) {
            return *current;
        }
        let source = if relative.is_empty() {
            PathBuf::from(&volume.root)
        } else {
            PathBuf::from(&volume.root).join(relative)
        };
        let source = source.to_string_lossy().into_owned();
        let current =
            filesystem::guard_path(&source, &s.volumes, false).is_ok_and(|(path, _guards)| {
                let observed = filesystem::inspect(&path);
                observed.problem.is_none()
                    && observed.fingerprint == fingerprint
                    && !s.index.changed_below(&source, asserted_at).unwrap_or(true)
                    && !s.audit.changed_below(&source, asserted_at).unwrap_or(true)
                    && s.watcher_fault
                        .lock()
                        .map(|fault| fault.is_none())
                        .unwrap_or(false)
            });
        self.0.borrow_mut().insert(key, current);
        current
    }
}
