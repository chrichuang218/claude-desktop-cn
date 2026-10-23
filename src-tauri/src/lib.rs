#[cfg_attr(target_os = "macos", path = "macos/assistant.rs")]
mod assistant;
#[cfg_attr(target_os = "macos", path = "macos/claude.rs")]
mod claude;
mod engine_cache;
mod operation;
#[cfg_attr(target_os = "macos", path = "macos/patch.rs")]
mod patch;
#[cfg_attr(target_os = "macos", path = "macos/self_update.rs")]
mod self_update;
#[cfg_attr(target_os = "macos", path = "macos/util.rs")]
mod util;

use assistant::{AssistantConfig, AssistantStatus};
use claude::ClaudeStatus;
use operation::{OperationSnapshot, OperationState};
use tauri::Manager;

#[tauri::command]
fn get_platform() -> &'static str {
    std::env::consts::OS
}

pub(crate) fn app_context() -> tauri::Context<tauri::Wry> {
    // On macOS this macro also defines the embedded Info.plist symbol, so
    // startup and the side-effect-free updater check must share one expansion.
    tauri::generate_context!()
}

fn initial_window_size(
    design: tauri::LogicalSize<f64>,
    web_scale: f64,
    available: tauri::PhysicalSize<u32>,
) -> Result<tauri::PhysicalSize<u32>, String> {
    if !web_scale.is_finite() || web_scale <= 0.0 {
        return Err("界面缩放比例无效。".into());
    }
    Ok(tauri::PhysicalSize::new(
        (design.width * web_scale)
            .round()
            .min(available.width as f64) as u32,
        (design.height * web_scale)
            .round()
            .min(available.height as f64) as u32,
    ))
}

#[tauri::command]
fn fit_initial_window(window: tauri::WebviewWindow, web_scale: f64) -> Result<(), String> {
    let monitor = window
        .current_monitor()
        .map_err(|e| e.to_string())?
        .ok_or("无法读取窗口所在屏幕。")?;
    let config = window
        .app_handle()
        .config()
        .app
        .windows
        .iter()
        .find(|config| config.label == window.label())
        .ok_or("未找到窗口尺寸配置。")?;
    let inner = window.inner_size().map_err(|e| e.to_string())?;
    let outer = window.outer_size().map_err(|e| e.to_string())?;
    let frame = tauri::PhysicalSize::new(
        outer.width.saturating_sub(inner.width),
        outer.height.saturating_sub(inner.height),
    );
    let work = monitor.work_area();
    let available = tauri::PhysicalSize::new(
        work.size.width.saturating_sub(frame.width),
        work.size.height.saturating_sub(frame.height),
    );
    // WebView's ratio already includes display DPI and Windows text scaling.
    // Physical units avoid applying the monitor's DPI a second time.
    let size = initial_window_size(
        tauri::LogicalSize::new(config.width, config.height),
        web_scale,
        available,
    )?;
    let minimum = initial_window_size(
        tauri::LogicalSize::new(
            config.min_width.unwrap_or(0.0),
            config.min_height.unwrap_or(0.0),
        ),
        monitor.scale_factor(),
        available,
    )?;
    window
        .set_min_size(Some(minimum))
        .map_err(|e| e.to_string())?;
    window.set_size(size).map_err(|e| e.to_string())?;
    window
        .set_position(tauri::PhysicalPosition::new(
            work.position.x + (available.width.saturating_sub(size.width) / 2) as i32,
            work.position.y + (available.height.saturating_sub(size.height) / 2) as i32,
        ))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn get_status() -> Result<ClaudeStatus, String> {
    claude::status()
}

#[tauri::command]
fn get_assistant_status() -> Result<AssistantStatus, String> {
    assistant::status()
}

#[tauri::command]
fn get_config() -> Result<AssistantConfig, String> {
    assistant::load_config()
}

#[tauri::command]
fn save_config(config: AssistantConfig) -> Result<AssistantConfig, String> {
    assistant::save_config(config)
}

#[tauri::command]
fn get_operation_snapshot(state: tauri::State<'_, OperationState>) -> OperationSnapshot {
    state.snapshot()
}

#[tauri::command]
fn start_operation(
    kind: String,
    mode: Option<String>,
    state: tauri::State<'_, OperationState>,
) -> Result<OperationSnapshot, String> {
    state.start(&kind, mode.as_deref())
}

#[tauri::command]
fn cancel_operation(state: tauri::State<'_, OperationState>) -> Result<(), String> {
    state.request_cancel()
}

// Opens only the assistant's own data directory, where recovery records and logs live.
#[tauri::command]
fn open_data_dir() -> Result<(), String> {
    let path = util::data_dir()?;
    std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    let program = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "explorer.exe"
    };
    std::process::Command::new(program)
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法打开 {}：{error}", path.display()))
}

