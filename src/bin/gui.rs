//! neutronsync GUI — a native egui/eframe front-end over the `service::Controller`.
//!
//! Visual language follows Proton Drive's desktop client (see reference/gui and
//! reference/gui/mockup): a frameless dark window with a custom title bar, a left
//! navigation rail with an animated selector, a sync-status + Auto-sync footer,
//! and a detail pane with large titles. Motion is intentional: page crossfades,
//! hover fades, a pulsing status dot, an animated progress meter, a spinning
//! sync glyph, and sliding toasts.
//!
//! Build/run: `cargo run --features gui --bin neutronsync-gui`.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, FontFamily, FontId, Rect, RichText, Sense, Stroke,
};

use neutronsync::config::{self, Config, ConflictPolicy, LocalDelete, Pair, UpdateChannel};
use neutronsync::models::{Compare, Entry};
use neutronsync::protoncli::{ProtonCli, Remote};
use neutronsync::service::{ActivityKind, ActivityOp, AppState, Controller, PairState, Phase};
use neutronsync::updater::{self, UpdateInfo};

// --- palette (reference/gui/palette/palette.md) -----------------------------
const NAV_BG: Color32 = Color32::from_rgb(0x16, 0x15, 0x1C);
const PANEL: Color32 = Color32::from_rgb(0x1C, 0x1B, 0x24);
const PANEL2: Color32 = Color32::from_rgb(0x23, 0x22, 0x30);
const SEL: Color32 = Color32::from_rgb(0x2A, 0x28, 0x33);
const TEXT: Color32 = Color32::from_rgb(0xFF, 0xFF, 0xFF);
const DIM: Color32 = Color32::from_rgb(0x8E, 0x8B, 0x9A);
const DIM2: Color32 = Color32::from_rgb(0x6C, 0x6A, 0x7A);
const ACCENT: Color32 = Color32::from_rgb(0x6D, 0x4A, 0xFF);
const ACCENT_HI: Color32 = Color32::from_rgb(0x7C, 0x5C, 0xFF);
const ACCENT_LO: Color32 = Color32::from_rgb(0x5C, 0x3E, 0xDB);
const OK: Color32 = Color32::from_rgb(0x3C, 0xBB, 0x87);
const DANGER: Color32 = Color32::from_rgb(0xE0, 0x50, 0x64);
const WARN: Color32 = Color32::from_rgb(0xD8, 0xA0, 0x50);

fn line_col() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, 20)
}
fn hover_col() -> Color32 {
    Color32::from_rgba_unmultiplied(255, 255, 255, 14)
}

fn ff_bold() -> FontFamily {
    FontFamily::Name("bold".into())
}

// ============================================================================
// icons — hand-drawn line glyphs via the painter (no font/dep needed)
// ============================================================================
#[derive(Clone, Copy)]
enum Icon {
    Activity,
    Folder,
    Gear,
    User,
    Info,
    Sync,
    Check,
    Cloud,
    Hdd,
    Add,
    Trash,
    Arrow,
    Upload,
    Download,
    Copy,
}

fn icon_glyph(icon: Icon) -> &'static str {
    use egui_phosphor::regular as ph;
    match icon {
        Icon::Activity => ph::PULSE,
        Icon::Folder => ph::FOLDER,
        Icon::Gear => ph::GEAR,
        Icon::User => ph::USER,
        Icon::Info => ph::INFO,
        Icon::Sync => ph::ARROWS_CLOCKWISE,
        Icon::Check => ph::CHECK,
        Icon::Cloud => ph::CLOUD,
        Icon::Hdd => ph::HARD_DRIVE,
        Icon::Add => ph::PLUS,
        Icon::Trash => ph::TRASH,
        Icon::Arrow => ph::ARROW_RIGHT,
        Icon::Upload => ph::ARROW_UP,
        Icon::Download => ph::ARROW_DOWN,
        Icon::Copy => ph::COPY,
    }
}

fn draw_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        icon_glyph(icon),
        FontId::new(rect.height() * 0.98, FontFamily::Proportional),
        color,
    );
}

/// Paint the NeutronSync logo (the atom mark from `assets/logo.png`) into `rect`.
/// Falls back to a filled disc if the texture isn't loaded yet.
fn draw_logo(painter: &egui::Painter, rect: Rect, tex: Option<&egui::TextureHandle>) {
    if let Some(tex) = tex {
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        painter.image(tex.id(), rect, uv, Color32::WHITE);
    } else {
        painter.circle_filled(rect.center(), rect.width() * 0.5, ACCENT);
    }
}

// ============================================================================
// small helpers
// ============================================================================
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgba_unmultiplied(
        l(a.r(), b.r()),
        l(a.g(), b.g()),
        l(a.b(), b.b()),
        l(a.a(), b.a()),
    )
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn rel_time(ts: i64) -> String {
    let d = (now_secs() - ts).max(0);
    match d {
        0..=44 => "just now".into(),
        45..=89 => "1 minute ago".into(),
        90..=3599 => format!("{} minutes ago", d / 60),
        3600..=7199 => "1 hour ago".into(),
        7200..=86399 => format!("{} hours ago", d / 3600),
        86400..=172799 => "yesterday".into(),
        _ => format!("{} days ago", d / 86400),
    }
}

fn galley_w(ui: &egui::Ui, text: &str, font: FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, TEXT)
        .size()
        .x
}

/// Front-elide (keep the tail, which is the useful part of a path) so `text`
/// fits within `max_w`, prefixing "…".
fn elide_front(ui: &egui::Ui, text: &str, font: FontId, max_w: f32) -> String {
    if galley_w(ui, text, font.clone()) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    for start in 1..chars.len() {
        let cand: String = std::iter::once('…')
            .chain(chars[start..].iter().copied())
            .collect();
        if galley_w(ui, &cand, font.clone()) <= max_w {
            return cand;
        }
    }
    "…".to_owned()
}

/// A raised surface (card / banner) with a soft drop shadow.
fn card_frame() -> egui::Frame {
    egui::Frame::default()
        .fill(PANEL2)
        .stroke(Stroke::new(1.0, line_col()))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(16))
        .shadow(egui::epaint::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: Color32::from_black_alpha(70),
        })
}

// ============================================================================
// widget toolkit
// ============================================================================
#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

/// A pill button with an optional leading icon, hover/press animation.
fn button(
    ui: &mut egui::Ui,
    icon: Option<Icon>,
    label: &str,
    kind: Btn,
    small: bool,
    enabled: bool,
) -> egui::Response {
    let h = if small { 30.0 } else { 34.0 };
    let fs = if small { 12.5 } else { 13.0 };
    let font = FontId::new(fs, ff_bold());
    let pad = 13.0;
    let icon_sz = if small { 14.0 } else { 15.0 };
    let gap = 7.0;
    let tw = galley_w(ui, label, font.clone());
    let iw = if icon.is_some() { icon_sz + gap } else { 0.0 };
    let w = pad * 2.0 + iw + tw;
    let (rect, resp) = ui.allocate_exact_size(
        vec2(w, h),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id, enabled && resp.hovered(), 0.12);
    let down = enabled && resp.is_pointer_button_down_on();
    let painter = ui.painter();

    let (bg, fg) = match kind {
        Btn::Primary => {
            let base = if down {
                ACCENT_LO
            } else {
                mix(ACCENT, ACCENT_HI, hov)
            };
            (base, TEXT)
        }
        Btn::Secondary => {
            let base = mix(PANEL2, mix(PANEL2, TEXT, 0.10), hov);
            (if down { ACCENT_LO } else { base }, TEXT)
        }
        Btn::Ghost => (
            mix(Color32::TRANSPARENT, hover_col(), hov),
            if enabled { TEXT } else { DIM2 },
        ),
        Btn::Danger => (
            mix(
                Color32::TRANSPARENT,
                Color32::from_rgba_unmultiplied(224, 80, 100, 40),
                hov,
            ),
            DANGER,
        ),
    };
    let fg = if enabled { fg } else { DIM2 };
    if bg.a() > 0 {
        painter.rect_filled(rect, 8.0, bg);
    }
    let mut x = rect.left() + pad;
    if let Some(ic) = icon {
        let ir = Rect::from_center_size(
            pos2(x + icon_sz / 2.0, rect.center().y),
            vec2(icon_sz, icon_sz),
        );
        draw_icon(painter, ir, ic, fg);
        x += icon_sz + gap;
    }
    painter.text(
        pos2(x, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        font,
        fg,
    );
    resp
}

/// An animated on/off switch. Returns true if toggled this frame.
fn switch(ui: &mut egui::Ui, on: bool) -> bool {
    let (w, h) = (40.0, 23.0);
    let (rect, resp) = ui.allocate_exact_size(vec2(w, h), Sense::click());
    let t = ui.ctx().animate_bool_with_time(resp.id, on, 0.16);
    let painter = ui.painter();
    let track = mix(PANEL2, ACCENT, t);
    painter.rect_filled(rect, h / 2.0, track);
    let kr = h / 2.0 - 3.0;
    let cx = rect.left() + h / 2.0 + t * (w - h);
    painter.circle_filled(pos2(cx, rect.center().y), kr, mix(DIM, TEXT, t));
    resp.clicked()
}

/// A thin progress meter with an animated fill.
fn meter(ui: &mut egui::Ui, width: f32, frac: f32, col: Color32, id: egui::Id) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, PANEL2);
    let f = ui
        .ctx()
        .animate_value_with_time(id, frac.clamp(0.0, 1.0), 0.5);
    if f > 0.001 {
        let fill = Rect::from_min_size(rect.min, vec2(rect.width() * f, rect.height()));
        painter.rect_filled(fill, 3.0, col);
    }
}

