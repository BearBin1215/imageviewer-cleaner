//! ④ 加固层 —— 阻止组件被重新部署
//!
//! 客户端会检查自己的模块清单，发现缺失就重新下载。占位法利用文件系统的命名约束：
//! Windows 不允许同名的文件与目录共存，因此客户端的部署流程无法重建目标。
//!
//! 这一手段与权限设置无关，所以比修改访问控制列表更可靠——
//! 以 SYSTEM 身份运行的进程可以取得所有权并改写访问控制列表，但绕不过命名约束。

use std::path::Path;

use crate::backup::Journal;
use crate::error::Result;
use crate::model::{Action, ComponentId, PlaceholderKind};
use crate::sys::SysContext;

/// 占位清单
///
/// 真正起作用的是堵住播放器启动器：没有它，播放器即使代码包被重新下载也无法被调用。
pub const PLACEHOLDER_TARGETS: &[(ComponentId, PlaceholderKind)] = &[
    // 原本是文件 → 放一个同名目录
    (
        ComponentId::PlayerLauncher,
        PlaceholderKind::DirInsteadOfFile,
    ),
];

/// 建立占位物。目标仍存在时跳过，避免破坏现场。
pub fn apply(sys: &SysContext, install_dir: &Path, journal: &mut Journal) -> Result<()> {
    for (id, kind) in PLACEHOLDER_TARGETS {
        let Some(rel) = id.relative_paths().first().copied() else {
            continue;
        };
        let path = install_dir.join(rel);
        if sys.fs.exists(&path) {
            continue;
        }
        match kind {
            PlaceholderKind::FileInsteadOfDir => sys.fs.create_placeholder_file(&path)?,
            PlaceholderKind::DirInsteadOfFile => sys.fs.create_placeholder_dir(&path)?,
        }
        journal.record(Action::PlaceholderCreated { path, kind: *kind })?;
    }
    Ok(())
}

/// 按 manifest 记录移除占位物。还原时先执行这一步，否则文件移不回去。
pub fn remove_recorded(sys: &SysContext, actions: &[Action]) -> Result<usize> {
    let mut n = 0;
    for a in actions {
        if let Action::PlaceholderCreated { path, .. } = a {
            sys.fs.remove_path(path)?;
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Manifest;
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

    #[test]
    fn placeholder_covers_only_player_launcher() {
        assert_eq!(PLACEHOLDER_TARGETS.len(), 1);
        assert!(
            PLACEHOLDER_TARGETS
                .iter()
                .any(|(id, _)| *id == ComponentId::PlayerLauncher)
        );
    }

    #[test]
    fn apply_creates_placeholder_with_opposite_kind() {
        let sys = ctx();
        let dir = std::env::temp_dir().join("bc_harden_apply");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut j = journal();
        apply(&sys, &dir, &mut j).unwrap();
        let actions: Vec<Action> = j.actions().to_vec();
        assert_eq!(actions.len(), 1, "启动器是唯一的占位目标");

        // 文件位置放的是目录
        let launcher = dir.join(r"module\BrowserEngine\BaiduNetdiskPlayerLaunch.exe");
        assert!(launcher.is_dir(), "启动器位置应是目录而非文件");

        // 再跑一次应幂等（目标已存在则跳过）
        let mut j2 = journal();
        apply(&sys, &dir, &mut j2).unwrap();
        assert!(j2.actions().is_empty(), "已存在占位物时不应重复创建");

        // 移除后可以重建
        let n = remove_recorded(&sys, &actions).unwrap();
        assert_eq!(n, 1);
        assert!(!launcher.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
