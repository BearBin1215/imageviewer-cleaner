//! 编排层 —— 按顺序执行各层，上报进度，任一步失败则回滚已完成的部分
//!
//! 全部操作跑在调用方的 worker 线程上，进度通过 [`Reporter`] 送回界面。

use std::path::{Path, PathBuf};

use crate::backup::{self, Journal, Quarantine};
use crate::clean::{self, CleanOptions};
use crate::data::Tables;
use crate::detect;
use crate::error::Result;
use crate::harden;
use crate::model::{Action, ComponentId, Detection, Manifest};
use crate::progress::{Progress, Reporter, Stage, Summary};
use crate::restore;
use crate::sys::SysContext;

/// 清理配置
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// 用户勾选的组件
    pub selected: Vec<ComponentId>,
    /// 是否建立占位物阻止重新部署。
    ///
    /// 默认开启：占位法不会导致客户端启动失败，是阻断组件被重新下载部署的
    /// 主要手段。若个别环境下仍出现异常，可关闭本项，或清理后用「还原」恢复。
    pub harden: bool,
    /// 覆盖隔离区根目录。默认取安装目录所在卷下的 `imageviewer-cleaner\quarantine`。
    /// 测试用它指向临时目录。
    pub quarantine_root: Option<PathBuf>,
}

impl CleanupConfig {
    pub fn new(selected: Vec<ComponentId>) -> Self {
        Self {
            selected,
            harden: true,
            quarantine_root: None,
        }
    }

    pub fn with_harden(mut self, harden: bool) -> Self {
        self.harden = harden;
        self
    }

    pub fn with_quarantine_root(mut self, root: PathBuf) -> Self {
        self.quarantine_root = Some(root);
        self
    }
}

/// 执行一次完整清理。
///
/// 隔离区建立后立即写入基线 manifest，之后**每完成一个动作就落盘**（写前日志，
/// 见 [`Journal`]）。即使中途进程被杀/断电，隔离区里也始终有一份可还原的记录。
pub fn run_cleanup(
    sys: &SysContext,
    tables: &Tables,
    cfg: &CleanupConfig,
    rep: &Reporter,
) -> Result<Summary> {
    rep.stage(Stage::Detect);
    let d = detect::detect(sys, tables)?;
    rep.success(format!("安装目录：{}", d.install_dir.display()));
    rep.success(format!("目标用户：{}", d.target_user_sid));
    rep.emit(Progress::Detected(Box::new(d.clone())));

    rep.stage(Stage::Backup);
    let root = match &cfg.quarantine_root {
        Some(r) => r.clone(),
        None => backup::default_root(&d.install_dir)?,
    };
    let q = Quarantine::new(&root, &d.install_dir, &backup::local_timestamp());
    let manifest = Manifest {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at: backup::local_timestamp(),
        finished_at: None,
        install_dir: d.install_dir.clone(),
        quarantine_dir: q.dir.clone(),
        target_user_sid: d.target_user_sid.clone(),
        actions: Vec::new(),
    };
    let mut journal = Journal::start(sys, &q, manifest)?;
    q.mark_hidden(sys); // 目录此时已建立，隐藏属性才能真正生效
    rep.info(format!("隔离区：{}", q.dir.display()));

    match mutate(sys, tables, cfg, &d, &q, &mut journal, rep) {
        Ok(()) => {
            let mut summary = summarize(&journal);
            let m = journal.finish()?;
            summary.quarantine_dir = Some(m.quarantine_dir.clone());
            rep.success(format!("隔离区：{}", m.quarantine_dir.display()));
            Ok(summary)
        }
        Err(e) => {
            rep.warn(format!("清理中断：{e}"));
            rep.warn("正在回滚已完成的改动…");
            let undone = rollback(sys, journal.actions(), rep);
            rep.warn(format!("回滚完成，共撤销 {undone} 步"));
            discard_quarantine_if_rolled_back(&q, journal.actions(), undone);
            Err(e)
        }
    }
}

/// 执行破坏性步骤；每个动作都通过 journal 即时落盘。
fn mutate(
    sys: &SysContext,
    tables: &Tables,
    cfg: &CleanupConfig,
    d: &Detection,
    q: &Quarantine,
    journal: &mut Journal,
    rep: &Reporter,
) -> Result<()> {
    rep.stage(Stage::KillProcesses);
    let opts = CleanOptions::from_tables(tables, cfg.selected.clone());
    clean::kill_processes(sys, tables, &opts, journal, rep)?;

    rep.stage(Stage::BreakAssoc);
    let items = detect::existing_registry_items(sys, &d.registry_items);
    rep.info(format!("待处理关联项 {} 条", items.len()));
    clean::break_associations(sys, &items, q, journal, rep)?;

    rep.stage(Stage::MoveFiles);
    clean::move_files(sys, tables, &d.install_dir, &cfg.selected, q, journal, rep)?;

    // 加固：默认开启，阻断组件被重新部署。
    rep.stage(Stage::Harden);
    if cfg.harden {
        let before = journal.actions().len();
        harden::apply(sys, tables, &d.install_dir, journal)?;
        let n = journal.actions().len() - before;
        if n > 0 {
            rep.success(format!("已建立 {n} 个占位物，阻断重新部署"));
        }
    } else {
        rep.info("未启用防复发加固；网盘更新时可能重新部署这些组件");
    }

    rep.stage(Stage::Refresh);
    clean::refresh(sys);
    rep.stage(Stage::Verify);
    clean::verify(sys, tables, &d.install_dir)?;
    rep.success("主客户端完整性校验通过");
    Ok(())
}