fn status_dot(ui: &mut egui::Ui, col: Color32, pulse: bool) {
    let (rect, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    let c = rect.center();
    let painter = ui.painter();
    if pulse {
        let t = ui.ctx().input(|i| i.time) as f32;
        let a = (0.5 + 0.5 * (t * 3.0).sin()).clamp(0.0, 1.0);
        painter.circle_filled(c, 6.0, col.gamma_multiply(0.25 * a));
    }
    painter.circle_filled(c, 4.0, col);
}

// ============================================================================
// app
// ============================================================================
#[derive(PartialEq, Clone, Copy)]
enum Nav {
    Activity,
    Folders,
    Settings,
    Account,
    About,
}

const NAV_ITEMS: [(Nav, Icon, &str); 5] = [
    (Nav::Activity, Icon::Activity, "Activity"),
    (Nav::Folders, Icon::Folder, "Folders"),
    (Nav::Settings, Icon::Gear, "Settings"),
    (Nav::Account, Icon::User, "Account"),
    (Nav::About, Icon::Info, "About"),
];

struct Toast {
    msg: String,
    err: bool,
    born: f64,
}

// ============================================================================
// system tray (tray-icon / GTK AppIndicator) — lives in the headless daemon
// ============================================================================

/// The app icon as a `tray_icon::Icon` (RGBA).
fn tray_icon_image() -> Option<tray_icon::Icon> {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/logo.png")).ok()?;
    tray_icon::Icon::from_rgba(icon.rgba, icon.width, icon.height).ok()
}

// ---------------------------------------------------------------------------
// single-instance locks + process spawning (daemon <-> window coordination)
// ---------------------------------------------------------------------------

/// Advisory PID lock: a file under `state_dir` holding a PID, cross-checked
/// against `/proc`. Removed on drop. Used so at most one daemon and one window
/// run at a time.
struct PidLock {
    path: PathBuf,
}

impl PidLock {
    /// Try to take the lock. Returns `None` if a live process already holds it.
    fn acquire(state_dir: &std::path::Path, name: &str) -> Option<Self> {
        let _ = std::fs::create_dir_all(state_dir);
        let path = state_dir.join(name);
        if pid_alive_from(&path) {
            return None;
        }
        std::fs::write(&path, std::process::id().to_string()).ok()?;
        Some(PidLock { path })
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Last-modified time of a file, if it exists (used to detect config changes).
fn file_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether the PID recorded in `lock` names a currently-running process.
fn pid_alive_from(lock: &std::path::Path) -> bool {
    if let Ok(s) = std::fs::read_to_string(lock) {
        if let Ok(pid) = s.trim().parse::<u32>() {
            return std::path::Path::new(&format!("/proc/{pid}")).exists();
        }
    }
    false
}

const DAEMON_LOCK: &str = "tray-daemon.lock";
const WINDOW_LOCK: &str = "window.lock";

/// Is a headless tray daemon already running for this state dir?
fn daemon_running(state_dir: &std::path::Path) -> bool {
    pid_alive_from(&state_dir.join(DAEMON_LOCK))
}

/// Is a GUI window already open for this state dir?
fn window_running(state_dir: &std::path::Path) -> bool {
    pid_alive_from(&state_dir.join(WINDOW_LOCK))
}

/// Path of our own executable (falls back to the on-PATH name).
fn self_exe() -> std::ffi::OsString {
    std::env::current_exe()
        .map(std::ffi::OsString::from)
        .unwrap_or_else(|_| "neutronsync-gui".into())
}

/// Propagate session-related env vars to a child process. Autostart and manual
/// spawns must reach the OS keyring via D-Bus; without these the CLI reports
/// "No session loaded" even when the user is signed in elsewhere.
fn inherit_session_env(cmd: &mut std::process::Command) {
    for key in [
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "SSH_AUTH_SOCK",
        "DISPLAY",
        "WAYLAND_DISPLAY",
    ] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
}

/// Launch a detached GUI window process (no `--tray`).
fn spawn_window() {
    let mut cmd = std::process::Command::new(self_exe());
    inherit_session_env(&mut cmd);
    let _ = cmd.spawn();
}

/// Launch a detached headless tray daemon (`--tray`).
fn spawn_daemon() {
    let mut cmd = std::process::Command::new(self_exe());
    cmd.arg("--tray");
    inherit_session_env(&mut cmd);
    let _ = cmd.spawn();
}

/// Terminate the open GUI window process, if any. Used by the tray "Quit" so a
/// full quit closes the window too — not just the daemon — giving one coherent
/// exit path (closing the window alone still leaves the tray syncing). A killed
/// window can't corrupt state: the baseline is only committed at the end of a
/// completed sync, so an aborted transfer is simply retried next run.
fn terminate_window(state_dir: &std::path::Path) {
    let lock = state_dir.join(WINDOW_LOCK);
    if let Ok(s) = std::fs::read_to_string(&lock) {
        if let Ok(pid) = s.trim().parse::<u32>() {
            if std::path::Path::new(&format!("/proc/{pid}")).exists() {
                let _ = std::process::Command::new("kill")
                    .arg(pid.to_string())
                    .status();
            }
        }
    }
}

/// State of the "Check for updates" flow, shared with the checker thread.
enum UpdateState {
    Idle,
    Checking,
    Done(std::result::Result<UpdateInfo, String>),
    /// Streaming the package down: bytes so far, total (0 when unknown).
    Downloading {
        done: u64,
        total: u64,
    },
    /// Checking the download against the release's published SHA-256.
    Verifying,
    /// Handed to the package manager; the polkit prompt is on screen.
    Installing,
    /// Installed and ready; the app has to restart to run the new build.
    Installed {
        version: String,
    },
    /// Downloaded but not installed (no package manager owns this copy).
    Downloaded {
        path: PathBuf,
    },
    Failed(String),
}

struct App {
    ctrl: Controller,
    cfg: Config,
    config_path: PathBuf,
    config_loaded: bool,
    nav: Nav,
    nav_t: f32,
    dirty: bool,
    toast: Option<Toast>,
    // Update check: shared with the background checker thread.
    update: std::sync::Arc<std::sync::Mutex<UpdateState>>,
    confirm_logout: bool,
    confirm_reset: bool,
    // after launching browser login, poll the account until signed in (or deadline)
    login_poll_until: Option<f64>,
    last_account_poll: f64,

    // remote browser
    browser_open: bool,
    browser_path: String,
    browser_entries: Vec<Entry>,
    browser_loading: bool,
    browser_error: Option<String>,
    browser_rx: Option<std::sync::mpsc::Receiver<Result<Vec<Entry>, String>>>,

    // app logo texture (lazy-loaded from assets/logo.png)
    logo_tex: Option<egui::TextureHandle>,

    // exclude editor ("choose folders to sync" per pair)
    excl_open: Option<usize>,
    excl_entries: Vec<String>,
    excl_loading: bool,
    excl_error: Option<String>,
    excl_rx: Option<std::sync::mpsc::Receiver<Result<Vec<Entry>, String>>>,
    excl_newly: Vec<String>,
    confirm_remove_local: Option<(usize, Vec<String>)>,

    // When true (run_in_tray), a headless daemon owns the tray + watcher; this
    // window just ensures one exists and exits on close (the daemon lives on).
    mode_daemon: bool,
    _win_lock: Option<PidLock>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_theme(&cc.egui_ctx);
        ensure_desktop_integration();
        let config_path = config::find_config(None).unwrap_or_else(config::default_config_path);
        let (cfg, config_loaded) = match config::load(None) {
            Ok(c) => (c, true),
            Err(_) => (starter_config(&config_path), false),
        };
        let ctrl = Controller::new(cfg.clone());
        // With the tray on, a headless daemon owns the tray + watcher; this window
        // just ensures one exists, takes a window lock, and exits on close.
        let mode_daemon = cfg.run_in_tray;
        let win_lock = if mode_daemon {
            if !daemon_running(&cfg.state_dir) {
                spawn_daemon();
            }
            PidLock::acquire(&cfg.state_dir, WINDOW_LOCK)
        } else {
            None
        };

        let app = App {
            ctrl,
            cfg,
            config_path,
            config_loaded,
            nav: Nav::Activity,
            nav_t: 1.0,
            dirty: false,
            toast: None,
            update: std::sync::Arc::new(std::sync::Mutex::new(UpdateState::Idle)),
            confirm_logout: false,
            confirm_reset: false,
            login_poll_until: None,
            last_account_poll: 0.0,
            browser_open: false,
            browser_path: String::new(),
            browser_entries: Vec::new(),
            browser_loading: false,
            browser_error: None,
            browser_rx: None,
            logo_tex: None,
            excl_open: None,
            excl_entries: Vec::new(),
            excl_loading: false,
            excl_error: None,
            excl_rx: None,
            excl_newly: Vec::new(),
            confirm_remove_local: None,
            mode_daemon,
            _win_lock: win_lock,
        };
        // Resume Auto sync if it was on last time — but not in daemon mode, where
        // the daemon owns the watcher (only one watcher may hold the lock).
        if app.cfg.auto_sync && !app.mode_daemon {
            let names = app.auto_names();
            if !names.is_empty() {
                app.ctrl.start_watch(names);
            }
        }
        // Optional quiet update check on launch (notify only).
        if app.cfg.check_on_launch {
            spawn_update_check(
                app.update.clone(),
                app.cfg.update_channel,
                cc.egui_ctx.clone(),
            );
        }
        app
    }

    fn toast(&mut self, ctx: &egui::Context, msg: impl Into<String>, err: bool) {
        self.toast = Some(Toast {
            msg: msg.into(),
            err,
            born: ctx.input(|i| i.time),
        });
    }

    fn commit(&mut self) {
        self.ctrl.commit_config(self.cfg.clone());
    }

    /// When the tray is on, the headless daemon owns it. This window just makes
    /// sure a daemon is alive on close, then lets itself exit — the daemon keeps
    /// the tray + sync running in the background.
    fn pump_tray(&mut self, ctx: &egui::Context) {
        if self.mode_daemon
            && ctx.input(|i| i.viewport().close_requested())
            && !daemon_running(&self.cfg.state_dir)
        {
            spawn_daemon();
        }
    }

    /// Commit + write config without a toast (used for auto-save on change).
    fn save_silent(&mut self) {
        self.commit();
        if self.ctrl.save(&self.config_path).is_ok() {
            self.dirty = false;
            self.config_loaded = true;
        }
    }

    fn auto_names(&self) -> Vec<String> {
        auto_names_of(&self.cfg)
    }

    fn set_nav(&mut self, nav: Nav) {
        if self.nav != nav {
            self.nav = nav;
            self.nav_t = 0.0;
        }
    }

    // -- remote browser (add a pair from Proton Drive) ----------------------
    fn open_browser(&mut self) {
        self.browser_open = true;
        self.browser_path = self.cfg.remote_root.clone();
        self.browser_error = None;
        self.load_browser();
    }
    fn load_browser(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.browser_rx = Some(rx);
        self.browser_loading = true;
        self.browser_error = None;
        let cfg = self.cfg.clone();
        let path = self.browser_path.clone();
        thread::spawn(move || {
            let proton = ProtonCli::new(&cfg);
            let _ = tx.send(proton.list_dir(&path).map_err(|e| e.to_string()));
        });
    }

    // -- exclude editor ("choose folders to sync" for a pair) ---------------
    fn open_exclude_editor(&mut self, i: usize) {
        if i >= self.cfg.pairs.len() {
            return;
        }
        self.excl_open = Some(i);
        self.excl_entries.clear();
        self.excl_error = None;
        self.excl_newly.clear();
        self.excl_loading = true;
        let (tx, rx) = std::sync::mpsc::channel();
        self.excl_rx = Some(rx);
        let cfg = self.cfg.clone();
        let remote = self.cfg.pairs[i].remote.clone();
        thread::spawn(move || {
            let proton = ProtonCli::new(&cfg);
            let _ = tx.send(proton.list_dir(&remote).map_err(|e| e.to_string()));
        });
    }

    fn exclude_window(&mut self, ctx: &egui::Context) {
        let Some(i) = self.excl_open else {
            return;
        };
        if i >= self.cfg.pairs.len() {
            self.excl_open = None;
            return;
        }
        let name = self.cfg.pairs[i].name.clone();
        let mut win_open = true;
        let mut done = false;
        egui::Window::new(format!("Folders to sync — {name}"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .open(&mut win_open)
            .show(ctx, |ui| {
                ui.set_min_width(340.0);
                if self.excl_loading {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Loading folders…").color(DIM));
                    });
                } else if let Some(e) = self.excl_error.clone() {
                    ui.label(RichText::new(e).size(12.0).color(WARN));
                } else if self.excl_entries.is_empty() {
                    ui.label(
                        RichText::new("This Proton folder has no subfolders to choose from.")
                            .size(12.0)
                            .color(DIM),
                    );
                } else {
                    ui.label(
                        RichText::new(
                            "Untick a folder to stop syncing it. Excluded folders are left \
                             alone — the Proton copy is never touched.",
                        )
                        .size(12.0)
                        .color(DIM),
                    );
                    ui.add_space(8.0);
                    let entries = self.excl_entries.clone();
                    for n in &entries {
                        let mut on = !self.cfg.pairs[i].exclude.iter().any(|e| e == n);
                        if ui.checkbox(&mut on, n).changed() {
                            if on {
                                self.cfg.pairs[i].exclude.retain(|e| e != n);
                                self.excl_newly.retain(|x| x != n);
                            } else {
                                if !self.cfg.pairs[i].exclude.iter().any(|e| e == n) {
                                    self.cfg.pairs[i].exclude.push(n.clone());
                                }
                                if self.cfg.pairs[i].local.join(n).exists()
                                    && !self.excl_newly.contains(n)
                                {
                                    self.excl_newly.push(n.clone());
                                }
                            }
                            self.dirty = true;
                        }
                    }
                    ui.add_space(10.0);
                    if button(ui, None, "Done", Btn::Primary, false, true).clicked() {
                        done = true;
                    }
                }
            });
        if !win_open || done {
            self.excl_open = None;
            self.commit();
            if !self.excl_newly.is_empty() {
                let newly = std::mem::take(&mut self.excl_newly);
                self.confirm_remove_local = Some((i, newly));
            }
        }
    }

    fn remove_local_dialog(&mut self, ctx: &egui::Context) {
        let Some((i, folders)) = self.confirm_remove_local.clone() else {
            return;
        };
        if i >= self.cfg.pairs.len() {
            self.confirm_remove_local = None;
            return;
        }
        let local = self.cfg.pairs[i].local.clone();
        let mut choice: Option<bool> = None;
        egui::Window::new("Free up local space?")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_min_width(360.0);
                ui.label(
                    RichText::new("These folders will no longer sync:")
                        .size(13.0)
                        .color(TEXT),
                );
                for f in &folders {
                    ui.label(RichText::new(format!("   • {f}")).size(12.0).color(DIM));
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Remove them from this computer to free up space? Your Proton copy \
                         stays untouched, and they re-download if you include them again.",
                    )
                    .size(12.0)
                    .color(DIM),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if button(
                        ui,
                        Some(Icon::Trash),
                        "Remove locally",
                        Btn::Danger,
                        false,
                        true,
                    )
                    .clicked()
                    {
                        choice = Some(true);
                    }
                    if button(ui, None, "Keep files", Btn::Secondary, false, true).clicked() {
                        choice = Some(false);
                    }
                });
            });
        if let Some(remove) = choice {
            if remove {
                let mut n = 0usize;
                for f in &folders {
                    let p = local.join(f);
                    if p.exists() && neutronsync::trash::trash_local(&p, None).is_ok() {
                        n += 1;
                    }
                }
                self.toast(
                    ctx,
                    format!("Moved {n} folder(s) to trash — your Proton copy is untouched."),
                    false,
                );
            }
            self.confirm_remove_local = None;
        }
    }

    fn add_remote(&mut self, ctx: &egui::Context, remote_path: String) {
        if let Some(dir) = rfd::FileDialog::new()
            .set_title(format!("Local folder to sync with {remote_path}"))
            .pick_folder()
        {
            let name = remote_path
                .rsplit('/')
                .next()
                .unwrap_or("folder")
                .to_string();
            self.cfg.pairs.push(Pair {
                name: name.clone(),
                local: dir,
                remote: remote_path,
                auto: true,
                exclude: Vec::new(),
            });
            self.commit();
            self.dirty = true;
            self.browser_open = false;
            self.ctrl.sync(vec![name], false); // auto-initialize
            self.set_nav(Nav::Activity);
            self.toast(ctx, "Added — initializing…", false);
        }
    }
}

