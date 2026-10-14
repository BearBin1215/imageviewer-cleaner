//! `--scan` 无界面自检：探测一次并写入 `scan-output.txt`。
//!
//! 便于在没有交互桌面的环境里验证探测逻辑，也用于清理前后的对照记录。
//! 发布构建没有控制台，终端里不会打印提示，直接看文件即可。

use std::fmt::Write as _;

use imageviewer_cleaner_core::data::Tables;
use imageviewer_cleaner_core::detect;
use imageviewer_cleaner_core::sys::SysContext;

/// 执行自检并写报告文件。
pub fn run() {
    let mut out = String::new();
    let result = (|| {
        let tables = Tables::load()?;
        let sys = SysContext::win32();
        let d = detect::detect(&sys, &tables)?;
        Ok::<_, imageviewer_cleaner_core::Error>((tables, sys, d))
    })();

    match result {
        Ok((tables, sys, d)) => {
            let _ = writeln!(out, "安装目录 : {}", d.install_dir.display());
            let _ = writeln!(out, "目标用户 : {}", d.target_user_sid);
            let _ = writeln!(out, "\n组件清单 ({}):", d.components.len());

            for c in &d.components {
                let _ = writeln!(
                    out,
                    "  [{}] {:<14} {}",
                    if c.present() && c.default_enabled {
                        "x"
                    } else {
                        " "
                    },
                    c.label,
                    if c.present() {
                        c.detail.clone()
                    } else {
                        "已清理".to_owned()
                    }
                );
                // 与探测、清理共用同一次目标展开，通配目标在这里也是实际命中的条目
                for rel in detect::component_rels(&d.install_dir, c.id) {
                    let full = d.install_dir.join(&rel);
                    let state = if detect::is_placeholder(&sys, &tables, &d.install_dir, &rel) {
                        if full.is_dir() {
                            "占位目录"
                        } else {
                            "占位文件"
                        }
                    } else if full.is_dir() {
                        "目录"
                    } else if full.is_file() {
                        "文件"
                    } else {
                        "缺失"
                    };
                    let _ = writeln!(out, "        {state:<4} {rel}");
                }
            }

            // 关联项在探测阶段已按存在性过滤，这里只含实际命中的项
            let existing = &d.registry_items;
            let _ = writeln!(out, "\n注册表: 实际命中 {} 项", existing.len());
            for i in existing.iter().take(10) {
                let _ = writeln!(
                    out,
                    "  {}{}",
                    i.path.full(),
                    i.value
                        .as_ref()
                        .map(|v| format!("  [{v}]"))
                        .unwrap_or_default()
                );
            }
            if existing.is_empty() {
                let _ = writeln!(out, "  （没有残留的关联项）");
            }
        }
        Err(e) => {
            let _ = writeln!(out, "探测失败: {e}");
        }
    }

    // 优先写当前目录（终端里直接可见）；工作目录不可写时（例如提权后落在
    // Program Files）回退到 %TEMP%，避免发布构建在无控制台场景下静默失败
    let path = std::path::Path::new("scan-output.txt");
    match std::fs::write(path, &out) {
        Ok(()) => println!("已写入 {}", path.display()),
        Err(e) => {
            let fallback = std::env::temp_dir().join("imageviewer-cleaner-scan.txt");
            match std::fs::write(&fallback, &out) {
                Ok(()) => println!("当前目录不可写（{e}），已写入 {}", fallback.display()),
                Err(e2) => eprintln!("写入失败：{e}；回退到 {} 也失败：{e2}", fallback.display()),
            }
        }
    }
}
