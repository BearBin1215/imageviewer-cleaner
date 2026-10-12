//! 百度网盘捆绑组件清理工具 · egui 界面
//!
//! 线程模型：所有阻塞操作跑在 worker 线程，通过 `Reporter` 把进度送回主线程，
//! 并触发 `request_repaint()` 唤醒界面。主线程每帧只做非阻塞的 `try_recv()`，
//! 因此清理期间窗口不会「未响应」。
//!
//! 视觉部分见 `theme.rs`（Windows 11 Fluent 浅色主题），本文件负责布局与交互。

// 发布构建声明为 GUI 子系统，避免双击启动时额外弹出一个控制台窗口；
// 调试构建保留控制台子系统，便于在终端里查看输出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod scan;
mod theme;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use eframe::egui;
use egui::{Margin, RichText, Vec2};

use imageviewer_cleaner_core::backup;
use imageviewer_cleaner_core::clean;
use imageviewer_cleaner_core::data::Tables;
use imageviewer_cleaner_core::model::{ComponentId, Detection, Risk};
use imageviewer_cleaner_core::orchestrator::{self, CleanupConfig};
use imageviewer_cleaner_core::progress::{Level, Progress, Reporter, Summary};
use imageviewer_cleaner_core::sys::SysContext;

use theme::{Btn, Icon};

/// 中文字体子集（由 tools/subset_font.py 生成）
#[cfg(feature = "embed-font")]
const FONT: &[u8] = include_bytes!("../assets/font_subset.ttf");

/// 日志区的内边距
const LOG_MARGIN: Margin = Margin {
    left: 13,
    right: 13,
    top: 10,
    bottom: 10,
};