fn install_theme(ctx: &egui::Context) {
    // fonts: Noto Sans as the proportional face + a bold family for headings.
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "noto".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/NotoSans-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "noto-bold".to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/NotoSans-Bold.ttf"
        ))),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "noto".to_owned());
    fonts.families.insert(
        FontFamily::Name("bold".into()),
        vec!["noto-bold".to_owned(), "noto".to_owned()],
    );
    // Phosphor icon glyphs, merged as a fallback into the Proportional family so
    // icon code points render inline anywhere we draw text.
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0, line_col());
    visuals.extreme_bg_color = NAV_BG;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.5);
    visuals.hyperlink_color = ACCENT_HI;
    visuals.widgets.inactive.bg_fill = PANEL2;
    visuals.widgets.inactive.weak_bg_fill = PANEL2;
    visuals.widgets.hovered.bg_fill = SEL;
    visuals.widgets.hovered.weak_bg_fill = SEL;
    visuals.widgets.active.bg_fill = SEL;
    ctx.set_visuals(visuals);

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = vec2(8.0, 8.0);
        style.spacing.button_padding = vec2(10.0, 6.0);
    });
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(FontId::new(26.0, ff_bold()))
            .color(TEXT),
    );
}

impl eframe::App for App {
    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        let ctx = &ctx;
        if self.logo_tex.is_none() {
            if let Ok(ic) =
                eframe::icon_data::from_png_bytes(include_bytes!("../../assets/logo.png"))
            {
                let img = egui::ColorImage::from_rgba_unmultiplied(
                    [ic.width as usize, ic.height as usize],
                    &ic.rgba,
                );
                self.logo_tex =
                    Some(ctx.load_texture("neutron-logo", img, egui::TextureOptions::LINEAR));
            }
        }
        self.pump_tray(ctx);
        // pump the remote browser channel
        if let Some(rx) = &self.browser_rx {
            if let Ok(res) = rx.try_recv() {
                self.browser_rx = None;
                self.browser_loading = false;
                match res {
                    Ok(entries) => {
                        let mut dirs: Vec<Entry> =
                            entries.into_iter().filter(|e| e.is_dir).collect();
                        dirs.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
                        self.browser_entries = dirs;
                    }
                    Err(e) => self.browser_error = Some(e),
                }
            }
        }
        // pump the exclude-editor folder list
        if let Some(rx) = &self.excl_rx {
            if let Ok(res) = rx.try_recv() {
                self.excl_rx = None;
                self.excl_loading = false;
                match res {
                    Ok(entries) => {
                        let mut dirs: Vec<String> = entries
                            .into_iter()
                            .filter(|e| e.is_dir)
                            .map(|e| e.path)
                            .collect();
                        dirs.sort_by_key(|s| s.to_lowercase());
                        self.excl_entries = dirs;
                    }
                    Err(e) => self.excl_error = Some(e),
                }
            }
        }

        // In tray mode the background daemon owns the watcher/sync in a separate
        // process, so this window's own controller is idle. Display the daemon's
        // published live state instead (scanning, syncing, per-file ops), keeping
        // this window's own account check. Falls back to our own state if the
        // daemon hasn't published yet.
        let snap = {
            let mine = self.ctrl.snapshot();
            if self.mode_daemon {
                match Controller::read_status(&self.cfg.state_dir) {
                    Some(mut pubd) => {
                        pubd.account = mine.account.clone();
                        // The window probes auth directly; trust it over the
                        // daemon's possibly stale signed_out flag (up to 20s lag
                        // without a refresh signal).
                        if mine.account.checked {
                            pubd.signed_out =
                                mine.account.binary_found && !mine.account.signed_in;
                        }
                        pubd
                    }
                    None => mine,
                }
            } else {
                mine
            }
        };
        // Keep polling the daemon's published state while a window is open.
        if self.mode_daemon {
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
        // after a browser login, poll the account until signed in (or deadline),
        // so the app returns to itself without a manual Refresh.
        if let Some(until) = self.login_poll_until {
            let now = ctx.input(|i| i.time);
            if snap.account.signed_in || now > until {
                self.login_poll_until = None;
            } else if now - self.last_account_poll > 2.5 && !snap.account.checking {
                self.ctrl.refresh_account(true);
                self.last_account_poll = now;
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(250));
        }

        // advance page-fade animation
        if self.nav_t < 1.0 {
            let dt = ctx.input(|i| i.stable_dt).min(0.05);
            self.nav_t = (self.nav_t + dt / 0.26).min(1.0);
        }

        // The startup account probe lands on a background thread and egui only
        // repaints on input, so an idle window would show a stale pre-probe
        // frame forever (and never flip to the sign-in gate below). Keep
        // painting until the probe result has arrived.
        if !snap.account.checked || snap.account.checking {
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
        }

        // First-run / broken-prereq gate: show the sign-in page unless we have
        // CONFIRMED a working, signed-in CLI. Covers both "proton-drive missing"
        // (page_signin renders install guidance) and "present but signed out".
        let signin = snap.account.checked && !(snap.account.binary_found && snap.account.signed_in);

        if !signin {
            self.nav_rail(root_ui, &snap);
        }

        egui::containers::CentralPanel::default().show(root_ui, |ui| {
            if signin {
                self.page_signin(ui, &snap);
            } else {
                let t = ease_out(self.nav_t);
                ui.scope(|ui| {
                    ui.multiply_opacity(t);
                    let dy = (1.0 - t) * 8.0;
                    ui.add_space(dy);
                    match self.nav {
                        Nav::Activity => self.page_activity(ui, &snap),
                        Nav::Folders => self.page_folders(ui, &snap),
                        Nav::Settings => self.page_settings(ui),
                        Nav::Account => self.page_account(ui, &snap),
                        Nav::About => self.page_about(ui),
                    }
                });
            }
        });

        self.browser_window(ctx);
        self.exclude_window(ctx);
        self.remove_local_dialog(ctx);
        self.logout_dialog(ctx);
        self.reset_dialog(ctx);
        self.toast_overlay(ctx);

        // auto-save any config change (settings, folders, auto-sync) — no manual Save needed
        if self.dirty {
            self.save_silent();
        }

        if snap.busy || snap.watching || self.nav_t < 1.0 || self.toast.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

// -- nav rail ----------------------------------------------------------------
impl App {
    fn nav_rail(&mut self, root_ui: &mut egui::Ui, snap: &AppState) {
        let logo = self.logo_tex.clone();
        let frame = egui::Frame::default()
            .fill(NAV_BG)
            .inner_margin(egui::Margin::same(10));
        egui::containers::Panel::left("nav")
            .exact_size(224.0)
            .resizable(false)
            .frame(frame)
            .show(root_ui, |ui| {
                // brand
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let (lr, _) = ui.allocate_exact_size(vec2(26.0, 26.0), Sense::hover());
                    draw_logo(ui.painter(), lr, logo.as_ref());
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("NeutronSync")
                                .font(FontId::new(13.5, ff_bold()))
                                .color(TEXT),
                        );
                        ui.label(RichText::new("protondrive for linux").size(11.0).color(DIM));
                    });
                });
                ui.add_space(16.0);

                // nav items with animated selection indicator
                let row_h = 40.0;
                let gap = 4.0;
                let avail_w = ui.available_width();
                let sel_index = NAV_ITEMS
                    .iter()
                    .position(|(n, _, _)| *n == self.nav)
                    .unwrap_or(0);

                // reserve rows and capture responses
                let mut resps = Vec::with_capacity(NAV_ITEMS.len());
                let mut rects = Vec::with_capacity(NAV_ITEMS.len());
                for _ in NAV_ITEMS.iter() {
                    let (rect, resp) = ui.allocate_exact_size(vec2(avail_w, row_h), Sense::click());
                    rects.push(rect);
                    resps.push(resp);
                    ui.add_space(gap);
                }

                // indicator (behind), animated y — anchored to the real selected row
                let sel_rect = rects[sel_index];
                let target_y = sel_rect.top();
                let cur_y =
                    ui.ctx()
                        .animate_value_with_time(egui::Id::new("nav_ind"), target_y, 0.28);
                let x = sel_rect.left();
                let w = sel_rect.width();
                let painter = ui.painter();
                painter.rect_filled(
                    Rect::from_min_size(pos2(x, cur_y), vec2(w, row_h)),
                    8.0,
                    SEL,
                );
                painter.rect_filled(
                    Rect::from_min_size(pos2(x, cur_y + 9.0), vec2(3.0, row_h - 18.0)),
                    2.0,
                    ACCENT,
                );

                // labels/icons on top
                let mut clicked = None;
                for (i, (nav, icon, label)) in NAV_ITEMS.iter().enumerate() {
                    let rect = rects[i];
                    let resp = &resps[i];
                    let selected = *nav == self.nav;
                    let hov =
                        ui.ctx()
                            .animate_bool_with_time(resp.id, resp.hovered() && !selected, 0.12);
                    if hov > 0.0 && !selected {
                        ui.painter().rect_filled(
                            rect,
                            8.0,
                            mix(Color32::TRANSPARENT, hover_col(), hov),
                        );
                    }
                    let col = if selected { TEXT } else { mix(DIM, TEXT, hov) };
                    let icol = if selected { ACCENT } else { col };
                    let ir = Rect::from_center_size(
                        pos2(rect.left() + 24.0, rect.center().y),
                        vec2(18.0, 18.0),
                    );
                    draw_icon(ui.painter(), ir, *icon, icol);
                    ui.painter().text(
                        pos2(rect.left() + 44.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        *label,
                        FontId::new(
                            13.0,
                            if selected {
                                ff_bold()
                            } else {
                                FontFamily::Proportional
                            },
                        ),
                        col,
                    );
                    if resp.clicked() {
                        clicked = Some(*nav);
                    }
                }
                if let Some(n) = clicked {
                    self.set_nav(n);
                }

                // footer: status + auto-sync master
                egui::containers::Panel::bottom("nav_foot")
                    .frame(
                        egui::Frame::default()
                            .fill(NAV_BG)
                            .inner_margin(egui::Margin {
                                left: 4,
                                right: 4,
                                top: 8,
                                bottom: 4,
                            }),
                    )
                    .show_separator_line(false)
                    .show(ui, |ui| {
                        ui.painter().hline(
                            ui.min_rect().x_range(),
                            ui.min_rect().top(),
                            Stroke::new(1.0, line_col()),
                        );
                        ui.add_space(8.0);
                        self.footer_status(ui, snap);
                        ui.add_space(12.0);
                        self.footer_auto(ui, snap);
                        ui.add_space(4.0);
                    });
            });
    }

    fn footer_status(&self, ui: &mut egui::Ui, snap: &AppState) {
        let last = snap.pairs.iter().filter_map(|p| p.last_synced).max();
        let any_err = snap.pairs.iter().any(|p| p.phase == Phase::Error);
        let (col, pulse, text) = if snap.busy {
            (ACCENT, true, "Syncing…".to_string())
        } else if snap.watching {
            (ACCENT, true, "Auto sync on".to_string())
        } else if any_err {
            (DANGER, false, "Sync error".to_string())
        } else if let Some(ts) = last {
            (OK, false, format!("Synced {}", rel_time(ts)))
        } else {
            (DIM, false, "Idle".to_string())
        };
        ui.horizontal(|ui| {
            status_dot(ui, col, pulse);
            ui.add_space(2.0);
            ui.label(RichText::new(text).size(12.5).color(TEXT));
        });
    }

    fn footer_auto(&mut self, ui: &mut egui::Ui, snap: &AppState) {
        let n = self.auto_names().len();
        // In tray mode the background daemon owns the watcher, so this window
        // shows/edits the *setting* (cfg.auto_sync) rather than its own watch
        // state — starting a second watcher here would fight over the lock.
        let on = if self.mode_daemon {
            self.cfg.auto_sync
        } else {
            snap.watching
        };
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Auto sync").size(12.5).color(TEXT));
                let sub = if on && self.mode_daemon {
                    format!(
                        "On · tray is watching {n} folder{}",
                        if n == 1 { "" } else { "s" }
                    )
                } else if on {
                    format!("On · watching {n} folder{}", if n == 1 { "" } else { "s" })
                } else {
                    "Off · sync on demand".to_string()
                };
                ui.label(RichText::new(sub).size(11.0).color(DIM));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if switch(ui, on) {
                    if self.mode_daemon {
                        // Just update the setting; the tray daemon applies it.
                        let ctx = ui.ctx().clone();
                        if on {
                            self.cfg.auto_sync = false;
                            self.dirty = true;
                            self.toast(&ctx, "Auto sync off — restart the tray to apply.", false);
                        } else if n == 0 {
                            self.toast(&ctx, "No folders set to Auto.", true);
                        } else {
                            self.cfg.auto_sync = true;
                            self.dirty = true;
                            self.toast(&ctx, "Auto sync on — restart the tray to apply.", false);
                        }
                    } else if on {
                        self.ctrl.stop_watch();
                        self.cfg.auto_sync = false;
                        self.dirty = true;
                    } else if n == 0 {
                        let ctx = ui.ctx().clone();
                        self.toast(&ctx, "No folders set to Auto.", true);
                    } else {
                        self.commit();
                        self.ctrl.start_watch(self.auto_names());
                        self.cfg.auto_sync = true;
                        self.dirty = true;
                    }
                }
            });
        });
    }
}

