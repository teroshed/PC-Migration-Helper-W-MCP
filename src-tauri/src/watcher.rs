//! Windows directory notifications are change hints pending an audit/hash pass.
//! No enumeration loop or content reads are performed by the watcher.
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_IO_PENDING, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, WAIT_FAILED,
        WAIT_OBJECT_0,
    },
    Storage::FileSystem::{
        ReadDirectoryChangesW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
        FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    },
    System::{
        Threading::{CreateEventW, ResetEvent, SetEvent, WaitForMultipleObjects, INFINITE},
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
    },
};

const BUFFER_BYTES: usize = 64 * 1024;
const CHANNEL_CAPACITY: usize = 256;
const MAX_PENDING_PATHS: usize = 1024;
const DEBOUNCE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Removed,
    Modified,
    RenamedFrom,
    RenamedTo,
    Multiple,
}
impl ChangeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Removed => "removed",
            Self::Modified => "modified",
            Self::RenamedFrom => "renamed-from",
            Self::RenamedTo => "renamed-to",
            Self::Multiple => "multiple",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    Changed {
        root: String,
        path: String,
        kind: ChangeKind,
    },
    RescanRequired {
        root: String,
        reason: String,
    },
    Unavailable {
        root: String,
        reason: String,
    },
}

struct RootSignals {
    root: String,
    overflow: AtomicBool,
    unavailable: Mutex<Option<String>>,
}
enum Message {
    Changed {
        root: usize,
        path: String,
        kind: ChangeKind,
    },
    Rescan {
        root: usize,
        reason: String,
    },
}
#[derive(Clone)]
struct Exclusion {
    value: String,
    prefix: bool,
}

pub struct WatchHandle {
    stop: Arc<OwnedHandle>,
    stopped: Arc<AtomicBool>,
    dispatcher_stopped: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    dispatcher: Option<JoinHandle<()>>,
}
impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        unsafe {
            SetEvent(self.stop.as_raw_handle());
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
        self.dispatcher_stopped.store(true, Ordering::SeqCst);
        if let Some(handle) = self.dispatcher.take() {
            let _ = handle.join();
        }
    }
}

fn event() -> Result<OwnedHandle, String> {
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if handle.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}
fn normalized(path: &str) -> Result<String, String> {
    crate::filesystem::normalize(path)
}
fn under(path: &str, root: &str) -> bool {
    crate::filesystem::under(path, root)
}
fn exclusions(paths: Vec<String>) -> Result<Vec<Exclusion>, String> {
    paths
        .into_iter()
        .map(|path| {
            let prefix = path.ends_with('*');
            let path = if prefix {
                &path[..path.len() - 1]
            } else {
                &path
            };
            if path.contains('*') || path.contains('?') {
                return Err(
                    "Watcher exclusions support exact paths or a trailing * prefix only".into(),
                );
            }
            let value = if prefix {
                // A prefix is not itself a filesystem path: journal.* intentionally
                // leaves a trailing dot. Validate its parent/segments with a safe suffix.
                const SENTINEL: &str = "__watch_prefix_sentinel__";
                let validated = normalized(&format!("{path}{SENTINEL}"))?;
                validated
                    .strip_suffix(SENTINEL)
                    .ok_or("Invalid watcher exclusion prefix")?
                    .to_string()
            } else {
                normalized(path)?
            };
            Ok(Exclusion {
                value: value.to_lowercase(),
                prefix,
            })
        })
        .collect()
}
fn excluded(path: &str, exclusions: &[Exclusion]) -> bool {
    let path = path.to_lowercase();
    exclusions.iter().any(|rule| {
        if rule.prefix {
            path.starts_with(&rule.value)
        } else {
            under(&path, &rule.value)
        }
    })
}
fn open_directory(root: &str) -> Result<File, String> {
    // OPEN_REPARSE_POINT protects the final component only. Reject linked
    // ancestors before opening a nested fixture/review root as well.
    if !ancestors_safe(root, &root[..3])? {
        return Err("The watcher root has a reparse ancestor; its target is not followed".into());
    }
    let file = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OVERLAPPED,
        )
        .open(root)
        .map_err(|e| e.to_string())?;
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_dir() || crate::filesystem::reparse(&metadata) {
        return Err(
            "The watcher root must be a real directory; a reparse root is not followed".into(),
        );
    }
    Ok(file)
}

