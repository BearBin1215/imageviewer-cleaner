//! 编排层 —— 按顺序执行各层，上报进度，任一步失败则回滚已完成的部分
//!
//! 全部操作跑在调用方的 worker 线程上，进度通过 [`Reporter`] 送回界面。

use std::path::{Path, PathBuf};

use crate::backup::{self, BackupInfo, Journal, Quarantine};
use crate::clean::{self, CleanOptions};
use crate::data::Tables;
use crate::detect;
use crate::error::{Error, Result};
use crate::harden;
use crate::model::{Action, ComponentId, Detection, Manifest};
use crate::progress::{Progress, Reporter, Stage, Summary};
use crate::restore;
use crate::sys::SysContext;

/// 清理配置
#[derive(Debug, Clone)]
pub struct CleanupConfig {
    /// 用户勾选的组件（各组件互相独立，见 `model::Component`）
    pub selected: Vec<ComponentId>,
    /// 是否建立占位物阻止重新部署。
    ///
    /// 默认开启：占位法不会导致客户端启动失败，是阻断组件被重新下载部署的
    /// 主要手段。若个别环境下仍出现异常，可关闭本项，或清理后用「还原」恢复。
    pub harden: bool,
    /// 覆盖隔离区根目录。默认取安装目录所在卷下的 `imageviewer-cleaner\quarantine`。
    /// 测试用它指向临时目录。
    pub quarantine_root: Option<PathBuf>,
    /// 是否把隔离区目录的权限收紧为「仅管理员可访问」。默认开启，见
    /// [`backup::Quarantine::prepare`]。测试置 false：收紧之后非提权的测试进程
    /// 连自己写进去的 manifest 都读不到。
    pub restrict_quarantine_acl: bool,
}

