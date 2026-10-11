//! 禁区保护 —— 每次删除前必经的闸门
//!
//! 主客户端与播放器共享 `module\BrowserEngine`，误删会导致网盘无法启动。
//! 本模块采用「精确匹配 + 前缀禁区 + 例外白名单」三层判定。

use std::path::Path;

use crate::data::ForbiddenTable;
use crate::error::{Error, Result};

fn normalize(s: &str) -> String {
    s.replace('/', "\\").trim_matches('\\').to_ascii_lowercase()
}

/// 相对安装目录的路径是否被禁区保护
pub fn is_forbidden(t: &ForbiddenTable, relative: &str) -> bool {
    let norm = normalize(relative);

    // 1) 例外优先：精确命中例外白名单的路径即使在禁区前缀之内也放行。
    for e in &t.shared_dir_exceptions {
        if normalize(e) == norm {
            return false;
        }
    }

    // 2) 精确匹配 + 前缀禁区（目标是禁区路径的后代）
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

/// 删除前的硬闸门：目标必须位于安装目录内，且不得命中禁区
pub fn assert_not_forbidden(t: &ForbiddenTable, install_dir: &Path, target: &Path) -> Result<()> {
    let Ok(rel) = target.strip_prefix(install_dir) else {
        // 不在安装目录内 —— 一律拒绝（防止路径穿越/误配置）
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
        // 例外列表为空 → resources 子树整体受保护，无一豁免
        assert!(is_forbidden(&t, r"module\BrowserEngine\resources\app.asar"));
        assert!(is_forbidden(
            &t,
            r"module\BrowserEngine\resources\anything.asar"
        ));
        assert!(is_forbidden(
            &t,
            r"module\BrowserEngine\resources\video_player.asar"
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
