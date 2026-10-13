# 百度网盘捆绑应用清理工具

移除 Windows 百度网盘客户端捆绑的智能看图与智能播放器，恢复它们占用的文件关联。

当前对应度盘基线版本：`8.8.8.101`。

## 使用

从 [releases](https://github.com/BearBin1215/imageviewer-cleaner/releases) 下载后直接运行，无需安装。支持备份、还原。

## 从源码构建

需要 Rust 工具链与 Windows 下的 MSVC 链接器。

```bash
cargo test --workspace
cargo build --release -p imageviewer-cleaner-gui
```

产物为 `target/release/imageviewer-cleaner.exe`。

发布构建会请求管理员权限。调试时加上 `--features dev-no-elevation` 跳过提权，否则调试器无法启动这个进程。

中文字体子集由 `tools/subset_font.py` 生成，产物已提交进仓库，日常构建不需要重跑。

## 开发模式

改动即重建并重启窗口，无需额外装 `cargo-watch`：

```bash
pwsh -File tools/dev.ps1          # 构建并运行界面
pwsh -File tools/dev.ps1 -Scan    # 不弹界面，只验证探测逻辑
```

脚本监听 `crates/`、`data/` 与清单文件（数据表是编译期内嵌的，改完同样需要重建），
构建前会先结束正在跑的 debug 实例，否则 Windows 的文件占用会让链接失败。
在 VS Code 里对应「开发（watch 自动重建重启）」任务。

需要断点调试时改用 F5。

## 项目结构

```
crates/core/     核心逻辑，不依赖界面，可独立测试
crates/gui/      egui 界面（theme.rs 放配色与自绘控件，main.rs 编排布局与交互）
data/            外置数据表：候选路径、注册表键、扩展名清单、进程名、禁区、
                 占位目标、客户端完整性判据
tools/           字体子集生成脚本、开发模式 watcher
```

网盘更新后如果候选安装路径或注册表键发生变化，只需修改 `data/` 下的数据，不必改动逻辑。
清理目标的相对路径定义在 `crates/core/src/model.rs` 的 `ComponentId` 中。

## 声明

本工具仅清理用户本机已安装软件的文件关联与残留文件。它不修改百度网盘主程序，也不提供任何绕过官方付费或限速的功能。