impl CleanupConfig {
    pub fn new(selected: Vec<ComponentId>) -> Self {
        Self {
            selected,
            harden: true,
            quarantine_root: None,
            restrict_quarantine_acl: true,
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

    /// 关闭隔离区权限收紧（仅测试使用）
    pub fn without_acl_restriction(mut self) -> Self {
        self.restrict_quarantine_acl = false;
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
    let q = Quarantine::new(&root, &d.install_dir, &backup::local_timestamp())
        .with_acl_restriction(cfg.restrict_quarantine_acl);
    let manifest = Manifest {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        started_at: backup::local_timestamp(),
        finished_at: None,
        install_dir: d.install_dir.clone(),
        quarantine_dir: q.dir.clone(),
        target_user_sid: d.target_user_sid.clone(),
        actions: Vec::new(),
    };
    let mut journal = Journal::start(sys, &q, manifest, rep)?;
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
            let undone = rollback(sys, tables, &d, &q, journal.actions(), rep);
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
    // 基线在任何改动之前收集；收尾校验只对「清理前存在、清理后消失」的
    // 必需文件报错
    let baseline = clean::required_files_present(sys, tables, &d.install_dir);

    rep.stage(Stage::KillProcesses);
    let opts = CleanOptions::from_tables(tables);
    clean::kill_processes(sys, tables, &opts, journal, rep)?;

    rep.stage(Stage::BreakAssoc);
    // 必须按勾选过滤：注册表项没有文件路径，不像文件组件那样能靠 selected 里的
    // relative_paths 自然过滤，漏掉这一步会让「取消勾选文件关联」形同虚设。
    // 关联项清单在探测阶段已按存在性过滤，直接使用。
    if cfg.selected.contains(&ComponentId::RegistryAssoc) {
        let items = &d.registry_items;
        rep.info(format!("待处理关联项 {} 条", items.len()));
        clean::break_associations(sys, items, q, journal, rep)?;
    } else {
        rep.info("未勾选文件关联，跳过");
    }

    rep.stage(Stage::MoveFiles);
    clean::move_files(sys, tables, &d.install_dir, &cfg.selected, q, journal, rep)?;

    // 加固：默认开启，阻断组件被重新部署。
    rep.stage(Stage::Harden);
    if cfg.harden {
        let before = journal.actions().len();
        harden::apply(sys, tables, &d.install_dir, &cfg.selected, journal)?;
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
    clean::verify(sys, &d.install_dir, &baseline)?;
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
///
/// 动作来自本次运行的内存日志，边界同样由探测结果给出：不因为是自己写的就跳过
/// 校验，两处使用同一套校验，避免回滚比还原更宽松。
fn rollback(
    sys: &SysContext,
    tables: &Tables,
    d: &Detection,
    q: &Quarantine,
    actions: &[Action],
    rep: &Reporter,
) -> usize {
    let scope = restore::RestoreScope::new(tables, &d.target_user_sid, &d.install_dir, q);
    let report = restore::undo_actions(sys, actions, &scope, rep);
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

/// 从指定备份还原
///
/// 安装目录以**探测结果**为准，不用 manifest 自报的值：manifest 是磁盘上的
/// 输入，可能被替换或篡改。
pub fn run_restore(
    sys: &SysContext,
    tables: &Tables,
    quarantine_dir: &Path,
    rep: &Reporter,
) -> Result<()> {
    rep.stage(Stage::Backup);
    let q = Quarantine::at(quarantine_dir);
    let m = q.read_manifest()?;
    let Some(install_dir) = resolve_install_dir(sys, tables, &m, rep) else {
        return Err(Error::InstallNotFound);
    };

    rep.info(format!("从 {} 还原", q.dir.display()));
    // 目标用户取 manifest 记录值：它只决定关联键候选按哪个用户的展开来匹配，
    // 导入范围随后仍被限定在数据表声明的百度关联键内（见 RestoreScope::sid）
    let scope = restore::RestoreScope::new(tables, &m.target_user_sid, &install_dir, &q);
    let (_, report) = restore::restore(sys, &q, &scope, rep)?;

    if !report.registry_failed.is_empty() {
        rep.warn(format!(
            "{} 个注册表项未能导入，系统会重新询问默认程序",
            report.registry_failed.len()
        ));
    }
    rep.success("还原完成");
    Ok(())
}

/// 还原时使用的安装目录：优先用探测结果；探测不到（客户端已卸载）才退回
/// manifest 记录的位置，并要求 marker 文件仍在——manifest 单独不足以充当边界。
fn resolve_install_dir(
    sys: &SysContext,
    tables: &Tables,
    m: &Manifest,
    rep: &Reporter,
) -> Option<PathBuf> {
    if let Ok(d) = detect::locate_install_dir(sys, tables, &m.target_user_sid) {
        return Some(d);
    }
    let fallback = m.install_dir.clone();
    if fallback.join(&tables.paths.marker).exists() {
        rep.warn(format!(
            "未能探测到安装目录，改用备份记录的位置：{}",
            fallback.display()
        ));
        return Some(fallback);
    }
    rep.warn("既探测不到安装目录，备份记录的位置也没有 marker 文件，拒绝还原");
    None
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
/// 旧版本把备份放在安装目录的同级目录，那里的存量备份也要继续列出。
pub fn locate_backups(sys: &SysContext, tables: &Tables, sid: &str) -> Result<Vec<BackupInfo>> {
    let install_dir = detect::locate_install_dir(sys, tables, sid)?;
    let mut list = backup::list_backups(&backup::default_root(&install_dir)?);
    if let Some(old) = backup::legacy_root(&install_dir) {
        for info in backup::list_backups(&old) {
            if !list.iter().any(|b| b.dir == info.dir) {
                list.push(info);
            }
        }
    }
    list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    Ok(list)
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
        // 播放器引擎三件套的其余两项
        std::fs::write(dir.join(r"module\BrowserEngine\vastplayer.dll"), b"vp").unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\resources\BaiduNetdiskPlayer.ico"),
            b"ico",
        )
        .unwrap();
        // 与播放器共用、必须原封不动的依赖
        std::fs::write(dir.join(r"module\BrowserEngine\videoipc.dll"), b"ipc").unwrap();
        std::fs::write(dir.join(r"module\BrowserEngine\localplayer.dll"), b"lp").unwrap();
        std::fs::write(dir.join(r"module\VastPlayer\vastplayer.dll"), b"vpold").unwrap();
        // 清理目标：旧目录的 D3D 运行库
        std::fs::write(dir.join(r"module\VastPlayer\d3dx9_43.dll"), b"d3d").unwrap();
        // 清理目标：引擎更新备份（通配命中）。引擎本体在同目录，不得被移走
        std::fs::write(
            dir.join(r"module\BrowserEngine\vastplayer.dll.old20250825T082343"),
            b"old1",
        )
        .unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\vastplayer.dll.old20260608T084445"),
            b"old2",
        )
        .unwrap();
        // 清理目标：播放器代码包已失效的暂存下载。已生效的那份在 resources 下，属禁区
        std::fs::create_dir_all(dir.join(r"module\BrowserEngine\module\asar")).unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\module\asar\video_player.asar.new"),
            b"staged",
        )
        .unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\module\asar\video_player.asar.sig"),
            b"sig",
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

        // 显式启用加固，以便覆盖占位物的建立与还原；全选，覆盖所有组件
        let cfg = CleanupConfig::new(ComponentId::ALL.to_vec())
            .with_harden(true)
            .with_quarantine_root(qroot.clone())
            .without_acl_restriction();

        let summary = run_cleanup(&sys, &tables, &cfg, &Reporter::silent()).unwrap();
        // 全部组件都被清理：看图目录内容 2 项 + 启动器 + 旧版引擎 + D3D 运行库
        // + 通配命中的更新备份 2 项 + 暂存下载包 2 项
        assert_eq!(summary.files_moved, 9, "应移走全部目标");
        // 两个文件型组件需要占位；目录本身保留，不需要占位
        assert_eq!(summary.placeholders_created, 2);
        assert!(summary.registry_items_cleared > 0);

        // 目录本身必须还在，只是内容被清空
        assert!(
            dir.join(r"module\ImageViewer").is_dir(),
            "组件目录本身必须保留，否则客户端会检测到模块缺失"
        );
        assert!(!dir.join(r"module\ImageViewer\Qt5Core.dll").exists());

        // 组件已不在原位，原位置被占位物堵住
        assert!(
            !dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe")
                .exists()
        );
        assert!(
            dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe")
                .is_dir()
        );
        assert!(dir.join(r"module\VastPlayer\vastplayer.dll").is_dir());

        // 新增的清理目标也已被移走
        assert!(!dir.join(r"module\VastPlayer\d3dx9_43.dll").exists());
        assert!(
            !dir.join(r"module\BrowserEngine\vastplayer.dll.old20250825T082343")
                .exists()
        );
        assert!(
            !dir.join(r"module\BrowserEngine\module\asar\video_player.asar.new")
                .exists()
        );

        // 禁区与启动必需文件原封不动。
        // 其中 `module\BrowserEngine\vastplayer.dll` 的文件名正好是通配前缀
        // `vastplayer.dll.old` 的开头——它还在，证明展开没有把正在被加载的引擎一并移走。
        for keep in [
            "BaiduNetdisk.exe",
            r"module\BrowserEngine\BaiduNetdiskUnite.exe",
            r"module\BrowserEngine\resources\app.asar",
            r"module\BrowserEngine\resources\video_player.asar",
            r"module\BrowserEngine\vastplayer.dll",
            r"module\BrowserEngine\localplayer.dll",
            r"module\BrowserEngine\videoipc.dll",
        ] {
            assert!(dir.join(keep).is_file(), "{keep} 不应被动到");
        }

        // 找到隔离区并还原
        let qdir = std::fs::read_dir(&qroot)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.is_dir())
            .expect("应存在隔离区目录");

        run_restore(&sys, &tables, &qdir, &Reporter::silent()).unwrap();

        // 组件回到原位
        assert!(
            dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe")
                .is_file()
        );
        assert!(
            dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe")
                .is_file(),
            "还原应把占位目录换回原文件"
        );
        assert!(dir.join(r"module\VastPlayer\vastplayer.dll").is_file());

        // 通配目标按展开后的原名还原：名字里带时间戳，不能靠固定清单对应回来
        assert!(
            dir.join(r"module\BrowserEngine\vastplayer.dll.old20250825T082343")
                .is_file()
        );
        assert!(
            dir.join(r"module\BrowserEngine\vastplayer.dll.old20260608T084445")
                .is_file()
        );
        assert!(
            dir.join(r"module\BrowserEngine\module\asar\video_player.asar.new")
                .is_file()
        );
        assert!(dir.join(r"module\VastPlayer\d3dx9_43.dll").is_file());

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&qroot);
    }

    /// 必需文件在清理开始前就缺失（版本差异或用户手动处理）时，
    /// 清理应当照常完成——缺失与本次清理无关，校验只对「从有到无」负责。
    #[test]
    fn cleanup_succeeds_when_required_file_already_missing() {
        let (sys, dir) = fake_env("bc_orch_missing_required");
        let tables = Tables::load().unwrap();
        let qroot = std::env::temp_dir().join("bc_orch_missing_required_q");
        let _ = std::fs::remove_dir_all(&qroot);

        // 预先删掉一个必需文件，模拟非基线版本的安装现场
        std::fs::remove_file(dir.join(r"module\BrowserEngine\vastplayer.dll")).unwrap();

        let cfg = CleanupConfig::new(vec![ComponentId::ImageViewer])
            .with_quarantine_root(qroot.clone())
            .without_acl_restriction();

        let summary = run_cleanup(&sys, &tables, &cfg, &Reporter::silent()).unwrap();
        assert_eq!(summary.files_moved, 2, "看图目录内容应照常被清理");
        assert!(
            !dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe")
                .exists(),
            "清理前就缺失的必需文件不应阻塞清理"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&qroot);
    }

    /// 任何一步失败时，已完成的动作必须全部回滚。
    ///
    /// 这里用「占位目标的父目录被同名文件占用」让加固阶段报错：
    /// 此前旧版引擎已被移入隔离区，回滚后必须回到原位。
    #[test]
    fn cleanup_rolls_back_when_a_step_fails() {
        let (sys, dir) = fake_env("bc_orch_rollback");
        let tables = Tables::load().unwrap();
        let qroot = std::env::temp_dir().join("bc_orch_rollback_q");
        let _ = std::fs::remove_dir_all(&qroot);

        // 把 module\BrowserEngine 换成同名文件：加固层无法在它下面创建占位目录
        let _ = std::fs::remove_dir_all(dir.join(r"module\BrowserEngine"));
        std::fs::write(dir.join(r"module\BrowserEngine"), b"x").unwrap();

        let cfg = CleanupConfig::new(vec![ComponentId::Player])
            .with_quarantine_root(qroot.clone())
            .without_acl_restriction();

        let err = run_cleanup(&sys, &tables, &cfg, &Reporter::silent()).unwrap_err();
        assert!(
            !matches!(err, crate::error::Error::ClientIntegrityBroken(_)),
            "失败应来自加固阶段，而非完整性校验，实际 {err:?}"
        );

        // 回滚之后旧版引擎应回到原位
        assert!(
            dir.join(r"module\VastPlayer\vastplayer.dll").is_file(),
            "回滚后组件应已恢复"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&qroot);
    }

    #[test]
    fn unselected_components_are_left_untouched() {
        let (sys, dir) = fake_env("bc_orch_partial");
        let tables = Tables::load().unwrap();
        let qroot = std::env::temp_dir().join("bc_orch_partial_q");
        let _ = std::fs::remove_dir_all(&qroot);

        // 只勾播放器：看图与文件关联都不勾
        let cfg = CleanupConfig::new(vec![ComponentId::Player])
            .with_harden(true)
            .with_quarantine_root(qroot.clone())
            .without_acl_restriction();

        let summary = run_cleanup(&sys, &tables, &cfg, &Reporter::silent()).unwrap();
        // 播放器一项就覆盖五个固定目标加两个通配命中的更新备份
        assert_eq!(summary.files_moved, 7, "应动播放器组件的全部目标");
        assert_eq!(
            summary.registry_items_cleared, 0,
            "未勾选文件关联时不得清理任何关联项"
        );
        // 只有会被重新部署的两个入口点需要占位
        assert_eq!(summary.placeholders_created, 2);

        // 看图原封不动，也没被占位物堵住
        assert!(
            dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe")
                .is_file()
        );
        assert!(dir.join(r"module\ImageViewer\Qt5Core.dll").is_file());

        // 播放器入口点已被移走并占位
        assert!(
            dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe")
                .is_dir()
        );
        assert!(dir.join(r"module\VastPlayer\vastplayer.dll").is_dir());
        // 不占位的那几个也已被移走
        assert!(!dir.join(r"module\VastPlayer\d3dx9_43.dll").exists());
        assert!(
            !dir.join(r"module\BrowserEngine\module\asar\video_player.asar.new")
                .exists()
        );
        assert!(
            !dir.join(r"module\BrowserEngine\vastplayer.dll.old20260608T084445")
                .exists()
        );
        // 但正在被加载的引擎本体原样保留
        assert!(dir.join(r"module\BrowserEngine\vastplayer.dll").is_file());

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
