//! What to tell the user through desktop notifications, decided from state.
//!
//! [`notices`] is a pure function of the previous and current [`AppState`],
//! the conflicts settled since the last call, a small [`NotifyMemory`] and
//! the clock. It sends nothing; the caller hands each [`Notice`] to the
//! desktop. The rules:
//!
//! - Conflicts are batched: at most one notice per five minutes, summing the
//!   conflicts that arrived in between.
//! - A folder that has stayed in trouble for ten minutes gets one notice, and
//!   no second one until it recovers and fails again.
//! - A sign-out gets one notice per sign-out. A locked keyring gets none,
//!   and neither does the first account probe after start: a tray that
//!   starts while the user is already signed out stays quiet.

use std::collections::{BTreeMap, BTreeSet};

use crate::models::ConflictOutcome;
use crate::service::{AppState, Phase, SettledConflict};

/// Shortest gap between two conflict notices, in seconds.
pub const CONFLICT_GAP_SECS: i64 = 5 * 60;
/// How long a folder stays in trouble before the user hears about it.
pub const PROBLEM_AFTER_SECS: i64 = 10 * 60;
/// Longest problem text a notice body carries.
const BODY_CHARS: usize = 200;

/// One desktop notification: a short title and a one-sentence body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

/// What the notifier remembers between ticks.
#[derive(Clone, Debug, Default)]
pub struct NotifyMemory {
    /// Conflicts that arrived since the last conflict notice.
    pending: Vec<SettledConflict>,
    /// When the last conflict notice went out.
    last_conflict_notice: Option<i64>,
    /// When each folder entered its current stretch in `Phase::Error`.
    error_since: BTreeMap<String, i64>,
    /// Folders already told about in their current stretch of trouble.
    error_told: BTreeSet<String>,
}

/// The notices due now. Call once per tick with the state before and after
/// it, every conflict settled in between, and `now` in epoch seconds.
pub fn notices(
    prev: &AppState,
    cur: &AppState,
    settled: &[SettledConflict],
    mem: &mut NotifyMemory,
    now: i64,
) -> Vec<Notice> {
    let mut out = Vec::new();

    // Sign-out: the edge from signed in to signed out, once. The state at
    // start says "signed in" only because nothing has been checked yet, so
    // the edge counts only after a completed account probe.
    if cur.signed_out && !prev.signed_out && prev.account.checked {
        out.push(Notice {
            title: "Signed out of Proton".into(),
            body: "Sync is paused. Open NeutronSync to sign in.".into(),
        });
    }

    // Conflicts: batch them, one notice per five minutes at most.
    mem.pending.extend(settled.iter().cloned());
    let gap_passed = mem
        .last_conflict_notice
        .is_none_or(|at| now - at >= CONFLICT_GAP_SECS);
    if !mem.pending.is_empty() && gap_passed {
        out.push(conflict_notice(&mem.pending));
        mem.pending.clear();
        mem.last_conflict_notice = Some(now);
    }

    // Lasting problems: ten minutes in Phase::Error, once per stretch.
    let in_error: BTreeSet<&str> = cur
        .pairs
        .iter()
        .filter(|p| p.phase == Phase::Error)
        .map(|p| p.name.as_str())
        .collect();
    mem.error_since
        .retain(|name, _| in_error.contains(name.as_str()));
    mem.error_told
        .retain(|name| in_error.contains(name.as_str()));
    for pair in cur.pairs.iter().filter(|p| p.phase == Phase::Error) {
        let since = *mem.error_since.entry(pair.name.clone()).or_insert(now);
        if now - since >= PROBLEM_AFTER_SECS && !mem.error_told.contains(&pair.name) {
            mem.error_told.insert(pair.name.clone());
            out.push(Notice {
                title: format!("Sync problem in {}", pair.name),
                body: trim_chars(pair.last_error.as_deref().unwrap_or(""), BODY_CHARS),
            });
        }
    }

    out
}

fn conflict_notice(conflicts: &[SettledConflict]) -> Notice {
    let folders: BTreeSet<&str> = conflicts.iter().map(|c| c.pair.as_str()).collect();
    let where_ = if folders.len() == 1 {
        format!("in {}", folders.iter().next().unwrap_or(&""))
    } else {
        format!("in {} folders", folders.len())
    };
    let all_kept = conflicts
        .iter()
        .all(|c| c.outcome == ConflictOutcome::KeptBoth);
    let n = conflicts.len();
    Notice {
        title: if n == 1 {
            "1 file changed in two places".into()
        } else {
            format!("{n} files changed in two places")
        },
        body: if all_kept {
            format!("Both versions were kept {where_}. Open NeutronSync to see them.")
        } else {
            format!("Settled {where_}. Open NeutronSync to see what was kept.")
        },
    }
}

