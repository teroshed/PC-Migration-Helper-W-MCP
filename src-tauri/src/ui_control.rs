//! Session-fenced commands to the live application shell. No DOM/script execution.
use crate::{
    store::{error, hash},
    AppState, MutationOrigin, Shared,
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Mutex};
use tauri::State;

const FRESH_MS: i64 = 15_000;
const MAX_REPORT: usize = 2 * 1024 * 1024;
const MAX_COMMANDS: usize = 1024;
const MAX_RESULT: usize = 64 * 1024;

#[derive(Default)]
pub(crate) struct UiControl {
    inner: Mutex<Session>,
}
#[derive(Default)]
struct Session {
    id: String,
    registration_id: String,
    reported_at: i64,
    state: Value,
    report_error: Option<String>,
    sequence: u64,
    commands: BTreeMap<String, Value>,
    inspection_tombstones: BTreeMap<String, String>,
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v[key]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 200)
        .ok_or_else(|| format!("Missing or invalid {key}"))
}
fn check_instance(s: &AppState, expected: &str) -> Result<(), String> {
    if s.gateway.lock().map_err(error)?["instanceId"].as_str() != Some(expected) {
        return Err(
            "The native app instance changed. Read the current UI session before continuing."
                .into(),
        );
    }
    Ok(())
}
fn pending_fingerprint(state: &Value) -> Result<String, String> {
    if state.is_null() {
        return Ok(hash(b"null"));
    }
    state["pendingFingerprint"]
        .as_str()
        .filter(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
        .map(str::to_owned)
        .ok_or("The UI report has no valid full pending-work fingerprint".into())
}
fn begin(
    s: &AppState,
    expected_instance: &str,
    registration_id: &str,
    previous: Option<&str>,
) -> Result<Value, String> {
    check_instance(s, expected_instance)?;
    if registration_id.is_empty() || registration_id.len() > 200 {
        return Err("Invalid UI registration identifier".into());
    }
    let mut session = s.review.ui.inner.lock().map_err(error)?;
    if session.registration_id == registration_id && !session.id.is_empty() {
        return Ok(json!({"registered":true,"uiSessionId":session.id}));
    }
    if (!session.id.is_empty() && previous != Some(session.id.as_str()))
        || (session.id.is_empty() && previous.is_some())
    {
        return Ok(json!({"registered":false,"currentUiSessionId":session.id}));
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(error)?;
    let id: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let invalidated = session
        .commands
        .values()
        .filter(|v| matches!(v["status"].as_str(), Some("accepted" | "delivered")))
        .count();
    *session = Session {
        id: id.clone(),
        registration_id: registration_id.into(),
        ..Session::default()
    };
    Ok(json!({"registered":true,"uiSessionId":id,"invalidatedCommands":invalidated}))
}
#[tauri::command]
pub(crate) fn begin_ui_session(
    state: State<'_, Shared>,
    expected_instance_id: String,
    registration_id: String,
    previous_ui_session_id: Option<String>,
) -> Result<Value, String> {
    begin(
        &state,
        &expected_instance_id,
        &registration_id,
        previous_ui_session_id.as_deref(),
    )
}
fn validate_command(s: &AppState, command: &Value) -> Result<(), String> {
    let fields = command.as_object().ok_or("UI command must be an object")?;
    let action = text(command, "action")?;
    let allowed: &[&str] = match action {
        "setFollowAI" | "setAutoCopy" => &["action", "enabled"],
        "openPanel" => &["action", "panel"],
        "closePanels" | "refreshVisible" | "searchKnownFolders" | "flushPending"
        | "retryPending" | "compareConflict" => &["action"],
        "setSavedRecords" => &["action", "search", "filter", "page"],
        "setNotifications" => &["action", "mode", "offset"],
        "setAssistanceOverride" => &["action", "open", "questionId"],
        "setCopyDetails" | "setGatewayDetails" => &["action", "open"],
        "setRowMenu" => &["action", "path"],
        "retryReviewed" => &[
            "action",
            "expectedComparisonRevision",
            "expectedPendingFingerprint",
        ],
        "dismissFeedback" => &["action", "kind"],
        "listPending" => &["action", "offset", "limit", "expectedPendingFingerprint"],
        "readPendingText" => &[
            "action",
            "operationId",
            "offset",
            "limit",
            "expectedPendingFingerprint",
        ],
        _ => return Err("Unsupported UI action".into()),
    };
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("Unsupported UI command field".into());
    }
    match action {
        "setFollowAI" | "setAutoCopy" if !command["enabled"].is_boolean() => {
            return Err("enabled must be true or false".into())
        }
        "setCopyDetails" | "setGatewayDetails" | "setAssistanceOverride"
            if !command["open"].is_boolean() =>
        {
            return Err("open must be true or false".into())
        }
        "openPanel"
            if !matches!(
                command["panel"].as_str(),
                Some(
                    "assistance"
                        | "decision"
                        | "context"
                        | "history"
                        | "saved"
                        | "gaps"
                        | "gateway"
                        | "conflict"
                )
            ) =>
        {
            return Err("Unsupported panel".into())
        }
        "dismissFeedback" if !matches!(command["kind"].as_str(), Some("status" | "error")) => {
            return Err("Unsupported feedback kind".into())
        }
        "setNotifications" => {
            if !matches!(
                command["mode"].as_str(),
                Some("closed" | "preview" | "full")
            ) {
                return Err("Unsupported notifications mode".into());
            }
            if command
                .get("offset")
                .is_some_and(|v| !v.as_u64().is_some_and(|n| n <= 10_000 && n % 50 == 0))
            {
                return Err("Notification offset must be a multiple of 50, up to 10000".into());
            }
        }
        "setSavedRecords" => {
            if command
                .get("search")
                .is_some_and(|v| !v.as_str().is_some_and(|v| v.len() <= 4000))
            {
                return Err("Invalid saved search".into());
            }
            if command.get("filter").is_some_and(|v| {
                !v.as_str()
                    .is_some_and(|v| v == "all" || crate::store::valid_status(v))
            }) {
                return Err("Invalid saved filter".into());
            }
            if command
                .get("page")
                .is_some_and(|v| !v.as_u64().is_some_and(|n| n <= 10_000_000))
            {
                return Err("Invalid saved page".into());
            }
        }
        "setRowMenu" => {
            if !command["path"].is_null() {
                let path = command["path"].as_str().ok_or("Invalid menu path")?;
                crate::filesystem::safe(path, &s.volumes, false)?;
            }
        }
        "retryReviewed" => {
            command["expectedComparisonRevision"]
                .as_u64()
                .ok_or("Missing comparison revision")?;
            text(command, "expectedPendingFingerprint")?;
        }
        "listPending" | "readPendingText" => {
            let offset = command["offset"]
                .as_u64()
                .ok_or("Missing inspection offset")?;
            let limit = command["limit"]
                .as_u64()
                .ok_or("Missing inspection limit")?;
            if action == "listPending" && (offset > 10_000_000 || !(1..=32).contains(&limit)) {
                return Err("Pending list page is outside the supported range".into());
            }
            if action == "readPendingText"
                && (offset > 1_000_000_000 || !(4..=16_384).contains(&limit))
            {
                return Err("Pending text range is outside the supported range".into());
            }
            let expected = text(command, "expectedPendingFingerprint")?;
            if expected.len() != 64
                || !expected
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("Invalid pending fingerprint".into());
            }
            if action == "readPendingText" {
                text(command, "operationId")?;
            }
        }
        _ => {}
    }
    if action == "setAssistanceOverride" {
        text(command, "questionId")?;
    }
    Ok(())
}
pub(crate) fn status(s: &AppState) -> Result<Value, String> {
    let instance = s.gateway.lock().map_err(error)?["instanceId"].clone();
    let revision = crate::document(s)?["revision"].clone();
    let mut session = s.review.ui.inner.lock().map_err(error)?;
    expire(&mut session);
    let mut recent: Vec<&Value> = session.commands.values().collect();
    recent.sort_by_key(|v| std::cmp::Reverse(v["sequence"].as_u64().unwrap_or(0)));
    let full_receipts = session.commands.len();
    let expired_inspections = session.inspection_tombstones.len();
    let total = full_receipts + expired_inspections;
    let operations:Vec<Value>=recent.into_iter().take(32).enumerate().map(|(index,v)|{
        let mut receipt=json!({"operationId":v["operationId"],"status":v["status"],"error":v["error"],"finishedAtMs":v["finishedAtMs"],"resultAvailable":!v["result"].is_null()});
        if index<8 && !v["result"].is_null() { receipt["result"]=v["result"].clone(); }
        else if !v["result"].is_null() { receipt["resultOmitted"]=json!(true); }
        receipt
    }).collect();
    Ok(
        json!({"instanceId":instance,"revision":revision,"uiSessionId":session.id,"reportedAtMs":session.reported_at,"reportError":session.report_error,
        "sessionState":if session.id.is_empty(){"waiting"}else if now()-session.reported_at>FRESH_MS{"stale"}else{"current"},
        "state":session.state,"pendingFingerprint":pending_fingerprint(&session.state)?,
        "operations":operations,"operationCount":total,"operationsTruncated":total>32,
        "receiptRetention":{"fullReceipts":full_receipts,"expiredReadOnlyInspections":expired_inspections},
        "resultReadback":"Recent eight receipts include results. Exact control_ui replay reads retained receipts without executing. Evicted completed read-only inspections return resultExpired; a fresh fingerprint-fenced read can continue without restarting the UI."}),
    )
}
fn expire(session: &mut Session) {
    for command in session.commands.values_mut() {
        if matches!(command["status"].as_str(), Some("accepted" | "delivered"))
            && command["expiresAtMs"].as_i64().is_some_and(|at| now() > at)
        {
            command["status"] = json!("error");
            command["error"]=json!("UI command expired before acknowledgement. Inspect current state; it will not be replayed.");
            command["finishedAtMs"] = json!(now());
        }
    }
}
fn expired_inspection(
    session: &Session,
    operation_id: &str,
    payload_hash: &str,
) -> Result<Option<Value>, String> {
    let Some(previous) = session.inspection_tombstones.get(operation_id) else {
        return Ok(None);
    };
    if previous != payload_hash {
        return Err(
            "UI operationId already belongs to a different expired inspection request".into(),
        );
    }
    Ok(Some(
        json!({"operationId":operation_id,"status":"error","replayed":true,"resultExpired":true,
        "error":"This completed read-only inspection result expired. Read the current pending fingerprint and use a fresh operationId to inspect it again. The old command was not executed again."}),
    ))
}
fn is_inspection(command: &Value) -> bool {
    matches!(
        command["request"]["command"]["action"].as_str(),
        Some("listPending" | "readPendingText")
    )
}
fn make_receipt_space(session: &mut Session, inspection: bool) -> Result<(), String> {
    while session.commands.len() >= MAX_COMMANDS {
        let oldest = session
            .commands
            .iter()
            .filter(|(_, v)| {
                is_inspection(v) && matches!(v["status"].as_str(), Some("applied" | "error"))
            })
            .min_by_key(|(_, v)| v["sequence"].as_u64().unwrap_or(0))
            .map(|(id, _)| id.clone());
        if let Some(id) = oldest {
            let previous = session
                .commands
                .remove(&id)
                .ok_or("Inspection receipt disappeared")?;
            session.inspection_tombstones.insert(
                id,
                previous["payloadHash"]
                    .as_str()
                    .ok_or("Invalid inspection receipt hash")?
                    .into(),
            );
        } else if inspection && session.commands.len() < MAX_COMMANDS + 32 {
            // Reserve bounded transient capacity for read-only access even when
            // all durable session controls already occupy the normal slots.
            return Ok(());
        } else {
            return Err("The session command receipt limit was reached. Wait for pending read-only inspection receipts; mutating receipts remain protected.".into());
        }
    }
    Ok(())
}
pub(crate) fn request(
    s: &AppState,
    request: &Value,
    origin: MutationOrigin,
) -> Result<Value, String> {
    origin.validate("ui")?;
    if matches!(&origin,MutationOrigin::Agent{basis,..} if basis!="user_instruction") {
        return Err("UI control requires an explicit user instruction".into());
    }
    let fields = request.as_object().ok_or("UI request must be an object")?;
    if fields.keys().any(|k| {
        !matches!(
            k.as_str(),
            "operationId"
                | "expectedInstanceId"
                | "expectedUiSessionId"
                | "expectedRevision"
                | "command"
                | "confirmed"
                | "expectedQueueSignature"
                | "expectedSourceVolumeSignature"
                | "expectedDestinationId"
        )
    }) {
        return Err("Unsupported UI request field".into());
    }
    let operation_id = text(request, "operationId")?;
    check_instance(s, text(request, "expectedInstanceId")?)?;
    let expected_session = text(request, "expectedUiSessionId")?;
    let command = &request["command"];
    validate_command(s, command)?;
    let payload_hash = hash(
        &serde_json::to_vec(&crate::canonical_json(
            &json!({"request":request,"origin":origin.payload_hash(request)?}),
        ))
        .map_err(error)?,
    );
    {
        let mut session = s.review.ui.inner.lock().map_err(error)?;
        expire(&mut session);
        if session.id != expected_session || now() - session.reported_at > FRESH_MS {
            return Err(
                "The UI session changed or stopped reporting. Read get_ui_state before continuing."
                    .into(),
            );
        }
        if let Some(previous) = expired_inspection(&session, operation_id, &payload_hash)? {
            return Ok(previous);
        }
        if let Some(previous) = session.commands.get(operation_id) {
            if previous["payloadHash"] != payload_hash {
                return Err("UI operationId already belongs to a different request".into());
            }
            return Ok(
                json!({"operationId":operation_id,"status":previous["status"],"error":previous["error"],"result":previous["result"],"replayed":true}),
            );
        }
    }
    // Copy validation takes its own lock before the UI lock; no engine runs here.
    let enabling_auto = command["action"] == "setAutoCopy" && command["enabled"] == true;
    if enabling_auto {
        crate::copy_control::validate_fences(s, request)?;
    }
    let mut session = s.review.ui.inner.lock().map_err(error)?;
    expire(&mut session);
    if session.id != expected_session || now() - session.reported_at > FRESH_MS {
        return Err(
            "The UI session changed or stopped reporting. Read get_ui_state before continuing."
                .into(),
        );
    }
    if let Some(previous) = expired_inspection(&session, operation_id, &payload_hash)? {
        return Ok(previous);
    }
    if let Some(previous) = session.commands.get(operation_id) {
        if previous["payloadHash"] != payload_hash {
            return Err("UI operationId already belongs to a different request".into());
        }
        return Ok(
            json!({"operationId":operation_id,"status":previous["status"],"error":previous["error"],"result":previous["result"],"replayed":true}),
        );
    }
    let revision = crate::document(s)?["revision"]
        .as_u64()
        .ok_or("Invalid current revision")?;
    if request["expectedRevision"].as_u64() != Some(revision) {
        return Err("The review changed. Inspect the current UI state before continuing.".into());
    }
    if matches!(
        command["action"].as_str(),
        Some("listPending" | "readPendingText")
    ) && command["expectedPendingFingerprint"] != pending_fingerprint(&session.state)?
    {
        return Err(
            "Pending work changed. Read the current fingerprint before inspecting it.".into(),
        );
    }
    if command["action"] == "retryReviewed"
        && (command["expectedPendingFingerprint"] != pending_fingerprint(&session.state)?
            || command["expectedComparisonRevision"]
                != session.state["conflict"]["comparisonRevision"]
            || command["expectedComparisonRevision"].as_u64() != Some(revision))
    {
        return Err(
            "Pending edits or the reviewed comparison changed. Inspect and compare again.".into(),
        );
    }
    make_receipt_space(
        &mut session,
        matches!(
            command["action"].as_str(),
            Some("listPending" | "readPendingText")
        ),
    )?;
    session.sequence = session
        .sequence
        .checked_add(1)
        .ok_or("UI command sequence exhausted")?;
    let mut entry = json!({"operationId":operation_id,"payloadHash":payload_hash,"request":request,"status":"accepted","acceptedAtMs":now(),"expiresAtMs":now()+FRESH_MS});
    entry["sequence"] = json!(session.sequence);
    entry["uiSessionId"] = json!(session.id);
    session.commands.insert(operation_id.into(), entry);
    Ok(json!({"operationId":operation_id,"status":"accepted","replayed":false}))
}
#[tauri::command]
pub(crate) fn report_ui_state(state: State<'_, Shared>, report: Value) -> Result<(), String> {
    report_state(&state, report)
}
fn report_state(state: &AppState, report: Value) -> Result<(), String> {
    check_instance(&state, text(&report, "expectedInstanceId")?)?;
    let id = text(&report, "uiSessionId")?;
    let mut session = state.review.ui.inner.lock().map_err(error)?;
    if session.id != id {
        return Err(
            "This UI session was replaced. Reports from the previous window cannot reclaim it."
                .into(),
        );
    }
    if !report["state"].is_object()
        || serde_json::to_vec(&report).map_err(error)?.len() > MAX_REPORT
    {
        let problem="UI report exceeds the 2 MiB state limit or is invalid. The last accepted state is retained; no text was silently truncated.".to_string();
        session.report_error = Some(problem.clone());
        return Err(problem);
    }
    pending_fingerprint(&report["state"])?;
    // Acknowledgements cannot select a different request or revive an expired command.
    expire(&mut session);
    if let Some(acks) = report["acknowledgements"].as_array() {
        if acks.len() > 32 {
            return Err("Too many UI acknowledgements".into());
        }
        for ack in acks {
            let command = session
                .commands
                .get_mut(text(ack, "operationId")?)
                .ok_or("Unknown UI acknowledgement")?;
            if command["status"] != "delivered" {
                continue;
            }
            if !matches!(ack["status"].as_str(), Some("applied" | "error")) {
                return Err("Invalid acknowledgement status".into());
            }
            if let Some(result) = ack.get("result") {
                if serde_json::to_vec(result).map_err(error)?.len() > MAX_RESULT {
                    return Err("UI command result exceeds the 64 KiB acknowledgement limit".into());
                }
                command["result"] = result.clone();
            }
            command["status"] = ack["status"].clone();
            command["error"] = ack
                .get("error")
                .and_then(Value::as_str)
                .map(|v| json!(v.chars().take(8000).collect::<String>()))
                .unwrap_or(Value::Null);
            command["finishedAtMs"] = json!(now());
        }
    }
    session.reported_at = now();
    session.state = report["state"].clone();
    session.report_error = None;
    Ok(())
}
#[tauri::command]
pub(crate) fn poll_ui_commands(
    state: State<'_, Shared>,
    expected_instance_id: String,
    ui_session_id: String,
) -> Result<Value, String> {
    check_instance(&state, &expected_instance_id)?;
    let revision = crate::document(&state)?["revision"].clone();
    let mut session = state.review.ui.inner.lock().map_err(error)?;
    if session.id != ui_session_id || now() - session.reported_at > FRESH_MS {
        return Err("The UI session is no longer current".into());
    }
    expire(&mut session);
    let mut commands = Vec::new();
    let fingerprint = pending_fingerprint(&session.state)?;
    let accepted: Vec<String> = session
        .commands
        .iter()
        .filter(|(_, command)| command["status"] == "accepted")
        .map(|(id, _)| id.clone())
        .collect();
    let mut accepted = accepted;
    accepted.sort_by_key(|id| session.commands[id]["sequence"].as_u64().unwrap_or(0));
    for id in accepted {
        if let Some(command) = session.commands.get_mut(&id) {
            if command["request"]["expectedRevision"] != revision {
                command["status"] = json!("error");
                command["error"]=json!("The saved revision changed before the UI command was delivered. Read current state again.");
                command["finishedAtMs"] = json!(now());
                continue;
            }
            command["status"] = json!("delivered");
            commands.push(json!({"operationId":command["operationId"],"request":command["request"],"pendingFingerprint":fingerprint}));
            break; // Shell applies requests in a bounded serial queue.
        }
    }
    Ok(json!({"commands":commands}))
}
#[tauri::command]
pub(crate) fn validate_auto_copy(
    state: State<'_, Shared>,
    request: Value,
) -> Result<Value, String> {
    crate::copy_control::validate_fences(&state, &request)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pending_state() -> Value {
        json!({"writer":{"pending":0},"drafts":{},"pendingFingerprint":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"})
    }
    #[test]
    fn session_and_replay_fences_preserve_exact_ui_request() {
        let s = crate::tests::fixture();
        *s.gateway.lock().unwrap() = json!({"instanceId":"ui-test"});
        {
            let mut ui = s.review.ui.inner.lock().unwrap();
            ui.id = "session-test".into();
            ui.reported_at = now();
            ui.state = pending_state();
        }
        let mut r = json!({"operationId":"toggle-follow","expectedInstanceId":"ui-test","expectedUiSessionId":"session-test","expectedRevision":crate::document(&s).unwrap()["revision"],"command":{"action":"setFollowAI","enabled":true}});
        assert_eq!(
            request(&s, &r, MutationOrigin::ManualUi).unwrap()["status"],
            "accepted"
        );
        assert_eq!(
            request(&s, &r, MutationOrigin::ManualUi).unwrap()["replayed"],
            true
        );
        r["command"]["enabled"] = json!(false);
        assert!(request(&s, &r, MutationOrigin::ManualUi).is_err());
        r["operationId"] = json!("other");
        r["expectedUiSessionId"] = json!("old-session");
        assert!(request(&s, &r, MutationOrigin::ManualUi).is_err());
    }
    #[test]
    fn suggestions_cannot_control_shell_and_arbitrary_actions_are_rejected() {
        let s = crate::tests::fixture();
        let r = json!({"command":{"action":"closePanels"}});
        assert!(request(
            &s,
            &r,
            MutationOrigin::Agent {
                basis: "assistant_suggestion".into(),
                reason: "proposal".into()
            }
        )
        .is_err());
        assert!(validate_command(&s, &json!({"action":"evaluate","script":"anything"})).is_err());
        assert!(validate_command(
            &s,
            &json!({"action":"setNotifications","mode":"full","offset":1})
        )
        .is_err());
    }
    #[test]
    fn reload_replaces_session_and_late_old_reports_cannot_reclaim_it() {
        let s = crate::tests::fixture();
        *s.gateway.lock().unwrap() = json!({"instanceId":"ui-test"});
        let first = begin(&s, "ui-test", "first-renderer", None).unwrap();
        let first_id = first["uiSessionId"].as_str().unwrap();
        report_state(
            &s,
            json!({"expectedInstanceId":"ui-test","uiSessionId":first_id,"state":pending_state()}),
        )
        .unwrap();
        request(&s,&json!({"operationId":"old-pending","expectedInstanceId":"ui-test","expectedUiSessionId":first_id,"expectedRevision":crate::document(&s).unwrap()["revision"],"command":{"action":"closePanels"}}),MutationOrigin::ManualUi).unwrap();
        let replacement = begin(&s, "ui-test", "replacement-renderer", Some(first_id)).unwrap();
        assert_eq!(replacement["invalidatedCommands"], 1);
        let second_id = replacement["uiSessionId"].as_str().unwrap();
        assert_ne!(first_id, second_id);
        assert!(report_state(
            &s,
            json!({"expectedInstanceId":"ui-test","uiSessionId":first_id,"state":{}})
        )
        .is_err());
        assert_eq!(
            begin(&s, "ui-test", "delayed-old-registration", Some(first_id)).unwrap()["registered"],
            false
        );
        report_state(
            &s,
            json!({"expectedInstanceId":"ui-test","uiSessionId":second_id,"state":pending_state()}),
        )
        .unwrap();
        assert_eq!(status(&s).unwrap()["uiSessionId"], second_id);
        assert!(request(&s,&json!({"operationId":"stale-inspection","expectedInstanceId":"ui-test","expectedUiSessionId":first_id,"expectedRevision":crate::document(&s).unwrap()["revision"],"command":{"action":"listPending","offset":0,"limit":1,"expectedPendingFingerprint":pending_state()["pendingFingerprint"]}}),MutationOrigin::ManualUi).is_err());
    }
    #[test]
    fn pending_inspection_results_and_status_projection_are_bounded() {
        let s = crate::tests::fixture();
        *s.gateway.lock().unwrap() = json!({"instanceId":"ui-test"});
        let registration = begin(&s, "ui-test", "inspection-renderer", None).unwrap();
        let id = registration["uiSessionId"].as_str().unwrap();
        report_state(
            &s,
            json!({"expectedInstanceId":"ui-test","uiSessionId":id,"state":pending_state()}),
        )
        .unwrap();
        let mut req = json!({"operationId":"inspect","expectedInstanceId":"ui-test","expectedUiSessionId":id,"expectedRevision":crate::document(&s).unwrap()["revision"],"command":{"action":"listPending","offset":0,"limit":1,"expectedPendingFingerprint":pending_state()["pendingFingerprint"]}});
        request(&s, &req, MutationOrigin::ManualUi).unwrap();
        {
            let mut ui = s.review.ui.inner.lock().unwrap();
            ui.commands.get_mut("inspect").unwrap()["status"] = json!("delivered");
        }
        assert!(report_state(&s,json!({"expectedInstanceId":"ui-test","uiSessionId":id,"state":pending_state(),"acknowledgements":[{"operationId":"inspect","status":"applied","result":{"text":"x".repeat(MAX_RESULT)}}]})).is_err());
        report_state(&s,json!({"expectedInstanceId":"ui-test","uiSessionId":id,"state":pending_state(),"acknowledgements":[{"operationId":"inspect","status":"applied","result":{"entries":[],"total":0}}]})).unwrap();
        assert_eq!(
            request(&s, &req, MutationOrigin::ManualUi).unwrap()["result"]["total"],
            0
        );
        req["operationId"] = json!("changed-fingerprint");
        req["command"]["expectedPendingFingerprint"] = json!("b".repeat(64));
        assert!(request(&s, &req, MutationOrigin::ManualUi).is_err());
        {
            let mut ui = s.review.ui.inner.lock().unwrap();
            for sequence in 2..=42 {
                ui.commands.insert(format!("receipt-{sequence}"),json!({"operationId":format!("receipt-{sequence}"),"sequence":sequence,"status":"applied","result":{"text":"x".repeat(60_000)}}));
            }
        }
        let response = status(&s).unwrap();
        assert_eq!(response["operations"].as_array().unwrap().len(), 32);
        assert_eq!(
            response["operations"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|v| v.get("result").is_some())
                .count(),
            8
        );
        assert!(serde_json::to_vec(&response).unwrap().len() < 600_000);
        assert_eq!(response["operationsTruncated"], true);
    }
    #[test]
    fn large_exports_evict_only_completed_inspections_and_reject_reused_payload_ids() {
        let s = crate::tests::fixture();
        *s.gateway.lock().unwrap() = json!({"instanceId":"ui-test"});
        let registration = begin(&s, "ui-test", "export-renderer", None).unwrap();
        let id = registration["uiSessionId"].as_str().unwrap();
        report_state(
            &s,
            json!({"expectedInstanceId":"ui-test","uiSessionId":id,"state":pending_state()}),
        )
        .unwrap();
        let mut old = json!({"operationId":"first-inspection","expectedInstanceId":"ui-test","expectedUiSessionId":id,"expectedRevision":crate::document(&s).unwrap()["revision"],"command":{"action":"listPending","offset":0,"limit":1,"expectedPendingFingerprint":pending_state()["pendingFingerprint"]}});
        request(&s, &old, MutationOrigin::ManualUi).unwrap();
        {
            let mut ui = s.review.ui.inner.lock().unwrap();
            ui.commands.get_mut("first-inspection").unwrap()["status"] = json!("applied");
            for sequence in 2..=MAX_COMMANDS {
                ui.commands.insert(format!("protected-{sequence}"),json!({"operationId":format!("protected-{sequence}"),"sequence":sequence,"status":"applied","request":{"command":{"action":"setFollowAI"}}}));
            }
            ui.sequence = MAX_COMMANDS as u64;
        }
        let mut next = old.clone();
        next["operationId"] = json!("next-inspection");
        request(&s, &next, MutationOrigin::ManualUi).unwrap();
        assert_eq!(
            request(&s, &old, MutationOrigin::ManualUi).unwrap()["resultExpired"],
            true
        );
        old["command"]["limit"] = json!(2);
        assert!(request(&s, &old, MutationOrigin::ManualUi).is_err());
        let ui = s.review.ui.inner.lock().unwrap();
        assert!(ui.commands.contains_key("protected-2"));
        assert_eq!(
            ui.commands.values().filter(|v| !is_inspection(v)).count(),
            MAX_COMMANDS - 1
        );
        assert_eq!(ui.inspection_tombstones.len(), 1);
    }
}