/// `callback` must return promptly: record pending changes/gaps and queue expensive hashing elsewhere.
/// Exclusions are exact paths/subtrees, or an explicit trailing-* prefix (e.g. data-file artifacts).
pub fn start(
    roots: Vec<String>,
    excluded_paths: Vec<String>,
    callback: Arc<dyn Fn(WatchEvent) + Send + Sync>,
) -> Result<WatchHandle, String> {
    if roots.is_empty() || roots.len() > 32 {
        return Err("Configure between 1 and 32 watcher roots".into());
    }
    let roots = roots
        .into_iter()
        .map(|root| normalized(&root))
        .collect::<Result<Vec<_>, _>>()?;
    let exclusions = Arc::new(exclusions(excluded_paths)?);
    let signals: Arc<Vec<RootSignals>> = Arc::new(
        roots
            .iter()
            .map(|root| RootSignals {
                root: root.clone(),
                overflow: AtomicBool::new(false),
                unavailable: Mutex::new(None),
            })
            .collect(),
    );
    let stop = Arc::new(event()?);
    let stopped = Arc::new(AtomicBool::new(false));
    let dispatcher_stopped = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
    let dispatch_signals = signals.clone();
    let dispatch_finished = dispatcher_stopped.clone();
    let dispatcher = thread::Builder::new().name("migration-watch-events".into()).spawn(move || {
        let mut pending: HashMap<(usize,String),ChangeKind> = HashMap::new();
        let mut gaps: HashMap<usize,String> = HashMap::new();
        let mut last_flush = Instant::now();
        loop {
            let mut disconnected=false;
            let incoming=if dispatch_finished.load(Ordering::SeqCst){
                match receiver.try_recv(){
                    Ok(message)=>Ok(message),
                    Err(mpsc::TryRecvError::Empty|mpsc::TryRecvError::Disconnected)=>Err(mpsc::RecvTimeoutError::Disconnected),
                }
            }else{receiver.recv_timeout(Duration::from_millis(100))};
            match incoming {
                Ok(Message::Changed { root, path, kind }) => {
                    if pending.len() >= MAX_PENDING_PATHS && !pending.contains_key(&(root,path.clone())) {
                        dispatch_signals[root].overflow.store(true, Ordering::Relaxed);
                    } else {
                        pending.entry((root,path)).and_modify(|old| { if *old != kind { *old=ChangeKind::Multiple; } }).or_insert(kind);
                    }
                },
                Ok(Message::Rescan { root, reason }) => { gaps.insert(root,reason); },
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(mpsc::RecvTimeoutError::Disconnected) => {disconnected=true;},
            }
            // Drop first joins readers, then drains this bounded queue. A separate
            // stop flag also covers startup failure while a local sender still exists.
            let finishing = disconnected;
            if last_flush.elapsed() >= DEBOUNCE || finishing {
                for (index, signal) in dispatch_signals.iter().enumerate() {
                    if signal.overflow.swap(false,Ordering::SeqCst) {
                        gaps.insert(index,"Directory notification events were dropped or exceeded the coalescing budget; rescan required to restore coverage".into());
                    }
                    if let Ok(mut unavailable) = signal.unavailable.lock() {
                        if let Some(reason) = unavailable.take() { callback(WatchEvent::Unavailable { root:signal.root.clone(),reason }); }
                    }
                }
                for (root,reason) in gaps.drain() {
                    pending.retain(|(index,_),_| *index != root);
                    callback(WatchEvent::RescanRequired { root:dispatch_signals[root].root.clone(),reason });
                }
                for ((root,path),kind) in pending.drain() {
                    callback(WatchEvent::Changed { root:dispatch_signals[root].root.clone(),path,kind });
                }
                last_flush=Instant::now();
                if finishing { break; }
            }
        }
    }).map_err(|e|e.to_string())?;
    let mut handle = WatchHandle {
        stop: stop.clone(),
        stopped: stopped.clone(),
        dispatcher_stopped,
        threads: Vec::new(),
        dispatcher: Some(dispatcher),
    };
    for (index, root) in roots.into_iter().enumerate() {
        if excluded(&root, &exclusions) {
            continue;
        }
        let directory = match open_directory(&root) {
            Ok(file) => file,
            Err(error) => {
                *signals[index]
                    .unavailable
                    .lock()
                    .map_err(|e| e.to_string())? = Some(format!("Cannot monitor {root}: {error}"));
                continue;
            }
        };
        let completion = event()?;
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let worker_stop = stop.clone();
        let worker_stopped = stopped.clone();
        let worker_signals = signals.clone();
        let worker_exclusions = exclusions.clone();
        let worker_sender = sender.clone();
        let worker = thread::Builder::new()
            .name(format!("migration-watch-{index}"))
            .spawn(move || {
                watch_directory(
                    index,
                    root,
                    directory,
                    completion,
                    worker_stop,
                    worker_stopped,
                    worker_sender,
                    worker_signals,
                    worker_exclusions,
                    ready_sender,
                );
            })
            .map_err(|e| e.to_string())?;
        handle.threads.push(worker);
        if ready_receiver.recv_timeout(Duration::from_secs(2)).is_err() {
            signals[index].overflow.store(true, Ordering::SeqCst);
        }
    }
    Ok(handle)
}

