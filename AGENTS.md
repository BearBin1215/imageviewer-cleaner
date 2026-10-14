## 项目概述

Windows 桌面工具（Rust + egui）：移除百度网盘客户端捆绑的「智能看图」「智能播放器」组件，恢复被占用的文件关联，支持备份、还原和防复发加固。

## 行为准则

- **不遵循最小修改原则**：修改时将关联模块作为整体进行修改，综合考虑全局，不要为了少修改而拒绝更优方案。
- **拒绝猜测性编码**：修改时不猜测行为及后果，先验证，不确定的告知用户。
- **中文优先**：面向用户的文本均使用中文，包括文档、注释、提交信息（除约定式提交前缀）。

## 运行约束

改动相关逻辑前先读这些，每条都有代码或测试作为强制点：

- **每次删除都要过禁区闸门**：`guard.rs::assert_not_forbidden` 要求目标位于安装目录内，且不在禁区表（`data/forbidden.json`，每条附理由）里。主客户端与播放器共用 `module\BrowserEngine`，误删会导致网盘无法启动。一个路径要么在禁区表里、要么是组件的清理目标，**不能同时登记在两边**——同时登记会让清理被禁区拦截，整次中止并回滚。
- **新增清理目标先在真机上实测**：确认「移除后客户端能正常启动」才允许默认勾选。做法是先作为独立组件，在默认不勾选的状态下验证，通过后合并进所属组件，并把「为什么删得掉」写进 `model.rs` 里该目标的注释——那里是唯一的解释出处。
- **`module\ImageViewer` 只清空内容，目录本身保留**（`ComponentId::moves_dir_contents`）：整目录移走会被客户端判定为模块缺失而拒绝启动。
- **组件按用户意图划分，不按文件**：一个勾对应一个「用户想移除的捆绑功能」。按文件粒度提供选项等于让用户替代码做归类。可逆性不同的事必须分开勾——文件能从隔离区原样搬回，`UserChoice` 删掉只能让系统重新询问默认程序。
- **只对会被重新部署的入口点占位**：`data/paths.json` 的 `placeholder_targets` 逐条点名具体路径，`harden.rs` 断言它属于该组件的清理目标；不属于的不得占位。
- **通配前缀必须严格长于同目录的禁区/必需文件名**：`PrefixTarget` 的展开不做二次过滤，前缀一旦短到能匹配到引擎本体，展开就会把它一起删掉。`guard.rs` 里的两条断言保证：展开结果能过禁区闸门，且前缀覆盖不到 `required_files` 里的文件。探测与清理共用 `detect::component_rels()` 的展开，界面列出的与实际搬走的必须是同一批路径。
- **进程名必须精确匹配**：`data/processes.json` 的 `match` 为 `"exact"`，子串匹配会误杀 BaiduYunGuanjia 等其他产品。`data.rs` 里那条校验只是 `debug_assert`，发布构建下不生效——本条是发布构建下唯一的提示。
- **类关联键位于 `HKU\<SID>_Classes`**：SID 由 `detect/sid.rs` 解析目标用户；HKLM 层只是防御性补充。
- **隔离区在安装卷根目录**：`imageviewer-cleaner\quarantine`，整层隐藏。本工具免安装，不得在 `Program Files` 里留下看似安装目录的文件夹，位置也与工具自身装在哪里无关。`locate_backups` 仍会列出旧位置（安装目录同级）的备份，不要当作 bug 移除。
- **失败保护**：隔离区建立后立即写入基线 manifest，之后每个动作完成都同步落盘（写前日志 `Journal`）；任何一步失败都按相反顺序回滚。

判断某个目标会不会被重新部署，查安装目录自带的下发清单 `AutoUpdate\Download\AutoUpdate.xml`。它由更新程序落盘，可能滞后于当前版本，只能作为「百度下发过什么」的证据，不作为安全性判据。

## 常用命令

```bash
cargo test --workspace   # 运行全部测试
cargo test -p imageviewer-cleaner-core rollback   # 按名称过滤并运行单个测试
cargo fmt --all --check   # CI 格式检查
cargo clippy --workspace --all-targets --locked -- -D warnings   # CI 静态检查
cargo build -p imageviewer-cleaner-gui --features dev-no-elevation   # 调试构建（跳过 UAC）
./target/debug/imageviewer-cleaner.exe --scan   # 无界面自检，结果写入当前目录 scan-output.txt（不可写时回退 %TEMP%）
cargo build --release -p imageviewer-cleaner-gui   # 发布构建（清单要求管理员权限）
```

## 代码地图

Workspace 由以下 crate 组成：

- `crates/core`：全部业务逻辑，不依赖 GUI，可独立进行单元测试。
- `crates/gui`：eframe/egui 界面。`theme.rs` 负责配色和自绘控件（Win11 Fluent 浅色主题），`main.rs` 负责布局和交互。

**数据驱动**：`data/*.json`（候选安装路径、注册表键、扩展名、进程名、禁区、占位目标、客户端完整性判据）通过 `include_str!` 在编译期内嵌到 `core/src/data.rs` 中。网盘更新导致路径或键名变化时，只需修改 JSON，不需要修改代码逻辑，但仍需要重新编译。清理目标的相对路径定义在 `core/src/model.rs` 的 `ComponentId` 中。

**系统后端注入**：`core/src/sys/mod.rs` 定义 registry、process、fs、shell 等 trait 后端，打包为 `SysContext` 提供给业务层使用。真实实现是 `SysContext::win32()`；测试使用 `sys/mock.rs` 中的内存模拟实现，用来完整驱动探测与清理流程，不会触碰真实系统。

**执行管线**：`orchestrator.rs` 按阶段编排：探测 `detect` → 备份 `backup` → 清理 `clean` → 加固 `harden` → 还原 `restore`。进度通过 `Reporter` 回调上报；GUI 的阻塞操作运行在 worker 线程中，主线程每帧只调用 `try_recv()`，因此窗口不会卡死。

## 交付与发布

- **提交检查**：提交前检查门禁是否可以通过。
- **发布流程**：推送 `v*` tag 会触发 release workflow。tag 必须与根 `Cargo.toml` 中的 `version` 一致，否则发布流程会中止。
- **字体**：中文字体子集 `crates/gui/assets/font_subset.ttf` 由 `python tools/subset_font.py` 生成（GB2312 + ASCII + UI 符号），产物已提交仓库，日常构建无需重跑。`embed-font` feature 默认开启，保留 `default_fonts` 供子集未覆盖的字符回退。