#[tauri::command]
fn open_claude() -> Result<(), String> {
    claude::launch()
}

#[tauri::command]
fn choose_assistant_install_path() -> Result<Option<String>, String> {
    assistant::choose_install_path()
}

#[cfg(target_os = "windows")]
fn set_windows_taskbar_icon(hwnd: windows_sys::Win32::Foundation::HWND) -> Result<(), String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SendMessageW, ICON_BIG, ICON_SMALL, WM_GETICON, WM_SETICON,
    };
    // Tao only sets ICON_SMALL. Reuse its 256px icon, owned for the window's lifetime.
    unsafe {
        let icon = SendMessageW(hwnd, WM_GETICON, ICON_SMALL as usize, 0);
        if icon == 0 {
            return Err("无法读取助手窗口图标。".into());
        }
        SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, icon);
    }
    Ok(())
}

pub fn run() {
    if util::run_helper_if_requested() {
        return;
    }
    if std::env::args().any(|arg| arg == "--self-test") {
        std::process::exit(if self_update::self_test() { 0 } else { 1 });
    }
    if std::env::args().any(|arg| arg == "--uninstall-assistant") {
        let state = OperationState::new();
        std::process::exit(if assistant::uninstall(&state).is_ok() {
            0
        } else {
            1
        });
    }

    self_update::start_daily_scheduler();
    let mut context = app_context();
    // Tauri decodes only the first ICO frame (16px); do not upscale it for the taskbar.
    context.set_default_window_icon(Some(tauri::include_image!("icons/128x128@2x.png")));
    tauri::Builder::default()
        .setup(|_app| {
            #[cfg(target_os = "windows")]
            set_windows_taskbar_icon(
                _app.get_webview_window("main")
                    .ok_or("助手主窗口不存在。")?
                    .hwnd()?
                    .0,
            )?;
            Ok(())
        })
        .manage(OperationState::new())
        .invoke_handler(tauri::generate_handler![
            get_platform,
            fit_initial_window,
            get_status,
            get_assistant_status,
            get_config,
            save_config,
            get_operation_snapshot,
            start_operation,
            cancel_operation,
            open_data_dir,
            open_claude,
            choose_assistant_install_path,
        ])
        .run(context)
        .expect("failed to run app");
}

#[cfg(test)]
mod window_tests {
    use super::*;

    #[cfg(target_os = "windows")]
    #[test]
    fn taskbar_icon_uses_the_existing_window_icon() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, LoadIconW, SendMessageW, ICON_BIG, ICON_SMALL,
            IDI_APPLICATION, WM_GETICON, WM_SETICON,
        };
        unsafe {
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            assert!(!hwnd.is_null());
            let missing = set_windows_taskbar_icon(hwnd);
            let icon = LoadIconW(std::ptr::null_mut(), IDI_APPLICATION);
            assert!(!icon.is_null());
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icon as isize);
            let before = SendMessageW(hwnd, WM_GETICON, ICON_BIG as usize, 0);
            let result = set_windows_taskbar_icon(hwnd);
            let after = SendMessageW(hwnd, WM_GETICON, ICON_BIG as usize, 0);
            let small = SendMessageW(hwnd, WM_GETICON, ICON_SMALL as usize, 0);
            DestroyWindow(hwnd);
            assert_eq!(before, 0);
            assert!(result.is_ok());
            assert_eq!(after, icon as isize);
            assert_eq!(small, icon as isize);
            assert!(missing.is_err());
        }
    }

    #[test]
    fn initial_size_preserves_css_space_until_the_work_area_is_full() {
        let design = tauri::LogicalSize::new(800.0, 740.0);
        for (scale, work, expected) in [
            (1.0, (1920, 1040), (800, 740)),
            (1.22, (1920, 1040), (976, 903)),
            (1.5, (2560, 1400), (1200, 1110)),
            (1.83, (2560, 1400), (1464, 1354)),
            (2.0, (1280, 680), (1280, 680)),
            (1.22, (1366, 720), (976, 720)),
        ] {
            let actual =
                initial_window_size(design, scale, tauri::PhysicalSize::new(work.0, work.1))
                    .unwrap();
            assert_eq!((actual.width, actual.height), expected);
        }
        for invalid in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                initial_window_size(design, invalid, tauri::PhysicalSize::new(1920, 1040)).is_err()
            );
        }
    }
}
