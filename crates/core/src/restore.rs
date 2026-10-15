//! ⑤ 还原层 —— 按 manifest 把系统恢复到清理前
//!
//! 顺序：先移除占位物，再把文件移回原位，最后恢复注册表。
//! 占位物不移除的话，文件移回去会被同名的占位物挡住。
//!
//! **manifest 与 `.reg` 都是磁盘上的输入，不能当作事实。** 它们可能被替换或篡改
//! （隔离区目录一旦可被他人写入，或用户在别处编辑过备份），因此这里执行的每个
//! 路径都要先过 [`RestoreScope`]：文件只允许位于安装目录或隔离区内，且必须是
//! 组件声明过的清理目标；注册表动作的键必须命中数据表声明的关联项，`.reg` 内容
//! 再被限定在该键子树之内。

use std::path::{Path, PathBuf};

use crate::backup::Quarantine;
use crate::data::Tables;
use crate::detect;
use crate::error::Result;
use crate::model::{Action, Manifest, RegPath};
use crate::progress::Reporter;
use crate::sys::SysContext;

/// 一次还原的结果
#[derive(Debug, Default, Clone)]
pub struct RestoreReport {
    pub placeholders_removed: usize,
    pub files_restored: usize,
    pub files_skipped: usize,
    pub registry_files: usize,
    pub registry_failed: Vec<String>,
    /// 因越出 [`RestoreScope`] 而被拒绝的动作数。
    ///
    /// 正常备份不会产生这个数；非零意味着备份文件被改动过，或安装目录已变。
    pub blocked: usize,
}

impl RestoreReport {
    /// 已成功撤销的动作数（供编排层回滚统计）
    pub fn total_undone(&self) -> usize {
        self.placeholders_removed + self.files_restored + self.registry_files
    }
}

/// 还原/回滚允许触碰的边界
pub struct RestoreScope<'a> {
    pub tables: &'a Tables,
    /// 目标用户，取自 manifest。它只决定候选键按哪个用户的展开来匹配，边界的安全性
    /// 不依赖它：候选键全部是百度命名的关联键（外加 `RegisteredApplications` 与
    /// FileExts 的 UserChoice），见 `declared_registry_keys`；伪造 SID 至多
    /// 把导入引向另一个用户的同名关联键，不会触及自启动位置。
    pub sid: &'a str,
    /// 真实安装目录，来自探测结果。不能用 manifest 自报的值：那样边界就成了
    /// 攻击者可以随手修改的字段。
    pub install_dir: PathBuf,
    /// 隔离区里的文件目录
    pub files_dir: PathBuf,
    /// 隔离区里的注册表导出目录
    pub registry_dir: PathBuf,
}

impl<'a> RestoreScope<'a> {
    /// 由探测结果与隔离区构造。回滚与还原共用这一入口，保证两处边界一致。
    pub fn new(
        tables: &'a Tables,
        sid: &'a str,
        install_dir: &Path,
        q: &Quarantine,
    ) -> RestoreScope<'a> {
        Self {
            tables,
            sid,
            install_dir: install_dir.to_path_buf(),
            files_dir: q.files_dir(),
            registry_dir: q.registry_dir(),
        }
    }

    /// 数据表声明的全部关联键候选（与探测、清理同源）。
    ///
    /// 动作键必须恰好命中其中某一项。manifest 记录的 hive/key 同样来自磁盘，
    /// 不能直接当作导入边界：篡改者可以把它写成任意键（如 `Run`），再配一份同键的
    /// `.reg`，子树校验就形同虚设。
    fn declared_registry_keys(&self) -> Vec<RegPath> {
        detect::scan_registry(self.tables, self.sid)
            .into_iter()
            .map(|item| item.path)
            .collect()
    }

    /// 占位物：必须位于安装目录内，且现场确实是占位物。
    ///
    /// 类型判定复用 [`detect::is_placeholder`]，与探测、清理同源。仅比较路径不够：
    /// `remove_path` 会递归删目录，一份伪造的 manifest 可以把任意路径写成占位物删掉；
    /// 加上类型判定之后，真文件已经还原回来的位置就不会再被当作占位物删除。
    fn allows_placeholder(&self, sys: &SysContext, path: &Path) -> bool {
        let Some(rel) = rel_within(&self.install_dir, path) else {
            return false;
        };
        detect::is_placeholder(sys, self.tables, &self.install_dir, &rel)
    }

    /// 文件动作：`from` 是安装目录里的声明目标，`to` 是隔离区里的同一相对路径
    fn allows_file_move(&self, from: &Path, to: &Path) -> bool {
        let Some(rel) = rel_within(&self.install_dir, from) else {
            return false;
        };
        if !crate::model::is_declared_target(&rel) {
            return false;
        }
        match rel_within(&self.files_dir, to) {
            Some(to_rel) => to_rel.eq_ignore_ascii_case(&rel),
            None => false,
        }
    }

    /// 注册表备份文件：必须位于隔离区的导出目录内
    fn allows_registry_file(&self, p: &Path) -> bool {
        rel_within(&self.registry_dir, p).is_some()
    }
}