// -- pages -------------------------------------------------------------------
impl App {
    fn page_header(
        &mut self,
        ui: &mut egui::Ui,
        title: &str,
        actions: impl FnOnce(&mut egui::Ui, &mut Self),
    ) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            heading(ui, title);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                actions(ui, self);
            });
        });
        ui.add_space(14.0);
    }

    fn page_activity(&mut self, ui: &mut egui::Ui, snap: &AppState) {
        let busy = snap.busy;
        let watching = snap.watching;
        // A scan or sync is running (possibly in the background daemon, whose
        // published state sets a pair's phase without setting `busy`).
        let scanning = snap.pairs.iter().any(|p| p.phase == Phase::Scanning);
        let syncing = snap.pairs.iter().any(|p| p.phase == Phase::Syncing);
        let active = busy || scanning || syncing;
        self.page_header(ui, "Activity", |ui, app| {
            if active {
                // Only the window's OWN sync can be cancelled from here; a
                // background daemon run just shows as busy.
                if busy && button(ui, None, "Cancel", Btn::Secondary, false, true).clicked() {
                    app.ctrl.cancel();
                }
            } else {
                if button(ui, Some(Icon::Sync), "Sync now", Btn::Primary, false, true).clicked() {
                    app.commit();
                    app.ctrl.sync(vec![], false);
                }
                if button(ui, None, "Dry run", Btn::Ghost, false, true).clicked() {
                    app.commit();
                    app.ctrl.sync(vec![], true);
                }
            }
        });

        // banner. Reflect the LIVE phase: a pair mid-scan or mid-sync shows a
        // working banner even when `busy` is false (a background scan doesn't set
        // busy) and even for an already-synced pair — otherwise an active rescan
        // wrongly reads as the idle "watching" state.
        let never_synced = snap.pairs.iter().any(|p| {
            matches!(p.phase, Phase::Scanning | Phase::Syncing) && p.last_synced.is_none()
        });
        let (bcol, bicon, btitle, bsub) = if snap.signed_out {
            // A signed-out session is surfaced explicitly — everything else is
            // blocked on it, so it takes priority over scan/sync/idle states.
            (
                DANGER,
                Icon::Info,
                "Signed out of Proton",
                "Sign in on the Account tab to resume syncing".into(),
            )
        } else if active {
            (
                ACCENT,
                Icon::Sync,
                if never_synced {
                    "Initializing…"
                } else if syncing || busy {
                    "Syncing…"
                } else {
                    "Scanning…"
                },
                current_op_line(snap),
            )
        } else if watching {
            (
                ACCENT,
                Icon::Cloud,
                "Auto sync on",
                format!("Watching {} folder(s) for changes", self.auto_names().len()),
            )
        } else if snap.pairs.iter().any(|p| p.phase == Phase::Error) {
            (
                DANGER,
                Icon::Info,
                "Last sync had errors",
                "See the log below".into(),
            )
        } else {
            (OK, Icon::Check, "Synced", last_synced_line(snap))
        };
        activity_banner(ui, bcol, bicon, btitle, &bsub, busy);

        ui.add_space(14.0);

        // live in-progress operations, pinned above the feed
        if snap.active.len() > 1 {
            ui.label(
                RichText::new(format!("{} files in flight", snap.active.len()))
                    .font(FontId::new(11.0, ff_bold()))
                    .color(ACCENT),
            );
            ui.add_space(4.0);
        }
        for op in &snap.active {
            active_row(ui, op);
        }
        if !snap.active.is_empty() {
            ui.add_space(6.0);
        }

        // The feed shows file movements and real errors only — plan/summary
        // lines ("2 applied", "delete-remote=1") are log noise, not activity.
        let rows: Vec<&neutronsync::service::ActivityItem> = snap
            .activity
            .iter()
            .rev()
            .filter(|a| {
                a.op.is_some()
                    || (a.kind == ActivityKind::Error
                        && !a.text.starts_with("Signed out of Proton")
                        && !a.text.starts_with("Signed back in to Proton"))
            })
            .collect();
        let has_files = rows.iter().any(|a| a.op.is_some());
        if snap.active.is_empty() && rows.is_empty() {
            empty_state(
                ui,
                "No activity yet",
                "Run a sync or turn on Auto sync to see files move here.",
            );
            return;
        }

        if has_files {
            ui.scope(|ui| {
                ui.set_max_width((ui.available_width() - 15.0).max(200.0));
                activity_cols(ui, "FOLDER", DIM2, "WHEN", DIM2, false, |ui| {
                    ui.add_space(32.0);
                    ui.label(RichText::new("NAME").size(11.0).color(DIM2));
                });
                ui.add_space(4.0);
                ui.separator();
            });
        }

        // map pair name -> local root, to open files/folders on click
        let locals: std::collections::HashMap<String, PathBuf> = self
            .cfg
            .pairs
            .iter()
            .map(|p| (p.name.clone(), p.local.clone()))
            .collect();
        let mut open_target: Option<PathBuf> = None;

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width((ui.available_width() - 15.0).max(200.0));
                for item in &rows {
                    match &item.op {
                        Some(op) => {
                            if let Some(act) = activity_row(ui, op, item.ts, &item.text) {
                                if let Some(root) = locals.get(&op.pair) {
                                    open_target = Some(match act {
                                        Open::File => root.join(&op.path),
                                        Open::Folder => {
                                            let parent = op
                                                .path
                                                .rsplit_once('/')
                                                .map(|(d, _)| d)
                                                .unwrap_or("");
                                            root.join(parent)
                                        }
                                    });
                                }
                            }
                        }
                        None => {
                            // a real error with no file attached
                            activity_cols(ui, "", DIM2, &rel_time(item.ts), DIM2, false, |ui| {
                                let (ir, _) =
                                    ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                                ui.painter()
                                    .rect_filled(ir, 6.0, DANGER.gamma_multiply(0.16));
                                draw_icon(ui.painter(), ir.shrink(5.0), Icon::Info, DANGER);
                                ui.add_space(8.0);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&item.text).size(13.0).color(DANGER),
                                    )
                                    .truncate(),
                                );
                            });
                            ui.add_space(4.0);
                        }
                    }
                }
            });

        if let Some(p) = open_target {
            open_path(&p);
        }
    }

    fn page_folders(&mut self, ui: &mut egui::Ui, snap: &AppState) {
        let busy = snap.busy;
        let mut do_add = false;
        let mut do_browse = false;
        self.page_header(ui, "Folders", |ui, _app| {
            if button(ui, Some(Icon::Add), "Add folder", Btn::Primary, false, true).clicked() {
                do_add = true;
            }
            if button(
                ui,
                Some(Icon::Cloud),
                "Browse Proton",
                Btn::Secondary,
                false,
                true,
            )
            .clicked()
            {
                do_browse = true;
            }
        });

        ui.label(RichText::new("Each pair keeps a local folder and a Proton Drive folder in sync, both ways. Sync one on demand, or set it to Auto to keep it live while Auto sync is on.").size(13.0).color(DIM));
        ui.add_space(12.0);

        let mut remove: Option<usize> = None;
        let mut sync_one: Option<String> = None;
        let mut open_excl: Option<usize> = None;
        let mut changed = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width((ui.available_width() - 15.0).max(200.0));
                if self.cfg.pairs.is_empty() {
                    empty_state(
                        ui,
                        "No folders yet",
                        "Add a local folder, or browse your Proton Drive to pick one.",
                    );
                }
                for i in 0..self.cfg.pairs.len() {
                    let ps = snap
                        .pairs
                        .iter()
                        .find(|p| p.name == self.cfg.pairs[i].name)
                        .cloned();
                    card_frame().show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (ir, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                            ui.painter()
                                .rect_filled(ir, 8.0, ACCENT.gamma_multiply(0.18));
                            draw_icon(ui.painter(), ir.shrink(7.0), Icon::Folder, ACCENT);
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.cfg.pairs[i].name)
                                        .desired_width(180.0)
                                        .font(FontId::new(14.0, ff_bold()))
                                        .frame(egui::Frame::NONE),
                                );
                                pair_status(
                                    ui,
                                    ps.as_ref(),
                                    self.cfg.pairs[i].auto && snap.watching,
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if button(
                                        ui,
                                        Some(Icon::Trash),
                                        "Remove",
                                        Btn::Danger,
                                        true,
                                        true,
                                    )
                                    .clicked()
                                    {
                                        remove = Some(i);
                                    }
                                    let syncing = ps.as_ref().map_or(false, |p| {
                                        matches!(p.phase, Phase::Scanning | Phase::Syncing)
                                    });
                                    if button(
                                        ui,
                                        Some(Icon::Sync),
                                        "Sync",
                                        Btn::Primary,
                                        true,
                                        !busy && !syncing,
                                    )
                                    .clicked()
                                    {
                                        sync_one = Some(self.cfg.pairs[i].name.clone());
                                    }
                                    ui.add_space(4.0);
                                    ui.label(RichText::new("Auto").size(12.0).color(DIM));
                                    if switch(ui, self.cfg.pairs[i].auto) {
                                        self.cfg.pairs[i].auto = !self.cfg.pairs[i].auto;
                                        changed = true;
                                    }
                                },
                            );
                        });

                        // progress bar while active
                        if let Some(p) = &ps {
                            if matches!(p.phase, Phase::Syncing) && p.progress.total > 0 {
                                ui.add_space(8.0);
                                meter(
                                    ui,
                                    ui.available_width(),
                                    p.progress.fraction(),
                                    ACCENT,
                                    egui::Id::new(("pm", i)),
                                );
                            }
                        }

                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            let avail = ui.available_width();
                            let each = ((avail - 34.0) / 2.0).clamp(80.0, 400.0);
                            chip(
                                ui,
                                Icon::Hdd,
                                &self.cfg.pairs[i].local.display().to_string(),
                                each,
                            );
                            ui.add_space(2.0);
                            let (ar, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                            draw_icon(ui.painter(), ar, Icon::Arrow, ACCENT);
                            ui.add_space(2.0);
                            chip(ui, Icon::Cloud, &self.cfg.pairs[i].remote.clone(), each);
                        });

                        ui.add_space(8.0);
                        let nx = self.cfg.pairs[i].exclude.len();
                        let flabel = if nx == 0 {
                            "Choose folders to sync…".to_string()
                        } else {
                            format!("Choose folders to sync…  ({nx} excluded)")
                        };
                        if button(ui, Some(Icon::Folder), &flabel, Btn::Ghost, true, !busy)
                            .clicked()
                        {
                            open_excl = Some(i);
                        }
                    });
                    ui.add_space(10.0);
                }
            });

        if let Some(idx) = open_excl {
            self.open_exclude_editor(idx);
        }
        if let Some(i) = remove {
            // Purge the folder's stored data too, so re-adding a same-named
            // folder later starts fresh instead of inheriting a stale baseline.
            let name = self.cfg.pairs[i].name.clone();
            self.ctrl.forget_pair(&name);
            self.cfg.pairs.remove(i);
            changed = true;
        }
        if do_add {
            if let Some(dir) = rfd::FileDialog::new()
                .set_title("Choose a folder to sync")
                .pick_folder()
            {
                let name = dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("folder")
                    .to_string();
                let remote = config::remote_join(&self.cfg.remote_root, &name);
                self.cfg.pairs.push(Pair {
                    name: name.clone(),
                    local: dir,
                    remote,
                    auto: true,
                    exclude: Vec::new(),
                });
                changed = true;
                sync_one = Some(name); // auto-initialize: scan both sides + first sync
            }
        }
        if do_browse {
            self.open_browser();
        }
        if changed {
            self.commit();
            self.dirty = true;
        }
        if let Some(name) = sync_one {
            self.commit();
            self.ctrl.sync(vec![name], false);
            self.set_nav(Nav::Activity);
        }
    }

    fn page_settings(&mut self, ui: &mut egui::Ui) {
        self.page_header(ui, "Settings", |_ui, _app| {});
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // leave a little room on the right so cards clear the scrollbar
                ui.set_max_width((ui.available_width() - 15.0).max(200.0));

                settings_group(ui, "GENERAL", |ui| {
                    let tray = self.cfg.run_in_tray;
                    if setting_row(
                        ui,
                        "Run in system tray",
                        "Keep a background tray running so sync continues after you \
                         close the window. Takes effect next launch.",
                        |ui| switch(ui, tray),
                    ) {
                        self.cfg.run_in_tray = !tray;
                        self.dirty = true;
                        if autostart_enabled() {
                            set_autostart(true, &self.cfg);
                        }
                    }
                    let x11 = self.cfg.x11_compat;
                    if setting_row(
                        ui,
                        "X11 compatibility",
                        "Render the window through XWayland (the X11 backend) instead \
                         of native Wayland. Takes effect next launch.",
                        |ui| switch(ui, x11),
                    ) {
                        self.cfg.x11_compat = !x11;
                        self.dirty = true;
                        if autostart_enabled() {
                            set_autostart(true, &self.cfg);
                        }
                    }
                    let auto = autostart_enabled();
                    if setting_row(
                        ui,
                        "Launch at login",
                        "Start NeutronSync automatically when you log in.",
                        |ui| switch(ui, auto),
                    ) {
                        set_autostart(!auto, &self.cfg);
                    }
                });

                settings_group(ui, "DELETION", |ui| {
                    let on = self.cfg.propagate_deletes;
                    if setting_row(
                        ui,
                        "Propagate deletions",
                        "A file deleted on one side is removed on the other, recoverably.",
                        |ui| switch(ui, on),
                    ) {
                        self.cfg.propagate_deletes = !on;
                        self.dirty = true;
                    }
                    if setting_row(
                        ui,
                        "Local deletes go to",
                        "Remote deletes always go to Proton's online trash.",
                        |ui| combo_local_delete(ui, &mut self.cfg.local_delete),
                    ) {
                        self.dirty = true;
                    }
                });

                settings_group(ui, "CONFLICTS", |ui| {
                    if setting_row(
                        ui,
                        "When both sides changed",
                        "How to resolve a file edited in two places.",
                        |ui| combo_conflict(ui, &mut self.cfg.conflict),
                    ) {
                        self.dirty = true;
                    }
                });

                settings_group(ui, "UPDATES", |ui| {
                    if setting_row(
                        ui,
                        "Update channel",
                        "Stable follows releases you've promoted; Pre-release follows \
                         the newest published build.",
                        |ui| combo_update_channel(ui, &mut self.cfg.update_channel),
                    ) {
                        self.dirty = true;
                    }
                    let col = self.cfg.check_on_launch;
                    if setting_row(
                        ui,
                        "Check on launch",
                        "Quietly check for a newer version when the app starts. \
                         Notifies only; never installs anything.",
                        |ui| switch(ui, col),
                    ) {
                        self.cfg.check_on_launch = !col;
                        self.dirty = true;
                    }
                });

                settings_group(ui, "CHANGE DETECTION", |ui| {
                    if setting_row(
                        ui,
                        "Compare files by",
                        "Stronger checks cost more time per sync.",
                        |ui| combo_compare(ui, &mut self.cfg.compare),
                    ) {
                        self.dirty = true;
                    }
                });

                settings_group(ui, "ADVANCED", |ui| {
                    if setting_row(ui, "Remote root", "", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.remote_root)
                                .desired_width(220.0),
                        )
                        .changed()
                    }) {
                        self.dirty = true;
                    }
                    if setting_row(ui, "proton-drive binary", "", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.binary).desired_width(220.0),
                        )
                        .changed()
                    }) {
                        self.dirty = true;
                    }
                    let fc = self.cfg.fresh_cache;
                    if setting_row(
                        ui,
                        "Fresh metadata each run",
                        "Avoids stale directory listings from the CLI cache.",
                        |ui| switch(ui, fc),
                    ) {
                        self.cfg.fresh_cache = !fc;
                        self.dirty = true;
                    }
                });

                settings_group(ui, "DANGER ZONE", |ui| {
                    if setting_row(
                        ui,
                        "Reset NeutronSync",
                        "Remove all folders and wipe NeutronSync's stored data \
                         (baselines and sync history). Your actual files are not touched.",
                        |ui| {
                            button(ui, Some(Icon::Trash), "Reset…", Btn::Danger, true, true)
                                .clicked()
                        },
                    ) {
                        self.confirm_reset = true;
                    }
                });
            });
    }

    fn page_account(&mut self, ui: &mut egui::Ui, snap: &AppState) {
        self.page_header(ui, "Account", |_ui, _app| {});
        let acc = &snap.account;
        let (col, label) = if !acc.checked {
            (DIM, "Checking…")
        } else if acc.signed_in {
            (OK, "Signed in")
        } else if acc.binary_found {
            (DANGER, "Not signed in")
        } else {
            (WARN, "proton-drive not found")
        };
        ui.horizontal(|ui| {
            status_dot(ui, col, false);
            ui.add_space(2.0);
            ui.label(
                RichText::new(label)
                    .font(FontId::new(16.0, ff_bold()))
                    .color(TEXT),
            );
        });
        ui.add_space(10.0);
        ui.label(RichText::new("proton-drive").color(DIM));
        ui.label(RichText::new(&acc.version).size(12.5).color(DIM));
        ui.add_space(16.0);
        let waiting = self.login_poll_until.is_some();
        let checking = acc.checking || waiting;
        ui.horizontal(|ui| {
            if button(
                ui,
                Some(Icon::User),
                "Log in",
                Btn::Primary,
                false,
                !waiting,
            )
            .clicked()
            {
                let ctx = ui.ctx().clone();
                self.start_login(&ctx);
            }
            if button(ui, None, "Log out", Btn::Ghost, false, acc.signed_in).clicked() {
                self.confirm_logout = true;
            }
            if button(
                ui,
                Some(Icon::Sync),
                "Refresh",
                Btn::Ghost,
                false,
                !checking,
            )
            .clicked()
            {
                self.last_account_poll = ui.ctx().input(|i| i.time);
                self.ctrl.refresh_account(false);
            }
            if checking {
                ui.add_space(4.0);
                ui.spinner();
                ui.label(
                    RichText::new(if waiting {
                        "Waiting for sign-in…"
                    } else {
                        "Checking…"
                    })
                    .color(DIM),
                );
            }
        });
    }

    fn page_about(&mut self, ui: &mut egui::Ui) {
        self.page_header(ui, "About", |_ui, _app| {});
        ui.label(
            RichText::new(format!("NeutronSync {}", env!("CARGO_PKG_VERSION")))
                .font(FontId::new(16.0, ff_bold()))
                .color(TEXT),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Bidirectional Proton Drive folder sync, built on the official proton-drive CLI.",
            )
            .color(DIM),
        );
        ui.add_space(12.0);
        ui.label(
            RichText::new(format!("Config   {}", self.config_path.display()))
                .size(12.0)
                .color(DIM2),
        );
        ui.label(
            RichText::new(format!("State    {}", self.cfg.state_dir.display()))
                .size(12.0)
                .color(DIM2),
        );
        if !self.config_loaded {
            ui.add_space(6.0);
            ui.label(
                RichText::new("Config not yet saved — add folders and Save.")
                    .size(12.0)
                    .color(WARN),
            );
        }

        ui.add_space(20.0);
        ui.label(
            RichText::new("HOW IT WORKS")
                .font(FontId::new(11.0, ff_bold()))
                .color(ACCENT),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "NeutronSync is an independent, unofficial tool with no access to your \
                 Proton account. It simply drives Proton's official proton-drive CLI, which \
                 you sign in yourself. Your password and Proton credentials are never seen, \
                 stored, or sent by NeutronSync — the CLI keeps your session in your OS \
                 keyring, and all encryption and decryption is done by Proton's own software.",
            )
            .size(12.5)
            .color(DIM),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Not affiliated with, endorsed by, or sponsored by Proton AG. \"Proton\" and \
                 \"Proton Drive\" are trademarks of Proton AG.",
            )
            .size(11.5)
            .color(DIM2),
        );

        ui.add_space(16.0);
        ui.label(
            RichText::new("DETECTING CHANGES")
                .font(FontId::new(11.0, ff_bold()))
                .color(ACCENT),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Proton's CLI has no \"recently changed\" feed, so NeutronSync watches your \
                 local folders live and aims the work where the activity is. A local change \
                 reconciles just the folder that changed (not the whole tree); deeper changes \
                 arrive as their own events. On startup it syncs folders with fresh local \
                 changes first, then recently active folders, then everything else. A full \
                 walk of both sides is the safety net that catches edits made on your other \
                 devices (the CLI only reveals those by re-walking); it is paced to how long \
                 a walk takes, so a big tree is not re-walked constantly. Remote-side changes \
                 therefore appear on the next pass, not instantly.",
            )
            .size(12.5)
            .color(DIM),
        );

        ui.add_space(20.0);
        ui.label(
            RichText::new("UPDATES")
                .font(FontId::new(11.0, ff_bold()))
                .color(ACCENT),
        );
        ui.add_space(8.0);
        let channel = self.cfg.update_channel;
        ui.horizontal(|ui| {
            if button(
                ui,
                Some(Icon::Sync),
                "Check for updates",
                Btn::Secondary,
                false,
                true,
            )
            .clicked()
            {
                spawn_update_check(self.update.clone(), channel, ui.ctx().clone());
            }
            ui.add_space(10.0);
            let state = self.update.lock().unwrap();
            match &*state {
                UpdateState::Idle => {
                    ui.label(
                        RichText::new(format!("You're on {}.", updater::current_version()))
                            .size(12.0)
                            .color(DIM),
                    );
                }
                UpdateState::Checking => {
                    ui.label(RichText::new("Checking…").size(12.0).color(DIM));
                }
                UpdateState::Done(Ok(info)) if info.newer => {
                    ui.label(
                        RichText::new(format!(
                            "Update available: {} (you have {})",
                            info.latest, info.current
                        ))
                        .size(12.0)
                        .color(ACCENT),
                    );
                }
                UpdateState::Done(Ok(info)) => {
                    ui.label(
                        RichText::new(format!("Up to date ({}).", info.current))
                            .size(12.0)
                            .color(DIM),
                    );
                }
                UpdateState::Done(Err(e)) => {
                    ui.label(
                        RichText::new(format!("Couldn't check: {e}"))
                            .size(12.0)
                            .color(WARN),
                    );
                }
                UpdateState::Downloading { done, total } => {
                    let text = if *total > 0 {
                        format!(
                            "Downloading… {}% ({} of {})",
                            done * 100 / total.max(&1),
                            human_bytes(*done),
                            human_bytes(*total)
                        )
                    } else {
                        format!("Downloading… {}", human_bytes(*done))
                    };
                    ui.label(RichText::new(text).size(12.0).color(ACCENT));
                }
                UpdateState::Verifying => {
                    ui.label(
                        RichText::new("Verifying checksum…")
                            .size(12.0)
                            .color(ACCENT),
                    );
                }
                UpdateState::Installing => {
                    ui.label(
                        RichText::new("Installing… (confirm the password prompt)")
                            .size(12.0)
                            .color(ACCENT),
                    );
                }
                UpdateState::Installed { version } => {
                    ui.label(
                        RichText::new(format!("{version} installed. Restart to finish."))
                            .size(12.0)
                            .color(ACCENT),
                    );
                }
                UpdateState::Downloaded { path } => {
                    ui.label(
                        RichText::new(format!("Downloaded to {}", path.display()))
                            .size(12.0)
                            .color(DIM),
                    );
                }
                UpdateState::Failed(e) => {
                    ui.label(RichText::new(e.clone()).size(12.0).color(WARN));
                }
            }
        });

        // Decide the follow-up action without holding the lock across the UI.
        enum UpdateAction {
            Install(UpdateInfo),
            Restart,
            OpenRelease(String),
            None,
        }
        let (action, busy) = match &*self.update.lock().unwrap() {
            UpdateState::Done(Ok(info)) if info.newer => (
                if updater::install_kind().can_install()
                    && updater::pick_asset(info, updater::install_kind()).is_some()
                {
                    UpdateAction::Install(info.clone())
                } else {
                    UpdateAction::OpenRelease(info.html_url.clone())
                },
                false,
            ),
            UpdateState::Installed { .. } => (UpdateAction::Restart, false),
            UpdateState::Downloading { .. } | UpdateState::Verifying | UpdateState::Installing => {
                (UpdateAction::None, true)
            }
            UpdateState::Failed(_) => (UpdateAction::None, false),
            _ => (UpdateAction::None, false),
        };
        if busy {
            // Keep repainting so the byte counter actually moves.
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        }
        match action {
            UpdateAction::Install(info) => {
                ui.add_space(6.0);
                let label = format!("Download and install {}", info.latest);
                if button(ui, Some(Icon::Sync), &label, Btn::Primary, false, true).clicked() {
                    // The ONLY path that downloads or installs anything. Nothing
                    // updates on a timer, on launch, or as a side effect of a
                    // check: an update happens because this was pressed.
                    spawn_update_install(self.update.clone(), info, ui.ctx().clone());
                }
            }
            UpdateAction::Restart => {
                ui.add_space(6.0);
                if button(ui, None, "Restart now", Btn::Primary, false, true).clicked() {
                    if let Err(e) = updater::restart() {
                        *self.update.lock().unwrap() =
                            UpdateState::Failed(format!("Couldn't restart: {e}"));
                    } else {
                        // The helper stops this process; leaving is the point.
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            }
            UpdateAction::OpenRelease(url) if !url.is_empty() => {
                ui.add_space(6.0);
                if button(ui, None, "Open release", Btn::Primary, false, true).clicked() {
                    open_url(&url);
                }
            }
            _ => {}
        }

        ui.add_space(4.0);
        let chan = match channel {
            UpdateChannel::Stable => "stable",
            UpdateChannel::Prerelease => "pre-release",
        };
        let how = match updater::install_kind() {
            updater::InstallKind::Deb => "Updates install with apt, so your package list stays correct.",
            updater::InstallKind::Rpm => "Updates install with your rpm package manager, so your package list stays correct.",
            updater::InstallKind::Unmanaged => "This copy wasn't installed from a package, so updates are downloaded for you to install.",
        };
        ui.label(
            RichText::new(format!(
                "Following the {chan} channel. Releases come from GitHub; nothing is \
                 installed without you choosing to. {how}"
            ))
            .size(11.5)
            .color(DIM2),
        );
    }

    fn page_signin(&mut self, ui: &mut egui::Ui, snap: &AppState) {
        let found = snap.account.binary_found;
        let logo = self.logo_tex.clone();
        let col_w: f32 = 470.0;

        // A numbered step row: circled number, then a bold title.
        let step = |ui: &mut egui::Ui, n: &str, title: &str| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 10.0, SEL);
                ui.painter().text(
                    r.center(),
                    Align2::CENTER_CENTER,
                    n,
                    FontId::new(11.5, ff_bold()),
                    ACCENT_HI,
                );
                ui.add_space(8.0);
                ui.label(
                    RichText::new(title)
                        .font(FontId::new(13.5, ff_bold()))
                        .color(TEXT),
                );
            });
        };

        // Center the whole block vertically as one unit.
        let est_h = if found { 300.0 } else { 470.0 };
        ui.add_space(((ui.available_height() - est_h) / 2.0).max(24.0));
        let side = ((ui.available_width() - col_w) / 2.0).max(0.0);
        ui.horizontal(|ui| {
            ui.add_space(side);
            ui.vertical(|ui| {
                ui.set_width(col_w);
                ui.vertical_centered(|ui| {
                    let (lr, _) = ui.allocate_exact_size(vec2(72.0, 72.0), Sense::hover());
                    draw_logo(ui.painter(), lr, logo.as_ref());
                    ui.add_space(20.0);
                    let title = if found {
                        "Sign in to Proton Drive"
                    } else {
                        "Get the proton-drive CLI"
                    };
                    ui.label(
                        RichText::new(title)
                            .font(FontId::new(24.0, ff_bold()))
                            .color(TEXT),
                    );
                    ui.add_space(10.0);
                    let sub = if found {
                        "Signing in opens Proton's own login in your browser and signs in \
                         the official CLI. NeutronSync never sees your password or \
                         credentials."
                    } else {
                        "NeutronSync drives Proton's official command-line client, which \
                         isn't installed yet (or isn't on your PATH). Two steps and you're \
                         syncing:"
                    };
                    ui.label(RichText::new(sub).size(13.5).color(TEXT));
                });

                if !found {
                    ui.add_space(20.0);
                    card_frame().show(ui, |ui| {
                        ui.set_width(col_w - 34.0);
                        step(ui, "1", "Install the CLI");
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.add_space(28.0);
                            ui.hyperlink_to(
                                "proton.me/blog/proton-drive-cli",
                                "https://proton.me/blog/proton-drive-cli",
                            );
                        });
                        ui.add_space(16.0);
                        step(ui, "2", "Confirm it works in a terminal");
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.add_space(28.0);
                            egui::Frame::default()
                                .fill(NAV_BG)
                                .corner_radius(6.0)
                                .inner_margin(egui::Margin::symmetric(10, 6))
                                .show(ui, |ui| {
                                    ui.label(
                                        RichText::new("proton-drive --version")
                                            .font(FontId::new(12.5, egui::FontFamily::Monospace))
                                            .color(TEXT),
                                    );
                                });
                            ui.add_space(6.0);
                            if button(ui, Some(Icon::Copy), "Copy", Btn::Ghost, true, true)
                                .clicked()
                            {
                                ui.ctx().copy_text("proton-drive --version".to_owned());
                                let ctx = ui.ctx().clone();
                                self.toast(&ctx, "Command copied", false);
                            }
                        });
                    });
                }

                ui.add_space(22.0);
                let waiting = self.login_poll_until.is_some();
                ui.vertical_centered(|ui| {
                    if found {
                        ui.horizontal(|ui| {
                            ui.add_space((ui.available_width() - 290.0).max(0.0) / 2.0);
                            if button(
                                ui,
                                Some(Icon::User),
                                "Sign in with Proton",
                                Btn::Primary,
                                false,
                                !waiting,
                            )
                            .clicked()
                            {
                                let ctx = ui.ctx().clone();
                                self.start_login(&ctx);
                            }
                            ui.add_space(8.0);
                            if button(ui, Some(Icon::Sync), "Refresh", Btn::Secondary, false, true)
                                .clicked()
                            {
                                self.last_account_poll = ui.ctx().input(|i| i.time);
                                self.ctrl.refresh_account(false);
                            }
                        });
                    } else if button(
                        ui,
                        Some(Icon::Sync),
                        "Check again",
                        Btn::Primary,
                        false,
                        true,
                    )
                    .clicked()
                    {
                        self.last_account_poll = ui.ctx().input(|i| i.time);
                        self.ctrl.refresh_account(false);
                    }
                    if waiting {
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            ui.add_space((ui.available_width() - 150.0).max(0.0) / 2.0);
                            ui.spinner();
                            ui.label(RichText::new("Waiting for sign-in…").color(DIM));
                        });
                    }
                    ui.add_space(14.0);
                    let caption = if found {
                        snap.account.version.clone()
                    } else {
                        format!("Checked for \"{}\" on your PATH.", self.cfg.binary)
                    };
                    ui.label(RichText::new(caption).size(11.5).color(DIM2));
                });
            });
        });
    }

    fn launch_login(&self) {
        let bin = self.cfg.binary.clone();
        thread::spawn(move || {
            let mut cmd = std::process::Command::new(&bin);
            cmd.args(["auth", "login"]);
            inherit_session_env(&mut cmd);
            let _ = cmd.status();
        });
    }

    fn start_login(&mut self, ctx: &egui::Context) {
        self.launch_login();
        let now = ctx.input(|i| i.time);
        self.login_poll_until = Some(now + 180.0);
        self.last_account_poll = now;
        self.ctrl.refresh_account(false);
        self.toast(
            ctx,
            "Login opened in your browser — the app updates once you're signed in.",
            false,
        );
    }

    fn logout_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_logout {
            return;
        }
        egui::Window::new("Log out")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .frame(card_frame().inner_margin(egui::Margin::same(18)))
            .title_bar(false)
            .show(ctx, |ui| {
                ui.set_max_width(300.0);
                ui.label(
                    RichText::new("Log out of Proton Drive?")
                        .font(FontId::new(15.0, ff_bold()))
                        .color(TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("You'll need to sign in again before the next sync.")
                        .size(12.5)
                        .color(DIM),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, None, "Log out", Btn::Danger, false, true).clicked() {
                            let proton = ProtonCli::new(&self.cfg);
                            let _ = proton.logout();
                            self.ctrl.refresh_account(false);
                            self.confirm_logout = false;
                            self.toast(ctx, "Logged out.", false);
                        }
                        if button(ui, None, "Cancel", Btn::Secondary, false, true).clicked() {
                            self.confirm_logout = false;
                        }
                    });
                });
            });
    }

    fn reset_dialog(&mut self, ctx: &egui::Context) {
        if !self.confirm_reset {
            return;
        }
        egui::Window::new("Reset")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .frame(card_frame().inner_margin(egui::Margin::same(18)))
            .title_bar(false)
            .show(ctx, |ui| {
                ui.set_max_width(320.0);
                ui.label(
                    RichText::new("Reset NeutronSync?")
                        .font(FontId::new(15.0, ff_bold()))
                        .color(TEXT),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Removes all folders and wipes stored data (baselines, sync history). \
                         Your actual files are left untouched.",
                    )
                    .size(12.5)
                    .color(DIM),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, None, "Reset everything", Btn::Danger, false, true).clicked()
                        {
                            self.ctrl.reset_data();
                            self.cfg.pairs.clear();
                            self.cfg.auto_sync = false;
                            self.dirty = true; // auto-save writes the clean config
                            self.confirm_reset = false;
                            self.toast(
                                ctx,
                                "NeutronSync reset. Add a folder to start fresh.",
                                false,
                            );
                        }
                        if button(ui, None, "Cancel", Btn::Secondary, false, true).clicked() {
                            self.confirm_reset = false;
                        }
                    });
                });
            });
    }
}

