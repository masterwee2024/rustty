//! Application state and UI.

use crate::input::{encode_event, paste_payload};
use crate::sessions::{
    known_hosts_path, sessions_path, AuthMethod, KnownHosts, SessionProfile, SessionStore, Tunnel,
    TunnelKind,
};
use crate::ssh::{self, ConnectOptions, HostKeyInfo, SshCommand, SshConnection, SshEvent};
use crate::terminal::{Terminal, TerminalAction};
use egui::{
    Align, Align2, Color32, FontId, Id, Key, Layout, Margin, Modifiers, RichText, Sense, Stroke,
    Vec2,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ACCENT: Color32 = Color32::from_rgb(0x58, 0xa6, 0xff);
const PANEL_BG: Color32 = Color32::from_rgb(0x16, 0x1a, 0x21);
const TOOLBAR_BG: Color32 = Color32::from_rgb(0x1b, 0x20, 0x28);
const CARD_BG: Color32 = Color32::from_rgb(0x20, 0x26, 0x30);
const HOVER_BG: Color32 = Color32::from_rgb(0x25, 0x2c, 0x37);
const ACTIVE_BG: Color32 = Color32::from_rgb(0x24, 0x3a, 0x55);
const TEXT: Color32 = Color32::from_rgb(0xd8, 0xdc, 0xe4);
const MUTED: Color32 = Color32::from_rgb(0x8b, 0x95, 0xa3);
const GREEN: Color32 = Color32::from_rgb(0x3f, 0xb9, 0x50);
const YELLOW: Color32 = Color32::from_rgb(0xe5, 0xc0, 0x7b);
const ORANGE: Color32 = Color32::from_rgb(0xd2, 0x99, 0x22);
const RED: Color32 = Color32::from_rgb(0xf8, 0x51, 0x49);

pub struct RusttyApp {
    runtime: tokio::runtime::Runtime,
    wake: Arc<dyn Fn() + Send + Sync>,
    store: SessionStore,
    known_hosts: KnownHosts,
    tabs: Vec<Tab>,
    next_tab_id: u64,
    active_tab: Option<u64>,
    selected_profile: Option<u64>,
    focus_request: Option<u64>,
    font_size: f32,
    editor: Option<SessionEditor>,
    connect_prompt: Option<ConnectPrompt>,
    host_key_prompts: Vec<HostKeyPrompt>,
    confirm_delete: Option<u64>,
    show_about: bool,
    show_tunnels: Option<u64>,
    clipboard: Option<arboard::Clipboard>,
    toast: Option<Toast>,
}

struct Tab {
    id: u64,
    title: String,
    profile_id: Option<u64>,
    options: ConnectOptions,
    conn: Option<SshConnection>,
    terminal: Terminal,
    status: TabStatus,
    terminal_focused: bool,
    last_size: (u16, u16),
    tunnel_states: Vec<TunnelState>,
}

#[derive(Clone)]
struct TunnelState {
    active: bool,
    message: String,
}

enum TabStatus {
    Connecting,
    Connected,
    AwaitingHostKey,
    Closed(String),
    Failed(String),
}

struct SessionEditor {
    profile: SessionProfile,
    persist: bool,
    is_quick: bool,
    is_edit: bool,
    error: Option<String>,
    autofocus: bool,
}

struct ConnectPrompt {
    profile: SessionProfile,
    password: String,
    passphrase: String,
    error: Option<String>,
    autofocus: bool,
}

struct HostKeyPrompt {
    tab_id: u64,
    info: HostKeyInfo,
}

struct Toast {
    text: String,
    at: Instant,
}

#[derive(Clone, Copy)]
enum RowAction {
    Connect,
    Edit,
    Duplicate,
    Delete,
}

enum StatusAction {
    Reconnect,
    Disconnect,
    Tunnels,
}

impl RusttyApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_style(&cc.egui_ctx);

        let ctx = cc.egui_ctx.clone();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || ctx.request_repaint());

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("rustty-ssh")
            .build()
            .expect("failed to start tokio runtime");

        Self {
            runtime,
            wake,
            store: SessionStore::load(&sessions_path()),
            known_hosts: KnownHosts::load(&known_hosts_path()),
            tabs: Vec::new(),
            next_tab_id: 1,
            active_tab: None,
            selected_profile: None,
            focus_request: None,
            font_size: 15.0,
            editor: None,
            connect_prompt: None,
            host_key_prompts: Vec::new(),
            confirm_delete: None,
            show_about: false,
            show_tunnels: None,
            clipboard: arboard::Clipboard::new().ok(),
            toast: None,
        }
    }

    fn modal_open(&self) -> bool {
        self.editor.is_some()
            || self.connect_prompt.is_some()
            || !self.host_key_prompts.is_empty()
            || self.confirm_delete.is_some()
            || self.show_about
            || self.show_tunnels.is_some()
    }

    fn active_index(&self) -> Option<usize> {
        let id = self.active_tab?;
        self.tabs.iter().position(|tab| tab.id == id)
    }

    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some(Toast {
            text: text.into(),
            at: Instant::now(),
        });
    }

    fn persist_sessions(&mut self) {
        if let Err(error) = self.store.save(&sessions_path()) {
            self.toast(format!("Could not save sessions: {error}"));
        }
    }

    fn persist_known_hosts(&mut self) {
        if let Err(error) = self.known_hosts.save(&known_hosts_path()) {
            self.toast(format!("Could not save known hosts: {error}"));
        }
    }

    fn pump_events(&mut self) {
        let mut host_key_events = Vec::new();
        for tab in &mut self.tabs {
            let events: Vec<SshEvent> = match &tab.conn {
                Some(conn) => {
                    let mut events = Vec::new();
                    while let Ok(event) = conn.rx.try_recv() {
                        events.push(event);
                    }
                    events
                }
                None => Vec::new(),
            };

            for event in events {
                match event {
                    SshEvent::Banner(banner) => {
                        tab.terminal
                            .write_notice(&format!("Server banner: {}", banner.trim()));
                    }
                    SshEvent::HostKey(info) => {
                        tab.status = TabStatus::AwaitingHostKey;
                        host_key_events.push(HostKeyPrompt {
                            tab_id: tab.id,
                            info,
                        });
                    }
                    SshEvent::Connected => {
                        tab.status = TabStatus::Connected;
                    }
                    SshEvent::Data(data) => {
                        tab.terminal.process(&data);
                        if matches!(
                            tab.status,
                            TabStatus::Connecting | TabStatus::AwaitingHostKey
                        ) {
                            tab.status = TabStatus::Connected;
                        }
                    }
                    SshEvent::TunnelStatus {
                        index,
                        message,
                        active,
                    } => {
                        if let Some(state) = tab.tunnel_states.get_mut(index) {
                            state.active = active;
                            state.message = message;
                        }
                    }
                    SshEvent::Closed(reason) => {
                        for state in &mut tab.tunnel_states {
                            if state.active {
                                state.active = false;
                                state.message.push_str(" (connection closed)");
                            } else if state.message == "Starting..." {
                                state.message = "Not started (connection closed)".to_owned();
                            }
                        }
                        tab.conn = None;
                        tab.terminal
                            .write_notice(&format!("Disconnected: {reason}"));
                        tab.status = TabStatus::Closed(reason);
                    }
                    SshEvent::Error(error) => {
                        for state in &mut tab.tunnel_states {
                            if state.active {
                                state.active = false;
                                state.message.push_str(" (connection closed)");
                            } else if state.message == "Starting..." {
                                state.message = "Not started (connection failed)".to_owned();
                            }
                        }
                        tab.conn = None;
                        tab.terminal
                            .write_notice(&format!("Connection failed: {error}"));
                        tab.status = TabStatus::Failed(error);
                    }
                }
            }
        }

        if !host_key_events.is_empty() {
            self.host_key_prompts.extend(host_key_events);
        }
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if self.modal_open() {
            return;
        }

        let ctrl_shift = Modifiers::CTRL | Modifiers::SHIFT;
        let mut copy = false;
        let mut paste = false;
        let mut new_session = false;
        let mut quick_connect = false;
        let mut close_tab = false;
        let mut next_tab = false;
        let mut zoom_in = false;
        let mut zoom_out = false;

        ctx.input_mut(|input| {
            copy = input.consume_key(ctrl_shift, Key::C);
            paste = input.consume_key(ctrl_shift, Key::V);
            new_session = input.consume_key(ctrl_shift, Key::N);
            quick_connect = input.consume_key(ctrl_shift, Key::T);
            close_tab = input.consume_key(ctrl_shift, Key::W);
            next_tab = input.consume_key(Modifiers::CTRL, Key::Tab);
            zoom_in = input.consume_key(ctrl_shift, Key::Plus)
                || input.consume_key(ctrl_shift, Key::Equals);
            zoom_out = input.consume_key(ctrl_shift, Key::Minus);
        });

        if copy {
            self.copy_active();
        }
        if paste {
            self.paste_active();
        }
        if new_session {
            self.open_new_editor(false);
        }
        if quick_connect {
            self.open_new_editor(true);
        }
        if close_tab {
            self.close_active_tab();
        }
        if next_tab {
            self.cycle_tab();
        }
        if zoom_in {
            self.font_size = (self.font_size + 1.0).min(32.0);
        }
        if zoom_out {
            self.font_size = (self.font_size - 1.0).max(9.0);
        }
    }

    fn cycle_tab(&mut self) {
        if self.tabs.len() < 2 {
            return;
        }
        let Some(current) = self.active_index() else {
            return;
        };
        let next = (current + 1) % self.tabs.len();
        self.active_tab = Some(self.tabs[next].id);
        self.focus_request = self.active_tab;
    }

    fn show_toolbar(&mut self, ui: &mut egui::Ui) {
        let mut new_session = false;
        let mut quick_connect = false;
        let mut about = false;
        let mut zoom_in = false;
        let mut zoom_out = false;

        egui::Panel::top("toolbar")
            .frame(
                egui::Frame::NONE
                    .fill(TOOLBAR_BG)
                    .inner_margin(Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Rustty").size(20.0).strong().color(ACCENT));
                    ui.label(RichText::new("ssh client").size(11.0).color(MUTED));
                    ui.add_space(10.0);
                    ui.separator();
                    if ui.button("+ New session").clicked() {
                        new_session = true;
                    }
                    if ui.button("Quick connect").clicked() {
                        quick_connect = true;
                    }
                    ui.separator();
                    if ui.button("About").clicked() {
                        about = true;
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .small_button("A+")
                            .on_hover_text("Larger terminal font")
                            .clicked()
                        {
                            zoom_in = true;
                        }
                        if ui
                            .small_button("A-")
                            .on_hover_text("Smaller terminal font")
                            .clicked()
                        {
                            zoom_out = true;
                        }
                        ui.label(
                            RichText::new(format!("{:.0} pt", self.font_size))
                                .small()
                                .color(MUTED),
                        );
                    });
                });
            });

        if new_session {
            self.open_new_editor(false);
        }
        if quick_connect {
            self.open_new_editor(true);
        }
        if about {
            self.show_about = true;
        }
        if zoom_in {
            self.font_size = (self.font_size + 1.0).min(32.0);
        }
        if zoom_out {
            self.font_size = (self.font_size - 1.0).max(9.0);
        }
    }

    fn show_sidebar(&mut self, ui: &mut egui::Ui) {
        let mut selected = None;
        let mut connect = None;
        let mut row_action = None;
        let mut new_session = false;

        egui::Panel::left("sessions")
            .resizable(true)
            .default_size(256.0)
            .size_range(210.0..=400.0)
            .frame(
                egui::Frame::NONE
                    .fill(PANEL_BG)
                    .inner_margin(Margin::symmetric(10, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Sessions").size(15.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("+").on_hover_text("New session").clicked() {
                            new_session = true;
                        }
                    });
                });
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if self.store.sessions.is_empty() {
                            ui.add_space(16.0);
                            ui.label(
                                RichText::new("No saved sessions yet.\nCreate one to get started.")
                                    .color(MUTED),
                            );
                        }

                        for profile in &self.store.sessions {
                            let dot = self
                                .tabs
                                .iter()
                                .find(|tab| tab.profile_id == Some(profile.id))
                                .map(|tab| status_color(&tab.status));
                            let is_selected = self.selected_profile == Some(profile.id);
                            let (response, action) = session_row(ui, profile, is_selected, dot);

                            if response.clicked() {
                                selected = Some(profile.id);
                            }
                            if response.double_clicked() {
                                connect = Some(profile.id);
                            }
                            if let Some(action) = action {
                                row_action = Some((action, profile.id));
                            }
                        }
                    });

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                if ui
                    .add_sized(
                        [ui.available_width(), 32.0],
                        egui::Button::new("+ New session"),
                    )
                    .clicked()
                {
                    new_session = true;
                }
                ui.add_space(2.0);
                ui.label(
                    RichText::new("Double-click a session to connect.")
                        .size(10.0)
                        .color(MUTED),
                );
            });

        if let Some(id) = selected {
            self.selected_profile = Some(id);
        }
        if let Some(id) = connect {
            if let Some(profile) = self.store.get(id).cloned() {
                self.request_connect(profile);
            }
        }
        if let Some((action, id)) = row_action {
            self.handle_row_action(action, id);
        }
        if new_session {
            self.open_new_editor(false);
        }
    }

    fn handle_row_action(&mut self, action: RowAction, id: u64) {
        match action {
            RowAction::Connect => {
                if let Some(profile) = self.store.get(id).cloned() {
                    self.request_connect(profile);
                }
            }
            RowAction::Edit => {
                if let Some(profile) = self.store.get(id).cloned() {
                    self.editor = Some(SessionEditor {
                        profile,
                        persist: true,
                        is_quick: false,
                        is_edit: true,
                        error: None,
                        autofocus: false,
                    });
                }
            }
            RowAction::Duplicate => {
                if let Some(profile) = self.store.get(id).cloned() {
                    let mut copy = profile;
                    copy.id = 0;
                    copy.name = format!("{} copy", copy.display_name());
                    self.editor = Some(SessionEditor {
                        profile: copy,
                        persist: true,
                        is_quick: false,
                        is_edit: false,
                        error: None,
                        autofocus: false,
                    });
                }
            }
            RowAction::Delete => {
                self.confirm_delete = Some(id);
            }
        }
    }

    fn show_central(&mut self, ui: &mut egui::Ui) {
        let modal_open = self.modal_open();
        let ctx = ui.ctx().clone();

        egui::CentralPanel::default().show(ui, |ui| {
            self.show_tab_bar(ui);
            ui.add_space(4.0);

            let Some(index) = self.active_index() else {
                self.show_welcome(ui);
                return;
            };

            if let Some(action) = self.show_status_bar(ui, index) {
                match action {
                    StatusAction::Reconnect => self.reconnect_with_prompt(index),
                    StatusAction::Disconnect => {
                        if let Some(conn) = &self.tabs[index].conn {
                            conn.disconnect();
                        }
                    }
                    StatusAction::Tunnels => {
                        self.show_tunnels = Some(self.tabs[index].id);
                    }
                }
            }
            ui.add_space(2.0);

            let actions;

            {
                let tab = &mut self.tabs[index];
                let tab_id = tab.id;
                let want_focus = self.focus_request == Some(tab_id);
                let font_size = self.font_size;
                let focused = tab.terminal_focused;

                let out = ui
                    .push_id(("terminal", tab_id), |ui| {
                        tab.terminal.show(ui, font_size, focused, want_focus)
                    })
                    .inner;

                tab.terminal_focused = out.response.has_focus() || want_focus;
                actions = out.actions;

                let (cols, rows) = tab.terminal.size();
                if out.size_changed || (cols, rows) != tab.last_size {
                    if let Some(conn) = &tab.conn {
                        conn.send(SshCommand::Resize(cols, rows));
                    }
                    tab.last_size = (cols, rows);
                }

                if tab.terminal_focused && !modal_open {
                    let connected = matches!(tab.status, TabStatus::Connected);
                    if connected {
                        let app_cursor = tab.terminal.application_cursor();
                        let modifiers = ctx.input(|input| input.modifiers);
                        let events = ctx.input(|input| input.events.clone());
                        let mut bytes = Vec::new();

                        for event in &events {
                            if let egui::Event::Key {
                                key,
                                pressed: true,
                                modifiers,
                                ..
                            } = event
                            {
                                if *key == Key::PageUp && modifiers.shift {
                                    tab.terminal.scroll_up(rows.saturating_sub(2) as usize);
                                    continue;
                                }
                                if *key == Key::PageDown && modifiers.shift {
                                    tab.terminal.scroll_down(rows.saturating_sub(2) as usize);
                                    continue;
                                }
                                if *key == Key::Home && modifiers.shift {
                                    tab.terminal.scroll_to_bottom();
                                    continue;
                                }
                            }
                            if let egui::Event::Paste(text) = event {
                                // Ctrl+V is kept as the terminal's literal-next
                                // character; Shift+Insert and middle click paste.
                                if modifiers.ctrl && !modifiers.shift {
                                    bytes.push(0x16);
                                } else {
                                    bytes.extend_from_slice(&paste_payload(
                                        text,
                                        tab.terminal.bracketed_paste(),
                                    ));
                                }
                                continue;
                            }
                            if let Some(mut encoded) = encode_event(event, app_cursor, modifiers) {
                                bytes.append(&mut encoded);
                            }
                        }

                        if !bytes.is_empty() {
                            if let Some(conn) = &tab.conn {
                                conn.send(SshCommand::Input(bytes));
                            }
                            tab.terminal.scroll_to_bottom();
                        }
                    }
                }
            }

            for action in actions {
                self.handle_terminal_action(index, action);
            }

            self.focus_request = None;
        });
    }

    fn show_tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut select = None;
        let mut close = None;

        ui.horizontal(|ui| {
            egui::ScrollArea::horizontal()
                .id_salt("tab-scroll")
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for tab in &self.tabs {
                            ui.label(
                                RichText::new("●")
                                    .size(10.0)
                                    .color(status_color(&tab.status)),
                            );
                            let response = ui.selectable_label(
                                self.active_tab == Some(tab.id),
                                RichText::new(&tab.title).size(13.0),
                            );
                            if response.clicked() {
                                select = Some(tab.id);
                            }
                            if response.middle_clicked() {
                                close = Some(tab.id);
                            }
                            if ui
                                .add(egui::Button::new(RichText::new("×").size(12.0)).frame(false))
                                .on_hover_text("Close tab")
                                .clicked()
                            {
                                close = Some(tab.id);
                            }
                            ui.add_space(6.0);
                        }
                    });
                });
        });

        if let Some(id) = select {
            self.active_tab = Some(id);
            self.focus_request = Some(id);
        }
        if let Some(id) = close {
            self.close_tab(id);
        }
    }

    fn show_status_bar(&mut self, ui: &mut egui::Ui, index: usize) -> Option<StatusAction> {
        let tab = &self.tabs[index];
        let mut action = None;
        let mut scroll_bottom = false;
        let (cols, rows) = tab.terminal.size();

        ui.horizontal(|ui| {
            ui.label(
                RichText::new("●")
                    .size(11.0)
                    .color(status_color(&tab.status)),
            );
            ui.label(
                RichText::new(status_label(&tab.status))
                    .small()
                    .color(status_color(&tab.status)),
            );
            ui.separator();
            ui.label(
                RichText::new(format!("{cols}×{rows}"))
                    .small()
                    .monospace()
                    .color(MUTED),
            );
            ui.separator();
            ui.label(
                RichText::new(format!("{}@{}", tab.options.username, tab.options.target()))
                    .small()
                    .monospace()
                    .color(MUTED),
            );

            if tab.terminal.is_scrolled_back() {
                ui.separator();
                ui.label(RichText::new("SCROLLBACK").small().color(YELLOW));
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if matches!(tab.status, TabStatus::Closed(_) | TabStatus::Failed(_)) {
                    if ui.small_button("Reconnect").clicked() {
                        action = Some(StatusAction::Reconnect);
                    }
                } else if tab.conn.is_some() && ui.small_button("Disconnect").clicked() {
                    action = Some(StatusAction::Disconnect);
                }
                if tab.terminal.is_scrolled_back() && ui.small_button("Bottom").clicked() {
                    scroll_bottom = true;
                }
                if !tab.options.tunnels.is_empty()
                    && ui
                        .small_button(format!("Tunnels ({})", tab.options.tunnels.len()))
                        .on_hover_text("Show SSH tunnels for this connection")
                        .clicked()
                {
                    action = Some(StatusAction::Tunnels);
                }
            });
        });

        if scroll_bottom {
            self.tabs[index].terminal.scroll_to_bottom();
        }

        action
    }

    fn show_welcome(&mut self, ui: &mut egui::Ui) {
        let mut new_session = false;
        let mut quick_connect = false;

        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.16).max(30.0));
            ui.label(RichText::new("Rustty").size(46.0).strong().color(ACCENT));
            ui.label(
                RichText::new("A small, fast SSH terminal client")
                    .size(14.0)
                    .color(MUTED),
            );
            ui.add_space(22.0);
            ui.horizontal(|ui| {
                let total = 2.0 * 160.0 + 8.0;
                ui.add_space((ui.available_width() - total).max(0.0) / 2.0);
                if ui
                    .add_sized(
                        [160.0, 36.0],
                        egui::Button::new(RichText::new("New session").size(14.0)),
                    )
                    .clicked()
                {
                    new_session = true;
                }
                if ui
                    .add_sized(
                        [160.0, 36.0],
                        egui::Button::new(RichText::new("Quick connect").size(14.0)),
                    )
                    .clicked()
                {
                    quick_connect = true;
                }
            });
            ui.add_space(30.0);
            ui.label(
                RichText::new(
                    "Ctrl+Shift+N  new session     Ctrl+Shift+W  close tab     Ctrl+Shift+C / V  copy / paste",
                )
                .size(11.0)
                .color(MUTED),
            );
            ui.add_space(4.0);
            ui.label(
                RichText::new("Ctrl+Tab  next tab     Ctrl+Shift+T  quick connect")
                    .size(11.0)
                    .color(MUTED),
            );
        });

        if new_session {
            self.open_new_editor(false);
        }
        if quick_connect {
            self.open_new_editor(true);
        }
    }

    fn handle_terminal_action(&mut self, index: usize, action: TerminalAction) {
        match action {
            TerminalAction::Copy => self.copy_from_tab(index),
            TerminalAction::Paste => self.paste_into(index),
            TerminalAction::SelectAll => self.tabs[index].terminal.select_all(),
            TerminalAction::ClearScrollback => {
                self.tabs[index].terminal.clear_scrollback();
                self.toast("Scrollback cleared");
            }
            TerminalAction::Reset => {
                self.tabs[index].terminal.reset();
                self.toast("Terminal reset");
            }
        }
    }

    fn copy_active(&mut self) {
        if let Some(index) = self.active_index() {
            self.copy_from_tab(index);
        }
    }

    fn copy_from_tab(&mut self, index: usize) {
        let text = self.tabs[index].terminal.selection_text();
        match text {
            Some(text) if !text.is_empty() => {
                if self.clipboard.is_none() {
                    self.clipboard = arboard::Clipboard::new().ok();
                }
                let copied = self
                    .clipboard
                    .as_mut()
                    .map(|clipboard| clipboard.set_text(text).is_ok())
                    .unwrap_or(false);
                if copied {
                    self.toast("Copied selection");
                } else {
                    self.toast("Could not access the clipboard");
                }
            }
            _ => self.toast("Select some terminal text first"),
        }
    }

    fn paste_active(&mut self) {
        if let Some(index) = self.active_index() {
            self.paste_into(index);
        }
    }

    fn paste_into(&mut self, index: usize) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        let text = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| clipboard.get_text().ok());
        let Some(text) = text else {
            self.toast("Could not read the clipboard");
            return;
        };
        let bracketed = self.tabs[index].terminal.bracketed_paste();
        let payload = paste_payload(&text, bracketed);
        if let Some(conn) = &self.tabs[index].conn {
            conn.send(SshCommand::Input(payload));
        }
        self.tabs[index].terminal.scroll_to_bottom();
    }

    fn open_new_editor(&mut self, quick: bool) {
        self.editor = Some(SessionEditor {
            profile: SessionProfile::default(),
            persist: !quick,
            is_quick: quick,
            is_edit: false,
            error: None,
            autofocus: true,
        });
    }

    fn request_connect(&mut self, profile: SessionProfile) {
        self.connect_prompt = Some(ConnectPrompt {
            profile,
            password: String::new(),
            passphrase: String::new(),
            error: None,
            autofocus: true,
        });
    }

    fn start_connection(&mut self, profile: SessionProfile, password: String, passphrase: String) {
        let (cols, rows) = (100, 30);
        let options = ConnectOptions {
            host: profile.host.trim().to_owned(),
            port: profile.port,
            username: profile.username.trim().to_owned(),
            auth: profile.auth.clone(),
            key_path: profile.key_path.trim().to_owned(),
            password,
            passphrase,
            terminal_type: if profile.terminal_type.trim().is_empty() {
                "xterm-256color".to_owned()
            } else {
                profile.terminal_type.trim().to_owned()
            },
            keepalive_secs: profile.keepalive_secs,
            cols,
            rows,
            tunnels: profile.tunnels.clone(),
        };

        let id = self.next_tab_id;
        self.next_tab_id += 1;

        let mut terminal = Terminal::new(rows, cols);
        terminal.write_notice(&format!(
            "Connecting to {} as {} ...",
            options.target(),
            options.username
        ));

        let conn = ssh::start(
            &self.runtime,
            options.clone(),
            self.known_hosts.clone(),
            self.wake.clone(),
        );

        let tunnel_states = options
            .tunnels
            .iter()
            .map(|_| TunnelState {
                active: false,
                message: "Starting...".to_owned(),
            })
            .collect();

        self.tabs.push(Tab {
            id,
            title: profile.display_name(),
            profile_id: (profile.id != 0).then_some(profile.id),
            options,
            conn: Some(conn),
            terminal,
            status: TabStatus::Connecting,
            terminal_focused: false,
            last_size: (cols, rows),
            tunnel_states,
        });
        self.active_tab = Some(id);
        self.focus_request = Some(id);
    }

    fn retry_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let (cols, rows) = self.tabs[index].terminal.size();
        let mut options = self.tabs[index].options.clone();
        options.cols = cols;
        options.rows = rows;

        if let Some(conn) = &self.tabs[index].conn {
            conn.disconnect();
        }

        let conn = ssh::start(
            &self.runtime,
            options.clone(),
            self.known_hosts.clone(),
            self.wake.clone(),
        );
        let tab = &mut self.tabs[index];
        tab.options = options;
        tab.conn = Some(conn);
        tab.status = TabStatus::Connecting;
        tab.last_size = (cols, rows);
        tab.terminal.write_notice("Reconnecting...");
        self.active_tab = Some(tab.id);
        self.focus_request = Some(tab.id);
    }

    /// Close a failed tab and prompt for credentials again, so that a typo in a
    /// password or key passphrase can be corrected.
    fn reconnect_with_prompt(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let tab = &self.tabs[index];
        let profile = SessionProfile {
            id: tab.profile_id.unwrap_or(0),
            name: tab.title.clone(),
            host: tab.options.host.clone(),
            port: tab.options.port,
            username: tab.options.username.clone(),
            auth: tab.options.auth.clone(),
            key_path: tab.options.key_path.clone(),
            terminal_type: tab.options.terminal_type.clone(),
            keepalive_secs: tab.options.keepalive_secs,
            tunnels: tab.options.tunnels.clone(),
        };
        let tab_id = tab.id;
        self.close_tab(tab_id);
        self.request_connect(profile);
    }

    fn close_active_tab(&mut self) {
        if let Some(id) = self.active_tab {
            self.close_tab(id);
        }
    }

    fn close_tab(&mut self, id: u64) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        if let Some(conn) = &self.tabs[index].conn {
            conn.disconnect();
        }
        self.tabs.remove(index);
        if self.active_tab == Some(id) {
            let next = if self.tabs.is_empty() {
                None
            } else {
                Some(self.tabs[index.min(self.tabs.len() - 1)].id)
            };
            self.active_tab = next;
            self.focus_request = next;
        }
    }

    fn show_dialogs(&mut self, ctx: &egui::Context) {
        self.show_editor(ctx);
        self.show_connect_prompt(ctx);
        self.show_host_key_prompt(ctx);
        self.show_delete_confirm(ctx);
        self.show_tunnels_modal(ctx);
        self.show_about(ctx);
    }

    fn show_editor(&mut self, ctx: &egui::Context) {
        let Some(mut editor) = self.editor.take() else {
            return;
        };
        let mut close = false;
        let mut save_clicked = false;
        let mut connect_clicked = false;

        let title = if editor.is_quick {
            "Quick connect"
        } else if editor.is_edit {
            "Edit session"
        } else {
            "New session"
        };

        let modal = egui::Modal::new(Id::unique("session-editor")).show(ctx, |ui| {
            ui.set_width(720.0);
            ui.heading(title);
            ui.add_space(4.0);

            egui::Grid::new("session-fields")
                .num_columns(2)
                .spacing([14.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.profile.name)
                            .hint_text("Optional; defaults to user@host")
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Host");
                    ui.horizontal(|ui| {
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut editor.profile.host)
                                .hint_text("example.com")
                                .desired_width(300.0),
                        );
                        if editor.autofocus {
                            response.request_focus();
                            editor.autofocus = false;
                        }
                        ui.label("Port");
                        ui.add(
                            egui::DragValue::new(&mut editor.profile.port)
                                .speed(1.0)
                                .range(1..=65535),
                        );
                    });
                    ui.end_row();

                    ui.label("Username");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.profile.username)
                            .desired_width(f32::INFINITY),
                    );
                    ui.end_row();

                    ui.label("Authentication");
                    egui::ComboBox::from_id_salt("auth-method")
                        .selected_text(editor.profile.auth.label())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut editor.profile.auth,
                                AuthMethod::Password,
                                "Password",
                            );
                            ui.selectable_value(
                                &mut editor.profile.auth,
                                AuthMethod::PublicKey,
                                "Public key",
                            );
                        });
                    ui.end_row();

                    if editor.profile.auth == AuthMethod::PublicKey {
                        ui.label("Private key");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut editor.profile.key_path)
                                    .hint_text("C:\\Users\\you\\.ssh\\id_ed25519")
                                    .desired_width(330.0),
                            );
                            if ui.button("Browse...").clicked() {
                                let mut dialog =
                                    rfd::FileDialog::new().set_title("Select a private key");
                                if let Some(home) = std::env::var_os("USERPROFILE")
                                    .or_else(|| std::env::var_os("HOME"))
                                {
                                    let ssh = std::path::PathBuf::from(home).join(".ssh");
                                    if ssh.is_dir() {
                                        dialog = dialog.set_directory(ssh);
                                    }
                                }
                                if let Some(path) = dialog.pick_file() {
                                    editor.profile.key_path = path.display().to_string();
                                }
                            }
                        });
                        ui.end_row();
                    }

                    ui.label("Terminal type");
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.profile.terminal_type)
                            .desired_width(180.0),
                    );
                    ui.end_row();

                    ui.label("Keepalive");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::DragValue::new(&mut editor.profile.keepalive_secs)
                                .speed(1.0)
                                .range(0..=3600),
                        );
                        ui.label(RichText::new("seconds (0 disables)").small().color(MUTED));
                    });
                    ui.end_row();
                });

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(2.0);

            let mut add_tunnel = false;
            let mut remove_tunnel = None;
            ui.horizontal(|ui| {
                ui.label(RichText::new("SSH tunnels").strong());
                if ui.small_button("+ Add tunnel").clicked() {
                    add_tunnel = true;
                }
            });

            if editor.profile.tunnels.is_empty() {
                ui.label(
                    RichText::new(
                        "No tunnels. Supports local (-L), remote (-R) and dynamic SOCKS5 (-D) forwarding.",
                    )
                    .small()
                    .color(MUTED),
                );
            } else {
                for (index, tunnel) in editor.profile.tunnels.iter_mut().enumerate() {
                    ui.push_id(("tunnel", index), |ui| {
                        egui::Frame::NONE
                            .fill(CARD_BG)
                            .corner_radius(6)
                            .inner_margin(Margin::symmetric(8, 6))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    egui::ComboBox::from_id_salt(("tunnel-kind", index))
                                        .width(78.0)
                                        .selected_text(tunnel.kind.label())
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut tunnel.kind,
                                                TunnelKind::Local,
                                                "Local",
                                            );
                                            ui.selectable_value(
                                                &mut tunnel.kind,
                                                TunnelKind::Remote,
                                                "Remote",
                                            );
                                            ui.selectable_value(
                                                &mut tunnel.kind,
                                                TunnelKind::Dynamic,
                                                "SOCKS5",
                                            );
                                        });
                                    ui.label(RichText::new("bind").small().color(MUTED));
                                    ui.add(
                                        egui::TextEdit::singleline(&mut tunnel.bind_host)
                                            .desired_width(108.0),
                                    );
                                    ui.add(
                                        egui::DragValue::new(&mut tunnel.bind_port)
                                            .speed(1.0)
                                            .range(1..=65535),
                                    );
                                    if tunnel.kind != TunnelKind::Dynamic {
                                        ui.label(RichText::new("→").color(MUTED));
                                        ui.add(
                                            egui::TextEdit::singleline(&mut tunnel.target_host)
                                                .hint_text("target host")
                                                .desired_width(132.0),
                                        );
                                        ui.add(
                                            egui::DragValue::new(&mut tunnel.target_port)
                                                .speed(1.0)
                                                .range(1..=65535),
                                        );
                                    }
                                    if ui.small_button("Remove").clicked() {
                                        remove_tunnel = Some(index);
                                    }
                                });
                                ui.label(
                                    RichText::new(tunnel.kind.hint()).size(10.0).color(MUTED),
                                );
                            });
                    });
                    ui.add_space(2.0);
                }
            }

            if add_tunnel {
                editor.profile.tunnels.push(Tunnel::default());
            }
            if let Some(index) = remove_tunnel {
                editor.profile.tunnels.remove(index);
            }

            if !editor.profile.tunnels.is_empty() {
                ui.label(
                    RichText::new(
                        "Tunnels start when the connection opens. Changes apply on the next connect.",
                    )
                    .size(10.0)
                    .color(MUTED),
                );
            }

            if !editor.is_quick {
                ui.add_space(4.0);
                ui.checkbox(&mut editor.persist, "Save this session");
            }

            if let Some(error) = &editor.error {
                ui.add_space(4.0);
                ui.label(RichText::new(error).color(RED));
            }

            ui.add_space(8.0);
            ui.label(
                RichText::new("Passwords are only kept in memory and are never written to disk.")
                    .size(11.0)
                    .color(MUTED),
            );
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(
                            RichText::new("Connect").strong().color(Color32::WHITE),
                        ))
                        .clicked()
                    {
                        connect_clicked = true;
                    }
                    if !editor.is_quick && ui.button("Save").clicked() {
                        save_clicked = true;
                    }
                });
            });
        });

        if modal.should_close() {
            close = true;
        }

        if save_clicked || connect_clicked {
            match editor.profile.validate() {
                Err(error) => {
                    editor.error = Some(error);
                }
                Ok(()) => {
                    if editor.profile.name.trim().is_empty() {
                        editor.profile.name = format!(
                            "{}@{}",
                            editor.profile.username.trim(),
                            editor.profile.host.trim()
                        );
                    }

                    let should_persist = editor.persist || (save_clicked && !editor.is_quick);
                    if should_persist {
                        let id = self.store.upsert(editor.profile.clone());
                        editor.profile.id = id;
                        self.persist_sessions();
                        self.selected_profile = Some(id);
                    }

                    if connect_clicked {
                        self.request_connect(editor.profile.clone());
                        close = true;
                    } else if save_clicked {
                        self.toast("Session saved");
                        close = true;
                    }
                }
            }
        }

        if !close {
            self.editor = Some(editor);
        }
    }

    fn show_connect_prompt(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.connect_prompt.take() else {
            return;
        };
        let mut close = false;
        let mut submit = false;

        let modal = egui::Modal::new(Id::unique("connect-prompt")).show(ctx, |ui| {
            ui.set_width(430.0);
            ui.heading(format!("Connect to {}", prompt.profile.display_name()));
            ui.label(
                RichText::new(format!(
                    "{}@{}",
                    prompt.profile.username,
                    prompt.profile.target()
                ))
                .monospace()
                .color(MUTED),
            );
            ui.add_space(6.0);

            egui::Grid::new("connect-fields")
                .num_columns(2)
                .spacing([12.0, 8.0])
                .show(ui, |ui| {
                    ui.label("Username");
                    ui.add(
                        egui::TextEdit::singleline(&mut prompt.profile.username)
                            .desired_width(280.0),
                    );
                    ui.end_row();

                    match prompt.profile.auth {
                        AuthMethod::Password => {
                            ui.label("Password");
                            let response = ui.add(
                                egui::TextEdit::singleline(&mut prompt.password)
                                    .password(true)
                                    .desired_width(280.0),
                            );
                            if prompt.autofocus {
                                response.request_focus();
                                prompt.autofocus = false;
                            }
                            ui.end_row();
                        }
                        AuthMethod::PublicKey => {
                            ui.label("Key");
                            ui.label(
                                RichText::new(&prompt.profile.key_path)
                                    .monospace()
                                    .small()
                                    .color(MUTED),
                            );
                            ui.end_row();

                            ui.label("Passphrase");
                            ui.add(
                                egui::TextEdit::singleline(&mut prompt.passphrase)
                                    .password(true)
                                    .hint_text("Leave blank if the key is not encrypted")
                                    .desired_width(280.0),
                            );
                            ui.end_row();
                        }
                    }
                });

            if let Some(error) = &prompt.error {
                ui.add_space(4.0);
                ui.label(RichText::new(error).color(RED));
            }
            ui.add_space(8.0);
            if ui.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter)) {
                submit = true;
            }
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(
                            RichText::new("Connect").strong().color(Color32::WHITE),
                        ))
                        .clicked()
                    {
                        submit = true;
                    }
                });
            });
        });

        if modal.should_close() {
            close = true;
        }

        if submit {
            if prompt.profile.username.trim().is_empty() {
                prompt.error = Some("Username is required.".to_owned());
                self.connect_prompt = Some(prompt);
                return;
            }
            let profile = prompt.profile.clone();
            self.start_connection(profile, prompt.password, prompt.passphrase);
            return;
        }

        if !close {
            self.connect_prompt = Some(prompt);
        }
    }

    fn show_host_key_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = self.host_key_prompts.first() else {
            return;
        };
        let info = prompt.info.clone();
        let tab_id = prompt.tab_id;
        let mut trust = false;
        let mut cancel = false;

        let modal = egui::Modal::new(Id::unique("host-key-prompt")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.heading("Verify host key");
            ui.add_space(4.0);

            if let Some(previous) = &info.previous {
                ui.label(
                    RichText::new(
                        "WARNING: the host key has changed since you last connected. \
                         This could indicate a man-in-the-middle attack.",
                    )
                    .color(RED)
                    .strong(),
                );
                ui.add_space(4.0);
                ui.label(RichText::new(format!("Previous fingerprint: {previous}")).color(MUTED));
                ui.add_space(4.0);
            }

            ui.label(format!(
                "The authenticity of {} can't be established.",
                info.display_host
            ));
            ui.add_space(6.0);
            egui::Grid::new("host-key-fields")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Algorithm");
                    ui.label(RichText::new(&info.algorithm).monospace());
                    ui.end_row();
                    ui.label("Fingerprint");
                    ui.label(RichText::new(&info.fingerprint).monospace().color(ACCENT));
                    ui.end_row();
                });
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "Only continue if this fingerprint matches the one shown by the server administrator.",
                )
                .small()
                .color(MUTED),
            );
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    cancel = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(
                            RichText::new("Trust and connect")
                                .strong()
                                .color(Color32::WHITE),
                        ))
                        .clicked()
                    {
                        trust = true;
                    }
                });
            });
        });

        if modal.should_close() {
            cancel = true;
        }

        if trust {
            let prompt = self.host_key_prompts.remove(0);
            self.known_hosts.insert(
                prompt.info.host_key.clone(),
                prompt.info.algorithm.clone(),
                prompt.info.fingerprint.clone(),
            );
            self.persist_known_hosts();
            if let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) {
                self.retry_tab(index);
            }
        } else if cancel {
            self.host_key_prompts.remove(0);
            if let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) {
                self.tabs[index].status = TabStatus::Failed("Host key was not trusted".to_owned());
                self.tabs[index].conn = None;
                self.tabs[index]
                    .terminal
                    .write_notice("Connection cancelled: host key was not trusted.");
            }
        }
    }

    fn show_delete_confirm(&mut self, ctx: &egui::Context) {
        let Some(id) = self.confirm_delete else {
            return;
        };
        let Some(profile) = self.store.get(id).cloned() else {
            self.confirm_delete = None;
            return;
        };
        let mut close = false;
        let mut delete = false;

        let modal = egui::Modal::new(Id::unique("delete-session")).show(ctx, |ui| {
            ui.set_width(380.0);
            ui.heading("Delete session?");
            ui.add_space(4.0);
            ui.label(format!(
                "\"{}\" will be removed from your saved sessions.",
                profile.display_name()
            ));
            ui.add_space(10.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    close = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new(
                            RichText::new("Delete").color(Color32::WHITE),
                        ))
                        .clicked()
                    {
                        delete = true;
                    }
                });
            });
        });

        if modal.should_close() {
            close = true;
        }

        if delete {
            self.store.remove(id);
            self.persist_sessions();
            if self.selected_profile == Some(id) {
                self.selected_profile = None;
            }
            self.confirm_delete = None;
            self.toast("Session deleted");
        } else if close {
            self.confirm_delete = None;
        }
    }

    fn show_tunnels_modal(&mut self, ctx: &egui::Context) {
        let Some(tab_id) = self.show_tunnels else {
            return;
        };
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            self.show_tunnels = None;
            return;
        };

        let tab = &self.tabs[index];
        let mut close = false;

        let modal = egui::Modal::new(Id::unique("tunnels")).show(ctx, |ui| {
            ui.set_width(620.0);
            ui.heading("SSH tunnels");
            ui.label(
                RichText::new(format!("{}@{}", tab.options.username, tab.options.target()))
                    .monospace()
                    .color(MUTED),
            );
            ui.add_space(6.0);

            if tab.options.tunnels.is_empty() {
                ui.label(RichText::new("This session has no tunnels configured.").color(MUTED));
            } else {
                for (tunnel_index, tunnel) in tab.options.tunnels.iter().enumerate() {
                    let state = tab.tunnel_states.get(tunnel_index);
                    egui::Frame::NONE
                        .fill(CARD_BG)
                        .corner_radius(6)
                        .inner_margin(Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(tunnel.summary()).monospace().size(11.5));
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if let Some(state) = state {
                                        let color = if state.active {
                                            GREEN
                                        } else if state.message.starts_with("Starting") {
                                            YELLOW
                                        } else {
                                            RED
                                        };
                                        ui.label(
                                            RichText::new(if state.active {
                                                "active"
                                            } else {
                                                "inactive"
                                            })
                                            .small()
                                            .color(color),
                                        );
                                    }
                                });
                            });
                            if let Some(state) = state {
                                ui.label(RichText::new(&state.message).small().color(MUTED));
                            }
                        });
                    ui.add_space(2.0);
                }
            }

            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Configure tunnels in the session editor, then reconnect to apply changes.",
                )
                .small()
                .color(MUTED),
            );
            ui.add_space(6.0);
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });

        if modal.should_close() {
            close = true;
        }
        if close {
            self.show_tunnels = None;
        }
    }

    fn show_about(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut close = false;
        let modal = egui::Modal::new(Id::unique("about")).show(ctx, |ui| {
            ui.set_width(540.0);
            ui.label(RichText::new("Rustty").size(32.0).strong().color(ACCENT));
            ui.label(
                RichText::new("A PuTTY-style SSH terminal client written in Rust").color(MUTED),
            );
            ui.add_space(8.0);
            ui.label("Built with egui, russh and vt100.");
            ui.label("SSH tunnels: local (-L), remote (-R), dynamic SOCKS5 (-D).");
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(RichText::new("Shortcuts").strong());
            for line in [
                "Ctrl+Shift+N   New session",
                "Ctrl+Shift+T   Quick connect",
                "Ctrl+Shift+W   Close tab",
                "Ctrl+Tab       Next tab",
                "Ctrl+Shift+C   Copy selection",
                "Ctrl+Shift+V   Paste",
                "Ctrl+Shift++   Larger font",
                "Ctrl+Shift+-   Smaller font",
                "Shift+PageUp / PageDown   Scroll back / forward",
                "Shift+Insert   Paste",
                "Right click    Copy / paste / select all",
            ] {
                ui.label(RichText::new(line).monospace().size(11.0));
            }
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Host keys are checked against known_hosts.json in the Rustty config folder. \
                     Passwords are never persisted.",
                )
                .small()
                .color(MUTED),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if close {
            self.show_about = false;
        }
    }

    fn show_toast(&mut self, ctx: &egui::Context) {
        let expired = self
            .toast
            .as_ref()
            .is_some_and(|toast| toast.at.elapsed() > Duration::from_secs(4));
        if expired {
            self.toast = None;
        }
        let Some(toast) = &self.toast else {
            return;
        };

        egui::Area::new(Id::unique("toast"))
            .anchor(Align2::RIGHT_TOP, Vec2::new(-18.0, 64.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::NONE
                    .fill(CARD_BG)
                    .stroke(Stroke::new(1.0, Color32::from_rgb(0x35, 0x3e, 0x4b)))
                    .corner_radius(8)
                    .inner_margin(Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.label(RichText::new(&toast.text).color(TEXT));
                    });
            });
        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

impl eframe::App for RusttyApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let had_modal = self.modal_open();

        self.pump_events();
        self.handle_shortcuts(&ctx);
        self.show_toolbar(ui);
        self.show_sidebar(ui);
        self.show_central(ui);
        self.show_dialogs(&ctx);
        self.show_toast(&ctx);

        if had_modal && !self.modal_open() {
            self.focus_request = self.active_tab;
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for tab in &self.tabs {
            if let Some(conn) = &tab.conn {
                conn.disconnect();
            }
        }
        let _ = self.store.save(&sessions_path());
        let _ = self.known_hosts.save(&known_hosts_path());
    }
}