/// `p` 相对 `root` 的路径；不在其下时返回 `None`。
///
/// 只做词法比较，且按 Windows 的习惯忽略大小写。重解析点不在这里处理——
/// 真正的物理位置校验在移动前由 `FsBackend::resolves_within` 负责。
fn rel_within(root: &Path, p: &Path) -> Option<String> {
    let root = root.to_string_lossy();
    let root = root.trim_end_matches(['\\', '/']);
    if root.is_empty() {
        return None;
    }
    let p = p.to_string_lossy();
    if p.len() <= root.len() {
        return None;
    }
    let head = p.get(..root.len())?;
    if !head.eq_ignore_ascii_case(root) {
        return None;
    }
    let sep = *p.as_bytes().get(root.len())?;
    if sep != b'\\' && sep != b'/' {
        return None;
    }
    Some(p.get(root.len() + 1..)?.to_owned())
}

/// 从隔离区还原（读取 manifest 后逆序撤销全部动作）
pub fn restore(
    sys: &SysContext,
    q: &Quarantine,
    scope: &RestoreScope<'_>,
    rep: &Reporter,
) -> Result<(Manifest, RestoreReport)> {
    let m = q.read_manifest()?;
    let report = undo_actions(sys, &m.actions, scope, rep);
    sys.shell.notify_assoc_changed();
    Ok((m, report))
}