/// The first `max` characters of `s`, with "…" when something was cut.
fn trim_chars(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
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

    fn conflict(pair: &str, path: &str) -> SettledConflict {
        SettledConflict {
            pair: pair.into(),
            path: path.into(),
            outcome: ConflictOutcome::KeptBoth,
        }
    }

    #[test]
    fn two_conflicts_in_one_tick_give_one_notice() {
        let s = state(vec![pair("Documents", Phase::Synced)]);
        let mut mem = NotifyMemory::default();
        let got = notices(
            &s,
            &s,
            &[
                conflict("Documents", "a.txt"),
                conflict("Documents", "b.txt"),
            ],
            &mut mem,
            1_000,
        );
        assert_eq!(
            got,
            vec![Notice {
                title: "2 files changed in two places".into(),
                body: "Both versions were kept in Documents. Open NeutronSync to see them.".into(),
            }]
        );
    }

    #[test]
    fn a_third_conflict_waits_for_the_five_minute_gap_then_is_summed() {
        let s = state(vec![pair("Documents", Phase::Synced)]);
        let mut mem = NotifyMemory::default();
        let first = notices(
            &s,
            &s,
            &[
                conflict("Documents", "a.txt"),
                conflict("Documents", "b.txt"),
            ],
            &mut mem,
            1_000,
        );
        assert_eq!(first.len(), 1);
        let one_minute = notices(&s, &s, &[conflict("Photos", "c.jpg")], &mut mem, 1_000 + 60);
        assert!(one_minute.is_empty(), "{one_minute:?}");
        let quiet = notices(&s, &s, &[], &mut mem, 1_000 + 299);
        assert!(quiet.is_empty(), "{quiet:?}");
        let later = notices(&s, &s, &[], &mut mem, 1_000 + 300);
        assert_eq!(
            later,
            vec![Notice {
                title: "1 file changed in two places".into(),
                body: "Both versions were kept in Photos. Open NeutronSync to see them.".into(),
            }]
        );
        assert!(notices(&s, &s, &[], &mut mem, 1_000 + 301).is_empty());
    }

    #[test]
    fn conflicts_from_several_folders_count_the_folders() {
        let s = state(vec![]);
        let mut mem = NotifyMemory::default();
        let got = notices(
            &s,
            &s,
            &[
                conflict("Documents", "a.txt"),
                conflict("Photos", "b.jpg"),
                conflict("Work", "c.docx"),
                conflict("Work", "d.docx"),
            ],
            &mut mem,
            5,
        );
        assert_eq!(got[0].title, "4 files changed in two places");
        assert_eq!(
            got[0].body,
            "Both versions were kept in 3 folders. Open NeutronSync to see them."
        );
    }

    #[test]
    fn a_conflict_that_did_not_keep_both_says_so() {
        let s = state(vec![]);
        let mut mem = NotifyMemory::default();
        let got = notices(
            &s,
            &s,
            &[SettledConflict {
                pair: "Documents".into(),
                path: "a.txt".into(),
                outcome: ConflictOutcome::KeptLocal,
            }],
            &mut mem,
            5,
        );
        assert_eq!(got[0].title, "1 file changed in two places");
        assert_eq!(
            got[0].body,
            "Settled in Documents. Open NeutronSync to see what was kept."
        );
    }

    #[test]
    fn a_lasting_problem_is_told_once_after_ten_minutes() {
        let ok = state(vec![pair("Documents", Phase::Synced)]);
        let mut broken = state(vec![pair("Documents", Phase::Error)]);
        broken.pairs[0].last_error = Some("upload a.txt: rate_limited".into());
        let mut mem = NotifyMemory::default();
        let t0 = 10_000;
        assert!(notices(&ok, &broken, &[], &mut mem, t0).is_empty());
        assert!(notices(&broken, &broken, &[], &mut mem, t0 + 9 * 60).is_empty());
        let got = notices(&broken, &broken, &[], &mut mem, t0 + 10 * 60);
        assert_eq!(
            got,
            vec![Notice {
                title: "Sync problem in Documents".into(),
                body: "upload a.txt: rate_limited".into(),
            }]
        );
        // Still broken: no second notice, however long it lasts.
        assert!(notices(&broken, &broken, &[], &mut mem, t0 + 11 * 60).is_empty());
        assert!(notices(&broken, &broken, &[], &mut mem, t0 + 3 * 3600).is_empty());
        // Recovers, then breaks again: the clock starts over.
        assert!(notices(&broken, &ok, &[], &mut mem, t0 + 4 * 3600).is_empty());
        let t1 = t0 + 4 * 3600 + 5;
        assert!(notices(&ok, &broken, &[], &mut mem, t1).is_empty());
        assert!(notices(&broken, &broken, &[], &mut mem, t1 + 9 * 60).is_empty());
        assert_eq!(
            notices(&broken, &broken, &[], &mut mem, t1 + 10 * 60).len(),
            1
        );
    }

    #[test]
    fn a_long_problem_text_is_trimmed_to_two_hundred_characters() {
        let mut broken = state(vec![pair("Documents", Phase::Error)]);
        broken.pairs[0].last_error = Some("x".repeat(500));
        let mut mem = NotifyMemory::default();
        notices(&broken, &broken, &[], &mut mem, 0);
        let got = notices(&broken, &broken, &[], &mut mem, 10 * 60);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body.chars().count(), 200);
        assert!(got[0].body.ends_with('…'));
    }

    #[test]
    fn two_folders_in_trouble_are_told_separately() {
        let mut broken = state(vec![
            pair("Documents", Phase::Error),
            pair("Photos", Phase::Error),
        ]);
        broken.pairs[0].last_error = Some("one".into());
        broken.pairs[1].last_error = Some("two".into());
        let mut mem = NotifyMemory::default();
        notices(&broken, &broken, &[], &mut mem, 0);
        let got = notices(&broken, &broken, &[], &mut mem, 600);
        let titles: Vec<&str> = got.iter().map(|n| n.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["Sync problem in Documents", "Sync problem in Photos"]
        );
    }

    #[test]
    fn sign_out_is_told_once_per_sign_out() {
        let signed_in = state(vec![]);
        let mut signed_out = state(vec![]);
        signed_out.signed_out = true;
        signed_out.account.signed_in = false;
        let mut mem = NotifyMemory::default();
        let got = notices(&signed_in, &signed_out, &[], &mut mem, 0);
        assert_eq!(
            got,
            vec![Notice {
                title: "Signed out of Proton".into(),
                body: "Sync is paused. Open NeutronSync to sign in.".into(),
            }]
        );
        assert!(notices(&signed_out, &signed_out, &[], &mut mem, 1).is_empty());
        assert!(notices(&signed_out, &signed_out, &[], &mut mem, 3600).is_empty());
        assert!(notices(&signed_out, &signed_in, &[], &mut mem, 3601).is_empty());
        assert_eq!(
            notices(&signed_in, &signed_out, &[], &mut mem, 3602).len(),
            1
        );
    }

    #[test]
    fn the_first_account_probe_after_start_is_not_a_sign_out() {
        // Controller::new seeds signed_out = false before the probe has run.
        let mut unchecked = state(vec![pair("Documents", Phase::Synced)]);
        unchecked.account = AccountState::default();
        assert!(!unchecked.account.checked && !unchecked.signed_out);
        let mut probed = state(vec![pair("Documents", Phase::Synced)]);
        probed.signed_out = true;
        probed.account.signed_in = false;
        let mut mem = NotifyMemory::default();
        assert!(notices(&unchecked, &probed, &[], &mut mem, 0).is_empty());
        assert!(notices(&probed, &probed, &[], &mut mem, 5).is_empty());
        // Signing in and later out again, after a completed probe, is told.
        let mut back = probed.clone();
        back.signed_out = false;
        back.account.signed_in = true;
        assert!(notices(&probed, &back, &[], &mut mem, 60).is_empty());
        assert_eq!(notices(&back, &probed, &[], &mut mem, 120).len(), 1);
    }

    #[test]
    fn a_locked_keyring_is_not_a_sign_out() {
        let fine = state(vec![pair("Documents", Phase::Synced)]);
        let mut locked = fine.clone();
        locked.account.keyring_locked = true;
        locked.account.signed_in = false;
        let mut mem = NotifyMemory::default();
        assert!(notices(&fine, &locked, &[], &mut mem, 0).is_empty());
        assert!(notices(&locked, &locked, &[], &mut mem, 3600).is_empty());
    }

    #[test]
    fn nothing_happening_gives_nothing() {
        let s = state(vec![pair("Documents", Phase::Synced)]);
        let mut mem = NotifyMemory::default();
        assert!(notices(&s, &s, &[], &mut mem, 0).is_empty());
        assert!(notices(&s, &s, &[], &mut mem, 10_000).is_empty());
    }
}
