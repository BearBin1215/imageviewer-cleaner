//! ① 探测层
//!
//! 安装目录按以下顺序定位，取第一个通过校验的结果：
//! 1. 运行中进程的镜像路径；
//! 2. 注册表 InstallDir（部分机器上没有该键）；
//! 3. 候选路径扫描（兜底）。

pub mod sid;

use std::path::{Path, PathBuf};

use crate::data::Tables;
use crate::error::{Error, Result};
use crate::model::{Component, ComponentId, Detection, PlaceholderKind, RegItem};
use crate::sys::SysContext;

pub fn detect(sys: &SysContext, tables: &Tables) -> Result<Detection> {
    let target_user_sid = sid::resolve_target_user_sid(sys)?;
    let install_dir = locate_install_dir(sys, tables, &target_user_sid)?;

    if !install_dir.join(&tables.paths.marker).exists() {
        return Err(Error::InstallNotFound);
    }

    // 关联项在探测阶段按存在性过滤一次，结果存入 Detection；
    // 展示与清理直接复用，不再各自重查一遍注册表
    let candidates = scan_registry(tables, &target_user_sid);
    let registry_items = existing_registry_items(sys, &candidates);
    let components = scan_components(sys, tables, &install_dir, &registry_items);

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

/// 展开 `%VAR%` 形式的环境变量。未定义的变量按 Windows 的约定原样保留。
pub fn expand_env(s: &str) -> String {
    use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows::core::PCWSTR;

    let src = crate::sys::wide(s);
    // 第一次调用只取所需长度（含结尾的 0）
    let need = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), None) };
    if need == 0 {
        return s.to_owned();
    }
    let mut buf = vec![0u16; need as usize];
    let written = unsafe { ExpandEnvironmentStringsW(PCWSTR(src.as_ptr()), Some(&mut buf)) };
    if written == 0 || written as usize > buf.len() {
        return s.to_owned();
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

// ───────────────────────────── 组件扫描 ─────────────────────────────

/// 组件声明的文件目标（相对安装目录），通配目标就地展开成实际命中的条目。
///
/// 探测与清理共用这一份展开结果：两处各写一遍匹配规则，会让界面显示的目标和
/// 真正被搬走的目标不一致。返回的路径不保证存在——固定目标由调用方按存在性
/// 过滤，通配展开本身只返回磁盘上有的条目。
pub fn component_rels(install_dir: &Path, id: ComponentId) -> Vec<String> {
    let mut out: Vec<String> = id.relative_paths().iter().map(|s| s.to_string()).collect();

    for pt in id.prefix_targets() {
        let Ok(entries) = std::fs::read_dir(install_dir.join(pt.dir)) else {
            continue;
        };
        let prefix = pt.prefix.to_ascii_lowercase();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_lowercase();
            if name.starts_with(&prefix) {
                out.push(format!("{}\\{}", pt.dir, e.file_name().to_string_lossy()));
            }
        }
    }

    out
}

/// 扫描组件清单。
///
/// `registry_items` 是探测阶段实际命中的关联项：注册表组件没有文件路径，
/// 它的存在与否和数量都由这份清单表达。
pub fn scan_components(
    sys: &SysContext,
    tables: &Tables,
    install_dir: &Path,
    registry_items: &[RegItem],
) -> Vec<Component> {
    let mut out = Vec::new();

    for id in ComponentId::ALL {
        if id == ComponentId::RegistryAssoc {
            out.push(Component {
                id,
                label: id.label().to_owned(),
                detail: format!("已发现 {} 项", registry_items.len()),
                default_enabled: id.default_enabled(),
                found: registry_items.len(),
            });
            continue;
        }

        // 收集实际存在的路径；一个都不存在时也保留该条目，
        // 使界面能显示「已清理」
        let mut paths = Vec::new();
        let mut size = 0u64;
        for rel in component_rels(install_dir, id) {
            let rel = rel.as_str();
            let full = install_dir.join(rel);
            // 占位物不是组件本体。把它算作「存在」会让界面把一个已被堵住的位置
            // 显示成待清理目标，重复清理时还会把占位目录搬进隔离区。
            if !sys.fs.exists(&full) || is_placeholder(sys, tables, install_dir, rel) {
                continue;
            }
            // 「清空目录内容」型组件的目录会被刻意保留，空目录等同于已清理；
            // 否则界面会长期挂着一个 0 KB 的待清理项
            if id.moves_dir_contents() && sys.fs.is_empty_dir(&full) {
                continue;
            }
            size += sys.fs.size_of(&full).unwrap_or(0);
            paths.push(full);
        }

        out.push(Component {
            id,
            label: id.label().to_owned(),
            // 副标题只显示大小：路径等实现细节对用户没有信息量
            detail: format!("（{}）", fmt_size(size)),
            default_enabled: id.default_enabled(),
            found: paths.len(),
        });
    }
    out
}