fn current_op_line(snap: &AppState) -> String {
    snap.pairs
        .iter()
        .find_map(|p| p.current_op.clone())
        .unwrap_or_else(|| "Scanning folder pairs".to_string())
}
fn last_synced_line(snap: &AppState) -> String {
    match snap.pairs.iter().filter_map(|p| p.last_synced).max() {
        Some(ts) => format!("All folders up to date · {}", rel_time(ts)),
        None => "Nothing synced yet".to_string(),
    }
}

fn activity_banner(
    ui: &mut egui::Ui,
    col: Color32,
    icon: Icon,
    title: &str,
    sub: &str,
    spin: bool,
) {
    // full-width, state-tinted status bar
    let frame = egui::Frame::default()
        .fill(col.gamma_multiply(0.11))
        .stroke(Stroke::new(1.0, col.gamma_multiply(0.34)))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::symmetric(16, 14));
    ui.scope(|ui| {
        ui.set_max_width((ui.available_width() - 15.0).max(200.0));
        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (br, _) = ui.allocate_exact_size(vec2(38.0, 38.0), Sense::hover());
                ui.painter().rect_filled(br, 10.0, col.gamma_multiply(0.22));
                let inner = br.shrink(9.0);
                if spin {
                    ui.put(inner, egui::Spinner::new().size(inner.width()).color(col));
                } else {
                    draw_icon(ui.painter(), inner, icon, col);
                }
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    ui.add_space(1.0);
                    ui.label(
                        RichText::new(title)
                            .font(FontId::new(15.0, ff_bold()))
                            .color(TEXT),
                    );
                    ui.add_space(1.0);
                    ui.label(RichText::new(sub).size(12.0).color(DIM));
                });
            });
        });
    });
}