/// 逆序撤销一组动作。编排层回滚与事后还原共用这一套逻辑，避免两处实现不一致。
///
/// 逆序的意义：占位物在动作列表末尾（加固最后执行），逆序时先被移除，
/// 这样随后把文件移回原位时不会被同名占位物挡住。整个过程容错推进，
/// 单项失败只记录、不中断，以便尽量多地恢复——但**越界的目标一律不执行**：
/// 那说明备份被改动过或环境已变，此时宁可少恢复。
pub fn undo_actions(
    sys: &SysContext,
    actions: &[Action],
    scope: &RestoreScope<'_>,
    rep: &Reporter,
) -> RestoreReport {
    let mut report = RestoreReport::default();

    // 1) 先移除所有占位物（与动作先后顺序无关），否则文件移不回去
    for a in actions {
        if let Action::PlaceholderCreated { path, .. } = a {
            if !scope.allows_placeholder(sys, path) {
                rep.warn(format!("跳过越界的占位物：{}", path.display()));
                report.blocked += 1;
                continue;
            }
            // `remove_path` 会递归删目录。联接能把删除引到安装目录之外，而词法闸门
            // 对这种路径一律放行，因此这里与移文件那一步一样要单独拦一次
            if !sys.fs.resolves_within(&scope.install_dir, path) {
                rep.warn(format!("占位物路径经过重解析点，跳过：{}", path.display()));
                report.blocked += 1;
                continue;
            }
            if sys.fs.remove_path(path).is_ok() {
                report.placeholders_removed += 1;
            }
        }
    }

    // 2) 逆序把文件移回原位
    for a in actions.iter().rev() {
        let Action::FileMoved { from, to, .. } = a else {
            continue;
        };
        if !scope.allows_file_move(from, to) {
            rep.warn(format!(
                "跳过越界的文件动作：{} -> {}",
                to.display(),
                from.display()
            ));
            report.blocked += 1;
            continue;
        }
        if !sys.fs.exists(to) {
            continue;
        }
        // 原位已有东西就跳过，不覆盖用户后来的文件
        if sys.fs.exists(from) {
            rep.warn(format!("原位已存在，跳过：{}", from.display()));
            report.files_skipped += 1;
            continue;
        }
        // 经过重解析点的路径不搬：那说明安装目录被换成了别处的入口
        if !sys.fs.resolves_within(&scope.install_dir, from) {
            rep.warn(format!("路径经过重解析点，跳过：{}", from.display()));
            report.blocked += 1;
            continue;
        }
        if sys.fs.move_path(to, from).is_ok() {
            report.files_restored += 1;
        }
    }

    // 3) 恢复注册表：导入之前导出的 .reg（UserChoice 等哈希保护项可能失败，不致命）
    let candidates = scope.declared_registry_keys();
    for a in actions {
        let (hive, key, exported) = match a {
            Action::RegKeyDeleted {
                hive,
                key,
                exported_to,
            } => (hive, key, exported_to),
            Action::RegValueDeleted {
                hive,
                key,
                exported_to,
                ..
            } => (hive, key, exported_to),
            _ => continue,
        };
        if !scope.allows_registry_file(exported) {
            rep.warn(format!("跳过越界的注册表备份：{}", exported.display()));
            report.blocked += 1;
            continue;
        }
        // 动作的键必须恰好是数据表声明过的关联项
        let Some(allowed) = candidates
            .iter()
            .find(|p| p.root_name().eq_ignore_ascii_case(hive) && p.sub.eq_ignore_ascii_case(key))
        else {
            rep.warn(format!("动作的键不是声明的关联项，拒绝导入：{hive}\\{key}"));
            report.blocked += 1;
            continue;
        };
        if !exported.exists() {
            continue;
        }
        // 导入范围限定在声明的那个键之内：备份里出现子树之外的键，
        // 说明文件被换过，整份拒绝
        match sys.registry.import_reg_file(exported, allowed) {
            Ok(()) => report.registry_files += 1,
            Err(e) => {
                rep.warn(format!("注册表项导入失败（{}）：{e}", exported.display()));
                report.registry_failed.push(exported.display().to_string());
            }
        }
    }

    if report.blocked > 0 {
        rep.warn(format!(
            "有 {} 项超出了本次允许的范围，已跳过（备份可能被改动过）",
            report.blocked
        ));
    }
    if report.placeholders_removed > 0 {
        rep.info(format!("已移除 {} 个占位物", report.placeholders_removed));
    }
    if report.files_restored > 0 {
        rep.success(format!("已移回 {} 项文件", report.files_restored));
    }
    if report.registry_files > 0 {
        rep.success(format!("已恢复 {} 个注册表项", report.registry_files));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PlaceholderKind;
    use crate::sys::SysContext;
    use crate::sys::mock::{FakeProcesses, InMemoryRegistry, NoopShell};
    use std::path::PathBuf;

    const SID: &str = "S-1-5-21-1-2-3-1001";

    fn sys_with(reg: InMemoryRegistry) -> SysContext {
        SysContext {
            registry: Box::new(reg),
            process: Box::new(FakeProcesses::new()),
            fs: Box::new(crate::sys::fs::Win32Fs::new()),
            shell: Box::new(NoopShell::default()),
        }
    }

    fn tables() -> Tables {
        Tables::load().unwrap()
    }

    fn scope<'a>(tables: &'a Tables, install: &Path, q: &Quarantine) -> RestoreScope<'a> {
        RestoreScope::new(tables, SID, install, q)
    }

    fn link_dir(link: &Path, target: &Path) -> bool {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn unlink_dir(link: &Path) {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "rmdir"])
            .arg(link)
            .status();
    }

    fn manifest_with(install: PathBuf, qdir: PathBuf, actions: Vec<Action>) -> Manifest {
        Manifest {
            version: "0.1.0".into(),
            started_at: "20261011-130000".into(),
            finished_at: None,
            install_dir: install,
            quarantine_dir: qdir,
            target_user_sid: SID.into(),
            actions,
        }
    }

    #[test]
    fn restore_removes_placeholder_then_moves_file_back() {
        let base = std::env::temp_dir().join("bc_restore_roundtrip");
        let _ = std::fs::remove_dir_all(&base);
        let install = base.join("install");
        let qroot = base.join("quarantine");
        std::fs::create_dir_all(install.join(r"module\BrowserEngine")).unwrap();
        std::fs::create_dir_all(&qroot).unwrap();

        let reg = InMemoryRegistry::new();
        let sys = sys_with(reg);

        // 用真实的占位目标：启动器位置本该是文件，清理后放的是同名目录
        let rel = r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe";
        let q = Quarantine::new(&qroot, &install, "t");
        let moved_to = q.files_dir().join(rel);
        std::fs::create_dir_all(moved_to.parent().unwrap()).unwrap();
        std::fs::write(&moved_to, b"payload").unwrap();

        let original = install.join(rel);
        std::fs::create_dir_all(&original).unwrap(); // 占位目录

        let m = manifest_with(
            install.clone(),
            q.dir.clone(),
            vec![
                Action::PlaceholderCreated {
                    path: original.clone(),
                    kind: PlaceholderKind::DirInsteadOfFile,
                },
                Action::FileMoved {
                    from: original.clone(),
                    to: moved_to.clone(),
                    cross_volume: false,
                },
            ],
        );
        q.write_manifest(&sys, &m).unwrap();

        let tables = tables();
        let (_, report) =
            restore(&sys, &q, &scope(&tables, &install, &q), &Reporter::silent()).unwrap();

        assert_eq!(report.placeholders_removed, 1);
        assert_eq!(report.files_restored, 1);
        assert_eq!(report.blocked, 0);
        assert!(original.is_file(), "占位目录应被移除，真文件应移回");
        assert_eq!(std::fs::read(&original).unwrap(), b"payload");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn restore_skips_when_destination_already_exists() {
        let base = std::env::temp_dir().join("bc_restore_skip");
        let _ = std::fs::remove_dir_all(&base);
        let install = base.join("install");
        let qroot = base.join("quarantine");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::create_dir_all(&qroot).unwrap();

        let sys = sys_with(InMemoryRegistry::new());
        let q = Quarantine::new(&qroot, &install, "t");

        let from = install.join("module").join("ImageViewer");
        let to = q.files_dir().join("module").join("ImageViewer");
        std::fs::create_dir_all(&to).unwrap();
        // 原位已有用户后来的文件 → 不得覆盖
        std::fs::create_dir_all(&from).unwrap();

        let m = manifest_with(
            install.clone(),
            q.dir.clone(),
            vec![Action::FileMoved {
                from: from.clone(),
                to,
                cross_volume: false,
            }],
        );
        q.write_manifest(&sys, &m).unwrap();

        let tables = tables();
        let (_, report) =
            restore(&sys, &q, &scope(&tables, &install, &q), &Reporter::silent()).unwrap();
        assert_eq!(report.files_restored, 0);
        assert_eq!(report.files_skipped, 1, "原位已存在时应跳过");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn restore_reports_registry_failures_without_aborting() {
        let base = std::env::temp_dir().join("bc_restore_regfail");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let sys = sys_with(InMemoryRegistry::new());
        let q = Quarantine::new(&base, &base, "t");

        // 动作键是声明的关联项，备份文件在导出目录内但不存在 → 静默跳过，不中断还原
        let m = manifest_with(
            base.clone(),
            q.dir.clone(),
            vec![Action::RegKeyDeleted {
                hive: "HKU".into(),
                key: format!("{SID}_Classes\\BaiduNetdiskPlayerAssociations"),
                exported_to: q.registry_dir().join("不存在.reg"),
            }],
        );
        q.write_manifest(&sys, &m).unwrap();

        let tables = tables();
        let (_, report) =
            restore(&sys, &q, &scope(&tables, &base, &q), &Reporter::silent()).unwrap();
        assert_eq!(report.registry_files, 0);
        assert_eq!(report.blocked, 0, "声明过的键与导出目录内的路径都应放行");
        assert!(report.registry_failed.is_empty(), "文件不存在时应静默跳过");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 备份里出现安装目录之外的路径时必须拒绝——这是「manifest 被改动过」的信号
    #[test]
    fn restore_refuses_paths_outside_the_scope() {
        let base = std::env::temp_dir().join("bc_restore_scope");
        let _ = std::fs::remove_dir_all(&base);
        let install = base.join("install");
        let qroot = base.join("quarantine");
        let victim = base.join("victim");
        std::fs::create_dir_all(install.join("module")).unwrap();
        std::fs::create_dir_all(&qroot).unwrap();
        std::fs::create_dir_all(&victim).unwrap();

        let sys = sys_with(InMemoryRegistry::new());
        let q = Quarantine::new(&qroot, &install, "t");

        // 安装目录之外的重要文件，以及攻击者准备搬进去的载荷
        let important = victim.join("important.txt");
        std::fs::write(&important, b"IMPORTANT").unwrap();
        let payload = q.files_dir().join("payload.dll");
        std::fs::create_dir_all(q.files_dir()).unwrap();
        std::fs::write(&payload, b"PAYLOAD").unwrap();
        let planted = victim.join("planted.exe");

        let m = manifest_with(
            install.clone(),
            q.dir.clone(),
            vec![
                // 越界删除
                Action::PlaceholderCreated {
                    path: important.clone(),
                    kind: PlaceholderKind::DirInsteadOfFile,
                },
                // 越界写入
                Action::FileMoved {
                    from: planted.clone(),
                    to: payload.clone(),
                    cross_volume: false,
                },
                // 安装目录内、但不是任何组件声明的目标 → 同样拒绝
                Action::PlaceholderCreated {
                    path: install.join("BaiduNetdisk.exe"),
                    kind: PlaceholderKind::DirInsteadOfFile,
                },
            ],
        );
        q.write_manifest(&sys, &m).unwrap();
        let client = install.join("BaiduNetdisk.exe");
        std::fs::write(&client, b"client").unwrap();

        let tables = tables();
        let (_, report) =
            restore(&sys, &q, &scope(&tables, &install, &q), &Reporter::silent()).unwrap();

        assert_eq!(report.blocked, 3, "三个越界动作都应被拒绝");
        assert!(important.is_file(), "安装目录之外的文件不得被删");
        assert!(!planted.exists(), "不得在安装目录之外创建文件");
        assert!(client.is_file(), "安装目录内的非声明目标不得被删");
        assert_eq!(report.placeholders_removed, 0);
        assert_eq!(report.files_restored, 0);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 备份文件被替换成指向别处的 .reg 时，导入必须整份失败
    #[test]
    fn restore_rejects_registry_backup_out_of_scope() {
        let base = std::env::temp_dir().join("bc_restore_regscope");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let sys = sys_with(InMemoryRegistry::new());
        let q = Quarantine::new(&base, &base, "t");
        std::fs::create_dir_all(q.registry_dir()).unwrap();

        let evil = q.registry_dir().join("evil.reg");
        std::fs::write(
            &evil,
            "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run]\r\n\
\"Updater\"=\"C:\\\\evil.exe\"\r\n",
        )
        .unwrap();

        let m = manifest_with(
            base.clone(),
            q.dir.clone(),
            vec![Action::RegKeyDeleted {
                hive: "HKU".into(),
                key: r"S-1-5-21-1-2-3-1001_Classes\BaiduNetdiskPlayerAssociations".into(),
                exported_to: evil.clone(),
            }],
        );
        q.write_manifest(&sys, &m).unwrap();

        let tables = tables();
        let (_, report) =
            restore(&sys, &q, &scope(&tables, &base, &q), &Reporter::silent()).unwrap();
        assert_eq!(report.registry_files, 0, "越界的注册表备份不得被导入");
        assert_eq!(report.registry_failed.len(), 1);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// manifest 里记录的 hive/key 不能自己充当导入边界：动作键必须是数据表
    /// 声明过的关联项。否则篡改者把它写成任意键（如 Run）再配一份同键的
    /// `.reg`，子树校验就形同虚设，导入成了任意注册表写。
    #[test]
    fn restore_rejects_actions_on_undeclared_keys() {
        let base = std::env::temp_dir().join("bc_restore_keyscope");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        let sys = sys_with(InMemoryRegistry::new());
        let q = Quarantine::new(&base, &base, "t");
        std::fs::create_dir_all(q.registry_dir()).unwrap();

        // 备份内容与动作声明的键一致（都指向 Run），但 Run 不是任何声明的关联项
        let evil = q.registry_dir().join("evil.reg");
        std::fs::write(
            &evil,
            "Windows Registry Editor Version 5.00\r\n\r\n\
[HKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run]\r\n\
\"Updater\"=\"C:\\\\evil.exe\"\r\n",
        )
        .unwrap();

        let m = manifest_with(
            base.clone(),
            q.dir.clone(),
            vec![Action::RegKeyDeleted {
                hive: "HKLM".into(),
                key: r"SOFTWARE\Microsoft\Windows\CurrentVersion\Run".into(),
                exported_to: evil.clone(),
            }],
        );
        q.write_manifest(&sys, &m).unwrap();

        let tables = tables();
        let (_, report) =
            restore(&sys, &q, &scope(&tables, &base, &q), &Reporter::silent()).unwrap();
        assert_eq!(report.registry_files, 0, "非声明键的动作不得被导入");
        assert_eq!(report.blocked, 1, "应在候选键校验处被拦截");
        assert!(report.registry_failed.is_empty());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// 占位物位置被换成联接时不得删除。
    ///
    /// `remove_path` 对目录调用 `remove_dir_all`，而联接能把删除引到安装目录之外；
    /// 词法闸门对这种路径一律放行，所以必须由 `resolves_within` 拦下。
    /// 拦下之后原位仍被占着，移文件那一步会按「原位已存在」跳过，备份保持完整。
    #[test]
    fn restore_refuses_placeholder_that_is_a_junction() {
        let base = std::env::temp_dir().join("bc_restore_junction");
        let _ = std::fs::remove_dir_all(&base);
        let install = base.join("install");
        let outside = base.join("outside");
        let qroot = base.join("quarantine");
        std::fs::create_dir_all(install.join(r"module\BrowserEngine")).unwrap();
        std::fs::create_dir_all(&qroot).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"SECRET").unwrap();

        // 占位目标的位置被做成指向安装目录之外的联接
        let launcher = install.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe");
        if !link_dir(&launcher, &outside) {
            println!("跳过：无法创建 junction");
            let _ = std::fs::remove_dir_all(&base);
            return;
        }

        let sys = sys_with(InMemoryRegistry::new());
        let q = Quarantine::new(&qroot, &install, "t");
        let moved = q
            .files_dir()
            .join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::write(&moved, b"payload").unwrap();

        let m = manifest_with(
            install.clone(),
            q.dir.clone(),
            vec![
                Action::PlaceholderCreated {
                    path: launcher.clone(),
                    kind: PlaceholderKind::DirInsteadOfFile,
                },
                Action::FileMoved {
                    from: launcher.clone(),
                    to: moved.clone(),
                    cross_volume: false,
                },
            ],
        );
        q.write_manifest(&sys, &m).unwrap();

        let tables = tables();
        let (_, report) =
            restore(&sys, &q, &scope(&tables, &install, &q), &Reporter::silent()).unwrap();

        assert_eq!(report.blocked, 1, "联接处的占位物应被拒绝");
        assert_eq!(report.placeholders_removed, 0);
        assert!(
            outside.join("secret.txt").is_file(),
            "联接目标的内容不得被删除"
        );
        assert!(moved.is_file(), "备份里的文件不得被移出隔离区");
        assert!(sys.fs.is_reparse_point(&launcher), "联接本身应仍在原位");

        unlink_dir(&launcher);
        let _ = std::fs::remove_dir_all(&base);
    }
}
