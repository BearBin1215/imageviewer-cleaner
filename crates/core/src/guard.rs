//! 禁区保护 —— 每次删除前的强制校验
//!
//! 主客户端与播放器共享 `module\BrowserEngine`，误删会导致网盘无法启动。
//!
//! 命中禁区即返回错误，调用方中止整次清理并回滚。当前没有任何组件目标命中
//! 禁区表，因此正常运行中不会触发；它防的是「把禁区文件误加进组件目标」这类
//! 错误在任何删除发生之前被拦下。
//! 禁区表本身见 `data/forbidden.json`，里面记着每条的理由。

use std::path::Path;

use crate::data::ForbiddenTable;
use crate::error::{Error, Result};
use crate::sys::SysContext;

fn normalize(s: &str) -> String {
    s.replace('/', "\\").trim_matches('\\').to_ascii_lowercase()
}

/// 相对安装目录的路径是否被禁区保护
///
/// 两层判定：精确匹配，以及前缀禁区（目标是禁区路径的后代）。
/// 没有例外清单——需要放行某个文件时，正确做法是不把它写进禁区表。
pub fn is_forbidden(t: &ForbiddenTable, relative: &str) -> bool {
    let norm = normalize(relative);

    for f in &t.relative_paths {
        let f = normalize(f);
        if norm == f {
            return true;
        }
        if norm.len() > f.len() && norm.starts_with(&format!("{f}\\")) {
            return true;
        }
    }
    false
}

/// 删除前的强制校验：目标必须位于安装目录内，且不得命中禁区
pub fn assert_not_forbidden(t: &ForbiddenTable, install_dir: &Path, target: &Path) -> Result<()> {
    let Ok(rel) = target.strip_prefix(install_dir) else {
        // 不在安装目录内时一律拒绝，防止路径穿越或误配置
        return Err(Error::Forbidden(target.to_path_buf()));
    };
    if is_forbidden(t, &rel.to_string_lossy()) {
        return Err(Error::Forbidden(target.to_path_buf()));
    }
    Ok(())
}

