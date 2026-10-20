#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(target_os = "windows")]
struct SingleInstanceGuard(windows_sys::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

fn main() {
    let _single_instance = acquire_single_instance_or_exit();
    claude_windows_cn_lib::run()
}

#[cfg(target_os = "windows")]
fn acquire_single_instance_or_exit() -> Option<SingleInstanceGuard> {
    if std::env::args().any(|arg| arg == "--run-assistant-helper" || arg == "--self-test") {
        return None;
    }

    let name = "Local\\ClaudeWindowsCN.SingleInstance"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    unsafe {
        let handle = windows_sys::Win32::System::Threading::CreateMutexW(
            std::ptr::null_mut(),
            1,
            name.as_ptr(),
        );
        if handle.is_null() {
            return None;
        }
        if windows_sys::Win32::Foundation::GetLastError()
            == windows_sys::Win32::Foundation::ERROR_ALREADY_EXISTS
        {
            windows_sys::Win32::Foundation::CloseHandle(handle);
            std::process::exit(0);
        }
        Some(SingleInstanceGuard(handle))
    }
}

#[cfg(not(target_os = "windows"))]
fn acquire_single_instance_or_exit() {}
