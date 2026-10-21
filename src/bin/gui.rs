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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::os::unix::process::{CommandExt, ExitStatusExt};

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, FontFamily, FontId, Rect, RichText, Sense, Stroke,
};

use neutronsync::bookmarks;
use neutronsync::config::{self, Config, ConflictPolicy, LocalDelete, Pair, Theme, UpdateChannel};
use neutronsync::models::{Compare, ConflictOutcome, Entry};
use neutronsync::notify_policy::{self, NotifyMemory};
use neutronsync::service::{ActivityKind, ActivityOp, AppState, Controller, PairState, Phase};
use neutronsync::stats::{ConflictRecord, HeldDeletes};
use neutronsync::tray_policy::{self, ChildEnd, DaemonBanner, SupervisorAction, TrayState};
use neutronsync::updater::{self, UpdateInfo};

// --- palette (reference/gui/palette/palette.md) -----------------------------
/// The window's colours. `DARK` is the original palette, `LIGHT` its
/// counterpart for a light desktop; `pal()` returns the active one, chosen by
/// the Appearance setting (`config::Theme`) and the desktop's preference.
struct Palette {
    nav_bg: Color32,
    panel: Color32,
    panel2: Color32,
    sel: Color32,
    text: Color32,
    dim: Color32,
    dim2: Color32,
    accent: Color32,
    accent_hi: Color32,
    accent_lo: Color32,
    ok: Color32,
    danger: Color32,
    warn: Color32,
    /// Off-state switch track: visibly a control, even on a `panel2` card.
    track_off: Color32,
    /// Disabled filled button: muted, with a legible label.
    btn_off: Color32,
    /// Switch knob when off and when on.
    knob_off: Color32,
    knob_on: Color32,
    /// Text and glyphs on an accent-filled surface (primary button, avatar).
    on_accent: Color32,
    /// Card drop shadow.
    shadow: Color32,
    /// Hairlines and hover washes: a tint of this colour at these alphas.
    wash: (u8, u8, u8),
    line_alpha: u8,
    hover_alpha: u8,
}

impl Palette {
    /// Hairline between surfaces.
    fn line(&self) -> Color32 {
        let (r, g, b) = self.wash;
        Color32::from_rgba_unmultiplied(r, g, b, self.line_alpha)
    }
    /// Hover wash over a row or a ghost button.
    fn hover(&self) -> Color32 {
        let (r, g, b) = self.wash;
        Color32::from_rgba_unmultiplied(r, g, b, self.hover_alpha)
    }
}

static DARK: Palette = Palette {
    nav_bg: Color32::from_rgb(0x16, 0x15, 0x1C),
    panel: Color32::from_rgb(0x1C, 0x1B, 0x24),
    panel2: Color32::from_rgb(0x23, 0x22, 0x30),
    sel: Color32::from_rgb(0x2A, 0x28, 0x33),
    text: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    dim: Color32::from_rgb(0x8E, 0x8B, 0x9A),
    dim2: Color32::from_rgb(0x6C, 0x6A, 0x7A),
    accent: Color32::from_rgb(0x6D, 0x4A, 0xFF),
    accent_hi: Color32::from_rgb(0x7C, 0x5C, 0xFF),
    accent_lo: Color32::from_rgb(0x5C, 0x3E, 0xDB),
    ok: Color32::from_rgb(0x3C, 0xBB, 0x87),
    danger: Color32::from_rgb(0xE0, 0x50, 0x64),
    warn: Color32::from_rgb(0xD8, 0xA0, 0x50),
    track_off: Color32::from_rgb(0x3B, 0x39, 0x4A),
    btn_off: Color32::from_rgb(0x2E, 0x2C, 0x3E),
    knob_off: Color32::from_rgb(0xD4, 0xD2, 0xDE),
    knob_on: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    shadow: Color32::from_black_alpha(70),
    wash: (255, 255, 255),
    line_alpha: 20,
    hover_alpha: 14,
};

static LIGHT: Palette = Palette {
    nav_bg: Color32::from_rgb(0xEC, 0xEA, 0xF3),
    panel: Color32::from_rgb(0xF7, 0xF6, 0xFB),
    panel2: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    sel: Color32::from_rgb(0xE2, 0xDF, 0xF0),
    text: Color32::from_rgb(0x1C, 0x1B, 0x24),
    dim: Color32::from_rgb(0x5E, 0x5B, 0x6E),
    dim2: Color32::from_rgb(0x7A, 0x77, 0x89),
    accent: Color32::from_rgb(0x6D, 0x4A, 0xFF),
    accent_hi: Color32::from_rgb(0x5C, 0x3E, 0xDB),
    accent_lo: Color32::from_rgb(0x4B, 0x30, 0xC4),
    ok: Color32::from_rgb(0x2E, 0x9A, 0x6E),
    danger: Color32::from_rgb(0xC8, 0x38, 0x4C),
    warn: Color32::from_rgb(0xB0, 0x7A, 0x1E),
    track_off: Color32::from_rgb(0xC9, 0xC6, 0xD6),
    btn_off: Color32::from_rgb(0xE6, 0xE4, 0xEF),
    knob_off: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    knob_on: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    on_accent: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    shadow: Color32::from_black_alpha(28),
    wash: (0, 0, 0),
    line_alpha: 26,
    hover_alpha: 12,
};

/// True while the window paints with `LIGHT`. Flipped by `sync_theme`.
static LIGHT_MODE: AtomicBool = AtomicBool::new(false);

/// The active palette.
fn pal() -> &'static Palette {
    if LIGHT_MODE.load(Ordering::Relaxed) {
        &LIGHT
    } else {
        &DARK
    }
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
    Conflict,
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
        Icon::Conflict => ph::ARROWS_LEFT_RIGHT,
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
        painter.circle_filled(rect.center(), rect.width() * 0.5, pal().accent);
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
    neutronsync::datefmt::relative_time(now_secs(), ts)
}

/// "1 folder" / "2 folders".
fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{} {one}s", thousands(n))
    }
}

/// 1284 -> "1,284".
fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn galley_w(ui: &egui::Ui, text: &str, font: FontId) -> f32 {
    ui.painter()
        .layout_no_wrap(text.to_owned(), font, pal().text)
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
        .fill(pal().panel2)
        .stroke(Stroke::new(1.0, pal().line()))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(16))
        .shadow(egui::epaint::Shadow {
            offset: [0, 4],
            blur: 16,
            spread: 0,
            color: pal().shadow,
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
    button_width(ui, icon, label, kind, small, enabled, None)
}

/// Like [`button`], but at least `width` wide, with the label centred in it.
fn button_width(
    ui: &mut egui::Ui,
    icon: Option<Icon>,
    label: &str,
    kind: Btn,
    small: bool,
    enabled: bool,
    width: Option<f32>,
) -> egui::Response {
    let h = if small { 30.0 } else { 34.0 };
    let fs = if small { 12.5 } else { 13.0 };
    let font = FontId::new(fs, ff_bold());
    let pad = 13.0;
    let icon_sz = if small { 14.0 } else { 15.0 };
    let gap = 7.0;
    let tw = galley_w(ui, label, font.clone());
    let iw = if icon.is_some() { icon_sz + gap } else { 0.0 };
    let natural = pad * 2.0 + iw + tw;
    let w = width.unwrap_or(natural).max(natural);
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
        // A disabled filled button goes muted rather than translucent, so the
        // label stays readable instead of fading into the accent.
        Btn::Primary | Btn::Secondary if !enabled => (pal().btn_off, pal().dim),
        Btn::Primary => {
            let base = if down {
                pal().accent_lo
            } else {
                mix(pal().accent, pal().accent_hi, hov)
            };
            (base, pal().on_accent)
        }
        Btn::Secondary => {
            let base = mix(pal().sel, mix(pal().sel, pal().text, 0.10), hov);
            if down {
                (pal().accent_lo, pal().on_accent)
            } else {
                (base, pal().text)
            }
        }
        Btn::Ghost => (
            mix(Color32::TRANSPARENT, pal().hover(), hov),
            if enabled { pal().text } else { pal().dim2 },
        ),
        Btn::Danger => (
            mix(
                Color32::TRANSPARENT,
                Color32::from_rgba_unmultiplied(224, 80, 100, 40),
                hov,
            ),
            pal().danger,
        ),
    };
    let fg = if enabled { fg } else { pal().dim2 };
    if bg.a() > 0 {
        painter.rect_filled(rect, 8.0, bg);
    }
    // Hug-width buttons already have `pad` on each side, so centring the
    // icon+label lands in the same place. A wider button (the sign-in action)
    // keeps that label in the middle instead of on the left edge.
    let mut x = rect.left() + (rect.width() - iw - tw) / 2.0;
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
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.12);
    let off = mix(pal().track_off, mix(pal().track_off, pal().text, 0.08), hov);
    let track = mix(off, mix(pal().accent, pal().accent_hi, hov), t);
    painter.rect_filled(rect, h / 2.0, track);
    let kr = h / 2.0 - 3.0;
    let cx = rect.left() + h / 2.0 + t * (w - h);
    let knob = pos2(cx, rect.center().y);
    // soft shadow under the knob lifts it off the track
    painter.circle_filled(knob + vec2(0.0, 1.0), kr, Color32::from_black_alpha(60));
    painter.circle_filled(knob, kr, mix(pal().knob_off, pal().knob_on, t));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}

