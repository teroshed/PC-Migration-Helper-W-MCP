//! Private loopback RPC transport for the stdio MCP bridge. No source filesystem writes.
//! One HTTP request per connection; strict length framing deliberately excludes chunked requests.
#[cfg(test)]
use crate::{boot, document};
use crate::{
    details_service, filesystem, list_folder_service, next_attention_service, run_mutation_as,
    scan_service, status_service,
    store::{atomic_write, error, now},
    AppState, MutationOrigin, Shared,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TrySendError},
        Arc, Mutex, Weak,
    },
    thread,
    time::{Duration, Instant},
};

const MAX_BODY: usize = 256 * 1024;
const MAX_RESPONSE: usize = 4 * 1024 * 1024;
const MAX_HEADERS: usize = 16 * 1024;
const READ_DEADLINE: Duration = Duration::from_secs(10);
const WRITE_DEADLINE: Duration = Duration::from_secs(10);
const WORKERS: usize = 4;
const QUEUED: usize = 4;

pub struct GatewayHandle {
    stop: Arc<AtomicBool>,
}
impl Drop for GatewayHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
struct Accepted {
    stream: TcpStream,
    received: Instant,
}
struct Auth {
    token: String,
    instance_id: String,
    authority: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RpcRequest {
    method: String,
    #[serde(default)]
    params: Value,
}
struct Failure {
    status: u16,
    message: String,
}
impl Failure {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

fn random_hex<const N: usize>() -> Result<String, String> {
    let mut random = [0u8; N];
    getrandom::fill(&mut random).map_err(error)?;
    Ok(random.iter().map(|b| format!("{b:02x}")).collect())
}

/// Only the Store owner publishes a descriptor. A damaged journal permits the
/// restricted inspection/recovery routes; competing instances remain blocked.
pub fn start(state: &Shared, runtime: &Path) -> Result<(GatewayHandle, Value), String> {
    {
        let locked = state.store.lock().map_err(error)?;
        let _store = locked
            .as_ref()
            .ok_or("This instance does not own the data file")?;
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(error)?;
    listener.set_nonblocking(true).map_err(error)?;
    let address = listener.local_addr().map_err(error)?;
    let token = random_hex::<32>()?;
    let instance_id = random_hex::<16>()?;
    let endpoint = format!("http://{address}/rpc");
    let started = now();
    let auth = Arc::new(Auth {
        token: token.clone(),
        instance_id: instance_id.clone(),
        authority: address.to_string(),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let handle = GatewayHandle { stop: stop.clone() };
    let (sender, receiver) = mpsc::sync_channel::<Accepted>(QUEUED);
    let receiver = Arc::new(Mutex::new(receiver));
    for number in 0..WORKERS {
        let input = receiver.clone();
        let stop = stop.clone();
        let auth = auth.clone();
        let state = Arc::downgrade(state);
        thread::Builder::new()
            .name(format!("migration-rpc-{number}"))
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let accepted = {
                        let Ok(input) = input.lock() else {
                            break;
                        };
                        input.recv_timeout(Duration::from_millis(200))
                    };
                    match accepted {
                        Ok(accepted) => {
                            let Some(state) = state.upgrade() else {
                                break;
                            };
                            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                handle_connection(accepted, &auth, &state)
                            }));
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .map_err(error)?;
    }
    let accept_stop = stop.clone();
    let weak = Arc::downgrade(state);
    thread::Builder::new()
        .name("migration-rpc-listener".into())
        .spawn(move || {
            while !accept_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, address)) => {
                        if !address.ip().is_loopback() {
                            continue;
                        }
                        // Windows accepted sockets can inherit the listener's nonblocking mode.
                        // Workers use blocking I/O with absolute deadlines, including requests
                        // whose headers and JSON body arrive in separate network packets.
                        if stream.set_nonblocking(false).is_err() {
                            continue;
                        }
                        // No unbounded task/thread creation. Overload drops the new connection.
                        match sender.try_send(Accepted {
                            stream,
                            received: Instant::now(),
                        }) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20))
                    }
                    Err(_) => {
                        set_listener_problem(&weak);
                        break;
                    }
                }
            }
        })
        .map_err(error)?;
    fs::create_dir_all(runtime).map_err(error)?;
    let descriptor = json!({"schemaVersion":1,"endpoint":endpoint,"token":token,"instanceId":instance_id,
        "pid":std::process::id(),"fixture":state.fixture,"dataPath":state.data_path,"startedAt":started});
    atomic_write(
        &runtime.join("mcp-gateway.json"),
        &serde_json::to_vec_pretty(&descriptor).map_err(error)?,
    )?;
    let public =
        json!({"state":"running","endpoint":endpoint,"instanceId":instance_id,"startedAt":started});
    Ok((handle, public))
}

fn set_listener_problem(state: &Weak<AppState>) {
    if let Some(state) = state.upgrade() {
        if let Ok(mut status) = state.gateway.lock() {
            *status = json!({"state":"error","problem":"The local connection listener stopped. Review remains available in this app."});
        }
    }
}

fn handle_connection(mut accepted: Accepted, auth: &Auth, state: &Shared) {
    let result = read_request(&mut accepted.stream, accepted.received, auth);
    let (status, payload) = match result {
        Ok(request) => match dispatch(state, &request.method, &request.params) {
            Ok(result) => (200, json!({"ok":true,"result":result})),
            Err(message) => (200, json!({"ok":false,"error":message})),
        },
        Err(failure) => (failure.status, json!({"ok":false,"error":failure.message})),
    };
    let _ = write_response(&mut accepted.stream, status, &payload);
    let _ = accepted.stream.shutdown(Shutdown::Both);
}