fn status_color(status: &TabStatus) -> Color32 {
    match status {
        TabStatus::Connecting => YELLOW,
        TabStatus::Connected => GREEN,
        TabStatus::AwaitingHostKey => ORANGE,
        TabStatus::Closed(_) => MUTED,
        TabStatus::Failed(_) => RED,
    }
}

fn status_label(status: &TabStatus) -> String {
    match status {
        TabStatus::Connecting => "connecting".to_owned(),
        TabStatus::Connected => "connected".to_owned(),
        TabStatus::AwaitingHostKey => "waiting for host key approval".to_owned(),
        TabStatus::Closed(reason) => format!("disconnected - {reason}"),
        TabStatus::Failed(reason) => format!("failed - {reason}"),
    }
}

fn session_row(
    ui: &mut egui::Ui,
    profile: &SessionProfile,
    selected: bool,
    status: Option<Color32>,
) -> (egui::Response, Option<RowAction>) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 46.0), Sense::click());
    let response = response
        .on_hover_text("Double-click to connect")
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, 6.0, ACTIVE_BG);
    } else if response.hovered() {
        painter.rect_filled(rect, 6.0, HOVER_BG);
    }

    let name = profile.display_name();
    painter.text(
        rect.left_top() + Vec2::new(10.0, 6.0),
        Align2::LEFT_TOP,
        name,
        FontId::proportional(14.0),
        TEXT,
    );
    let mut subtitle = format!("{}@{}", profile.username, profile.target());
    if !profile.tunnels.is_empty() {
        subtitle.push_str(&format!("  [{} tunnel(s)]", profile.tunnels.len()));
    }
    painter.text(
        rect.left_top() + Vec2::new(10.0, 25.0),
        Align2::LEFT_TOP,
        subtitle,
        FontId::monospace(10.5),
        MUTED,
    );

    if let Some(color) = status {
        painter.circle_filled(rect.right_center() - Vec2::new(12.0, 0.0), 4.0, color);
    }

    let mut action = None;
    response.context_menu(|ui| {
        if ui.button("Connect").clicked() {
            action = Some(RowAction::Connect);
            ui.close();
        }
        if ui.button("Edit").clicked() {
            action = Some(RowAction::Edit);
            ui.close();
        }
        if ui.button("Duplicate").clicked() {
            action = Some(RowAction::Duplicate);
            ui.close();
        }
        ui.separator();
        if ui.button("Delete").clicked() {
            action = Some(RowAction::Delete);
            ui.close();
        }
    });

    (response, action)
}