/// A thin progress meter with an animated fill.
fn meter(ui: &mut egui::Ui, width: f32, frac: f32, col: Color32, id: egui::Id) {
    let (rect, _) = ui.allocate_exact_size(vec2(width, 6.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, pal().track_off);
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
    Conflicts,
    Settings,
    Account,
    About,
}

const NAV_ITEMS: [(Nav, Icon, &str); 6] = [
    (Nav::Activity, Icon::Activity, "Activity"),
    (Nav::Folders, Icon::Folder, "Folders"),
    (Nav::Conflicts, Icon::Conflict, "Conflicts"),
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

/// The four tray icons, built once at start from `assets/logo.png`: the plain
/// logo for Idle, and the logo with a coloured dot for the other states.
struct TrayIcons {
    idle: tray_icon::Icon,
    syncing: tray_icon::Icon,
    attention: tray_icon::Icon,
    error: tray_icon::Icon,
}

impl TrayIcons {
    fn build() -> Option<Self> {
        let logo =
            eframe::icon_data::from_png_bytes(include_bytes!("../../assets/logo.png")).ok()?;
        let make = |dot: Option<Color32>| {
            let mut rgba = logo.rgba.clone();
            if let Some(colour) = dot {
                paint_state_dot(&mut rgba, logo.width, logo.height, colour);
            }
            tray_icon::Icon::from_rgba(rgba, logo.width, logo.height).ok()
        };
        Some(TrayIcons {
            idle: make(None)?,
            syncing: make(Some(DARK.accent))?,
            attention: make(Some(DARK.warn))?,
            error: make(Some(DARK.danger))?,
        })
    }

    fn for_state(&self, state: TrayState) -> &tray_icon::Icon {
        match state {
            TrayState::Idle => &self.idle,
            TrayState::Syncing => &self.syncing,
            TrayState::Attention => &self.attention,
            TrayState::Error => &self.error,
        }
    }
}

/// Paint a filled dot in the lower right quarter of a straight-alpha RGBA
/// image, with a dark outline so it reads on light and dark panels.
fn paint_state_dot(rgba: &mut [u8], width: u32, height: u32, colour: Color32) {
    let (w, h) = (width as f32, height as f32);
    let (cx, cy) = (w * 0.76, h * 0.76);
    let radius = w * 0.17;
    let outline = w * 0.045;
    let edge = (w / 128.0).max(1.0); // anti-aliasing width in pixels
    let dark = DARK.nav_bg;
    for y in 0..height {
        for x in 0..width {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            if d > radius + outline + edge {
                continue;
            }
            // Outline covers the whole disc; the fill sits on top of it.
            let ring_cov = ((radius + outline + edge - d) / edge).clamp(0.0, 1.0);
            let fill_cov = ((radius + edge * 0.5 - d) / edge).clamp(0.0, 1.0);
            let i = ((y * width + x) * 4) as usize;
            blend_over(&mut rgba[i..i + 4], dark, ring_cov);
            blend_over(&mut rgba[i..i + 4], colour, fill_cov);
        }
    }
}

/// Source-over `colour` at `coverage` onto one straight-alpha RGBA pixel.
fn blend_over(px: &mut [u8], colour: Color32, coverage: f32) {
    if coverage <= 0.0 {
        return;
    }
    let sa = coverage.clamp(0.0, 1.0);
    let da = px[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    if oa <= 0.0 {
        return;
    }
    let src = [colour.r() as f32, colour.g() as f32, colour.b() as f32];
    for c in 0..3 {
        let dst = px[c] as f32;
        px[c] = ((src[c] * sa + dst * da * (1.0 - sa)) / oa)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    px[3] = (oa * 255.0).round().clamp(0.0, 255.0) as u8;
}

// ---------------------------------------------------------------------------
// desktop notifications (org.freedesktop.Notifications over the session bus)
// ---------------------------------------------------------------------------

/// Set once the first failed notification has been logged, so an unreachable
/// bus costs one line on stderr and nothing else.
static NOTIFY_FAILURE_LOGGED: AtomicBool = AtomicBool::new(false);

/// Decides and sends desktop notices for the process that owns sync: the
/// tray when `run_in_tray` is on, otherwise the window. The window in tray
/// mode never has one, so no notice is sent twice.
struct Notifier {
    prev: AppState,
    mem: NotifyMemory,
}

impl Notifier {
    fn new(snap: &AppState) -> Self {
        Notifier {
            prev: Self::remember(snap),
            mem: NotifyMemory::default(),
        }
    }

    /// The policy reads phases, errors and the account; the feeds are dropped
    /// so the kept copy stays small.
    fn remember(snap: &AppState) -> AppState {
        let mut kept = snap.clone();
        kept.activity.clear();
        kept.active.clear();
        kept
    }

    /// One pass: drain the settled conflicts and send what the policy says.
    /// With notifications off the memory is cleared, so nothing old fires
    /// when they are turned back on.
    fn tick(&mut self, ctrl: &Controller, snap: &AppState, enabled: bool) {
        let settled = ctrl.take_settled_conflicts();
        if enabled {
            for notice in
                notify_policy::notices(&self.prev, snap, &settled, &mut self.mem, now_secs())
            {
                send_notification(notice.title, notice.body);
            }
        } else {
            self.mem = NotifyMemory::default();
        }
        self.prev = Self::remember(snap);
    }
}

/// Send one notice on a short-lived thread, so a slow notification service
/// never holds up the tray tick or a frame. Failures are logged once.
fn send_notification(title: String, body: String) {
    thread::spawn(move || {
        if let Err(e) = notify_over_dbus(&title, &body) {
            if !NOTIFY_FAILURE_LOGGED.swap(true, Ordering::SeqCst) {
                eprintln!(
                    "neutronsync: desktop notifications are unavailable ({e}); sync continues without them"
                );
            }
        }
    });
}

/// `org.freedesktop.Notifications.Notify` with app name "NeutronSync" and the
/// installed `neutronsync` icon. No actions.
fn notify_over_dbus(title: &str, body: &str) -> Result<(), gtk::glib::Error> {
    use gtk::gio;
    use gtk::glib::ToVariant;
    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)?;
    let mut hints: std::collections::HashMap<String, gtk::glib::Variant> =
        std::collections::HashMap::new();
    hints.insert("desktop-entry".into(), "neutronsync-gui".to_variant());
    let args = (
        "NeutronSync",
        0u32,
        "neutronsync",
        title,
        body,
        Vec::<String>::new(),
        hints,
        -1i32,
    )
        .to_variant();
    conn.call_sync(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// file manager sidebar bookmarks
// ---------------------------------------------------------------------------

/// `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, falling back to `~/.config`.
fn bookmarks_path() -> PathBuf {
    config::config_home().join("gtk-3.0").join("bookmarks")
}

/// Bring the sidebar bookmarks in line with the config: one line per synced
/// folder while the option is on, none of ours when it is off. Only lines
/// NeutronSync added (listed in `<state_dir>/bookmarks.json`) are ever
/// removed. With the option off and nothing owned the bookmarks file is not
/// opened at all.
fn reconcile_bookmarks(cfg: &Config) {
    let owned_path = cfg.state_dir.join("bookmarks.json");
    let owned: Vec<bookmarks::Owned> = std::fs::read(&owned_path)
        .ok()
        .and_then(|bytes| read_owned_bookmarks(&bytes))
        .unwrap_or_default();
    if !cfg.file_manager_bookmarks && owned.is_empty() {
        return;
    }
    let wanted: Vec<bookmarks::Entry> = if cfg.file_manager_bookmarks {
        cfg.pairs
            .iter()
            .filter(|p| p.local.is_absolute())
            .map(|p| bookmarks::Entry {
                path: p.local.clone(),
                label: p.name.clone(),
            })
            .collect()
    } else {
        Vec::new()
    };
    let path = bookmarks_path();
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    let result = bookmarks::reconcile(&current, &wanted, &owned);
    if result.changed {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        write_atomic(&path, result.text.as_bytes());
    }
    if result.owned != owned {
        let _ = std::fs::create_dir_all(&cfg.state_dir);
        if let Ok(json) = serde_json::to_vec(&result.owned) {
            write_atomic(&owned_path, &json);
        }
    }
}

/// `bookmarks.json`: a list of `{path, label}` records. The first version of
/// the file held bare paths; those are read as records without a label,
/// which `bookmarks::reconcile` matches only when the folder has one line.
fn read_owned_bookmarks(bytes: &[u8]) -> Option<Vec<bookmarks::Owned>> {
    if let Ok(records) = serde_json::from_slice::<Vec<bookmarks::Owned>>(bytes) {
        return Some(records);
    }
    let paths: Vec<PathBuf> = serde_json::from_slice(bytes).ok()?;
    Some(
        paths
            .into_iter()
            .map(|path| bookmarks::Owned { path, label: None })
            .collect(),
    )
}

/// Write through a temp file in the same directory, then rename over.
fn write_atomic(path: &Path, bytes: &[u8]) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".neutronsync-tmp");
    let tmp = PathBuf::from(tmp);
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

// ---------------------------------------------------------------------------
// single-instance locks + process spawning (daemon <-> window coordination)
// ---------------------------------------------------------------------------

/// Advisory process lock: a PID, start time and boot ID checked against `/proc`.
/// Removed on drop. Used so at most one daemon and one window
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
        neutronsync::process_lock::write(&path).ok()?;
        Some(PidLock { path })
    }
}

impl Drop for PidLock {
    fn drop(&mut self) {
        neutronsync::process_lock::remove_owned(&self.path);
    }
}

/// Last-modified time of a file, if it exists (used to detect config changes).
fn file_mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Whether the process identity recorded in `lock` is still running.
fn pid_alive_from(lock: &std::path::Path) -> bool {
    neutronsync::process_lock::is_alive(lock)
}

/// Age of `status.json` in seconds, if the file exists and has a modification time.
fn status_age_secs(state_dir: &Path) -> Option<f64> {
    let modified = std::fs::metadata(state_dir.join("status.json"))
        .and_then(|meta| meta.modified())
        .ok()?;
    modified.elapsed().ok().map(|age| age.as_secs_f64())
}

/// Whether a Wayland or X11 display socket for this session still exists.
fn display_session_present() -> bool {
    if let Some(path) = tray_policy::wayland_socket_path(
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
        std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
    ) {
        if path.exists() {
            return true;
        }
    }
    if let Some(path) = std::env::var("DISPLAY")
        .ok()
        .as_deref()
        .and_then(tray_policy::x11_socket_path)
    {
        if path.exists() {
            return true;
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
/// spawns must reach the OS keyring via D-Bus; without these the sidecar finds
/// no session even when the user is signed in elsewhere.
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
        if let Some(pid) = s
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u32>().ok())
        {
            if pid_alive_from(&lock) {
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

#[derive(Default)]
struct ApiSignIn {
    username: String,
    password: String,
    factor: String,
    step: serde_json::Value,
    error: Option<String>,
    pending: Option<std::sync::mpsc::Receiver<Result<serde_json::Value, String>>>,
}

/// Samples of the tray lock and `status.json`, at most once every two seconds.
struct DaemonWatch {
    last_check: f64,
    alive: bool,
    banner: Option<DaemonBanner>,
    attempts: u32,
    spawned: bool,
    last_spawn: f64,
    status_missing_since: Option<f64>,
    /// Set when this window itself started the tray. The "stopped" banner waits
    /// out the first two seconds, because that spawn is a start, not a restart.
    startup_grace: bool,
}

impl DaemonWatch {
    fn new() -> Self {
        Self {
            last_check: -1.0,
            alive: true,
            banner: None,
            attempts: 0,
            spawned: false,
            last_spawn: 0.0,
            status_missing_since: None,
            startup_grace: false,
        }
    }
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
    last_account_poll: f64,
    api_signin: ApiSignIn,

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
    daemon_watch: DaemonWatch,
    _win_lock: Option<PidLock>,

    // Held deletions and the conflict inbox, read from stats.db at most every
    // two seconds so they are right whichever process did the sync.
    holds: Vec<HeldDeletes>,
    conflicts: Vec<ConflictRecord>,
    last_state_poll: f64,
    /// The pair whose "Delete them" answer is awaiting confirmation.
    confirm_delete_held: Option<String>,

    // `--screenshots <dir>`: drive the window through scripted scenes with
    // made-up data and write one PNG per scene. None in normal use.
    demo: Option<Demo>,

    /// Desktop notices, decided from this window's own controller. Unused
    /// in tray mode, where the tray process owns sync and notifies.
    notifier: Notifier,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let config_path = config::find_config(None).unwrap_or_else(config::default_config_path);
        let (cfg, config_loaded) = match config::load(None) {
            Ok(c) => (c, true),
            Err(_) => (starter_config(&config_path), false),
        };
        // The desktop's preference is unknown until the first frame; a fixed
        // choice applies before anything is painted so a light window never
        // flashes dark.
        LIGHT_MODE.store(cfg.theme == Theme::Light, Ordering::Relaxed);
        install_theme(&cc.egui_ctx);
        ensure_desktop_integration();
        let ctrl = Controller::new(cfg.clone());
        let notifier = Notifier::new(&ctrl.snapshot());
        // With the tray on, a headless daemon owns the tray + watcher; this window
        // just ensures one exists, takes a window lock, and exits on close.
        let mode_daemon = cfg.run_in_tray;
        let mut daemon_watch = DaemonWatch::new();
        let win_lock = if mode_daemon {
            if !daemon_running(&cfg.state_dir) {
                spawn_daemon();
                // egui's clock starts at 0, so this counts as a spawn at open.
                // The stopped banner waits until the first poll after two seconds.
                daemon_watch.spawned = true;
                daemon_watch.last_spawn = 0.0;
                daemon_watch.attempts = 1;
                daemon_watch.startup_grace = true;
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
            last_account_poll: 0.0,
            api_signin: ApiSignIn::default(),
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
            daemon_watch,
            _win_lock: win_lock,
            holds: Vec::new(),
            conflicts: Vec::new(),
            last_state_poll: -1.0,
            confirm_delete_held: None,
            demo: None,
            notifier,
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

    /// Pick the palette for this frame: the Appearance setting, or the
    /// desktop's preference when it is "system" (dark when the desktop
    /// reports none). Repaints egui's widgets only when the palette changes.
    fn sync_theme(&self, ctx: &egui::Context) {
        let light = match self.cfg.theme {
            Theme::Light => true,
            Theme::Dark => false,
            Theme::System => ctx.system_theme() == Some(egui::Theme::Light),
        };
        if LIGHT_MODE.load(Ordering::Relaxed) != light {
            LIGHT_MODE.store(light, Ordering::Relaxed);
            apply_visuals(ctx);
        }
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
        if self.demo.is_none() {
            reconcile_bookmarks(&self.cfg);
        }
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

    /// Sample the tray lock and `status.json` at most every two seconds.
    fn poll_daemon_liveness(&mut self, now: f64) {
        if !self.mode_daemon || self.demo.is_some() {
            return;
        }
        if !tray_policy::check_due(now, self.daemon_watch.last_check, 2.0) {
            return;
        }
        self.daemon_watch.last_check = now;
        let alive = daemon_running(&self.cfg.state_dir);
        if self.daemon_watch.startup_grace && !alive && now - self.daemon_watch.last_spawn < 2.0 {
            // A tray this window just started is not "stopped" yet. Ignore a
            // leftover status.json until the grace passes or the lock appears.
            self.daemon_watch.alive = false;
            self.daemon_watch.banner = None;
            return;
        }
        self.daemon_watch.startup_grace = false;
        let age = status_age_secs(&self.cfg.state_dir);
        if age.is_some() || !alive {
            self.daemon_watch.status_missing_since = None;
        }
        let missing_for = if alive && age.is_none() {
            let since = self.daemon_watch.status_missing_since.get_or_insert(now);
            Some(now - *since)
        } else {
            None
        };
        let stale = tray_policy::status_is_stale(age, missing_for);
        let spawn_due = !self.daemon_watch.spawned || now - self.daemon_watch.last_spawn >= 10.0;
        let decision =
            tray_policy::window_liveness(alive, stale, self.daemon_watch.attempts, spawn_due);
        self.daemon_watch.alive = alive;
        self.daemon_watch.banner = decision.banner;
        self.daemon_watch.attempts = decision.attempts;
        if decision.spawn {
            spawn_daemon();
            self.daemon_watch.spawned = true;
            self.daemon_watch.last_spawn = now;
        }
    }

    /// Re-read held deletions and the conflict inbox from stats.db, at most
    /// every two seconds. Both are written by whichever process ran the sync
    /// (this window, the tray, or the CLI), so the file is the one truth.
    fn poll_state(&mut self, now: f64) {
        if self.demo.is_some() || !tray_policy::check_due(now, self.last_state_poll, 2.0) {
            return;
        }
        self.last_state_poll = now;
        self.holds = self.ctrl.held_deletes();
        self.conflicts = self.ctrl.conflicts();
    }

    /// "Try again" resets the failure count and starts background sync once.
    fn respawn_daemon_now(&mut self, now: f64) {
        let decision = tray_policy::window_liveness(false, false, 0, true);
        self.daemon_watch.alive = false;
        self.daemon_watch.banner = decision.banner;
        self.daemon_watch.attempts = decision.attempts;
        if decision.spawn {
            spawn_daemon();
            self.daemon_watch.spawned = true;
            self.daemon_watch.last_spawn = now;
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
            let _ = tx.send(neutronsync::backend::list_remote(&cfg, &path));
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
            let _ = tx.send(neutronsync::backend::list_remote(&cfg, &remote));
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
                        ui.label(RichText::new("Loading folders…").color(pal().dim));
                    });
                } else if let Some(e) = self.excl_error.clone() {
                    ui.label(RichText::new(e).size(12.0).color(pal().warn));
                } else if self.excl_entries.is_empty() {
                    ui.label(
                        RichText::new("This Proton folder has no subfolders to choose from.")
                            .size(12.0)
                            .color(pal().dim),
                    );
                } else {
                    ui.label(
                        RichText::new(
                            "Untick a folder to stop syncing it. Excluded folders are left \
                             alone — the Proton copy is never touched.",
                        )
                        .size(12.0)
                        .color(pal().dim),
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
                        .color(pal().text),
                );
                for f in &folders {
                    ui.label(
                        RichText::new(format!("   • {f}"))
                            .size(12.0)
                            .color(pal().dim),
                    );
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Remove them from this computer to free up space? Your Proton copy \
                         stays untouched, and they re-download if you include them again.",
                    )
                    .size(12.0)
                    .color(pal().dim),
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

/// Fonts once, then the visuals for the active palette.
fn install_theme(ctx: &egui::Context) {
    install_fonts(ctx);
    apply_visuals(ctx);
}

fn install_fonts(ctx: &egui::Context) {
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
}

/// Paint egui's own widgets from the active palette. Called at start and
/// again whenever `sync_theme` switches palettes, so a change in Settings
/// repaints at once.
fn apply_visuals(ctx: &egui::Context) {
    let light = LIGHT_MODE.load(Ordering::Relaxed);
    let mut visuals = if light {
        egui::Visuals::light()
    } else {
        egui::Visuals::dark()
    };
    visuals.panel_fill = pal().panel;
    visuals.window_fill = pal().panel;
    visuals.window_stroke = Stroke::new(1.0, pal().line());
    visuals.extreme_bg_color = pal().nav_bg;
    visuals.override_text_color = Some(pal().text);
    visuals.selection.bg_fill = pal().accent.gamma_multiply(0.5);
    visuals.hyperlink_color = pal().accent_hi;
    visuals.widgets.inactive.bg_fill = pal().panel2;
    visuals.widgets.inactive.weak_bg_fill = pal().panel2;
    visuals.widgets.hovered.bg_fill = pal().sel;
    visuals.widgets.hovered.weak_bg_fill = pal().sel;
    visuals.widgets.active.bg_fill = pal().sel;
    // Pin egui's own theme to ours, so its light/dark switch never fights
    // the palette: the Appearance setting is the one source of truth.
    ctx.set_theme(if light {
        egui::ThemePreference::Light
    } else {
        egui::ThemePreference::Dark
    });
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
            .color(pal().text),
    );
}

impl eframe::App for App {
    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        let ctx = &ctx;
        self.sync_theme(ctx);
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
        let now = ctx.input(|i| i.time);
        self.poll_daemon_liveness(now);
        self.poll_state(now);
        // pump the remote browser channel
        if let Some(rx) = &self.browser_rx {
            if let Ok(res) = rx.try_recv() {
                self.browser_rx = None;
                self.browser_loading = false;
                match res {
                    Ok(entries) => {
                        let mut dirs: Vec<Entry> =
                            entries.into_iter().filter(|e| e.is_dir).collect();
                        dirs.sort_by_key(|d| d.path.to_lowercase());
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
        let snap = if let Some(state) = self.demo_frame(ctx) {
            state
        } else {
            let mine = self.ctrl.snapshot();
            // Only the process that owns sync notifies (D1): this window when
            // the tray is off; in tray mode the tray does it.
            if !self.mode_daemon {
                self.notifier
                    .tick(&self.ctrl, &mine, self.cfg.notifications);
            }
            // A dead tray leaves status.json behind. Ignore it and show this
            // window's own state until background sync is publishing again.
            if self.mode_daemon && self.daemon_watch.alive {
                match Controller::read_status(&self.cfg.state_dir) {
                    Some(mut pubd) => {
                        pubd.account = mine.account.clone();
                        // The window probes auth directly; trust it over the
                        // daemon's possibly stale signed_out flag (up to 20s lag
                        // without a refresh signal).
                        if mine.account.checked {
                            pubd.signed_out = mine.account.backend_ready
                                && !mine.account.signed_in
                                && !mine.account.keyring_locked;
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
        // CONFIRMED a reachable, signed-in sidecar. Covers both "sidecar missing"
        // (page_signin explains) and "present but signed out".
        let signin =
            snap.account.checked && !(snap.account.backend_ready && snap.account.signed_in);
        // The gate used to probe once. A session that was already in the keyring,
        // or that becomes readable after the keyring unlocks, never got a second
        // look, so the login page stayed up until Refresh. Keep asking.
        if signin && self.demo.is_none() {
            let now = ctx.input(|i| i.time);
            if now - self.last_account_poll > 2.5 && !snap.account.checking {
                self.ctrl.refresh_account(true);
                self.last_account_poll = now;
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(400));
        }

        if !signin {
            self.nav_rail(root_ui, &snap);
        }

        egui::containers::CentralPanel::default().show(root_ui, |ui| {
            let mut try_again = false;
            if !snap.signed_out {
                if let Some(kind) = self.daemon_watch.banner {
                    if daemon_banner(ui, kind) {
                        try_again = true;
                    }
                    ui.add_space(14.0);
                }
            }
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
                        Nav::Conflicts => self.page_conflicts(ui),
                        Nav::Settings => self.page_settings(ui),
                        Nav::Account => self.page_account(ui, &snap),
                        Nav::About => self.page_about(ui),
                    }
                });
            }
            if try_again {
                self.respawn_daemon_now(now);
            }
        });

        self.browser_window(ctx);
        self.exclude_window(ctx);
        self.remove_local_dialog(ctx);
        self.delete_held_dialog(ctx);
        self.logout_dialog(ctx);
        self.reset_dialog(ctx);
        self.toast_overlay(ctx);

        // auto-save any config change (settings, folders, auto-sync) — no manual Save needed
        if self.dirty {
            self.save_silent();
        }

        if snap.busy || snap.watching || self.nav_t < 1.0 || self.toast.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        } else if self.demo.is_none() {
            // Holds and conflicts can arrive from the tray or the CLI with no
            // input here; wake up for the next two-second read.
            ctx.request_repaint_after(std::time::Duration::from_millis(2100));
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
            .fill(pal().nav_bg)
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
                                .color(pal().text),
                        );
                        ui.label(
                            RichText::new("Proton Drive sync for Linux")
                                .size(11.0)
                                .color(pal().dim),
                        );
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
                    pal().sel,
                );
                painter.rect_filled(
                    Rect::from_min_size(pos2(x, cur_y + 9.0), vec2(3.0, row_h - 18.0)),
                    2.0,
                    pal().accent,
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
                            mix(Color32::TRANSPARENT, pal().hover(), hov),
                        );
                    }
                    let col = if selected {
                        pal().text
                    } else {
                        mix(pal().dim, pal().text, hov)
                    };
                    let icol = if selected { pal().accent } else { col };
                    let ir = Rect::from_center_size(
                        pos2(rect.left() + 24.0, rect.center().y),
                        vec2(18.0, 18.0),
                    );
                    draw_icon(ui.painter(), ir, *icon, icol);
                    let font = FontId::new(
                        13.0,
                        if selected {
                            ff_bold()
                        } else {
                            FontFamily::Proportional
                        },
                    );
                    let label_rect = ui.painter().text(
                        pos2(rect.left() + 44.0, rect.center().y),
                        Align2::LEFT_CENTER,
                        *label,
                        font,
                        col,
                    );
                    // The number of conflicts waiting, beside the label.
                    if *nav == Nav::Conflicts && !self.conflicts.is_empty() {
                        let count = thousands(self.conflicts.len());
                        let cfont = FontId::new(11.0, ff_bold());
                        let cw = galley_w(ui, &count, cfont.clone()) + 12.0;
                        let pill = Rect::from_min_size(
                            pos2(label_rect.right() + 8.0, rect.center().y - 9.0),
                            vec2(cw, 18.0),
                        );
                        ui.painter()
                            .rect_filled(pill, 9.0, pal().warn.gamma_multiply(0.22));
                        ui.painter().text(
                            pill.center(),
                            Align2::CENTER_CENTER,
                            count,
                            cfont,
                            pal().warn,
                        );
                    }
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
                            .fill(pal().nav_bg)
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
                            Stroke::new(1.0, pal().line()),
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
            (pal().accent, true, "Syncing…".to_string())
        } else if snap.watching {
            (pal().accent, true, "Auto sync on".to_string())
        } else if any_err {
            (pal().danger, false, "Sync error".to_string())
        } else if let Some(ts) = last {
            (pal().ok, false, format!("Synced {}", rel_time(ts)))
        } else {
            (pal().dim, false, "Idle".to_string())
        };
        ui.horizontal(|ui| {
            status_dot(ui, col, pulse);
            ui.add_space(2.0);
            ui.label(RichText::new(text).size(12.5).color(pal().text));
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
                ui.label(RichText::new("Auto sync").size(12.5).color(pal().text));
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
                ui.label(RichText::new(sub).size(11.0).color(pal().dim));
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
        let suppress_phase =
            self.mode_daemon && self.daemon_watch.banner.is_some() && !snap.signed_out;
        let (bcol, bicon, btitle, bsub) = if snap.signed_out {
            // A signed-out session is surfaced explicitly — everything else is
            // blocked on it, so it takes priority over scan/sync/idle states
            // and over the background-sync banner drawn above this page.
            (
                pal().danger,
                Icon::Info,
                "Signed out of Proton",
                "Sign in on the Account tab to resume syncing".into(),
            )
        } else if suppress_phase {
            (pal().accent, Icon::Info, "", String::new())
        } else if active {
            (
                pal().accent,
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
            (pal().accent, Icon::Cloud, "Auto sync on", {
                let tracked: usize = snap.pairs.iter().map(|p| p.tracked).sum();
                let folders = plural(self.auto_names().len(), "folder");
                if tracked > 0 {
                    format!("Watching {folders} · {} tracked", plural(tracked, "file"))
                } else {
                    format!("Watching {folders} for changes")
                }
            })
        } else if snap.pairs.iter().any(|p| p.phase == Phase::Error) {
            (
                pal().danger,
                Icon::Info,
                "Last sync had errors",
                "See the log below".into(),
            )
        } else {
            (pal().ok, Icon::Check, "Synced", last_synced_line(snap))
        };
        if !suppress_phase {
            let _ = activity_banner(ui, bcol, bicon, btitle, &bsub, busy, None);
        }

        // One banner per folder whose deletions were held back. Signed out
        // wins: nothing can be answered until the session is back.
        if !snap.signed_out {
            let holds = self.holds.clone();
            for hold in &holds {
                ui.add_space(10.0);
                match held_banner(ui, hold, !active) {
                    HeldAction::Restore => {
                        self.ctrl.restore_held(hold.pair.clone());
                        self.toast(
                            ui.ctx(),
                            format!("Bringing the files back in {}…", hold.pair),
                            false,
                        );
                    }
                    HeldAction::Delete => {
                        self.confirm_delete_held = Some(hold.pair.clone());
                    }
                    HeldAction::None => {}
                }
            }
        }

        ui.add_space(14.0);

        // live in-progress operations, pinned above the feed
        if snap.active.len() > 1 {
            ui.label(
                RichText::new(format!("{} files in flight", snap.active.len()))
                    .font(FontId::new(11.0, ff_bold()))
                    .color(pal().accent),
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
                activity_cols(ui, "FOLDER", pal().dim2, "WHEN", pal().dim2, false, |ui| {
                    ui.add_space(32.0);
                    ui.label(RichText::new("NAME").size(11.0).color(pal().dim2));
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
                            activity_cols(
                                ui,
                                "",
                                pal().dim2,
                                &rel_time(item.ts),
                                pal().dim2,
                                false,
                                |ui| {
                                    let (ir, _) =
                                        ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
                                    ui.painter().rect_filled(
                                        ir,
                                        6.0,
                                        pal().danger.gamma_multiply(0.16),
                                    );
                                    draw_icon(
                                        ui.painter(),
                                        ir.shrink(5.0),
                                        Icon::Info,
                                        pal().danger,
                                    );
                                    ui.add_space(8.0);
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&item.text)
                                                .size(13.0)
                                                .color(pal().danger),
                                        )
                                        .truncate(),
                                    );
                                },
                            );
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

        ui.label(RichText::new("Each pair keeps a local folder and a Proton Drive folder in sync, both ways. Sync one on demand, or set it to Auto to keep it live while Auto sync is on.").size(13.0).color(pal().dim));
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
                                .rect_filled(ir, 8.0, pal().accent.gamma_multiply(0.18));
                            draw_icon(ui.painter(), ir.shrink(7.0), Icon::Folder, pal().accent);
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
                                if let Some(h) =
                                    self.holds.iter().find(|h| h.pair == self.cfg.pairs[i].name)
                                {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} held back",
                                            plural(h.local + h.remote, "deletion")
                                        ))
                                        .size(12.0)
                                        .color(pal().warn),
                                    );
                                }
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
                                    let syncing = ps.as_ref().is_some_and(|p| {
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
                                    ui.label(RichText::new("Auto").size(12.0).color(pal().dim));
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
                                    pal().accent,
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
                            draw_icon(ui.painter(), ar, Icon::Arrow, pal().accent);
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
                let pair = Pair {
                    name: name.clone(),
                    local: dir,
                    remote,
                    auto: true,
                    exclude: Vec::new(),
                };
                if let Err(e) = config::validate_pair(&self.cfg, &pair, None) {
                    self.toast(ui.ctx(), format!("{e:#}"), true);
                } else {
                    self.cfg.pairs.push(pair);
                    changed = true;
                    sync_one = Some(name); // auto-initialize: scan both sides + first sync
                }
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

    fn page_conflicts(&mut self, ui: &mut egui::Ui) {
        let count = self.conflicts.len();
        let mut dismiss_all = false;
        self.page_header(ui, "Conflicts", |ui, _app| {
            if count > 0 && button(ui, None, "Dismiss all", Btn::Secondary, false, true).clicked() {
                dismiss_all = true;
            }
        });
        if self.conflicts.is_empty() {
            empty_state(
                ui,
                "No conflicts.",
                "A file changed on both sides since its last sync shows up here once \
                 a sync has settled it.",
            );
            return;
        }
        ui.label(
            RichText::new(
                "Files that changed on both sides since their last sync, and how each \
                 one was settled. Dismiss a row once you have looked at it.",
            )
            .size(13.0)
            .color(pal().dim),
        );
        ui.add_space(12.0);

        let locals: std::collections::HashMap<String, PathBuf> = self
            .cfg
            .pairs
            .iter()
            .map(|p| (p.name.clone(), p.local.clone()))
            .collect();
        let rows = self.conflicts.clone();
        let mut open_target: Option<PathBuf> = None;
        let mut dismiss: Option<i64> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.set_max_width((ui.available_width() - 15.0).max(200.0));
                for c in &rows {
                    let root = locals.get(&c.pair);
                    let copy_full = c
                        .copy
                        .as_ref()
                        .and_then(|copy| root.map(|r| r.join(copy)))
                        .filter(|p| p.exists());
                    card_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (ir, _) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::hover());
                            ui.painter()
                                .rect_filled(ir, 8.0, pal().warn.gamma_multiply(0.18));
                            draw_icon(ui.painter(), ir.shrink(7.0), Icon::Conflict, pal().warn);
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&c.path)
                                            .font(FontId::new(14.0, ff_bold()))
                                            .color(pal().text),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(&c.path);
                                ui.label(
                                    RichText::new(conflict_sentence(c))
                                        .size(12.5)
                                        .color(pal().dim),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    let when = if c.seen > 1 {
                                        format!("{} · {} times", rel_time(c.last_ts), c.seen)
                                    } else {
                                        rel_time(c.last_ts)
                                    };
                                    ui.label(RichText::new(when).size(11.5).color(pal().dim2));
                                    ui.add_space(10.0);
                                    ui.label(RichText::new(&c.pair).size(12.0).color(pal().accent));
                                },
                            );
                        });
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if button(
                                ui,
                                Some(Icon::Folder),
                                "Show in folder",
                                Btn::Secondary,
                                true,
                                root.is_some(),
                            )
                            .clicked()
                            {
                                if let Some(r) = root {
                                    let parent =
                                        c.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
                                    open_target = Some(r.join(parent));
                                }
                            }
                            if let Some(copy) = &copy_full {
                                if button(ui, None, "Open copy", Btn::Ghost, true, true).clicked() {
                                    open_target = Some(copy.clone());
                                }
                            }
                            if button(ui, None, "Dismiss", Btn::Ghost, true, true).clicked() {
                                dismiss = Some(c.id);
                            }
                        });
                    });
                    ui.add_space(10.0);
                }
            });
        if let Some(p) = open_target {
            open_path(&p);
        }
        if let Some(id) = dismiss {
            self.ctrl.dismiss_conflict(id);
            self.conflicts.retain(|c| c.id != id);
            self.last_state_poll = -1.0;
        }
        if dismiss_all {
            self.ctrl.dismiss_all_conflicts();
            self.conflicts.clear();
            self.last_state_poll = -1.0;
        }
    }

    /// "Delete them" asks once more, and says where the files go.
    fn delete_held_dialog(&mut self, ctx: &egui::Context) {
        let Some(name) = self.confirm_delete_held.clone() else {
            return;
        };
        let Some(hold) = self.holds.iter().find(|h| h.pair == name).cloned() else {
            self.confirm_delete_held = None;
            return;
        };
        let total = hold.local + hold.remote;
        let local_fate = match self.cfg.local_delete {
            LocalDelete::Trash => "go to the desktop trash",
            LocalDelete::Remove => "are removed from this computer for good",
        };
        let mut parts = Vec::new();
        if hold.remote > 0 {
            parts.push(format!(
                "the {} removed on this computer go to the Proton Drive trash",
                plural(hold.remote, "file")
            ));
        }
        if hold.local > 0 {
            parts.push(format!(
                "the {} removed in Proton Drive {local_fate}",
                plural(hold.local, "file")
            ));
        }
        let body = format!(
            "This finishes the deletion that was held back: {}.",
            parts.join(", and ")
        );
        let mut choice: Option<bool> = None;
        egui::Window::new("Delete held files")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .frame(card_frame().inner_margin(egui::Margin::same(18)))
            .title_bar(false)
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                ui.label(
                    RichText::new(format!("Delete {} from {name}?", plural(total, "file")))
                        .font(FontId::new(15.0, ff_bold()))
                        .color(pal().text),
                );
                ui.add_space(4.0);
                ui.label(RichText::new(body).size(12.5).color(pal().dim));
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(
                            ui,
                            Some(Icon::Trash),
                            "Delete them",
                            Btn::Danger,
                            false,
                            true,
                        )
                        .clicked()
                        {
                            choice = Some(true);
                        }
                        if button(ui, None, "Cancel", Btn::Secondary, false, true).clicked() {
                            choice = Some(false);
                        }
                    });
                });
            });
        if let Some(go) = choice {
            self.confirm_delete_held = None;
            if go {
                self.ctrl.delete_held(name.clone());
                self.toast(ctx, format!("Deleting the held files in {name}…"), false);
                self.set_nav(Nav::Activity);
            }
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
                    let notify = self.cfg.notifications;
                    if setting_row(
                        ui,
                        "Show desktop notifications",
                        "Files changed in two places, a sync problem that does not clear, \
                         and a Proton sign-out.",
                        |ui| switch(ui, notify),
                    ) {
                        self.cfg.notifications = !notify;
                        self.dirty = true;
                    }
                    if setting_row(
                        ui,
                        "Appearance",
                        "Follow the desktop's light or dark preference, or fix one.",
                        |ui| combo_theme(ui, &mut self.cfg.theme),
                    ) {
                        self.dirty = true;
                    }
                    let marks = self.cfg.file_manager_bookmarks;
                    if setting_row(
                        ui,
                        "Show synced folders in the file manager sidebar",
                        "One bookmark per folder. Only bookmarks NeutronSync added are \
                         ever removed; your own stay as they are.",
                        |ui| switch(ui, marks),
                    ) {
                        self.cfg.file_manager_bookmarks = !marks;
                        self.dirty = true;
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

                settings_group(ui, "NETWORK", |ui| {
                    if setting_row(
                        ui,
                        "Upload limit",
                        "Applies to all folders. Takes effect on the next sync.",
                        |ui| combo_speed_limit(ui, "upload_limit", &mut self.cfg.upload_limit_kbps),
                    ) {
                        self.dirty = true;
                    }
                    if setting_row(
                        ui,
                        "Download limit",
                        "Applies to all folders. Takes effect on the next sync.",
                        |ui| {
                            combo_speed_limit(
                                ui,
                                "download_limit",
                                &mut self.cfg.download_limit_kbps,
                            )
                        },
                    ) {
                        self.dirty = true;
                    }
                });

                settings_group(ui, "ADVANCED", |ui| {
                    if setting_row(ui, "Backend", "", |ui| {
                        ui.label(
                            RichText::new("Proton Drive SDK (neutronsync-drive)")
                                .size(12.5)
                                .color(pal().dim),
                        );
                        false
                    }) {
                        // read-only
                    }
                    if setting_row(ui, "Remote root", "", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.cfg.remote_root)
                                .desired_width(220.0),
                        )
                        .changed()
                    }) {
                        self.dirty = true;
                    }
                    if setting_row(
                        ui,
                        "Full check every",
                        "Safety-net walk of every folder on both sides.",
                        |ui| combo_full_walk(ui, &mut self.cfg.full_walk_interval),
                    ) {
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
        let (col, label) = if !acc.checked || acc.checking {
            (pal().dim, "Checking…")
        } else if acc.signed_in {
            (pal().ok, "Signed in")
        } else if acc.keyring_locked {
            (pal().warn, "Keyring locked")
        } else if acc.backend_ready {
            (pal().danger, "Not signed in")
        } else {
            (pal().warn, "Sign-in service unavailable")
        };
        let checking = acc.checking;
        let name = if acc.account.is_empty() {
            "No Proton account".to_string()
        } else {
            acc.account.clone()
        };
        let mut sign_out = false;
        let mut refresh = false;

        // identity card: avatar, account, status, actions
        ui.scope(|ui| {
            ui.set_max_width((ui.available_width() - 15.0).max(200.0));
            card_frame().show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (ar, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
                    let initial = acc
                        .account
                        .chars()
                        .next()
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_default();
                    let p = ui.painter();
                    if acc.signed_in {
                        p.circle_filled(ar.center(), 24.0, pal().accent);
                        p.text(
                            ar.center(),
                            Align2::CENTER_CENTER,
                            initial,
                            FontId::new(20.0, ff_bold()),
                            pal().on_accent,
                        );
                    } else {
                        p.circle_filled(ar.center(), 24.0, pal().sel);
                        draw_icon(p, ar.shrink(13.0), Icon::User, pal().dim);
                    }
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(&name)
                                .font(FontId::new(16.0, ff_bold()))
                                .color(if acc.signed_in { pal().text } else { pal().dim }),
                        );
                        ui.add_space(3.0);
                        ui.horizontal(|ui| {
                            status_dot_small(ui, col);
                            ui.label(RichText::new(label).size(12.5).color(pal().dim));
                            if checking {
                                ui.add_space(2.0);
                                ui.add(egui::Spinner::new().size(12.0).color(pal().dim));
                            }
                        });
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if acc.signed_in
                            && button(ui, None, "Sign out", Btn::Secondary, true, true).clicked()
                        {
                            sign_out = true;
                        }
                        if button(ui, Some(Icon::Sync), "Refresh", Btn::Ghost, true, !checking)
                            .clicked()
                        {
                            refresh = true;
                        }
                    });
                });

                if acc.signed_in || acc.keyring_locked {
                    ui.add_space(14.0);
                    ui.separator();
                    ui.add_space(12.0);
                    let (kcol, kicon, ktext) = if acc.keyring_locked {
                        (
                            pal().warn,
                            Icon::Info,
                            "Session saved in the system keyring, which is locked",
                        )
                    } else {
                        (pal().ok, Icon::Check, "Session saved in the system keyring")
                    };
                    ui.horizontal(|ui| {
                        let (ir, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
                        ui.painter().rect_filled(ir, 6.0, kcol.gamma_multiply(0.16));
                        draw_icon(ui.painter(), ir.shrink(5.0), kicon, kcol);
                        ui.add_space(8.0);
                        ui.label(RichText::new(ktext).size(13.0).color(pal().text));
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "Your password went to Proton only. Signing out removes the \
                             session from this computer; your files stay in Proton Drive.",
                        )
                        .size(12.0)
                        .color(pal().dim),
                    );
                    if !acc.version.is_empty() {
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Helper").size(12.0).color(pal().dim2));
                            ui.add_space(6.0);
                            ui.label(RichText::new(&acc.version).size(12.0).color(pal().dim));
                        });
                    }
                }
            });
        });

        if sign_out {
            self.confirm_logout = true;
        }
        if refresh {
            self.last_account_poll = ui.ctx().input(|i| i.time);
            self.ctrl.refresh_account(false);
        }

        if acc.keyring_locked {
            ui.add_space(16.0);
            ui.label(
                RichText::new(
                    "Your Proton session is still saved. Unlock the system keyring and \
                     NeutronSync will continue on its own.",
                )
                .size(12.5)
                .color(pal().dim),
            );
        } else if acc.backend_ready && !acc.signed_in {
            ui.add_space(20.0);
            self.api_signin_form(ui);
        }
    }

    fn page_about(&mut self, ui: &mut egui::Ui) {
        self.page_header(ui, "About", |_ui, _app| {});
        // The release notes alone can be taller than the window.
        egui::ScrollArea::vertical()
            .id_salt("about_page")
            .auto_shrink([false, false])
            .show(ui, |ui| self.about_body(ui));
    }

    fn about_body(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new(format!("NeutronSync {}", env!("CARGO_PKG_VERSION")))
                .font(FontId::new(16.0, ff_bold()))
                .color(pal().text),
        );
        // The commit this binary was built from, selectable so a bug report
        // can carry it. Never part of the version comparison.
        ui.add(
            egui::Label::new(
                RichText::new(format!("Build {}", env!("NEUTRONSYNC_BUILD_SHA")))
                    .size(12.0)
                    .color(pal().dim),
            )
            .selectable(true),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new("Bidirectional Proton Drive folder sync through the Proton Drive SDK.")
                .color(pal().dim),
        );
        ui.add_space(12.0);
        ui.label(
            RichText::new(format!("Config   {}", self.config_path.display()))
                .size(12.0)
                .color(pal().dim2),
        );
        ui.label(
            RichText::new(format!("State    {}", self.cfg.state_dir.display()))
                .size(12.0)
                .color(pal().dim2),
        );
        if !self.config_loaded {
            ui.add_space(6.0);
            ui.label(
                RichText::new("Config not yet saved — add folders and Save.")
                    .size(12.0)
                    .color(pal().warn),
            );
        }

        ui.add_space(20.0);
        ui.label(
            RichText::new("HOW IT WORKS")
                .font(FontId::new(11.0, ff_bold()))
                .color(pal().accent),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "NeutronSync is an independent, unofficial tool. It talks to Proton Drive \
                 through neutronsync-drive, a helper built on Proton's official Drive SDK. \
                 You sign in inside NeutronSync. Your password goes only to Proton. The \
                 session is kept in your system keyring. Proton's SDK code does all \
                 encryption and decryption, on this computer.",
            )
            .size(12.5)
            .color(pal().dim),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Not affiliated with, endorsed by, or sponsored by Proton AG. \"Proton\" and \
                 \"Proton Drive\" are trademarks of Proton AG.",
            )
            .size(11.5)
            .color(pal().dim2),
        );

        ui.add_space(16.0);
        ui.label(
            RichText::new("DETECTING CHANGES")
                .font(FontId::new(11.0, ff_bold()))
                .color(pal().accent),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Local folders are watched live, and only the changed folder is reconciled. \
                 Proton Drive changes arrive through Proton's change feed within seconds, \
                 and only the affected folders are re-checked. A full walk runs on first \
                 sync, when Proton asks for a refresh, and once a day as a safety net.",
            )
            .size(12.5)
            .color(pal().dim),
        );

        ui.add_space(20.0);
        ui.label(
            RichText::new("UPDATES")
                .font(FontId::new(11.0, ff_bold()))
                .color(pal().accent),
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
                            .color(pal().dim),
                    );
                }
                UpdateState::Checking => {
                    ui.label(RichText::new("Checking…").size(12.0).color(pal().dim));
                }
                UpdateState::Done(Ok(info)) if info.newer => {
                    ui.label(
                        RichText::new(format!(
                            "Update available: {} (you have {})",
                            info.latest, info.current
                        ))
                        .size(12.0)
                        .color(pal().accent),
                    );
                }
                UpdateState::Done(Ok(info)) => {
                    ui.label(
                        RichText::new(format!("Up to date ({}).", info.current))
                            .size(12.0)
                            .color(pal().dim),
                    );
                }
                UpdateState::Done(Err(e)) => {
                    ui.label(
                        RichText::new(format!("Couldn't check: {e}"))
                            .size(12.0)
                            .color(pal().warn),
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
                    ui.label(RichText::new(text).size(12.0).color(pal().accent));
                }
                UpdateState::Verifying => {
                    ui.label(
                        RichText::new("Verifying checksum…")
                            .size(12.0)
                            .color(pal().accent),
                    );
                }
                UpdateState::Installing => {
                    ui.label(
                        RichText::new("Installing… (confirm the password prompt)")
                            .size(12.0)
                            .color(pal().accent),
                    );
                }
                UpdateState::Installed { version } => {
                    ui.label(
                        RichText::new(format!("{version} installed. Restart to finish."))
                            .size(12.0)
                            .color(pal().accent),
                    );
                }
                UpdateState::Downloaded { path } => {
                    ui.label(
                        RichText::new(format!("Downloaded to {}", path.display()))
                            .size(12.0)
                            .color(pal().dim),
                    );
                }
                UpdateState::Failed(e) => {
                    ui.label(RichText::new(e.clone()).size(12.0).color(pal().warn));
                }
            }
        });

        // What changed since this copy, shown before the install button so the
        // choice to update is an informed one.
        let notes = match &*self.update.lock().unwrap() {
            UpdateState::Done(Ok(info)) if info.newer && !info.notes.trim().is_empty() => {
                Some(info.notes.clone())
            }
            _ => None,
        };
        if let Some(notes) = notes {
            ui.add_space(10.0);
            ui.scope(|ui| {
                ui.set_max_width((ui.available_width() - 15.0).max(200.0));
                card_frame().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        RichText::new("WHAT'S NEW")
                            .font(FontId::new(11.0, ff_bold()))
                            .color(pal().accent),
                    );
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .id_salt("release_notes")
                        .max_height(260.0)
                        .show(ui, |ui| release_notes(ui, &notes));
                });
            });
        }

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
            .color(pal().dim2),
        );
    }

    fn page_signin(&mut self, ui: &mut egui::Ui, snap: &AppState) {
        let ready = snap.account.backend_ready;
        let locked = snap.account.keyring_locked;
        let logo = self.logo_tex.clone();
        let col_w: f32 = 470.0;
        let est_h = if locked {
            320.0
        } else if ready {
            420.0
        } else {
            360.0
        };
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
                    let title = if locked {
                        "Unlock your keyring"
                    } else if ready {
                        "Sign in to Proton Drive"
                    } else {
                        "Sign-in service didn't start"
                    };
                    ui.label(
                        RichText::new(title)
                            .font(FontId::new(24.0, ff_bold()))
                            .color(pal().text),
                    );
                    ui.add_space(10.0);
                    if locked {
                        ui.label(
                            RichText::new(
                                "Your Proton session is already saved. The system keyring \
                                 is locked, so NeutronSync can't read it yet. Unlock the \
                                 keyring and this screen goes away. You don't need to \
                                 sign in to Proton again.",
                            )
                            .size(13.5)
                            .color(pal().text),
                        );
                    } else if ready {
                        ui.label(
                            RichText::new(
                                "Your password goes only to Proton; NeutronSync keeps the \
                                 session in the system keyring.",
                            )
                            .size(13.5)
                            .color(pal().text),
                        );
                    }
                });
                if locked {
                    ui.add_space(22.0);
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        ui.add_space(8.0);
                        ui.label(RichText::new("Waiting for the keyring…").color(pal().dim));
                    });
                } else if ready {
                    ui.add_space(20.0);
                    self.api_signin_form(ui);
                } else {
                    ui.add_space(20.0);
                    card_frame().show(ui, |ui| {
                        ui.set_width(col_w - 34.0);
                        let err = if snap.account.version.is_empty() {
                            "The sign-in service could not be reached."
                        } else {
                            snap.account.version.as_str()
                        };
                        ui.label(RichText::new(err).size(12.5).color(pal().danger));
                        ui.add_space(10.0);
                        ui.label(
                            RichText::new("Reinstall NeutronSync, or set [cli] sidecar.")
                                .size(12.5)
                                .color(pal().dim),
                        );
                    });
                    ui.add_space(22.0);
                    ui.vertical_centered(|ui| {
                        if button(
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
                    });
                }
            });
        });
    }

    fn api_signin_form(&mut self, ui: &mut egui::Ui) {
        if let Some(result) = self
            .api_signin
            .pending
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.api_signin.pending = None;
            self.api_signin.factor.clear();
            match result {
                Ok(step) => {
                    if step["ok"] == true {
                        self.api_signin = ApiSignIn::default();
                        self.ctrl.refresh_account(true);
                    } else {
                        if step["need_human_verification"] != true {
                            self.api_signin.password.clear();
                        }
                        self.api_signin.step = step;
                    }
                }
                Err(error) => {
                    self.api_signin.password.clear();
                    self.api_signin.error = Some(plain_signin_error(error));
                }
            }
        }
        let busy = self.api_signin.pending.is_some();
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let col_w = ui.available_width().min(470.0);
        let field_w = (col_w - 34.0).max(200.0);
        let mut submit = false;
        let mut open_verify: Option<String> = None;
        card_frame().show(ui, |ui| {
            ui.set_width(field_w);
            ui.add_enabled_ui(!busy, |ui| {
                let form = &mut self.api_signin;
                let two_factor = form.step["need_2fa"] == true;
                let mailbox = form.step["need_mailbox_password"] == true;
                let hv = form.step["need_human_verification"] == true;
                let pad = egui::Margin::symmetric(4, 2);
                if hv {
                    ui.label(
                        RichText::new("Complete human verification, then retry sign-in.")
                            .size(12.5)
                            .color(pal().text),
                    );
                    ui.add_space(10.0);
                    if let Some(url) = form.step["url"]
                        .as_str()
                        .filter(|url| url.starts_with("https://verify.proton.me/"))
                    {
                        let width = ui.available_width();
                        if button_width(
                            ui,
                            None,
                            "Open verification page",
                            Btn::Secondary,
                            false,
                            true,
                            Some(width),
                        )
                        .clicked()
                        {
                            open_verify = Some(url.to_string());
                        }
                        ui.add_space(8.0);
                    }
                } else if two_factor || mailbox {
                    ui.label(
                        RichText::new(if two_factor {
                            "Two-factor code"
                        } else {
                            "Mailbox password"
                        })
                        .size(12.5)
                        .color(pal().dim),
                    );
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut form.factor)
                            .password(true)
                            .desired_width(field_w - 16.0)
                            .margin(pad),
                    );
                } else {
                    ui.label(RichText::new("Username").size(12.5).color(pal().dim));
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut form.username)
                            .desired_width(field_w - 16.0)
                            .margin(pad),
                    );
                    ui.add_space(10.0);
                    ui.label(RichText::new("Password").size(12.5).color(pal().dim));
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut form.password)
                            .password(true)
                            .desired_width(field_w - 16.0)
                            .margin(pad),
                    );
                }
                if let Some(error) = &form.error {
                    ui.add_space(10.0);
                    ui.label(RichText::new(error).size(12.5).color(pal().danger));
                }
                ui.add_space(14.0);
                let label = if hv { "Retry sign-in" } else { "Sign in" };
                let width = ui.available_width();
                if button_width(
                    ui,
                    Some(Icon::User),
                    label,
                    Btn::Primary,
                    false,
                    !busy,
                    Some(width),
                )
                .clicked()
                    || (enter && !busy)
                {
                    submit = true;
                }
                if busy {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space((ui.available_width() - 24.0).max(0.0) / 2.0);
                        ui.spinner();
                    });
                }
            });
        });
        if let Some(url) = open_verify {
            ui.ctx().open_url(egui::OpenUrl::new_tab(url));
        }
        if submit && !busy {
            let form = &mut self.api_signin;
            let two_factor = form.step["need_2fa"] == true;
            let mailbox = form.step["need_mailbox_password"] == true;
            let hv = form.step["need_human_verification"] == true;
            let (method, params) = if two_factor {
                (
                    "auth.submit_2fa",
                    serde_json::json!({"code": std::mem::take(&mut form.factor)}),
                )
            } else if mailbox {
                (
                    "auth.submit_mailbox_password",
                    serde_json::json!({"password": std::mem::take(&mut form.factor)}),
                )
            } else {
                let mut params =
                    serde_json::json!({"username": form.username, "password": form.password});
                if hv {
                    params["hv_token"] = form.step["token"].clone();
                    params["hv_type"] = form.step["methods"][0].clone();
                }
                ("auth.login", params)
            };
            let (tx, rx) = std::sync::mpsc::channel();
            form.pending = Some(rx);
            form.error = None;
            let cfg = self.cfg.clone();
            let ctx = ui.ctx().clone();
            thread::spawn(move || {
                let result = neutronsync::driveapi::DriveApi::new(&cfg)
                    .and_then(|api| api.auth(method, params))
                    .map_err(|e| e.to_string());
                let _ = tx.send(result);
                ctx.request_repaint();
            });
        }
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
                        .color(pal().text),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new("You'll need to sign in again before the next sync.")
                        .size(12.5)
                        .color(pal().dim),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, None, "Log out", Btn::Danger, false, true).clicked() {
                            let result = neutronsync::driveapi::DriveApi::new(&self.cfg)
                                .and_then(|api| api.auth("auth.logout", serde_json::json!({})))
                                .map(|_| ());
                            self.ctrl.refresh_account(false);
                            self.confirm_logout = false;
                            match result {
                                Ok(()) => self.toast(ctx, "Logged out.", false),
                                Err(error) => self.toast(ctx, error.to_string(), true),
                            }
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
                        .color(pal().text),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(
                        "Removes all folders and wipes stored data (baselines, sync history). \
                         Your actual files are left untouched.",
                    )
                    .size(12.5)
                    .color(pal().dim),
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

fn daemon_banner(ui: &mut egui::Ui, kind: DaemonBanner) -> bool {
    let (col, title, action) = match kind {
        DaemonBanner::Restarting => (pal().danger, "Background sync stopped. Restarting…", None),
        DaemonBanner::NotResponding => (pal().warn, "Background sync is not responding", None),
        DaemonBanner::CouldNotStart => (
            pal().danger,
            "Background sync could not start",
            Some("Try again"),
        ),
    };
    activity_banner(ui, col, Icon::Info, title, "", false, action)
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
    action: Option<&str>,
) -> bool {
    let mut clicked = false;
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
                            .color(pal().text),
                    );
                    if !sub.is_empty() {
                        ui.add_space(1.0);
                        ui.label(RichText::new(sub).size(12.0).color(pal().dim));
                    }
                });
                if let Some(label) = action {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, None, label, Btn::Secondary, false, true).clicked() {
                            clicked = true;
                        }
                    });
                }
            });
        });
    });
    clicked
}

