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
        // 插件宿主正在加载的 D3D9 运行时，且位于会被当作 DLL 搜索目录的 VastPlayer 下
        assert!(is_forbidden(&t, r"module\VastPlayer\d3dx9_43.dll"));
        // 推断列入：与 vastplayer.dll 同处 browserengine.dll 的启动校验清单
        assert!(is_forbidden(&t, r"module\BrowserEngine\localplayer.dll"));
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
        }
    }

    /// 不变量：占位目标也必须能通过闸门（占位物同样写在安装目录里）。
    #[test]
    fn every_placeholder_target_passes_the_gate() {
        use crate::model::ComponentId;

        let t = table();
        let install = PathBuf::from(r"D:\Program Files\BaiduNetdisk");
        for target in &Tables::load().unwrap().paths.placeholder_targets {
            let rel = target
                .component
                .relative_paths()
                .first()
                .copied()
                .expect("占位目标必须有相对路径");
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
