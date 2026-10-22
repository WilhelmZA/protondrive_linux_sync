//! Restart, window-liveness and status decisions for the headless tray.
//!
//! These functions do not spawn processes, read the display sockets or touch
//! GTK. The supervisor, the tray tick and the window supply the facts and
//! apply the decision.

use std::path::PathBuf;

use crate::datefmt::relative_time;
use crate::service::{AppState, Phase};

/// What the tray icon shows. One state at a time, in this priority order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayState {
    /// Signed out, or the keyring is locked: sync is waiting on the user.
    Attention,
    /// A folder's last sync had a problem.
    Error,
    /// A folder is scanning or syncing, or a run is in flight.
    Syncing,
    /// Nothing running, nothing wrong.
    Idle,
}

/// The icon state for a snapshot: Attention beats Error beats Syncing beats Idle.
pub fn tray_state(state: &AppState) -> TrayState {
    if state.signed_out || state.account.keyring_locked {
        return TrayState::Attention;
    }
    if state.pairs.iter().any(|p| p.phase == Phase::Error) {
        return TrayState::Error;
    }
    if state.busy
        || state
            .pairs
            .iter()
            .any(|p| matches!(p.phase, Phase::Scanning | Phase::Syncing))
    {
        return TrayState::Syncing;
    }
    TrayState::Idle
}

/// The one-line status in the tray menu (and after "NeutronSync — " in the
/// tooltip). `now` is epoch seconds, for the "last sync" wording.
pub fn tray_status_text(state: &AppState, now: i64) -> String {
    match tray_state(state) {
        TrayState::Attention => {
            if state.signed_out {
                "Signed out — open NeutronSync to sign in".to_string()
            } else {
                "Waiting for the keyring to unlock".to_string()
            }
        }
        TrayState::Error => {
            let broken: Vec<&str> = state
                .pairs
                .iter()
                .filter(|p| p.phase == Phase::Error)
                .map(|p| p.name.as_str())
                .collect();
            if broken.len() == 1 {
                format!("Problem in {}", broken[0])
            } else {
                format!("Problems in {} folders", broken.len())
            }
        }
        TrayState::Syncing => {
            let (done, total) = state
                .pairs
                .iter()
                .filter(|p| matches!(p.phase, Phase::Scanning | Phase::Syncing))
                .fold((0usize, 0usize), |(d, t), p| {
                    (d + p.progress.done, t + p.progress.total)
                });
            if total > 0 {
                format!("Syncing {} of {total}", done.min(total))
            } else {
                "Syncing…".to_string()
            }
        }
        TrayState::Idle => match state.pairs.iter().filter_map(|p| p.last_synced).max() {
            Some(ts) => format!("Up to date · last sync {}", relative_time(now, ts)),
            None => "Up to date".to_string(),
        },
    }
}

/// How a tray child stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildEnd {
    Code(i32),
    Signal(i32),
}

impl ChildEnd {
    /// Short text for the restart log line.
    pub fn describe(self) -> String {
        match self {
            ChildEnd::Code(code) => format!("exit code {code}"),
            ChildEnd::Signal(signal) => format!("signal {signal}"),
        }
    }
}

/// What the supervisor does after a child stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SupervisorAction {
    /// Exit code 0: tray Quit, or there was nothing to run.
    StopClean,
    /// The display session is gone, so a restart would spin after logout.
    StopSessionGone,
    /// More than five restarts already happened inside five minutes.
    StopRestartCap,
    /// Start the child again after `delay_secs`.
    Restart { delay_secs: u64 },
}

/// Back-off before each restart inside the five-minute window: 2s, 5s, 15s, 30s, then 30s.
const RESTART_BACKOFF_SECS: [u64; 5] = [2, 5, 15, 30, 30];

/// Five restarts are allowed. The next one inside the same five minutes stops the supervisor.
const MAX_RESTARTS_IN_WINDOW: u32 = 5;

/// `restarts_in_window` counts restarts already performed in the last five minutes,
/// not including the decision being made.
pub fn supervisor_action(
    end: ChildEnd,
    session_present: bool,
    restarts_in_window: u32,
) -> SupervisorAction {
    if matches!(end, ChildEnd::Code(0)) {
        return SupervisorAction::StopClean;
    }
    if !session_present {
        return SupervisorAction::StopSessionGone;
    }
    if restarts_in_window >= MAX_RESTARTS_IN_WINDOW {
        return SupervisorAction::StopRestartCap;
    }
    let index = (restarts_in_window as usize).min(RESTART_BACKOFF_SECS.len() - 1);
    SupervisorAction::Restart {
        delay_secs: RESTART_BACKOFF_SECS[index],
    }
}

