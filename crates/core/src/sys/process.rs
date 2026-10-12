//! 进程后端 —— trait 抽象 + Win32 实现
//!
//! 目标进程的几点事实：
//! - `BaiduNetdiskUnite.exe` 可能同时存在多个实例，因此按名全量枚举。
//! - `YunDetectService.exe` 等没有顶层窗口，收不到 `WM_CLOSE`，需要容许直接终止。
//! - 这些进程都不是 SCM 服务，服务分支为其他版本预留。

use std::path::PathBuf;

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Services::{
    CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW, SC_MANAGER_CONNECT,
    SERVICE_CONTROL_STOP, SERVICE_QUERY_STATUS, SERVICE_STATUS,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};
use windows::core::{BOOL, PCWSTR};

use crate::error::{Error, Result};
use crate::sys::wide;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub name: String,
    pub pid: u32,
}

pub trait ProcessBackend: Send + Sync {
    fn list(&self) -> Result<Vec<ProcessInfo>>;
    fn image_path(&self, pid: u32) -> Result<Option<PathBuf>>;
    /// 请求优雅关闭。
    /// 返回 `Ok(false)` 表示该进程**没有可投递的顶层窗口**（如后台服务进程），
    /// 调用方应直接进入终止流程，这不算失败。
    fn request_close(&self, pid: u32) -> Result<bool>;
    fn terminate(&self, pid: u32) -> Result<()>;
    fn is_service(&self, name: &str) -> Result<bool>;
    fn stop_service(&self, name: &str) -> Result<()>;
    fn disable_service(&self, name: &str) -> Result<()>;
    /// 进程所在会话 ID（用于定位活动控制台会话的用户）
    fn session_id(&self, pid: u32) -> Result<Option<u32>>;
    /// 进程令牌的用户 SID（用于定位活动控制台会话的用户）
    fn token_user_sid(&self, pid: u32) -> Result<Option<String>>;
}

/// 活动控制台会话 ID（无控制台会话时返回 None）
pub fn active_console_session_id() -> Option<u32> {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;
    let s = unsafe { WTSGetActiveConsoleSessionId() };
    if s == u32::MAX { None } else { Some(s) }
}

// ───────────────────────────────── Win32 实现 ─────────────────────────────────

pub struct Win32Process;

impl Win32Process {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Win32Process {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessBackend for Win32Process {
    fn list(&self) -> Result<Vec<ProcessInfo>> {
        let mut out = Vec::new();
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
            .map_err(|e| Error::win32("CreateToolhelp32Snapshot", e))?;

        let mut e: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        if unsafe { Process32FirstW(snap, &mut e) }.is_ok() {
            loop {
                let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                out.push(ProcessInfo {
                    name: String::from_utf16_lossy(&e.szExeFile[..end]),
                    pid: e.th32ProcessID,
                });
                if unsafe { Process32NextW(snap, &mut e) }.is_err() {
                    break;
                }
            }
        }
        let _ = unsafe { CloseHandle(snap) };
        Ok(out)
    }

    fn image_path(&self, pid: u32) -> Result<Option<PathBuf>> {
        let h = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = unsafe {
            QueryFullProcessImageNameW(
                h,
                PROCESS_NAME_FORMAT(0),
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        };
        let _ = unsafe { CloseHandle(h) };
        if r.is_err() {
            return Ok(None);
        }
        Ok(Some(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        ))))
    }

    fn request_close(&self, pid: u32) -> Result<bool> {
        /// 回调上下文。用 `LPARAM` 传递，避免使用全局可变状态，
        /// 这样函数可重入、可并发调用。
        struct Ctx {
            pid: u32,
            found: bool,
        }

        unsafe extern "system" fn cb(hwnd: HWND, l: LPARAM) -> BOOL {
            let ctx = unsafe { &mut *(l.0 as *mut Ctx) };
            let mut owner = 0u32;
            let _ = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
            if owner == ctx.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() {
                let _ = unsafe { PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0)) };
                ctx.found = true;
            }
            BOOL(1) // 继续枚举
        }

