//! Windows 11 Fluent 浅色主题。
//!
//! 调色板、圆角、间距与自绘控件（复选框 / 开关 / 图标方块）集中在这里，
//! `main.rs` 只引用这里的命名常量，不出现硬编码色值。
//!
//! 关于字重：内嵌的中文子集只有 Regular 一档，egui 自带字体也只有 Light/Regular，
//! 因此层级全部靠「字号 + 颜色」表达，不使用 `RichText::strong()`（它不会真的变粗）。

use eframe::egui;
use egui::{
    Color32, CornerRadius, Frame, Margin, Response, RichText, Sense, Shape, Stroke, StrokeKind, Ui,
    Vec2, pos2,
};

use imageviewer_cleaner_core::progress::Level;

// ───────────────────────────── 调色板 ─────────────────────────────

/// 窗口底色（Windows 11 设置页的浅灰）
pub const PANEL: Color32 = Color32::from_rgb(0xF3, 0xF3, 0xF3);
/// 卡片底色
pub const CARD: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
/// 卡片描边
pub const BORDER: Color32 = Color32::from_rgb(0xE5, 0xE5, 0xE5);
/// 卡片内部分隔线
pub const DIVIDER: Color32 = Color32::from_rgb(0xF0, 0xF0, 0xF0);
/// Windows 11 强调色
pub const ACCENT: Color32 = Color32::from_rgb(0x00, 0x67, 0xC0);
const ACCENT_HOVER: Color32 = Color32::from_rgb(0x1A, 0x76, 0xCB);
const ACCENT_DOWN: Color32 = Color32::from_rgb(0x00, 0x5A, 0xA8);
/// 正文
pub const TEXT: Color32 = Color32::from_rgb(0x1A, 0x1A, 0x1A);
/// 次级文字
pub const TEXT_WEAK: Color32 = Color32::from_rgb(0x61, 0x61, 0x61);
/// 成功 / 警告 / 危险
pub const OK: Color32 = Color32::from_rgb(0x10, 0x7C, 0x10);
pub const WARN: Color32 = Color32::from_rgb(0x9A, 0x62, 0x00);
pub const DANGER: Color32 = Color32::from_rgb(0xA3, 0x2D, 0x2D);

/// 图标方块的浅蓝底
const TILE_BG: Color32 = Color32::from_rgb(0xEA, 0xF2, 0xFC);
/// 控件 hover / 按下时的中性底色
const HOVER_BG: Color32 = Color32::from_rgb(0xF5, 0xF5, 0xF5);
const DOWN_BG: Color32 = Color32::from_rgb(0xEB, 0xEB, 0xEB);
const HOVER_BORDER: Color32 = Color32::from_rgb(0xD0, 0xD0, 0xD0);
/// 未选中复选框的描边
const BOX_BORDER: Color32 = Color32::from_rgb(0x8A, 0x8A, 0x8A);
const BOX_BORDER_HOVER: Color32 = Color32::from_rgb(0x5A, 0x5A, 0x5A);
/// 开关关闭时的轨道
const TRACK_OFF: Color32 = Color32::from_rgb(0xC9, 0xC9, 0xC9);
const TRACK_OFF_HOVER: Color32 = Color32::from_rgb(0xB0, 0xB0, 0xB0);
/// 禁用态
const DISABLED_BG: Color32 = Color32::from_rgb(0xF0, 0xF0, 0xF0);
const DISABLED_BORDER: Color32 = Color32::from_rgb(0xC8, 0xC8, 0xC8);
const DISABLED_TEXT: Color32 = Color32::from_rgb(0xA0, 0xA0, 0xA0);

/// 卡片左右内边距
const PAD_X: i8 = 14;
/// 卡片圆角
const CARD_RADIUS: u8 = 8;
/// 控件圆角
const CTRL_RADIUS: u8 = 5;
/// 按钮最小尺寸，保证弹窗与底部动作栏规格一致
const BTN_SIZE: Vec2 = Vec2::new(88.0, 32.0);

// ───────────────────────────── 主题安装 ─────────────────────────────

/// 把主题写进 egui 的全局样式。在 `App` 创建时调用一次。
///
/// egui 0.36 起样式按主题分开存放，这里固定用浅色（不跟随系统），
/// 只改 light 那一份。
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Light);
    ctx.style_mut_of(egui::Theme::Light, |style| {
        style.visuals = visuals();
        style.animation_time = 0.15;

        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(14.0, 6.0);
        style.spacing.interact_size.y = 28.0;
        style.spacing.scroll = egui::style::ScrollStyle::solid();
        style.spacing.window_margin = Margin::same(18);

        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Small, egui::FontId::proportional(11.0));
        style
            .text_styles
            .insert(egui::TextStyle::Monospace, egui::FontId::monospace(12.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(17.0));
    });
}