/// Banner the open window shows about background sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonBanner {
    Restarting,
    NotResponding,
    CouldNotStart,
}

/// One liveness sample. `spawn` means the window should start `--tray` once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowLiveness {
    pub banner: Option<DaemonBanner>,
    pub spawn: bool,
    pub attempts: u32,
}

/// Failed `--tray` starts from the window before it stops trying.
pub const MAX_WINDOW_SPAWN_ATTEMPTS: u32 = 3;

/// `status_stale` is true when the engine is not answering: the unix socket's
/// `/health` has lapsed and the event stream is missing or has been quiet for
/// more than 10 seconds. See [`engine_not_responding`]. `spawn_due` is true when
/// the window has not started background sync in the last 10 seconds.
///
/// A spawn counts as failed only once that 10 second wait has passed and the lock
/// is still dead. The third failure switches the banner to [`DaemonBanner::CouldNotStart`].
pub fn window_liveness(
    alive: bool,
    status_stale: bool,
    attempts: u32,
    spawn_due: bool,
) -> WindowLiveness {
    if alive {
        return WindowLiveness {
            banner: if status_stale {
                Some(DaemonBanner::NotResponding)
            } else {
                None
            },
            spawn: false,
            attempts: 0,
        };
    }
    if attempts >= MAX_WINDOW_SPAWN_ATTEMPTS && spawn_due {
        return WindowLiveness {
            banner: Some(DaemonBanner::CouldNotStart),
            spawn: false,
            attempts,
        };
    }
    if attempts >= MAX_WINDOW_SPAWN_ATTEMPTS {
        return WindowLiveness {
            banner: Some(DaemonBanner::Restarting),
            spawn: false,
            attempts,
        };
    }
    if spawn_due {
        return WindowLiveness {
            banner: Some(DaemonBanner::Restarting),
            spawn: true,
            attempts: attempts + 1,
        };
    }
    WindowLiveness {
        banner: Some(DaemonBanner::Restarting),
        spawn: false,
        attempts,
    }
}

/// The engine is not publishing when `/health` has not succeeded within the
/// last 10 seconds and the event stream has not delivered a frame (or a
/// keepalive) in that same stretch.
///
/// A successful health check is enough on its own. The server keepalive is
/// 15 seconds, so a quiet stream is not a stall while health still answers.
/// A stream that delivered something in the last 10 seconds also counts, so
/// one missed health probe does not flash the banner.
///
/// `events_quiet_secs` is `None` when the stream has never connected.
pub fn engine_not_responding(health_ok: bool, events_quiet_secs: Option<f64>) -> bool {
    if health_ok {
        return false;
    }
    events_quiet_secs.is_none_or(|age| age > 10.0)
}

/// True when the window should sample the lock and the engine socket again.
/// `last_check_secs < 0` forces the first sample.
pub fn check_due(now_secs: f64, last_check_secs: f64, interval_secs: f64) -> bool {
    last_check_secs < 0.0 || now_secs - last_check_secs >= interval_secs
}

/// `status.json` is stale when its age is over 10 seconds. A missing file counts
/// only after it has been missing for 10 seconds (`missing_for_secs`).
pub fn status_is_stale(age_secs: Option<f64>, missing_for_secs: Option<f64>) -> bool {
    age_secs.is_some_and(|age| age > 10.0) || missing_for_secs.is_some_and(|age| age > 10.0)
}

/// `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`, or an absolute `WAYLAND_DISPLAY`.
pub fn wayland_socket_path(runtime: Option<&str>, display: Option<&str>) -> Option<PathBuf> {
    let display = display.filter(|value| !value.is_empty())?;
    let path = PathBuf::from(display);
    if path.is_absolute() {
        return Some(path);
    }
    let runtime = runtime.filter(|value| !value.is_empty())?;
    Some(PathBuf::from(runtime).join(path))
}

