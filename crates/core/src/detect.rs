//! ① 探测层
//!
//! 安装目录定位顺序：
//!   (a) 进程镜像路径反查   ← 最可靠
//!   (b) 注册表 InstallDir  ← 该键在部分机器上不存在
//!   (c) 候选路径扫描       ← 兜底

use std::path::{Path, PathBuf};

use crate::data::Tables;
use crate::error::{Error, Result};
use crate::model::{Component, ComponentId, ComponentKind, Detection, RegItem};
use crate::sid;
use crate::sys::SysContext;

pub fn detect(sys: &SysContext, tables: &Tables) -> Result<Detection> {
    let target_user_sid = sid::resolve_target_user_sid(sys)?;
    let install_dir = locate_install_dir(sys, tables, &target_user_sid)?;

    if !install_dir.join(&tables.paths.marker).exists() {
        return Err(Error::InstallNotFound);
    }

    let components = scan_components(sys, tables, &install_dir);
    let registry_items = scan_registry(tables, &target_user_sid);

    Ok(Detection {
        target_user_sid,
        components,
        registry_items,
        install_dir,
    })
}

// ───────────────────────────── 安装目录定位 ─────────────────────────────

/// 三路定位，返回第一个通过校验的结果
pub fn locate_install_dir(sys: &SysContext, tables: &Tables, sid: &str) -> Result<PathBuf> {
    let marker = &tables.paths.marker;

    // (a) 首选：运行中的 BaiduNetdisk.exe 的镜像路径
    if let Some(dir) = via_process_image_path(sys, marker) {
        return Ok(dir);
    }

    // (b) 次选：注册表 InstallDir（部分机器上没有该键）
    if let Some(dir) = via_registry(sys, tables, sid) {
        return Ok(dir);
    }

    // (c) 兜底：候选路径扫描
    if let Some(dir) = via_candidates(tables) {
        return Ok(dir);
    }

    Err(Error::InstallNotFound)
}

fn via_process_image_path(sys: &SysContext, marker: &str) -> Option<PathBuf> {
    let procs = sys.process.list().ok()?;
    for p in procs.iter().filter(|p| p.name.eq_ignore_ascii_case(marker)) {
        if let Some(img) = sys.process.image_path(p.pid).ok().flatten() {
            if let Some(dir) = img.parent() {
                return Some(dir.to_path_buf());
            }
        }
    }
    None
}

fn via_registry(sys: &SysContext, tables: &Tables, sid: &str) -> Option<PathBuf> {
    let rv = &tables.paths.install_dir_registry_value;
    let path = crate::model::RegPath::user_software(sid).join(&rv.sub);
    let raw = sys.registry.read_string(&path, &rv.value).ok().flatten()?;
    if raw.trim().is_empty() {
        return None;
    }
    let dir = PathBuf::from(expand_env(&raw));
    dir.join(&tables.paths.marker).exists().then_some(dir)
}

fn via_candidates(tables: &Tables) -> Option<PathBuf> {
    for c in &tables.paths.candidates {
        let dir = PathBuf::from(expand_env(c));
        if dir.join(&tables.paths.marker).exists() {
            return Some(dir);
        }
    }
    None
}

