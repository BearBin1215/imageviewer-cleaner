//! ② 备份层 —— 隔离区与注册表导出
//!
//! 所有改动先移入隔离区，不直接删除。隔离区与安装目录必须同卷：
//! 跨卷移动会退化为复制加删除，既不原子，也要求目标卷有等量空余空间。
//!
//! 还原所需的全部信息记录在 `manifest.json` 里，还原时按它定位，不再推算。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{Action, Manifest, RegItem};
use crate::progress::Reporter;
use crate::sys::SysContext;
use crate::sys::wide;

/// 隔离区根目录名（放在安装目录所在卷下）
pub const QUARANTINE_DIR_NAME: &str = "imageviewer-cleaner";

/// `manifest.json` 的体积上限。一次清理的动作条数在几百量级，
/// 正常文件只有几十 KB；超过这个数说明文件异常。
const MAX_MANIFEST_BYTES: u64 = 8 * 1024 * 1024;

/// 一次清理操作对应的隔离区
pub struct Quarantine {
    /// 隔离区根目录，隐藏属性加在它上面
    pub root: PathBuf,
    /// 本次操作的目录，如 `D:\imageviewer-cleaner\quarantine\20261011-132600`
    pub dir: PathBuf,
    /// 安装目录。只有 [`Quarantine::new`] 构造的实例（清理时）会用到，
    /// 见 [`Quarantine::move_into`]。
    pub install_dir: PathBuf,
    /// 建立时是否把权限收紧为「仅管理员可访问」。
    ///
    /// 生产路径必须开启。测试置 false：收紧之后非提权的测试进程连自己写进去的
    /// manifest 都读不到，无法驱动还原流程。与 `CleanupConfig::quarantine_root`
    /// 一样，这是给测试留的口子。
    restrict_acl: bool,
}

/// 默认隔离区根目录：安装目录所在卷的根目录下。
///
/// 例如安装在 `D:\Program Files\BaiduNetdisk`，隔离区就在
/// `D:\imageviewer-cleaner\quarantine`。本工具是免安装的绿色软件，不应该在
/// `Program Files` 里留下一个看似安装目录的文件夹，因此放在安装卷根目录、
/// 整层加隐藏属性：既满足同卷要求（同卷移动才是原子改名），也与工具自身
/// 被放在哪里、会不会被删无关。
pub fn default_root(install_dir: &Path) -> Result<PathBuf> {
    let volume = install_dir
        .ancestors()
        .last()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| Error::io(install_dir, std::io::Error::other("无法确定安装目录所在卷")))?;
    Ok(volume.join(QUARANTINE_DIR_NAME).join("quarantine"))
}

/// 旧版隔离区位置：安装目录的同级目录。
///
/// 0.2.x 把备份放在这里，升级后旧备份仍要可还原、可删除，所以探测时会把
/// 这个位置一并列出。安装在卷根目录时两个位置重合，返回 `None`。
pub fn legacy_root(install_dir: &Path) -> Option<PathBuf> {
    let old = install_dir
        .parent()?
        .join(QUARANTINE_DIR_NAME)
        .join("quarantine");
    match default_root(install_dir) {
        Ok(new) if new == old => None,
        _ => Some(old),
    }
}

/// 「删除失败后重试」前的等待时长。资源管理器开着备份目录、杀毒软件正在
/// 扫描等瞬时占用，等一小会儿通常就消失了。
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// 删除一份备份（用户确认网盘一切正常后释放空间用）。
///
/// 资源管理器打开着备份目录、杀毒软件正在扫描时，Windows 会报
/// 「另一个程序正在使用此文件」；这类占用是瞬时的，稍候重试一次通常即可成功。
/// 删除成功后，若祖先目录里只剩本工具创建的空壳（`quarantine`、
/// `imageviewer-cleaner` 两层），会一并移除，不在磁盘上留空文件夹。
pub fn delete_backup(dir: &Path) -> Result<()> {
    if !dir.join("manifest.json").exists() {
        return Err(Error::io(
            dir,
            std::io::Error::other("该目录不是有效的备份（缺少 manifest.json）"),
        ));
    }
    if let Err(first) = std::fs::remove_dir_all(dir) {
        std::thread::sleep(RETRY_DELAY);
        if let Err(second) = std::fs::remove_dir_all(dir) {
            // 首次失败可能已删掉大半，重试报错但目录实际已不存在时按成功算
            if dir.exists() {
                return Err(Error::io(
                    dir,
                    std::io::Error::other(format!("{first}；重试仍失败：{second}")),
                ));
            }
        }
    }
    prune_empty_parents(dir);
    Ok(())
}

