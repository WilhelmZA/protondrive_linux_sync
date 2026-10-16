//! Restart and window-liveness decisions for the headless tray.
//!
//! These functions do not spawn processes or read the display sockets. The
//! supervisor and the window supply the facts and apply the decision.

use std::path::PathBuf;

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

/// `status_stale` is true when `status.json` is older than 10 seconds, or missing
/// for that long while the lock is still live. `spawn_due` is true when the window
/// has not started background sync in the last 10 seconds.
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

/// True when the window should sample the lock and `status.json` again.
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
