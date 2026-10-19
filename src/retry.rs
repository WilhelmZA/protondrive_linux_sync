//! In-run retry for transfers: which failures are worth another attempt, how
//! long to wait, and when a run should stop retrying altogether.
//!
//! Classification and the delay are pure functions so the engine's choices can
//! be unit-tested without a clock. Sleeping is the one side effect, and it
//! stops at once on cancel.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::driveapi::RpcError;

/// Attempts per transfer in one run: the first try plus three retries.
pub const MAX_ATTEMPTS: u32 = 4;
/// Consecutive transfers that may exhaust their retries before the run stops
/// retrying.
pub const BREAKER_TRIP: usize = 3;

/// Whether a failed transfer may be tried again in this run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// A network drop, a server error, a file still being written.
    Transient,
    /// The server asked for a pause; `retry_after` is in seconds when sent.
    RateLimited { retry_after: Option<u64> },
    /// Not found, conflict, auth, disk full, a refused path: retrying cannot help.
    Permanent,
}

impl Class {
    pub fn is_retryable(self) -> bool {
        !matches!(self, Class::Permanent)
    }
}

/// Classify a transfer failure. Only a typed sidecar error or a connection
/// level io error is retried; every other error, including unknown codes, is
/// permanent.
pub fn classify(error: &anyhow::Error) -> Class {
    if let Some(rpc) = error.downcast_ref::<RpcError>() {
        return match rpc.code.as_str() {
            "transient" => Class::Transient,
            "rate_limited" => Class::RateLimited {
                retry_after: rpc.retry_after,
            },
            _ => Class::Permanent,
        };
    }
    if let Some(io) = error.downcast_ref::<std::io::Error>() {
        use std::io::ErrorKind::*;
        return match io.kind() {
            Interrupted | TimedOut | WouldBlock | ConnectionReset | ConnectionAborted
            | BrokenPipe => Class::Transient,
            _ => Class::Permanent,
        };
    }
    Class::Permanent
}

/// Timing of retries. The defaults are the production values; tests scale
/// `unit` down so a run with retries finishes in milliseconds.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// One second of the schedule. Every other field counts in units.
    pub unit: Duration,
    /// Longest base delay, before jitter: 30 units.
    pub cap: u32,
    /// A `retry_after` above this many units is not retried in this run.
    pub max_retry_after: u64,
    /// Longest single sleep before cancel is checked again.
    pub slice: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            unit: Duration::from_secs(1),
            cap: 30,
            max_retry_after: 300,
            slice: Duration::from_millis(200),
        }
    }
}

impl Policy {
    /// The same schedule with every second shortened to `unit`.
    pub fn scaled(unit: Duration) -> Self {
        Self {
            unit,
            slice: Duration::from_millis(200).min(unit),
            ..Self::default()
        }
    }

    /// How long to wait before retry `retry` (1, 2 or 3) of a transfer that
    /// failed with `class`. `None` means the failure is not retried: it is
    /// permanent, the retries are used up, or the server asked for a pause
    /// longer than this run will wait.
    pub fn delay(&self, retry: u32, class: Class) -> Option<Duration> {
        if retry == 0 || retry >= MAX_ATTEMPTS {
            return None;
        }
        let retry_after = match class {
            Class::Permanent => return None,
            Class::Transient => None,
            Class::RateLimited { retry_after } => Some(retry_after.unwrap_or(1)),
        };
        if retry_after.is_some_and(|secs| secs > self.max_retry_after) {
            return None;
        }
        let base = (1u64 << retry).min(u64::from(self.cap));
        // Equal jitter: a uniform value in [base/2, base].
        let half = self.unit.mul_f64(base as f64 / 2.0);
        let jittered = half + half.mul_f64(uniform());
        let floor = retry_after.map(|secs| self.unit.mul_f64(secs as f64));
        Some(match floor {
            Some(floor) if floor > jittered => floor,
            _ => jittered,
        })
    }
}

