## 项目概述

Windows 桌面工具（Rust + egui）：移除百度网盘客户端捆绑的「智能看图」「智能播放器」组件，恢复被占用的文件关联，支持备份、还原和防复发加固。

## 行为准则

- **不遵循最小修改原则**：修改时将关联模块作为整体进行修改，综合考虑全局，不要为了少修改而拒绝更优方案。
- **拒绝猜测性编码**：修改时不猜测行为及后果，先验证，不确定的告知用户。
- **中文优先**：面向用户的文本均使用中文，包括文档、注释、提交信息（除约定式提交前缀）。

## 安全红线

- **禁区校验**：每次删除前必须经过 `guard.rs::assert_not_forbidden` —— 目标必须在安装目录内，且不在禁区表（`data/forbidden.json`）中。主客户端和播放器共用 `module\BrowserEngine`，误删会导致网盘无法启动。
- **ImageViewer 目录本身保留**：只清空 `module\ImageViewer` 的内容（`ComponentId::moves_dir_contents`），移走整个目录会让客户端检测到模块缺失而拒绝启动。
- **进程名精确匹配**：`data/processes.json` 的 `match` 字段必须为 `"exact"`；使用子串匹配会误杀 BaiduYunGuanjia 等其他百度产品。
- **注册表位于用户 hive**：类关联键位于 `HKU\<SID>_Classes`，SID 由 `detect/sid.rs` 解析目标用户；HKLM 层仅作为防御性补充。
- **占位加固**：清理后在原位置放置同名反向占位对象（目录放只读文件，文件放同名目录），阻断重新部署；不会导致客户端启动失败。
- **穷尽匹配防止遗漏**：`ComponentId::default_risk` 显式覆盖所有变体——新增组件时，编译器会强制要求明确指定其风险等级和默认勾选状态。

## 常用命令

```bash
cargo test --workspace   # 运行全部测试
cargo test -p imageviewer-cleaner-core rollback   # 按名称过滤并运行单个测试
cargo fmt --all --check   # CI 格式检查
cargo clippy --workspace --all-targets --locked -- -D warnings   # CI 静态检查
cargo build -p imageviewer-cleaner-gui --features dev-no-elevation   # 调试构建（跳过 UAC）
cargo build --release -p imageviewer-cleaner-gui   # 发布构建（清单要求管理员权限）
```

## 代码地图

Workspace 由以下 crate 组成：

- `crates/core`：全部业务逻辑，不依赖 GUI，可独立进行单元测试。
- `crates/gui`：eframe/egui 界面。`theme.rs` 负责配色和自绘控件（Win11 Fluent 浅色主题），`main.rs` 负责布局和交互。

**数据驱动**：`data/*.json`（候选安装路径、注册表键、扩展名、进程名、禁区、占位目标、客户端完整性判据）通过 `include_str!` 在编译期内嵌到 `core/src/data.rs` 中。网盘更新导致路径或键名变化时，只需修改 JSON，不需要修改代码逻辑，但仍需要重新编译。清理目标的相对路径定义在 `core/src/model.rs` 的 `ComponentId` 中。

**系统后端注入**：`core/src/sys/mod.rs` 定义 registry、process、fs、shell 等 trait 后端，打包为 `SysContext` 提供给业务层使用。真实实现是 `SysContext::win32()`；测试使用 `sys/mock.rs` 中的内存模拟实现，用来完整驱动探测与清理流程，不会触碰真实系统。

**执行管线**：`orchestrator.rs` 按阶段编排：探测 `detect` → 备份 `backup` → 清理 `clean` → 加固 `harden` → 还原 `restore`。进度通过 `Reporter` 回调上报；GUI 的阻塞操作运行在 worker 线程中，主线程每帧只调用 `try_recv()`，因此窗口不会卡死。

失败保护：隔离区建立后立即写入基线 manifest，之后**每个操作完成都会同步写入磁盘**（写前日志 `Journal`）——任何时刻进程被终止或系统断电，都会留下可还原的记录。任何一步失败时，都会复用还原层的逻辑按相反顺序回滚。

## 交付与发布

- **提交检查**：提交前检查门禁是否可以通过。
- **发布流程**：推送 `v*` tag 会触发 release workflow。tag 必须与根 `Cargo.toml` 中的 `version` 一致，否则发布流程会中止。
- **字体**：中文字体子集 `crates/gui/assets/font_subset.ttf` 由 `python tools/subset_font.py` 生成（GB2312 + ASCII + UI 符号），产物已提交仓库，日常构建无需重跑。`embed-font` feature 默认开启，保留 `default_fonts` 供子集未覆盖的字符回退。
