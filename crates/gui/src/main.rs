//! 百度网盘捆绑组件清理工具 · egui 界面
//!
//! 线程模型：所有阻塞操作跑在 worker 线程，
//! 通过 `Reporter` 把进度送回主线程，并触发 `request_repaint()` 唤醒界面。
//! 主线程每帧只做非阻塞的 `try_recv()`，因此清理期间窗口不会「未响应」。

// 发布构建声明为 GUI 子系统，避免双击启动时额外弹出一个控制台窗口；
// 调试构建保留控制台子系统，便于在终端里查看输出。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use eframe::egui;
use imageviewer_cleaner_core::backup;
use imageviewer_cleaner_core::data::Tables;
use imageviewer_cleaner_core::detect;
use imageviewer_cleaner_core::model::{ComponentId, Detection, Risk};
use imageviewer_cleaner_core::orchestrator::{self, CleanupConfig};
use imageviewer_cleaner_core::progress::{Level, Progress, Reporter, Summary};
use imageviewer_cleaner_core::sys::SysContext;

/// 中文字体子集（由 tools/subset_font.py 生成）
#[cfg(feature = "embed-font")]
const FONT: &[u8] = include_bytes!("../assets/font_subset.ttf");

fn main() -> eframe::Result {
    // --scan：无界面自检，结果写入 scan-output.txt。
    // 便于在没有交互桌面的环境里验证探测逻辑。
    // 发布构建没有控制台，终端里不会打印提示，直接看文件即可。
    if std::env::args().any(|a| a == "--scan") {
        run_scan();
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 620.0])
            .with_min_inner_size([560.0, 460.0]),
        ..Default::default()
    };
    eframe::run_native(
        "百度网盘捆绑组件清理工具",
        options,
        Box::new(|cc| {
            install_fonts(cc);
            Ok(Box::new(App::new()))
        }),
    )
}

/// 注册中文字体。
///
/// 关键：**前插**到 `Proportional`（而非整体替换），并保留 `default_fonts`，
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

fn run_scan() {
    use std::fmt::Write as _;

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

/// 当前正在执行的任务，决定进度消息怎么解释
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Task {
    Scan,
    Clean,
    Restore,
}

// ───────────────────────────── 弹窗按钮样式 ─────────────────────────────
//
// 统一尺寸、右对齐、分隔线，主/次/危险三档视觉权重。
// Windows 惯例：主按钮在左，次要按钮在右；配合 `right_to_left` 布局时，
// 先添加的按钮会落在最右，因此调用处应先加次要、再加主要。

/// 弹窗主按钮：加粗、加宽
fn primary_btn(text: &str) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).strong()).min_size(egui::vec2(96.0, 30.0))
}

/// 弹窗次要按钮：与主按钮同尺寸，视觉权重弱
fn ghost_btn(text: &str) -> egui::Button<'static> {
    egui::Button::new(text).min_size(egui::vec2(96.0, 30.0))
}

/// 破坏性按钮：红色加粗文字，用于「删除」等不可逆操作
fn danger_btn(text: &str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text)
            .strong()
            .color(egui::Color32::from_rgb(0xA3, 0x2D, 0x2D)),
    )
    .min_size(egui::vec2(96.0, 30.0))
}

/// 弹窗底部：分隔线 + 右对齐按钮组
fn dialog_footer(ui: &mut egui::Ui, add_buttons: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(14.0);
    ui.separator();
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            add_buttons(ui);
        });
    });
}

struct App {
    rx: Option<Receiver<Progress>>,
    task: Option<Task>,
    pending_scan: bool,
    detection: Option<Detection>,
    error: Option<String>,
    log: Vec<(Level, String)>,
    checks: BTreeMap<ComponentId, bool>,