fn mark_rescan(
    sender: &SyncSender<Message>,
    signals: &[RootSignals],
    root: usize,
    reason: impl Into<String>,
) {
    if sender
        .try_send(Message::Rescan {
            root,
            reason: reason.into(),
        })
        .is_err()
    {
        signals[root].overflow.store(true, Ordering::SeqCst);
    }
}
fn send_change(
    sender: &SyncSender<Message>,
    signals: &[RootSignals],
    root: usize,
    path: String,
    kind: ChangeKind,
) {
    if let Err(TrySendError::Full(_)) = sender.try_send(Message::Changed { root, path, kind }) {
        signals[root].overflow.store(true, Ordering::SeqCst);
    }
}
fn cancel_and_drain(directory: &File, operation: &mut OVERLAPPED) {
    unsafe {
        CancelIoEx(directory.as_raw_handle(), operation);
        let mut transferred = 0;
        // Buffer and OVERLAPPED remain alive until the kernel completes cancellation.
        GetOverlappedResult(directory.as_raw_handle(), operation, &mut transferred, 1);
    }
}

#[allow(clippy::too_many_arguments)]
fn watch_directory(
    index: usize,
    root: String,
    directory: File,
    completion: OwnedHandle,
    stop: Arc<OwnedHandle>,
    stopped: Arc<AtomicBool>,
    sender: SyncSender<Message>,
    signals: Arc<Vec<RootSignals>>,
    exclusions: Arc<Vec<Exclusion>>,
    ready: SyncSender<()>,
) {
    // u32 allocation ensures the DWORD alignment required by ReadDirectoryChangesW.
    let mut buffer = vec![0u32; BUFFER_BYTES / 4];
    let mut first = true;
    while !stopped.load(Ordering::SeqCst) {
        let mut operation: OVERLAPPED = unsafe { std::mem::zeroed() };
        operation.hEvent = completion.as_raw_handle();
        unsafe {
            ResetEvent(completion.as_raw_handle());
        }
        let result = unsafe {
            ReadDirectoryChangesW(
                directory.as_raw_handle(),
                buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
                1,
                FILE_NOTIFY_CHANGE_FILE_NAME
                    | FILE_NOTIFY_CHANGE_DIR_NAME
                    | FILE_NOTIFY_CHANGE_SIZE
                    | FILE_NOTIFY_CHANGE_LAST_WRITE
                    | FILE_NOTIFY_CHANGE_CREATION
                    | FILE_NOTIFY_CHANGE_ATTRIBUTES,
                std::ptr::null_mut(),
                &mut operation,
                None,
            )
        };
        if result == 0
            && std::io::Error::last_os_error().raw_os_error() != Some(ERROR_IO_PENDING as i32)
        {
            if let Ok(mut error) = signals[index].unavailable.lock() {
                *error = Some(format!(
                    "Directory monitoring stopped: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let _ = ready.try_send(());
            break;
        }
        if first {
            let _ = ready.try_send(());
            first = false;
        }
        let waits = [stop.as_raw_handle(), completion.as_raw_handle()];
        let outcome = unsafe { WaitForMultipleObjects(2, waits.as_ptr(), 0, INFINITE) };
        if outcome == WAIT_OBJECT_0 || stopped.load(Ordering::SeqCst) {
            cancel_and_drain(&directory, &mut operation);
            break;
        }
        if outcome == WAIT_FAILED || outcome != WAIT_OBJECT_0 + 1 {
            cancel_and_drain(&directory, &mut operation);
            if let Ok(mut error) = signals[index].unavailable.lock() {
                *error = Some(
                    "Directory notification wait failed; monitoring coverage is unavailable".into(),
                );
            }
            break;
        }
        let mut transferred = 0;
        let complete = unsafe {
            GetOverlappedResult(directory.as_raw_handle(), &operation, &mut transferred, 0)
        };
        if complete == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32)
                && stopped.load(Ordering::SeqCst)
            {
                break;
            }
            if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                mark_rescan(
                    &sender,
                    &signals,
                    index,
                    "Windows directory notification buffer overflowed; a rescan is required",
                );
                continue;
            }
            if let Ok(mut problem) = signals[index].unavailable.lock() {
                *problem = Some(format!("Directory monitoring failed: {error}"));
            }
            break;
        }
        if transferred == 0 {
            mark_rescan(
                &sender,
                &signals,
                index,
                "Windows discarded directory notifications; a rescan is required",
            );
            continue;
        }
        if transferred as usize > BUFFER_BYTES {
            mark_rescan(
                &sender,
                &signals,
                index,
                "Windows returned an invalid notification size; a rescan is required",
            );
            continue;
        }
        let bytes = unsafe {
            std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), transferred as usize)
        };
        match parse_notifications(bytes) {
            Ok(events) => {
                for (relative, kind) in events {
                    let path = Path::new(&root)
                        .join(relative)
                        .to_string_lossy()
                        .to_string();
                    let path = match normalized(&path) {
                        Ok(path) if under(&path, &root) => path,
                        _ => {
                            mark_rescan(
                                &sender,
                                &signals,
                                index,
                                "An unrepresentable notification path requires a rescan",
                            );
                            continue;
                        }
                    };
                    if excluded(&path, &exclusions) {
                        continue;
                    }
                    match ancestors_safe(&path, &root) {
                        Ok(true) => send_change(&sender, &signals, index, path, kind),
                        Ok(false) => {} // Linked targets are outside the no-follow watcher scope.
                        Err(reason) => mark_rescan(&sender, &signals, index, reason),
                    }
                }
            }
            Err(reason) => mark_rescan(&sender, &signals, index, reason),
        }
    }
}