/// 删除前的第二道校验：物理位置必须仍位于安装目录内。
///
/// [`assert_not_forbidden`] 只做词法比较，而目录联接（junction）能让一个
/// 「安装目录内」的路径实际指向别处——Windows 上创建联接**不需要任何特权**，
/// 因此这不是理论问题：把 `module\ImageViewer` 做成指向 `C:\Windows` 的联接，
/// 词法闸门会照常放行，清理层却会把联接目标里的内容整体搬走。
///
/// 判定规则：安装目录本身允许是联接（装在别处再联通过来是常见做法），
/// 但它**之下**的任何一段都不允许是重解析点。命中即视为异常现场，
/// 与禁区一样中止整次清理，不静默跳过。
pub fn assert_not_redirected(sys: &SysContext, install_dir: &Path, target: &Path) -> Result<()> {
    if sys.fs.resolves_within(install_dir, target) {
        Ok(())
    } else {
        Err(Error::Redirected(target.to_path_buf()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::Tables;
    use std::path::PathBuf;

    fn table() -> ForbiddenTable {
        Tables::load().unwrap().forbidden
    }

    #[test]
    fn every_declared_forbidden_path_is_rejected() {
        let t = table();
        for p in &t.relative_paths {
            assert!(is_forbidden(&t, p), "应被拦截: {p}");
        }
    }

    #[test]
    fn clean_targets_are_allowed() {
        let t = table();
        assert!(!is_forbidden(&t, r"module\ImageViewer"));
        assert!(!is_forbidden(
            &t,
            r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe"
        ));
        assert!(!is_forbidden(&t, r"module\VastPlayer\vastplayer.dll"));
        // 更新备份：带 .old<时间戳> 后缀的目标不是禁区文件本体，通配展开能命中它，也能删除它
        assert!(!is_forbidden(
            &t,
            r"module\BrowserEngine\vastplayer.dll.old20260608T084445"
        ));
        // asar 暂存目录不在 resources 子树内
        assert!(!is_forbidden(
            &t,
            r"module\BrowserEngine\module\asar\video_player.asar.new"
        ));
    }

    #[test]
    fn player_shared_deps_stay_forbidden() {
        let t = table();
        // 启动必需文件（依据见 forbidden.json 的 _comment_vastplayer）：
        // 客户端启动时会无条件拉起 plugin_id=3 的插件宿主加载它
        assert!(is_forbidden(&t, r"module\BrowserEngine\vastplayer.dll"));
        // 主客户端进程已加载的 IPC 层
        assert!(is_forbidden(&t, r"module\BrowserEngine\videoipc.dll"));
        // 插件系统宿主
        assert!(is_forbidden(&t, r"module\BrowserEngine\dls.dll"));
        // 推断列入：与 vastplayer.dll 同处 browserengine.dll 的启动校验清单
        assert!(is_forbidden(&t, r"module\BrowserEngine\localplayer.dll"));
    }

    /// `module\VastPlayer\d3dx9_43.dll` 已从禁区移出，是播放器组件的清理目标。
    ///
    /// 它确实被宿主加载着（按模块完整路径确认），但实测确认移除后宿主会回落到
    /// `C:\Windows\System32` 下的同一份副本，客户端照常启动。这条断言把「移出」
    /// 固化下来：如果日后新版本不再回落，正确做法是把它加回 `data/forbidden.json`
    /// 并从 `ComponentId::Player` 的目标里删掉——同一个文件不能同时登记在两边，
    /// 那样清理会被禁区拦截，整次中止。
    #[test]
    fn legacy_player_d3d_is_a_target_not_a_forbidden_path() {
        use crate::model::ComponentId;

        let t = table();
        assert!(
            !is_forbidden(&t, r"module\VastPlayer\d3dx9_43.dll"),
            "d3dx9_43.dll 仍在禁区里，无法作为清理目标"
        );
        assert!(
            ComponentId::Player
                .relative_paths()
                .contains(&r"module\VastPlayer\d3dx9_43.dll"),
            "d3dx9_43.dll 应登记在播放器组件的目标清单里"
        );
    }

    /// 不变量：任何组件声明的清理目标都不得是客户端启动必需文件。
    ///
    /// `required_files` 是清理后的完整性判据；一个文件同时出现在两边，意味着
    /// 清理必然触发回滚。这条断言把「实测学到的启动必需文件」固化为测试期
    /// 约束：把它误加回组件目标时，测试立刻失败。
    #[test]
    fn no_component_target_is_a_required_client_file() {
        use crate::model::ComponentId;

        let tables = Tables::load().unwrap();
        for id in ComponentId::ALL {
            for rel in id.relative_paths() {
                assert!(
                    !tables
                        .paths
                        .required_files
                        .iter()
                        .any(|r| r.eq_ignore_ascii_case(rel)),
                    "{id:?} 的清理目标 {rel} 是客户端启动必需文件，不得列入组件"
                );
            }
            for pt in id.prefix_targets() {
                // 通配前缀一旦短到能覆盖某个必需文件的文件名，展开就会把它一起删掉。
                // 例如前缀写成 `vastplayer.dll` 会命中正在被加载的引擎本体。
                for req in &tables.paths.required_files {
                    // 根目录下的必需文件不可能出现在任何通配目录中
                    let Some((req_dir, req_name)) = req.rsplit_once('\\') else {
                        continue;
                    };
                    let same_dir = req_dir.eq_ignore_ascii_case(pt.dir.trim_end_matches('\\'));
                    let covered = req_name
                        .to_ascii_lowercase()
                        .starts_with(&pt.prefix.to_ascii_lowercase());
                    assert!(
                        !(same_dir && covered),
                        "{id:?} 的通配前缀 {}\\{} 会匹配到启动必需文件 {req}",
                        pt.dir,
                        pt.prefix
                    );
                }
            }
        }
    }

    #[test]
    fn main_client_deps_are_forbidden() {
        let t = table();
        assert!(is_forbidden(&t, "BaiduNetdisk.exe"));
        assert!(is_forbidden(
            &t,
            r"module\BrowserEngine\BaiduNetdiskUnite.exe"
        ));
        assert!(is_forbidden(&t, r"module\BrowserEngine\resources\app.asar"));
        assert!(is_forbidden(
            &t,
            r"module\BrowserEngine\resources\core.asar"
        ));
    }

    #[test]
    fn prefix_ban_covers_subtree() {
        let t = table();
        assert!(is_forbidden(&t, r"module\AiEngine\model\foo.onnx"));
        assert!(is_forbidden(&t, r"module\SpeechSDK\bin\x.dll"));
    }

    #[test]
    fn resources_subtree_fully_forbidden() {
        let t = table();
        // 例外列表为空 → resources 子树整体受保护，无一豁免。
        // 实测确认：清掉其中的 video_player.asar 会让客户端反复启动失败。
        assert!(is_forbidden(&t, r"module\BrowserEngine\resources\app.asar"));
        assert!(is_forbidden(
            &t,
            r"module\BrowserEngine\resources\core.asar"
        ));
        assert!(is_forbidden(
            &t,
            r"module\BrowserEngine\resources\video_player.asar"
        ));
        assert!(is_forbidden(
            &t,
            r"module\BrowserEngine\resources\BaiduNetdiskPlayer.ico"
        ));
        assert!(is_forbidden(
            &t,
            r"module\BrowserEngine\resources\anything.asar"
        ));
    }

    #[test]
    fn path_separator_and_case_insensitive() {
        let t = table();
        assert!(is_forbidden(&t, "MODULE/AIENGINE/x"));
        assert!(is_forbidden(&t, r"\module\AiEngine"));
    }

    #[test]
    fn targets_outside_install_dir_are_rejected() {
        let t = table();
        let install = PathBuf::from(r"D:\Program Files\BaiduNetdisk");
        let outside = PathBuf::from(r"C:\Windows\System32\calc.exe");
        assert!(matches!(
            assert_not_forbidden(&t, &install, &outside),
            Err(Error::Forbidden(_))
        ));
    }

    /// 不变量：任何组件声明的清理目标都必须能通过禁区闸门。
    ///
    /// 新增组件或给禁区表加条目时，这条断言会在测试阶段立刻暴露冲突，
    /// 而不是等到用户机器上才因 `Error::Forbidden` 中断整次清理。
    /// 通配目标按其展开后的形态一并校验：目录本身、`dir\prefix`、
    /// `dir\prefix + 时间戳` 都必须能通过。
    #[test]
    fn every_declared_component_target_passes_the_gate() {
        use crate::model::ComponentId;

        let t = table();
        let install = PathBuf::from(r"D:\Program Files\BaiduNetdisk");
        for id in ComponentId::ALL {
            for rel in id.relative_paths() {
                assert!(
                    assert_not_forbidden(&t, &install, &install.join(rel)).is_ok(),
                    "{id:?} 的清理目标 {rel} 被禁区拦截"
                );
            }
            for pt in id.prefix_targets() {
                for rel in [
                    pt.dir.to_owned(),
                    format!("{}\\{}", pt.dir, pt.prefix),
                    // 真实形态：前缀后面直接接时间戳
                    format!("{}\\{}20260608T084445", pt.dir, pt.prefix),
                ] {
                    assert!(
                        assert_not_forbidden(&t, &install, &install.join(&rel)).is_ok(),
                        "{id:?} 的通配目标 {rel} 被禁区拦截"
                    );
                }
            }
        }
    }

    /// 不变量：占位目标也必须能通过闸门（占位物同样写在安装目录里）。
    #[test]
    fn every_placeholder_target_passes_the_gate() {
        use crate::model::ComponentId;

        let t = table();
        let install = PathBuf::from(r"D:\Program Files\BaiduNetdisk");
        for target in &Tables::load().unwrap().paths.placeholder_targets {
            let rel = target.relative_path.as_str();
            assert!(
                assert_not_forbidden(&t, &install, &install.join(rel)).is_ok(),
                "{:?} 的占位目标 {rel} 被禁区拦截",
                target.component
            );
            assert!(
                !matches!(target.component, ComponentId::RegistryAssoc),
                "注册表组件没有文件路径，不能作为占位目标"
            );
        }
    }

    #[test]
    fn legit_target_inside_install_dir_passes_gate() {
        let t = table();
        let install = PathBuf::from(r"D:\Program Files\BaiduNetdisk");
        let target = install.join(r"module\ImageViewer");
        assert!(assert_not_forbidden(&t, &install, &target).is_ok());

        let bad = install.join(r"module\BrowserEngine\BaiduNetdiskUnite.exe");
        assert!(assert_not_forbidden(&t, &install, &bad).is_err());
    }
}