fn visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();

    v.panel_fill = PANEL;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(10);
    v.window_shadow = shadow(10, 30, 58);
    v.popup_shadow = shadow(6, 18, 48);
    v.menu_corner_radius = CornerRadius::same(8);

    // 让 ui.weak(...) 直接用主题的次级灰，不必到处写 RichText
    v.weak_text_color = Some(TEXT_WEAK);
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = DANGER;
    v.extreme_bg_color = CARD;
    v.faint_bg_color = Color32::from_rgb(0xFA, 0xFA, 0xFA);
    v.code_bg_color = Color32::from_rgb(0xF5, 0xF5, 0xF5);
    v.selection.bg_fill = Color32::from_rgb(0xCC, 0xE4, 0xF7);
    v.selection.stroke = Stroke::new(1.0, TEXT);

    let w = &mut v.widgets;

    // 静态文字与窗口描边
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.bg_fill = CARD;
    w.noninteractive.weak_bg_fill = CARD;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    w.noninteractive.corner_radius = CornerRadius::same(CTRL_RADIUS);

    // 次要按钮：白底 + 细边框
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.bg_fill = HOVER_BG;
    w.inactive.weak_bg_fill = CARD;
    w.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    w.inactive.corner_radius = CornerRadius::same(CTRL_RADIUS);

    w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.bg_fill = HOVER_BG;
    w.hovered.weak_bg_fill = HOVER_BG;
    w.hovered.bg_stroke = Stroke::new(1.0, HOVER_BORDER);
    w.hovered.corner_radius = CornerRadius::same(CTRL_RADIUS);

    w.active.fg_stroke = Stroke::new(1.0, TEXT);
    w.active.bg_fill = DOWN_BG;
    w.active.weak_bg_fill = DOWN_BG;
    w.active.bg_stroke = Stroke::new(1.0, HOVER_BORDER);
    w.active.corner_radius = CornerRadius::same(CTRL_RADIUS);

    w.open = w.active;

    v
}

fn shadow(offset_y: i8, blur: u8, alpha: u8) -> egui::epaint::Shadow {
    egui::epaint::Shadow {
        offset: [0, offset_y],
        blur,
        spread: 0,
        color: Color32::from_black_alpha(alpha),
    }
}

/// 日志级别 → 文字颜色
pub fn level_color(level: Level) -> Color32 {
    match level {
        Level::Info => TEXT_WEAK,
        Level::Success => OK,
        Level::Warn => WARN,
        Level::Error => DANGER,
    }
}

// ───────────────────────────── 版面零件 ─────────────────────────────

/// 主体内容区：浅灰底 + 四周留白
pub fn content_frame() -> Frame {
    Frame::new()
        .fill(PANEL)
        .inner_margin(Margin::symmetric(16, 14))
}

/// 底部动作栏
pub fn footer_frame() -> Frame {
    Frame::new()
        .fill(PANEL)
        .inner_margin(Margin::symmetric(16, 12))
}

/// 弹窗。直角边框：与主窗口的圆角卡片刻意区分开，一眼能看出「这是模态」。
pub fn dialog_frame() -> Frame {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::ZERO)
        .inner_margin(Margin::symmetric(20, 18))
        .shadow(shadow(16, 40, 70))
}

/// 白底描边卡片。内部 `item_spacing` 归零，行距由各行的内边距控制，
/// 这样分隔线才能横贯整张卡片。
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let w = ui.available_width();
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(CARD_RADIUS))
        .show(ui, |ui| {
            ui.set_min_width((w - 2.0).max(0.0));
            ui.spacing_mut().item_spacing = Vec2::ZERO;
            add(ui)
        })
        .inner
}

/// 撑满可用宽度的内边距容器。用于卡片内部的整块留白（如日志区）。
pub fn inset<R>(ui: &mut Ui, margin: Margin, add: impl FnOnce(&mut Ui) -> R) -> R {
    let w = ui.available_width();
    let dw = (margin.left + margin.right) as f32;
    Frame::new()
        .inner_margin(margin)
        .show(ui, |ui| {
            ui.set_min_width((w - dw).max(0.0));
            add(ui)
        })
        .inner
}

/// 卡片内的一行：左右 14px、上下 `pad_y`。
pub fn row<R>(ui: &mut Ui, pad_y: i8, add: impl FnOnce(&mut Ui) -> R) -> R {
    inset(ui, Margin::symmetric(PAD_X, pad_y), add)
}