/// 删除备份后把变空的祖先目录一并移除。只处理本工具创建的那两层
/// （`quarantine` 与 `imageviewer-cleaner`），遇到非空或删不掉的目录立即停止。
fn prune_empty_parents(dir: &Path) {
    for anc in dir.ancestors().skip(1) {
        let ours = anc
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == QUARANTINE_DIR_NAME || n == "quarantine");
        if !ours || std::fs::remove_dir(anc).is_err() {
            break;
        }
    }
}

impl Quarantine {
    /// 在给定根目录下建一个带时间戳的隔离区（不落盘，首次写入时才创建目录）
    pub fn new(root: &Path, install_dir: &Path, stamp: &str) -> Self {
        Self {
            root: root.to_path_buf(),
            dir: root.join(stamp),
            install_dir: install_dir.to_path_buf(),
            restrict_acl: true,
        }
    }

    /// 从已存在的隔离区目录构造（还原、列出备份时用），根目录由该目录反推。
    ///
    /// 这一路径只读备份，因此不记录安装目录。
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        let root = dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dir.clone());
        Self {
            root,
            dir,
            install_dir: PathBuf::new(),
            restrict_acl: true,
        }
    }

    /// 设置是否收紧权限。生产路径保持默认的 `true`，测试传 `false`。
    pub fn with_acl_restriction(mut self, on: bool) -> Self {
        self.restrict_acl = on;
        self
    }

    /// 关闭权限收紧（仅测试使用）
    pub fn without_acl_restriction(self) -> Self {
        self.with_acl_restriction(false)
    }

    /// 需要收紧权限的目录链，从最外层到本次操作的目录。
    ///
    /// 逐层收紧：只收紧最内层没有意义，外层可写就能删掉内层目录再重建一个。
    /// 命名不符（自定义根目录）时只收紧实际用到的两层。
    fn levels(&self) -> Vec<PathBuf> {
        let mut v = vec![self.root.clone(), self.dir.clone()];
        let named_ours = self
            .root
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some(QUARANTINE_DIR_NAME);
        if named_ours {
            if let Some(top) = self.root.parent() {
                v.insert(0, top.to_path_buf());
            }
        }
        v
    }

    /// 建立隔离区目录并立即收紧权限。**必须在任何写入之前调用。**
    ///
    /// 卷根的默认 ACL 会把 `Authenticated Users:(OI)(CI)(IO)(M)` 继承给新建的
    /// 每一层目录，任何普通用户都能读写隔离区里的 manifest 与注册表导出，而
    /// manifest 是还原阶段的输入，被改动就会让还原内容不可控。
    ///
    /// 同时拒绝使用重解析点路径：权限收紧作用在联接自身，写入仍会到达联接
    /// 指向的位置，收紧同样没有意义。
    ///
    /// 权限收紧失败只告警、不中止：FAT32 / exFAT 等文件系统根本不支持 ACL，
    /// 硬失败会让这些卷上的清理完全不可用。此时还原侧仍受 manifest 路径闸门
    /// 与注册表作用域约束，只是少了一层纵深防御。
    pub fn prepare(&self, sys: &SysContext, rep: &Reporter) -> Result<()> {
        for level in self.levels() {
            if sys.fs.exists(&level) {
                if sys.fs.is_reparse_point(&level) {
                    return Err(Error::io(
                        &level,
                        std::io::Error::other("隔离区路径是重解析点（联接或符号链接），拒绝使用"),
                    ));
                }
                if !sys.fs.is_dir(&level) {
                    return Err(Error::io(
                        &level,
                        std::io::Error::other("隔离区路径不是目录"),
                    ));
                }
            }
            sys.fs.ensure_dir(&level)?;
            if self.restrict_acl {
                if let Err(e) = sys.fs.restrict_to_admins(&level) {
                    rep.warn(format!(
                        "无法收紧隔离区权限（{}）：{e}；\
                         本机其他用户可能改写备份文件",
                        level.display()
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn files_dir(&self) -> PathBuf {
        self.dir.join("files")
    }

    /// 给隔离区所在的两层目录加隐藏属性，避免用户误删。
    ///
    /// 只藏内层会留下一个看似空置的 `imageviewer-cleaner` 文件夹，反而让人起疑。
    pub fn mark_hidden(&self, sys: &SysContext) {
        let _ = sys.fs.set_hidden(&self.root);
        if let Some(top) = self.root.parent() {
            let ours = top
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == QUARANTINE_DIR_NAME);
            if ours {
                let _ = sys.fs.set_hidden(top);
            }
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
    ///
    /// 只对 [`Self::new`] 构造的实例有效：`at` 构造的实例没有安装目录。
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
    /// 导出整个键（含子键与全部值），用户手工恢复时能看到完整上下文。
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
        // manifest 是外部输入：先检查大小再看内容，避免把超大文件整体读入内存
        let len = std::fs::metadata(&path)
            .map_err(|e| Error::io(&path, e))?
            .len();
        if len > MAX_MANIFEST_BYTES {
            return Err(Error::io(
                &path,
                std::io::Error::other(format!(
                    "manifest.json 过大（{len} 字节，上限 {MAX_MANIFEST_BYTES}）"
                )),
            ));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_str(&text).map_err(|e| Error::data("manifest.json", e))
    }
}

// ───────────────────────────── 写前日志（Journal） ─────────────────────────────

/// 操作日志：每记录一个动作就**原子落盘** `manifest.json`。
///
/// 这保证即使清理中途进程被杀或断电，隔离区里也始终有一份与已执行动作一致的
/// manifest，这些改动因此都可被还原，不会变成无法识别、无法还原的残留文件。
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
    pub fn start(
        sys: &SysContext,
        q: &Quarantine,
        manifest: Manifest,
        rep: &Reporter,
    ) -> Result<Self> {
        // 先建目录并收紧权限，再写入第一份 manifest：
        // 权限必须在第一个字节落盘之前就位
        q.prepare(sys, rep)?;
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

/// 一个可用备份的展示信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupInfo {
    pub dir: PathBuf,
    /// 本次清理的开始时间（形如 `20261011-132600`）。读不到时退回目录名。
    pub started_at: String,
    /// 已记录的动作数（manifest 可解析时才有意义）
    pub actions: usize,
    /// manifest 无法解析（被截断或被改过）时为 false
    pub manifest_ok: bool,
}

/// 列出某个根目录下所有可用的备份（即含 `manifest.json` 的子目录），最近的排在前面
///
/// 排序依据 manifest 记录的开始时间，不用路径字符串：路径里带着上级目录名，
/// 新旧两个根目录混在一起时字典序排不出先后。
pub fn list_backups(root: &Path) -> Vec<BackupInfo> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut v: Vec<BackupInfo> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join("manifest.json").is_file())
        .map(describe_backup)
        .collect();
    v.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    v
}

/// 读出一份备份的展示信息；manifest 读不出来时不报错，只标记 `manifest_ok = false`
fn describe_backup(dir: PathBuf) -> BackupInfo {
    let q = Quarantine::at(dir.clone());
    match q.read_manifest() {
        Ok(m) => BackupInfo {
            started_at: m.started_at,
            actions: m.actions.len(),
            manifest_ok: true,
            dir,
        },
        Err(_) => BackupInfo {
            // 读不出来时退回目录名——本工具正是用时间戳命名的，
            // 至少还能排出先后
            started_at: dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_owned(),
            actions: 0,
            manifest_ok: false,
            dir,
        },
    }
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
        let q = Quarantine::new(&base.join("quarantine"), &install, "t").without_acl_restriction();
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
        let mut j = Journal::start(&sys, &q, m, &Reporter::silent()).unwrap();
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

    #[test]
    fn default_root_is_on_install_volume_root() {
        // 嵌套安装（如 Program Files 下）时，隔离区在卷根目录，与安装目录同级无关
        assert_eq!(
            default_root(Path::new(r"D:\Program Files\BaiduNetdisk")).unwrap(),
            Path::new(r"D:\imageviewer-cleaner\quarantine")
        );
        // 安装在卷根目录时位置不变
        assert_eq!(
            default_root(Path::new(r"D:\BaiduNetdisk")).unwrap(),
            Path::new(r"D:\imageviewer-cleaner\quarantine")
        );
    }

    #[test]
    fn legacy_root_only_exists_for_nested_installs() {
        assert_eq!(
            legacy_root(Path::new(r"D:\Program Files\BaiduNetdisk")),
            Some(PathBuf::from(
                r"D:\Program Files\imageviewer-cleaner\quarantine"
            ))
        );
        assert_eq!(legacy_root(Path::new(r"D:\BaiduNetdisk")), None);
    }

    #[test]
    fn delete_backup_rejects_dirs_without_manifest() {
        let base = std::env::temp_dir().join("bc_delete_invalid");
        let _ = std::fs::remove_dir_all(&base);
        let dir = base.join("not-a-backup");
        std::fs::create_dir_all(&dir).unwrap();

        assert!(delete_backup(&dir).is_err());
        assert!(dir.exists(), "不是备份的目录不得删除");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn delete_backup_prunes_only_our_empty_ancestors() {
        let base = std::env::temp_dir().join("bc_delete_prune");
        let _ = std::fs::remove_dir_all(&base);
        let dir = base
            .join("Program Files")
            .join(QUARANTINE_DIR_NAME)
            .join("quarantine")
            .join("t1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("manifest.json"), b"{}").unwrap();

        delete_backup(&dir).unwrap();

        assert!(!dir.exists());
        assert!(
            !base
                .join("Program Files")
                .join(QUARANTINE_DIR_NAME)
                .exists(),
            "本工具留下的空壳目录应被一并移除"
        );
        assert!(
            base.join("Program Files").is_dir(),
            "不属于自己的目录不得动"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