/// 从日志汇总本次清理结果。
fn summarize(j: &Journal) -> Summary {
    let mut s = Summary {
        bytes_reclaimed: j.bytes_reclaimed,
        failed: j.failures,
        ..Default::default()
    };
    for a in j.actions() {
        match a {
            Action::ProcessKilled { .. } => s.processes_killed += 1,
            Action::RegKeyDeleted { .. } | Action::RegValueDeleted { .. } => {
                s.registry_items_cleared += 1
            }
            Action::FileMoved { .. } => s.files_moved += 1,
            Action::PlaceholderCreated { .. } => s.placeholders_created += 1,
        }
    }
    s
}

/// 回滚已完成的动作（复用还原层的逆序撤销逻辑，避免两处实现不一致）。
fn rollback(sys: &SysContext, actions: &[Action], rep: &Reporter) -> usize {
    let report = restore::undo_actions(sys, actions, rep);
    if actions
        .iter()
        .any(|a| matches!(a, Action::ProcessKilled { .. }))
    {
        rep.info("已结束的进程不会被重新拉起，这是预期行为");
    }
    report.total_undone()
}

/// 若可逆动作已全部撤销，删除这份隔离区，避免界面出现一份实为空的备份。
fn discard_quarantine_if_rolled_back(q: &Quarantine, actions: &[Action], undone: usize) {
    let reversible = actions
        .iter()
        .filter(|a| !matches!(a, Action::ProcessKilled { .. }))
        .count();
    if undone >= reversible {
        let _ = std::fs::remove_dir_all(&q.dir);
    }
}

/// 从最近一次备份还原
pub fn run_restore(sys: &SysContext, quarantine_dir: &Path, rep: &Reporter) -> Result<()> {
    rep.stage(Stage::Backup);
    let q = Quarantine::at(quarantine_dir, PathBuf::new());
    // install_dir 由 manifest 提供，这里只是为了构造读取入口
    let m = q.read_manifest()?;
    let q = Quarantine::at(quarantine_dir, m.install_dir.clone());

    rep.info(format!("从 {} 还原", q.dir.display()));
    let (_, report) = restore::restore(sys, &q, rep)?;

    if !report.registry_failed.is_empty() {
        rep.warn(format!(
            "{} 个注册表项未能导入，系统会重新询问默认程序",
            report.registry_failed.len()
        ));
    }
    rep.success("还原完成");
    Ok(())
}

/// 只探测，不做任何改动
pub fn run_detect(sys: &SysContext, tables: &Tables, rep: &Reporter) -> Result<Detection> {
    rep.stage(Stage::Detect);
    rep.info("正在探测安装位置…");
    let d = detect::detect(sys, tables)?;
    rep.success(format!("安装目录：{}", d.install_dir.display()));
    rep.success(format!("目标用户：{}", d.target_user_sid));
    Ok(d)
}