/// 把字节数转成人类可读的短文本（如 `156 MB`、`3 KB`）。
///
/// 探测详情与界面日志共用；调用方按需自行加括号等修饰。
pub fn fmt_size(b: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if b as f64 >= MB {
        format!("{:.0} MB", b as f64 / MB)
    } else {
        format!("{:.0} KB", b as f64 / 1024.0)
    }
}

/// 该相对路径当前是否是「占位物」。
///
/// 判据是类型与预期相反：声明为 `dir_instead_of_file` 的目标本应是文件，
/// 现在成了目录即视为占位物。据此可以区分「组件本体还在」与「组件已清理、
/// 原位置被占位堵住」——没有这层判断，重复清理会把占位物当成组件搬进隔离区。
/// 探测与清理共用这一个判定。
pub fn is_placeholder(
    sys: &SysContext,
    tables: &Tables,
    install_dir: &Path,
    relative: &str,
) -> bool {
    let Some(kind) = tables.paths.placeholder_kind_for(relative) else {
        return false;
    };
    let p = install_dir.join(relative);
    if !sys.fs.exists(&p) {
        return false;
    }
    match kind {
        PlaceholderKind::DirInsteadOfFile => sys.fs.is_dir(&p),
        PlaceholderKind::FileInsteadOfDir => !sys.fs.is_dir(&p),
    }
}

// ───────────────────────────── 关联扫描 ─────────────────────────────

/// 两层都扫：`HKU\<SID>_Classes`（百度的关联键写在这里）+ `HKLM\Software\Classes`（防御性）
///
/// 本函数只按数据表展开候选，不查注册表；存在性过滤在 [`existing_registry_items`]。
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