fn ancestors_safe(path: &str, root: &str) -> Result<bool, String> {
    let mut ancestors = Vec::new();
    let mut current = Path::new(path).parent();
    while let Some(parent) = current {
        let text = parent.to_string_lossy();
        if !under(&text, root) {
            break;
        }
        ancestors.push(parent);
        current = parent.parent();
    }
    // Inspect from the root outward. Probing a deeper path first could resolve
    // an earlier junction before that reparse ancestor has been rejected.
    for parent in ancestors.into_iter().rev() {
        match fs::symlink_metadata(parent) {
            Ok(metadata) if crate::filesystem::reparse(&metadata) => return Ok(false),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(format!(
                    "Cannot establish notification coverage at {}: {e}; rescan required",
                    parent.display()
                ))
            }
        }
    }
    Ok(true)
}
fn parse_notifications(bytes: &[u8]) -> Result<Vec<(String, ChangeKind)>, String> {
    let mut offset = 0usize;
    let mut result = Vec::new();
    loop {
        let header = bytes
            .get(offset..offset + 12)
            .ok_or("Truncated directory notification; rescan required")?;
        let read =
            |start| u32::from_le_bytes(header[start..start + 4].try_into().unwrap()) as usize;
        let next = read(0);
        let action = read(4);
        let name_bytes = read(8);
        if name_bytes % 2 != 0 {
            return Err("Invalid Unicode notification; rescan required".into());
        }
        let name = bytes
            .get(offset + 12..offset + 12 + name_bytes)
            .ok_or("Truncated directory filename; rescan required")?;
        let wide = name
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        let name = String::from_utf16(&wide)
            .map_err(|_| "Unrepresentable Unicode notification; rescan required")?;
        if name.is_empty()
            || name.starts_with(['\\', '/'])
            || name
                .split(['\\', '/'])
                .any(|part| part == ".." || part == ".")
        {
            return Err("Invalid relative notification path; rescan required".into());
        }
        let kind = match action {
            1 => ChangeKind::Created,
            2 => ChangeKind::Removed,
            3 => ChangeKind::Modified,
            4 => ChangeKind::RenamedFrom,
            5 => ChangeKind::RenamedTo,
            _ => return Err("Unknown directory notification action; rescan required".into()),
        };
        result.push((name, kind));
        if next == 0 {
            break;
        }
        if next < 12 + name_bytes || next % 4 != 0 || offset.checked_add(next).is_none() {
            return Err("Invalid directory notification chain; rescan required".into());
        }
        offset += next;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "migration-watcher-{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }
    #[test]
    fn real_changes_are_coalesced_exclusions_hold_and_stop_cancels_pending_io() {
        let root = fixture("native");
        let ignored = root.join("runtime");
        fs::create_dir(&ignored).unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = events.clone();
        let watcher = start(
            vec![root.to_string_lossy().into()],
            vec![
                ignored.to_string_lossy().into(),
                root.join("journal.*").to_string_lossy().into(),
            ],
            Arc::new(move |event| received.lock().unwrap().push(event)),
        )
        .unwrap();
        let file = root.join("changed.txt");
        fs::write(&file, b"created").unwrap();
        fs::write(&file, b"modified").unwrap();
        fs::write(ignored.join("cache.txt"), b"ignored").unwrap();
        fs::write(root.join("journal.tmp-test"), b"ignored").unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until
            && !events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e,WatchEvent::Changed{path,..}if path.ends_with("changed.txt")))
        {
            thread::sleep(Duration::from_millis(50));
        }
        let captured = events.lock().unwrap().clone();
        assert!(
            captured
                .iter()
                .any(|e| matches!(e,WatchEvent::Changed{path,..}if path.ends_with("changed.txt"))),
            "{captured:?}"
        );
        assert!(!captured.iter().any(|e|matches!(e,WatchEvent::Changed{path,..}if path.contains("cache.txt")||path.contains("journal.tmp-test"))));
        fs::remove_file(&file).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now()<until && !events.lock().unwrap().iter().any(|e|matches!(e,WatchEvent::Changed{path,kind:ChangeKind::Removed,..}if path.ends_with("changed.txt"))){thread::sleep(Duration::from_millis(50));}
        assert!(events.lock().unwrap().iter().any(|e| matches!(
            e,
            WatchEvent::Changed {
                kind: ChangeKind::Removed,
                ..
            }
        )));
        let started = Instant::now();
        drop(watcher);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
    #[test]
    fn parser_and_prefix_rules_reject_ambiguous_paths_and_signal_gaps() {
        let mut valid = Vec::new();
        let name = "nested\\item.txt"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        valid.extend_from_slice(&0u32.to_le_bytes());
        valid.extend_from_slice(&3u32.to_le_bytes());
        valid.extend_from_slice(&(name.len() as u32).to_le_bytes());
        valid.extend_from_slice(&name);
        assert_eq!(
            parse_notifications(&valid).unwrap(),
            vec![("nested\\item.txt".into(), ChangeKind::Modified)]
        );
        assert!(parse_notifications(&valid[..13]).is_err());
        let rules =
            exclusions(vec![r"E:\app\runtime".into(), r"E:\data\journal.*".into()]).unwrap();
        assert!(excluded(r"E:\app\runtime\cache.sqlite", &rules));
        assert!(!excluded(r"E:\app\runtime-other\file", &rules));
        assert!(excluded(r"E:\data\journal.tmp-1", &rules));
        assert!(!excluded(r"E:\data\journal-other.json", &rules));
    }
    #[test]
    fn unavailable_root_is_reported_without_blocking_shutdown() {
        let root = fixture("unavailable").join("missing");
        let events = Arc::new(Mutex::new(Vec::new()));
        let received = events.clone();
        let watcher = start(
            vec![root.to_string_lossy().into()],
            Vec::new(),
            Arc::new(move |e| received.lock().unwrap().push(e)),
        )
        .unwrap();
        drop(watcher);
        assert!(events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, WatchEvent::Unavailable { .. })));
    }

    #[test]
    fn a_full_bounded_channel_sets_a_durable_rescan_signal() {
        let signals = vec![RootSignals {
            root: r"E:\fixture".into(),
            overflow: AtomicBool::new(false),
            unavailable: Mutex::new(None),
        }];
        let (sender, _receiver) = mpsc::sync_channel(1);
        send_change(
            &sender,
            &signals,
            0,
            r"E:\fixture\one.txt".into(),
            ChangeKind::Modified,
        );
        assert!(!signals[0].overflow.load(Ordering::SeqCst));
        send_change(
            &sender,
            &signals,
            0,
            r"E:\fixture\two.txt".into(),
            ChangeKind::Modified,
        );
        assert!(signals[0].overflow.load(Ordering::SeqCst));
        signals[0].overflow.store(false, Ordering::SeqCst);
        mark_rescan(&sender, &signals, 0, "Kernel notification buffer overflow");
        assert!(signals[0].overflow.load(Ordering::SeqCst));
    }
}