/// The answer chosen on a held-deletions banner.
enum HeldAction {
    None,
    Restore,
    Delete,
}

/// "They were removed on this computer", "in Proton Drive", or both.
fn held_where(hold: &HeldDeletes) -> &'static str {
    match (hold.remote > 0, hold.local > 0) {
        (true, true) => "on this computer and in Proton Drive",
        (true, false) => "on this computer",
        _ => "in Proton Drive",
    }
}

/// A WARN banner for one folder whose deletions were held back, with the two
/// answers: bring the files back, or delete them after all.
fn held_banner(ui: &mut egui::Ui, hold: &HeldDeletes, enabled: bool) -> HeldAction {
    let total = hold.local + hold.remote;
    let share = match (total * 100).checked_div(hold.tracked) {
        Some(pct) => format!(
            "That is {}% of the {} this folder keeps in sync.",
            pct.max(1),
            plural(hold.tracked, "file")
        ),
        None => String::new(),
    };
    let sub = format!(
        "{share} They were removed {}. Nothing was deleted.",
        held_where(hold)
    );
    let mut action = HeldAction::None;
    let col = pal().warn;
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
                draw_icon(ui.painter(), br.shrink(9.0), Icon::Trash, col);
                ui.add_space(12.0);
                // Leave room for the two buttons on the right; the sentence
                // wraps inside what is left instead of running under them.
                let text_width = (ui.available_width() - 270.0).max(160.0);
                ui.vertical(|ui| {
                    ui.set_max_width(text_width);
                    ui.add_space(1.0);
                    ui.label(
                        RichText::new(format!(
                            "{} held back in {}",
                            plural(total, "deletion"),
                            hold.pair
                        ))
                        .font(FontId::new(15.0, ff_bold()))
                        .color(pal().text),
                    );
                    ui.add_space(1.0);
                    ui.add(
                        egui::Label::new(RichText::new(sub.trim()).size(12.0).color(pal().dim))
                            .wrap(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(ui, None, "Delete them", Btn::Secondary, false, enabled).clicked() {
                        action = HeldAction::Delete;
                    }
                    if button(ui, None, "Restore them", Btn::Primary, false, enabled).clicked() {
                        action = HeldAction::Restore;
                    }
                });
            });
        });
    });
    action
}

