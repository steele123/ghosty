mod config_proxy;
mod lcu_api;
mod models;
mod persistence;
mod presence;
mod riot;
mod service;

use std::sync::{Arc, Mutex};

use models::{AppSnapshot, LaunchGame, PreflightReport, PresenceStatus, StartupStatus};
use service::{AppState, StartOptions};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager, State,
};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

type SharedState = Arc<Mutex<AppState>>;

#[tauri::command]
fn get_snapshot(state: State<'_, SharedState>) -> Result<AppSnapshot, String> {
    Ok(state.lock().map_err(|e| e.to_string())?.snapshot())
}

#[tauri::command]
fn locate_riot_client() -> Result<Option<String>, String> {
    Ok(riot::riot_client_path().map(|p| p.display().to_string()))
}

#[tauri::command]
fn kill_riot_processes(state: State<'_, SharedState>) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state.kill_riot_processes().map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[tauri::command]
fn running_riot_processes() -> Result<Vec<String>, String> {
    riot::running_riot_processes().map_err(|e| e.to_string())
}

#[tauri::command]
fn call_lcu_api(
    method: String,
    endpoint: String,
    body: Option<serde_json::Value>,
) -> Result<lcu_api::LcuApiResponse, String> {
    lcu_api::call_endpoint(&method, &endpoint, body).map_err(|e| e.to_string())
}

#[tauri::command]
fn run_preflight(state: State<'_, SharedState>) -> Result<PreflightReport, String> {
    Ok(state.lock().map_err(|e| e.to_string())?.preflight())
}

#[tauri::command]
fn start_deceive(
    state: State<'_, SharedState>,
    game: LaunchGame,
    game_patchline: String,
    riot_client_params: Option<String>,
    game_params: Option<String>,
    launch_game: bool,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state
        .start(StartOptions {
            game,
            game_patchline,
            riot_client_params,
            game_params,
            launch_game,
        })
        .map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[tauri::command]
fn clean_restart(
    state: State<'_, SharedState>,
    game: LaunchGame,
    game_patchline: String,
    riot_client_params: Option<String>,
    game_params: Option<String>,
    launch_game: bool,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state
        .clean_restart(StartOptions {
            game,
            game_patchline,
            riot_client_params,
            game_params,
            launch_game,
        })
        .map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[tauri::command]
fn stop_deceive(state: State<'_, SharedState>) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state.stop();
    Ok(state.snapshot())
}

#[tauri::command]
fn set_presence_status(
    state: State<'_, SharedState>,
    status: PresenceStatus,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state.set_status(status).map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[tauri::command]
fn set_enabled(state: State<'_, SharedState>, enabled: bool) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state.set_enabled(enabled);
    Ok(state.snapshot())
}

#[tauri::command]
fn set_safe_mode(state: State<'_, SharedState>, safe_mode: bool) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state.set_safe_mode(safe_mode);
    Ok(state.snapshot())
}

#[tauri::command]
fn set_helper_friend(
    state: State<'_, SharedState>,
    helper_friend: bool,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state
        .set_helper_friend(helper_friend)
        .map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[tauri::command]
fn set_auto_accept(
    state: State<'_, SharedState>,
    auto_accept: bool,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state
        .set_auto_accept(auto_accept)
        .map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[tauri::command]
fn set_auto_accept_delay_ms(
    state: State<'_, SharedState>,
    delay_ms: u32,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state
        .set_auto_accept_delay_ms(delay_ms)
        .map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[tauri::command]
fn set_discord_webhook_url(
    state: State<'_, SharedState>,
    url: String,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state
        .set_discord_webhook_url(url)
        .map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[tauri::command]
fn set_connect_to_muc(
    state: State<'_, SharedState>,
    connect_to_muc: bool,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state.set_connect_to_muc(connect_to_muc);
    Ok(state.snapshot())
}

#[tauri::command]
fn set_startup_status(
    state: State<'_, SharedState>,
    startup_status: StartupStatus,
) -> Result<AppSnapshot, String> {
    let mut state = state.lock().map_err(|e| e.to_string())?;
    state
        .set_startup_status(startup_status)
        .map_err(|e| e.to_string())?;
    Ok(state.snapshot())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            let state = Arc::new(Mutex::new(AppState::load()?));
            app.manage(state.clone());
            setup_tray(app.handle(), state.clone())?;
            setup_hotkeys(app.handle(), state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            locate_riot_client,
            kill_riot_processes,
            running_riot_processes,
            call_lcu_api,
            run_preflight,
            start_deceive,
            clean_restart,
            stop_deceive,
            set_presence_status,
            set_enabled,
            set_safe_mode,
            set_helper_friend,
            set_auto_accept,
            set_auto_accept_delay_ms,
            set_discord_webhook_url,
            set_connect_to_muc,
            set_startup_status
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn setup_tray(app: &tauri::AppHandle, state: SharedState) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Ghosty", true, None::<&str>)?;
    let online = MenuItem::with_id(app, "online", "Appear Online", true, None::<&str>)?;
    let offline = MenuItem::with_id(app, "offline", "Appear Offline", true, None::<&str>)?;
    let mobile = MenuItem::with_id(app, "mobile", "Appear Mobile", true, None::<&str>)?;
    let toggle = MenuItem::with_id(app, "toggle", "Enable/Disable Masking", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &online, &offline, &mobile, &toggle, &quit])?;

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Ghosty")
        .menu(&menu)
        .show_menu_on_left_click(false);
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }

    tray.on_tray_icon_event(|tray, event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            if let Some(window) = tray.app_handle().get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
    })
    .on_menu_event(move |app, event| match event.id().as_ref() {
        "show" => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
        "online" => set_tray_status(&state, PresenceStatus::Chat),
        "offline" => set_tray_status(&state, PresenceStatus::Offline),
        "mobile" => set_tray_status(&state, PresenceStatus::Mobile),
        "toggle" => {
            if let Ok(mut state) = state.lock() {
                let enabled = !state.snapshot().enabled;
                state.set_enabled(enabled);
            }
        }
        "quit" => app.exit(0),
        _ => {}
    })
    .build(app)?;

    Ok(())
}

fn setup_hotkeys(app: &tauri::AppHandle, state: SharedState) {
    let shortcuts = [
        (
            Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyO),
            PresenceStatus::Offline,
            "Ctrl+Alt+O",
        ),
        (
            Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyM),
            PresenceStatus::Mobile,
            "Ctrl+Alt+M",
        ),
        (
            Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyN),
            PresenceStatus::Chat,
            "Ctrl+Alt+N",
        ),
    ];

    let manager = app.global_shortcut();
    for (shortcut, status, label) in shortcuts {
        let shortcut_state = state.clone();
        if let Err(error) = manager.on_shortcut(shortcut, move |_app, _shortcut, event| {
            if event.state == ShortcutState::Pressed {
                set_tray_status(&shortcut_state, status);
            }
        }) {
            if let Ok(state) = state.lock() {
                state.note_warning(format!("{label} hotkey unavailable: {error}"));
            }
        }
    }
}

fn set_tray_status(state: &SharedState, status: PresenceStatus) {
    if let Ok(mut state) = state.lock() {
        let _ = state.set_status(status);
        state.set_enabled(true);
    }
}