/// 横贯可用宽度的 1px 分隔线。
pub fn hline(ui: &mut Ui, color: Color32) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, color);
}

/// 在给定矩形顶部画一条 1px 分隔线。
///
/// `Frame` 的描边只能四边同画，需要单画一条边时用它。
pub fn top_hline(ui: &Ui, rect: egui::Rect) {
    ui.painter()
        .hline(rect.x_range(), rect.top(), Stroke::new(1.0, BORDER));
}

/// 横贯卡片宽度的 1px 分隔线。
pub fn divider(ui: &mut Ui) {
    hline(ui, DIVIDER);
}

/// 小节标题（如「清理项目」）。
pub fn section(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(13.0).color(TEXT));
    ui.add_space(8.0);
}

/// 次要说明文字。
pub fn weak(ui: &mut Ui, text: &str) -> Response {
    ui.label(RichText::new(text).size(12.0).color(TEXT_WEAK))
}

/// 主标题行（组件名 / 路径键名）
pub fn strong(ui: &mut Ui, text: &str) -> Response {
    ui.label(RichText::new(text).size(13.5).color(TEXT))
}

// ───────────────────────────── 按钮 ─────────────────────────────

/// 按钮档位
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// 主操作：强调色填充 + 白字
    Primary,
    /// 次要操作：白底 + 细边框
    Ghost,
    /// 破坏性操作：与次要按钮同规格，红字
    Danger,
}

/// 统一规格的按钮。`enabled == false` 时自己画禁用配色，
/// 因为 egui 对禁用控件只做透明度处理，白底按钮看起来仍像可用。
pub fn button(ui: &mut Ui, text: &str, kind: Btn, enabled: bool) -> Response {
    let (fill, border, label, hover_fill, down_fill, hover_border) = if enabled {
        match kind {
            Btn::Primary => (
                ACCENT,
                Stroke::NONE,
                Color32::WHITE,
                ACCENT_HOVER,
                ACCENT_DOWN,
                Stroke::NONE,
            ),
            Btn::Ghost => (
                CARD,
                Stroke::new(1.0, BORDER),
                TEXT,
                HOVER_BG,
                DOWN_BG,
                Stroke::new(1.0, HOVER_BORDER),
            ),
            Btn::Danger => (
                CARD,
                Stroke::new(1.0, BORDER),
                DANGER,
                HOVER_BG,
                DOWN_BG,
                Stroke::new(1.0, HOVER_BORDER),
            ),
        }
    } else {
        let dim = Stroke::new(1.0, DISABLED_BORDER);
        (
            DISABLED_BG,
            dim,
            DISABLED_TEXT,
            DISABLED_BG,
            DISABLED_BG,
            dim,
        )
    };

    ui.scope(|ui| {
        let w = &mut ui.style_mut().visuals.widgets;
        for (v, f, s) in [
            (&mut w.inactive, fill, border),
            (&mut w.hovered, hover_fill, hover_border),
            (&mut w.active, down_fill, hover_border),
        ] {
            v.weak_bg_fill = f;
            v.bg_fill = f;
            v.bg_stroke = s;
            v.corner_radius = CornerRadius::same(CTRL_RADIUS);
        }
        // 禁用时 egui 会改走 noninteractive 配色，这里一并覆盖
        let n = &mut w.noninteractive;
        n.weak_bg_fill = fill;
        n.bg_fill = fill;
        n.bg_stroke = border;
        n.corner_radius = CornerRadius::same(CTRL_RADIUS);

        ui.add_enabled(
            enabled,
            egui::Button::new(RichText::new(text).size(13.0).color(label)).min_size(BTN_SIZE),
        )
    })
    .inner
}

/// 弹窗主按钮
pub fn primary_button(ui: &mut Ui, text: &str) -> Response {
    button(ui, text, Btn::Primary, true)
}

/// 弹窗次要按钮
pub fn ghost_button(ui: &mut Ui, text: &str) -> Response {
    button(ui, text, Btn::Ghost, true)
}

/// 弹窗破坏性按钮
pub fn danger_button(ui: &mut Ui, text: &str) -> Response {
    button(ui, text, Btn::Danger, true)
}

// ───────────────────────────── 自绘控件 ─────────────────────────────