/// One plain sentence per conflict outcome.
fn conflict_sentence(c: &ConflictRecord) -> String {
    match c.outcome {
        ConflictOutcome::KeptBoth => {
            let name = c
                .copy
                .as_deref()
                .map(|p| p.rsplit('/').next().unwrap_or(p).to_string())
                .unwrap_or_else(|| "a conflict copy".to_string());
            format!("Changed on both sides. Kept both; your version is saved as {name}.")
        }
        ConflictOutcome::KeptLocal => {
            "Changed on both sides. Kept the newer version from this computer.".to_string()
        }
        ConflictOutcome::KeptRemote => {
            "Changed on both sides. Kept the newer version from Proton Drive.".to_string()
        }
        ConflictOutcome::Skipped => {
            "Changed on both sides. Left as it is until you choose.".to_string()
        }
    }
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

        // Verify before anything is handed to a root process: the release's
        // signed checksum list (required from 0.7.0 on) and the checksum the
        // release API published. Any failure discards the file.
        *slot.lock().unwrap() = UpdateState::Verifying;
        ctx.request_repaint();
        let checked = updater::fetch_proof(&info)
            .and_then(|proof| updater::verify_signed(&downloaded, &asset, &info, &proof));
        if let Err(e) = checked {
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
/// Render a release body (Keep a Changelog Markdown as the release workflow
/// publishes it) with native widgets: version and section headings, bullets,
/// plain paragraphs. Inline code, bold and links are reduced to their text; the
/// "Full Changelog" compare line is dropped, the Open release button covers it.
fn release_notes(ui: &mut egui::Ui, notes: &str) {
    let mut first = true;
    for raw in notes.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() || line.starts_with("**Full Changelog**") {
            continue;
        }
        if let Some(version) = line.strip_prefix("## ") {
            if !first {
                ui.add_space(10.0);
            }
            ui.label(
                RichText::new(version.trim())
                    .font(FontId::new(14.0, ff_bold()))
                    .color(pal().text),
            );
            ui.add_space(2.0);
        } else if let Some(section) = line.strip_prefix("### ") {
            ui.add_space(4.0);
            ui.label(
                RichText::new(section.trim().to_uppercase())
                    .font(FontId::new(10.5, ff_bold()))
                    .color(pal().dim),
            );
        } else if let Some(item) = line.trim_start().strip_prefix("- ") {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new("•").size(12.5).color(pal().accent));
                ui.label(
                    RichText::new(plain_markdown(item))
                        .size(12.5)
                        .color(pal().text),
                );
            });
        } else {
            ui.label(
                RichText::new(plain_markdown(line.trim()))
                    .size(12.5)
                    .color(pal().dim),
            );
        }
        first = false;
    }
}