    /// 待确认的清理请求
    confirm_cleanup: bool,
    /// 待确认的备份删除请求
    confirm_delete: Option<PathBuf>,
    /// 备份根目录（refresh_backups 时记录）
    backup_root: Option<PathBuf>,
    /// 清理完成后的提示弹窗（值为本次的隔离区目录）
    cleanup_done: Option<PathBuf>,
    /// 打开确认框时检测到的运行中目标进程
    running_on_open: Vec<String>,
    /// 是否建立占位物阻止重新部署。默认开启：真机实测不会导致客户端无法启动。
    harden: bool,
    /// 可还原的备份（最近的排在前面）。全部保留，由用户确认后再逐个删除。
    backups: Vec<PathBuf>,
}

impl App {
    fn new() -> Self {
        Self {
            rx: None,
            task: None,
            pending_scan: true,
            detection: None,
            error: None,
            log: Vec::new(),
            checks: BTreeMap::new(),
            confirm_cleanup: false,
            confirm_delete: None,
            backup_root: None,
            cleanup_done: None,
            running_on_open: Vec::new(),
            harden: true,
            backups: Vec::new(),
        }
    }

    /// 当前正在运行的目标进程名（去重）
    ///
    /// 清理需要结束这些进程才能释放文件占用，但强制结束会被网盘记成异常退出，
    /// 于是下次启动时弹「检测到多次异常退出」。让用户先手动退出可以避免这一点。
    fn running_targets(&self) -> Vec<String> {
        let Ok(tables) = Tables::load() else {
            return Vec::new();
        };
        let sys = SysContext::win32();
        let mut names: Vec<String> = sys
            .process
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|p| tables.processes.is_target(&p.name))
            .map(|p| p.name)
            .collect();
        names.sort();
        names.dedup();
        names
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
        self.task = Some(Task::Restore);
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

    /// 扫描可还原的备份目录。
    ///
    /// 保留**全部**备份：绝不自动删除仍持有文件的隔离区——否则「先清 A、
    /// 后清 B」会在 B 完成时把 A 里的唯一副本一并删掉，造成不可还原的数据丢失。
    /// 回收空间交给用户在确认网盘正常后，用每份备份旁的「删除」按钮完成。
    fn refresh_backups(&mut self) {
        let Ok(tables) = Tables::load() else { return };
        let sys = SysContext::win32();
        let sid = self
            .detection
            .as_ref()
            .map(|d| d.target_user_sid.clone())
            .unwrap_or_default();
        let Ok(install_dir) = detect::locate_install_dir(&sys, &tables, &sid) else {
            return;
        };
        let Ok(root) = backup::default_root(&install_dir) else {
            return;
        };
        self.backup_root = Some(root.clone());
        self.backups = backup::list_backups(&root);
    }

