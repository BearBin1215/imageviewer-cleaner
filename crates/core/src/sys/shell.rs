//! Shell 后端 —— 清理后通知系统刷新文件关联缓存

use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

pub trait ShellBackend: Send + Sync {
    /// `SHChangeNotify(SHCNE_ASSOCCHANGED)` —— 让关联改动立即生效
    fn notify_assoc_changed(&self);
}

pub struct Win32Shell;

impl Win32Shell {
    pub fn new() -> Self {
        Self
    }
}

impl Default for Win32Shell {
    fn default() -> Self {
        Self::new()
    }
}

impl ShellBackend for Win32Shell {
    fn notify_assoc_changed(&self) {
        unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
    }
}
