//! Terminal emulation and painting.
//!
//! `vt100` parses the byte stream from the remote shell into a cell grid; this
//! module turns that grid into egui shapes and handles selection/scrolling.

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use std::num::NonZeroU16;

const DEFAULT_BG: Color32 = Color32::from_rgb(0x0d, 0x11, 0x17);
const DEFAULT_FG: Color32 = Color32::from_rgb(0xd8, 0xdc, 0xe4);
const SELECTION_BG: Color32 = Color32::from_rgb(0x26, 0x4f, 0x78);
const SELECTION_FG: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
const SCROLLBACK_LINES: usize = 10_000;
const PAD: f32 = 6.0;

const ANSI: [Color32; 16] = [
    Color32::from_rgb(0x1b, 0x1d, 0x22),
    Color32::from_rgb(0xcd, 0x31, 0x31),
    Color32::from_rgb(0x0d, 0xbc, 0x79),
    Color32::from_rgb(0xe5, 0xe5, 0x10),
    Color32::from_rgb(0x24, 0x72, 0xc8),
    Color32::from_rgb(0xbc, 0x3f, 0xbc),
    Color32::from_rgb(0x11, 0xa8, 0xcd),
    Color32::from_rgb(0xd8, 0xdc, 0xe4),
    Color32::from_rgb(0x6a, 0x73, 0x7d),
    Color32::from_rgb(0xf1, 0x4c, 0x4c),
    Color32::from_rgb(0x23, 0xd1, 0x8b),
    Color32::from_rgb(0xf5, 0xf5, 0x43),
    Color32::from_rgb(0x3b, 0x8e, 0xea),
    Color32::from_rgb(0xd6, 0x70, 0xd6),
    Color32::from_rgb(0x29, 0xb8, 0xdb),
    Color32::from_rgb(0xff, 0xff, 0xff),
];

#[derive(Clone, Copy, Debug)]
struct Selection {
    anchor: (u16, u16),
    focus: (u16, u16),
}