/// What an Activity row click wants to open locally.
enum Open {
    File,
    Folder,
}

/// Open a local path with the desktop's default handler.
fn open_path(path: &std::path::Path) {
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Open a URL in the user's browser (e.g. a release page).
fn open_url(url: &str) {
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Run an update check on a background thread, storing the outcome in `slot`
/// and requesting a repaint when done. Never blocks the UI.
fn spawn_update_check(
    slot: std::sync::Arc<std::sync::Mutex<UpdateState>>,
    channel: UpdateChannel,
    ctx: egui::Context,
) {
    *slot.lock().unwrap() = UpdateState::Checking;
    std::thread::spawn(move || {
        let res = updater::check(channel).map_err(|e| e.to_string());
        *slot.lock().unwrap() = UpdateState::Done(res);
        ctx.request_repaint();
    });
}

/// Download the release asset for this install kind, then install it. Progress
/// and the outcome land in `slot`, which the Updates panel renders.
fn spawn_update_install(
    slot: std::sync::Arc<std::sync::Mutex<UpdateState>>,
    info: UpdateInfo,
    ctx: egui::Context,
) {
    *slot.lock().unwrap() = UpdateState::Downloading { done: 0, total: 0 };
    std::thread::spawn(move || {
        let kind = updater::install_kind();
        let Some(asset) = updater::pick_asset(&info, kind).cloned() else {
            *slot.lock().unwrap() =
                UpdateState::Failed("That release has no package for this system.".into());
            ctx.request_repaint();
            return;
        };

        let prog_slot = slot.clone();
        let progress = move |done: u64, total: u64| {
            *prog_slot.lock().unwrap() = UpdateState::Downloading { done, total };
        };
        let never_cancel = || false;
        let downloaded = match updater::download(&asset, &progress, &never_cancel) {
            Ok(p) => p,
            Err(e) => {
                *slot.lock().unwrap() = UpdateState::Failed(format!("Download failed: {e}"));
                ctx.request_repaint();
                return;
            }
        };

        // Verify against the checksum the release API published before anything
        // is handed to a root process. A mismatch discards the file.
        *slot.lock().unwrap() = UpdateState::Verifying;
        ctx.request_repaint();
        if let Err(e) = updater::verify(&downloaded, &asset) {
            let _ = std::fs::remove_file(&downloaded);
            *slot.lock().unwrap() = UpdateState::Failed(format!("{e}"));
            ctx.request_repaint();
            return;
        }

        if !kind.can_install() {
            *slot.lock().unwrap() = UpdateState::Downloaded { path: downloaded };
            ctx.request_repaint();
            return;
        }

        *slot.lock().unwrap() = UpdateState::Installing;
        ctx.request_repaint();
        *slot.lock().unwrap() = match updater::install(&downloaded, kind) {
            Ok(()) => UpdateState::Installed {
                version: info.latest.clone(),
            },
            Err(e) => UpdateState::Failed(format!("{e}")),
        };
        ctx.request_repaint();
    });
}

/// Bytes as a short human string ("6.5 MB"), for download progress.
fn human_bytes(n: u64) -> String {
    const MB: f64 = 1_000_000.0;
    const KB: f64 = 1_000.0;
    let n = n as f64;
    if n >= MB {
        format!("{:.1} MB", n / MB)
    } else if n >= KB {
        format!("{:.0} kB", n / KB)
    } else {
        format!("{n:.0} B")
    }
}

/// Base of the XDG data dir (`$XDG_DATA_HOME` or `~/.local/share`).
fn xdg_data_home() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share")
        })
}