fn main() -> eframe::Result {
    // --scan：无界面自检，结果写入 scan-output.txt
    if std::env::args().any(|a| a == "--scan") {
        scan::run();
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 680.0])
            .with_min_inner_size([600.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "百度网盘捆绑组件清理工具",
        options,
        Box::new(|cc| {
            install_fonts(cc);
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

/// 注册中文字体。
///
/// 把子集前插到 `Proportional` 并保留 `default_fonts`，
/// 这样子集之外的字符（符号等）可以回退到 egui 自带字体。
fn install_fonts(cc: &eframe::CreationContext<'_>) {
    #[cfg(feature = "embed-font")]
    {
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "cjk".to_owned(),
            std::sync::Arc::new(egui::FontData::from_static(FONT)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "cjk".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push("cjk".to_owned());
        cc.egui_ctx.set_fonts(fonts);
    }
    #[cfg(not(feature = "embed-font"))]
    {
        let _ = cc;
    }
}

/// 当前正在执行的任务，决定进度消息怎么解释
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    Scan,
    Clean,
    Restore,
    DeleteBackup,
}

/// 弹窗底部：分隔线 + 右对齐按钮组。
///
/// 用 `right_to_left` 布局，先添加的按钮排在最右，
/// 因此调用处按「主按钮 → 次按钮」的顺序添加，视觉上得到 [次][主]。
fn dialog_footer(ui: &mut egui::Ui, add_buttons: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(16.0);
    theme::hline(ui, theme::BORDER);
    ui.add_space(12.0);
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        add_buttons(ui);
    });
}

/// 弹窗标题
fn dialog_title(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(15.0).color(theme::TEXT));
    ui.add_space(10.0);
}

struct App {
    /// 主任务的进度通道
    rx: Option<Receiver<Progress>>,
    task: Option<Task>,
    /// 备份列表的后台扫描通道
    backups_rx: Option<Receiver<Vec<PathBuf>>>,
    /// 打开清理确认框前的目标进程检查通道
    precheck_rx: Option<Receiver<Vec<String>>>,

    pending_scan: bool,
    detection: Option<Detection>,
    error: Option<String>,
    log: Vec<(Level, String)>,
    checks: BTreeMap<ComponentId, bool>,

    /// 待确认的清理请求
    confirm_cleanup: bool,
    /// 待确认的备份删除请求
    confirm_delete: Option<PathBuf>,
    /// 清理完成后的提示弹窗（值为本次的隔离区目录）
    cleanup_done: Option<PathBuf>,
    /// 打开确认框时检测到的运行中目标进程
    running_on_open: Vec<String>,
    /// 是否建立占位物阻止重新部署。默认开启：占位法不会导致客户端无法启动。
    harden: bool,
    /// 可还原的备份（最近的排在前面）。全部保留，由用户确认后再删除。
    backups: Vec<PathBuf>,
}

impl App {
    fn new() -> Self {
        Self {
            rx: None,
            task: None,
            backups_rx: None,
            precheck_rx: None,
            pending_scan: true,
            detection: None,
            error: None,
            log: Vec::new(),
            checks: BTreeMap::new(),
            confirm_cleanup: false,
            confirm_delete: None,
            cleanup_done: None,
            running_on_open: Vec::new(),
            harden: true,
            backups: Vec::new(),
        }
    }

    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn selected(&self) -> Vec<ComponentId> {
        let Some(d) = &self.detection else {
            return Vec::new();
        };
        self.checks
            .iter()
            .filter(|(id, on)| **on && d.components.iter().any(|c| c.id == **id && c.present()))
            .map(|(id, _)| *id)
            .collect()
    }

    fn reset(&mut self) {
        self.log.clear();
        self.checks.clear();
        self.detection = None;
        self.error = None;
    }

    /// 在 worker 线程里跑探测
    fn start_scan(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.task = Some(Task::Scan);
        self.error = None;

        let wake = ctx.clone();
        thread::spawn(move || {
            let rep = Reporter::new(tx, Box::new(move || wake.request_repaint()));
            match (|| {
                let tables = Tables::load()?;
                let sys = SysContext::win32();
                orchestrator::run_detect(&sys, &tables, &rep)
            })() {
                Ok(d) => {
                    rep.emit(Progress::Detected(Box::new(d)));
                    rep.emit(Progress::Finished(Ok(Summary::default())));
                }
                Err(e) => rep.emit(Progress::Finished(Err(e))),
            }
        });
    }

    /// 在 worker 线程里跑清理
    fn start_cleanup(&mut self, ctx: &egui::Context, selected: Vec<ComponentId>, harden: bool) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.task = Some(Task::Clean);
        self.error = None;

        let wake = ctx.clone();
        thread::spawn(move || {
            let rep = Reporter::new(tx, Box::new(move || wake.request_repaint()));
            let result = (|| {
                let tables = Tables::load()?;
                let sys = SysContext::win32();
                let cfg = CleanupConfig::new(selected).with_harden(harden);
                orchestrator::run_cleanup(&sys, &tables, &cfg, &rep)
            })();
            rep.emit(Progress::Finished(result));
        });
    }

    /// 在 worker 线程里删除备份
    fn start_delete_backup(&mut self, ctx: &egui::Context, dir: PathBuf) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.task = Some(Task::DeleteBackup);
        self.error = None;

        let wake = ctx.clone();
        thread::spawn(move || {
            let rep = Reporter::new(tx, Box::new(move || wake.request_repaint()));
            match backup::delete_backup(&dir) {
                Ok(()) => {
                    rep.success(format!("备份已删除：{}", dir.display()));
                    rep.emit(Progress::Finished(Ok(Summary::default())));
                }
                Err(e) => rep.emit(Progress::Finished(Err(e))),
            }
        });
    }

    /// 在 worker 线程里跑还原
    fn start_restore(&mut self, ctx: &egui::Context, dir: PathBuf) {
        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        self.task = Some(Task::Restore);
        self.error = None;

        let wake = ctx.clone();
        thread::spawn(move || {
            let rep = Reporter::new(tx, Box::new(move || wake.request_repaint()));
            let result: imageviewer_cleaner_core::Result<Summary> = (|| {
                let sys = SysContext::win32();
                orchestrator::run_restore(&sys, &dir, &rep)?;
                Ok(Summary::default())
            })();
            rep.emit(Progress::Finished(result));
        });
    }

    /// 在 worker 线程里刷新可还原的备份列表。
    ///
    /// 定位安装目录需要枚举进程与读注册表，放在主线程会拖慢界面。
    fn start_backup_scan(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.backups_rx = Some(rx);

        let wake = ctx.clone();
        let sid = self
            .detection
            .as_ref()
            .map(|d| d.target_user_sid.clone())
            .unwrap_or_default();
        thread::spawn(move || {
            let list = match Tables::load() {
                Ok(tables) => orchestrator::locate_backups(&SysContext::win32(), &tables, &sid)
                    .unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            let _ = tx.send(list);
            wake.request_repaint();
        });
    }

    /// 在 worker 线程里检查目标进程是否在运行。
    ///
    /// 结果到达后才打开确认框，避免在主线程枚举进程。
    fn start_precheck(&mut self, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        self.precheck_rx = Some(rx);

        let wake = ctx.clone();
        thread::spawn(move || {
            let names = match Tables::load() {
                Ok(tables) => clean::running_targets(&SysContext::win32(), &tables),
                Err(_) => Vec::new(),
            };
            let _ = tx.send(names);
            wake.request_repaint();
        });
    }

    /// 每帧非阻塞地取主任务的进度
    fn drain(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.rx else { return };
        let mut finished = false;

        while let Ok(p) = rx.try_recv() {
            match p {
                Progress::Detected(d) => {
                    for c in &d.components {
                        self.checks.insert(c.id, c.default_enabled);
                    }
                    self.detection = Some(*d);
                }
                Progress::Stage(s) => self.log.push((Level::Info, s.label().to_owned())),
                Progress::Log { level, text } => self.log.push((level, text)),
                Progress::Finished(Err(e)) => {
                    self.error = Some(e.to_string());
                    self.log.push((Level::Error, e.to_string()));
                    finished = true;
                }
                Progress::Finished(Ok(s)) => {
                    if self.task == Some(Task::Clean) {
                        self.log.push((
                            Level::Success,
                            format!(
                                "清理完成：结束 {} 个进程，清理 {} 项关联，移走 {} 项文件",
                                s.processes_killed, s.registry_items_cleared, s.files_moved
                            ),
                        ));
                        if s.failed > 0 {
                            self.log.push((
                                Level::Warn,
                                format!(
                                    "有 {} 项因被占用或受保护未能处理，可退出网盘后重试清理",
                                    s.failed
                                ),
                            ));
                        }
                        self.log.push((
                            Level::Info,
                            "请现在启动百度网盘，确认它能正常使用。确认无误后可删除备份释放空间。"
                                .to_owned(),
                        ));
                        // 隔离区目录随汇总返回，无需再扫描一遍备份
                        self.cleanup_done = s.quarantine_dir.clone();
                    }
                    finished = true;
                }
            }
        }

        if finished {
            let task = self.task;
            self.rx = None;
            self.task = None;

            // 清理或还原之后现场已变，重新探测并刷新备份列表
            if matches!(task, Some(Task::Clean) | Some(Task::Restore)) {
                self.detection = None;
                self.checks.clear();
                let scan_ctx = ctx.clone();
                self.start_backup_scan(&scan_ctx);
                let detect_ctx = ctx.clone();
                self.start_scan(&detect_ctx);
            } else if task == Some(Task::DeleteBackup) {
                let scan_ctx = ctx.clone();
                self.start_backup_scan(&scan_ctx);
            }
            ctx.request_repaint();
        }
    }

    /// 每帧非阻塞地取后台扫描（备份列表、进程预检）的结果
    fn drain_side_tasks(&mut self) {
        if let Some(rx) = &self.backups_rx {
            if let Ok(list) = rx.try_recv() {
                self.backups = list;
                self.backups_rx = None;
            }
        }
        if let Some(rx) = &self.precheck_rx {
            if let Ok(names) = rx.try_recv() {
                self.running_on_open = names;
                self.precheck_rx = None;
                self.confirm_cleanup = true;
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if self.pending_scan {
            self.pending_scan = false;
            let ctx = ui.ctx().clone();
            self.start_scan(&ctx);
            // 备份扫描自行定位安装目录，不必等探测完成
            let ctx = ui.ctx().clone();
            self.start_backup_scan(&ctx);
        }
        self.drain(ui.ctx());
        self.drain_side_tasks();

        // UI 里只置标志，动作放到面板之外执行，避免闭包与 self 的借用冲突
        let mut act_scan = false;
        let mut act_ask_cleanup = false;
        let mut restore_dir: Option<PathBuf> = None;
        let mut delete_dir: Option<PathBuf> = None;

        let busy = self.busy();
        let prechecking = self.precheck_rx.is_some();
        let backups = self.backups.clone();
        let has_backup = !backups.is_empty();
        let can_clean =
            !busy && !prechecking && self.detection.is_some() && !self.selected().is_empty();

        let install_text = match &self.detection {
            Some(d) => d.install_dir.display().to_string(),
            None if busy => "探测中…".to_owned(),
            None => "未找到".to_owned(),
        };
        let mut env_rows: Vec<(Icon, &str, String)> =
            vec![(Icon::Folder, "安装位置", install_text)];
        if let Some(d) = &self.detection {
            env_rows.push((Icon::User, "目标用户", d.target_user_sid.clone()));
        }

        // ── 底部动作栏 ──
        // 先于中央面板声明，日志区才能吃掉剩余高度
        let footer = egui::Panel::bottom("actions")
            .frame(theme::footer_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let clean = theme::button(ui, "清理", Btn::Primary, can_clean);
                    if clean.clicked() {
                        act_ask_cleanup = true;
                    }
                    if !can_clean && !busy {
                        clean.on_hover_text("先勾选要清理的组件");
                    }

                    let restore = theme::button(ui, "还原", Btn::Ghost, !busy && has_backup);
                    if restore.clicked() {
                        restore_dir = backups.first().cloned();
                    }
                    let del = theme::button(ui, "删除备份", Btn::Ghost, !busy && has_backup);
                    if del.clicked() {
                        delete_dir = backups.first().cloned();
                    }
                    if !has_backup && !busy {
                        restore.on_hover_text("没有可用的备份");
                        del.on_hover_text("没有可用的备份");
                    }

                    if busy {
                        ui.add_space(10.0);
                        ui.add(egui::Spinner::new().size(16.0));
                        theme::weak(ui, "处理中…");
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if theme::button(ui, "重新探测", Btn::Ghost, !busy).clicked() {
                            act_scan = true;
                        }
                    });
                });
            });
        let footer_rect = footer.response.rect;

        // ── 主体 ──
        egui::CentralPanel::default()
            .frame(theme::content_frame())
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 8.0);

                // ── 环境信息 ──
                theme::card(ui, |ui| {
                    for (i, (icon, key, value)) in env_rows.iter().enumerate() {
                        if i > 0 {
                            theme::divider(ui);
                        }
                        theme::row(ui, 11, |ui| {
                            ui.horizontal(|ui| {
                                theme::icon_tile(ui, *icon, 30.0);
                                ui.add_space(10.0);
                                ui.vertical(|ui| {
                                    theme::weak(ui, key);
                                    ui.label(
                                        RichText::new(value)
                                            .monospace()
                                            .size(12.0)
                                            .color(theme::TEXT),
                                    );
                                });
                            });
                        });
                    }
                });

                ui.add_space(6.0);

                // ── 组件清单 ──
                // 不套滚动区：条目数量固定，把富余高度留给日志区
                theme::section(ui, "清理项目");
                match &self.detection {
                    Some(d) if !d.components.is_empty() => {
                        theme::card(ui, |ui| {
                            for (i, c) in d.components.iter().enumerate() {
                                if i > 0 {
                                    theme::divider(ui);
                                }
                                let present = c.present();
                                let on = !busy && present;
                                let mut v =
                                    present && self.checks.get(&c.id).copied().unwrap_or(false);

                                theme::row(ui, 12, |ui| {
                                    ui.horizontal(|ui| {
                                        theme::checkbox(ui, &mut v, on);
                                        ui.add_space(11.0);
                                        if present {
                                            theme::strong(ui, &c.label);
                                            // 详情右对齐到卡片内缘；有风险徽章时，
                                            // 徽章作为后缀留在更右侧
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.spacing_mut().item_spacing.x = 8.0;
                                                    if c.risk == Risk::Caution {
                                                        theme::badge(ui, "谨慎");
                                                    }
                                                    theme::weak(ui, &c.detail);
                                                },
                                            );
                                        } else {
                                            // 已清理的条目保留在原位，避免列表突然变短让人以为出错
                                            theme::weak(ui, &c.label);
                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| theme::weak(ui, "已清理"),
                                            );
                                        }
                                    });
                                });

                                if present {
                                    self.checks.insert(c.id, v);
                                }
                            }
                        });
                    }
                    Some(_) => {
                        theme::weak(ui, "未发现可清理的组件。");
                    }
                    None => {}
                }

                ui.add_space(6.0);

                // ── 防复发加固 ──
                theme::card(ui, |ui| {
                    theme::row(ui, 12, |ui| {
                        ui.horizontal(|ui| {
                            theme::toggle(ui, &mut self.harden, !busy);
                            ui.add_space(12.0);
                            ui.vertical(|ui| {
                                theme::strong(ui, "阻止重新部署");
                                theme::weak(
                                    ui,
                                    "在组件原位置放置同名的只读文件或目录，让客户端无法重建它们",
                                );
                            })
                            .response
                            .on_hover_text(
                                "不影响网盘正常启动，默认开启；\n\
                                 若个别环境异常，可取消勾选，或清理后用「还原」恢复。",
                            );
                        });
                    });
                });

                if let Some(e) = &self.error {
                    ui.add_space(6.0);
                    ui.label(RichText::new(e).size(12.5).color(theme::DANGER));
                }

                ui.add_space(6.0);

                // ── 日志 ──
                // 占满剩余空间：窗口缩小时先压缩这里，滚动条因此出现在日志上
                theme::section(ui, "运行日志");
                let rest = ui.available_height();
                theme::card(ui, |ui| {
                    ui.set_max_height((rest - 2.0).max(56.0));
                    theme::inset(ui, LOG_MARGIN, |ui| {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .stick_to_bottom(true)
                            .show(ui, |ui| {
                                if self.log.is_empty() {
                                    theme::weak(ui, "暂无输出。");
                                }
                                for (level, text) in &self.log {
                                    ui.label(
                                        RichText::new(text)
                                            .monospace()
                                            .size(12.0)
                                            .color(theme::level_color(*level)),
                                    );
                                }
                            });
                    });
                });
            });

        // 底栏顶部的细分隔线。必须画在中央面板之后，否则会被它的底色盖掉。
        theme::top_hline(ui, footer_rect);

        // ── 清理完成后的提醒 ──
        if let Some(qdir) = self.cleanup_done.clone() {
            let mut delete_now = false;
            let mut dismiss = false;
            let modal = egui::Modal::new(egui::Id::new("cleanup_done"))
                .frame(theme::dialog_frame())
                .show(ui.ctx(), |ui| {
                    ui.set_width(412.0);
                    dialog_title(ui, "清理完成");
                    ui.label(
                        RichText::new("清理已完成。请启动百度网盘，确认它能正常使用。")
                            .size(13.0)
                            .color(theme::TEXT),
                    );
                    ui.add_space(4.0);
                    theme::weak(ui, "确认无误后可删除备份释放空间。");
                    dialog_footer(ui, |ui| {
                        if theme::primary_button(ui, "稍后再说").clicked() {
                            dismiss = true;
                        }
                        if theme::ghost_button(ui, "删除备份").clicked() {
                            delete_now = true;
                        }
                    });
                });
            if modal.should_close() {
                dismiss = true;
            }

            if delete_now {
                self.confirm_delete = Some(qdir);
                self.cleanup_done = None;
            } else if dismiss {
                self.cleanup_done = None;
            }
        }

        // ── 清理前的确认 ──
        if self.confirm_cleanup {
            let mut go = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("confirm_cleanup"))
                .frame(theme::dialog_frame())
                .show(ui.ctx(), |ui| {
                    ui.set_width(440.0);
                    dialog_title(ui, "确认清理");

                    if !self.running_on_open.is_empty() {
                        ui.label(
                            RichText::new(format!(
                                "检测到百度网盘正在运行：{}",
                                self.running_on_open.join("、")
                            ))
                            .size(12.5)
                            .color(theme::WARN),
                        );
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new("建议先右键任务栏的网盘图标退出，这样不会被记成异常退出。")
                                .size(13.0)
                                .color(theme::TEXT),
                        );
                        theme::weak(
                            ui,
                            "直接继续则强制结束进程，网盘下次启动可能提示「异常退出」，点取消即可。",
                        );
                        ui.add_space(12.0);
                    }

                    ui.label(
                        RichText::new("将执行以下改动，全部先备份到隔离区：")
                            .size(13.0)
                            .color(theme::TEXT),
                    );
                    ui.add_space(8.0);
                    for id in self.selected() {
                        if let Some(c) = self
                            .detection
                            .as_ref()
                            .and_then(|d| d.components.iter().find(|c| c.id == id))
                        {
                            theme::strong(ui, &format!("· {}", c.label));
                            theme::weak(ui, &format!("   {}", c.detail));
                        }
                    }
                    ui.add_space(12.0);
                    theme::weak(ui, "清理期间请不要关闭窗口。");

                    dialog_footer(ui, |ui| {
                        if theme::primary_button(ui, "开始清理").clicked() {
                            go = true;
                        }
                        if theme::ghost_button(ui, "取消").clicked() {
                            cancel = true;
                        }
                    });
                });
            if modal.should_close() {
                cancel = true;
            }

            if go {
                self.confirm_cleanup = false;
                let ctx = ui.ctx().clone();
                self.start_cleanup(&ctx, self.selected(), self.harden);
            } else if cancel {
                self.confirm_cleanup = false;
            }
        }

        // ── 删除备份的确认 ──
        if let Some(dir) = self.confirm_delete.clone() {
            let mut go = false;
            let mut cancel = false;
            let modal = egui::Modal::new(egui::Id::new("confirm_delete"))
                .frame(theme::dialog_frame())
                .show(ui.ctx(), |ui| {
                    ui.set_width(412.0);
                    dialog_title(ui, "删除备份");
                    ui.label(
                        RichText::new(format!("将删除备份：{}", dir.display()))
                            .monospace()
                            .size(12.0)
                            .color(theme::TEXT),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("删除后无法再用这份备份还原，请确认网盘一切正常。")
                            .size(12.5)
                            .color(theme::DANGER),
                    );
                    dialog_footer(ui, |ui| {
                        if theme::danger_button(ui, "删除").clicked() {
                            go = true;
                        }
                        if theme::ghost_button(ui, "取消").clicked() {
                            cancel = true;
                        }
                    });
                });
            if modal.should_close() {
                cancel = true;
            }

            if go {
                self.confirm_delete = None;
                let ctx = ui.ctx().clone();
                self.start_delete_backup(&ctx, dir);
            } else if cancel {
                self.confirm_delete = None;
            }
        }

        // ── 面板之外执行动作 ──
        if act_scan {
            self.reset();
            let ctx = ui.ctx().clone();
            self.start_scan(&ctx);
        }
        if act_ask_cleanup && self.precheck_rx.is_none() {
            // 先到后台检查目标进程，结果到达后再打开确认框
            let ctx = ui.ctx().clone();
            self.start_precheck(&ctx);
        }
        if let Some(dir) = restore_dir {
            let ctx = ui.ctx().clone();
            self.start_restore(&ctx, dir);
        }
        if let Some(dir) = delete_dir {
            self.confirm_delete = Some(dir);
        }
    }
}
