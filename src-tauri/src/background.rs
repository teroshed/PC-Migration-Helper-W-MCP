//! Window hiding never changes worker state. Only an explicitly confirmed exit
//! pauses work; one gate serializes that decision with every worker start.
use crate::{store::error, AppState, Shared};
use serde_json::{json, Value};
use std::sync::{atomic::{AtomicBool, Ordering}, Mutex, MutexGuard};
use tauri::{Emitter, Manager, State};

#[derive(Default)]
pub struct Session { ticket: u64, pending: Option<Value>, draining: bool }
pub struct Control {
    pub enabled: bool,
    ready: AtomicBool,
    problem: Mutex<Option<String>>,
    session: Mutex<Session>,
}
impl Control {
    pub fn new(enabled: bool) -> Self {
        Self { enabled, ready: AtomicBool::new(false), problem: Mutex::new(None), session: Mutex::new(Session::default()) }
    }
    pub fn start_guard(&self) -> Result<MutexGuard<'_, Session>, String> {
        let guard = self.session.lock().map_err(error)?;
        if guard.pending.is_some() || guard.draining {
            return Err("Exit confirmation is open. Cancel Exit before starting more work.".into());
        }
        Ok(guard)
    }
}
fn activity(s: &AppState) -> Result<Value, String> {
    Ok(json!({"inventory":s.index.running.load(Ordering::SeqCst), "hash":s.audit.is_running(), "copy":s.copy_queue.status()?["running"] == true}))
}
pub fn status(s: &AppState) -> Result<Value, String> {
    let guard = s.background.session.lock().map_err(error)?;
    Ok(json!({"enabled":s.background.enabled,"ready":s.background.ready.load(Ordering::SeqCst),"problem":*s.background.problem.lock().map_err(error)?,"pending":guard.pending,"draining":guard.draining}))
}
fn begin(s: &AppState) -> Result<Value, String> {
    let mut guard = s.background.session.lock().map_err(error)?;
    if let Some(pending) = &guard.pending { return Ok(pending.clone()); }
    let current = activity(s)?;
    guard.ticket += 1;
    let pending = json!({"ticket":guard.ticket,"activity":current});
    guard.pending = Some(pending.clone());
    Ok(pending)
}
fn cancel(s: &AppState, ticket: u64) -> Result<(), String> {
    let mut guard = s.background.session.lock().map_err(error)?;
    if guard.draining { return Err("Workers are already stopping. Wait for exit to finish.".into()); }
    if guard.pending.as_ref().and_then(|p| p["ticket"].as_u64()) != Some(ticket) {
        return Err("This exit confirmation is no longer current.".into());
    }
    guard.pending = None;
    Ok(())
}
pub fn show(app: &tauri::AppHandle) -> Result<(), String> {
    let window = app.get_webview_window("main").ok_or("Main window unavailable")?;
    window.show().map_err(error)?;
    window.unminimize().map_err(error)?;
    window.set_focus().map_err(error)
}
fn request_exit(app: &tauri::AppHandle) -> Result<Value, String> {
    show(app)?;
    let state = app.state::<Shared>();
    let pending = begin(&state)?;
    if let Err(problem) = app.emit("background-exit-requested", &pending) {
        let _ = cancel(&state, pending["ticket"].as_u64().unwrap());
        return Err(error(problem));
    }
    Ok(pending)
}
pub fn setup(app: &tauri::AppHandle, s: &Shared) {
    if !s.background.enabled { return; }
    let result = (|| -> Result<(), String> {
        use tauri::{menu::{Menu, MenuItem}, tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent}};
        let open = MenuItem::with_id(app, "background-open", "Open Migration Master", true, None::<&str>).map_err(error)?;
        let exit = MenuItem::with_id(app, "background-exit", "Exit…", true, None::<&str>).map_err(error)?;
        let menu = Menu::with_items(app, &[&open, &exit]).map_err(error)?;
        // Embedded RGBA avoids reliance on a bundled/default icon being configured.
        let mut pixels = vec![0_u8; 32 * 32 * 4];
        for y in 0..32 { for x in 0..32 {
            let offset = (y * 32 + x) * 4;
            let mark = (8..24).contains(&x) && ((8..11).contains(&y) || (15..18).contains(&y) || (22..25).contains(&y));
            pixels[offset..offset+4].copy_from_slice(if mark { &[220,240,255,255] } else { &[32,85,145,255] });
        }}
        TrayIconBuilder::with_id("migration-master-background")
            .icon(tauri::image::Image::new_owned(pixels, 32, 32))
            .tooltip("Migration Master · scans continue in background")
            .menu(&menu).show_menu_on_left_click(false)
            .on_menu_event(|app, event| {
                let result = match event.id.as_ref() {
                    "background-open" => show(app),
                    "background-exit" => request_exit(app).map(|_| ()),
                    _ => Ok(()),
                };
                if let Err(problem) = result { eprintln!("Tray action failed: {problem}"); let _ = show(app); }
            })
            .on_tray_icon_event(|tray, event| {
                if matches!(event, TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }) {
                    let _ = show(tray.app_handle());
                }
            }).build(app).map_err(error)?;
        Ok(())
    })();
    match result {
        Ok(()) => s.background.ready.store(true, Ordering::SeqCst),
        Err(problem) => { if let Ok(mut saved) = s.background.problem.lock() { *saved = Some(format!("Background mode unavailable: {problem}. The window will stay open; use Exit to stop safely.")); } }
    }
}
#[tauri::command]
pub fn background_status(state: State<'_, Shared>) -> Result<Value, String> { status(&state) }
#[tauri::command]
pub fn background_request_exit(app: tauri::AppHandle) -> Result<Value, String> { request_exit(&app) }
#[tauri::command]
pub fn background_cancel_exit(state: State<'_, Shared>, ticket: u64) -> Result<(), String> { cancel(&state, ticket) }
#[tauri::command]
pub fn background_hide(app: tauri::AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    if !state.background.enabled || !state.background.ready.load(Ordering::SeqCst)
        || app.tray_by_id("migration-master-background").is_none() {
        return Err("No usable tray icon is available. The window is staying open; use Exit to stop safely.".into());
    }
    let guard = state.background.session.lock().map_err(error)?;
    if guard.pending.is_some() || guard.draining { return Err("Finish or cancel the Exit confirmation before hiding the window.".into()); }
    app.get_webview_window("main").ok_or("Main window unavailable")?.hide().map_err(error)
}
fn drain(s: &AppState, ticket: u64) -> Result<(), String> {
    {
        let mut guard = s.background.session.lock().map_err(error)?;
        if guard.pending.as_ref().and_then(|p| p["ticket"].as_u64()) != Some(ticket) || guard.draining {
            return Err("This exit confirmation is no longer current.".into());
        }
        guard.draining = true;
        s.index.cancel.store(true, Ordering::SeqCst);
        s.audit.pause();
        s.copy_queue.pause();
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let current = activity(s)?;
        if current["inventory"] == false && current["hash"] == false && current["copy"] == false { return Ok(()); }
        if std::time::Instant::now() >= deadline { return Err("Workers are still finishing a safe checkpoint. The app has not exited. Wait, then try Exit again.".into()); }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
#[tauri::command]
pub async fn background_confirm_exit(app: tauri::AppHandle, state: State<'_, Shared>, ticket: u64) -> Result<(), String> {
    let state = state.inner().clone();
    let worker = state.clone();
    let result = tauri::async_runtime::spawn_blocking(move || drain(&worker, ticket)).await.map_err(error).and_then(|r| r);
    if let Err(problem) = result {
        if let Ok(mut guard) = state.background.session.lock() { guard.draining = false; guard.pending = None; }
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
        let first = begin(&s).unwrap();
        assert_eq!(first["activity"]["inventory"], true);
        assert_eq!(begin(&s).unwrap(), first);
        assert!(!s.index.cancel.load(Ordering::SeqCst));
        assert!(s.background.start_guard().is_err());
        assert!(cancel(&s, first["ticket"].as_u64().unwrap() + 1).is_err());
        cancel(&s, first["ticket"].as_u64().unwrap()).unwrap();
        assert!(s.background.start_guard().is_ok());
        s.index.running.store(false, Ordering::SeqCst);
    }
    #[test]
    fn confirmed_exit_requests_pause_and_rejects_stale_confirmation() {
        let s = crate::tests::fixture();
        let first = begin(&s).unwrap();
        let ticket = first["ticket"].as_u64().unwrap();
        assert!(drain(&s, ticket + 1).is_err());
        assert!(!s.index.cancel.load(Ordering::SeqCst));
        drain(&s, ticket).unwrap();
        assert!(s.index.cancel.load(Ordering::SeqCst));
        assert!(s.background.start_guard().is_err());
        assert!(cancel(&s, ticket).is_err());
    }
}
