//! 独立的会话/电源/显示通知窗口。
//!
//! 托盘窗口可能因为完整性级别/Explorer 状态建不出来；但“锁屏/挂起/分辨率变化时停录”
//! 不能跟着一起失效。这里单独建一个隐藏顶层窗口，注册 WTS 并接收
//! `WM_POWERBROADCAST` / `WM_DISPLAYCHANGE`，与托盘图标完全解耦。

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};

const WM_WTSSESSION_CHANGE_MSG: u32 = 0x02B1;
const WM_POWERBROADCAST_MSG: u32 = 0x0218;
const WM_DISPLAYCHANGE_MSG: u32 = 0x007E;
const WTS_SESSION_LOCK: usize = 0x7;
const WTS_SESSION_UNLOCK: usize = 0x8;
const PBT_APMSUSPEND: usize = 0x4;
const PBT_APMRESUMEAUTOMATIC: usize = 0x12;

/// 隐藏的会话通知窗口。`Drop` 注销并销毁。
pub struct SessionWatcher {
    hwnd: HWND,
    registered: bool,
}

unsafe extern "system" fn session_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_WTSSESSION_CHANGE_MSG => {
            if wparam.0 == WTS_SESSION_LOCK {
                crate::tray::push_command(crate::tray::TrayCommand::SystemStop(
                    "Workstation locked; recording stopped".into(),
                ));
            } else if wparam.0 == WTS_SESSION_UNLOCK {
                tracing::info!("会话已解锁（录制未自动恢复）");
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST_MSG => {
            if wparam.0 == PBT_APMSUSPEND {
                crate::tray::push_command(crate::tray::TrayCommand::SystemStop(
                    "System is suspending; recording stopped".into(),
                ));
            } else if wparam.0 == PBT_APMRESUMEAUTOMATIC {
                tracing::info!("系统已从挂起恢复（录制未自动恢复）");
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE_MSG => {
            crate::tray::push_command(crate::tray::TrayCommand::SystemStop(
                "Display configuration changed; recording stopped".into(),
            ));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

impl SessionWatcher {
    /// 在主线程创建隐藏顶层窗口；即使托盘安装失败也应调用。
    pub fn install(ctx: &eframe::egui::Context) -> Result<Self, String> {
        crate::tray::set_repaint_context(ctx);
        let class = w!("CapLiteSessionWatcher");
        let hinstance =
            unsafe { GetModuleHandleW(None) }.map_err(|e| format!("GetModuleHandleW: {e}"))?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(session_wndproc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: class,
            ..Default::default()
        };
        unsafe {
            RegisterClassW(&wc);
        }
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class,
                w!("CapLiteSessionWatcher"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                // 顶层窗口：`WM_POWERBROADCAST` / `WM_DISPLAYCHANGE` 才会广播到它。
                None,
                None,
                Some(HINSTANCE(hinstance.0)),
                None,
            )
        }
        .map_err(|e| format!("CreateWindowExW(SessionWatcher): {e}"))?;
        let registered = unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) }
            .is_ok();
        if !registered {
            tracing::warn!("WTSRegisterSessionNotification 失败：锁屏时不会自动停录");
        }
        Ok(Self { hwnd, registered })
    }
}

impl Drop for SessionWatcher {
    fn drop(&mut self) {
        if self.registered && !self.hwnd.0.is_null() {
            let _ = unsafe { WTSUnRegisterSessionNotification(self.hwnd) };
        }
        if !self.hwnd.0.is_null() {
            let _ = unsafe { DestroyWindow(self.hwnd) };
            self.hwnd = HWND(std::ptr::null_mut());
        }
    }
}