fn remaining(start: Instant, limit: Duration) -> Result<Duration, Failure> {
    limit
        .checked_sub(start.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| Failure::new(408, "Local request timed out"))
}
fn read_request(
    stream: &mut TcpStream,
    received: Instant,
    auth: &Auth,
) -> Result<RpcRequest, Failure> {
    let mut bytes = Vec::with_capacity(8192);
    let mut block = [0u8; 8192];
    let header_end = loop {
        if let Some(position) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
            let end = position + 4;
            if end > MAX_HEADERS {
                return Err(Failure::new(413, "Request headers are too large"));
            }
            break end;
        }
        if bytes.len() >= MAX_HEADERS {
            return Err(Failure::new(413, "Request headers are too large"));
        }
        stream
            .set_read_timeout(Some(remaining(received, READ_DEADLINE)?))
            .map_err(|_| Failure::new(400, "Cannot read local request"))?;
        let read = stream
            .read(&mut block)
            .map_err(|_| Failure::new(408, "Local request timed out or disconnected"))?;
        if read == 0 {
            return Err(Failure::new(400, "Incomplete HTTP request"));
        }
        bytes.extend_from_slice(&block[..read]);
    };
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    parsed
        .parse(&bytes[..header_end])
        .map_err(|_| Failure::new(400, "Malformed HTTP request"))?;
    if parsed.method != Some("POST") || parsed.path != Some("/rpc") || parsed.version != Some(1) {
        return Err(Failure::new(404, "Only POST /rpc HTTP/1.1 is available"));
    }
    if parsed
        .headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case("Origin"))
    {
        return Err(Failure::new(403, "Browser-origin requests are not allowed"));
    }
    let unique = |name: &str| -> Result<Option<&[u8]>, Failure> {
        let mut matching = parsed
            .headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case(name));
        let value = matching.next().map(|h| h.value);
        if matching.next().is_some() {
            return Err(Failure::new(400, "Duplicate protected HTTP header"));
        }
        Ok(value)
    };
    if unique("Host")? != Some(auth.authority.as_bytes()) {
        return Err(Failure::new(403, "Unexpected local host"));
    }
    let authorization = unique("Authorization")?.unwrap_or_default();
    let expected = format!("Bearer {}", auth.token);
    if !constant_eq(authorization, expected.as_bytes()) {
        return Err(Failure::new(401, "Local connection authentication failed"));
    }
    if unique("X-Migration-Instance")? != Some(auth.instance_id.as_bytes()) {
        return Err(Failure::new(
            409,
            "The native app instance changed; reconnect before continuing",
        ));
    }
    if unique("Transfer-Encoding")?.is_some() {
        return Err(Failure::new(400, "Chunked requests are not supported"));
    }
    let content_type = unique("Content-Type")?
        .and_then(|s| std::str::from_utf8(s).ok())
        .unwrap_or("");
    if !content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
    {
        return Err(Failure::new(
            415,
            "Content-Type application/json is required",
        ));
    }
    let length =
        unique("Content-Length")?.ok_or_else(|| Failure::new(411, "Content-Length is required"))?;
    if length.is_empty() || !length.iter().all(u8::is_ascii_digit) {
        return Err(Failure::new(400, "Invalid Content-Length"));
    }
    let length = std::str::from_utf8(length)
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n <= MAX_BODY)
        .ok_or_else(|| Failure::new(413, "Local RPC body exceeds 256 KiB"))?;
    let mut body = bytes[header_end..].to_vec();
    if body.len() > length {
        return Err(Failure::new(400, "Unexpected bytes after the request body"));
    }
    while body.len() < length {
        stream
            .set_read_timeout(Some(remaining(received, READ_DEADLINE)?))
            .map_err(|_| Failure::new(400, "Cannot read local request"))?;
        let available = (length - body.len()).min(block.len());
        let read = stream
            .read(&mut block[..available])
            .map_err(|_| Failure::new(408, "Local request timed out or disconnected"))?;
        if read == 0 {
            return Err(Failure::new(400, "Incomplete RPC body"));
        }
        body.extend_from_slice(&block[..read]);
    }
    serde_json::from_slice(&body).map_err(|_| Failure::new(400, "Invalid JSON RPC request"))
}
fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b) {
        difference |= x ^ y;
    }
    difference == 0
}
fn write_response(stream: &mut TcpStream, status: u16, payload: &Value) -> Result<(), String> {
    let body = response_bytes(payload);
    let header=format!("HTTP/1.1 {status} Local RPC\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",body.len());
    let started = Instant::now();
    for part in [header.as_bytes(), body.as_slice()] {
        let mut offset = 0;
        while offset < part.len() {
            let left = WRITE_DEADLINE
                .checked_sub(started.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or("Local response timed out")?;
            stream.set_write_timeout(Some(left)).map_err(error)?;
            let end = (offset + 16 * 1024).min(part.len());
            let count = stream.write(&part[offset..end]).map_err(error)?;
            if count == 0 {
                return Err("Local connection disconnected".into());
            }
            offset += count;
        }
    }
    Ok(())
}

struct ResponseBuffer {
    bytes: Vec<u8>,
}
impl Write for ResponseBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_RESPONSE.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "RPC response limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn response_bytes(payload: &Value) -> Vec<u8> {
    let mut output = ResponseBuffer {
        bytes: Vec::with_capacity(8192),
    };
    if serde_json::to_writer(&mut output, payload).is_ok() {
        return output.bytes;
    }
    // The bounded writer stops allocation before crossing the cap, even for unknown future fields.
    br#"{"ok":false,"error":"Local RPC response exceeds 4 MiB. Request a narrower path, filter or page; saved originals are unchanged."}"#.to_vec()
}

fn limited_text(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}
fn project_metadata(
    value: &Value,
    depth: usize,
    budget: &mut usize,
    truncated: &mut bool,
) -> Value {
    if *budget == 0 {
        *truncated = true;
        return Value::Null;
    }
    *budget -= 1;
    match value {
        Value::String(text) => {
            let (value, cut) = limited_text(text, 2048);
            *truncated |= cut;
            json!(value)
        }
        Value::Array(items) if depth > 0 => {
            *truncated |= items.len() > 12;
            Value::Array(
                items
                    .iter()
                    .take(12)
                    .map(|item| project_metadata(item, depth - 1, budget, truncated))
                    .collect(),
            )
        }
        Value::Object(fields) if depth > 0 => {
            *truncated |= fields.len() > 16;
            Value::Object(
                fields
                    .iter()
                    .take(16)
                    .map(|(key, value)| {
                        let (key, cut) = limited_text(key, 256);
                        *truncated |= cut;
                        (key, project_metadata(value, depth - 1, budget, truncated))
                    })
                    .collect(),
            )
        }
        Value::Array(_) | Value::Object(_) => {
            *truncated = true;
            Value::Null
        }
        _ => value.clone(),
    }
}

