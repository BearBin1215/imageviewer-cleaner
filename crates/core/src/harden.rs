//! ④ 加固层 —— 阻止组件被重新部署
//!
//! 客户端会检查自己的模块清单，发现缺失就重新下载。占位法利用文件系统的命名约束：
//! Windows 不允许同名的文件与目录共存，因此客户端的部署流程无法重建目标。
//!
//! 这一手段不依赖权限设置：以 SYSTEM 身份运行的进程可以取得所有权并改写
//! 访问控制列表，但绕不过同名的命名约束。

use std::path::Path;

use crate::backup::Journal;
use crate::data::Tables;
use crate::error::Result;
use crate::guard;
use crate::model::{Action, ComponentId, PlaceholderKind};
use crate::sys::SysContext;

/// 建立占位物。目标仍存在时跳过，避免破坏现场。
///
/// 占位清单来自 `data/paths.json` 的 `placeholder_targets`，每条点名一个具体路径。
/// 关键目标是堵住播放器启动器与旧版引擎这两个入口：没有它们，播放器即使代码包被重新
/// 下载也无法被调用。
///
/// 只处理 `selected` 里的组件：未勾选的组件即使文件已缺失也不得占位，
/// 否则「保留这个组件」的意愿会被加固逻辑覆盖。
pub fn apply(
    sys: &SysContext,
    tables: &Tables,
    install_dir: &Path,
    selected: &[ComponentId],
    journal: &mut Journal,
) -> Result<()> {
    for target in &tables.paths.placeholder_targets {
        if !selected.contains(&target.component) {
            continue;
        }
        let path = install_dir.join(&target.relative_path);
        if sys.fs.exists(&path) {
            continue;
        }
        // 占位物要写在安装目录里，路径同样不能经过重解析点，
        // 否则占位目录会建到别处，加固失效
        guard::assert_not_redirected(sys, install_dir, &path)?;
        match target.kind {
            PlaceholderKind::FileInsteadOfDir => sys.fs.create_placeholder_file(&path)?,
            PlaceholderKind::DirInsteadOfFile => sys.fs.create_placeholder_dir(&path)?,
        }
        journal.record(Action::PlaceholderCreated {
            path,
            kind: target.kind,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ComponentId, Manifest};
    use crate::sys::SysContext;
    use crate::sys::mock::NoopShell;

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

    fn ctx() -> SysContext {
        SysContext {
            registry: Box::new(crate::sys::mock::InMemoryRegistry::new()),
            process: Box::new(crate::sys::mock::FakeProcesses::new()),
            fs: Box::new(crate::sys::fs::Win32Fs::new()),
            shell: Box::new(NoopShell::default()),
        }
    }

    /// 占位清单必须点名到具体路径，而且只能点名该组件清理目标里的路径。
    ///
    /// 组件是按用户意图合并的（播放器一项有多个固定目标），「取第一个路径」这类
    /// 推断会占位到错误文件上。这里同时固化了「哪些目标已被实测确认可移除」：
    /// 新增占位目标要改这张表，就得先有实测依据。
    #[test]
    fn placeholder_targets_name_verified_paths() {
        let tables = Tables::load().unwrap();
        let targets = &tables.paths.placeholder_targets;

        let paths: Vec<&str> = targets.iter().map(|t| t.relative_path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe",
                r"module\VastPlayer\vastplayer.dll",
            ],
            "占位目标应只覆盖实测确认可移除且会被重新部署的入口"
        );

        for t in targets {
            assert!(
                t.component
                    .relative_paths()
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(&t.relative_path)),
                "{:?} 的占位路径 {} 不在它的清理目标里",
                t.component,
                t.relative_path
            );
            assert!(
                !t.component.moves_dir_contents(),
                "{:?} 是「清空目录内容」型组件，目录本身要保留，不该整目录占位",
                t.component
            );
        }

        // 同组件里不占位的那几个目标：旧目录的 D3D 运行库与暂存下载包
        let skip = [
            r"module\VastPlayer\d3dx9_43.dll",
            r"module\BrowserEngine\module\asar\video_player.asar.new",
            r"module\BrowserEngine\module\asar\video_player.asar.sig",
        ];
        for rel in skip {
            assert!(
                tables.paths.placeholder_kind_for(rel).is_none(),
                "{rel} 不该被列为占位目标"
            );
        }
    }

    #[test]
    fn apply_creates_placeholder_with_opposite_kind() {
        let sys = ctx();
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_harden_apply");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let selected = vec![ComponentId::Player];
        let mut j = journal();
        apply(&sys, &tables, &dir, &selected, &mut j).unwrap();
        let actions: Vec<Action> = j.actions().to_vec();
        assert_eq!(actions.len(), 2, "启动器与旧版引擎各占一个位");

        // 文件位置放的都是目录
        let launcher = dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe");
        assert!(launcher.is_dir(), "启动器位置应是目录");
        let legacy = dir.join(r"module\VastPlayer\vastplayer.dll");
        assert!(legacy.is_dir(), "旧版引擎位置应是目录");

        // 再跑一次应幂等（目标已存在则跳过）
        let mut j2 = journal();
        apply(&sys, &tables, &dir, &selected, &mut j2).unwrap();
        assert!(j2.actions().is_empty(), "已存在占位物时不应重复创建");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_skips_targets_that_still_exist() {
        let sys = ctx();
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_harden_existing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(r"module\BrowserEngine")).unwrap();

        // 启动器还在原位 → 不得被占位物覆盖，避免破坏现场
        let launcher = dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe");
        std::fs::write(&launcher, b"launch").unwrap();

        let selected = vec![ComponentId::Player];
        let mut j = journal();
        apply(&sys, &tables, &dir, &selected, &mut j).unwrap();
        assert!(launcher.is_file(), "仍存在的目标不得被占位物覆盖");
        // 只有缺了的旧版引擎会被占位
        assert_eq!(j.actions().len(), 1);
        assert!(dir.join(r"module\VastPlayer\vastplayer.dll").is_dir());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 用户取消勾选的组件，原位置即使已缺失也不得占位。
    ///
    /// 否则「只清播放器、保留看图」这类意愿会被加固逻辑覆盖掉。
    #[test]
    fn apply_skips_components_not_selected() {
        let sys = ctx();
        let tables = Tables::load().unwrap();
        let dir = std::env::temp_dir().join("bc_harden_unselected");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 只勾了看图：两个占位目标都必须跳过
        let selected = vec![ComponentId::ImageViewer];
        let mut j = journal();
        apply(&sys, &tables, &dir, &selected, &mut j).unwrap();
        assert!(j.actions().is_empty(), "未勾选的组件不得占位");
        assert!(
            !dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe")
                .exists(),
            "未勾选组件的原位置不得被占用"
        );
        assert!(
            !dir.join(r"module\VastPlayer\vastplayer.dll").exists(),
            "未勾选组件的原位置不得被占用"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