        let mut ctx = Ctx { pid, found: false };
        let _ = unsafe { EnumWindows(Some(cb), LPARAM(&mut ctx as *mut Ctx as isize)) };
        Ok(ctx.found)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        let h: HANDLE = unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) }
            .map_err(|e| Error::win32(format!("OpenProcess({pid})"), e))?;
        let r = unsafe { TerminateProcess(h, 1) };
        let _ = unsafe { CloseHandle(h) };
        r.map_err(|e| Error::win32(format!("TerminateProcess({pid})"), e))
    }

    fn is_service(&self, name: &str) -> Result<bool> {
        let scm =
            match unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) } {
                Ok(s) => s,
                Err(_) => return Ok(false),
            };
        let n = wide(name);
        let svc = unsafe { OpenServiceW(scm, PCWSTR(n.as_ptr()), SERVICE_QUERY_STATUS) };
        let _ = unsafe { CloseServiceHandle(scm) };
        match svc {
            Ok(svc) => {
                let _ = unsafe { CloseServiceHandle(svc) };
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    fn stop_service(&self, name: &str) -> Result<()> {
        let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
            .map_err(|e| Error::win32("OpenSCManagerW", e))?;
        let n = wide(name);
        let svc = unsafe {
            OpenServiceW(
                scm,
                PCWSTR(n.as_ptr()),
                windows::Win32::System::Services::SERVICE_STOP,
            )
        }
        .map_err(|e| Error::win32(format!("OpenServiceW({name})"), e))?;
        let mut st: SERVICE_STATUS = unsafe { std::mem::zeroed() };
        let r = unsafe { ControlService(svc, SERVICE_CONTROL_STOP, &mut st) };
        let _ = unsafe { CloseServiceHandle(svc) };
        let _ = unsafe { CloseServiceHandle(scm) };
        r.map_err(|e| Error::win32(format!("ControlService(STOP, {name})"), e))
    }

    fn disable_service(&self, name: &str) -> Result<()> {
        use windows::Win32::System::Services::{
            ChangeServiceConfigW, ENUM_SERVICE_TYPE, SERVICE_DISABLED, SERVICE_ERROR,
            SERVICE_NO_CHANGE,
        };
        let scm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT) }
            .map_err(|e| Error::win32("OpenSCManagerW", e))?;
        let n = wide(name);
        let svc = unsafe {
            OpenServiceW(
                scm,
                PCWSTR(n.as_ptr()),
                windows::Win32::System::Services::SERVICE_CHANGE_CONFIG,
            )
        }
        .map_err(|e| Error::win32(format!("OpenServiceW({name})"), e))?;
        let r = unsafe {
            ChangeServiceConfigW(
                svc,
                ENUM_SERVICE_TYPE(SERVICE_NO_CHANGE),
                SERVICE_DISABLED,
                SERVICE_ERROR(SERVICE_NO_CHANGE),
                PCWSTR::null(),
                PCWSTR::null(),
                None,
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
                PCWSTR::null(),
            )
        };
        let _ = unsafe { CloseServiceHandle(svc) };
        let _ = unsafe { CloseServiceHandle(scm) };
        r.map_err(|e| Error::win32(format!("ChangeServiceConfig(DISABLED, {name})"), e))
    }

    fn session_id(&self, pid: u32) -> Result<Option<u32>> {
        let mut s = 0u32;
        match unsafe { ProcessIdToSessionId(pid, &mut s) } {
            Ok(()) => Ok(Some(s)),
            Err(_) => Ok(None),
        }
    }

    fn token_user_sid(&self, pid: u32) -> Result<Option<String>> {
        use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
        use windows::Win32::System::Threading::OpenProcessToken;

        let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
            return Ok(None);
        };
        let mut tok = HANDLE::default();
        let opened = unsafe { OpenProcessToken(h, TOKEN_QUERY, &mut tok) };
        let _ = unsafe { CloseHandle(h) };
        if opened.is_err() {
            return Ok(None);
        }

        let mut len = 0u32;
        let _ = unsafe { GetTokenInformation(tok, TokenUser, None, 0, &mut len) };
        if len == 0 {
            let _ = unsafe { CloseHandle(tok) };
            return Ok(None);
        }
        let mut buf = vec![0u8; len as usize];
        let r = unsafe {
            GetTokenInformation(
                tok,
                TokenUser,
                Some(buf.as_mut_ptr() as *mut _),
                len,
                &mut len,
            )
        };
        let _ = unsafe { CloseHandle(tok) };
        if r.is_err() {
            return Ok(None);
        }
        let tu = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        Ok(sid_to_string(tu.User.Sid))
    }
}

/// 把 SID 结构格式化为 `S-1-5-21-...` 字符串
fn sid_to_string(sid: windows::Win32::Security::PSID) -> Option<String> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows::core::PWSTR;

    let mut raw = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut raw) }.ok()?;
    let text = unsafe { raw.to_string() }.ok();
    // ConvertSidToStringSidW 用 LocalAlloc 分配，必须由调用方释放
    unsafe { LocalFree(Some(HLOCAL(raw.0 as *mut _))) };
    text
}
