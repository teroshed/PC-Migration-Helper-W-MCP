//! Session-fenced commands to the live application shell. No DOM/script execution.
use crate::{
    store::{error, hash},
    AppState, MutationOrigin, Shared,
};
use serde_json::{json, Value};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::{collections::BTreeMap, fs, path::PathBuf, sync::Mutex};
use tauri::State;

const FRESH_MS: i64 = 15_000;
const MAX_REPORT: usize = 2 * 1024 * 1024;
const MAX_COMMANDS: usize = 1024;
const MAX_RESULT: usize = 64 * 1024;
const READ_RESERVE: usize = 32;
const ARCHIVE_BATCH: usize = 128;

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
    archived_receipts: usize,
    archived_inspections: usize,
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
fn validate_command_structure(command: &Value) -> Result<(), String> {
    let fields = command.as_object().ok_or("UI command must be an object")?;
    let action = text(command, "action")?;
    let allowed: &[&str] = match action {
        "setFollowAI" | "setAutoCopy" => &["action", "enabled"],
        "openPanel" => &["action", "panel"],
        "closePanels" | "refreshVisible" | "searchKnownFolders" | "flushPending"
        | "retryPending" | "compareConflict" | "closeWindow" | "exitApp" => &["action"],
        "retryExit" | "cancelExit" => &["action", "expectedExitTicket"],
        "setSavedRecords" => &["action", "search", "filter", "page"],
        "setNotifications" => &["action", "mode", "offset"],
        "setAssistanceOverride" => &["action", "open", "questionId"],
        "setCopyDetails" | "setGatewayDetails" => &["action", "open"],
        "setRowMenu" => &["action", "path"],
        "setEntrySelection" => &["action", "path", "selected", "expectedSelectionFingerprint"],
        "clearEntrySelection" | "openSelectionReview" => &["action", "expectedSelectionFingerprint"],
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
        "retryExit" | "cancelExit" => {
            if !command["expectedExitTicket"]
                .as_u64()
                .is_some_and(|ticket| ticket > 0 && ticket <= 9_007_199_254_740_991)
            {
                return Err("A current safe-integer exit ticket is required".into());
            }
        }
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
                crate::filesystem::normalize(path)?;
            }
        }
        "setEntrySelection" | "clearEntrySelection" | "openSelectionReview" => {
            let expected = text(command, "expectedSelectionFingerprint")?;
            if expected.len() != 64 || !expected.bytes().all(|byte|
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("A current selection SHA-256 fingerprint is required".into());
            }
            if action == "setEntrySelection" {
                if !command["selected"].is_boolean() {
                    return Err("selected must be true or false".into());
                }
                let path = text(command, "path")?;
                crate::filesystem::normalize(path)?;
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
fn validate_command(s: &AppState, command: &Value) -> Result<(), String> {
    validate_command_structure(command)?;
    if matches!(command["action"].as_str(), Some("setRowMenu" | "setEntrySelection")) {
        if let Some(path) = command["path"].as_str() {
            crate::filesystem::safe(path, &s.volumes, false)?;
        }
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
    let expired_inspections = session.archived_inspections;
    let total = full_receipts.checked_add(session.archived_receipts).ok_or("UI receipt count exhausted")?;
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
        "receiptRetention":{"fullReceipts":full_receipts,"archivedReceipts":session.archived_receipts,"expiredReadOnlyInspections":expired_inspections},
        "resultReadback":"Recent eight receipts include results. Exact control_ui replay reads retained or archived receipts without executing. Archived receipts retain terminal status, error and time; result payloads expire. A fresh fingerprint-fenced inspection can read current pending work."}),
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
fn is_inspection(command: &Value) -> bool {
    matches!(
        command["request"]["command"]["action"].as_str(),
        Some("listPending" | "readPendingText")
    )
}
fn archive_path(s: &AppState, session: &Session) -> PathBuf {
    s.runtime
        .join(format!("ui-receipts-{}.sqlite", hash(session.id.as_bytes())))
}
fn archive_error(cause: impl std::fmt::Display) -> String {
    format!("UI receipt archive unavailable. No new command was admitted: {cause}")
}
fn open_archive(
    s: &AppState,
    session: &Session,
    instance_id: &str,
    write: bool,
) -> Result<Connection, String> {
    let path = archive_path(s, session);
    let initialize = write && session.archived_receipts == 0;
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_file() || metadata.file_type().is_symlink() => {
            return Err(archive_error("archive path is not an ordinary file"));
        }
        Ok(_) => {}
        Err(cause) if initialize && cause.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(&s.runtime).map_err(archive_error)?;
        }
        Err(cause) => return Err(archive_error(cause)),
    }
    let mut flags = if write { OpenFlags::SQLITE_OPEN_READ_WRITE } else { OpenFlags::SQLITE_OPEN_READ_ONLY };
    if initialize { flags |= OpenFlags::SQLITE_OPEN_CREATE; }
    let connection = Connection::open_with_flags(path, flags).map_err(archive_error)?;
    connection.busy_timeout(std::time::Duration::from_secs(2)).map_err(archive_error)?;
    if write { connection.execute_batch("PRAGMA synchronous=FULL;").map_err(archive_error)?; }
    if initialize {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS ui_archive_identity (
                slot INTEGER PRIMARY KEY CHECK(slot=1), instance_id TEXT NOT NULL, ui_session_id TEXT NOT NULL,
                receipt_count INTEGER NOT NULL, inspection_count INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS ui_archive_receipts (
                operation_id TEXT PRIMARY KEY, payload_hash TEXT NOT NULL,
                status TEXT NOT NULL CHECK(status IN ('applied','error')), error TEXT,
                accepted_at_ms INTEGER NOT NULL, finished_at_ms INTEGER NOT NULL,
                inspection INTEGER NOT NULL, had_result INTEGER NOT NULL
            ) WITHOUT ROWID;",
        ).map_err(archive_error)?;
        connection.execute(
            "INSERT OR IGNORE INTO ui_archive_identity VALUES(1,?1,?2,0,0)",
            params![instance_id, session.id],
        ).map_err(archive_error)?;
    }
    let identity: (String, String, i64, i64) = connection.query_row(
        "SELECT instance_id,ui_session_id,receipt_count,inspection_count FROM ui_archive_identity WHERE slot=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).map_err(archive_error)?;
    if identity.0 != instance_id || identity.1 != session.id
        || usize::try_from(identity.2).ok() != Some(session.archived_receipts)
        || usize::try_from(identity.3).ok() != Some(session.archived_inspections)
    {
        return Err(archive_error("archive identity or counts changed"));
    }
    Ok(connection)
}
fn previous_receipt(
    s: &AppState,
    session: &Session,
    instance_id: &str,
    operation_id: &str,
    payload_hash: &str,
) -> Result<Option<Value>, String> {
    if let Some(previous) = session.commands.get(operation_id) {
        if previous["payloadHash"] != payload_hash {
            return Err("UI operationId already belongs to a different request".into());
        }
        return Ok(Some(json!({"operationId":operation_id,"status":previous["status"],"error":previous["error"],
            "acceptedAtMs":previous["acceptedAtMs"],"finishedAtMs":previous["finishedAtMs"],
            "result":previous["result"],"replayed":true})));
    }
    if session.archived_receipts == 0 { return Ok(None); }
    let connection = open_archive(s, session, instance_id, false)?;
    let archived = connection.query_row(
        "SELECT payload_hash,status,error,accepted_at_ms,finished_at_ms,inspection,had_result
         FROM ui_archive_receipts WHERE operation_id=?1",
        [operation_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?, row.get::<_, i64>(4)?, row.get::<_, bool>(5)?, row.get::<_, bool>(6)?)),
    ).optional().map_err(archive_error)?;
    let Some((previous, status, error, accepted, finished, inspection, had_result)) = archived else { return Ok(None); };
    if previous != payload_hash { return Err("UI operationId already belongs to a different archived request".into()); }
    let mut receipt = json!({"operationId":operation_id,"status":status,"error":error,
        "acceptedAtMs":accepted,"finishedAtMs":finished,"result":null,"replayed":true,"archived":true,
        "resultExpired":inspection || had_result});
    if inspection {
        receipt["readbackNotice"] = json!("This read-only result expired. Use a fresh operationId and the current pending fingerprint to inspect again. The archived command was not delivered again.");
    }
    Ok(Some(receipt))
}
fn archive_completed(
    s: &AppState,
    session: &mut Session,
    instance_id: &str,
    ids: &[String],
) -> Result<(), String> {
    let archived_count = session.archived_receipts.checked_add(ids.len()).ok_or("UI receipt count exhausted")?;
    let inspections = ids.iter().filter(|id| is_inspection(&session.commands[*id])).count();
    let inspection_count = session.archived_inspections.checked_add(inspections).ok_or("UI inspection count exhausted")?;
    let mut connection = open_archive(s, session, instance_id, true)?;
    let transaction = connection.transaction().map_err(archive_error)?;
    {
        let mut insert = transaction.prepare(
            "INSERT INTO ui_archive_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
        ).map_err(archive_error)?;
        for id in ids {
            let receipt = &session.commands[id];
            let payload_hash = receipt["payloadHash"].as_str().filter(|v| v.len() == 64)
                .ok_or_else(|| archive_error("invalid full receipt hash"))?;
            let status = receipt["status"].as_str().filter(|v| matches!(*v, "applied" | "error"))
                .ok_or_else(|| archive_error("receipt is not terminal"))?;
            let accepted = receipt["acceptedAtMs"].as_i64().ok_or_else(|| archive_error("missing accepted time"))?;
            let finished = receipt["finishedAtMs"].as_i64().ok_or_else(|| archive_error("missing terminal time"))?;
            insert.execute(params![id, payload_hash, status, receipt["error"].as_str(), accepted, finished,
                is_inspection(receipt), !receipt["result"].is_null()]).map_err(archive_error)?;
        }
    }
    transaction.execute("UPDATE ui_archive_identity SET receipt_count=?1,inspection_count=?2 WHERE slot=1",
        params![i64::try_from(archived_count).map_err(archive_error)?, i64::try_from(inspection_count).map_err(archive_error)?],
    ).map_err(archive_error)?;
    // Full receipts remain authoritative unless the entire compact batch commits.
    transaction.commit().map_err(archive_error)?;
    for id in ids { session.commands.remove(id); }
    session.archived_receipts = archived_count;
    session.archived_inspections = inspection_count;
    Ok(())
}
fn make_receipt_space(s: &AppState, session: &mut Session, instance_id: &str, inspection: bool) -> Result<(), String> {
    if session.commands.len() < MAX_COMMANDS { return Ok(()); }
    let mut terminal: Vec<_> = session.commands.iter().filter(|(_, receipt)| {
        matches!(receipt["status"].as_str(), Some("applied" | "error"))
    }).collect();
    terminal.sort_by_key(|(_, receipt)| (!is_inspection(receipt), receipt["sequence"].as_u64().unwrap_or(0)));
    let batch = ARCHIVE_BATCH.max(session.commands.len() - MAX_COMMANDS + 1);
    let ids: Vec<_> = terminal.into_iter().take(batch).map(|(id, _)| id.clone()).collect();
    if !ids.is_empty() { archive_completed(s, session, instance_id, &ids)?; }
    if session.commands.len() < MAX_COMMANDS
        || inspection && session.commands.len() < MAX_COMMANDS + READ_RESERVE
    {
        return Ok(());
    }
    Err("The session command receipt limit is occupied by pending work. Wait for acknowledgements before admitting more commands.".into())
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
    let expected_instance = text(request, "expectedInstanceId")?;
    check_instance(s, expected_instance)?;
    let expected_session = text(request, "expectedUiSessionId")?;
    request["expectedRevision"].as_u64().ok_or("Missing or invalid expectedRevision")?;
    let command = &request["command"];
    validate_command_structure(command)?;
    if matches!(command["action"].as_str(), Some("closeWindow" | "exitApp" | "retryExit"))
        && request["confirmed"] != true
    {
        return Err("Explicit confirmation is required to close or exit the app".into());
    }
    let payload_hash = hash(
        &serde_json::to_vec(&crate::canonical_json(
            &json!({"request":request,"origin":origin.payload_hash(request)?}),
        ))
        .map_err(error)?,
    );
    {
        let mut session = s.review.ui.inner.lock().map_err(error)?;
        expire(&mut session);
        if session.id != expected_session {
            return Err(
                "The UI session changed or stopped reporting. Read get_ui_state before continuing."
                    .into(),
            );
        }
        if let Some(previous) = previous_receipt(s, &session, expected_instance, operation_id, &payload_hash)? {
            return Ok(previous);
        }
        if now() - session.reported_at > FRESH_MS {
            return Err(
                "The UI session changed or stopped reporting. Read get_ui_state before continuing."
                    .into(),
            );
        }
    }
    // Current targets can disappear after an action completed. Exact receipts were
    // checked first, including when the renderer's latest report is stale.
    validate_command(s, command)?;
    // Copy validation takes its own lock before the UI lock; no engine runs here.
    let enabling_auto = command["action"] == "setAutoCopy" && command["enabled"] == true;
    if enabling_auto {
        crate::copy_control::validate_fences(s, request)?;
    }
    // Read lifecycle state before taking the UI lock. Exact receipt replays above
    // do not need a ticket that may already have been canceled or consumed.
    if matches!(command["action"].as_str(), Some("closeWindow" | "exitApp" | "retryExit" | "cancelExit")) {
        let lifecycle = crate::background::status(s)?;
        let retry = matches!(command["action"].as_str(), Some("retryExit" | "cancelExit"));
        if !retry && (!lifecycle["pending"].is_null() || lifecycle["draining"] == true) {
            return Err("An exit attempt already owns admission. Use its current ticket to retry or cancel.".into());
        }
        if retry && (lifecycle["pending"]["ticket"] != command["expectedExitTicket"]
            || lifecycle["draining"] == true) {
            return Err("The exit ticket changed or work is still stopping. Inspect the current UI state.".into());
        }
    }
    let mut session = s.review.ui.inner.lock().map_err(error)?;
    expire(&mut session);
    if session.id != expected_session {
        return Err(
            "The UI session changed or stopped reporting. Read get_ui_state before continuing."
                .into(),
        );
    }
    if let Some(previous) = previous_receipt(s, &session, expected_instance, operation_id, &payload_hash)? {
        return Ok(previous);
    }
    if now() - session.reported_at > FRESH_MS {
        return Err(
            "The UI session changed or stopped reporting. Read get_ui_state before continuing."
                .into(),
        );
    }
    let revision = crate::document(s)?["revision"]
        .as_u64()
        .ok_or("Invalid current revision")?;
    if request["expectedRevision"].as_u64() != Some(revision) {
        return Err("The review changed. Inspect the current UI state before continuing.".into());
    }
    if matches!(command["action"].as_str(),
        Some("setEntrySelection" | "clearEntrySelection" | "openSelectionReview"))
        && command["expectedSelectionFingerprint"] != session.state["selection"]["fingerprint"]
    {
        return Err("The active selection changed. Read the current selection before continuing.".into());
    }
    if matches!(command["action"].as_str(), Some("retryExit" | "cancelExit"))
        && (session.state["close"]["ticket"] != command["expectedExitTicket"]
            || session.state["close"]["phase"] != "waiting")
    {
        return Err("The displayed exit attempt is no longer waiting. Read the current UI state.".into());
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
        s,
        &mut session,
        expected_instance,
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
    let instance_id = text(&report, "expectedInstanceId")?;
    check_instance(&state, instance_id)?;
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
            let operation_id = text(ack, "operationId")?;
            let Some(command) = session.commands.get_mut(operation_id) else {
                if session.archived_receipts > 0 {
                    let connection = open_archive(state, &session, instance_id, false)?;
                    let exists: bool = connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM ui_archive_receipts WHERE operation_id=?1)",
                        [operation_id],
                        |row| row.get(0),
                    ).map_err(archive_error)?;
                    // A renderer can retry a report after its acknowledgement has
                    // already become terminal and moved to the archive.
                    if exists { continue; }
                }
                return Err("Unknown UI acknowledgement".into());
            };
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
    fn lifecycle_controls_require_confirmation_current_tickets_and_replay_before_ticket_checks() {
        let s = crate::tests::fixture();
        *s.gateway.lock().unwrap() = json!({"instanceId":"ui-test"});
        let registered = begin(&s, "ui-test", "exit-renderer", None).unwrap();
        let session_id = registered["uiSessionId"].as_str().unwrap();
        let pending = crate::background::begin_exit(&s).unwrap();
        let ticket = pending["ticket"].as_u64().unwrap();
        let mut reported = pending_state();
        reported["close"] = json!({"phase":"waiting","ticket":ticket});
        report_state(&s, json!({"expectedInstanceId":"ui-test","uiSessionId":session_id,"state":reported})).unwrap();
        let mut operation = json!({"operationId":"confirmed-exit-retry","expectedInstanceId":"ui-test","expectedUiSessionId":session_id,"expectedRevision":crate::document(&s).unwrap()["revision"],"command":{"action":"retryExit","expectedExitTicket":ticket}});
        assert!(request(&s, &operation, MutationOrigin::ManualUi).unwrap_err().contains("confirmation"));
        operation["confirmed"] = json!(true);
        let mut replacement = operation.clone();
        replacement["operationId"] = json!("cannot-replace-exit-ticket");
        replacement["command"] = json!({"action":"closeWindow"});
        assert!(request(&s, &replacement, MutationOrigin::ManualUi).unwrap_err().contains("already owns"));
        operation["command"]["expectedExitTicket"] = json!(ticket + 1);
        assert!(request(&s, &operation, MutationOrigin::ManualUi).unwrap_err().contains("ticket"));
        operation["command"]["expectedExitTicket"] = json!(ticket);
        assert_eq!(request(&s, &operation, MutationOrigin::ManualUi).unwrap()["status"], "accepted");
        crate::background::cancel_exit(&s, ticket).unwrap();
        assert_eq!(request(&s, &operation, MutationOrigin::ManualUi).unwrap()["replayed"], true);
        operation["operationId"] = json!("stale-new-exit-retry");
        assert!(request(&s, &operation, MutationOrigin::ManualUi).is_err());
        assert!(validate_command(&s, &json!({"action":"cancelExit","expectedExitTicket":0})).is_err());
        assert!(validate_command(&s, &json!({"action":"cancelExit","expectedExitTicket":9_007_199_254_740_992_u64})).is_err());
        operation["operationId"] = json!("unconfirmed-exit");
        operation["command"] = json!({"action":"exitApp"});
        operation.as_object_mut().unwrap().remove("confirmed");
        assert!(request(&s, &operation, MutationOrigin::ManualUi).is_err());
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
    fn large_exports_archive_inspections_without_evicting_pending_controls() {
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
            ui.commands.get_mut("first-inspection").unwrap()["finishedAtMs"] = json!(now());
            for sequence in 2..=MAX_COMMANDS {
                ui.commands.insert(format!("protected-{sequence}"),json!({"operationId":format!("protected-{sequence}"),"sequence":sequence,"status":"accepted","request":{"command":{"action":"setFollowAI"}}}));
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
        assert_eq!(ui.archived_inspections, 1);
        assert_eq!(ui.archived_receipts, 1);
    }

    fn ready_archive_session(s: &AppState) -> String {
        *s.gateway.lock().unwrap() = json!({"instanceId":"ui-archive-test"});
        let registered = begin(s, "ui-archive-test", "archive-renderer", None).unwrap();
        let id = registered["uiSessionId"].as_str().unwrap().to_owned();
        report_state(s, json!({"expectedInstanceId":"ui-archive-test","uiSessionId":id,
            "state":pending_state()})).unwrap();
        id
    }

    fn archive_request(s: &AppState, id: &str, operation_id: &str, command: Value) -> Value {
        json!({"operationId":operation_id,"expectedInstanceId":"ui-archive-test","expectedUiSessionId":id,
            "expectedRevision":crate::document(s).unwrap()["revision"],"command":command})
    }

    fn acknowledge(s: &AppState, id: &str, operation_id: &str, ack: Value) {
        {
            let mut ui = s.review.ui.inner.lock().unwrap();
            ui.commands.get_mut(operation_id).unwrap()["status"] = json!("delivered");
        }
        report_state(s, json!({"expectedInstanceId":"ui-archive-test","uiSessionId":id,
            "state":pending_state(),"acknowledgements":[ack]})).unwrap();
    }

    fn inspection_command() -> Value {
        json!({"action":"listPending","offset":0,"limit":1,
            "expectedPendingFingerprint":pending_state()["pendingFingerprint"]})
    }

    #[test]
    fn a_long_mixed_session_archives_without_losing_exact_terminal_status() {
        let s = crate::tests::fixture();
        let id = ready_archive_session(&s);
        let total = MAX_COMMANDS + READ_RESERVE + ARCHIVE_BATCH + 1;
        let mut initial = Vec::new();
        for sequence in 0..total {
            let operation_id = format!("mixed-{sequence}");
            let inspection = sequence % 17 == 0;
            let command = if inspection { inspection_command() }
                else { json!({"action":"setFollowAI","enabled":sequence % 2 == 0}) };
            let operation = archive_request(&s, &id, &operation_id, command);
            assert_eq!(request(&s, &operation, MutationOrigin::ManualUi).unwrap()["status"], "accepted");
            let acknowledgement = if inspection {
                json!({"operationId":operation_id,"status":"applied","result":{"draft":"private-draft-fixture"}})
            } else if sequence == 1 {
                json!({"operationId":operation_id,"status":"error","error":"fixture acknowledgement error"})
            } else {
                json!({"operationId":operation_id,"status":"applied"})
            };
            acknowledge(&s, &id, &operation_id, acknowledgement);
            if sequence < 3 { initial.push(operation); }
        }
        let projected = status(&s).unwrap();
        assert_eq!(projected["operationCount"].as_u64(), Some(total as u64));
        assert_eq!(projected["operations"].as_array().unwrap().len(), 32);
        assert_eq!(projected["operationsTruncated"], true);
        assert!(projected["receiptRetention"]["fullReceipts"].as_u64().unwrap() <= MAX_COMMANDS as u64);
        assert!(projected["receiptRetention"]["archivedReceipts"].as_u64().unwrap() > 0);
        let inspection = request(&s, &initial[0], MutationOrigin::ManualUi).unwrap();
        assert_eq!(inspection["status"], "applied");
        assert_eq!(inspection["resultExpired"], true);
        assert!(inspection["result"].is_null());
        let failed = request(&s, &initial[1], MutationOrigin::ManualUi).unwrap();
        assert_eq!(failed["archived"], true);
        assert_eq!(failed["status"], "error");
        assert_eq!(failed["error"], "fixture acknowledgement error");
        assert!(failed["finishedAtMs"].as_i64().is_some());
        let applied = request(&s, &initial[2], MutationOrigin::ManualUi).unwrap();
        assert_eq!(applied["archived"], true);
        assert_eq!(applied["status"], "applied");
        let mut changed = initial[2].clone();
        changed["command"]["enabled"] = json!(false);
        assert!(request(&s, &changed, MutationOrigin::ManualUi).unwrap_err().contains("different archived request"));
        let archive = {
            let ui = s.review.ui.inner.lock().unwrap();
            assert_eq!(ui.sequence, total as u64);
            assert!(ui.commands.values().all(|record| record["status"] != "accepted"));
            archive_path(&s, &ui)
        };
        let private_marker = b"private-draft-fixture";
        assert!(!fs::read(archive).unwrap().windows(private_marker.len()).any(|bytes| bytes == private_marker));
        report_state(&s, json!({"expectedInstanceId":"ui-archive-test","uiSessionId":id,"state":pending_state(),
            "acknowledgements":[{"operationId":"mixed-1","status":"applied"}]})).unwrap();
        assert_eq!(request(&s, &initial[1], MutationOrigin::ManualUi).unwrap()["status"], "error");
    }

    #[test]
    fn an_archived_row_receipt_survives_stale_reporting_and_a_missing_target() {
        let s = crate::tests::fixture();
        let id = ready_archive_session(&s);
        let row = PathBuf::from(&s.volumes[0].root).join("historical-fixture-row.txt");
        fs::write(&row, b"synthetic fixture").unwrap();
        let operation = archive_request(&s, &id, "historical-row", json!({"action":"setRowMenu","path":row}));
        request(&s, &operation, MutationOrigin::ManualUi).unwrap();
        acknowledge(&s, &id, "historical-row", json!({"operationId":"historical-row","status":"applied"}));
        let archive = {
            let mut ui = s.review.ui.inner.lock().unwrap();
            archive_completed(&s, &mut ui, "ui-archive-test", &["historical-row".into()]).unwrap();
            ui.reported_at = now() - FRESH_MS - 1;
            archive_path(&s, &ui)
        };
        fs::remove_file(&row).unwrap();
        let replay = request(&s, &operation, MutationOrigin::ManualUi).unwrap();
        assert_eq!(replay["status"], "applied");
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["archived"], true);
        let mut changed = operation.clone();
        changed["command"]["path"] = Value::Null;
        assert!(request(&s, &changed, MutationOrigin::ManualUi).unwrap_err().contains("different archived request"));
        changed["operationId"] = json!("new-operation-stale-ui");
        assert!(request(&s, &changed, MutationOrigin::ManualUi).unwrap_err().contains("stopped reporting"));
        let replacement = begin(&s, "ui-archive-test", "replacement", Some(&id)).unwrap();
        assert_ne!(replacement["uiSessionId"], id);
        assert!(archive.is_file());
        assert!(request(&s, &operation, MutationOrigin::ManualUi).is_err());
    }

    fn fill_receipts(ui: &mut Session, start: usize, status: &str) {
        for sequence in start..=MAX_COMMANDS {
            let id = format!("fixture-{sequence}");
            ui.commands.insert(id.clone(), json!({"operationId":id,"sequence":sequence,
                "status":status,"payloadHash":hash(id.as_bytes()),"request":{"command":{"action":"setFollowAI"}},
                "acceptedAtMs":now(),"finishedAtMs":now(),"expiresAtMs":now()+FRESH_MS}));
        }
        ui.sequence = MAX_COMMANDS as u64;
    }

    #[test]
    fn archive_write_failure_rolls_back_the_batch_and_retains_original_receipts() {
        let s = crate::tests::fixture();
        let id = ready_archive_session(&s);
        let first = archive_request(&s, &id, "original", json!({"action":"setFollowAI","enabled":true}));
        request(&s, &first, MutationOrigin::ManualUi).unwrap();
        acknowledge(&s, &id, "original", json!({"operationId":"original","status":"applied"}));
        {
            let mut ui = s.review.ui.inner.lock().unwrap();
            fill_receipts(&mut ui, 2, "applied");
            open_archive(&s, &ui, "ui-archive-test", true).unwrap().execute_batch(
                "CREATE TRIGGER synthetic_archive_failure BEFORE INSERT ON ui_archive_receipts
                 WHEN NEW.operation_id='fixture-2' BEGIN SELECT RAISE(ABORT,'synthetic archive write failure'); END;"
            ).unwrap();
        }
        let next = archive_request(&s, &id, "after-failure", json!({"action":"closePanels"}));
        assert!(request(&s, &next, MutationOrigin::ManualUi).unwrap_err().contains("archive unavailable"));
        assert_eq!(request(&s, &first, MutationOrigin::ManualUi).unwrap()["status"], "applied");
        {
            let ui = s.review.ui.inner.lock().unwrap();
            assert_eq!(ui.commands.len(), MAX_COMMANDS);
            assert_eq!(ui.archived_receipts, 0);
            assert!(!ui.commands.contains_key("after-failure"));
            let archive = open_archive(&s, &ui, "ui-archive-test", true).unwrap();
            let rows: i64 = archive.query_row("SELECT count(*) FROM ui_archive_receipts", [], |row| row.get(0)).unwrap();
            assert_eq!(rows, 0);
            archive.execute_batch("DROP TRIGGER synthetic_archive_failure;").unwrap();
        }
        assert_eq!(request(&s, &next, MutationOrigin::ManualUi).unwrap()["status"], "accepted");
        assert_eq!(request(&s, &first, MutationOrigin::ManualUi).unwrap()["archived"], true);
    }

    #[test]
    fn losing_an_existing_archive_never_readmits_an_operation() {
        let s = crate::tests::fixture();
        let id = ready_archive_session(&s);
        let operation = archive_request(&s, &id, "archived-once", json!({"action":"closePanels"}));
        request(&s, &operation, MutationOrigin::ManualUi).unwrap();
        acknowledge(&s, &id, "archived-once", json!({"operationId":"archived-once","status":"applied"}));
        {
            let mut ui = s.review.ui.inner.lock().unwrap();
            archive_completed(&s, &mut ui, "ui-archive-test", &["archived-once".into()]).unwrap();
            fs::remove_file(archive_path(&s, &ui)).unwrap();
        }
        assert!(request(&s, &operation, MutationOrigin::ManualUi).unwrap_err().contains("archive unavailable"));
        let ui = s.review.ui.inner.lock().unwrap();
        assert_eq!(ui.sequence, 1);
        assert_eq!(ui.archived_receipts, 1);
        assert!(ui.commands.is_empty());
    }

    #[test]
    fn thirty_two_inspections_are_reserved_when_all_full_receipts_are_pending() {
        let s = crate::tests::fixture();
        let id = ready_archive_session(&s);
        fill_receipts(&mut s.review.ui.inner.lock().unwrap(), 1, "accepted");
        let mutation = archive_request(&s, &id, "blocked-control", json!({"action":"closePanels"}));
        assert!(request(&s, &mutation, MutationOrigin::ManualUi).unwrap_err().contains("pending work"));
        for index in 0..READ_RESERVE {
            let inspection = archive_request(&s, &id, &format!("reserve-{index}"), inspection_command());
            assert_eq!(request(&s, &inspection, MutationOrigin::ManualUi).unwrap()["status"], "accepted");
        }
        let excessive = archive_request(&s, &id, "reserve-extra", inspection_command());
        assert!(request(&s, &excessive, MutationOrigin::ManualUi).unwrap_err().contains("pending work"));
        assert_eq!(status(&s).unwrap()["operationCount"].as_u64(), Some((MAX_COMMANDS + READ_RESERVE) as u64));
    }
}
