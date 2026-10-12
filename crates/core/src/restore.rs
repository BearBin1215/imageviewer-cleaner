//! ⑤ 还原层 —— 按 manifest 把系统恢复到清理前
//!
//! 顺序：先移除占位物，再把文件移回原位，最后恢复注册表。
//! 占位物不移除的话，文件移回去会被同名的占位物挡住。

use crate::backup::Quarantine;
use crate::error::Result;
use crate::model::{Action, Manifest};
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
}

impl RestoreReport {
    /// 已成功撤销的动作数（供编排层回滚统计）
    pub fn total_undone(&self) -> usize {
        self.placeholders_removed + self.files_restored + self.registry_files
    }
}

/// 从隔离区还原（读取 manifest 后逆序撤销全部动作）
pub fn restore(
    sys: &SysContext,
    q: &Quarantine,
    rep: &Reporter,
) -> Result<(Manifest, RestoreReport)> {
    let m = q.read_manifest()?;
    let report = undo_actions(sys, &m.actions, rep);
    sys.shell.notify_assoc_changed();
    Ok((m, report))
}

/// 逆序撤销一组动作。编排层回滚与事后还原共用这一套逻辑，避免两处实现不一致。
///
/// 逆序的意义：占位物在动作列表末尾（加固最后执行），逆序时先被移除，
/// 这样随后把文件移回原位时不会被同名占位物挡住。整个过程容错推进，
/// 单项失败只记录、不中断，以便尽量多地恢复。
pub fn undo_actions(sys: &SysContext, actions: &[Action], rep: &Reporter) -> RestoreReport {
    let mut report = RestoreReport::default();
    // 1) 先移除所有占位物（与动作先后顺序无关），否则文件移不回去
    for a in actions {
        if let Action::PlaceholderCreated { path, .. } = a {
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
        if !sys.fs.exists(to) {
            continue;
        }
        // 原位已有东西就跳过，不覆盖用户后来的文件
        if sys.fs.exists(from) {
            rep.warn(format!("原位已存在，跳过：{}", from.display()));
            report.files_skipped += 1;
            continue;
        }
        if sys.fs.move_path(to, from).is_ok() {
            report.files_restored += 1;
        }
    }
    // 3) 恢复注册表：导入之前导出的 .reg（UserChoice 等哈希保护项可能失败，不致命）
    for a in actions {
        let exported = match a {
            Action::RegKeyDeleted { exported_to, .. }
            | Action::RegValueDeleted { exported_to, .. } => exported_to,
            _ => continue,
        };
        if !exported.exists() {
            continue;
        }
        match sys.registry.import_reg_file(exported) {
            Ok(()) => report.registry_files += 1,
            Err(e) => {
                rep.warn(format!("注册表项导入失败（{}）：{e}", exported.display()));
                report.registry_failed.push(exported.display().to_string());
            }
        }
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
    use crate::model::{PlaceholderKind, RegPath, RegRoot};
    use crate::sys::SysContext;
    use crate::sys::mock::{FakeProcesses, InMemoryRegistry, NoopShell};
    use std::path::PathBuf;

    fn sys_with(reg: InMemoryRegistry) -> SysContext {
        SysContext {
            registry: Box::new(reg),
            process: Box::new(FakeProcesses::new()),
            fs: Box::new(crate::sys::fs::Win32Fs::new()),
            shell: Box::new(NoopShell::default()),
        }
    }

    fn manifest_with(install: PathBuf, qdir: PathBuf, actions: Vec<Action>) -> Manifest {
        Manifest {
            version: "0.1.0".into(),
            started_at: "20261011-130000".into(),
            finished_at: None,
            install_dir: install,
            quarantine_dir: qdir,
            target_user_sid: "S-1-5-21-1-2-3-1001".into(),
            actions,
        }
    }

    #[test]
    fn restore_removes_placeholder_then_moves_file_back() {
        let base = std::env::temp_dir().join("bc_restore_roundtrip");
        let _ = std::fs::remove_dir_all(&base);
        let install = base.join("install");
        let qroot = base.join("quarantine");
        std::fs::create_dir_all(install.join("module")).unwrap();
        std::fs::create_dir_all(&qroot).unwrap();

        let reg = InMemoryRegistry::new();
        let sys = sys_with(reg);

        // 模拟清理后的现场：原目录已被移走，原位放了占位文件
        let rel = r"module\ImageViewer";
        let q = Quarantine::new(&qroot, &install, "t");
        let moved_to = q.files_dir().join(rel);
        std::fs::create_dir_all(&moved_to).unwrap();
        std::fs::write(moved_to.join("BaiduNetdiskImageViewer.exe"), b"payload").unwrap();

        let original = install.join(rel);
        std::fs::write(&original, b"").unwrap(); // 占位文件

        let m = manifest_with(
            install.clone(),
            q.dir.clone(),
            vec![
                Action::PlaceholderCreated {
                    path: original.clone(),
                    kind: PlaceholderKind::FileInsteadOfDir,
                },
                Action::FileMoved {
                    from: original.clone(),
                    to: moved_to.clone(),
                    cross_volume: false,
                },
            ],
        );
        q.write_manifest(&sys, &m).unwrap();

        let (_, report) = restore(&sys, &q, &Reporter::silent()).unwrap();

        assert_eq!(report.placeholders_removed, 1);
        assert_eq!(report.files_restored, 1);
        assert!(original.is_dir(), "文件应已移回，且恢复成目录");
        assert!(original.join("BaiduNetdiskImageViewer.exe").is_file());

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

        let (_, report) = restore(&sys, &q, &Reporter::silent()).unwrap();
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

        // 指向一个不存在的 .reg → 应被跳过，不中断还原
        let m = manifest_with(
            base.clone(),
            q.dir.clone(),
            vec![Action::RegKeyDeleted {
                hive: "HKU".into(),
                key: RegPath {
                    root: RegRoot::Hku,
                    sub: "x".into(),
                }
                .sub,
                exported_to: base.join("不存在.reg"),
            }],
        );
        q.write_manifest(&sys, &m).unwrap();

        let (_, report) = restore(&sys, &q, &Reporter::silent()).unwrap();
        assert_eq!(report.registry_files, 0);
        assert!(report.registry_failed.is_empty(), "文件不存在时应静默跳过");

        let _ = std::fs::remove_dir_all(&base);
    }
}
