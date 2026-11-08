//! Normal close drains work and exits. Background hiding is an explicit opt-in.
//! One admission gate serializes saving/worker starts with the exit handoff.
use crate::{store::error, AppState, Shared};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex, MutexGuard,
};
use tauri::{Emitter, Manager, State};

#[derive(Default)]
pub struct Session {
    ticket: u64,
    pending: Option<Value>,
    draining: bool,
    ready_to_exit: bool,
    watcher_stopped: bool,
    watcher_gap_pending: bool,
    exit_problem: Option<String>,
    observations: usize,
}
pub struct Control {
    pub enabled: bool,
    ready: AtomicBool,
    problem: Mutex<Option<String>>,
    session: Mutex<Session>,
}
impl Control {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            ready: AtomicBool::new(false),
            problem: Mutex::new(None),
            session: Mutex::new(Session::default()),
        }
    }
    pub fn start_guard(&self) -> Result<MutexGuard<'_, Session>, String> {
        let guard = self.session.lock().map_err(error)?;
        if guard.pending.is_some() || guard.draining {
            return Err("Exit confirmation is open. Cancel Exit before starting more work.".into());
        }
        Ok(guard)
    }

    /// Pending exit still allows the frontend's final draft flush. Once drain
    /// starts, acquiring this gate waits for earlier writes and rejects new ones.
    pub fn edit_guard(&self) -> Result<MutexGuard<'_, Session>, String> {
        let guard = self.session.lock().map_err(error)?;
        if guard.draining || guard.ready_to_exit {
            return Err("The app is stopping safely. New edits were not accepted; keep the pending edit and retry after cancelling exit.".into());
        }
        Ok(guard)
    }

    /// A cache-building request can finish after exit is requested. Drain
    /// waits for its last write without holding the admission mutex.
    pub fn observe(&self) -> Result<Observation<'_>, String> {
        let mut guard = self.start_guard()?;
        guard.observations += 1;
        Ok(Observation(self))
    }
}

pub struct Observation<'a>(&'a Control);
impl Drop for Observation<'_> {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.0.session.lock() {
            guard.observations -= 1;
        }
    }
}

