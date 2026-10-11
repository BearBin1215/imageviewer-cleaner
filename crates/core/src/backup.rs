//! ② 备份层 —— 隔离区与注册表导出
//!
//! 所有改动先落到隔离区，不直接删除。隔离区**必须与安装目录同卷**：
//! 跨卷移动会退化成复制加删除，既不原子，也需要目标卷有等量空余空间。
//!
//! 还原所需的全部信息记录在 `manifest.json` 里，还原时按它定位，不再推算。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{Action, Manifest, RegItem};
use crate::sys::SysContext;

/// 隔离区根目录名（放在安装目录所在卷下）
pub const QUARANTINE_DIR_NAME: &str = "imageviewer-cleaner";

/// 一次清理操作对应的隔离区
pub struct Quarantine {
    /// 本次操作的目录，如 `D:\imageviewer-cleaner\quarantine\20261011-132600`
    pub dir: PathBuf,
    pub install_dir: PathBuf,
}

/// 默认隔离区根目录：安装目录的**同级目录**下。
///
/// 例如安装在 `D:\Program Files\BaiduNetdisk`，隔离区就在
/// `D:\Program Files\imageviewer-cleaner\quarantine`。
/// 必须与安装目录同卷（同卷移动才是原子改名），放在同级目录
/// 既满足同卷要求，也不会在磁盘根目录下另起一个显眼的文件夹。
pub fn default_root(install_dir: &Path) -> Result<PathBuf> {
    let parent = install_dir.parent().ok_or_else(|| {
        Error::io(install_dir, std::io::Error::other("安装目录没有上级目录"))
    })?;
    Ok(parent
        .join(QUARANTINE_DIR_NAME)
        .join("quarantine"))
}

/// 删除一份备份（用户确认网盘一切正常后释放空间用）
pub fn delete_backup(dir: &Path) -> Result<()> {
    if !dir.join("manifest.json").exists() {
        return Err(Error::io(
            dir,
            std::io::Error::other("该目录不是有效的备份（缺少 manifest.json）"),
        ));
    }
    std::fs::remove_dir_all(dir).map_err(|e| Error::io(dir, e))
}

impl Quarantine {
    /// 在给定根目录下建一个带时间戳的隔离区（不落盘，首次写入时才创建目录）
    pub fn new(root: &Path, install_dir: &Path, stamp: &str) -> Self {
        Self {
            dir: root.join(stamp),
            install_dir: install_dir.to_path_buf(),
        }
    }

    pub fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    /// 隔离区根目录加隐藏属性，避免用户误删或感觉突兀
    pub fn mark_hidden(&self, sys: &SysContext) {
        if let Some(root) = self.dir.ancestors().nth(2) {
            let _ = sys.fs.set_hidden(root);
        }
    }

    pub fn registry_dir(&self) -> PathBuf {
        self.dir.join("registry")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.dir.join("manifest.json")
    }

    /// 把安装目录下的某个相对路径移入隔离区，保持原有的目录结构。
    /// 返回 (动作记录, 移动的字节数)。
    pub fn move_into(&self, sys: &SysContext, relative: &str) -> Result<(Action, u64)> {
        let from = self.install_dir.join(relative);
        let to = self.files_dir().join(relative);
        let outcome = sys.fs.move_path(&from, &to)?;
        Ok((
            Action::FileMoved {
                from,
                to: outcome.to,
                cross_volume: outcome.cross_volume,
            },
            outcome.bytes,
        ))
    }

    /// 把某个注册表键导出为 `.reg` 文本
    ///
    /// 导出整个键（而不是单个值），这样用户手工恢复时能看到完整上下文。
    pub fn export(&self, sys: &SysContext, item: &RegItem) -> Result<PathBuf> {
        let out = self.registry_dir().join(reg_file_name(item));
        if let Some(parent) = out.parent() {
            sys.fs.ensure_dir(parent)?;
        }
        sys.registry.export_key(&item.path, &out)?;
        Ok(out)
    }

    pub fn write_manifest(&self, sys: &SysContext, m: &Manifest) -> Result<()> {
        sys.fs.ensure_dir(&self.dir)?;
        let json = serde_json::to_string_pretty(m).map_err(|e| Error::data("manifest.json", e))?;
        atomic_write(&self.manifest_path(), json.as_bytes())
    }

    pub fn read_manifest(&self) -> Result<Manifest> {
        let path = self.manifest_path();
        let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_str(&text).map_err(|e| Error::data("manifest.json", e))
    }
}

// ───────────────────────────── 写前日志（Journal） ─────────────────────────────

/// 操作日志：每记录一个动作就**原子落盘** `manifest.json`。
///
/// 这保证即使清理中途进程被杀/断电，隔离区里也始终有一份与「已执行动作」一致的
/// manifest，使这些改动可被还原——而不是变成无法识别、无法还原的孤儿文件。
pub struct Journal {
    path: Option<PathBuf>,
    manifest: Manifest,
    /// 已移入隔离区的总字节数（仅用于汇总展示）
    pub bytes_reclaimed: u64,
    /// 因占用/权限等原因被跳过、未能处理的项数（非致命）
    pub failures: usize,
}