/// Install a themed icon + `.desktop` so the shell shows our logo (not a generic
/// gear) in the dock/overview. GNOME matches the window's `app_id` ("neutronsync",
/// via `with_app_id`) to `neutronsync.desktop`'s `StartupWMClass` and uses its
/// `Icon=`. Idempotent: only writes when missing or changed.
fn ensure_desktop_integration() {
    let data = xdg_data_home();
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "neutronsync-gui".to_string());

    let write_if_changed = |path: &std::path::Path, bytes: &[u8]| {
        if std::fs::read(path).ok().as_deref() != Some(bytes) {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(path, bytes);
        }
    };

    // Raster icon under the hicolor theme (our own logo).
    write_if_changed(
        &data.join("icons/hicolor/256x256/apps/neutronsync.png"),
        include_bytes!("../../assets/logo.png"),
    );
    // Scalable icon too — GNOME Shell prefers scalable over the raster sizes,
    // and old builds installed Proton's OWN gradient-folder SVG here; writing
    // our atom mark over it also heals those machines (we may not ship
    // Proton's artwork).
    write_if_changed(
        &data.join("icons/hicolor/scalable/apps/neutronsync.svg"),
        include_bytes!("../../assets/neutron-logo.svg"),
    );

    // Quote the exec path: it may contain spaces (dev builds under a path like
    // ".../Projects - Personal/..."), and an unquoted Exec is an invalid entry
    // that the shell rejects — which shows a generic gear instead of our icon.
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=NeutronSync\nGenericName=Proton Drive sync\nComment=Bidirectional Proton Drive folder sync\nExec=\"{exe}\" %U\nIcon=neutronsync\nTerminal=false\nCategories=Network;FileTransfer;\nStartupWMClass=neutronsync\n"
    );
    // Write to the SAME basename the package ships, so this user copy SHADOWS
    // the packaged one (one launcher entry) rather than adding a second.
    write_if_changed(
        &data.join("applications/neutronsync-gui.desktop"),
        desktop.as_bytes(),
    );
    // Remove the legacy duplicate from older builds (a different basename showed
    // up as a second "NeutronSync" entry alongside the packaged one).
    let _ = std::fs::remove_file(data.join("applications/neutronsync.desktop"));
}

/// Path of the XDG autostart entry for launch-at-login.
fn autostart_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config")
        });
    base.join("autostart").join("neutronsync.desktop")
}

fn autostart_enabled() -> bool {
    autostart_path().is_file()
}

/// Create or remove the autostart .desktop entry.
/// Create or remove the autostart entry. The launched command matches the tray
/// mode: the headless daemon on Wayland (`--tray`), a hidden window in X11-compat
/// (`--hidden`), or a plain window when the tray is off.
fn set_autostart(enabled: bool, cfg: &Config) {
    let path = autostart_path();
    if enabled {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "neutronsync-gui".to_string());
        // With the tray on, launch the headless daemon; otherwise a plain window.
        let arg = if cfg.run_in_tray { " --tray" } else { "" };
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=NeutronSync\nComment=Bidirectional Proton Drive sync\nExec=\"{exe}\"{arg}\nIcon=neutronsync\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
        );
        let _ = std::fs::write(&path, entry);
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

/// Shared activity-row layout: a flexible left area (icon + name) plus fixed
/// FOLDER and WHEN columns on the right, so rows and the header line up and the
/// columns never get squished.
fn activity_cols(
    ui: &mut egui::Ui,
    folder: &str,
    folder_col: Color32,
    when: &str,
    when_col: Color32,
    folder_clickable: bool,
    left: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut folder_clicked = false;
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // fixed WHEN column, left-aligned so timestamps form a clean column
            ui.allocate_ui_with_layout(
                vec2(112.0, 24.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(when).size(11.5).color(when_col)).truncate(),
                    );
                },
            );
            ui.add_space(14.0);
            // fixed FOLDER column, left-aligned
            ui.allocate_ui_with_layout(
                vec2(140.0, 24.0),
                egui::Layout::left_to_right(egui::Align::Center),
                |ui| {
                    let mut lbl =
                        egui::Label::new(RichText::new(folder).size(12.0).color(folder_col))
                            .truncate();
                    if folder_clickable {
                        lbl = lbl.sense(Sense::click());
                    }
                    let r = ui.add(lbl);
                    if folder_clickable {
                        if r.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        folder_clicked = r.clicked();
                    }
                },
            );
            ui.add_space(16.0);
            // remaining space: icon + name, left-aligned
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), left);
        });
    });
    folder_clicked
}

fn active_row(ui: &mut egui::Ui, op: &ActivityOp) {
    let a = op.action.to_lowercase();
    let verb = if a.contains("upload") {
        "Uploading"
    } else if a.contains("download") {
        "Downloading"
    } else if a.contains("rename") || a.contains("move") {
        "Moving"
    } else if a.contains("delete") || a.contains("trash") {
        "Deleting"
    } else {
        "Syncing"
    };
    let name = op.path.rsplit('/').next().unwrap_or(&op.path).to_owned();
    activity_cols(ui, &op.pair, DIM, "now", ACCENT, false, |ui| {
        ui.add_sized([24.0, 24.0], egui::Spinner::new().size(16.0).color(ACCENT));
        ui.add_space(8.0);
        ui.add(
            egui::Label::new(
                RichText::new(format!("{verb} {name}"))
                    .size(13.0)
                    .color(TEXT),
            )
            .truncate(),
        );
    });
    ui.add_space(4.0);
}

/// Text to show for a failed operation. Older rows stored `ok=false` without an
/// `error` column; fall back to the stored line text or a generic label.
fn op_error_text(op: &ActivityOp, line: &str) -> Option<String> {
    if op.ok {
        return None;
    }
    if let Some(e) = op.error.as_deref().filter(|e| !e.is_empty()) {
        return Some(e.to_string());
    }
    // op_text() for failures is "action path: reason" when reason was captured.
    if let Some((_, reason)) = line.rsplit_once(": ") {
        if !reason.is_empty() && !reason.starts_with(&op.action) {
            return Some(reason.to_string());
        }
    }
    Some("failed".into())
}

fn activity_row(ui: &mut egui::Ui, op: &ActivityOp, ts: i64, line: &str) -> Option<Open> {
    let a = op.action.to_lowercase();
    let (icon, base) = if a.contains("rename") || a.contains("move") {
        (Icon::Arrow, ACCENT)
    } else if a.contains("upload") {
        (Icon::Upload, ACCENT)
    } else if a.contains("download") {
        (Icon::Download, ACCENT)
    } else if a.contains("delete") || a.contains("trash") {
        (Icon::Trash, WARN)
    } else {
        (Icon::Check, OK)
    };
    let col = if op.ok { base } else { DANGER };
    let name = op.path.rsplit('/').next().unwrap_or(&op.path).to_owned();
    let mut name_clicked = false;
    let folder_clicked = activity_cols(ui, &op.pair, DIM, &rel_time(ts), DIM2, true, |ui| {
        let (ir, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
        ui.painter().rect_filled(ir, 6.0, col.gamma_multiply(0.16));
        draw_icon(ui.painter(), ir.shrink(5.0), icon, col);
        ui.add_space(8.0);
        let r = ui.add(
            egui::Label::new(RichText::new(name).size(13.0).color(TEXT))
                .truncate()
                .sense(Sense::click()),
        );
        let r = r.on_hover_text(&op.path);
        if r.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        name_clicked = r.clicked();
        // A failed row says why on the row itself: the reason used to live only
        // in sync.log, so the feed showed a red filename and nothing else.
        if let Some(err) = op_error_text(op, line) {
            ui.add_space(6.0);
            let tip = err.clone();
            ui.add(egui::Label::new(RichText::new(err).size(11.5).color(DANGER)).truncate())
                .on_hover_text(tip);
        }
    });
    ui.add_space(4.0);
    if name_clicked {
        Some(Open::File)
    } else if folder_clicked {
        Some(Open::Folder)
    } else {
        None
    }
}

fn pair_status(ui: &mut egui::Ui, ps: Option<&PairState>, watching: bool) {
    let first_run = ps.map_or(false, |p| p.last_synced.is_none());
    let (col, text) = match ps.map(|p| p.phase) {
        Some(Phase::Scanning) => (
            ACCENT,
            if first_run {
                "Initializing…"
            } else {
                "Scanning…"
            }
            .to_string(),
        ),
        Some(Phase::Syncing) => (
            ACCENT,
            if first_run {
                "Initializing…"
            } else {
                "Syncing…"
            }
            .to_string(),
        ),
        Some(Phase::Error) => (
            DANGER,
            ps.and_then(|p| p.last_error.clone())
                .unwrap_or_else(|| "Error".into()),
        ),
        Some(Phase::Synced) | Some(Phase::Idle) | None => {
            if watching {
                (ACCENT, "Auto · watching".to_string())
            } else {
                match ps.and_then(|p| p.last_synced) {
                    Some(ts) => (OK, format!("Synced · {}", rel_time(ts))),
                    None => (DIM, "Not synced yet".to_string()),
                }
            }
        }
    };
    ui.horizontal(|ui| {
        status_dot_small(ui, col);
        ui.label(RichText::new(text).size(12.0).color(DIM));
    });
}

fn status_dot_small(ui: &mut egui::Ui, col: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.5, col);
}

fn chip(ui: &mut egui::Ui, icon: Icon, text: &str, max_w: f32) {
    let font = FontId::new(12.5, FontFamily::Proportional);
    let pad = 10.0;
    let icon_w = 14.0;
    let gap = 6.0;
    let text_budget = (max_w - (pad * 2.0 + icon_w + gap)).max(24.0);
    let shown = elide_front(ui, text, font.clone(), text_budget);
    let tw = galley_w(ui, &shown, font.clone());
    let w = pad * 2.0 + icon_w + gap + tw;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 7.0, NAV_BG);
    let ir = Rect::from_center_size(
        pos2(rect.left() + pad + icon_w / 2.0, rect.center().y),
        vec2(icon_w, icon_w),
    );
    draw_icon(painter, ir, icon, DIM);
    painter.text(
        pos2(rect.left() + pad + icon_w + gap, rect.center().y),
        Align2::LEFT_CENTER,
        &shown,
        font,
        TEXT,
    );
    // full path on hover, since we elide
    if shown != text {
        resp.on_hover_text(text);
    }
}

fn empty_state(ui: &mut egui::Ui, title: &str, sub: &str) {
    ui.add_space(40.0);
    ui.vertical_centered(|ui| {
        let (ir, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
        draw_icon(ui.painter(), ir, Icon::Cloud, DIM2);
        ui.add_space(10.0);
        ui.label(
            RichText::new(title)
                .font(FontId::new(15.0, ff_bold()))
                .color(DIM),
        );
        ui.label(RichText::new(sub).size(12.5).color(DIM2));
    });
}

// -- toast -------------------------------------------------------------------
impl App {
    fn toast_overlay(&mut self, ctx: &egui::Context) {
        let Some(t) = &self.toast else { return };
        let age = ctx.input(|i| i.time) - t.born;
        let visible = age < 2.8;
        let alpha = ctx.animate_bool_with_time(egui::Id::new("toast"), visible, 0.25);
        if !visible && alpha < 0.02 {
            self.toast = None;
            return;
        }
        let msg = t.msg.clone();
        let err = t.err;
        egui::Area::new(egui::Id::new("toast_area"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -22.0))
            .interactable(false)
            .show(ctx, |ui| {
                ui.multiply_opacity(alpha);
                let frame = egui::Frame::default()
                    .fill(SEL)
                    .stroke(Stroke::new(1.0, line_col()))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 16,
                        top: 10,
                        bottom: 10,
                    });
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        status_dot_small(ui, if err { DANGER } else { OK });
                        ui.label(RichText::new(msg).size(13.0).color(TEXT));
                    });
                });
            });
    }
}

