//! 进度与日志通道 —— core 不依赖 egui
//!
//! egui 是即时模式，`App::ui()` 每帧在主线程执行；清理必须跑在 worker 线程，
//! 通过本模块把进度送回 UI，并触发一次重绘唤醒。

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use crate::error::Error;
use crate::model::Detection;

#[derive(Debug)]
pub enum Progress {
    /// 探测完成，附结果供 UI 展示
    Detected(Box<Detection>),
    /// 阶段切换
    Stage(Stage),
    Log {
        level: Level,
        text: String,
    },
    Finished(Result<Summary, Error>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Detect,
    Backup,
    KillProcesses,
    BreakAssoc,
    MoveFiles,
    Harden,
    Refresh,
    Verify,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Detect => "探测环境",
            Self::Backup => "备份",
            Self::KillProcesses => "结束进程",
            Self::BreakAssoc => "断开文件关联",
            Self::MoveFiles => "移动组件文件",
            Self::Harden => "防复发加固",
            Self::Refresh => "刷新系统缓存",
            Self::Verify => "校验主客户端",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
    Success,
}

#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub processes_killed: usize,
    pub files_moved: usize,
    pub registry_items_cleared: usize,
    pub bytes_reclaimed: u64,
    pub placeholders_created: usize,
    /// 因占用/权限等原因被跳过、未能处理的项数（非致命）
    pub failed: usize,
    /// 本次清理的隔离区目录（供界面直接展示，无需再次扫描）
    pub quarantine_dir: Option<PathBuf>,
}

/// 上报器：每次 `emit` 后触发一次「唤醒 UI」回调
///
/// GUI 侧构造时把 `wake` 设为 `move || ctx.request_repaint()`，
/// 这样后台线程推消息后界面会立刻重绘（eframe 默认被动重绘）。
pub struct Reporter {
    tx: Option<Sender<Progress>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Reporter {
    pub fn new(tx: Sender<Progress>, wake: Box<dyn Fn() + Send + Sync>) -> Self {
        Self { tx: Some(tx), wake }
    }

    /// 无接收端的上报器（CLI / 测试用）
    pub fn silent() -> Self {
        Self {
            tx: None,
            wake: Box::new(|| {}),
        }
    }

    pub fn emit(&self, p: Progress) {
        if let Some(tx) = &self.tx {
            // 接收端已断开时静默忽略：不应让上报失败中断清理
            let _ = tx.send(p);
        }
        (self.wake)();
    }

    pub fn stage(&self, s: Stage) {
        self.emit(Progress::Stage(s));
    }

    pub fn info(&self, text: impl Into<String>) {
        self.log(Level::Info, text);
    }

    pub fn success(&self, text: impl Into<String>) {
        self.log(Level::Success, text);
    }

    pub fn warn(&self, text: impl Into<String>) {
        self.log(Level::Warn, text);
    }

    pub fn log(&self, level: Level, text: impl Into<String>) {
        self.emit(Progress::Log {
            level,
            text: text.into(),
        });
    }
}

impl Clone for Reporter {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            wake: Box::new(|| {}), // 克隆体不重复唤醒；唤醒由主上报器负责
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn emit_triggers_wake_and_delivers() {
        let (tx, rx) = std::sync::mpsc::channel();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let rep = Reporter::new(
            tx,
            Box::new(move || {
                h.fetch_add(1, Ordering::SeqCst);
            }),
        );

        rep.info("hello");
        rep.stage(Stage::Backup);
        rep.warn("careful");

        assert_eq!(hits.load(Ordering::SeqCst), 3, "每次 emit 都应唤醒 UI");
        let msgs: Vec<_> = rx.try_iter().collect();
        assert_eq!(msgs.len(), 3);
        assert!(matches!(
            msgs[0],
            Progress::Log {
                level: Level::Info,
                ..
            }
        ));
        assert!(matches!(msgs[1], Progress::Stage(Stage::Backup)));
    }

    #[test]
    fn silent_reporter_never_panics() {
        let rep = Reporter::silent();
        rep.info("no receiver");
        rep.success("ok");
    }

    #[test]
    fn emit_survives_dropped_receiver() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let rep = Reporter::new(tx, Box::new(|| {}));
        rep.info("receiver gone");
    }
}