/// A uniform value in [0, 1] from the standard library's randomly seeded
/// hasher; good enough to spread retries, with no extra crate.
fn uniform() -> f64 {
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default(),
    );
    (hasher.finish() % 10_001) as f64 / 10_000.0
}

/// Sleep for `delay` in short slices. Returns false as soon as `cancel` is set.
pub fn sleep(delay: Duration, slice: Duration, cancel: &AtomicBool) -> bool {
    let end = Instant::now() + delay;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return true;
        }
        std::thread::sleep(left.min(slice));
    }
}

/// Short text for a log line: "4s", "750ms".
pub fn describe(delay: Duration) -> String {
    if delay >= Duration::from_secs(1) {
        format!("{}s", delay.as_secs_f64().round() as u64)
    } else {
        format!("{}ms", delay.as_millis())
    }
}

/// Stops a run from retrying once `BREAKER_TRIP` transfers in a row have used
/// up their retries on a retryable error. Any success closes it again.
#[derive(Default, Debug)]
pub struct Breaker {
    consecutive: AtomicUsize,
}

impl Breaker {
    pub fn new() -> Self {
        Self::default()
    }
    /// A transfer failed on its last allowed attempt with a retryable error.
    pub fn exhausted(&self) {
        self.consecutive.fetch_add(1, Ordering::SeqCst);
    }
    pub fn success(&self) {
        self.consecutive.store(0, Ordering::SeqCst);
    }
    /// True once the run should stop retrying.
    pub fn open(&self) -> bool {
        self.consecutive.load(Ordering::SeqCst) >= BREAKER_TRIP
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn rpc(code: &str, retry_after: Option<u64>) -> anyhow::Error {
        RpcError {
            code: code.into(),
            retry_after,
        }
        .into()
    }

    #[test]
    fn classifies_every_rpc_code() {
        assert_eq!(classify(&rpc("transient", None)), Class::Transient);
        assert_eq!(
            classify(&rpc("rate_limited", Some(7))),
            Class::RateLimited {
                retry_after: Some(7)
            }
        );
        assert_eq!(
            classify(&rpc("rate_limited", None)),
            Class::RateLimited { retry_after: None }
        );
        for code in [
            "auth",
            "keyring_locked",
            "not_found",
            "conflict",
            "fatal",
            "something_new",
        ] {
            assert_eq!(classify(&rpc(code, None)), Class::Permanent, "{code}");
            assert!(!classify(&rpc(code, Some(3))).is_retryable(), "{code}");
        }
    }

    #[test]
    fn classifies_io_kinds_and_other_errors() {
        for kind in [
            ErrorKind::Interrupted,
            ErrorKind::TimedOut,
            ErrorKind::WouldBlock,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
        ] {
            let error: anyhow::Error = std::io::Error::new(kind, "io").into();
            assert_eq!(classify(&error), Class::Transient, "{kind:?}");
        }
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::NotFound,
            ErrorKind::AlreadyExists,
            ErrorKind::InvalidData,
            ErrorKind::Other,
            ErrorKind::StorageFull,
            ErrorKind::Unsupported,
        ] {
            let error: anyhow::Error = std::io::Error::new(kind, "io").into();
            assert_eq!(classify(&error), Class::Permanent, "{kind:?}");
        }
        assert_eq!(
            classify(&anyhow::anyhow!("refusing to join an escaping path")),
            Class::Permanent
        );
        // An io error wrapped with context is still read through the chain.
        let wrapped: anyhow::Error =
            anyhow::Error::from(std::io::Error::new(ErrorKind::BrokenPipe, "pipe"))
                .context("upload a.txt");
        assert_eq!(classify(&wrapped), Class::Transient);
    }

    #[test]
    fn attempts_and_delay_ranges() {
        let policy = Policy::default();
        assert_eq!(MAX_ATTEMPTS, 4);
        assert!(policy.delay(0, Class::Transient).is_none());
        assert!(policy.delay(4, Class::Transient).is_none());
        for (retry, base) in [(1u32, 2u64), (2, 4), (3, 8)] {
            for _ in 0..200 {
                let d = policy.delay(retry, Class::Transient).unwrap();
                assert!(
                    d >= Duration::from_secs(base / 2) && d <= Duration::from_secs(base),
                    "retry {retry}: {d:?}"
                );
            }
        }
        assert!(policy.delay(1, Class::Permanent).is_none());
    }

