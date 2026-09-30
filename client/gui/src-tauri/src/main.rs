#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use freec_runtime::{Command, Handle, Snapshot};
use serde::{Deserialize, Serialize};
use std::{io::Write, sync::Mutex, time::Duration};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};
use tauri_plugin_updater::UpdaterExt;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    minimize_to_tray: bool,
    check_updates: bool,
    theme: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            minimize_to_tray: true,
            check_updates: true,
            theme: "dark".into(),
        }
    }
}
struct Desktop {
    settings: Mutex<Settings>,
    update: Mutex<Option<tauri_plugin_updater::Update>>,
}

#[tauri::command]
fn snapshot(state: tauri::State<'_, Handle>) -> Snapshot {
    state.snapshot()
}
#[tauri::command]
fn dispatch(state: tauri::State<'_, Handle>, command: Command) -> Result<(), String> {
    state.send(command).map_err(|e| e.to_string())
}
#[tauri::command]
fn settings(state: tauri::State<'_, Desktop>) -> Settings {
    state.settings.lock().unwrap().clone()
}
#[tauri::command]
fn save_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, Desktop>,
    settings: Settings,
) -> Result<(), String> {
    let mut current = state.settings.lock().unwrap();
    let save = || -> Result<(), Box<dyn std::error::Error>> {
        let dir = app.path().app_config_dir()?;
        std::fs::create_dir_all(&dir)?;
        let mut file = tempfile::NamedTempFile::new_in(&dir)?;
        file.write_all(&serde_json::to_vec(&settings)?)?;
        file.as_file().sync_all()?;
        file.persist(dir.join("settings.json"))?;
        Ok(())
    };
    save().map_err(|e| e.to_string())?;
    *current = settings;
    Ok(())
}
fn update_config() -> Option<(&'static str, &'static str)> {
    let repo = option_env!("FREECTIER_GITHUB_REPOSITORY")?.trim();
    let key = option_env!("FREECTIER_UPDATER_PUBLIC_KEY")?.trim();
    let parts: Vec<_> = repo.split('/').collect();
    (parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
        && !key.is_empty())
    .then_some((repo, key))
}
#[derive(Serialize)]
struct UpdateStatus {
    configured: bool,
    version: Option<String>,
}
#[tauri::command]
async fn check_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, Desktop>,
) -> Result<UpdateStatus, String> {
    let Some((repo, key)) = update_config() else {
        return Ok(UpdateStatus {
            configured: false,
            version: None,
        });
    };
    let endpoint = tauri::Url::parse(&format!(
        "https://github.com/{repo}/releases/latest/download/latest.json"
    ))
    .map_err(|e| e.to_string())?;
    let update = app
        .updater_builder()
        .pubkey(key)
        .endpoints(vec![endpoint])
        .map_err(|e| e.to_string())?
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let version = update.as_ref().map(|u| u.version.clone());
    *state.update.lock().unwrap() = update;
    Ok(UpdateStatus {
        configured: true,
        version,
    })
}
#[tauri::command]
async fn install_update(state: tauri::State<'_, Desktop>) -> Result<(), String> {
    let update = state
        .update
        .lock()
        .unwrap()
        .take()
        .ok_or("Сначала проверьте обновления")?;
    update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string())
}
#[tauri::command]
fn quit(app: tauri::AppHandle) {
    app.exit(0);
}
#[tauri::command]
fn version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}
fn show(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

#[cfg(windows)]
fn show_startup_error(message: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(
            window: *mut std::ffi::c_void,
            text: *const u16,
            caption: *const u16,
            flags: u32,
        ) -> i32;
    }
    let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
    let caption: Vec<u16> = "FreeC Tier — ошибка запуска"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // Both UTF-16 buffers are NUL-terminated and live through the synchronous call.
    unsafe {
        MessageBoxW(std::ptr::null_mut(), text.as_ptr(), caption.as_ptr(), 0x10);
    }
}

fn main() {
    // Release builds have no console: retain startup panics for diagnosis.
    let log_path = std::env::var_os("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("FreeC Tier")
        .join("startup.log");
    std::panic::set_hook(Box::new(move |info| {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = writeln!(
                file,
                "{:?}: {info}\n{}",
                std::time::SystemTime::now(),
                std::backtrace::Backtrace::force_capture()
            );
        }
        #[cfg(windows)]
        show_startup_error(&format!(
            "Ошибка FreeC Tier:\n{info}\n\nЛог: {}",
            log_path.display()
        ));
    }));
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _| {
            show(app);
            if let Some(pair) = args.windows(2).find(|p| p[0] == "+connect_lobby") {
                let _ = app.state::<Handle>().send(Command::Join {
                    lobby: pair[1].clone(),
                });
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let app_id = std::env::var("FREECTIER_APP_ID")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(freec_runtime::DEFAULT_APP_ID);
            app.manage(freec_runtime::start(None, app_id)?);
            let path = app.path().app_config_dir()?.join("settings.json");
            let settings = if path.exists() {
                serde_json::from_slice(&std::fs::read(path)?)?
            } else {
                Settings::default()
            };
            app.manage(Desktop {
                settings: Mutex::new(settings),
                update: Mutex::new(None),
            });
            let open = MenuItem::with_id(app, "open", "Открыть FreeC Tier", true, None::<&str>)?;
            let exit = MenuItem::with_id(app, "quit", "Выйти", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &exit])?;
            let mut pixels = Vec::with_capacity(32 * 32 * 4);
            for y in 0..32 {
                for x in 0..32 {
                    let mark = (8..13).contains(&x) && (7..25).contains(&y)
                        || (8..25).contains(&x) && (7..12).contains(&y)
                        || (8..21).contains(&x) && (15..20).contains(&y);
                    pixels.extend_from_slice(if mark {
                        &[255, 238, 238, 255]
                    } else {
                        &[161, 40, 52, 255]
                    });
                }
            }
            TrayIconBuilder::with_id("main")
                .icon(tauri::image::Image::new_owned(pixels, 32, 32))
                .menu(&menu)
                .tooltip("FreeC Tier — сети работают в фоне")
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show(app),
                    "quit" => app.exit(0),
                    _ => {}
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
                        show(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if window.hide().is_ok() {
                    api.prevent_close();
                }
            }
            tauri::WindowEvent::Resized(_)
                if window.is_minimized().unwrap_or(false)
                    && window
                        .state::<Desktop>()
                        .settings
                        .lock()
                        .unwrap()
                        .minimize_to_tray =>
            {
                let _ = window.hide();
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            dispatch,
            settings,
            save_settings,
            check_update,
            install_update,
            quit,
            version
        ])
        .build(tauri::generate_context!())
        .expect("Cannot initialize FreeC Tier")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                let _ = app.state::<Handle>().send(Command::Shutdown);
            }
        });
}