    /// 每帧非阻塞地取进度
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
                Progress::Action(_) => {}
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
                                s.processes_killed,
                                s.registry_items_cleared,
                                s.files_moved
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
                            "请现在启动百度网盘，确认它能正常使用。确认无误后可删除备份释放空间。".to_owned(),
                        ));
                    }
                    finished = true;
                }
            }
        }

        if finished {
            let task = self.task;
            self.rx = None;
            self.task = None;

            // 清理或还原之后现场已变，重新探测一次
            if matches!(task, Some(Task::Clean) | Some(Task::Restore)) {
                self.detection = None;
                self.checks.clear();
                self.refresh_backups();

                // 清理成功后弹窗提示确认网盘正常；备份全部保留，弹窗只针对最近一份
                if task == Some(Task::Clean) {
                    self.cleanup_done = self.backups.first().cloned();
                }

                let ctx = ctx.clone();
                self.start_scan(&ctx);
            }
            ctx.request_repaint();
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
            self.refresh_backups();
        }
        self.drain(ui.ctx());

        // UI 里只置标志，动作放到面板之外执行，避免闭包与 self 的借用冲突
        let mut act_scan = false;
        let mut act_ask_cleanup = false;
        let mut restore_dir: Option<PathBuf> = None;
        let mut delete_dir: Option<PathBuf> = None;
        let backups = self.backups.clone();

        egui::CentralPanel::default().show(ui, |ui| {
            // ── 安装位置 ──
            ui.horizontal(|ui| {
                ui.label("安装位置");
                match &self.detection {
                    Some(d) => {
                        ui.monospace(d.install_dir.display().to_string());
                    }
                    None if self.busy() => {
                        ui.weak("探测中…");
                    }
                    None => {
                        ui.weak("未找到");
                    }
                }
                // 「重新探测」保持默认尺寸，不跟随下方动作按钮放大
                if ui
                    .add_enabled(!self.busy(), egui::Button::new("重新探测"))
                    .clicked()
                {
                    act_scan = true;
                }
            });

            if let Some(d) = &self.detection {
                ui.horizontal(|ui| {
                    ui.label("目标用户");
                    ui.monospace(&d.target_user_sid);
                });
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(4.0);

            // ── 组件清单 ──
            // 不套滚动区：条目数量固定，让日志去承担窗口压缩
            // 先把 busy 取出来：在闭包里调用 self.busy() 会借用整个 self，
            // 与同时修改 self.checks 冲突
            let busy = self.busy();
            match &self.detection {
                Some(d) if !d.components.is_empty() => {
                    for c in &d.components {
                        let present = c.present();
                        let enabled = present && self.checks.get(&c.id).copied().unwrap_or(false);
                        ui.horizontal(|ui| {
                            let mut v = enabled;
                            ui.add_enabled(!busy && present, egui::Checkbox::new(&mut v, ""));
                            if present {
                                self.checks.insert(c.id, v);
                            }

                            if present {
                                ui.label(&c.label);
                                ui.weak(&c.detail);

                                if c.risk == Risk::Caution {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(0xB0, 0x75, 0x17),
                                        "谨慎",
                                    );
                                }
                            } else {
                                // 已清理的条目保留在原位，避免列表突然变短让人以为出错
                                ui.weak(format!("{}  已清理", c.label));
                            }
                        });
                    }
                }
                Some(_) => {
                    ui.label("未发现可清理的组件。");
                }
                None => {}
            }

            // ── 防复发加固 ──
            // 与上面三个组件勾选框同款样式（空选框 + 文字），仅隔一点距离。
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_enabled(!busy, egui::Checkbox::new(&mut self.harden, ""));
                ui.label("阻止重新部署").on_hover_text(
                    "在组件原位置放置同名的只读文件或目录，让客户端无法重建它们。\n\
                     已实测不会影响网盘正常启动，默认开启；若个别环境异常，可取消勾选，或清理后用「还原」恢复。",
                );
            });

            ui.add_space(10.0);

            if let Some(e) = &self.error {
                ui.colored_label(egui::Color32::from_rgb(0xA3, 0x2D, 0x2D), e);
                ui.add_space(4.0);
            }

            // ── 动作按钮 ──
            // 清理 / 还原 / 删除备份 三个按钮并排。
            ui.scope(|ui| {
                ui.style_mut().spacing.button_padding = egui::vec2(16.0, 8.0);
                ui.style_mut().spacing.interact_size.y = 30.0;
                ui.horizontal(|ui| {
                    let can_clean =
                        !busy && self.detection.is_some() && !self.selected().is_empty();
                    let clean_btn = ui.add_enabled(can_clean, egui::Button::new("清理"));
                    if clean_btn.clicked() {
                        act_ask_cleanup = true;
                    }
                    if !can_clean && !busy {
                        clean_btn.on_hover_text("先勾选要清理的组件");
                    }

                    let has_backup = !backups.is_empty();
                    let restore_btn =
                        ui.add_enabled(!busy && has_backup, egui::Button::new("还原"));
                    if restore_btn.clicked() {
                        restore_dir = backups.first().cloned();
                    }
                    let del_btn =
                        ui.add_enabled(!busy && has_backup, egui::Button::new("删除备份"));
                    if del_btn.clicked() {
                        delete_dir = backups.first().cloned();
                    }
                    if !has_backup && !busy {
                        restore_btn.on_hover_text("没有可用的备份");
                        del_btn.on_hover_text("没有可用的备份");
                    }

                    if busy {
                        ui.add_space(10.0);
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label("处理中…");
                    }
                });
            });

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(4.0);

            // ── 日志 ──
            // auto_shrink([false, false]) 让它占满剩余空间：
            // 窗口缩小时先压缩这里，滚动条因此出现在日志上
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.log.is_empty() {
                        ui.weak("暂无输出。");
                    }
                    for (level, text) in &self.log {
                        let color = match level {
                            Level::Info => ui.visuals().text_color(),
                            Level::Success => egui::Color32::from_rgb(0x3B, 0x6D, 0x11),
                            Level::Warn => egui::Color32::from_rgb(0xB0, 0x75, 0x17),
                            Level::Error => egui::Color32::from_rgb(0xA3, 0x2D, 0x2D),
                        };
                        ui.colored_label(color, text);
                    }
                });
        });

        // ── 清理完成后的提醒 ──
        if let Some(qdir) = self.cleanup_done.clone() {
            let mut delete_now = false;
            let mut dismiss = false;
            egui::Window::new("清理完成")
                .collapsible(false)
                .resizable(false)
                .min_width(460.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label("清理已完成。");
                    ui.add_space(8.0);
                    ui.label("请启动百度网盘，确认它能正常使用。");
                    ui.label("确认无误后可删除备份释放空间。");
                    dialog_footer(ui, |ui| {
                        if ui.add(ghost_btn("删除备份")).clicked() {
                            delete_now = true;
                        }
                        if ui.add(primary_btn("稍后再说")).clicked() {
                            dismiss = true;
                        }
                    });
                });
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
            egui::Window::new("确认清理")
                .collapsible(false)
                .resizable(false)
                .min_width(520.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    if !self.running_on_open.is_empty() {
                        ui.colored_label(
                            egui::Color32::from_rgb(0xB0, 0x75, 0x17),
                            format!("检测到百度网盘正在运行：{}", self.running_on_open.join("、")),
                        );
                        ui.add_space(6.0);
                        ui.label("建议先右键任务栏的网盘图标退出，这样不会被记成异常退出。");
                        ui.weak("直接继续则强制结束进程，网盘下次启动可能提示「异常退出」，点取消即可。");
                        ui.add_space(12.0);
                    }

                    ui.label("将执行以下改动，全部先备份到隔离区：");
                    ui.add_space(8.0);
                    for id in self.selected() {
                        if let Some(c) = self
                            .detection
                            .as_ref()
                            .and_then(|d| d.components.iter().find(|c| c.id == id))
                        {
                            ui.label(format!("· {}", c.label));
                            ui.weak(format!("   {}", c.detail));
                        }
                    }
                    ui.add_space(12.0);
                    ui.label("清理期间请不要关闭窗口。");
                    dialog_footer(ui, |ui| {
                        if ui.add(ghost_btn("取消")).clicked() {
                            cancel = true;
                        }
                        if ui.add(primary_btn("开始清理")).clicked() {
                            go = true;
                        }
                    });
                });

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
            egui::Window::new("删除备份")
                .collapsible(false)
                .resizable(false)
                .min_width(460.0)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(format!("将删除备份：{}", dir.display()));
                    ui.add_space(6.0);
                    ui.colored_label(
                        egui::Color32::from_rgb(0xA3, 0x2D, 0x2D),
                        "删除后无法再用这份备份还原，请确认网盘一切正常。",
                    );
                    dialog_footer(ui, |ui| {
                        if ui.add(ghost_btn("取消")).clicked() {
                            cancel = true;
                        }
                        if ui.add(danger_btn("删除")).clicked() {
                            go = true;
                        }
                    });
                });

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
        if act_ask_cleanup {
            // 只在打开时检测一次；用户此时退出网盘，结束进程那一步会自行发现无可结束的目标
            self.running_on_open = self.running_targets();
            self.confirm_cleanup = true;
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
