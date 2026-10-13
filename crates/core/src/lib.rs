//! 百度网盘捆绑组件清理工具 · 核心逻辑
//!
//! 本 crate 不依赖 GUI，可独立单测。进程、注册表、文件系统、Shell 四类后端经
//! [`sys::SysContext`] 注入，探测与清理的主流程因此能在测试里被完整驱动；
//! 少数辅助操作（原子写 manifest、取本地时间、读取目录项）直接调用系统 API。
//!
//! 分层：
//! - [`detect`] ① 探测层：定位安装目录、解析目标用户、扫描组件与关联
//! - [`backup`] ② 备份层：隔离区与注册表导出
//! - [`clean`] ③ 清理层：结束进程、断关联、移文件、刷新、校验
//! - [`harden`] ④ 加固层：占位法阻断重新部署
//! - [`restore`] ⑤ 还原层：按 manifest 恢复
//! - [`orchestrator`] 编排以上各层，上报进度并在失败时回滚

pub mod backup;
pub mod clean;
pub mod data;
pub mod detect;
pub mod error;
pub mod guard;
pub mod harden;
pub mod model;
pub mod orchestrator;
pub mod progress;
pub mod restore;
pub mod sys;

pub use error::{Error, Result};
pub use model::{
    Action, Component, ComponentId, Detection, Manifest, PlaceholderKind, RegItem, RegPath, RegRoot,
};
pub use orchestrator::{CleanupConfig, run_cleanup, run_detect, run_restore};
pub use progress::{Level, Progress, Reporter, Stage, Summary};