    #[test]
    fn cap_applies_to_the_base_delay() {
        let policy = Policy {
            cap: 4,
            ..Policy::default()
        };
        for _ in 0..200 {
            let d = policy.delay(3, Class::Transient).unwrap();
            assert!(
                d >= Duration::from_secs(2) && d <= Duration::from_secs(4),
                "{d:?}"
            );
        }
        // With the production cap of 30 s, an eighth retry would still be capped
        // if it existed; the schedule itself never passes 8 s in 3 retries.
        let production = Policy::default();
        let longest = (0..200)
            .map(|_| production.delay(3, Class::Transient).unwrap())
            .max()
            .unwrap();
        assert!(longest <= Duration::from_secs(30));
    }

    #[test]
    fn retry_after_raises_the_delay_and_refuses_long_pauses() {
        let policy = Policy::default();
        for _ in 0..50 {
            let d = policy
                .delay(
                    1,
                    Class::RateLimited {
                        retry_after: Some(2),
                    },
                )
                .unwrap();
            assert!(d >= Duration::from_secs(2), "{d:?}");
            let d = policy
                .delay(
                    1,
                    Class::RateLimited {
                        retry_after: Some(45),
                    },
                )
                .unwrap();
            assert_eq!(d, Duration::from_secs(45));
            // Absent retry_after defaults to 1 s and never lowers the backoff.
            let d = policy
                .delay(3, Class::RateLimited { retry_after: None })
                .unwrap();
            assert!(d >= Duration::from_secs(4), "{d:?}");
        }
        assert!(policy
            .delay(
                1,
                Class::RateLimited {
                    retry_after: Some(300)
                }
            )
            .is_some());
        assert!(policy
            .delay(
                1,
                Class::RateLimited {
                    retry_after: Some(301)
                }
            )
            .is_none());
    }

    #[test]
    fn scaled_policy_keeps_the_shape() {
        let policy = Policy::scaled(Duration::from_millis(10));
        for _ in 0..50 {
            let d = policy.delay(2, Class::Transient).unwrap();
            assert!(
                d >= Duration::from_millis(20) && d <= Duration::from_millis(40),
                "{d:?}"
            );
        }
        assert_eq!(
            policy.delay(
                1,
                Class::RateLimited {
                    retry_after: Some(5)
                }
            ),
            Some(Duration::from_millis(50))
        );
        assert_eq!(policy.slice, Duration::from_millis(10));
    }

    #[test]
    fn cancel_ends_a_sleep_within_250ms() {
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        assert!(sleep(
            Duration::from_millis(50),
            Duration::from_millis(200),
            &cancel
        ));
        assert!(started.elapsed() >= Duration::from_millis(50));
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let setter = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            flag.store(true, Ordering::Relaxed);
        });
        let started = Instant::now();
        assert!(!sleep(
            Duration::from_secs(30),
            Duration::from_millis(200),
            &cancel
        ));
        assert!(
            started.elapsed() < Duration::from_millis(350),
            "{:?}",
            started.elapsed()
        );
        setter.join().unwrap();
        assert!(!sleep(
            Duration::from_secs(30),
            Duration::from_millis(200),
            &cancel
        ));
    }

    #[test]
    fn breaker_trips_after_three_and_resets_on_success() {
        let breaker = Breaker::new();
        breaker.exhausted();
        breaker.exhausted();
        assert!(!breaker.open());
        breaker.exhausted();
        assert!(breaker.open());
        breaker.success();
        assert!(!breaker.open());
    }

    #[test]
    fn describes_delays_in_plain_units() {
        assert_eq!(describe(Duration::from_secs(4)), "4s");
        assert_eq!(describe(Duration::from_millis(3600)), "4s");
        assert_eq!(describe(Duration::from_millis(750)), "750ms");
    }
}