/// 展开 `%VAR%` 形式的环境变量
pub fn expand_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '%' {
            if let Some(end) = bytes[i + 1..].iter().position(|&c| c == '%') {
                let name: String = bytes[i + 1..i + 1 + end].iter().collect();
                match std::env::var(&name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        // 未定义则原样保留
                        out.push('%');
                        out.push_str(&name);
                        out.push('%');
                    }
                }
                i += end + 2;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

// ───────────────────────────── 组件扫描 ─────────────────────────────

pub fn scan_components(sys: &SysContext, tables: &Tables, install_dir: &Path) -> Vec<Component> {
    let mut out = Vec::new();

    for id in ComponentId::ALL {
        let (risk, default_enabled) = id.default_risk();

        if id == ComponentId::RegistryAssoc {
            out.push(Component {
                id,
                label: id.label().to_owned(),
                detail: format!("约 {} 个扩展名", tables.extensions.count()),
                kind: ComponentKind::Registry,
                risk,
                default_enabled,
                size_bytes: None,
                // 注册表项的「存在与否」在探测阶段单独判定，这里恒为「存在」
                paths: vec![PathBuf::new()],
            });
            continue;
        }

        // 收集实际存在的路径；一个都不存在时也保留该条目，
        // 让界面能显示「已清理」而不是让条目凭空消失
        let mut paths = Vec::new();
        let mut size = 0u64;
        for rel in id.relative_paths() {
            let full = install_dir.join(rel);
            if sys.fs.exists(&full) {
                size += sys.fs.size_of(&full).unwrap_or(0);
                paths.push(full);
            }
        }

        let kind = if paths.first().map(|p| sys.fs.is_dir(p)).unwrap_or(false) {
            ComponentKind::Directory
        } else {
            ComponentKind::File
        };

        // 副标题只显示大小：路径等实现细节对用户没有信息量，
        // 需要时可以看日志或文档，不必挤在勾选行上。
        let detail = fmt_size(size);

        out.push(Component {
            id,
            label: id.label().to_owned(),
            detail,
            kind,
            risk,
            default_enabled,
            size_bytes: if paths.is_empty() { None } else { Some(size) },
            paths,
        });
    }
    out
}

fn fmt_size(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if b as f64 >= MB {
        format!("（{:.0} MB）", b as f64 / MB)
    } else {
        format!("（{:.0} KB）", b as f64 / 1024.0)
    }
}

// ───────────────────────────── 关联扫描 ─────────────────────────────

/// 两层都扫：`HKU\<SID>_Classes`（百度的关联键写在这里）+ `HKLM\Software\Classes`（防御性）
///
/// 本函数只负责「列出候选」，不判断是否存在 —— 过滤交给 [`existing_registry_items`]。
pub fn scan_registry(tables: &Tables, sid: &str) -> Vec<RegItem> {
    let mut items = tables.build_user_registry_items(sid);
    items.extend(tables.build_machine_registry_items());

    // 扩展名维度：OpenWithList 条目 + UserChoice 子键
    for ext in tables.extensions.all() {
        for entry in &tables.registry.open_with_list_entries {
            items.push(RegItem {
                path: tables.open_with_list_path(sid, ext).join(entry),
                value: None,
            });
        }
        let uc = tables.user_choice_path(sid, ext);
        items.push(RegItem {
            path: uc,
            value: None,
        });
    }

    items
}

/// 过滤出「实际存在」的项，避免 UI 展示一堆空项
pub fn existing_registry_items(sys: &SysContext, items: &[RegItem]) -> Vec<RegItem> {
    items
        .iter()
        .filter(|i| match &i.value {
            Some(v) => sys
                .registry
                .read_string(&i.path, v)
                .ok()
                .flatten()
                .is_some(),
            None => sys.registry.key_exists(&i.path).unwrap_or(false),
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Risk;
    use crate::sys::SysContext;
    use crate::sys::mock::{FakeProcesses, InMemoryRegistry, NoopShell};

    fn ctx(reg: InMemoryRegistry, proc: FakeProcesses) -> SysContext {
        SysContext {
            registry: Box::new(reg),
            process: Box::new(proc),
            fs: Box::new(crate::sys::fs::Win32Fs::new()),
            shell: Box::new(NoopShell::default()),
        }
    }

    const SID: &str = "S-1-5-21-1-2-3-1001";

    #[test]
    fn env_expansion() {
        // 用一个必然存在的变量，避免调用 set_var（edition 2024 中该函数为 unsafe）
        let root = std::env::var("SystemRoot").expect("Windows 上 SystemRoot 应存在");
        assert_eq!(
            expand_env("%SystemRoot%\\System32"),
            format!("{root}\\System32")
        );
        assert_eq!(expand_env(r"plain\path"), r"plain\path");
        // 未定义的变量原样保留，不吞掉
        assert_eq!(
            expand_env(r"%BCTEST_UNSET_VAR_XYZ%\sub"),
            r"%BCTEST_UNSET_VAR_XYZ%\sub"
        );
    }

    #[test]
    fn process_image_path_is_preferred_over_registry() {
        let tmp = std::env::temp_dir().join("bc_detect_proc_first");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("BaiduNetdisk.exe"), b"x").unwrap();

        let reg = InMemoryRegistry::new();
        let proc = FakeProcesses::new();
        // 注册表指向另一个（存在的）目录
        let other = std::env::temp_dir().join("bc_detect_reg_dir");
        let _ = std::fs::create_dir_all(&other);
        std::fs::write(other.join("BaiduNetdisk.exe"), b"x").unwrap();
        reg.set_value(
            &crate::model::RegPath::user_software(SID).join(r"Software\Baidu\BaiduNetdisk"),
            "InstallDir",
            &other.to_string_lossy(),
        );
        // 进程指向 tmp
        proc.add("BaiduNetdisk.exe", 100, Some(tmp.join("BaiduNetdisk.exe")));

        let tables = Tables::load().unwrap();
        let sys = ctx(reg, proc);
        let found = locate_install_dir(&sys, &tables, SID).unwrap();
        assert_eq!(found, tmp, "进程镜像路径应优先于注册表");

        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn registry_used_when_process_absent() {
        let dir = std::env::temp_dir().join("bc_detect_reg_only");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("BaiduNetdisk.exe"), b"x").unwrap();

        let reg = InMemoryRegistry::new();
        reg.set_value(
            &crate::model::RegPath::user_software(SID).join(r"Software\Baidu\BaiduNetdisk"),
            "InstallDir",
            &dir.to_string_lossy(),
        );
        let tables = Tables::load().unwrap();
        let sys = ctx(reg, FakeProcesses::new());
        assert_eq!(locate_install_dir(&sys, &tables, SID).unwrap(), dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_result_rejected_if_marker_missing() {
        let reg = InMemoryRegistry::new();
        reg.set_value(
            &crate::model::RegPath::user_software(SID).join(r"Software\Baidu\BaiduNetdisk"),
            "InstallDir",
            r"Z:\definitely\not\here",
        );
        let tables = Tables::load().unwrap();
        let sys = ctx(reg, FakeProcesses::new());

        // 注册表指向不存在的目录 → 该值必须被拒绝。
        // 本机可能真有候选路径命中（那是对的），所以不断言整体失败，
        // 只断言「绝不采用这个无效的注册表路径」。
        let invalid = PathBuf::from(r"Z:\definitely\not\here");
        if let Ok(dir) = locate_install_dir(&sys, &tables, SID) {
            assert_ne!(dir, invalid, "无效的注册表路径不得被采用");
        }
    }

    #[test]
    fn registry_scan_covers_both_layers_and_marks_caution() {
        let tables = Tables::load().unwrap();
        let reg = InMemoryRegistry::new();
        let sys = ctx(reg, FakeProcesses::new());

        // 造一个假的安装目录，写入各组件的文件（含两个已移出清理目标的）
        let dir = std::env::temp_dir().join("bc_scan_components");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"module\ImageViewer")).unwrap();
        std::fs::create_dir_all(dir.join(r"module\BrowserEngine\resources")).unwrap();
        std::fs::create_dir_all(dir.join(r"module\VastPlayer")).unwrap();
        std::fs::write(
            dir.join(r"module\ImageViewer\BaiduNetdiskImageViewer.exe"),
            b"x",
        )
        .unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe"),
            b"x",
        )
        .unwrap();
        std::fs::write(
            dir.join(r"module\BrowserEngine\resources\video_player.asar"),
            b"x",
        )
        .unwrap();
        std::fs::write(dir.join(r"module\VastPlayer\vastplayer.dll"), b"x").unwrap();

        let comps = scan_components(&sys, &tables, &dir);
        // 所有组件条目都会出现，不存在的也保留，便于界面显示「已清理」
        assert_eq!(comps.len(), ComponentId::ALL.len());

        let find = |id: ComponentId| comps.iter().find(|c| c.id == id).unwrap();

        // 智能看图与播放器启动器：安全，默认勾选
        for id in [ComponentId::ImageViewer, ComponentId::PlayerLauncher] {
            assert_eq!(find(id).risk, Risk::Safe, "{id:?} 应为安全项");
            assert!(find(id).default_enabled, "{id:?} 默认应勾选");
            assert!(find(id).present(), "{id:?} 在假目录里存在");
        }

        // 播放器代码包与引擎 DLL 已从组件列表移除（会导致网盘无法启动），
        // 因此组件只剩安全的三项
        assert_eq!(comps.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_items_include_both_hives_and_userchoice() {
        let tables = Tables::load().unwrap();
        let items = scan_registry(&tables, SID);

        assert!(
            items
                .iter()
                .any(|i| i.path.sub.starts_with(&format!("{SID}_Classes"))),
            "应含 HKU\\<SID>_Classes 层"
        );
        assert!(
            items
                .iter()
                .any(|i| i.path.sub.starts_with(r"Software\Classes")),
            "应含 HKLM 层（防御性）"
        );
        assert!(
            items.iter().any(|i| i.path.sub.contains("UserChoice")),
            "应含 UserChoice"
        );
        // 扩展名维度应产生大量项
        assert!(items.len() > 100, "项数应超过 100，实际 {}", items.len());
    }
}
