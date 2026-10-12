//! 测试替身 —— 使「进程反查 → 注册表 → 候选路径」三路探测都能在
//! 集成测试里被驱动，**不碰真实系统**

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::Result;
use crate::model::RegPath;
use crate::sys::process::{ProcessBackend, ProcessInfo};
use crate::sys::registry::RegistryBackend;
use crate::sys::shell::ShellBackend;

// ───────────────────────────── 注册表替身 ─────────────────────────────

/// 内存注册表：键 → (值名 → 值)
#[derive(Default)]
pub struct InMemoryRegistry {
    keys: Mutex<BTreeMap<RegPath, BTreeMap<String, String>>>,
    user_hives: Mutex<Vec<String>>,
}

impl InMemoryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 建立键（子键按路径自动视为存在）
    pub fn add_key(&self, path: RegPath) {
        self.keys.lock().unwrap().entry(path).or_default();
    }

    pub fn set_value(&self, path: &RegPath, name: &str, value: &str) {
        self.keys
            .lock()
            .unwrap()
            .entry(path.clone())
            .or_default()
            .insert(name.to_owned(), value.to_owned());
    }

    pub fn set_user_hives(&self, hives: Vec<String>) {
        *self.user_hives.lock().unwrap() = hives;
    }

    pub fn has_key(&self, path: &RegPath) -> bool {
        let keys = self.keys.lock().unwrap();
        keys.contains_key(path)
            || keys
                .keys()
                .any(|k| k.sub.starts_with(&format!("{}\\", path.sub)))
    }

    /// 用于断言：返回当前所有键路径（排序后）
    pub fn snapshot_keys(&self) -> Vec<String> {
        self.keys.lock().unwrap().keys().map(|k| k.full()).collect()
    }
}

impl RegistryBackend for InMemoryRegistry {
    fn enumerate_user_hives(&self) -> Result<Vec<String>> {
        Ok(self.user_hives.lock().unwrap().clone())
    }

    fn read_string(&self, path: &RegPath, value: &str) -> Result<Option<String>> {
        Ok(self
            .keys
            .lock()
            .unwrap()
            .get(path)
            .and_then(|v| v.get(value))
            .cloned())
    }

    fn key_exists(&self, path: &RegPath) -> Result<bool> {
        Ok(self.has_key(path))
    }

    fn delete_key(&self, path: &RegPath) -> Result<()> {
        let prefix = format!("{}\\", path.sub);
        self.keys.lock().unwrap().retain(|k, _| {
            k.root != path.root || (k.sub != path.sub && !k.sub.starts_with(&prefix))
        });
        Ok(())
    }

    fn delete_value(&self, path: &RegPath, value: &str) -> Result<()> {
        if let Some(v) = self.keys.lock().unwrap().get_mut(path) {
            v.remove(value);
        }
        Ok(())
    }

    fn export_key(&self, _path: &RegPath, _out: &std::path::Path) -> Result<()> {
        Ok(())
    }

    fn create_key(&self, path: &RegPath) -> Result<()> {
        self.add_key(path.clone());
        Ok(())
    }

    fn set_value(
        &self,
        path: &RegPath,
        name: Option<&str>,
        value: &crate::sys::regfile::RegValue,
    ) -> Result<()> {
        let text = match value {
            crate::sys::regfile::RegValue::Str(s) => s.clone(),
            crate::sys::regfile::RegValue::Dword(v) => v.to_string(),
            crate::sys::regfile::RegValue::Binary(b) => format!("{b:?}"),
        };
        InMemoryRegistry::set_value(self, path, name.unwrap_or(""), &text);
        Ok(())
    }

    fn import_reg_file(&self, _path: &std::path::Path) -> Result<()> {
        // 替身不做真实导入；调用方只需知道「调用被接受了」
        Ok(())
    }
}

// ───────────────────────────── 进程替身 ─────────────────────────────

#[derive(Default)]
pub struct FakeProcesses {
    procs: Mutex<Vec<ProcessInfo>>,
    images: Mutex<HashMap<u32, PathBuf>>,
    services: Mutex<Vec<String>>,
    /// 记录 request_close 被调用的 pid
    pub close_requests: Mutex<Vec<u32>>,
    /// 记录 terminate 被调用的 pid
    pub terminated: Mutex<Vec<u32>>,
}

impl FakeProcesses {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, name: &str, pid: u32, image: Option<PathBuf>) {
        self.procs.lock().unwrap().push(ProcessInfo {
            name: name.to_owned(),
            pid,
        });
        if let Some(p) = image {
            self.images.lock().unwrap().insert(pid, p);
        }
    }

    pub fn add_service(&self, name: &str) {
        self.services.lock().unwrap().push(name.to_owned());
    }
}

impl ProcessBackend for FakeProcesses {
    fn list(&self) -> Result<Vec<ProcessInfo>> {
        Ok(self.procs.lock().unwrap().clone())
    }

    fn image_path(&self, pid: u32) -> Result<Option<PathBuf>> {
        Ok(self.images.lock().unwrap().get(&pid).cloned())
    }

    fn request_close(&self, pid: u32) -> Result<bool> {
        self.close_requests.lock().unwrap().push(pid);
        // 后台无窗口进程（服务类）返回 false
        Ok(false)
    }

    fn terminate(&self, pid: u32) -> Result<()> {
        self.terminated.lock().unwrap().push(pid);
        Ok(())
    }

    fn is_service(&self, name: &str) -> Result<bool> {
        Ok(self.services.lock().unwrap().iter().any(|s| s == name))
    }

    fn stop_service(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn disable_service(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn session_id(&self, _pid: u32) -> Result<Option<u32>> {
        Ok(None)
    }

    fn token_user_sid(&self, _pid: u32) -> Result<Option<String>> {
        Ok(None)
    }
}

// ───────────────────────────── Shell 替身 ─────────────────────────────

#[derive(Default)]
pub struct NoopShell {
    pub calls: Mutex<usize>,
}

impl ShellBackend for NoopShell {
    fn notify_assoc_changed(&self) {
        *self.calls.lock().unwrap() += 1;
    }
}