impl Selection {
    fn normalized(self) -> ((u16, u16), (u16, u16)) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    fn contains(self, row: u16, col: u16) -> bool {
        let (start, end) = self.normalized();
        let pos = (row, col);
        pos >= start && pos <= end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalAction {
    Copy,
    Paste,
    SelectAll,
    ClearScrollback,
    Reset,
}

pub struct TerminalUi {
    pub response: egui::Response,
    pub actions: Vec<TerminalAction>,
    pub size_changed: bool,
}

pub struct Terminal {
    parser: vt100::Parser,
    rows: u16,
    cols: u16,
    scroll: usize,
    selection: Option<Selection>,
}

impl Terminal {
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(2);
        let cols = cols.max(2);
        Self {
            parser: vt100::Parser::new(nonzero(rows), nonzero(cols), SCROLLBACK_LINES),
            rows,
            cols,
            scroll: 0,
            selection: None,
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    pub fn process(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    pub fn reset(&mut self) {
        self.parser = vt100::Parser::new(nonzero(self.rows), nonzero(self.cols), SCROLLBACK_LINES);
        self.scroll = 0;
        self.selection = None;
    }

    pub fn set_size(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(2);
        let cols = cols.max(2);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        self.parser.set_size(nonzero(rows), nonzero(cols));
        self.scroll = 0;
        self.selection = None;
    }

    pub fn application_cursor(&self) -> bool {
        self.parser.screen().application_cursor()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    pub fn clear_scrollback(&mut self) {
        self.parser.screen_mut().clear_scrollback();
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.selection = None;
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let max = self.parser.screen().scrollback_len();
        self.scroll = (self.scroll + lines).min(max);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
    }

    pub fn is_scrolled_back(&self) -> bool {
        self.scroll > 0
    }

    pub fn write_notice(&mut self, text: &str) {
        let clean = text.replace(['\x1b', '\r'], "");
        let clean = clean.replace('\n', "\r\n");
        self.process(format!("\r\n\x1b[90m{clean}\x1b[0m\r\n").as_bytes());
    }

    pub fn select_all(&mut self) {
        if self.rows == 0 || self.cols == 0 {
            return;
        }
        self.selection = Some(Selection {
            anchor: (0, 0),
            focus: (self.rows - 1, self.cols - 1),
        });
    }

    pub fn selection_text(&mut self) -> Option<String> {
        let selection = self.selection?;
        let (start, end) = selection.normalized();
        let screen = self.parser.screen_mut();
        screen.set_scrollback(self.scroll);

        let mut result = String::new();
        for row in start.0..=end.0.min(self.rows.saturating_sub(1)) {
            let c0 = if row == start.0 { start.1 } else { 0 };
            let c1 = if row == end.0 {
                end.1.min(self.cols.saturating_sub(1))
            } else {
                self.cols.saturating_sub(1)
            };
            let mut line = String::new();
            for col in c0..=c1 {
                if let Some(cell) = screen.cell(row, col) {
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let contents = cell.contents();
                    if contents.is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(contents);
                    }
                }
            }

            if row != start.0 && !screen.row_wrapped(row - 1) {
                result.push('\n');
            }
            result.push_str(line.trim_end());
        }
        Some(result)
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        font_size: f32,
        focused: bool,
        request_focus: bool,
    ) -> TerminalUi {
        let font = FontId::monospace(font_size);
        let (cell_w, cell_h) = ui.ctx().fonts_mut(|fonts| {
            (
                fonts.glyph_width(&font, 'M').max(1.0),
                fonts.row_height(&font).max(1.0),
            )
        });

        let available = ui.available_size();
        let usable = (available - Vec2::new(PAD * 2.0, PAD * 2.0)).max(Vec2::new(1.0, 1.0));
        let cols = ((usable.x / cell_w).floor() as u16).clamp(2, 500);
        let rows = ((usable.y / cell_h).floor() as u16).clamp(2, 300);

        let size_changed = cols != self.cols || rows != self.rows;
        if size_changed {
            self.set_size(rows, cols);
        }

        let desired = Vec2::new(
            cols as f32 * cell_w + PAD * 2.0,
            rows as f32 * cell_h + PAD * 2.0,
        );
        let (rect, _) = ui.allocate_exact_size(desired, Sense::hover());
        let response = ui
            .interact(
                rect,
                ui.make_persistent_id("terminal-screen"),
                Sense::click_and_drag(),
            )
            .on_hover_cursor(egui::CursorIcon::Text);

        if response.clicked() || request_focus {
            response.request_focus();
        }

        if response.hovered() {
            let wheel = ui.input(|input| input.smooth_scroll_delta.y);
            if wheel.abs() > 0.1 {
                let lines = (wheel.abs() / 48.0).ceil() as usize;
                if wheel > 0.0 {
                    self.scroll_up(lines);
                } else {
                    self.scroll_down(lines);
                }
            }
        }

        let max_scroll = self.parser.screen().scrollback_len();
        self.scroll = self.scroll.min(max_scroll);

        if response.is_pointer_button_down_on() {
            if let Some(pos) = response.interact_pointer_pos() {
                let cell = self.cell_at(pos, rect, cell_w, cell_h);
                let pressed = ui.input(|input| input.pointer.any_pressed());
                let shift = ui.input(|input| input.modifiers.shift);
                if pressed {
                    if shift {
                        if let Some(selection) = &mut self.selection {
                            selection.focus = cell;
                        } else {
                            self.selection = Some(Selection {
                                anchor: cell,
                                focus: cell,
                            });
                        }
                    } else {
                        self.selection = Some(Selection {
                            anchor: cell,
                            focus: cell,
                        });
                    }
                } else if let Some(selection) = &mut self.selection {
                    selection.focus = cell;
                }
            }
        } else if response.clicked() && !ui.input(|input| input.modifiers.shift) {
            self.selection = None;
        }

        let mut actions = Vec::new();
        response.context_menu(|ui| {
            if ui.button("Copy").clicked() {
                actions.push(TerminalAction::Copy);
                ui.close();
            }
            if ui.button("Paste").clicked() {
                actions.push(TerminalAction::Paste);
                ui.close();
            }
            ui.separator();
            if ui.button("Select all").clicked() {
                actions.push(TerminalAction::SelectAll);
                ui.close();
            }
            if ui.button("Clear scrollback").clicked() {
                actions.push(TerminalAction::ClearScrollback);
                ui.close();
            }
            if ui.button("Reset terminal").clicked() {
                actions.push(TerminalAction::Reset);
                ui.close();
            }
        });

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(4), DEFAULT_BG);

        let origin = rect.min + Vec2::splat(PAD);
        let selection = self.selection;

        {
            let screen = self.parser.screen_mut();
            screen.set_scrollback(self.scroll);

            for row in 0..self.rows {
                if let Some(selection) = selection {
                    let (start, end) = selection.normalized();
                    if row >= start.0 && row <= end.0 {
                        let c0 = if row == start.0 { start.1 } else { 0 };
                        let c1 = if row == end.0 {
                            end.1.min(self.cols.saturating_sub(1))
                        } else {
                            self.cols.saturating_sub(1)
                        };
                        if c0 <= c1 {
                            let min = origin + Vec2::new(c0 as f32 * cell_w, row as f32 * cell_h);
                            let size = Vec2::new((c1 - c0 + 1) as f32 * cell_w, cell_h);
                            painter.rect_filled(
                                Rect::from_min_size(min, size),
                                CornerRadius::ZERO,
                                SELECTION_BG,
                            );
                        }
                    }
                }

                for col in 0..self.cols {
                    let Some(cell) = screen.cell(row, col) else {
                        continue;
                    };
                    let selected = selection.is_some_and(|s| s.contains(row, col));
                    let (_, bg) = resolve_colors(
                        cell.fgcolor(),
                        cell.bgcolor(),
                        cell.bold(),
                        cell.dim(),
                        cell.inverse(),
                    );
                    // Selected cells use the selection rectangle drawn above.
                    if !selected && bg != DEFAULT_BG {
                        let min = origin + Vec2::new(col as f32 * cell_w, row as f32 * cell_h);
                        painter.rect_filled(
                            Rect::from_min_size(min, Vec2::new(cell_w, cell_h)),
                            CornerRadius::ZERO,
                            bg,
                        );
                    }
                }

                let mut run = String::new();
                let mut run_start = 0u16;
                let mut run_fg = DEFAULT_FG;
                let mut run_underline = false;
                let mut col = 0u16;

                while col < self.cols {
                    let Some(cell) = screen.cell(row, col) else {
                        col += 1;
                        continue;
                    };
                    if cell.is_wide_continuation() {
                        col += 1;
                        continue;
                    }

                    let selected = selection.is_some_and(|s| s.contains(row, col));
                    let (fg, _) = resolve_colors(
                        cell.fgcolor(),
                        cell.bgcolor(),
                        cell.bold(),
                        cell.dim(),
                        cell.inverse(),
                    );
                    let fg = if selected { SELECTION_FG } else { fg };
                    let underline = cell.underline();
                    let contents = cell.contents();
                    let ch = contents.chars().next().unwrap_or(' ');
                    let wide = cell.is_wide();

                    if !run.is_empty() && (fg != run_fg || underline != run_underline) {
                        paint_run(
                            &painter,
                            origin,
                            cell_w,
                            cell_h,
                            row,
                            run_start,
                            &run,
                            run_fg,
                            run_underline,
                            &font,
                        );
                        run.clear();
                    }

                    if wide {
                        if !run.is_empty() {
                            paint_run(
                                &painter,
                                origin,
                                cell_w,
                                cell_h,
                                row,
                                run_start,
                                &run,
                                run_fg,
                                run_underline,
                                &font,
                            );
                            run.clear();
                        }
                        paint_wide_char(
                            &painter, origin, cell_w, cell_h, row, col, ch, fg, underline, &font,
                        );
                        col += 2;
                        continue;
                    }

                    if run.is_empty() {
                        run_start = col;
                        run_fg = fg;
                        run_underline = underline;
                    }
                    run.push(ch);
                    col += 1;
                }

                if !run.is_empty() {
                    paint_run(
                        &painter,
                        origin,
                        cell_w,
                        cell_h,
                        row,
                        run_start,
                        &run,
                        run_fg,
                        run_underline,
                        &font,
                    );
                }
            }

            if focused && self.scroll == 0 && !screen.hide_cursor() {
                let (cursor_row, cursor_col) = screen.cursor_position();
                if cursor_row < self.rows && cursor_col < self.cols {
                    let min =
                        origin + Vec2::new(cursor_col as f32 * cell_w, cursor_row as f32 * cell_h);
                    let cursor_rect = Rect::from_min_size(min, Vec2::new(cell_w, cell_h));
                    painter.rect_filled(cursor_rect, CornerRadius::ZERO, DEFAULT_FG);
                    if let Some(cell) = screen.cell(cursor_row, cursor_col) {
                        let ch = cell.contents().chars().next().unwrap_or(' ');
                        if ch != ' ' {
                            painter.text(
                                min,
                                Align2::LEFT_TOP,
                                ch.to_string(),
                                font.clone(),
                                DEFAULT_BG,
                            );
                        }
                    }
                }
            } else if focused && self.scroll == 0 {
                // Cursor hidden by the remote application: nothing to draw.
            }
        }

        if !focused {
            painter.rect_stroke(
                rect,
                CornerRadius::same(4),
                Stroke::new(1.0, Color32::from_rgb(0x2a, 0x31, 0x3c)),
                egui::StrokeKind::Inside,
            );
        }

        TerminalUi {
            response,
            actions,
            size_changed,
        }
    }

    fn cell_at(&self, pos: Pos2, rect: Rect, cell_w: f32, cell_h: f32) -> (u16, u16) {
        let local = pos - rect.min - Vec2::splat(PAD);
        let col = (local.x / cell_w).floor().max(0.0) as u16;
        let row = (local.y / cell_h).floor().max(0.0) as u16;
        (
            row.min(self.rows.saturating_sub(1)),
            col.min(self.cols.saturating_sub(1)),
        )
    }
}

fn nonzero(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value.max(1)).expect("non-zero terminal dimension")
}

#[allow(clippy::too_many_arguments)]
fn paint_run(
    painter: &egui::Painter,
    origin: Pos2,
    cell_w: f32,
    cell_h: f32,
    row: u16,
    start_col: u16,
    text: &str,
    fg: Color32,
    underline: bool,
    font: &FontId,
) {
    if text.is_empty() {
        return;
    }
    let pos = origin + Vec2::new(start_col as f32 * cell_w, row as f32 * cell_h);
    let drawn = painter.text(pos, Align2::LEFT_TOP, text, font.clone(), fg);
    if underline {
        let y = drawn.bottom() - 1.0;
        painter.line_segment(
            [Pos2::new(drawn.left(), y), Pos2::new(drawn.right(), y)],
            Stroke::new(1.0, fg),
        );
    }
    let _ = cell_h;
}

#[allow(clippy::too_many_arguments)]
fn paint_wide_char(
    painter: &egui::Painter,
    origin: Pos2,
    cell_w: f32,
    cell_h: f32,
    row: u16,
    col: u16,
    ch: char,
    fg: Color32,
    underline: bool,
    font: &FontId,
) {
    let pos = origin + Vec2::new(col as f32 * cell_w, row as f32 * cell_h);
    if underline {
        painter.line_segment(
            [
                Pos2::new(pos.x, pos.y + cell_h - 1.0),
                Pos2::new(pos.x + cell_w * 2.0, pos.y + cell_h - 1.0),
            ],
            Stroke::new(1.0, fg),
        );
    }
    painter.text(pos, Align2::LEFT_TOP, ch.to_string(), font.clone(), fg);
}

fn resolve_colors(
    fg: vt100::Color,
    bg: vt100::Color,
    bold: bool,
    dim: bool,
    inverse: bool,
) -> (Color32, Color32) {
    let mut fg = ansi_color(fg, true, bold, dim);
    let mut bg = ansi_color(bg, false, bold, dim);
    if inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    (fg, bg)
}

fn ansi_color(color: vt100::Color, foreground: bool, bold: bool, dim: bool) -> Color32 {
    let base = match color {
        vt100::Color::Default => {
            if foreground {
                if bold {
                    Color32::from_rgb(0xf2, 0xf4, 0xf8)
                } else {
                    DEFAULT_FG
                }
            } else {
                DEFAULT_BG
            }
        }
        vt100::Color::Idx(index) => indexed_color(index, bold),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    };
    if dim && foreground {
        Color32::from_rgb(
            (u16::from(base.r()) * 7 / 10) as u8,
            (u16::from(base.g()) * 7 / 10) as u8,
            (u16::from(base.b()) * 7 / 10) as u8,
        )
    } else {
        base
    }
}

fn indexed_color(index: u8, bold: bool) -> Color32 {
    if index < 16 {
        let index = if bold && index < 8 { index + 8 } else { index };
        ANSI[usize::from(index)]
    } else if index < 232 {
        let index = index - 16;
        let r = index / 36;
        let g = (index % 36) / 6;
        let b = index % 6;
        let levels = [0u8, 95, 135, 175, 215, 255];
        Color32::from_rgb(levels[r as usize], levels[g as usize], levels[b as usize])
    } else {
        let value = 8 + (index - 232) * 10;
        Color32::from_rgb(value, value, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_extracts_visible_text() {
        let mut terminal = Terminal::new(5, 24);
        terminal.process(b"hello\r\nworld");
        terminal.select_all();
        let text = terminal.selection_text().unwrap();
        assert!(text.starts_with("hello\nworld"));
        assert!(text.contains("hello"));
    }

    #[test]
    fn scrollback_can_be_examined() {
        let mut terminal = Terminal::new(3, 20);
        for index in 0..10 {
            terminal.process(format!("line {index}\r\n").as_bytes());
        }
        assert!(terminal.parser.screen().scrollback_len() > 0);
        terminal.scroll_up(2);
        assert_eq!(terminal.scroll, 2);
        terminal.scroll_to_bottom();
        assert!(!terminal.is_scrolled_back());
    }

    #[test]
    fn resize_keeps_dimensions() {
        let mut terminal = Terminal::new(24, 80);
        terminal.set_size(40, 120);
        assert_eq!(terminal.size(), (120, 40));
        terminal.set_size(0, 0);
        assert_eq!(terminal.size(), (2, 2));
    }

    #[test]
    fn bold_brightens_basic_ansi_colors() {
        assert_eq!(indexed_color(1, false), ANSI[1]);
        assert_eq!(indexed_color(1, true), ANSI[9]);
        assert_eq!(indexed_color(196, false), Color32::from_rgb(255, 0, 0));
    }
}