impl Journal {
    /// 建立日志并在**任何破坏性动作之前**写入基线 manifest。
    pub fn start(sys: &SysContext, q: &Quarantine, manifest: Manifest) -> Result<Self> {
        sys.fs.ensure_dir(&q.dir)?;
        let j = Self {
            path: Some(q.manifest_path()),
            manifest,
            bytes_reclaimed: 0,
            failures: 0,
        };
        j.persist()?;
        Ok(j)
    }

    /// 不落盘的内存日志（单元测试用）。
    pub fn memory(manifest: Manifest) -> Self {
        Self {
            path: None,
            manifest,
            bytes_reclaimed: 0,
            failures: 0,
        }
    }

    /// 记录一个已完成的动作并立即落盘。
    pub fn record(&mut self, action: Action) -> Result<()> {
        self.manifest.actions.push(action);
        self.persist()
    }

    pub fn actions(&self) -> &[Action] {
        &self.manifest.actions
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// 标记完成时间并落盘，交回最终 manifest。
    pub fn finish(mut self) -> Result<Manifest> {
        self.manifest.finished_at = Some(local_timestamp());
        self.persist()?;
        Ok(self.manifest)
    }

    fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let json = serde_json::to_string_pretty(&self.manifest)
            .map_err(|e| Error::data("manifest.json", e))?;
        atomic_write(path, json.as_bytes())
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 原子写：先写同目录临时文件，再用 `MoveFileExW(REPLACE_EXISTING)` 覆盖。
/// 避免「写到一半崩溃」导致 manifest 被截断、无法解析。
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, data).map_err(|e| Error::io(&tmp, e))?;
    let from = wide(&tmp.to_string_lossy());
    let to = wide(&path.to_string_lossy());
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| Error::win32(format!("MoveFileExW({})", path.display()), e))
}

/// 把注册表路径转成安全的文件名
fn reg_file_name(item: &RegItem) -> String {
    let mut s = item.path.full().replace('\\', "_").replace(':', "");
    if let Some(v) = &item.value {
        s.push_str("__");
        s.push_str(&v.replace('\\', "_"));
    }
    // 扩展名维度的路径很长，截断以免超过文件名长度限制
    if s.len() > 140 {
        s.truncate(140);
    }
    format!("{s}.reg")
}

/// 列出某个根目录下所有可用的备份（即含 `manifest.json` 的子目录），最近的排在前面
pub fn list_backups(root: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("manifest.json").is_file())
        .collect();
    v.sort();
    v.reverse();
    v
}

/// 本地时间戳，形如 `20261011-132600`
pub fn local_timestamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{RegPath, RegRoot};

    fn item(path: &str, value: Option<&str>) -> RegItem {
        RegItem {
            path: RegPath {
                root: RegRoot::Hku,
                sub: path.to_owned(),
            },
            value: value.map(str::to_owned),
        }
    }

    #[test]
    fn reg_file_name_is_filesystem_safe() {
        let n = reg_file_name(&item(
            r"S-1-5-21-1-2-3-1001_Classes\Applications\BaiduNetdisk.open",
            None,
        ));
        assert!(!n.contains('\\'), "文件名不得含反斜杠: {n}");
        assert!(!n.contains(':'), "文件名不得含冒号: {n}");
        assert!(n.ends_with(".reg"));

        let v = reg_file_name(&item(
            r"S-1-5-21-1-2-3-1001\Software\RegisteredApplications",
            Some("BaiduNetdiskPlayer"),
        ));
        assert!(v.contains("BaiduNetdiskPlayer"));
        assert!(v.ends_with(".reg"));
    }

    #[test]
    fn reg_file_name_is_truncated_when_too_long() {
        let long = "A".repeat(400);
        let n = reg_file_name(&item(&long, None));
        assert!(n.len() <= 144, "应截断，实际长度 {}", n.len());
    }

    #[test]
    fn timestamp_has_expected_shape() {
        let t = local_timestamp();
        assert_eq!(t.len(), 15, "形如 20261011-132600，实际 {t}");
        assert_eq!(&t[8..9], "-");
        assert!(t[..8].chars().all(|c| c.is_ascii_digit()));
        assert!(t[9..].chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn journal_persists_each_action_before_finish() {
        // 验证写前日志：动作在 finish 之前就已落盘，使中途崩溃也能还原。
        let base = std::env::temp_dir().join("bc_journal_wal");
        let _ = std::fs::remove_dir_all(&base);
        let install = base.join("install");
        std::fs::create_dir_all(&install).unwrap();
        let q = Quarantine::new(&base.join("quarantine"), &install, "t");
        let sys = SysContext::win32();

        let m = Manifest {
            version: "t".into(),
            started_at: "t".into(),
            finished_at: None,
            install_dir: install.clone(),
            quarantine_dir: q.dir.clone(),
            target_user_sid: "S".into(),
            actions: Vec::new(),
        };
        let mut j = Journal::start(&sys, &q, m).unwrap();
        j.record(Action::PlaceholderCreated {
            path: install.join("x"),
            kind: crate::model::PlaceholderKind::DirInsteadOfFile,
        })
        .unwrap();

        // 未调用 finish（模拟中途崩溃）：磁盘上的 manifest 必须已记录该动作
        let on_disk = q.read_manifest().unwrap();
        assert_eq!(on_disk.actions.len(), 1, "动作应在 finish 之前就落盘");
        assert!(on_disk.finished_at.is_none(), "尚未 finish");

        let _ = std::fs::remove_dir_all(&base);
    }
}