/// Socket for `$DISPLAY`, for example `:0` or `:0.0` → `/tmp/.X11-unix/X0`.
pub fn x11_socket_path(display: &str) -> Option<PathBuf> {
    let rest = display.trim().rsplit_once(':')?.1;
    let screen = rest.split('.').next()?.trim();
    if screen.is_empty() || !screen.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(PathBuf::from(format!("/tmp/.X11-unix/X{screen}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{AccountState, PairState, Progress};

    fn pair(name: &str, phase: Phase) -> PairState {
        PairState {
            name: name.into(),
            local: format!("/home/alex/{name}"),
            remote: format!("/my-files/{name}"),
            phase,
            progress: Progress::default(),
            scanned_folders: 0,
            current_op: None,
            last_error: None,
            last_synced: None,
            tracked: 0,
        }
    }

    fn state(pairs: Vec<PairState>) -> AppState {
        AppState {
            pairs,
            account: AccountState {
                checked: true,
                backend_ready: true,
                signed_in: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn idle_when_nothing_runs_and_nothing_is_wrong() {
        let s = state(vec![
            pair("Documents", Phase::Synced),
            pair("Photos", Phase::Idle),
        ]);
        assert_eq!(tray_state(&s), TrayState::Idle);
        assert_eq!(tray_state(&AppState::default()), TrayState::Idle);
    }

    #[test]
    fn syncing_for_a_scanning_or_syncing_folder_or_a_busy_run() {
        let s = state(vec![pair("Documents", Phase::Scanning)]);
        assert_eq!(tray_state(&s), TrayState::Syncing);
        let s = state(vec![pair("Documents", Phase::Syncing)]);
        assert_eq!(tray_state(&s), TrayState::Syncing);
        let mut s = state(vec![pair("Documents", Phase::Synced)]);
        s.busy = true;
        assert_eq!(tray_state(&s), TrayState::Syncing);
    }

    #[test]
    fn error_beats_syncing() {
        let s = state(vec![
            pair("Documents", Phase::Error),
            pair("Photos", Phase::Syncing),
        ]);
        assert_eq!(tray_state(&s), TrayState::Error);
    }

    #[test]
    fn attention_for_signed_out_and_for_a_locked_keyring() {
        let mut s = state(vec![
            pair("Documents", Phase::Error),
            pair("Photos", Phase::Syncing),
        ]);
        s.signed_out = true;
        assert_eq!(tray_state(&s), TrayState::Attention);

        let mut s = state(vec![pair("Documents", Phase::Error)]);
        s.account.keyring_locked = true;
        s.account.signed_in = false;
        assert_eq!(tray_state(&s), TrayState::Attention);
    }

    #[test]
    fn status_text_follows_the_state() {
        let now = 1_000_000;
        let mut s = state(vec![
            pair("Documents", Phase::Synced),
            pair("Photos", Phase::Synced),
        ]);
        assert_eq!(tray_status_text(&s, now), "Up to date");
        s.pairs[0].last_synced = Some(now - 300);
        s.pairs[1].last_synced = Some(now - 4000);
        assert_eq!(
            tray_status_text(&s, now),
            "Up to date · last sync 5 minutes ago"
        );

        s.pairs[0].phase = Phase::Syncing;
        s.pairs[0].progress = Progress {
            done: 30,
            total: 100,
        };
        s.pairs[1].phase = Phase::Syncing;
        s.pairs[1].progress = Progress { done: 7, total: 20 };
        assert_eq!(tray_status_text(&s, now), "Syncing 37 of 120");
        s.pairs[0].phase = Phase::Scanning;
        s.pairs[0].progress = Progress::default();
        s.pairs[1].phase = Phase::Scanning;
        s.pairs[1].progress = Progress::default();
        assert_eq!(tray_status_text(&s, now), "Syncing…");

        s.pairs[0].phase = Phase::Error;
        s.pairs[1].phase = Phase::Synced;
        assert_eq!(tray_status_text(&s, now), "Problem in Documents");
        s.pairs[1].phase = Phase::Error;
        assert_eq!(tray_status_text(&s, now), "Problems in 2 folders");

        s.account.keyring_locked = true;
        assert_eq!(
            tray_status_text(&s, now),
            "Waiting for the keyring to unlock"
        );
        s.signed_out = true;
        assert_eq!(
            tray_status_text(&s, now),
            "Signed out — open NeutronSync to sign in"
        );
    }

    #[test]
    fn clean_exit_stops_without_restart() {
        assert_eq!(
            supervisor_action(ChildEnd::Code(0), true, 0),
            SupervisorAction::StopClean
        );
        assert_eq!(
            supervisor_action(ChildEnd::Code(0), false, 4),
            SupervisorAction::StopClean
        );
    }

    #[test]
    fn backoff_sequence_then_cap() {
        let delays: Vec<_> = (0..5)
            .map(|n| supervisor_action(ChildEnd::Signal(9), true, n))
            .collect();
        assert_eq!(
            delays,
            vec![
                SupervisorAction::Restart { delay_secs: 2 },
                SupervisorAction::Restart { delay_secs: 5 },
                SupervisorAction::Restart { delay_secs: 15 },
                SupervisorAction::Restart { delay_secs: 30 },
                SupervisorAction::Restart { delay_secs: 30 },
            ]
        );
        assert_eq!(
            supervisor_action(ChildEnd::Code(1), true, 5),
            SupervisorAction::StopRestartCap
        );
        assert_eq!(
            supervisor_action(ChildEnd::Signal(6), true, 9),
            SupervisorAction::StopRestartCap
        );
    }

    #[test]
    fn missing_session_does_not_restart() {
        assert_eq!(
            supervisor_action(ChildEnd::Signal(11), false, 0),
            SupervisorAction::StopSessionGone
        );
        assert_eq!(
            supervisor_action(ChildEnd::Code(2), false, 1),
            SupervisorAction::StopSessionGone
        );
    }

    #[test]
    fn child_end_text_includes_status() {
        assert_eq!(ChildEnd::Code(1).describe(), "exit code 1");
        assert_eq!(ChildEnd::Signal(9).describe(), "signal 9");
    }

    #[test]
    fn window_restarts_dead_daemon_then_stops_trying() {
        let first = window_liveness(false, false, 0, true);
        assert_eq!(first.banner, Some(DaemonBanner::Restarting));
        assert!(first.spawn);
        assert_eq!(first.attempts, 1);

        let waiting = window_liveness(false, false, 1, false);
        assert_eq!(waiting.banner, Some(DaemonBanner::Restarting));
        assert!(!waiting.spawn);

        let second = window_liveness(false, false, 1, true);
        assert!(second.spawn);
        assert_eq!(second.attempts, 2);
        let third = window_liveness(false, false, 2, true);
        assert!(third.spawn);
        assert_eq!(third.attempts, 3);

        let in_flight = window_liveness(false, false, 3, false);
        assert_eq!(in_flight.banner, Some(DaemonBanner::Restarting));
        assert!(!in_flight.spawn);

        let failed = window_liveness(false, false, 3, true);
        assert_eq!(failed.banner, Some(DaemonBanner::CouldNotStart));
        assert!(!failed.spawn);
        assert_eq!(failed.attempts, 3);
    }

    #[test]
    fn socket_down_while_the_lock_is_live_is_not_responding() {
        assert!(engine_not_responding(false, None));
        assert!(engine_not_responding(false, Some(11.0)));
        assert!(!engine_not_responding(false, Some(3.0)));
        let down = window_liveness(true, true, 0, false);
        assert_eq!(down.banner, Some(DaemonBanner::NotResponding));
        assert!(!down.spawn);
        assert_eq!(down.attempts, 0);
    }

    #[test]
    fn socket_up_shows_no_banner() {
        assert!(!engine_not_responding(true, None));
        assert!(!engine_not_responding(true, Some(30.0)));
        let up = window_liveness(true, engine_not_responding(true, Some(1.0)), 2, true);
        assert_eq!(up.banner, None);
        assert!(!up.spawn);
        assert_eq!(up.attempts, 0);
    }

    #[test]
    fn live_daemon_clears_attempts_and_reports_stale_status() {
        let fresh = window_liveness(true, false, 2, true);
        assert_eq!(fresh.banner, None);
        assert!(!fresh.spawn);
        assert_eq!(fresh.attempts, 0);

        let stale = window_liveness(true, true, 2, true);
        assert_eq!(stale.banner, Some(DaemonBanner::NotResponding));
        assert!(!stale.spawn);
        assert_eq!(stale.attempts, 0);
    }

    #[test]
    fn try_again_is_a_fresh_spawn() {
        let again = window_liveness(false, false, 0, true);
        assert!(again.spawn);
        assert_eq!(again.attempts, 1);
        assert_eq!(again.banner, Some(DaemonBanner::Restarting));
    }

    #[test]
    fn liveness_check_is_at_most_every_two_seconds() {
        assert!(check_due(0.0, -1.0, 2.0));
        assert!(!check_due(1.9, 0.0, 2.0));
        assert!(check_due(2.0, 0.0, 2.0));
    }

    #[test]
    fn status_older_than_ten_seconds_is_stale() {
        assert!(!status_is_stale(Some(10.0), None));
        assert!(status_is_stale(Some(10.1), None));
        assert!(!status_is_stale(None, Some(10.0)));
        assert!(status_is_stale(None, Some(10.1)));
        assert!(!status_is_stale(None, None));
    }

    #[test]
    fn display_socket_paths() {
        assert_eq!(
            wayland_socket_path(Some("/run/user/1000"), Some("wayland-0")),
            Some(PathBuf::from("/run/user/1000/wayland-0"))
        );
        assert_eq!(
            wayland_socket_path(None, Some("/tmp/wayland-9")),
            Some(PathBuf::from("/tmp/wayland-9"))
        );
        assert_eq!(wayland_socket_path(None, Some("wayland-0")), None);
        assert_eq!(
            x11_socket_path(":0"),
            Some(PathBuf::from("/tmp/.X11-unix/X0"))
        );
        assert_eq!(
            x11_socket_path(":1.0"),
            Some(PathBuf::from("/tmp/.X11-unix/X1"))
        );
        assert_eq!(x11_socket_path("no-display"), None);
    }
}