/// 定位安装目录，并列出该处可用的备份（最近的排在前面）。
///
/// 界面用它刷新「还原 / 删除备份」可用的列表，无需自行拼装探测与备份逻辑。
pub fn locate_backups(sys: &SysContext, tables: &Tables, sid: &str) -> Result<Vec<PathBuf>> {
    let install_dir = detect::locate_install_dir(sys, tables, sid)?;
    let root = backup::default_root(&install_dir)?;
    Ok(backup::list_backups(&root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::SysContext;
    use crate::sys::mock::{FakeProcesses, InMemoryRegistry, NoopShell};

    fn sys_with(reg: InMemoryRegistry, proc: FakeProcesses) -> SysContext {
        SysContext {
            registry: Box::new(reg),
            process: Box::new(proc),
            fs: Box::new(crate::sys::fs::Win32Fs::new()),
            shell: Box::new(NoopShell::default()),
        }
    }

    const SID: &str = "S-1-5-21-1-2-3-1001";

    /// 造一份完整的假安装目录与假注册表
    fn fake_env(name: &str) -> (SysContext, PathBuf) {
        let dir = std::env::temp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"module\BrowserEngine\resources")).unwrap();
        std::fs::create_dir_all(dir.join(r"module\ImageViewer")).unwrap();
        std::fs::create_dir_all(dir.join(r"module\VastPlayer")).unwrap();

        // 主客户端关键文件（禁区，校验用）
        std::fs::write(dir.join("BaiduNetdisk.exe"), b"client").unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\BaiduNetdiskUnite.exe"),
            b"engine",
        )
        .unwrap();
        std::fs::write(dir.join(r"module\BrowserEngine\resources\app.asar"), b"app").unwrap();

        // 待清理的组件
        std::fs::write(
            dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe"),
            b"viewer",
        )
        .unwrap();
        // 目录里还有运行时依赖，用来验证「清空目录内容」会一并移走
        std::fs::write(dir.join(r"module\ImageViewer\Qt5Core.dll"), b"qt").unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe"),
            b"launch",
        )
        .unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\resources\video_player.asar"),
            b"asar",
        )
        .unwrap();

        let reg = InMemoryRegistry::new();
        reg.set_user_hives(vec![SID.into()]);
        reg.add_key(
            crate::model::RegPath::user_software(SID).join(r"Software\Baidu\BaiduNetdiskPlayer"),
        );
        reg.add_key(
            crate::model::RegPath::user_classes(SID).join("BaiduNetdiskPlayerAssociations"),
        );

        let proc = FakeProcesses::new();
        proc.add("BaiduNetdisk.exe", 100, Some(dir.join("BaiduNetdisk.exe")));

        (sys_with(reg, proc), dir)
    }

    #[test]
    fn cleanup_then_restore_returns_to_original_state() {
        let (sys, dir) = fake_env("bc_orch_roundtrip");
        let tables = Tables::load().unwrap();
        let qroot = std::env::temp_dir().join("bc_orch_quarantine");
        let _ = std::fs::remove_dir_all(&qroot);

        // 显式启用加固，以便覆盖占位物的建立与还原
        let cfg = CleanupConfig::new(vec![ComponentId::ImageViewer, ComponentId::PlayerLauncher])
            .with_harden(true)
            .with_quarantine_root(qroot.clone());

        let summary = run_cleanup(&sys, &tables, &cfg, &Reporter::silent()).unwrap();
        // 目录内容 2 项（入口程序 + 运行时 DLL）+ 启动器 1 项
        assert_eq!(summary.files_moved, 3, "应移走全部目标");
        // 只有一个文件型组件需要占位；目录本身保留，不需要占位
        assert_eq!(summary.placeholders_created, 1);
        assert!(summary.registry_items_cleared > 0);

        // 目录本身必须还在，只是内容被清空
        assert!(
            dir.join(r"module\ImageViewer").is_dir(),
            "组件目录本身必须保留，否则客户端会检测到模块缺失"
        );
        assert!(!dir.join(r"module\ImageViewer\Qt5Core.dll").exists());

        // 组件已不在原位
        assert!(
            !dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe")
                .exists()
        );
        // 禁区文件原封不动
        assert!(dir.join("BaiduNetdisk.exe").exists());
        assert!(
            dir.join(r"module\BrowserEngine\BaiduNetdiskUnite.exe")
                .exists()
        );
        assert!(
            dir.join(r"module\BrowserEngine\resources\app.asar")
                .exists()
        );

        // 找到隔离区并还原
        let qdir = std::fs::read_dir(&qroot)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.is_dir())
            .expect("应存在隔离区目录");

        run_restore(&sys, &qdir, &Reporter::silent()).unwrap();

        // 组件回到原位
        assert!(
            dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe")
                .is_file()
        );
        assert!(
            dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe")
                .is_file()
        );
        assert!(
            dir.join(r"module\BrowserEngine\resources\video_player.asar")
                .is_file()
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&qroot);
    }

    #[test]
    fn cleanup_aborts_and_rolls_back_when_client_file_missing() {
        let (sys, dir) = fake_env("bc_orch_rollback");
        let tables = Tables::load().unwrap();
        let qroot = std::env::temp_dir().join("bc_orch_rollback_q");
        let _ = std::fs::remove_dir_all(&qroot);

        // 故意删掉主客户端关键文件 → 校验必然失败
        std::fs::remove_file(dir.join(r"module\BrowserEngine\BaiduNetdiskUnite.exe")).unwrap();

        let cfg =
            CleanupConfig::new(vec![ComponentId::ImageViewer]).with_quarantine_root(qroot.clone());

        let err = run_cleanup(&sys, &tables, &cfg, &Reporter::silent()).unwrap_err();
        assert!(
            matches!(err, crate::error::Error::ClientIntegrityBroken(_)),
            "应因完整性校验失败而中断，实际 {err:?}"
        );

        // 回滚之后组件应回到原位
        assert!(
            dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe")
                .is_file(),
            "回滚后组件应已恢复"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&qroot);
    }

    #[test]
    fn detect_only_makes_no_changes() {
        let (sys, dir) = fake_env("bc_orch_detect_only");
        let tables = Tables::load().unwrap();

        let d = run_detect(&sys, &tables, &Reporter::silent()).unwrap();
        assert_eq!(d.install_dir, dir);
        assert!(!d.components.is_empty());
        // 只读：组件仍在原位
        assert!(
            dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe")
                .is_file()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