// -- remote browser window ---------------------------------------------------
impl App {
    fn browser_window(&mut self, ctx: &egui::Context) {
        if !self.browser_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Browse Proton Drive")
            .collapsible(false)
            .resizable(true)
            .default_width(480.0)
            .open(&mut open)
            .show(ctx, |ui| self.browser_ui(ui));
        if !open {
            self.browser_open = false;
        }
    }

    fn browser_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let at_root = self.browser_path == self.cfg.remote_root;
            if button(ui, Some(Icon::Arrow), "Up", Btn::Ghost, true, !at_root).clicked() {
                if let Some((parent, _)) = self.browser_path.rsplit_once('/') {
                    self.browser_path = if parent.is_empty() {
                        "/".into()
                    } else {
                        parent.to_string()
                    };
                    self.load_browser();
                }
            }
            ui.label(RichText::new(&self.browser_path).color(DIM));
        });
        ui.separator();
        if self.browser_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Loading…").color(DIM));
            });
            return;
        }
        if let Some(err) = self.browser_error.clone() {
            ui.label(RichText::new(err).color(DANGER));
            return;
        }
        let mut open_into: Option<String> = None;
        let mut select: Option<String> = None;
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(ui, |ui| {
                if self.browser_entries.is_empty() {
                    ui.label(RichText::new("(no sub-folders here)").color(DIM));
                }
                for e in &self.browser_entries {
                    let full = config::remote_join(&self.browser_path, &e.path);
                    ui.horizontal(|ui| {
                        let (ir, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        draw_icon(ui.painter(), ir, Icon::Folder, ACCENT);
                        if ui.link(RichText::new(&e.path).color(TEXT)).clicked() {
                            open_into = Some(full.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if button(ui, None, "Select", Btn::Secondary, true, true).clicked() {
                                select = Some(full.clone());
                            }
                        });
                    });
                }
            });
        ui.separator();
        ui.horizontal(|ui| {
            let cur = self.browser_path.clone();
            if button(
                ui,
                Some(Icon::Add),
                "Sync this folder",
                Btn::Primary,
                false,
                true,
            )
            .clicked()
            {
                select = Some(cur);
            }
            ui.label(
                RichText::new("(pick a local folder next)")
                    .size(11.5)
                    .color(DIM2),
            );
        });
        if let Some(p) = open_into {
            self.browser_path = p;
            self.load_browser();
        }
        if let Some(p) = select {
            let ctx = ui.ctx().clone();
            self.add_remote(&ctx, p);
        }
    }
}

// -- combos (settings) -------------------------------------------------------
/// A titled group of settings, rendered as a card.
fn settings_group(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.add_space(4.0);
    ui.label(
        RichText::new(title)
            .font(FontId::new(11.0, ff_bold()))
            .color(ACCENT),
    );
    ui.add_space(6.0);
    card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        body(ui);
    });
    ui.add_space(14.0);
}

/// A settings row: title + description on the left, a control on the right,
/// aligned to a consistent right edge. Returns the control closure's value.
fn setting_row<R>(
    ui: &mut egui::Ui,
    title: &str,
    desc: &str,
    right: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let mut out = None;
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).size(13.5).color(TEXT));
            if !desc.is_empty() {
                ui.add(egui::Label::new(RichText::new(desc).size(11.5).color(DIM2)));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            out = Some(right(ui));
        });
    });
    ui.add_space(12.0);
    out.expect("setting_row control ran")
}

fn combo_local_delete(ui: &mut egui::Ui, v: &mut LocalDelete) -> bool {
    let text = match v {
        LocalDelete::Trash => "Desktop trash",
        LocalDelete::Remove => "Permanent delete",
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt("local_delete")
        .selected_text(text)
        .show_ui(ui, |ui| {
            changed |= ui
                .selectable_value(v, LocalDelete::Trash, "Desktop trash")
                .clicked();
            changed |= ui
                .selectable_value(v, LocalDelete::Remove, "Permanent delete")
                .clicked();
        });
    changed
}
fn combo_conflict(ui: &mut egui::Ui, v: &mut ConflictPolicy) -> bool {
    let text = match v {
        ConflictPolicy::KeepBoth => "Keep both",
        ConflictPolicy::Newer => "Keep newer",
        ConflictPolicy::Skip => "Skip",
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt("conflict")
        .selected_text(text)
        .show_ui(ui, |ui| {
            for (val, lbl) in [
                (ConflictPolicy::KeepBoth, "Keep both"),
                (ConflictPolicy::Newer, "Keep newer"),
                (ConflictPolicy::Skip, "Skip"),
            ] {
                changed |= ui.selectable_value(v, val, lbl).clicked();
            }
        });
    changed
}

fn combo_update_channel(ui: &mut egui::Ui, v: &mut UpdateChannel) -> bool {
    let text = match v {
        UpdateChannel::Stable => "Stable",
        UpdateChannel::Prerelease => "Pre-release",
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt("update_channel")
        .selected_text(text)
        .show_ui(ui, |ui| {
            for (val, lbl) in [
                (UpdateChannel::Stable, "Stable"),
                (UpdateChannel::Prerelease, "Pre-release"),
            ] {
                changed |= ui.selectable_value(v, val, lbl).clicked();
            }
        });
    changed
}
fn combo_compare(ui: &mut egui::Ui, v: &mut Compare) -> bool {
    let text = match v {
        Compare::Size => "Size",
        Compare::SizeMtime => "Size + modified time",
        Compare::Sha1 => "SHA-1 (exact)",
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt("compare")
        .selected_text(text)
        .show_ui(ui, |ui| {
            for (val, lbl) in [
                (Compare::Size, "Size"),
                (Compare::SizeMtime, "Size + modified time"),
                (Compare::Sha1, "SHA-1 (exact)"),
            ] {
                changed |= ui.selectable_value(v, val, lbl).clicked();
            }
        });
    changed
}

// -- starter config ----------------------------------------------------------
fn starter_config(path: &PathBuf) -> Config {
    Config {
        binary: "proton-drive".into(),
        upload_flags: vec![
            "--file-conflict-strategy".into(),
            "replace".into(),
            "--folder-conflict-strategy".into(),
            "replace".into(),
        ],
        download_flags: vec![
            "--file-conflict-strategy".into(),
            "remove".into(),
            "--folder-conflict-strategy".into(),
            "remove".into(),
        ],
        credentials_store: None,
        fresh_cache: true,
        scan_threads: 0,
        download_threads: 0,
        remote_root: config::DEFAULT_REMOTE_ROOT.into(),
        propagate_deletes: true,
        auto_sync: false,
        run_in_tray: false,
        x11_compat: false,
        local_delete: LocalDelete::Trash,
        conflict: ConflictPolicy::KeepBoth,
        compare: Compare::SizeMtime,
        poll_interval_secs: 900,
        scan_interval_secs: 120,
        debounce_secs: 2,
        update_channel: UpdateChannel::Stable,
        check_on_launch: false,
        state_dir: {
            let base = std::env::var("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
                });
            base.join("neutronsync")
        },
        pairs: vec![],
        source_path: Some(path.clone()),
    }
}

/// Auto-sync pair names for `cfg` (pairs with `auto = true`).
fn auto_names_of(cfg: &Config) -> Vec<String> {
    cfg.pairs
        .iter()
        .filter(|p| p.auto)
        .map(|p| p.name.clone())
        .collect()
}

/// Headless tray daemon (`--tray`): owns the system tray (GTK/AppIndicator) and
/// an optional watcher, with no window. It's the Wayland-native background
/// presence — the window can't hide/restore/focus itself on Wayland, so instead
/// the tray simply opens a fresh window on demand. Runs a GTK main loop until
/// the tray "Quit" item is chosen or the process is killed.
fn run_daemon() {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::TrayIconBuilder;

    let cfg = match config::load(None) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("neutronsync: no config found; nothing to run in tray mode");
            return;
        }
    };
    let _lock = match PidLock::acquire(&cfg.state_dir, DAEMON_LOCK) {
        Some(l) => l,
        None => {
            eprintln!("neutronsync: a tray daemon is already running");
            return;
        }
    };
    ensure_desktop_integration();

    let ctrl = std::sync::Arc::new(Controller::new(cfg.clone()));
    if cfg.auto_sync {
        let names = auto_names_of(&cfg);
        if !names.is_empty() {
            ctrl.start_watch(names);
        }
    }

    if let Err(e) = gtk::init() {
        eprintln!("neutronsync: GTK init failed: {e}");
        return;
    }

    let menu = Menu::new();
    let open_i = MenuItem::new("Open NeutronSync", true, None);
    let sync_i = MenuItem::new("Sync now", true, None);
    let quit_i = MenuItem::new("Quit", true, None);
    let _ = menu.append(&open_i);
    let _ = menu.append(&sync_i);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_i);
    let (open_id, sync_id, quit_id) = (
        open_i.id().clone(),
        sync_i.id().clone(),
        quit_i.id().clone(),
    );

    let mut builder = TrayIconBuilder::new()
        .with_tooltip("NeutronSync")
        .with_menu(Box::new(menu));
    if let Some(icon) = tray_icon_image() {
        builder = builder.with_icon(icon);
    }
    let _tray = match builder.build() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("neutronsync: system tray unavailable: {e}");
            return;
        }
    };

    // Poll the menu-event channel from within the GTK loop, and hot-reload the
    // config file so added/removed folders and setting changes take effect
    // without restarting the tray.
    let menu_rx = MenuEvent::receiver();
    let state_dir = cfg.state_dir.clone();
    let config_path = config::find_config(None).unwrap_or_else(config::default_config_path);
    let ctrl_cb = ctrl.clone();
    let mut last_mtime = file_mtime(&config_path);
    let mut want_watch = cfg.auto_sync;
    let mut pending_restart = false;
    gtk::glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        // tray menu
        while let Ok(ev) = menu_rx.try_recv() {
            let id = ev.id();
            if id == &open_id {
                if !window_running(&state_dir) {
                    spawn_window();
                }
            } else if id == &sync_id {
                let names = auto_names_of(&ctrl_cb.config());
                if !names.is_empty() {
                    ctrl_cb.sync(names, false);
                }
            } else if id == &quit_id {
                // Full quit: close the window too (if open), then end the daemon.
                terminate_window(&state_dir);
                gtk::main_quit();
            }
        }

        // config hot-reload: on a change, adopt the new config and restart the
        // watcher (deferred until the old one releases its lock).
        let now_mtime = file_mtime(&config_path);
        if now_mtime != last_mtime {
            last_mtime = now_mtime;
            if let Ok(newcfg) = config::load(None) {
                let pairs = newcfg.pairs.len();
                want_watch = newcfg.auto_sync;
                ctrl_cb.commit_config(newcfg);
                ctrl_cb.stop_watch();
                pending_restart = true;
                eprintln!("neutronsync: config changed; reloaded ({pairs} folder(s))");
            }
        }
        // Start the watcher once the previous one has released watch.lock.
        if pending_restart && !pid_alive_from(&state_dir.join("watch.lock")) {
            pending_restart = false;
            if want_watch {
                let names = auto_names_of(&ctrl_cb.config());
                if !names.is_empty() {
                    ctrl_cb.start_watch(names);
                }
            }
        }

        // Publish our live state so an open window can display what the daemon
        // is doing (scanning, syncing, per-file ops) — the window has no other
        // view into this separate process.
        ctrl_cb.publish_status();

        gtk::glib::ControlFlow::Continue
    });

    gtk::main();
    ctrl.stop_watch();
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Headless tray daemon (Wayland-native background presence).
    if args.iter().any(|a| a == "--tray") {
        run_daemon();
        return Ok(());
    }

    // Force the X11 (XWayland) backend if the user prefers it.
    let x11 = config::load(None).map(|c| c.x11_compat).unwrap_or(false);

    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1040.0, 680.0])
        .with_min_inner_size([820.0, 520.0])
        .with_title("NeutronSync for Linux")
        .with_app_id("neutronsync");
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/logo.png")) {
        viewport = viewport.with_icon(icon);
    }
    let mut options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    if x11 {
        use winit::platform::x11::EventLoopBuilderExtX11;
        options.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
    }
    eframe::run_native(
        "neutronsync",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
