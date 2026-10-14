//! ③ 清理层 —— 结束进程、断开关联、移动文件、刷新、校验
//!
//! 顺序不可颠倒：先解除文件占用，再断关联，最后移文件。

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::backup::{Journal, Quarantine};
use crate::data::Tables;
use crate::detect;
use crate::error::{Error, Result};
use crate::guard;
use crate::model::{Action, ComponentId, RegItem};
use crate::progress::Reporter;
use crate::sys::SysContext;
use crate::sys::process::ProcessInfo;

/// 清理参数。超时可覆盖，测试时置 0 即可跳过等待。
#[derive(Debug, Clone)]
pub struct CleanOptions {
    pub graceful_timeout_ms: u64,
}

impl CleanOptions {
    pub fn from_tables(tables: &Tables) -> Self {
        Self {
            graceful_timeout_ms: tables.processes.graceful_timeout_ms,
        }
    }
}

/// 结束目标进程
///
/// 向全部目标一次性发出关闭请求，然后在同一个超时窗口内轮询：
/// 播放器可能同时存在多个实例，逐个等待会显著拉长时间。
pub fn kill_processes(
    sys: &SysContext,
    tables: &Tables,
    opts: &CleanOptions,
    journal: &mut Journal,
    rep: &Reporter,
) -> Result<()> {
    let targets: Vec<ProcessInfo> = sys
        .process
        .list()?
        .into_iter()
        .filter(|p| tables.processes.is_target(&p.name))
        .collect();

    if targets.is_empty() {
        rep.info("没有需要结束的进程");
        return Ok(());
    }
    rep.info(format!("发现 {} 个目标进程", targets.len()));

    // 防御性分支：若某个目标其实是系统服务，先停用，
    // 否则终止后可能被恢复策略重新拉起，在清理途中重新占用文件。
    if tables.processes.check_services {
        for t in &targets {
            let stem = t.name.trim_end_matches(".exe");
            if sys.process.is_service(stem).unwrap_or(false) {
                rep.warn(format!("{stem} 是系统服务，先停止并禁用"));
                let _ = sys.process.stop_service(stem);
                let _ = sys.process.disable_service(stem);
            }
        }
    }

    for t in &targets {
        match sys.process.request_close(t.pid) {
            Ok(true) => {}
            Ok(false) => rep.info(format!(
                "{} 是后台进程，无法请求它自行退出，将直接结束",
                t.name
            )),
            Err(e) => rep.warn(format!("向 {} 发送关闭请求失败：{e}", t.name)),
        }
    }

    let deadline = Instant::now() + Duration::from_millis(opts.graceful_timeout_ms);
    let mut remaining;
    loop {
        remaining = still_alive(sys, &targets)?;
        if remaining.is_empty() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let mut forced = 0usize;
    for t in &remaining {
        sys.process.terminate(t.pid)?;
        journal.record(Action::ProcessKilled {
            name: t.name.clone(),
            pid: t.pid,
            forced: true,
        })?;
        forced += 1;
        rep.warn(format!("强制结束 {}", t.name));
    }

    let graceful = targets.len() - forced;
    if graceful > 0 {
        rep.success(format!("{graceful} 个进程已退出"));
    }
    Ok(())
}

fn still_alive(sys: &SysContext, targets: &[ProcessInfo]) -> Result<Vec<ProcessInfo>> {
    let pids: HashSet<u32> = sys.process.list()?.into_iter().map(|p| p.pid).collect();
    Ok(targets
        .iter()
        .filter(|t| pids.contains(&t.pid))
        .cloned()
        .collect())
}

/// 当前正在运行的目标进程名，去重后按名称排序。
///
/// 界面用它提示用户先手动退出网盘：强制结束会被网盘记成异常退出。
pub fn running_targets(sys: &SysContext, tables: &Tables) -> Vec<String> {
    let mut names: Vec<String> = sys
        .process
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| tables.processes.is_target(&p.name))
        .map(|p| p.name)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// 断开关联。先导出再删除，顺序不能反。
///
/// 只处理实际存在的项——探测阶段会列出全部候选，多数并不存在。
pub fn break_associations(
    sys: &SysContext,
    items: &[RegItem],
    q: &Quarantine,
    journal: &mut Journal,
    rep: &Reporter,
) -> Result<()> {
    let mut cleared = 0usize;
    for item in items {
        let exists = match &item.value {
            Some(v) => sys
                .registry
                .read_string(&item.path, v)
                .ok()
                .flatten()
                .is_some(),
            None => sys.registry.key_exists(&item.path).unwrap_or(false),
        };
        if !exists {
            continue;
        }
        // 导出失败则不删——绝不在没有备份的情况下删除，宁可跳过
        let exported = match q.export(sys, item) {
            Ok(p) => p,
            Err(e) => {
                rep.warn(format!("备份 {} 失败，跳过：{e}", item.path.full()));
                journal.failures += 1;
                continue;
            }
        };
        let deleted = match &item.value {
            None => sys.registry.delete_key(&item.path),
            Some(v) => sys.registry.delete_value(&item.path, v),
        };
        match deleted {
            Ok(()) => {
                journal.record(reg_action(item, exported))?;
                cleared += 1;
            }
            Err(e) => {
                // 单项删除失败（如受保护的键）不致命：记录并继续
                rep.warn(format!("清理 {} 失败，跳过：{e}", item.path.full()));
                journal.failures += 1;
            }
        }
    }
    rep.success(format!("已清理 {cleared} 项注册表关联"));
    Ok(())
}

/// 根据 item 是「删键」还是「删值」构造对应的动作记录
fn reg_action(item: &RegItem, exported: std::path::PathBuf) -> Action {
    match &item.value {
        None => Action::RegKeyDeleted {
            hive: item.path.root_name().to_owned(),
            key: item.path.sub.clone(),
            exported_to: exported,
        },
        Some(v) => Action::RegValueDeleted {
            hive: item.path.root_name().to_owned(),
            key: item.path.sub.clone(),
            value: v.clone(),
            exported_to: exported,
        },
    }
}

/// 把选中的组件移入隔离区。每个目标都要先通过禁区校验。
pub fn move_files(
    sys: &SysContext,
    tables: &Tables,
    install_dir: &Path,
    selected: &[ComponentId],
    q: &Quarantine,
    journal: &mut Journal,
    rep: &Reporter,
) -> Result<()> {
    for id in selected {
        // 与探测层共用同一次目标展开，界面列出的和这里搬走的必定是同一批路径
        for rel in detect::component_rels(install_dir, *id) {
            let rel = rel.as_str();
            let target = install_dir.join(rel);

            // 「目录内容」型组件：逐个移走目录里的条目，保留目录本身
            if id.moves_dir_contents() {
                if !sys.fs.is_dir(&target) {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(&target) else {
                    continue;
                };
                for e in entries.flatten() {
                    let child_rel = format!("{rel}\\{}", e.file_name().to_string_lossy());
                    let child = install_dir.join(&child_rel);
                    // 禁区命中是致命错误：直接返回错误，不做任何删除
                    guard::assert_not_forbidden(&tables.forbidden, install_dir, &child)?;
                    if !sys.fs.exists(&child) {
                        continue;
                    }
                    move_one(sys, q, journal, &child_rel, rep)?;
                }
                rep.success(format!("已清空 {rel} 的内容（保留目录本身）"));
                continue;
            }

            guard::assert_not_forbidden(&tables.forbidden, install_dir, &target)?;
            if !sys.fs.exists(&target) {
                continue;
            }
            // 原位置已被占位物堵住说明该组件此前已清理过。搬走占位物没有意义：
            // 加固层随后会在同一位置重建占位物，隔离区里只会多出一份无用的文件。
            if detect::is_placeholder(sys, tables, install_dir, rel) {
                rep.info(format!("{rel} 已被占位物堵住，跳过"));
                continue;
            }
            move_one(sys, q, journal, rel, rep)?;
        }
    }
    Ok(())
}

/// 移动单个目标。移动本身失败（被占用等）**不致命**——记录并继续；
/// 只有 manifest 落盘失败才向上传播（动作已入内存清单，会被回滚）。
fn move_one(
    sys: &SysContext,
    q: &Quarantine,
    journal: &mut Journal,
    rel: &str,
    rep: &Reporter,
) -> Result<()> {
    match q.move_into(sys, rel) {
        Ok((action, bytes)) => {
            journal.bytes_reclaimed += bytes;
            journal.record(action)?;
            rep.success(format!("已移入隔离区：{rel}"));
        }
        Err(e) => {
            rep.warn(format!("移动失败，跳过：{rel}：{e}"));
            journal.failures += 1;
        }
    }
    Ok(())
}

/// 让关联改动立即生效
pub fn refresh(sys: &SysContext) {
    sys.shell.notify_assoc_changed();
}

/// 收集清理开始前实际存在的必需文件，作为 [`verify`] 的基线。
///
/// 清理前就缺失的必需文件不进基线：缺失源于版本差异或用户手动处理，
/// 与本次清理无关；把它们也纳入校验会让这类机器上的任何清理都以回滚收场。
/// 基线必须在任何改动发生之前收集。
pub fn required_files_present(
    sys: &SysContext,
    tables: &Tables,
    install_dir: &Path,
) -> Vec<String> {
    tables
        .paths
        .required_files
        .iter()
        .filter(|rel| sys.fs.exists(&install_dir.join(rel)))
        .cloned()
        .collect()
}

/// 校验基线中的必需文件全部仍然存在，任一缺失即返回错误，由编排层触发回滚。
///
/// 判据清单定义在 `data/paths.json` 的 `required_files`。
pub fn verify(sys: &SysContext, install_dir: &Path, baseline: &[String]) -> Result<()> {
    for rel in baseline {
        if !sys.fs.exists(&install_dir.join(rel)) {
            return Err(Error::ClientIntegrityBroken(rel.clone()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::Journal;
    use crate::model::{Manifest, RegPath, RegRoot};
    use crate::sys::SysContext;
    use crate::sys::mock::{FakeProcesses, InMemoryRegistry, NoopShell};

    fn journal() -> Journal {
        Journal::memory(Manifest {
            version: "t".into(),
            started_at: "t".into(),
            finished_at: None,
            install_dir: std::path::PathBuf::new(),
            quarantine_dir: std::path::PathBuf::new(),
            target_user_sid: String::new(),
            actions: Vec::new(),
        })
    }

    fn sys_with(reg: InMemoryRegistry, proc: FakeProcesses) -> SysContext {
        SysContext {
            registry: Box::new(reg),
            process: Box::new(proc),
            fs: Box::new(crate::sys::fs::Win32Fs::new()),
            shell: Box::new(NoopShell::default()),
        }
    }

    #[test]
    fn kill_uses_exact_whitelist_and_terminates() {
        let tables = Tables::load().unwrap();
        let proc = FakeProcesses::new();
        proc.add("BaiduNetdisk.exe", 100, None);
        proc.add("BaiduYunGuanjia.exe", 200, None); // 另一个产品，不得误杀
        proc.add("explorer.exe", 300, None);

        let sys = sys_with(InMemoryRegistry::new(), proc);
        let opts = CleanOptions {
            graceful_timeout_ms: 0, // 不等待，直接进入强制终止
        };
        let mut j = journal();
        kill_processes(&sys, &tables, &opts, &mut j, &Reporter::silent()).unwrap();
        let actions = j.actions();

        assert_eq!(actions.len(), 1, "只应结束白名单内的进程");
        match &actions[0] {
            Action::ProcessKilled { name, pid, .. } => {
                assert_eq!(name, "BaiduNetdisk.exe");
                assert_eq!(*pid, 100);
            }
            other => panic!("意外动作: {other:?}"),
        }
    }

    #[test]
    fn break_associations_skips_missing_and_exports_existing() {
        let tmp = std::env::temp_dir().join("bc_clean_reg");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let reg = InMemoryRegistry::new();
        let sid = "S-1-5-21-1-2-3-1001";
        let existing = RegPath::user_classes(sid).join("BaiduNetdiskPlayerAssociations");
        let missing = RegPath::user_classes(sid).join("BaiduNetdiskImageViewerAssociations");
        reg.add_key(existing.clone());

        let sys = sys_with(reg, FakeProcesses::new());
        let q = Quarantine::new(&tmp, std::path::Path::new("C:\\x"), "t");

        let items = vec![
            RegItem {
                path: existing.clone(),
                value: None,
            },
            RegItem {
                path: missing.clone(),
                value: None,
            },
        ];
        let mut j = journal();
        break_associations(&sys, &items, &q, &mut j, &Reporter::silent()).unwrap();

        assert_eq!(j.actions().len(), 1, "只应处理存在的项");
        assert!(
            !sys.registry.key_exists(&existing).unwrap(),
            "存在的键应被删除"
        );
        assert!(!sys.registry.key_exists(&missing).unwrap());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn move_files_moves_dir_contents_but_keeps_dir() {
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_clean_contents");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"module\ImageViewer\subdir")).unwrap();
        std::fs::write(dir.join(r"module\ImageViewer\Qt5Core.dll"), b"qt").unwrap();
        std::fs::write(dir.join(r"module\ImageViewer\subdir\plugin.dll"), b"p").unwrap();

        let sys = sys_with(InMemoryRegistry::new(), FakeProcesses::new());
        let q = Quarantine::new(&dir, &dir, "t");

        let selected = vec![ComponentId::ImageViewer];
        let mut j = journal();
        move_files(
            &sys,
            &tables,
            &dir,
            &selected,
            &q,
            &mut j,
            &Reporter::silent(),
        )
        .unwrap();

        // 顶层条目：Qt5Core.dll 与 subdir，各算一个动作（子目录整体移动）
        assert_eq!(j.actions().len(), 2, "顶层条目数");

        // 目录本身必须保留 —— 这是「清空内容、保留目录」的关键承诺
        assert!(dir.join(r"module\ImageViewer").is_dir());
        assert!(!dir.join(r"module\ImageViewer\Qt5Core.dll").exists());
        assert!(!dir.join(r"module\ImageViewer\subdir").exists());

        // 隔离区里能找回全部内容
        assert!(
            q.files_dir()
                .join(r"module\ImageViewer\Qt5Core.dll")
                .is_file()
        );
        assert!(
            q.files_dir()
                .join(r"module\ImageViewer\subdir\plugin.dll")
                .is_file()
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(q.dir);
    }

    #[test]
    fn move_files_rejects_forbidden_targets() {
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_clean_forbidden");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("module")).unwrap();
        // 伪造一个禁区文件
        std::fs::write(dir.join("BaiduNetdisk.exe"), b"x").unwrap();

        // 直接构造一个指向禁区的目标
        let rel = "BaiduNetdisk.exe";
        let target = dir.join(rel);
        assert!(matches!(
            guard::assert_not_forbidden(&tables.forbidden, &dir, &target),
            Err(Error::Forbidden(_))
        ));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_files_skips_paths_blocked_by_placeholder() {
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_clean_placeholder_skip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 启动器位置已被占位目录堵住（上一次清理 + 加固的结果）
        let launcher = dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe");
        std::fs::create_dir_all(&launcher).unwrap();
        // 旧版引擎本体仍在
        std::fs::create_dir_all(dir.join(r"module\VastPlayer")).unwrap();
        std::fs::write(dir.join(r"module\VastPlayer\vastplayer.dll"), b"vpold").unwrap();

        let sys = sys_with(InMemoryRegistry::new(), FakeProcesses::new());
        let q = Quarantine::new(&dir, &dir, "t");
        let selected = vec![ComponentId::Player];
        let mut j = journal();
        move_files(
            &sys,
            &tables,
            &dir,
            &selected,
            &q,
            &mut j,
            &Reporter::silent(),
        )
        .unwrap();

        // 只应搬走旧版引擎本体；占位目录留在原地
        assert_eq!(j.actions().len(), 1, "占位物不应被搬走");
        match &j.actions()[0] {
            Action::FileMoved { from, .. } => {
                assert!(
                    from.ends_with("vastplayer.dll"),
                    "意外目标: {}",
                    from.display()
                );
            }
            other => panic!("意外动作: {other:?}"),
        }
        assert!(launcher.is_dir(), "占位目录必须留在原地");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(q.dir);
    }

    #[test]
    fn running_targets_filters_to_whitelist() {
        let tables = Tables::load().unwrap();
        let proc = FakeProcesses::new();
        proc.add("BaiduNetdisk.exe", 100, None);
        proc.add("BaiduNetdisk.exe", 101, None); // 同名多实例应合并为一项
        proc.add("explorer.exe", 300, None);

        let sys = sys_with(InMemoryRegistry::new(), proc);
        assert_eq!(running_targets(&sys, &tables), vec!["BaiduNetdisk.exe"]);
    }

    #[test]
    fn verify_only_holds_cleanup_accountable_for_existing_files() {
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_clean_verify");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"module\BrowserEngine\resources")).unwrap();

        let sys = sys_with(InMemoryRegistry::new(), FakeProcesses::new());

        // 必需文件在清理开始前就全部缺失 → 基线为空，清理对它们不负责
        let baseline = required_files_present(&sys, &tables, &dir);
        assert!(baseline.is_empty());
        assert!(verify(&sys, &dir, &baseline).is_ok());

        // 文件齐全 → 全部进入基线，校验通过
        for rel in &tables.paths.required_files {
            std::fs::write(dir.join(rel), b"x").unwrap();
        }
        let baseline = required_files_present(&sys, &tables, &dir);
        assert_eq!(baseline.len(), tables.paths.required_files.len());
        assert!(verify(&sys, &dir, &baseline).is_ok());

        // 清理弄丢了基线中的 vastplayer.dll → 必须报错：
        // 它是实测确认的启动必需文件，缺失会让客户端反复启动失败
        std::fs::remove_file(dir.join(r"module\BrowserEngine\vastplayer.dll")).unwrap();
        assert!(matches!(
            verify(&sys, &dir, &baseline),
            Err(Error::ClientIntegrityBroken(rel)) if rel.ends_with("vastplayer.dll")
        ));

        // 清理前就缺失的文件不进基线 → 它的缺失不触发回滚
        let baseline = required_files_present(&sys, &tables, &dir);
        assert!(!baseline.iter().any(|r| r.ends_with("vastplayer.dll")));
        assert!(verify(&sys, &dir, &baseline).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn action_variants_round_trip_through_json() {
        let a = Action::RegValueDeleted {
            hive: "HKU".into(),
            key: r"Software\RegisteredApplications".into(),
            value: "BaiduNetdiskPlayer".into(),
            exported_to: std::path::PathBuf::from(r"C:\q\a.reg"),
        };
        let s = serde_json::to_string(&a).unwrap();
        let back: Action = serde_json::from_str(&s).unwrap();
        match back {
            Action::RegValueDeleted { value, .. } => assert_eq!(value, "BaiduNetdiskPlayer"),
            other => panic!("往返后类型变了: {other:?}"),
        }
    }

    #[test]
    fn reg_root_name_is_stable() {
        assert_eq!(RegRoot::Hku, RegRoot::Hku);
        let p = RegPath::hklm("Software\\Classes");
        assert_eq!(p.root_name(), "HKLM");
    }
}