fn configure_style(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL_BG;
    visuals.window_fill = Color32::from_rgb(0x1b, 0x20, 0x28);
    visuals.extreme_bg_color = Color32::from_rgb(0x0f, 0x13, 0x18);
    visuals.faint_bg_color = Color32::from_rgb(0x1e, 0x24, 0x2d);
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = Color32::from_rgb(0x26, 0x4f, 0x78);
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(0x22, 0x28, 0x32);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x1e, 0x24, 0x2c);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x2b, 0x33, 0x3f);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x28, 0x30, 0x3a);
    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, Color32::BLACK);
    ctx.set_visuals(visuals);

    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = Vec2::new(8.0, 8.0);
        style.spacing.button_padding = Vec2::new(10.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        style
            .text_styles
            .insert(egui::TextStyle::Heading, FontId::proportional(19.0));
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(13.5));
    });
}

fn install_fonts(ctx: &egui::Context) {
    let candidates = [
        "C:\\Windows\\Fonts\\CascadiaMono.ttf",
        "C:\\Windows\\Fonts\\consola.ttf",
        "C:\\Windows\\Fonts\\lucon.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        "/System/Library/Fonts/Menlo.ttc",
    ];

    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };

        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "rustty-mono".to_owned(),
            Arc::new(egui::FontData::from_owned(bytes)),
        );
        fonts
            .families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "rustty-mono".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .push("rustty-mono".to_owned());
        ctx.set_fonts(fonts);
        return;
    }
}
