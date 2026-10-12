//! `--scan` 无界面自检：探测一次并写入 `scan-output.txt`。
//!
//! 便于在没有交互桌面的环境里验证探测逻辑。发布构建没有控制台，
//! 终端里不会打印提示，直接看文件即可。

use std::fmt::Write as _;

use imageviewer_cleaner_core::data::Tables;
use imageviewer_cleaner_core::detect;
use imageviewer_cleaner_core::model::Risk;
use imageviewer_cleaner_core::sys::SysContext;

/// 执行自检并写报告文件。
pub fn run() {
    let mut out = String::new();
    let result = (|| {
        let tables = Tables::load()?;
        let sys = SysContext::win32();
        detect::detect(&sys, &tables)
    })();

    match result {
        Ok(d) => {
            let _ = writeln!(out, "安装目录 : {}", d.install_dir.display());
            let _ = writeln!(out, "目标用户 : {}", d.target_user_sid);
            let _ = writeln!(out, "\n组件清单 ({}):", d.components.len());
            for c in &d.components {
                let _ = writeln!(
                    out,
                    "  [{}] {:<12} {:<10} {}",
                    if c.present() && c.default_enabled {
                        "x"
                    } else {
                        " "
                    },
                    c.label,
                    match c.risk {
                        Risk::Safe => "safe",
                        Risk::Caution => "caution",
                        Risk::Forbidden => "forbidden",
                    },
                    if c.present() {
                        c.detail.clone()
                    } else {
                        "已清理".to_owned()
                    }
                );
            }
            let _ = writeln!(out, "\n注册表候选项: {} 条", d.registry_items.len());
            for i in d.registry_items.iter().take(6) {
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
        }
        Err(e) => {
            let _ = writeln!(out, "探测失败: {e}");
        }
    }

    let path = std::path::Path::new("scan-output.txt");
    match std::fs::write(path, &out) {
        Ok(()) => println!("已写入 {}", path.display()),
        Err(e) => eprintln!("写入失败: {e}"),
    }
}