/// Transport projections omit unknown fields and bound text. The authoritative JSON is untouched.
fn project_record(record: &Value, notes_limit: usize) -> Value {
    if record.is_null() {
        return Value::Null;
    }
    let Some(fields) = record.as_object() else {
        return json!({"truncation":{"unsupportedRecord":true,"storedOriginalPreserved":true}});
    };
    let selected = [
        "id",
        "key",
        "path",
        "volumeId",
        "relativePath",
        "status",
        "noteOnly",
        "notes",
        "createdAt",
        "updatedAt",
        "decidedAt",
        "provenance",
        "notesProvenance",
        "lastEditedBy",
        "verification",
        "snapshot",
        "reason",
        "basis",
        "state",
    ];
    let mut projected = serde_json::Map::new();
    let mut truncated_fields = serde_json::Map::new();
    let mut metadata_truncated = false;
    let mut budget = 64usize;
    for name in selected {
        let Some(value) = fields.get(name) else {
            continue;
        };
        if matches!(name, "notes" | "reason" | "path" | "relativePath" | "key") {
            if let Some(text) = value.as_str() {
                let limit = match name {
                    "notes" => notes_limit,
                    "reason" => 8192,
                    _ => 8192,
                };
                let (shortened, cut) = limited_text(text, limit);
                if cut {
                    truncated_fields.insert(
                        name.into(),
                        json!({"originalBytes":text.len(),"returnedBytes":shortened.len()}),
                    );
                }
                projected.insert(name.into(), json!(shortened));
                continue;
            }
        }
        projected.insert(
            name.into(),
            project_metadata(value, 3, &mut budget, &mut metadata_truncated),
        );
    }
    let omitted = fields
        .keys()
        .filter(|name| !selected.contains(&name.as_str()))
        .count();
    projected.insert("truncation".into(),json!({"fields":truncated_fields,"metadataTruncated":metadata_truncated,"omittedFieldCount":omitted,"storedOriginalPreserved":true}));
    Value::Object(projected)
}
fn project_history(event: &Value) -> Value {
    let mut result = serde_json::Map::new();
    let mut cut = false;
    let mut budget = 40;
    for key in [
        "id",
        "type",
        "key",
        "volumeId",
        "relativePath",
        "path",
        "status",
        "at",
        "undoes",
        "undoneAt",
        "provenance",
        "undoneBy",
    ] {
        if let Some(value) = event.get(key) {
            result.insert(
                key.into(),
                project_metadata(value, 3, &mut budget, &mut cut),
            );
        }
    }
    for key in ["before", "after"] {
        if let Some(value) = event.get(key) {
            result.insert(key.into(), project_record(value, 4096));
        }
    }
    result.insert(
        "truncation".into(),
        json!({"metadataTruncated":cut,"projection":true,"storedOriginalPreserved":true}),
    );
    Value::Object(result)
}
fn projected_details(state: &AppState, path: String) -> Result<Value, String> {
    let details = details_service(state, path)?;
    let decision_key=filesystem::identify(details["path"].as_str().ok_or("Missing details path")?,&state.volumes)?.2;
    let decision_record_id=format!("sha256:{}",crate::store::hash(decision_key.as_bytes()));
    let overrides = details["overrides"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .take(200)
                .map(|item| project_record(item, 4096))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let history = details["history"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .take(30)
                .map(project_history)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(
        json!({"path":details["path"],"decisionRecordId":decision_record_id,"effective":details["effective"],"decision":project_record(&details["decision"],16*1024),
        "overrides":overrides,"history":history,"exists":details["exists"],"inventory":details["inventory"],"audit":details["audit"],
        "truncation":{"projection":true,"notesLimitBytes":16*1024,"overrideNotesLimitBytes":4096,"historyNotesLimitBytes":4096,"storedOriginalPreserved":true}}),
    )
}

fn text(params: &Value, key: &str, default: Option<&str>) -> Result<String, String> {
    match params.get(key) {
        Some(value) => value
            .as_str()
            .map(String::from)
            .ok_or_else(|| format!("{key} must be text")),
        None => default
            .map(String::from)
            .ok_or_else(|| format!("Missing {key}")),
    }
}
fn page(params: &Value) -> Result<usize, String> {
    let page = params
        .get("page")
        .map(|v| v.as_u64().ok_or("Invalid page"))
        .transpose()?
        .unwrap_or(0);
    if page > 10_000_000 {
        return Err("Page is outside the supported range".into());
    }
    Ok(page as usize)
}
fn healthy(state: &AppState) -> Result<(), String> {
    let lock = state.store.lock().map_err(error)?;
    let store = lock
        .as_ref()
        .ok_or("This instance does not own the data file")?;
    if let Some(problem) = &store.problem {
        return Err(problem.clone());
    }
    Ok(())
}

fn state_summary(state: &AppState) -> Result<Value, String> {
    let locked = state.store.lock().map_err(error)?;
    let doc = &locked.as_ref().ok_or("Data unavailable")?.doc;
    let gateway = state.gateway.lock().map_err(error)?.clone();
    let mut view_truncated = false;
    let mut view_budget = 64;
    let view = project_metadata(&doc["view"], 3, &mut view_budget, &mut view_truncated);
    Ok(
        json!({"api":api_capabilities(),"revision":doc["revision"],"schemaVersion":doc["schemaVersion"],"view":view,
        "volumes":state.volumes,"dataPath":state.data_path,"fixture":state.fixture,"problem":null,
        "gateway":gateway,"decisionCount":doc["decisions"].as_object().map_or(0,|m|m.len()),
        "suggestionCount":doc["suggestions"].as_array().map_or(0,|a|a.len()),"historyCount":doc["history"].as_array().map_or(0,|a|a.len()),"backupReminder":doc["backupReminder"],"truncation":{"viewMetadataTruncated":view_truncated,"storedOriginalPreserved":true}}),
    )
}

pub(crate) fn api_capabilities()->Value {
    json!({"version":"0.4.0","transport":"authenticated-loopback-rpc","methods":[
        "get_state","get_review_view","list_folder","get_details","list_decisions","list_suggestions",
        "read_review_text","get_status","get_assisted_review","next_attention","search_dirs","scan","mutate",
        "audit_status","audit_control","audit_notifications","audit_read_notifications",
        "copy_queue_snapshot","control_copy_queue","get_ui_state","control_ui","get_recovery_state","recover_review"],
        "recoveryModeMethods":["get_recovery_state","recover_review","get_ui_state","control_ui"],
        "copyControls":["start","pause","resume"],"sourceDeletion":false,
        "uiControl":"registered native UI session; accepted commands require an applied/error acknowledgement",
        "textContinuation":"read_review_text for saved text; control_ui listPending/readPendingText for retained drafts",
        "authority":"Authenticated delegated clients must assert explicit user_instruction. This is actor provenance, not cryptographic human-presence proof."})
}

fn review_view_summary(state: &AppState) -> Result<Value, String> {
    let mut result = state_summary(state)?;
    let surface = state.review.surface()?;
    let age_ms = surface["reportedAtMs"].as_i64().map(|at| {
        chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(at)
            .max(0)
    });
    let status = match age_ms {
        None => "waiting",
        Some(age) if age > 5_000 => "stale",
        Some(_)
            if surface["loading"] == true || surface["appliedRevision"] != result["revision"] =>
        {
            "updating"
        }
        Some(_) => "current",
    };
    result["surfaceState"] = json!(status);
    result["surfaceAgeMs"] = json!(age_ms);
    result["surface"] = surface;
    Ok(result)
}
fn list_records(state: &AppState, params: &Value, suggestions: bool) -> Result<Value, String> {
    let search = text(params, "search", Some(""))?.to_lowercase();
    let status = text(params, "status", Some("all"))?;
    if status != "all" && !crate::store::valid_status(&status) {
        return Err("Unknown classification filter".into());
    }
    let page = page(params)?;
    let limit=params.get("limit").map(|v|v.as_u64().ok_or("Invalid record limit")).transpose()?.unwrap_or(100);
    if limit==0 || limit>100 {return Err("Record limit must be 1 to 100".into());}
    let limit=limit as usize;
    let lock = state.store.lock().map_err(error)?;
    let doc = &lock.as_ref().ok_or("Data unavailable")?.doc;
    let matches = |item: &Value| {
        format!("{}\n{}\n{}",item["path"].as_str().unwrap_or(""),item["notes"].as_str().unwrap_or(""),
            if suggestions {item["reason"].as_str().unwrap_or("")}else{""}).to_lowercase().contains(&search)
            && (status == "all" || item["status"] == status)
    };
    let mut total = 0usize;
    let mut entries = Vec::new();
    let mut visit = |item: &Value, record_id: &str| {
        if matches(item) {
            if total >= page * limit && entries.len() < limit {
                let mut entry=project_record(item,16*1024);
                entry["recordId"]=json!(if suggestions {record_id.to_string()} else {format!("sha256:{}",crate::store::hash(record_id.as_bytes()))});
                entries.push(entry);
            }
            total += 1;
        }
    };
    if suggestions {
        if let Some(items) = doc["suggestions"].as_array() {
            for item in items {
                visit(item,item["id"].as_str().or_else(||item["path"].as_str()).unwrap_or(""));
            }
        }
    } else if let Some(items) = doc["decisions"].as_object() {
        let mut ordered:Vec<_>=items.iter().collect();
        ordered.sort_by(|(left_key,left),(right_key,right)|left["path"].as_str().unwrap_or("").to_lowercase()
            .cmp(&right["path"].as_str().unwrap_or("").to_lowercase()).then(left_key.cmp(right_key)));
        for (key,item) in ordered {
            visit(item,key);
        }
    }
    Ok(json!({"revision":doc["revision"],"total":total,"page":page,"limit":limit,"entries":entries}))
}

fn read_review_text(state:&AppState,params:&Value)->Result<Value,String> {
    let collection=text(params,"collection",None)?;
    let record_id=text(params,"recordId",None)?;
    let field=text(params,"field",None)?;
    let offset=params.get("offset").map(|v|v.as_u64().ok_or("offset must be a nonnegative UTF-8 byte offset")).transpose()?.unwrap_or(0);
    let limit=params.get("limit").map(|v|v.as_u64().ok_or("limit must be a positive byte count")).transpose()?.unwrap_or(16*1024);
    if limit<4 || limit>16*1024 {return Err("Text chunks must contain 4 to 16384 bytes".into());}
    let lock=state.store.lock().map_err(error)?;
    let doc=&lock.as_ref().ok_or("Data unavailable")?.doc;
    let revision=doc["revision"].as_u64().ok_or("Invalid journal revision")?;
    if params.get("expectedRevision").is_some_and(|v|v.as_u64()!=Some(revision)) {return Err("The journal changed. Restart this text read from offset zero.".into());}
    if offset>0 && (params.get("expectedRevision").is_none() || params.get("expectedTextSha256").is_none()) {
        return Err("Continuation requires the revision and text SHA-256 from the first chunk".into());
    }
    let item=match collection.as_str() {
        "decisions"=>if let Some(digest)=record_id.strip_prefix("sha256:") {
            doc["decisions"].as_object().and_then(|items|items.iter().find(|(key,_)|crate::store::hash(key.as_bytes())==digest).map(|(_,item)|item))
        }else{doc["decisions"].get(&record_id)},
        "suggestions"=>doc["suggestions"].as_array().and_then(|items|items.iter().rev().find(|item|item["id"].as_str().or_else(||item["path"].as_str())==Some(record_id.as_str()))),
        "history"=>doc["history"].as_array().and_then(|items|items.iter().rev().find(|item|item["id"]==record_id)),
        _=>return Err("Unknown review-text collection".into()),
    }.ok_or("Review text record was not found")?;
    let value=match field.as_str() {
        "notes"=>&item["notes"],"reason"=>&item["reason"],"path"=>&item["path"],
        "before.notes" if collection=="history"=>&item["before"]["notes"],
        "after.notes" if collection=="history"=>&item["after"]["notes"],
        "provenance.reason"=>&item["provenance"]["reason"],
        _=>return Err("Unsupported review text field".into()),
    };
    let full=if value.is_null(){""}else{value.as_str().ok_or("The selected review field is not text")?};
    let digest=crate::store::hash(full.as_bytes());
    if params.get("expectedTextSha256").is_some_and(|v|v.as_str()!=Some(digest.as_str())) {return Err("The text changed. Restart this read from offset zero.".into());}
    let offset=usize::try_from(offset).map_err(|_|"Text offset is too large")?;
    if offset>full.len() || !full.is_char_boundary(offset) {return Err("Offset is outside this text or splits a UTF-8 character".into());}
    let (chunk,cut)=limited_text(&full[offset..],limit as usize);
    let next=offset+chunk.len();
    Ok(json!({"revision":revision,"collection":collection,"recordId":record_id,"field":field,"text":chunk,
        "offset":offset,"nextOffset":if cut{Some(next)}else{None},"totalBytes":full.len(),"textSha256":digest,"complete":!cut}))
}

fn agent_control_params(params: &Value) -> Result<(Value, MutationOrigin), String> {
    let origin=MutationOrigin::Agent{basis:text(params,"basis",None)?,reason:text(params,"reason",None)?};
    let mut request=params.clone();
    if let Some(fields)=request.as_object_mut(){ fields.remove("basis"); fields.remove("reason"); }
    Ok((request,origin))
}

fn copy_snapshot_for_rpc(state:&AppState)->Result<Value,String> {
    let mut snapshot=crate::copy_control::snapshot(state)?;
    for field in ["migrateNow","lastMoment"] {
        if let Some(items)=snapshot[field].as_array_mut() {
            for item in items {
                if let Some(path)=item["path"].as_str().map(String::from) {
                    let (preview,truncated)=limited_text(&path,4096);
                    item["path"]=json!(preview);item["pathTruncated"]=json!(truncated);item["pathBytes"]=json!(path.len());
                    if let Ok((_,_,key))=filesystem::identify(&path,&state.volumes) {item["recordId"]=json!(format!("sha256:{}",crate::store::hash(key.as_bytes())));}
                }
            }
        }
    }
    snapshot["projection"]=json!({"queueEntryPathLimitBytes":4096,"completePathTool":"read_review_text","collection":"decisions","field":"path","storedOriginalPreserved":true});
    Ok(snapshot)
}

fn dispatch(state: &Shared, method: &str, params: &Value) -> Result<Value, String> {
    if !matches!(method,"get_recovery_state"|"recover_review"|"get_ui_state"|"control_ui") { healthy(state)?; }
    if !params.is_null() && !params.is_object() {
        return Err("RPC params must be an object".into());
    }
    if matches!(
        method,
        "mutate" | "scan" | "audit_control" | "audit_read_notifications" | "control_copy_queue" | "control_ui" | "recover_review"
    ) {
        let expected=params["expectedInstanceId"].as_str().ok_or("Writes require expectedInstanceId from the current review state; reconnect before continuing")?;
        let current = state.gateway.lock().map_err(error)?;
        if current["instanceId"].as_str() != Some(expected) {
            return Err("The native app instance changed. Prepared writes were refused; read the current review state and reconcile before continuing.".into());
        }
    }
    match method {
        "get_recovery_state"=>crate::recovery_control::snapshot(state),
        "recover_review"=>{let(request,origin)=agent_control_params(params)?;crate::recovery_control::control(state,&request,origin)},
        "copy_queue_snapshot"=>copy_snapshot_for_rpc(state),
        "control_copy_queue"=>{let(request,origin)=agent_control_params(params)?;crate::copy_control::control(state,&request,origin)},
        "get_ui_state"=>crate::ui_control::status(state),
        "control_ui"=>{let(request,origin)=agent_control_params(params)?;crate::ui_control::request(state,&request,origin)},
        "get_state"=>state_summary(state),
        "get_review_view"=>review_view_summary(state),
        "list_decisions"=>list_records(state,params,false),
        "list_suggestions"=>list_records(state,params,true),
        "list_folder"=>{
            let refresh=params.get("refresh").map(|v|v.as_bool().ok_or("refresh must be true or false")).transpose()?.unwrap_or(false);
            let listing = list_folder_service(state,text(params,"path",None)?,refresh,text(params,"search",Some(""))?,text(params,"filter",Some("all"))?,page(params)?)?;
            if let Some(target) = listing["path"].as_str() {
                state.review.activity("read", target, None);
            }
            Ok(listing)
        },
        "get_details"=>{
            let details = projected_details(state,text(params,"path",None)?)?;
            if details["exists"] == true {
                if let Some(target) = details["path"].as_str() {
                    state.review.activity("read", target, None);
                }
            }
            Ok(details)
        },
        "get_status"=>status_service(state),
        "get_assisted_review"=>crate::assistance::snapshot(state,&crate::document(state)?),
        "read_review_text"=>read_review_text(state,params),
        "audit_status"=>crate::audit_status_service(state),
        "audit_control"=>crate::audit_control_service(state,&text(params,"action",None)?,&text(params,"scope",Some("all"))?,params.get("path").and_then(Value::as_str)),
        "audit_notifications"=>{
            let limit=params.get("limit").map(|v|v.as_u64().ok_or("Invalid notification limit")).transpose()?.unwrap_or(20);
            let offset=params.get("offset").map(|v|v.as_u64().ok_or("Invalid notification offset")).transpose()?.unwrap_or(0);
            if limit>100 || offset>10_000 {return Err("Notification page is outside the supported range".into());}
            crate::audit_notifications_service(state,limit as usize,offset as usize)
        },
        "audit_read_notifications"=>{
            let all=params.get("all").map(|v|v.as_bool().ok_or("all must be true or false")).transpose()?.unwrap_or(false);
            let ids=params.get("ids").map(|v|v.as_array().ok_or("ids must be an array")).transpose()?;
            let ids:Vec<String>=ids.into_iter().flatten().map(|id|id.as_str().map(String::from).ok_or("Notification IDs must be strings")).collect::<Result<_,_>>()?;
            crate::audit_read_service(state,&ids,all)
        },
        "search_dirs"=>serde_json::to_value(state.index.known_dirs(&text(params,"search",Some(""))?)?).map_err(error),
        "next_attention"=>{
            let after=match params.get("after"){None|Some(Value::Null)=>None,Some(v)=>Some(v.as_str().ok_or("Invalid after path")?.to_string())};
            if let Some(path)=&after{filesystem::identify(path,&state.volumes)?;}
            Ok(next_attention_service(state,text(params,"path",None)?,after)?.unwrap_or(Value::Null))
        },
        "scan"=>scan_service(state,text(params,"mode",None)?,text(params,"path",Some(&state.volumes[0].root))?),
        "mutate"=>{
            let operation=params.get("operation").filter(|v|v.is_object()).ok_or("Missing operation")?.clone();
            let operation_id=text(params,"operationId",None)?;
            let revision=params["expectedRevision"].as_u64().ok_or("Missing expectedRevision")?;
            let basis=text(params,"basis",Some("navigation"))?;let reason=text(params,"reason",Some(""))?;
            let doc=run_mutation_as(state,revision,operation_id.clone(),operation.clone(),MutationOrigin::Agent{basis,reason})?;
            let change=doc["history"].as_array().and_then(|history|history.iter().rev().find(|entry|entry["id"]==operation_id));
            let current_decision=change.and_then(|event|event["key"].as_str()).map(|key|project_record(&doc["decisions"][key],16*1024));
            let receipt=doc.get("agentOperations").and_then(|receipts|receipts.get(&operation_id));
            Ok(json!({"revision":doc["revision"],"updatedAt":doc["updatedAt"],"operationId":operation_id,
                "decision":current_decision,"operationReceipt":receipt.map(|receipt|{let mut cut=false;let mut budget=32;project_metadata(receipt,3,&mut budget,&mut cut)}),"decisionCount":doc["decisions"].as_object().map_or(0,|m|m.len()),
                "suggestionCount":doc["suggestions"].as_array().map_or(0,|a|a.len()),"truncation":{"projection":true,"notesLimitBytes":16*1024,"storedOriginalPreserved":true}}))
        },
        _=>Err("Unknown local RPC method. Recovery and filesystem execution are not available through MCP.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(state: &Shared, method: &str, mut params: Value) -> Value {
        if matches!(method, "mutate" | "scan") && params.get("expectedInstanceId").is_none() {
            params["expectedInstanceId"] = state.gateway.lock().unwrap()["instanceId"].clone();
        }
        dispatch(state, method, &params).unwrap()
    }
    #[test]
    fn saved_note_search_and_utf8_continuation_preserve_complete_text_and_revision_fences() {
        let state=Arc::new(crate::tests::fixture());
        let path=format!("{}\\Work",state.volumes[0].root);
        let notes=format!("Search Needle\n{}","猫".repeat(10_000));
        crate::run_mutation(&state,document(&state).unwrap()["revision"].as_u64().unwrap(),"text-chunk-note".into(),json!({"type":"note","path":path,"notes":notes})).unwrap();
        let found=call(&state,"list_decisions",json!({"search":"needle"}));
        assert_eq!(found["total"],1);
        let id=found["entries"][0]["recordId"].clone();
        let mut params=json!({"collection":"decisions","recordId":id,"field":"notes","limit":16384});
        let first=call(&state,"read_review_text",params.clone());
        assert_eq!(first["complete"],false);
        let mut reconstructed=first["text"].as_str().unwrap().to_string();
        let mut next=first["nextOffset"].clone();
        params["expectedRevision"]=first["revision"].clone();
        params["expectedTextSha256"]=first["textSha256"].clone();
        while !next.is_null() {
            params["offset"]=next;
            let part=call(&state,"read_review_text",params.clone());
            reconstructed.push_str(part["text"].as_str().unwrap());
            next=part["nextOffset"].clone();
        }
        assert_eq!(reconstructed,notes);
        params["offset"]=json!(15); // inside the first three-byte character after the ASCII prefix
        assert!(dispatch(&state,"read_review_text",&params).is_err());
        params["offset"]=first["nextOffset"].clone();
        crate::run_mutation(&state,document(&state).unwrap()["revision"].as_u64().unwrap(),"text-chunk-view-change".into(),json!({"type":"view","patch":{"search":"newer"}})).unwrap();
        assert!(dispatch(&state,"read_review_text",&params).unwrap_err().contains("journal changed"));
    }

    #[test]
    fn degraded_owner_gateway_allows_fenced_recovery_and_blocks_normal_mutations() {
        let state=Arc::new(crate::tests::fixture());
        let backup=document(&state).unwrap();
        fs::write(state.data_path.with_extension("json.bak"),serde_json::to_vec(&backup).unwrap()).unwrap();
        drop(state.store.lock().unwrap().take());
        fs::write(&state.data_path,b"damaged fixture").unwrap();
        *state.store.lock().unwrap()=Some(crate::store::Store::open(state.data_path.clone(),&state.volumes[0].root).unwrap());
        let (handle,public)=start(&state,&state.runtime).unwrap();
        *state.gateway.lock().unwrap()=public;
        *state.gateway_server.lock().unwrap()=Some(handle);
        let descriptor:Value=serde_json::from_slice(&fs::read(state.runtime.join("mcp-gateway.json")).unwrap()).unwrap();
        let address=descriptor["endpoint"].as_str().unwrap().strip_prefix("http://").unwrap().strip_suffix("/rpc").unwrap().parse().unwrap();
        let token=descriptor["token"].as_str().unwrap();let instance=descriptor["instanceId"].as_str().unwrap();
        let (_,read)=exchange(address,r#"{"method":"get_recovery_state","params":{}}"#,token,instance,"");
        assert_eq!(read["ok"],true);assert_eq!(read["result"]["available"],true);
        assert!(dispatch(&state,"mutate",&json!({})).is_err());
        let req=json!({"method":"recover_review","params":{"expectedInstanceId":instance,"operationId":"http-recover-fixture","expectedOriginalSha256":read["result"]["originalSha256"],"expectedBackupSha256":read["result"]["backupSha256"],"confirmed":true,"basis":"user_instruction","reason":"Explicit synthetic recovery"}}).to_string();
        assert_eq!(exchange(address,&req,"wrong",instance,"").0,401);
        let(_,done)=exchange(address,&req,token,instance,"");assert_eq!(done["ok"],true,"{done}");
        let(_,again)=exchange(address,&req,token,instance,"");assert_eq!(again["result"]["replayed"],true);
        assert_eq!(call(&state,"get_state",json!({}))["problem"],Value::Null);
    }

    #[test]
    fn authenticated_copy_control_routes_share_fences_and_exact_retry_receipts() {
        let state=Arc::new(crate::tests::fixture());
        let source=std::path::Path::new(&state.volumes[0].root).join("Work").join("keep.txt");
        fs::write(&source,b"synthetic original").unwrap();
        fs::create_dir_all(state.data_path.parent().unwrap().join("destination")).unwrap();
        crate::run_mutation(&state,document(&state).unwrap()["revision"].as_u64().unwrap(),"http-copy-mark".into(),json!({"type":"classify","path":source.to_string_lossy(),"status":"migrate-now"})).unwrap();
        let (_handle,public)=start(&state,&state.runtime).unwrap();*state.gateway.lock().unwrap()=public;
        let descriptor:Value=serde_json::from_slice(&fs::read(state.runtime.join("mcp-gateway.json")).unwrap()).unwrap();
        let address=descriptor["endpoint"].as_str().unwrap().strip_prefix("http://").unwrap().strip_suffix("/rpc").unwrap().parse().unwrap();
        let token=descriptor["token"].as_str().unwrap();let instance=descriptor["instanceId"].as_str().unwrap();
        let q=call(&state,"copy_queue_snapshot",json!({}));
        let mut request=json!({"expectedInstanceId":instance,"expectedRevision":q["revision"],"operationId":"http-copy-start-fixture","expectedQueueSignature":q["queueSignature"],"expectedSourceVolumeSignature":q["sourceVolumeSignature"],"expectedDestinationId":q["destination"]["id"],"confirmed":true,"action":"start","mode":"all","scope":null,"basis":"user_instruction","reason":"Explicit synthetic copy"});
        request["confirmed"]=json!(false);assert!(dispatch(&state,"control_copy_queue",&request).is_err());request["confirmed"]=json!(true);
        let body=json!({"method":"control_copy_queue","params":request}).to_string();
        assert_eq!(exchange(address,&body,"wrong",instance,"").0,401);
        let(_,started)=exchange(address,&body,token,instance,"");assert_eq!(started["ok"],true,"{started}");
        let(_,again)=exchange(address,&body,token,instance,"");assert_eq!(again["result"]["replayed"],true,"{again}");
        assert_eq!(again["result"]["receipt"]["jobId"],started["result"]["receipt"]["jobId"]);
        let deadline=std::time::Instant::now()+Duration::from_secs(10);
        while state.copy_queue.status().unwrap()["running"]==true {assert!(std::time::Instant::now()<deadline);thread::sleep(Duration::from_millis(10));}
        assert_eq!(fs::read(&source).unwrap(),b"synthetic original");
        assert_eq!(fs::read(state.data_path.parent().unwrap().join("destination\\Work\\keep.txt")).unwrap(),b"synthetic original");
    }

    #[test]
    fn agent_suggestions_and_user_directed_changes_keep_provenance_and_idempotence() {
        let state = Arc::new(crate::tests::fixture());
        let path = format!("{}\\Work", state.volumes[0].root);
        let initial = document(&state).unwrap()["revision"].as_u64().unwrap();
        let proposal = json!({"expectedRevision":initial,"operationId":"agent-suggestion","basis":"assistant_suggestion","reason":"Tentative grouping for the user to review","operation":{"type":"classify","path":path,"status":"last-moment"}});
        let result = call(&state, "mutate", proposal.clone());
        assert_eq!(result["decisionCount"], 0);
        assert_eq!(result["suggestionCount"], 1);
        assert_eq!(
            call(&state, "mutate", proposal)["revision"],
            result["revision"]
        );
        let suggestions = call(&state, "list_suggestions", json!({}));
        assert_eq!(suggestions["entries"][0]["provenance"]["kind"], "agent-mcp");
        let directed = json!({"expectedRevision":result["revision"],"operationId":"agent-decision","basis":"user_instruction","reason":"the user explicitly requested this fixture classification","operation":{"type":"classify","path":path,"status":"on-external"}});
        let result = call(&state, "mutate", directed);
        assert_eq!(result["decision"]["provenance"]["kind"], "agent-mcp");
        assert_eq!(
            result["decision"]["verification"]["kind"],
            "manual-assertion"
        );
        let ui = boot(&state);
        assert_eq!(ui["document"]["revision"], result["revision"]);
        assert_eq!(call(&state, "get_state", json!({}))["decisionCount"], 1);
        assert!(dispatch(&state,"mutate",&json!({"expectedRevision":result["revision"],"operationId":"no-basis","operation":{"type":"undo"}})).is_err());
        let undo = call(
            &state,
            "mutate",
            json!({"expectedRevision":result["revision"],"operationId":"agent-undo","basis":"user_instruction","reason":"the user requested undo in the fixture","operation":{"type":"undo"}}),
        );
        assert_eq!(undo["decisionCount"], 0);
        let doc = document(&state).unwrap();
        assert_eq!(
            doc["history"].as_array().unwrap().last().unwrap()["provenance"]["kind"],
            "agent-mcp"
        );
    }
    #[test]
    fn invalid_methods_and_paths_cannot_escape_review_scope() {
        let state = Arc::new(crate::tests::fixture());
        assert!(dispatch(&state, "recover", &json!({})).is_err());
        assert!(dispatch(&state, "delete", &json!({"path":"C:\\"})).is_err());
        assert!(dispatch(&state, "list_folder", &json!({"path":"Z:\\outside"})).is_err());
        assert!(dispatch(&state,"mutate",&json!({"expectedRevision":document(&state).unwrap()["revision"],"operationId":"bad-suggestion","basis":"assistant_suggestion","reason":"Unapproved undo","operation":{"type":"undo"}})).is_err());
    }
    #[test]
    fn durable_agent_receipts_survive_ui_autosaves_and_reject_payload_reuse() {
        let state = Arc::new(crate::tests::fixture());
        let path = format!("{}\\Work", state.volumes[0].root);
        let classification = json!({"expectedRevision":document(&state).unwrap()["revision"],"operationId":"durable-classification","basis":"user_instruction","reason":"the user requested the fixture decision","operation":{"type":"classify","path":path,"status":"last-moment"}});
        let first = call(&state, "mutate", classification.clone());
        let view = json!({"expectedRevision":first["revision"],"operationId":"durable-view","operation":{"type":"view","patch":{"search":"original agent search"}}});
        let view_result = call(&state, "mutate", view.clone());
        for number in 0..260 {
            let revision = document(&state).unwrap()["revision"].as_u64().unwrap();
            crate::run_mutation(
                &state,
                revision,
                format!("ui-autosave-{number}"),
                json!({"type":"view","patch":{"page":number,"search":"current user search"}}),
            )
            .unwrap();
        }
        let before = document(&state).unwrap();
        let recent = before["recentOperations"].as_array().unwrap();
        assert!(!recent
            .iter()
            .any(|id| id == "durable-classification" || id == "durable-view"));
        let duplicate = call(&state, "mutate", classification.clone());
        assert_eq!(duplicate["revision"], before["revision"]);
        assert_eq!(
            duplicate["operationReceipt"]["committedRevision"],
            first["revision"]
        );
        assert_eq!(
            call(&state, "mutate", view)["operationReceipt"]["committedRevision"],
            view_result["revision"]
        );
        let after = document(&state).unwrap();
        assert_eq!(after["view"]["search"], "current user search");
        assert_eq!(after["history"].as_array().unwrap().len(), 1);
        let mut different = classification;
        different["operation"]["status"] = json!("erase");
        different["expectedInstanceId"] = state.gateway.lock().unwrap()["instanceId"].clone();
        assert!(dispatch(&state, "mutate", &different)
            .unwrap_err()
            .contains("different payload"));
        assert_eq!(document(&state).unwrap()["revision"], before["revision"]);
        let persisted: Value =
            serde_json::from_slice(&fs::read(&state.data_path).unwrap()).unwrap();
        assert_eq!(persisted["agentOperations"].as_object().unwrap().len(), 2);
    }
    #[test]
    fn prepared_writes_require_the_callers_current_instance() {
        let state = Arc::new(crate::tests::fixture());
        let initial = document(&state).unwrap()["revision"].clone();
        let mut prepared = json!({"expectedRevision":initial,"operationId":"prepared-view","expectedInstanceId":"fixture-native-instance","operation":{"type":"view","patch":{"search":"prepared before restart"}}});
        *state.gateway.lock().unwrap() =
            json!({"state":"running","instanceId":"replacement-fixture-instance"});
        assert!(dispatch(&state, "mutate", &prepared)
            .unwrap_err()
            .contains("instance changed"));
        assert_eq!(document(&state).unwrap()["revision"], initial);
        assert!(dispatch(
            &state,
            "scan",
            &json!({"mode":"pause","expectedInstanceId":"fixture-native-instance"})
        )
        .is_err());
        for method in ["audit_control", "audit_read_notifications"] {
            assert!(dispatch(
                &state,
                method,
                &json!({"action":"pause","expectedInstanceId":"fixture-native-instance"})
            )
            .unwrap_err()
            .contains("instance changed"));
            assert!(dispatch(&state, method, &json!({"action":"pause"}))
                .unwrap_err()
                .contains("expectedInstanceId"));
        }
        prepared["expectedInstanceId"] = json!("replacement-fixture-instance");
        assert!(dispatch(&state, "mutate", &prepared).is_ok());
    }
    #[test]
    fn mcp_projections_bound_notes_and_keep_stored_originals() {
        let state = Arc::new(crate::tests::fixture());
        let path = format!("{}\\Work", state.volumes[0].root);
        let note = "🙂".repeat(250_000);
        crate::run_mutation(
            &state,
            document(&state).unwrap()["revision"].as_u64().unwrap(),
            "long-ui-note".into(),
            json!({"type":"note","path":path,"notes":note}),
        )
        .unwrap();
        let details = call(&state, "get_details", json!({"path":path}));
        assert!(details["decision"]["notes"].as_str().unwrap().len() <= 16 * 1024);
        assert_eq!(
            details["decision"]["truncation"]["fields"]["notes"]["originalBytes"],
            1_000_000
        );
        assert!(
            details["history"][0]["after"]["notes"]
                .as_str()
                .unwrap()
                .len()
                <= 4096
        );
        let decisions = call(&state, "list_decisions", json!({}));
        assert!(decisions["entries"][0]["notes"].as_str().unwrap().len() <= 16 * 1024);
        let key = filesystem::identify(&path, &state.volumes).unwrap().2;
        assert_eq!(
            document(&state).unwrap()["decisions"][&key]["notes"]
                .as_str()
                .unwrap(),
            note
        );
        let normal = response_bytes(&json!({"ok":true,"result":details}));
        assert!(normal.len() < MAX_RESPONSE);
        assert_eq!(
            serde_json::from_slice::<Value>(&normal).unwrap()["ok"],
            true
        );
        let fallback =
            response_bytes(&json!({"ok":true,"unboundedFutureField":"x".repeat(MAX_RESPONSE+1)}));
        assert!(fallback.len() < 512);
        assert_eq!(
            serde_json::from_slice::<Value>(&fallback).unwrap()["ok"],
            false
        );
    }
    #[test]
    fn details_history_is_scoped_to_stable_volume_identity() {
        let state = Arc::new(crate::tests::fixture());
        let path = format!("{}\\Work", state.volumes[0].root);
        crate::run_mutation(
            &state,
            document(&state).unwrap()["revision"].as_u64().unwrap(),
            "current-volume".into(),
            json!({"type":"classify","path":path,"status":"last-moment"}),
        )
        .unwrap();
        {
            let mut locked = state.store.lock().unwrap();
            let store = locked.as_mut().unwrap();
            let mut doc = store.doc.clone();
            doc["history"].as_array_mut().unwrap().push(
                json!({"id":"old-volume","type":"note","path":path,"key":"different-volume|Work"}),
            );
            doc["history"]
                .as_array_mut()
                .unwrap()
                .push(json!({"id":"legacy-no-identity","type":"note","path":path}));
            store.commit(doc).unwrap();
        }
        let details = call(&state, "get_details", json!({"path":path}));
        assert_eq!(details["history"].as_array().unwrap().len(), 1);
        assert_eq!(details["history"][0]["id"], "current-volume");
        assert_eq!(
            document(&state).unwrap()["history"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }
    fn exchange(
        address: std::net::SocketAddr,
        body: &str,
        token: &str,
        instance: &str,
        extra: &str,
    ) -> (u16, Value) {
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request=format!("POST /rpc HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nX-Migration-Instance: {instance}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",body.len());
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (status, body) = response.split_once("\r\n\r\n").unwrap();
        let code = status.split_whitespace().nth(1).unwrap().parse().unwrap();
        (code, serde_json::from_str(body).unwrap())
    }
    #[test]
    fn live_loopback_rejects_auth_origin_instance_and_reads_shared_state() {
        let state = Arc::new(crate::tests::fixture());
        let (handle, public) = start(&state, &state.runtime).unwrap();
        *state.gateway.lock().unwrap() = public;
        let descriptor: Value =
            serde_json::from_slice(&fs::read(state.runtime.join("mcp-gateway.json")).unwrap())
                .unwrap();
        let address = descriptor["endpoint"]
            .as_str()
            .unwrap()
            .trim_start_matches("http://")
            .trim_end_matches("/rpc")
            .parse()
            .unwrap();
        let token = descriptor["token"].as_str().unwrap();
        let instance = descriptor["instanceId"].as_str().unwrap();
        let body = r#"{"method":"get_state","params":{}}"#;
        let mut split = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        split
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let headers=format!("POST /rpc HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nX-Migration-Instance: {instance}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",body.len());
        split.write_all(headers.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(80));
        split.write_all(body.as_bytes()).unwrap();
        let mut split_response = String::new();
        split.read_to_string(&mut split_response).unwrap();
        assert!(
            split_response.starts_with("HTTP/1.1 200 "),
            "Split request failed: {split_response}"
        );
        let parsed: Value =
            serde_json::from_str(split_response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(exchange(address, body, "wrong", instance, "").0, 401);
        assert_eq!(
            exchange(
                address,
                body,
                token,
                instance,
                "Origin: https://example.invalid\r\n"
            )
            .0,
            403
        );
        assert_eq!(exchange(address, body, token, "stale-instance", "").0, 409);
        let (code, response) = exchange(address, body, token, instance, "");
        assert_eq!(code, 200);
        assert_eq!(response["ok"], true);
        assert_eq!(
            response["result"]["revision"],
            document(&state).unwrap()["revision"]
        );
        assert!(response["result"].get("token").is_none());
        assert!(boot(&state)["gateway"].get("token").is_none());
        assert_eq!(
            exchange(
                address,
                r#"{"method":"recover","params":{}}"#,
                token,
                instance,
                ""
            )
            .1["ok"],
            false
        );
        let stopped = Arc::clone(&handle.stop);
        drop(handle);
        assert!(stopped.load(Ordering::Relaxed));
    }
}