/// Windows 11 风格复选框：18px 圆角方块，选中时填强调色 + 白勾。
///
/// egui 自带的 `Checkbox` 勾选与否不影响方块底色（底色只跟 hover/按下有关），
/// 做不到「选中变蓝」，所以这里自绘。
pub fn checkbox(ui: &mut Ui, checked: &mut bool, enabled: bool) -> Response {
    const SIZE: f32 = 18.0;

    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::splat(SIZE), sense);

    if enabled && resp.clicked() {
        *checked = !*checked;
        resp.mark_changed();
    }

    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *checked);

        let (fill, stroke) = if enabled {
            let off_fill = if resp.hovered() { HOVER_BG } else { CARD };
            let off_stroke = if resp.hovered() {
                BOX_BORDER_HOVER
            } else {
                BOX_BORDER
            };
            let on = if resp.hovered() { ACCENT_HOVER } else { ACCENT };
            (
                off_fill.lerp_to_gamma(on, t),
                Stroke::new(1.0, off_stroke.lerp_to_gamma(on, t)),
            )
        } else {
            (DISABLED_BG, Stroke::new(1.0, DISABLED_BORDER))
        };

        ui.painter().rect(
            rect,
            CornerRadius::same(4),
            fill,
            stroke,
            StrokeKind::Inside,
        );

        if t > 0.01 {
            // 勾随底色一起淡入，避免未选中时出现白色勾线
            let alpha = (t * 255.0).round().clamp(0.0, 255.0) as u8;
            let mark = Color32::from_rgba_unmultiplied(255, 255, 255, alpha);
            let p = |x: f32, y: f32| rect.min + Vec2::new(x, y);
            ui.painter().add(Shape::line(
                vec![p(4.8, 9.4), p(7.7, 12.3), p(13.3, 6.1)],
                Stroke::new(1.9, mark),
            ));
        }
    }

    resp
}

/// Windows 11 风格开关：40×20 胶囊轨道 + 14px 白色滑块。
pub fn toggle(ui: &mut Ui, on: &mut bool, enabled: bool) -> Response {
    const W: f32 = 40.0;
    const H: f32 = 20.0;
    const KNOB_R: f32 = 7.0;
    const INSET: f32 = 3.0;

    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(W, H), sense);

    if enabled && resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }

    if ui.is_rect_visible(rect) {
        let t = ui.ctx().animate_bool_responsive(resp.id, *on);

        let track = if enabled {
            let off = if resp.hovered() {
                TRACK_OFF_HOVER
            } else {
                TRACK_OFF
            };
            let on_c = if resp.hovered() { ACCENT_HOVER } else { ACCENT };
            off.lerp_to_gamma(on_c, t)
        } else {
            Color32::from_rgb(0xE6, 0xE6, 0xE6)
        };
        ui.painter()
            .rect_filled(rect, CornerRadius::same((H / 2.0) as u8), track);

        let cy = rect.center().y;
        let x0 = rect.left() + INSET + KNOB_R;
        let x1 = rect.right() - INSET - KNOB_R;
        ui.painter()
            .circle_filled(pos2(x0 + (x1 - x0) * t, cy), KNOB_R, Color32::WHITE);
    }

    resp
}

/// 图标方块里画的图形
#[derive(Clone, Copy)]
pub enum Icon {
    Folder,
    User,
}

/// 浅蓝圆角方块 + 描边图标（Windows 11 设置页的观感）。
///
/// 图形用画笔自绘而非字符：字体子集里没有文件夹/人像字形，
/// 依赖 egui 自带字体的回退可能出现豆腐块。
pub fn icon_tile(ui: &mut Ui, icon: Icon, tile: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(tile), Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }

    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(6), TILE_BG);

    // 把 16×16 的图标坐标系映射到方块内部
    let inner = rect.shrink(tile * 0.26);
    let s = inner.width() / 16.0;
    let pt = |x: f32, y: f32| inner.min + Vec2::new(x * s, y * s);
    let stroke = Stroke::new(1.3, ACCENT);

    match icon {
        Icon::Folder => {
            p.add(Shape::closed_line(
                vec![
                    pt(1.5, 12.6),
                    pt(1.5, 4.4),
                    pt(5.6, 4.4),
                    pt(7.0, 6.0),
                    pt(14.5, 6.0),
                    pt(14.5, 12.6),
                ],
                stroke,
            ));
        }
        Icon::User => {
            p.circle_stroke(pt(8.0, 5.4), 2.5 * s, stroke);
            let mut pts = Vec::with_capacity(13);
            for i in 0..=12 {
                let deg = 205.0 + (335.0 - 205.0) * i as f32 / 12.0;
                let a = deg.to_radians();
                pts.push(pt(8.0 + 4.9 * a.cos(), 13.7 + 4.9 * a.sin()));
            }
            p.add(Shape::line(pts, stroke));
        }
    }
}