/// Strip the inline Markdown the changelog uses (`code`, **bold**, [text](url))
/// down to readable text.
fn plain_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find('[') {
        let (before, after) = rest.split_at(start);
        out.push_str(before);
        match after.find("](").and_then(|mid| {
            let end = after[mid..].find(')')? + mid;
            Some((mid, end))
        }) {
            Some((mid, end)) => {
                out.push_str(&after[1..mid]);
                rest = &after[end + 1..];
            }
            None => {
                out.push('[');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out.replace('`', "").replace("**", "")
}

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
    activity_cols(ui, &op.pair, pal().dim, "now", pal().accent, false, |ui| {
        ui.add_sized(
            [24.0, 24.0],
            egui::Spinner::new().size(16.0).color(pal().accent),
        );
        ui.add_space(8.0);
        ui.add(
            egui::Label::new(
                RichText::new(format!("{verb} {name}"))
                    .size(13.0)
                    .color(pal().text),
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
        (Icon::Arrow, pal().accent)
    } else if a.contains("upload") {
        (Icon::Upload, pal().accent)
    } else if a.contains("download") {
        (Icon::Download, pal().accent)
    } else if a.contains("delete") || a.contains("trash") {
        (Icon::Trash, pal().warn)
    } else {
        (Icon::Check, pal().ok)
    };
    let col = if op.ok { base } else { pal().danger };
    let name = op.path.rsplit('/').next().unwrap_or(&op.path).to_owned();
    let mut name_clicked = false;
    let folder_clicked = activity_cols(
        ui,
        &op.pair,
        pal().dim,
        &rel_time(ts),
        pal().dim2,
        true,
        |ui| {
            let (ir, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
            ui.painter().rect_filled(ir, 6.0, col.gamma_multiply(0.16));
            draw_icon(ui.painter(), ir.shrink(5.0), icon, col);
            ui.add_space(8.0);
            let r = ui.add(
                egui::Label::new(RichText::new(name).size(13.0).color(pal().text))
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
                ui.add(
                    egui::Label::new(RichText::new(err).size(11.5).color(pal().danger)).truncate(),
                )
                .on_hover_text(tip);
            }
        },
    );
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
    let first_run = ps.is_some_and(|p| p.last_synced.is_none());
    let (col, mut text) = match ps.map(|p| p.phase) {
        Some(Phase::Scanning) => (
            pal().accent,
            if first_run {
                "Initializing…"
            } else {
                "Scanning…"
            }
            .to_string(),
        ),
        Some(Phase::Syncing) => (
            pal().accent,
            match ps.map(|p| p.progress) {
                Some(pr) if pr.total > 0 && !first_run => {
                    format!(
                        "Syncing · {} of {}",
                        thousands(pr.done),
                        thousands(pr.total)
                    )
                }
                _ if first_run => "Initializing…".to_string(),
                _ => "Syncing…".to_string(),
            },
        ),
        Some(Phase::Error) => (
            pal().danger,
            ps.and_then(|p| p.last_error.clone())
                .unwrap_or_else(|| "Error".into()),
        ),
        Some(Phase::Synced) | Some(Phase::Idle) | None => {
            if watching {
                (pal().accent, "Auto · watching".to_string())
            } else {
                match ps.and_then(|p| p.last_synced) {
                    Some(ts) => (pal().ok, format!("Synced · {}", rel_time(ts))),
                    None => (pal().dim, "Not synced yet".to_string()),
                }
            }
        }
    };
    // how much this folder holds, once a sync has counted it
    if let Some(n) = ps.map(|p| p.tracked).filter(|n| *n > 0) {
        if !matches!(ps.map(|p| p.phase), Some(Phase::Error)) {
            text.push_str(&format!(" · {}", plural(n, "file")));
        }
    }
    ui.horizontal(|ui| {
        status_dot_small(ui, col);
        ui.label(RichText::new(text).size(12.0).color(pal().dim));
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
    painter.rect_filled(rect, 7.0, pal().nav_bg);
    let ir = Rect::from_center_size(
        pos2(rect.left() + pad + icon_w / 2.0, rect.center().y),
        vec2(icon_w, icon_w),
    );
    draw_icon(painter, ir, icon, pal().dim);
    painter.text(
        pos2(rect.left() + pad + icon_w + gap, rect.center().y),
        Align2::LEFT_CENTER,
        &shown,
        font,
        pal().text,
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
        draw_icon(ui.painter(), ir, Icon::Cloud, pal().dim2);
        ui.add_space(10.0);
        ui.label(
            RichText::new(title)
                .font(FontId::new(15.0, ff_bold()))
                .color(pal().dim),
        );
        ui.label(RichText::new(sub).size(12.5).color(pal().dim2));
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
                    .fill(pal().sel)
                    .stroke(Stroke::new(1.0, pal().line()))
                    .corner_radius(10.0)
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 16,
                        top: 10,
                        bottom: 10,
                    });
                frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        status_dot_small(ui, if err { pal().danger } else { pal().ok });
                        ui.label(RichText::new(msg).size(13.0).color(pal().text));
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
            ui.label(RichText::new(&self.browser_path).color(pal().dim));
        });
        ui.separator();
        if self.browser_loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("Loading…").color(pal().dim));
            });
            return;
        }
        if let Some(err) = self.browser_error.clone() {
            ui.label(RichText::new(err).color(pal().danger));
            return;
        }
        let mut open_into: Option<String> = None;
        let mut select: Option<String> = None;
        egui::ScrollArea::vertical()
            .max_height(300.0)
            .show(ui, |ui| {
                if self.browser_entries.is_empty() {
                    ui.label(RichText::new("(no sub-folders here)").color(pal().dim));
                }
                for e in &self.browser_entries {
                    let full = config::remote_join(&self.browser_path, &e.path);
                    ui.horizontal(|ui| {
                        let (ir, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
                        draw_icon(ui.painter(), ir, Icon::Folder, pal().accent);
                        if ui.link(RichText::new(&e.path).color(pal().text)).clicked() {
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
                    .color(pal().dim2),
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
            .color(pal().accent),
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
            ui.label(RichText::new(title).size(13.5).color(pal().text));
            if !desc.is_empty() {
                ui.add(egui::Label::new(
                    RichText::new(desc).size(11.5).color(pal().dim2),
                ));
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
fn combo_theme(ui: &mut egui::Ui, v: &mut Theme) -> bool {
    let text = match v {
        Theme::System => "System",
        Theme::Light => "Light",
        Theme::Dark => "Dark",
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt("theme")
        .selected_text(text)
        .show_ui(ui, |ui| {
            for (val, lbl) in [
                (Theme::System, "System"),
                (Theme::Light, "Light"),
                (Theme::Dark, "Dark"),
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

fn plain_signin_error(error: String) -> String {
    error
        .replace("sign in with the sidecar", "sign in again")
        .replace("api backend: ", "")
}

fn combo_full_walk(ui: &mut egui::Ui, v: &mut u64) -> bool {
    const CHOICES: [(u64, &str); 4] = [
        (21600, "6 h"),
        (43200, "12 h"),
        (86400, "1 day"),
        (604800, "7 days"),
    ];
    let text = CHOICES
        .iter()
        .find(|(secs, _)| *secs == *v)
        .map(|(_, label)| *label)
        .unwrap_or("Custom");
    let display = if text == "Custom" {
        format!("{v} s")
    } else {
        text.to_string()
    };
    let mut changed = false;
    egui::ComboBox::from_id_salt("full_walk_interval")
        .selected_text(display)
        .show_ui(ui, |ui| {
            for (secs, label) in CHOICES {
                changed |= ui.selectable_value(v, secs, label).clicked();
            }
        });
    changed
}

/// A speed limit in KB/s as the Settings page shows it: "No limit", a preset,
/// or the configured value spelled out.
fn speed_limit_label(kbps: u64) -> String {
    if kbps == 0 {
        "No limit".to_string()
    } else if kbps.is_multiple_of(1024) {
        format!("{} MB/s", kbps / 1024)
    } else {
        format!("{kbps} KB/s")
    }
}

/// Upload or download limit. A value from the config file that is not a
/// preset is listed as its own entry and kept until the user picks another.
fn combo_speed_limit(ui: &mut egui::Ui, id: &str, v: &mut u64) -> bool {
    const PRESETS: [u64; 7] = [0, 256, 512, 1024, 2048, 5120, 10240];
    let mut choices: Vec<u64> = PRESETS.to_vec();
    if !choices.contains(v) {
        let pos = choices
            .iter()
            .position(|c| *c > *v)
            .unwrap_or(choices.len());
        choices.insert(pos, *v);
    }
    let mut changed = false;
    egui::ComboBox::from_id_salt(id)
        .selected_text(speed_limit_label(*v))
        .height(320.0)
        .show_ui(ui, |ui| {
            for kbps in choices {
                changed |= ui
                    .selectable_value(v, kbps, speed_limit_label(kbps))
                    .clicked();
            }
        });
    changed
}

// -- starter config ----------------------------------------------------------
fn starter_config(path: &Path) -> Config {
    Config {
        sidecar: None,
        scan_threads: 0,
        download_threads: 0,
        remote_root: config::DEFAULT_REMOTE_ROOT.into(),
        propagate_deletes: true,
        max_delete_files: 200,
        max_delete_percent: 30,
        auto_sync: false,
        run_in_tray: false,
        x11_compat: false,
        local_delete: LocalDelete::Trash,
        conflict: ConflictPolicy::KeepBoth,
        compare: Compare::SizeMtime,
        full_walk_interval: 86400,
        scan_interval_secs: 120,
        debounce_secs: 2,
        update_channel: UpdateChannel::Stable,
        check_on_launch: false,
        upload_limit_kbps: 0,
        download_limit_kbps: 0,
        notifications: true,
        theme: Theme::System,
        file_manager_bookmarks: false,
        session_store: Default::default(),
        session_key_file: None,
        state_dir: {
            let base = std::env::var("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state")
                });
            base.join("neutronsync")
        },
        pairs: vec![],
        source_path: Some(path.to_path_buf()),
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

static SUPERVISOR_STOP: AtomicBool = AtomicBool::new(false);
static SUPERVISOR_CHILD: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_supervisor_signal(sig: libc::c_int) {
    SUPERVISOR_STOP.store(true, Ordering::SeqCst);
    let pid = SUPERVISOR_CHILD.load(Ordering::SeqCst);
    if pid > 0 {
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

fn install_supervisor_signals() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        let handler: extern "C" fn(libc::c_int) = on_supervisor_signal;
        action.sa_sigaction = handler as *const () as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// `--tray`: hold the tray lock and run `--tray-child`, restarting it after a crash.
/// Does no GTK, sync, or config watching, so a broken display pipe cannot take it down.
fn run_tray_supervisor() {
    let cfg = match config::load(None) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("neutronsync: no config found; nothing to run in tray mode");
            return;
        }
    };
    let _lock = match PidLock::acquire(&cfg.state_dir, DAEMON_LOCK) {
        Some(lock) => lock,
        None => {
            eprintln!("neutronsync: a tray daemon is already running");
            return;
        }
    };
    install_supervisor_signals();
    let log = neutronsync::logger::Logger::new(&cfg.state_dir, false, true);
    let mut restarts: Vec<Instant> = Vec::new();
    loop {
        if SUPERVISOR_STOP.load(Ordering::SeqCst) {
            break;
        }
        if !display_session_present() {
            log.warn("neutronsync: background sync stopped because the display session is gone");
            break;
        }
        let mut child = match spawn_tray_child() {
            Ok(child) => child,
            Err(err) => {
                let line =
                    format!("neutronsync: background sync restarted (child spawn failed: {err})");
                if !continue_after_child(&log, &mut restarts, ChildEnd::Code(1), &line) {
                    break;
                }
                continue;
            }
        };
        let end = wait_tray_child(&mut child);
        if SUPERVISOR_STOP.load(Ordering::SeqCst) {
            break;
        }
        let line = format!(
            "neutronsync: background sync restarted (child {})",
            end.describe()
        );
        if !continue_after_child(&log, &mut restarts, end, &line) {
            break;
        }
    }
}

/// Apply the restart policy. Returns true when the supervisor should spawn again.
fn continue_after_child(
    log: &neutronsync::logger::Logger,
    restarts: &mut Vec<Instant>,
    end: ChildEnd,
    restart_line: &str,
) -> bool {
    let now = Instant::now();
    let window = Duration::from_secs(5 * 60);
    restarts.retain(|at| now.saturating_duration_since(*at) < window);
    match tray_policy::supervisor_action(end, display_session_present(), restarts.len() as u32) {
        SupervisorAction::StopClean => false,
        SupervisorAction::StopSessionGone => {
            log.warn("neutronsync: background sync stopped because the display session is gone");
            false
        }
        SupervisorAction::StopRestartCap => {
            log.warn("neutronsync: background sync stopped after 5 restarts in 5 minutes");
            false
        }
        SupervisorAction::Restart { delay_secs } => {
            log.warn(restart_line);
            restarts.push(now);
            !sleep_unless_stopped(Duration::from_secs(delay_secs))
        }
    }
}

fn sleep_unless_stopped(total: Duration) -> bool {
    let step = Duration::from_millis(100);
    let start = Instant::now();
    while start.elapsed() < total {
        if SUPERVISOR_STOP.load(Ordering::SeqCst) {
            return true;
        }
        let left = total.saturating_sub(start.elapsed());
        thread::sleep(step.min(left));
    }
    SUPERVISOR_STOP.load(Ordering::SeqCst)
}

fn spawn_tray_child() -> std::io::Result<std::process::Child> {
    let parent = std::process::id() as libc::pid_t;
    let mut cmd = std::process::Command::new(self_exe());
    cmd.arg("--tray-child");
    inherit_session_env(&mut cmd);
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                libc::raise(libc::SIGTERM);
                libc::_exit(128 + libc::SIGTERM);
            }
            Ok(())
        });
    }
    cmd.spawn()
}

fn wait_tray_child(child: &mut std::process::Child) -> ChildEnd {
    let pid = child.id() as libc::pid_t;
    SUPERVISOR_CHILD.store(pid, Ordering::SeqCst);
    if SUPERVISOR_STOP.load(Ordering::SeqCst) {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    let status = child.wait();
    SUPERVISOR_CHILD.store(0, Ordering::SeqCst);
    match status {
        Ok(status) => {
            if let Some(code) = status.code() {
                ChildEnd::Code(code)
            } else if let Some(signal) = status.signal() {
                ChildEnd::Signal(signal)
            } else {
                ChildEnd::Code(1)
            }
        }
        Err(_) => ChildEnd::Code(1),
    }
}

/// Headless tray child (`--tray-child`): owns the system tray (GTK/AppIndicator)
/// and an optional watcher, with no window. The supervisor (`--tray`) holds
/// `tray-daemon.lock` and restarts this process; this child takes no lock.
/// It's the Wayland-native background presence — the window can't hide/restore/focus
/// itself on Wayland, so instead the tray simply opens a fresh window on demand.
/// Runs a GTK main loop until the tray "Quit" item is chosen or the process is killed.
/// Exit code 0 (Quit, or there is no config) tells the supervisor to stop.
/// GTK init failure, a missing tray icon, and the GTK loop ending without Quit
/// return 1 so the supervisor restarts.
fn run_daemon() -> i32 {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::TrayIconBuilder;

    let cfg = match config::load(None) {
        Ok(c) => c,
        Err(_) => {
            eprintln!("neutronsync: no config found; nothing to run in tray mode");
            return 0;
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
        ctrl.stop_watch();
        return 1;
    }

    // The first line is the status, which cannot be clicked.
    let first = ctrl.snapshot();
    let mut last_state = tray_policy::tray_state(&first);
    let mut last_status = tray_policy::tray_status_text(&first, now_secs());
    let menu = Menu::new();
    let status_i = MenuItem::new(&last_status, false, None);
    let open_i = MenuItem::new("Open NeutronSync", true, None);
    let sync_i = MenuItem::new("Sync now", true, None);
    let quit_i = MenuItem::new("Quit", true, None);
    let _ = menu.append(&status_i);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&open_i);
    let _ = menu.append(&sync_i);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit_i);
    let (open_id, sync_id, quit_id) = (
        open_i.id().clone(),
        sync_i.id().clone(),
        quit_i.id().clone(),
    );

    let icons = TrayIcons::build();
    let mut builder = TrayIconBuilder::new()
        .with_tooltip(format!("NeutronSync — {last_status}"))
        .with_menu(Box::new(menu));
    if let Some(icons) = &icons {
        builder = builder.with_icon(icons.for_state(last_state).clone());
    }
    let tray = match builder.build() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("neutronsync: system tray unavailable: {e}");
            ctrl.stop_watch();
            return 1;
        }
    };
    let quit = std::sync::Arc::new(AtomicBool::new(false));
    // `NEUTRONSYNC_TRAY_TRACE=1` prints every icon, tooltip and status change
    // to stderr, which is how the "only on change" rule is checked.
    let trace = std::env::var_os("NEUTRONSYNC_TRAY_TRACE").is_some();
    if trace {
        eprintln!("neutronsync: tray starts as {last_state:?} with text {last_status:?}");
    }
    let mut notify_on = cfg.notifications;
    let mut notifier = Notifier::new(&first);

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
    let quit_flag = quit.clone();
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
                quit_flag.store(true, Ordering::SeqCst);
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
                notify_on = newcfg.notifications;
                reconcile_bookmarks(&newcfg);
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

        // Icon, tooltip and status line follow the state, and are set only
        // when something changed, never on every tick.
        let snap = ctrl_cb.snapshot();
        let state = tray_policy::tray_state(&snap);
        if state != last_state {
            last_state = state;
            if let Some(icons) = &icons {
                let _ = tray.set_icon(Some(icons.for_state(state).clone()));
            }
            if trace {
                eprintln!("neutronsync: tray icon set to {state:?}");
            }
        }
        let status = tray_policy::tray_status_text(&snap, now_secs());
        if status != last_status {
            status_i.set_text(&status);
            let _ = tray.set_tooltip(Some(format!("NeutronSync — {status}")));
            if trace {
                eprintln!("neutronsync: tray text set to {status:?}");
            }
            last_status = status;
        }

        // Desktop notices come from this process alone (D1).
        notifier.tick(&ctrl_cb, &snap, notify_on);

        // Publish our live state so an open window can display what the daemon
        // is doing (scanning, syncing, per-file ops) — the window has no other
        // view into this separate process.
        ctrl_cb.publish_status();

        gtk::glib::ControlFlow::Continue
    });

    gtk::main();
    ctrl.stop_watch();
    if quit.load(Ordering::SeqCst) {
        0
    } else {
        1
    }
}

// ============================================================================
// --screenshots: scripted scenes with made-up data, one PNG each
// ============================================================================

/// One documentation capture: which page, with which state and palette.
struct Scene {
    file: &'static str,
    nav: Nav,
    state: fn() -> AppState,
    /// `Theme::Light` for the light captures; `Theme::System` otherwise, which
    /// falls back to dark under the headless X server.
    theme: Theme,
    /// Held deletions and settled conflicts, which the window reads from
    /// stats.db rather than from `AppState`.
    records: fn() -> (Vec<HeldDeletes>, Vec<ConflictRecord>),
}

fn no_records() -> (Vec<HeldDeletes>, Vec<ConflictRecord>) {
    (Vec::new(), Vec::new())
}

/// Two conflicts the sync settled in the last day, both still listed.
fn demo_conflicts() -> Vec<ConflictRecord> {
    let now = now_secs();
    let record = |id, pair: &str, path: &str, outcome, copy: Option<&str>, ago| ConflictRecord {
        id,
        pair: pair.into(),
        path: path.into(),
        outcome,
        copy: copy.map(Into::into),
        first_ts: now - ago,
        last_ts: now - ago,
        seen: 1,
        dismissed: false,
    };
    vec![
        record(
            1,
            "Documents",
            "Finance/2026/Budget.xlsx",
            ConflictOutcome::KeptBoth,
            Some("Finance/2026/Budget (conflict 2026-10-19 0912).xlsx"),
            40 * 60,
        ),
        record(
            2,
            "Work",
            "Reports/Q3 review.docx",
            ConflictOutcome::KeptLocal,
            None,
            5 * 3600,
        ),
    ]
}

fn demo_records_conflicts() -> (Vec<HeldDeletes>, Vec<ConflictRecord>) {
    (Vec::new(), demo_conflicts())
}

/// The Photos folder was emptied on this computer; the sync held it back.
fn demo_records_held() -> (Vec<HeldDeletes>, Vec<ConflictRecord>) {
    let now = now_secs();
    let hold = HeldDeletes {
        pair: "Photos".into(),
        local: 0,
        remote: 1640,
        tracked: 1712,
        since: now - 20 * 60,
        updated: now - 2 * 60,
    };
    (vec![hold], Vec::new())
}

struct Demo {
    dir: PathBuf,
    scenes: Vec<Scene>,
    index: usize,
    /// Wall-clock time the current scene was switched in; captures wait for
    /// the page fade and nav indicator to finish before asking for pixels.
    since: Option<f64>,
    requested: bool,
    /// Keeps the scratch state dir alive for the run.
    _scratch: tempfile::TempDir,
}

const DEMO_HOME: &str = "/home/alex";

fn demo_pairs() -> Vec<Pair> {
    let pair = |name: &str, local: &str| Pair {
        name: name.into(),
        local: PathBuf::from(format!("{DEMO_HOME}/{local}")),
        remote: format!("/my-files/{name}"),
        auto: true,
        exclude: Vec::new(),
    };
    let mut docs = pair("Documents", "Documents");
    docs.exclude = vec!["Archive/2019".into(), "Scans/raw".into()];
    let mut photos = pair("Photos", "Pictures");
    photos.auto = true;
    let mut work = pair("Work", "Work");
    work.auto = false;
    vec![docs, photos, work]
}

fn demo_pair_state(
    pair: &Pair,
    phase: Phase,
    last_synced: Option<i64>,
    tracked: usize,
) -> PairState {
    PairState {
        name: pair.name.clone(),
        local: pair.local.display().to_string(),
        remote: pair.remote.clone(),
        phase,
        progress: neutronsync::service::Progress::default(),
        scanned_folders: 0,
        current_op: None,
        last_error: None,
        last_synced,
        tracked,
    }
}

fn demo_op(
    ts: i64,
    action: &str,
    pair: &str,
    path: &str,
    error: Option<&str>,
) -> neutronsync::service::ActivityItem {
    let op = ActivityOp {
        action: action.into(),
        path: path.into(),
        pair: pair.into(),
        ok: error.is_none(),
        error: error.map(String::from),
    };
    neutronsync::service::ActivityItem {
        ts,
        kind: if op.ok {
            ActivityKind::Sync
        } else {
            ActivityKind::Error
        },
        text: format!("{action} {path}"),
        op: Some(op),
    }
}

/// Signed in, three folders, a recent history of file movements.
fn demo_state_synced() -> AppState {
    let now = now_secs();
    let pairs = demo_pairs();
    let mut s = AppState {
        pairs: vec![
            demo_pair_state(&pairs[0], Phase::Synced, Some(now - 95), 1_284),
            demo_pair_state(&pairs[1], Phase::Synced, Some(now - 1_900), 6_410),
            demo_pair_state(&pairs[2], Phase::Synced, Some(now - 4 * 3600), 312),
        ],
        account: neutronsync::service::AccountState {
            checked: true,
            backend_ready: true,
            signed_in: true,
            keyring_locked: false,
            account: "alex@proton.me".into(),
            version: "/usr/bin/neutronsync-drive".into(),
            checking: false,
        },
        watching: true,
        ..Default::default()
    };
    let history: &[(i64, &str, &str, &str, Option<&str>)] = &[
        (
            95,
            "upload",
            "Documents",
            "Finance/2026/Invoices/2026-10 Studio rent.pdf",
            None,
        ),
        (
            140,
            "download",
            "Documents",
            "Reading/Papers/Attention is all you need.pdf",
            None,
        ),
        (
            310,
            "mkdir-remote",
            "Documents",
            "Finance/2026/Receipts",
            None,
        ),
        (
            330,
            "upload",
            "Documents",
            "Finance/2026/Receipts/IMG_2210.jpg",
            None,
        ),
        (
            1_900,
            "download",
            "Photos",
            "2026-09 Cape Town/DSC04471.ARW",
            None,
        ),
        (
            1_930,
            "download",
            "Photos",
            "2026-09 Cape Town/DSC04472.ARW",
            None,
        ),
        (
            2_100,
            "upload",
            "Photos",
            "2026-10 Garden/IMG_0412.HEIC",
            Some("api backend: rate_limited"),
        ),
        (
            2_600,
            "delete-remote",
            "Photos",
            "2026-09 Cape Town/duplicates/DSC04471 (1).ARW",
            None,
        ),
        (
            4 * 3600,
            "upload",
            "Work",
            "Proposals/Nkosi Architects/Proposal v3.docx",
            None,
        ),
        (
            4 * 3600 + 20,
            "rename-remote",
            "Work",
            "Proposals/Nkosi Architects/Proposal v3.docx",
            None,
        ),
        (
            6 * 3600,
            "download",
            "Work",
            "Clients/Shared/Brand guide 2026.pdf",
            None,
        ),
        (
            26 * 3600,
            "upload",
            "Documents",
            "Personal/Insurance/Policy schedule 2026.pdf",
            None,
        ),
        (
            27 * 3600,
            "upload",
            "Documents",
            "Personal/Insurance/Claim form.pdf",
            None,
        ),
    ];
    for (ago, action, pair, path, err) in history.iter().rev() {
        s.activity
            .push_back(demo_op(now - ago, action, pair, path, *err));
    }
    s
}

/// Same account, but a sync is running on the Photos folder.
fn demo_state_syncing() -> AppState {
    let mut s = demo_state_synced();
    s.busy = true;
    s.pairs[1].phase = Phase::Syncing;
    s.pairs[1].progress = neutronsync::service::Progress {
        done: 37,
        total: 120,
    };
    s.pairs[1].current_op = Some("upload 2026-10 Garden/IMG_0413.HEIC".into());
    s.active = vec![
        ActivityOp {
            action: "upload".into(),
            path: "2026-10 Garden/IMG_0413.HEIC".into(),
            pair: "Photos".into(),
            ok: true,
            error: None,
        },
        ActivityOp {
            action: "upload".into(),
            path: "2026-10 Garden/IMG_0414.HEIC".into(),
            pair: "Photos".into(),
            ok: true,
            error: None,
        },
        ActivityOp {
            action: "download".into(),
            path: "Finance/2026/Statements/October.pdf".into(),
            pair: "Documents".into(),
            ok: true,
            error: None,
        },
    ];
    s
}

/// The sidecar answered, but there is no session: the sign-in page shows.
fn demo_state_signed_out() -> AppState {
    let mut s = demo_state_synced();
    s.account.signed_in = false;
    s.account.account.clear();
    s.watching = false;
    s.signed_out = true;
    s
}

/// The keyring is locked; the session is still in it.
fn demo_state_keyring_locked() -> AppState {
    let mut s = demo_state_signed_out();
    s.signed_out = false;
    s.account.keyring_locked = true;
    s
}

fn demo_scenes() -> Vec<Scene> {
    vec![
        Scene {
            file: "activity-syncing.png",
            nav: Nav::Activity,
            state: demo_state_syncing,
            theme: Theme::System,
            records: no_records,
        },
        Scene {
            file: "activity.png",
            nav: Nav::Activity,
            state: demo_state_synced,
            theme: Theme::System,
            records: no_records,
        },
        Scene {
            file: "folders.png",
            nav: Nav::Folders,
            state: demo_state_syncing,
            theme: Theme::System,
            records: no_records,
        },
        Scene {
            file: "settings.png",
            nav: Nav::Settings,
            state: demo_state_synced,
            theme: Theme::System,
            records: no_records,
        },
        Scene {
            file: "account.png",
            nav: Nav::Account,
            state: demo_state_synced,
            theme: Theme::System,
            records: no_records,
        },
        Scene {
            file: "sign-in.png",
            nav: Nav::Activity,
            state: demo_state_signed_out,
            theme: Theme::System,
            records: no_records,
        },
        Scene {
            file: "keyring-locked.png",
            nav: Nav::Activity,
            state: demo_state_keyring_locked,
            theme: Theme::System,
            records: no_records,
        },
        Scene {
            file: "about.png",
            nav: Nav::About,
            state: demo_state_synced,
            theme: Theme::System,
            records: no_records,
        },
        Scene {
            file: "conflicts.png",
            nav: Nav::Conflicts,
            state: demo_state_synced,
            theme: Theme::System,
            records: demo_records_conflicts,
        },
        Scene {
            file: "held.png",
            nav: Nav::Activity,
            state: demo_state_synced,
            theme: Theme::System,
            records: demo_records_held,
        },
        Scene {
            file: "activity-light.png",
            nav: Nav::Activity,
            state: demo_state_synced,
            theme: Theme::Light,
            records: no_records,
        },
        Scene {
            file: "settings-light.png",
            nav: Nav::Settings,
            state: demo_state_synced,
            theme: Theme::Light,
            records: no_records,
        },
    ]
}

impl App {
    /// A window wired to scripted data instead of the user's config. Writes
    /// nothing under $HOME: config and state live in a scratch directory.
    fn new_demo(cc: &eframe::CreationContext<'_>, dir: PathBuf) -> Self {
        install_theme(&cc.egui_ctx);
        cc.egui_ctx.set_zoom_factor(2.0);
        let scratch = tempfile::tempdir().expect("scratch dir for --screenshots");
        let config_path = scratch.path().join("neutronsync.toml");
        let mut cfg = starter_config(&config_path);
        cfg.state_dir = scratch.path().join("state");
        // Never reach the user's running sidecar from a documentation run.
        cfg.sidecar = Some(scratch.path().join("no-sidecar"));
        cfg.pairs = demo_pairs();
        cfg.auto_sync = true;
        let ctrl = Controller::new(cfg.clone());
        let notifier = Notifier::new(&ctrl.snapshot());
        let _ = std::fs::create_dir_all(&dir);
        App {
            ctrl,
            cfg,
            config_path,
            config_loaded: true,
            nav: Nav::Activity,
            nav_t: 1.0,
            dirty: false,
            toast: None,
            update: std::sync::Arc::new(std::sync::Mutex::new(UpdateState::Idle)),
            confirm_logout: false,
            confirm_reset: false,
            last_account_poll: 0.0,
            api_signin: ApiSignIn::default(),
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
            mode_daemon: false,
            daemon_watch: DaemonWatch::new(),
            _win_lock: None,
            holds: Vec::new(),
            conflicts: Vec::new(),
            last_state_poll: -1.0,
            confirm_delete_held: None,
            demo: Some(Demo {
                dir,
                scenes: demo_scenes(),
                index: 0,
                since: None,
                requested: false,
                _scratch: scratch,
            }),
            notifier,
        }
    }

    /// Advance the scripted run by one frame. Returns the state to render, or
    /// None when this is not a `--screenshots` run.
    fn demo_frame(&mut self, ctx: &egui::Context) -> Option<AppState> {
        let demo = self.demo.as_mut()?;
        // A capture from the previous frame lands as an input event.
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = shot {
            if let Some(scene) = demo.scenes.get(demo.index) {
                let [w, h] = image.size;
                let path = demo.dir.join(scene.file);
                let saved =
                    ::image::RgbaImage::from_raw(w as u32, h as u32, image.as_raw().to_vec())
                        .ok_or_else(|| "pixel buffer size mismatch".to_string())
                        .and_then(|img| img.save(&path).map_err(|e| e.to_string()));
                match saved {
                    Ok(()) => eprintln!("wrote {}", path.display()),
                    Err(e) => eprintln!("could not write {}: {e}", path.display()),
                }
            }
            demo.index += 1;
            demo.since = None;
            demo.requested = false;
        }
        let Some(scene) = demo.scenes.get(demo.index) else {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return Some(AppState::default());
        };
        let now = ctx.input(|i| i.time);
        let nav = scene.nav;
        let theme = scene.theme;
        let records = scene.records;
        let state = (scene.state)();
        if demo.since.is_none() {
            demo.since = Some(now);
            self.nav = nav;
            self.nav_t = 1.0;
            self.cfg.theme = theme;
            self.sync_theme(ctx);
            (self.holds, self.conflicts) = records();
        } else if !demo.requested && now - demo.since.unwrap_or(now) > 0.9 {
            demo.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
        Some(state)
    }
}

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Headless tray. `--tray` is the supervisor (lock + restarts). `--tray-child`
    // is the real tray and is not a user-facing command.
    if args.iter().any(|a| a == "--tray-child") {
        std::process::exit(run_daemon());
    }
    if args.iter().any(|a| a == "--tray") {
        run_tray_supervisor();
        return Ok(());
    }

    // Documentation captures: `neutronsync-gui --screenshots docs/screenshots`.
    if let Some(pos) = args.iter().position(|a| a == "--screenshots") {
        let dir = PathBuf::from(
            args.get(pos + 1)
                .cloned()
                .unwrap_or_else(|| "screenshots".into()),
        );
        // Twice the logical size, rendered at zoom 2.0, for crisp images.
        let viewport = egui::ViewportBuilder::default()
            .with_inner_size([2360.0, 1480.0])
            .with_decorations(false)
            .with_title("NeutronSync for Linux")
            .with_app_id("neutronsync-screenshots");
        let options = eframe::NativeOptions {
            viewport,
            ..Default::default()
        };
        return eframe::run_native(
            "neutronsync-screenshots",
            options,
            Box::new(move |cc| Ok(Box::new(App::new_demo(cc, dir)))),
        );
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

#[cfg(test)]
mod tests {
    use super::{paint_state_dot, plain_markdown, speed_limit_label, Color32, DARK};

    #[test]
    fn state_dot_sits_in_the_lower_right_quarter_with_a_dark_outline() {
        let (w, h) = (64u32, 64u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        paint_state_dot(&mut rgba, w, h, DARK.accent);
        let px = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
        };
        // Dot centre: the accent colour, fully opaque.
        let c = px((w as f32 * 0.76) as u32, (h as f32 * 0.76) as u32);
        assert_eq!(&c[..3], &[0x6D, 0x4A, 0xFF]);
        assert_eq!(c[3], 255);
        // Just outside the fill but inside the outline: dark and opaque.
        let ring = px(
            (w as f32 * 0.76 + w as f32 * 0.19) as u32,
            (h as f32 * 0.76) as u32,
        );
        assert_eq!(ring[3], 255);
        assert!(
            ring[0] < 0x30 && ring[1] < 0x30 && ring[2] < 0x30,
            "{ring:?}"
        );
        // The upper left of the image is untouched.
        assert_eq!(px(8, 8), [0, 0, 0, 0]);
        assert_eq!(px(w / 2, h / 4), [0, 0, 0, 0]);
    }

    #[test]
    fn owned_bookmarks_read_both_record_shapes() {
        let new = br#"[{"path":"/home/alex/Documents","label":"Documents"}]"#;
        let got = super::read_owned_bookmarks(new).unwrap();
        assert_eq!(
            got[0].path,
            std::path::PathBuf::from("/home/alex/Documents")
        );
        assert_eq!(got[0].label.as_deref(), Some("Documents"));
        let old = br#"["/home/alex/Documents"]"#;
        let got = super::read_owned_bookmarks(old).unwrap();
        assert_eq!(
            got[0].path,
            std::path::PathBuf::from("/home/alex/Documents")
        );
        assert_eq!(got[0].label, None);
        assert!(super::read_owned_bookmarks(b"not json").is_none());
    }

    #[test]
    fn blending_over_an_opaque_pixel_keeps_it_opaque() {
        let mut px = [200u8, 200, 200, 255];
        super::blend_over(&mut px, Color32::from_rgb(0, 0, 0), 0.5);
        assert_eq!(px[3], 255);
        assert_eq!(px[0], 100);
    }

    #[test]
    fn speed_limit_labels_are_plain_words() {
        assert_eq!(speed_limit_label(0), "No limit");
        assert_eq!(speed_limit_label(256), "256 KB/s");
        assert_eq!(speed_limit_label(750), "750 KB/s");
        assert_eq!(speed_limit_label(1024), "1 MB/s");
        assert_eq!(speed_limit_label(10240), "10 MB/s");
    }

    #[test]
    fn plain_markdown_strips_inline_marks() {
        assert_eq!(
            plain_markdown("A config with `[cli] backend = \"cli\"` is **refused**."),
            "A config with [cli] backend = \"cli\" is refused."
        );
        assert_eq!(
            plain_markdown("See [docs/SYNC_MODEL.md](docs/SYNC_MODEL.md) and [x]"),
            "See docs/SYNC_MODEL.md and [x]"
        );
        assert_eq!(plain_markdown("plain"), "plain");
    }
}