/// 过滤出实际存在的项。探测阶段调用一次，结果随 [`Detection`] 下发。
fn existing_registry_items(sys: &SysContext, items: &[RegItem]) -> Vec<RegItem> {
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
    fn scan_components_lists_all_and_marks_safe() {
        let tables = Tables::load().unwrap();
        let reg = InMemoryRegistry::new();
        let sys = ctx(reg, FakeProcesses::new());

        // 造一个假的安装目录，写入各组件的文件
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
        std::fs::write(dir.join(r"module\VastPlayer\vastplayer.dll"), b"x").unwrap();

        let comps = scan_components(&sys, &tables, &dir, &[]);
        // 所有组件条目都会出现，不存在的也保留，便于界面显示「已清理」
        assert_eq!(comps.len(), ComponentId::ALL.len());

        let find = |id: ComponentId| comps.iter().find(|c| c.id == id).unwrap();

        // 假目录里确实存在的那两项
        for id in [ComponentId::ImageViewer, ComponentId::Player] {
            assert!(find(id).present(), "{id:?} 在假目录里存在");
        }
        // 没有的那一个（关联项清单为空）不应显示为存在
        assert!(!find(ComponentId::RegistryAssoc).present());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholder_is_not_reported_as_present() {
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_detect_placeholder");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"module\BrowserEngine")).unwrap();
        // 启动器位置被占位目录堵住（上一次清理 + 加固的结果）
        std::fs::create_dir_all(dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe"))
            .unwrap();

        let sys = ctx(InMemoryRegistry::new(), FakeProcesses::new());
        let comps = scan_components(&sys, &tables, &dir, &[]);
        let launcher = comps.iter().find(|c| c.id == ComponentId::Player).unwrap();

        assert!(
            !launcher.present(),
            "占位物不应算作组件仍存在：唯一存在的那个目标已被占位目录取代"
        );
        assert_eq!(launcher.found, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emptied_component_dir_counts_as_cleaned() {
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_detect_emptied_dir");
        let _ = std::fs::remove_dir_all(&dir);
        // 目录本身被刻意保留，内容已清空
        std::fs::create_dir_all(dir.join(r"module\ImageViewer")).unwrap();

        let sys = ctx(InMemoryRegistry::new(), FakeProcesses::new());
        let comps = scan_components(&sys, &tables, &dir, &[]);
        let viewer = comps
            .iter()
            .find(|c| c.id == ComponentId::ImageViewer)
            .unwrap();
        assert!(
            !viewer.present(),
            "空目录应视为已清理，而不是一个 0 KB 的目标"
        );

        // 客户端重新部署后应再次被识别为存在
        std::fs::write(dir.join(r"module\ImageViewer\Qt5Core.dll"), b"qt").unwrap();
        let comps = scan_components(&sys, &tables, &dir, &[]);
        let viewer = comps
            .iter()
            .find(|c| c.id == ComponentId::ImageViewer)
            .unwrap();
        assert!(viewer.present(), "目录内有内容时应视为存在");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_component_reflects_actual_hit_count() {
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_detect_registry_count");
        let _ = std::fs::create_dir_all(&dir);
        let sys = ctx(InMemoryRegistry::new(), FakeProcesses::new());

        let items = (0..7)
            .map(|i| RegItem {
                path: crate::model::RegPath::hku(format!("S-x-{i}")),
                value: None,
            })
            .collect::<Vec<_>>();
        let find = |items: &[RegItem]| {
            scan_components(&sys, &tables, &dir, items)
                .into_iter()
                .find(|c| c.id == ComponentId::RegistryAssoc)
                .unwrap()
        };

        // 没有命中任何关联项 → 与已清理的文件组件一样显示「已清理」
        let none = find(&[]);
        assert!(!none.present(), "没有命中项时不应显示为存在");
        assert_eq!(none.found, 0);

        // 命中 7 项 → 存在，且副标题是命中数而不是候选清单长度
        let some = find(&items);
        assert!(some.present());
        assert_eq!(some.found, 7);
        assert_eq!(some.detail, "已发现 7 项");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn placeholder_detection_keys_on_opposite_type() {
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_data_placeholder");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"module\BrowserEngine")).unwrap();
        let sys = ctx(InMemoryRegistry::new(), FakeProcesses::new());

        // 启动器声明为 dir_instead_of_file：本该是文件
        let rel = r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe";
        assert!(
            !is_placeholder(&sys, &tables, &dir, rel),
            "目标不存在时不算占位物"
        );

        std::fs::write(dir.join(rel), b"x").unwrap();
        assert!(
            !is_placeholder(&sys, &tables, &dir, rel),
            "本体是文件时不算占位物"
        );

        std::fs::remove_file(dir.join(rel)).unwrap();
        std::fs::create_dir_all(dir.join(rel)).unwrap();
        assert!(
            is_placeholder(&sys, &tables, &dir, rel),
            "类型相反时才是占位物"
        );

        // 未声明为占位目标的路径一律不算占位物，哪怕它恰好是个目录
        assert!(!is_placeholder(&sys, &tables, &dir, r"module\ImageViewer"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_component_touches_player_resources_or_startup_files() {
        // 「不得触碰的文件」直接取自数据表（禁区表 + 启动必需清单），
        // 不在此维护第二份清单
        let tables = Tables::load().unwrap();
        for id in ComponentId::ALL {
            for rel in id.relative_paths() {
                assert!(
                    !crate::guard::is_forbidden(&tables.forbidden, rel),
                    "{id:?} 的目标 {rel} 属于禁区保护的文件"
                );
                assert!(
                    !tables
                        .paths
                        .required_files
                        .iter()
                        .any(|r| r.eq_ignore_ascii_case(rel)),
                    "{id:?} 的目标 {rel} 属于启动必需文件"
                );
            }
        }

        // 播放器合并的固定目标，逐个核对它们都在：`module\VastPlayer` 整个目录
        // 已不在下发清单里（条目数为 0），里面的两个文件都可以删；`resources` 子树
        // 依然完全不动
        assert_eq!(
            ComponentId::Player.relative_paths(),
            &[
                r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe",
                r"module\VastPlayer\vastplayer.dll",
                r"module\VastPlayer\d3dx9_43.dll",
                r"module\BrowserEngine\module\asar\video_player.asar.new",
                r"module\BrowserEngine\module\asar\video_player.asar.sig",
            ]
        );
        assert!(
            !ComponentId::Player.moves_dir_contents(),
            "播放器不得整目录清空：module\\VastPlayer 与 module\\BrowserEngine 都包含主客户端需要的文件"
        );
    }

    /// 通配目标就地展开成实际命中的条目，且展开结果不超出前缀的匹配范围。
    ///
    /// 这条测试检查的是展开规则本身：引擎本体 `vastplayer.dll` 必须留在结果之外，
    /// 而新增的备份文件不用改代码就能被纳入。
    #[test]
    fn prefix_targets_expand_to_matching_entries_only() {
        let dir = std::env::temp_dir().join("bc_detect_prefix");
        let _ = std::fs::remove_dir_all(&dir);
        let be = dir.join(r"module\BrowserEngine");
        std::fs::create_dir_all(&be).unwrap();

        // 禁区与启动必需的那个文件，加两个时间戳不同的备份
        std::fs::write(be.join("vastplayer.dll"), b"engine").unwrap();
        std::fs::write(be.join("vastplayer.dll.old20250825T082343"), b"a").unwrap();
        std::fs::write(be.join("vastplayer.dll.old20260608T084445"), b"bb").unwrap();
        // 不应命中的同目录条目：另一个必需 DLL，和一个仅大小写不同的同前缀条目
        std::fs::write(be.join("videoipc.dll"), b"c").unwrap();
        std::fs::write(be.join("VastPlayer.dll.old-x"), b"d").unwrap();

        let rels = component_rels(&dir, ComponentId::Player);
        // 五个固定目标 + 三个通配命中
        assert_eq!(rels.len(), 8, "展开结果：{rels:?}");
        assert!(
            !rels
                .iter()
                .any(|r| r.eq_ignore_ascii_case(r"module\BrowserEngine\vastplayer.dll")),
            "展开结果包含正在被加载的引擎本体：{rels:?}"
        );
        assert!(!rels.iter().any(|r| r.ends_with("videoipc.dll")));
        // 大小写不同的同前缀条目也要命中：Windows 路径不区分大小写
        assert!(rels
            .iter()
            .any(|r| r.ends_with("VastPlayer.dll.old-x") || r.ends_with("vastplayer.dll.old-x")));

        // 没有通配目标的组件，展开结果就是固定清单
        assert_eq!(
            component_rels(&dir, ComponentId::ImageViewer),
            vec![r"module\ImageViewer".to_owned()]
        );

        // 父目录不存在时不 panic：通配部分为空，只剩固定清单
        let empty = std::env::temp_dir().join("bc_detect_prefix_none");
        let _ = std::fs::remove_dir_all(&empty);
        assert_eq!(
            component_rels(&empty, ComponentId::Player).len(),
            ComponentId::Player.relative_paths().len(),
            "父目录缺失时应只返回固定清单"
        );

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