fn activity_at(s: &AppState, observations: usize) -> Result<Value, String> {
    Ok(
        json!({"inventory":s.index.running.load(Ordering::SeqCst), "hash":s.audit.is_running(), "copy":s.copy_queue.status()?["running"] == true,"observations":observations}),
    )
}
pub fn activity(s: &AppState) -> Result<Value, String> {
    let guard = s.background.session.lock().map_err(error)?;
    activity_at(s, guard.observations)
}
pub fn status(s: &AppState) -> Result<Value, String> {
    let guard = s.background.session.lock().map_err(error)?;
    Ok(
        json!({"enabled":s.background.enabled,"ready":s.background.ready.load(Ordering::SeqCst),"problem":*s.background.problem.lock().map_err(error)?,"pending":guard.pending,"draining":guard.draining,"readyToExit":guard.ready_to_exit,"monitoringStopped":guard.watcher_stopped,"exitProblem":guard.exit_problem,"canStart":guard.pending.is_none() && !guard.draining,"activity":activity_at(s,guard.observations)?}),
    )
}
pub fn begin_exit(s: &AppState) -> Result<Value, String> {
    let mut guard = s.background.session.lock().map_err(error)?;
    if let Some(pending) = &guard.pending {
        return Ok(pending.clone());
    }
    let current = activity_at(s, guard.observations)?;
    guard.ticket = guard
        .ticket
        .checked_add(1)
        .ok_or("Exit ticket range exhausted; the app remains open")?;
    guard.exit_problem = None;
    guard.ready_to_exit = false;
    let pending = json!({"ticket":guard.ticket,"activity":current});
    guard.pending = Some(pending.clone());
    Ok(pending)
}
pub fn cancel_exit(s: &AppState, ticket: u64) -> Result<(), String> {
    let mut guard = s.background.session.lock().map_err(error)?;
    if guard.draining {
        return Err("Workers are already stopping. Wait for exit to finish.".into());
    }
    if guard.pending.as_ref().and_then(|p| p["ticket"].as_u64()) != Some(ticket) {
        return Err("This exit confirmation is no longer current.".into());
    }
    if guard.watcher_stopped {
        crate::ensure_watcher(s);
    }
    guard.watcher_stopped = false;
    guard.watcher_gap_pending = false;
    guard.pending = None;
    guard.exit_problem = None;
    Ok(())
}
pub fn show(app: &tauri::AppHandle) -> Result<(), String> {
    let window = app
        .get_webview_window("main")
        .ok_or("Main window unavailable")?;
    window.show().map_err(error)?;
    window.unminimize().map_err(error)?;
    window.set_focus().map_err(error)
}
fn request_exit(app: &tauri::AppHandle) -> Result<Value, String> {
    show(app)?;
    let state = app.state::<Shared>();
    let pending = begin_exit(&state)?;
    if let Err(problem) = app.emit("background-exit-requested", &pending) {
        let _ = cancel_exit(&state, pending["ticket"].as_u64().unwrap());
        return Err(error(problem));
    }
    Ok(pending)
}
pub fn setup(app: &tauri::AppHandle, s: &Shared) {
    if !s.background.enabled {
        return;
    }
    let result = (|| -> Result<(), String> {
        use tauri::{
            menu::{Menu, MenuItem},
            tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
        };
        let open = MenuItem::with_id(
            app,
            "background-open",
            "Open Migration Master",
            true,
            None::<&str>,
        )
        .map_err(error)?;
        let exit = MenuItem::with_id(app, "background-exit", "Exit…", true, None::<&str>)
            .map_err(error)?;
        let menu = Menu::with_items(app, &[&open, &exit]).map_err(error)?;
        // Embedded RGBA avoids reliance on a bundled/default icon being configured.
        let mut pixels = vec![0_u8; 32 * 32 * 4];
        for y in 0..32 {
            for x in 0..32 {
                let offset = (y * 32 + x) * 4;
                let mark = (8..24).contains(&x)
                    && ((8..11).contains(&y) || (15..18).contains(&y) || (22..25).contains(&y));
                pixels[offset..offset + 4].copy_from_slice(if mark {
                    &[220, 240, 255, 255]
                } else {
                    &[32, 85, 145, 255]
                });
            }
        }
        TrayIconBuilder::with_id("migration-master-background")
            .icon(tauri::image::Image::new_owned(pixels, 32, 32))
            .tooltip("Migration Master · scans continue in background")
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(|app, event| {
                let result = match event.id.as_ref() {
                    "background-open" => show(app),
                    "background-exit" => request_exit(app).map(|_| ()),
                    _ => Ok(()),
                };
                if let Err(problem) = result {
                    eprintln!("Tray action failed: {problem}");
                    let _ = show(app);
                }
            })
            .on_tray_icon_event(|tray, event| {
                if matches!(
                    event,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    }
                ) {
                    let _ = show(tray.app_handle());
                }
            })
            .build(app)
            .map_err(error)?;
        Ok(())
    })();
    match result {
        Ok(()) => s.background.ready.store(true, Ordering::SeqCst),
        Err(problem) => {
            if let Ok(mut saved) = s.background.problem.lock() {
                *saved = Some(format!("Background mode unavailable: {problem}. The window will stay open; use Exit to stop safely."));
            }
        }
    }
}
#[tauri::command]
pub fn background_status(state: State<'_, Shared>) -> Result<Value, String> {
    status(&state)
}
#[tauri::command]
pub fn background_begin_exit(state: State<'_, Shared>) -> Result<Value, String> {
    begin_exit(&state)
}
#[tauri::command]
pub fn background_request_exit(app: tauri::AppHandle) -> Result<Value, String> {
    request_exit(&app)
}
#[tauri::command]
pub fn background_cancel_exit(state: State<'_, Shared>, ticket: u64) -> Result<(), String> {
    cancel_exit(&state, ticket)
}
#[tauri::command]
pub fn background_hide(app: tauri::AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    if !state.background.enabled
        || !state.background.ready.load(Ordering::SeqCst)
        || app.tray_by_id("migration-master-background").is_none()
    {
        return Err("No usable tray icon is available. The window is staying open; use Exit to stop safely.".into());
    }
    let guard = state.background.session.lock().map_err(error)?;
    if guard.pending.is_some() || guard.draining {
        return Err("Finish or cancel the Exit confirmation before hiding the window.".into());
    }
    app.get_webview_window("main")
        .ok_or("Main window unavailable")?
        .hide()
        .map_err(error)
}
pub fn drain_exit(
    s: &AppState,
    ticket: u64,
    timeout: std::time::Duration,
) -> Result<Value, String> {
    {
        let mut guard = s.background.session.lock().map_err(error)?;
        if guard.pending.as_ref().and_then(|p| p["ticket"].as_u64()) != Some(ticket) {
            return Err("This exit confirmation is no longer current.".into());
        }
        if guard.ready_to_exit {
            return Ok(json!({"ticket":ticket,"readyToExit":true}));
        }
        if guard.draining {
            return Err("Exit is already waiting for workers to finish safely.".into());
        }
        guard.draining = true;
        guard.exit_problem = None;
        s.index.cancel.store(true, Ordering::SeqCst);
        s.audit.pause();
        s.copy_queue.pause();
    }
    let result: Result<Value, String> = (|| {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let current = activity(s)?;
            if current["inventory"] == false
                && current["hash"] == false
                && current["copy"] == false
                && current["observations"] == 0
            {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err("Workers are still finishing a safe checkpoint. The app has not exited; new work remains blocked. Retry Exit or explicitly cancel it.".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        s.index.join_stopped()?;
        s.audit.join_stopped()?;
        s.copy_queue.join_stopped()?;
        // Join watcher dispatch before checkpointing; it writes derived audit
        // observations independently of the inventory/hash coordinators.
        let watcher = s.watcher.lock().map_err(error)?.take();
        if watcher.is_some() {
            {
                let mut guard = s.background.session.lock().map_err(error)?;
                guard.watcher_stopped = true;
                guard.watcher_gap_pending = true;
            }
            drop(watcher);
        }
        let fault_pending = s.watcher_fault.lock().map_err(error)?.is_some();
        let watcher_gap_pending = s
            .background
            .session
            .lock()
            .map_err(error)?
            .watcher_gap_pending
            || fault_pending;
        if watcher_gap_pending {
            if let Err(problem) = s.audit.record_watch_gap_with_timeout("", "Monitoring stopped during graceful exit. Source changes while the app is closed require a fresh comparison.", deadline.saturating_duration_since(std::time::Instant::now())) {
                *s.watcher_fault.lock().map_err(error)? = Some(problem.clone());
                return Err(problem);
            }
            s.background
                .session
                .lock()
                .map_err(error)?
                .watcher_gap_pending = false;
            *s.watcher_fault.lock().map_err(error)? = None;
        }
        s.index.checkpoint_with_timeout(
            deadline.saturating_duration_since(std::time::Instant::now()),
        )?;
        s.audit.checkpoint_with_timeout(
            deadline.saturating_duration_since(std::time::Instant::now()),
        )?;
        Ok(json!({"ticket":ticket,"readyToExit":true}))
    })();
    let mut guard = s.background.session.lock().map_err(error)?;
    match &result {
        Ok(_) => guard.ready_to_exit = true,
        Err(problem) => {
            // Preserve the ticket/admission barrier after timeout or checkpoint
            // failure. Opening admission requires an explicit cancellation.
            guard.draining = false;
            guard.exit_problem = Some(problem.clone());
        }
    }
    result
}
#[tauri::command]
pub async fn background_confirm_exit(
    app: tauri::AppHandle,
    state: State<'_, Shared>,
    ticket: u64,
) -> Result<(), String> {
    let state = state.inner().clone();
    let worker = state.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        drain_exit(&worker, ticket, std::time::Duration::from_secs(30))
    })
    .await
    .map_err(error)
    .and_then(|r| r);
    if let Err(problem) = result {
        let _ = show(&app);
        return Err(problem);
    }
    app.exit(0);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exit_freezes_starts_without_pausing_and_cancel_releases_gate() {
        let s = crate::tests::fixture();
        s.index.running.store(true, Ordering::SeqCst);
        let first = begin_exit(&s).unwrap();
        assert_eq!(first["activity"]["inventory"], true);
        assert_eq!(begin_exit(&s).unwrap(), first);
        assert!(!s.index.cancel.load(Ordering::SeqCst));
        assert!(s.background.start_guard().is_err());
        assert!(cancel_exit(&s, first["ticket"].as_u64().unwrap() + 1).is_err());
        cancel_exit(&s, first["ticket"].as_u64().unwrap()).unwrap();
        assert!(s.background.start_guard().is_ok());
        s.index.running.store(false, Ordering::SeqCst);
    }
    #[test]
    fn confirmed_exit_requests_pause_and_rejects_stale_confirmation() {
        let s = crate::tests::fixture();
        let first = begin_exit(&s).unwrap();
        let ticket = first["ticket"].as_u64().unwrap();
        assert!(drain_exit(&s, ticket + 1, std::time::Duration::from_secs(1)).is_err());
        assert!(!s.index.cancel.load(Ordering::SeqCst));
        drain_exit(&s, ticket, std::time::Duration::from_secs(1)).unwrap();
        assert!(s.index.cancel.load(Ordering::SeqCst));
        assert!(s.background.start_guard().is_err());
        assert!(cancel_exit(&s, ticket).is_err());
    }

    #[test]
    fn timeout_retains_ticket_and_admission_until_an_explicit_cancel() {
        let s = crate::tests::fixture();
        s.index.running.store(true, Ordering::SeqCst);
        let pending = begin_exit(&s).unwrap();
        let ticket = pending["ticket"].as_u64().unwrap();
        assert!(drain_exit(&s, ticket, std::time::Duration::ZERO).is_err());
        let observed = status(&s).unwrap();
        assert_eq!(observed["pending"], pending);
        assert_eq!(observed["canStart"], false);
        assert_eq!(observed["draining"], false);
        assert!(observed["exitProblem"]
            .as_str()
            .unwrap()
            .contains("still finishing"));
        assert!(s.background.start_guard().is_err());
        assert_eq!(begin_exit(&s).unwrap(), pending);
        s.index.running.store(false, Ordering::SeqCst);
        cancel_exit(&s, ticket).unwrap();
        assert_eq!(status(&s).unwrap()["canStart"], true);
    }

    #[test]
    fn checkpoint_failure_keeps_the_ticket_and_retry_finishes_without_new_admission() {
        let s = crate::tests::fixture();
        let reader = s.index.connect().unwrap();
        reader.execute_batch("BEGIN; SELECT * FROM meta;").unwrap();
        s.index
            .record_gap(&s.volumes[0].root, "Synthetic checkpoint writer")
            .unwrap();
        let ticket = begin_exit(&s).unwrap()["ticket"].as_u64().unwrap();
        assert!(drain_exit(&s, ticket, std::time::Duration::ZERO).is_err());
        assert_eq!(status(&s).unwrap()["canStart"], false);
        reader.execute_batch("COMMIT;").unwrap();
        assert_eq!(
            drain_exit(&s, ticket, std::time::Duration::from_secs(2)).unwrap()["readyToExit"],
            true
        );
        assert_eq!(status(&s).unwrap()["readyToExit"], true);
        assert!(s.background.edit_guard().is_err());
        assert!(s.background.start_guard().is_err());
    }

    #[test]
    fn exit_waits_for_admitted_observations_and_checkpoints_their_final_write() {
        use std::time::Duration;

        let s = crate::tests::fixture();
        let observation = s.background.observe().unwrap();
        let ticket = begin_exit(&s).unwrap()["ticket"].as_u64().unwrap();
        assert_eq!(status(&s).unwrap()["activity"]["observations"], 1);
        assert!(s.background.observe().is_err());
        assert!(drain_exit(&s, ticket, Duration::ZERO).is_err());
        assert_eq!(status(&s).unwrap()["pending"]["ticket"], ticket);
        assert!(s.background.observe().is_err());

        // Keep an external read snapshot open across the last admitted write.
        // A nonblocking FULL checkpoint must see the still-uncheckpointed work.
        let reader = s.index.connect().unwrap();
        reader.execute_batch("BEGIN; SELECT * FROM meta;").unwrap();
        s.index
            .record_gap(&s.volumes[0].root, "Final admitted observation")
            .unwrap();
        drop(observation);
        assert_eq!(status(&s).unwrap()["activity"]["observations"], 0);
        let error = drain_exit(&s, ticket, Duration::ZERO).unwrap_err();
        assert!(error.contains("checkpoint"), "{error}");
        reader.execute_batch("COMMIT;").unwrap();
        assert_eq!(
            drain_exit(&s, ticket, Duration::from_secs(2)).unwrap()["readyToExit"],
            true
        );
    }

    #[test]
    fn failed_shutdown_watcher_gap_must_persist_before_the_same_ticket_can_exit() {
        use std::time::Duration;

        let s = crate::tests::fixture();
        let audit = rusqlite::Connection::open(s.data_path.with_file_name("audit.sqlite")).unwrap();
        let path = &s.volumes[0].root;
        let before = crate::store::now();
        // Represents a gap already observed by the joined watcher. Hold the
        // writer lock independently of app locks to force the first write to fail.
        s.background.session.lock().unwrap().watcher_gap_pending = true;
        audit.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let ticket = begin_exit(&s).unwrap()["ticket"].as_u64().unwrap();
        assert!(drain_exit(&s, ticket, Duration::ZERO).is_err());
        assert!(!s.audit.changed_below(path, &before).unwrap());
        assert_eq!(status(&s).unwrap()["pending"]["ticket"], ticket);
        audit.execute_batch("ROLLBACK;").unwrap();
        assert_eq!(
            drain_exit(&s, ticket, Duration::from_secs(2)).unwrap()["readyToExit"],
            true
        );
        assert!(s.audit.changed_below(path, &before).unwrap());
        let count: i64 = audit
            .query_row(
                "SELECT count(*) FROM notifications WHERE kind='coverage-gap'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn drain_joins_a_real_copy_coordinator_and_blocks_edits_while_stopping() {
        use std::{
            fs,
            path::Path,
            sync::Barrier,
            time::{Duration, Instant},
        };
        let s = std::sync::Arc::new(crate::tests::fixture());
        fs::create_dir(s.data_path.parent().unwrap().join("destination")).unwrap();
        let source = Path::new(&s.volumes[0].root).join("Work").join("keep.txt");
        fs::write(
            &source,
            b"synthetic source unchanged by cancelled exit fixture",
        )
        .unwrap();
        let before = fs::read(&source).unwrap();
        let doc = crate::run_mutation(
            &s,
            crate::document(&s).unwrap()["revision"].as_u64().unwrap(),
            "exit-copy-mark".into(),
            json!({"type":"classify","path":source.to_string_lossy(),"status":"migrate-now"}),
        )
        .unwrap();
        let barrier = std::sync::Arc::new(Barrier::new(2));
        s.copy_queue.hold_next_worker(barrier.clone());
        s.copy_queue
            .start(
                doc,
                s.volumes.clone(),
                crate::copy_destination(&s),
                "all",
                None,
            )
            .unwrap();
        let ticket = begin_exit(&s).unwrap()["ticket"].as_u64().unwrap();
        let worker = s.clone();
        let drain = std::thread::spawn(move || drain_exit(&worker, ticket, Duration::from_secs(5)));
        let deadline = Instant::now() + Duration::from_secs(2);
        while status(&s).unwrap()["draining"] != true {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(s.background.edit_guard().is_err());
        assert!(s.background.start_guard().is_err());
        barrier.wait();
        assert_eq!(drain.join().unwrap().unwrap()["readyToExit"], true);
        assert_eq!(s.copy_queue.status().unwrap()["running"], false);
        s.copy_queue.join_stopped().unwrap();
        s.index.checkpoint().unwrap();
        s.audit.checkpoint().unwrap();
        assert_eq!(fs::read(&source).unwrap(), before);
        assert!(!s
            .data_path
            .parent()
            .unwrap()
            .join("destination")
            .join("Work")
            .join("keep.txt")
            .exists());
    }
}
