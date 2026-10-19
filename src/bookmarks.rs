//! File manager sidebar bookmarks (`gtk-3.0/bookmarks`), as pure text logic.
//!
//! The file is one bookmark per line: a `file://` URI, optionally followed by
//! a space and a label. NeutronSync adds one line per synced folder and only
//! ever removes a line it added itself: a line counts as ours when both the
//! folder and the label match what NeutronSync wrote (see [`Owned`]). A
//! bookmark the user already had for the same folder is left alone and never
//! claimed, and so is a second line the user adds for a folder we bookmark.
//! Other lines keep their text and order. A line we added and the user then
//! removed by hand stays out until the folder is no longer wanted (the switch
//! goes off, or the folder leaves NeutronSync). [`reconcile`] computes the
//! new text; the caller reads, writes and remembers the owned list.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A bookmark NeutronSync wants in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub label: String,
}

/// A line NeutronSync wrote: the folder and the label it carried. `label` is
/// `None` only for records written before labels were kept; such a record
/// matches a line only when it is the sole line for that folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owned {
    pub path: PathBuf,
    pub label: Option<String>,
}

/// The outcome of a reconcile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reconciled {
    /// The whole file as it should read now.
    pub text: String,
    /// The lines NeutronSync owns after this reconcile, including a wanted
    /// line the user removed by hand (kept so it is not put back).
    pub owned: Vec<Owned>,
    /// False when `text` equals the input, so no write is needed.
    pub changed: bool,
}

/// Bring the file in line with `wanted`. `current` is the file as it reads
/// now (empty when missing) and `owned` the lines NeutronSync added before.
///
/// A wanted folder with no line in the file gets one appended and becomes
/// owned, unless NeutronSync owned a line for it that the user removed by
/// hand: that folder stays out and stays owned. A wanted folder whose only
/// lines are the user's is never claimed. An owned line that is no longer
/// wanted is removed; an owned line still wanted is kept, with its label
/// brought up to date. Every other line is untouched.
pub fn reconcile(current: &str, wanted: &[Entry], owned: &[Owned]) -> Reconciled {
    let lines: Vec<(&str, Option<(PathBuf, String)>)> = current
        .lines()
        .map(|line| (line, parse_line(line)))
        .collect();

    // Which line each owned record stands for, if it is still there.
    let mut ours: Vec<Option<usize>> = Vec::with_capacity(owned.len());
    let mut claimed: Vec<bool> = vec![false; lines.len()];
    for record in owned {
        let found = lines.iter().enumerate().position(|(i, (_, parsed))| {
            !claimed[i]
                && parsed.as_ref().is_some_and(|(path, label)| {
                    *path == record.path
                        && match &record.label {
                            Some(want) => label == want,
                            None => {
                                lines
                                    .iter()
                                    .filter(|(_, p)| {
                                        p.as_ref().is_some_and(|(q, _)| *q == record.path)
                                    })
                                    .count()
                                    == 1
                            }
                        }
                })
        });
        if let Some(i) = found {
            claimed[i] = true;
        }
        ours.push(found);
    }

    let mut out: Vec<String> = Vec::new();
    let mut now_owned: Vec<Owned> = Vec::new();
    for (i, (line, parsed)) in lines.iter().enumerate() {
        if !claimed[i] {
            out.push(line.to_string());
            continue;
        }
        let (path, _) = parsed.as_ref().expect("a claimed line parsed");
        // Ours and still wanted: keep it, with the current label. Ours and
        // no longer wanted: drop it.
        if let Some(entry) = wanted.iter().find(|e| e.path == *path) {
            out.push(bookmark_line(&entry.path, &entry.label));
            now_owned.push(Owned {
                path: entry.path.clone(),
                label: Some(entry.label.clone()),
            });
        }
    }

    // An owned line the user removed by hand: while the folder is still
    // wanted, remember it and leave it out. Once it is not wanted, forget it.
    for (record, found) in owned.iter().zip(&ours) {
        if found.is_none() && wanted.iter().any(|e| e.path == record.path) {
            now_owned.push(record.clone());
        }
    }

    for entry in wanted {
        let present = lines
            .iter()
            .any(|(_, p)| p.as_ref().is_some_and(|(q, _)| *q == entry.path));
        let declined = now_owned.iter().any(|o| o.path == entry.path);
        if present || declined {
            continue;
        }
        out.push(bookmark_line(&entry.path, &entry.label));
        now_owned.push(Owned {
            path: entry.path.clone(),
            label: Some(entry.label.clone()),
        });
    }

    let mut text = out.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    let changed = text != current;
    Reconciled {
        text,
        owned: now_owned,
        changed,
    }
}

/// `file:///percent-encoded/path Label`, as GTK writes it.
pub fn bookmark_line(path: &Path, label: &str) -> String {
    let uri = file_uri(path);
    let label = label.trim();
    if label.is_empty() {
        uri
    } else {
        format!("{uri} {label}")
    }
}

/// `file://` URI for an absolute path. Everything but unreserved characters
/// and `/` is percent-encoded, which every GTK reader decodes.
pub fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The folder a bookmark line points at, if it is a `file://` bookmark.
pub fn line_path(line: &str) -> Option<PathBuf> {
    parse_line(line).map(|(path, _)| path)
}

/// Folder and label (trimmed, possibly empty) of a `file://` bookmark line.
fn parse_line(line: &str) -> Option<(PathBuf, String)> {
    let line = line.trim_end();
    let (uri, label) = match line.split_once(char::is_whitespace) {
        Some((uri, rest)) => (uri, rest.trim()),
        None => (line, ""),
    };
    let rest = uri.strip_prefix("file://")?;
    // "file:///path" and "file://localhost/path" both mean this machine.
    let path = if let Some(p) = rest.strip_prefix("localhost/") {
        format!("/{p}")
    } else if rest.starts_with('/') {
        rest.to_string()
    } else {
        return None;
    };
    Some((PathBuf::from(percent_decode(&path)), label.to_string()))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, label: &str) -> Entry {
        Entry {
            path: PathBuf::from(path),
            label: label.into(),
        }
    }

    fn owned(path: &str, label: &str) -> Owned {
        Owned {
            path: PathBuf::from(path),
            label: Some(label.into()),
        }
    }

    const DOCS: &str = "/home/alex/Documents";
    const PICS: &str = "/home/alex/Pictures";

    #[test]
    fn adds_to_an_empty_or_missing_file() {
        let wanted = vec![entry(DOCS, "Documents"), entry(PICS, "Photos")];
        let r = reconcile("", &wanted, &[]);
        assert!(r.changed);
        assert_eq!(
            r.text,
            "file:///home/alex/Documents Documents\nfile:///home/alex/Pictures Photos\n"
        );
        assert_eq!(
            r.owned,
            vec![owned(DOCS, "Documents"), owned(PICS, "Photos")]
        );
    }

    #[test]
    fn adds_beside_user_lines_which_keep_their_order() {
        let current = "file:///home/alex/Music\nfile:///home/alex/Videos Films\n\nfile:///home/alex/Downloads\n";
        let r = reconcile("", &[], &[]);
        assert!(!r.changed);
        let r = reconcile(current, &[entry(DOCS, "Documents")], &[]);
        assert!(r.changed);
        assert_eq!(
            r.text,
            format!("{current}file:///home/alex/Documents Documents\n")
        );
        assert_eq!(r.owned, vec![owned(DOCS, "Documents")]);
    }

    #[test]
    fn spaces_and_non_ascii_are_percent_encoded_and_decoded() {
        let path = "/home/alex/My Files/Zürich (2026)";
        let line = bookmark_line(Path::new(path), "Zürich");
        assert_eq!(
            line,
            "file:///home/alex/My%20Files/Z%C3%BCrich%20%282026%29 Zürich"
        );
        assert_eq!(line_path(&line), Some(PathBuf::from(path)));
        let r = reconcile("", &[entry(path, "Zürich")], &[]);
        assert_eq!(r.text, format!("{line}\n"));
    }

    #[test]
    fn removes_only_lines_it_owns() {
        let current = "file:///home/alex/Music\nfile:///home/alex/Documents Documents\nfile:///home/alex/Pictures Photos\n";
        let mine = vec![owned(DOCS, "Documents"), owned(PICS, "Photos")];
        // Photos no longer wanted: its line goes, Documents and Music stay.
        let r = reconcile(current, &[entry(DOCS, "Documents")], &mine);
        assert!(r.changed);
        assert_eq!(
            r.text,
            "file:///home/alex/Music\nfile:///home/alex/Documents Documents\n"
        );
        assert_eq!(r.owned, vec![owned(DOCS, "Documents")]);
        // Switch off: every owned line goes, the user's stays.
        let r = reconcile(current, &[], &mine);
        assert_eq!(r.text, "file:///home/alex/Music\n");
        assert!(r.owned.is_empty());
    }

    #[test]
    fn a_users_own_bookmark_for_the_same_folder_is_never_claimed_or_removed() {
        let current = "file:///home/alex/Documents\nfile:///home/alex/Music\n";
        let r = reconcile(current, &[entry(DOCS, "Documents")], &[]);
        assert!(!r.changed, "{:?}", r.text);
        assert!(r.owned.is_empty());
        // Later, with the folder gone from NeutronSync, the user's line stays.
        let r = reconcile(current, &[], &r.owned);
        assert!(!r.changed);
        assert_eq!(r.text, current);
    }

    #[test]
    fn a_users_second_line_for_an_owned_folder_is_kept() {
        // R2-001: ours plus the user's own shortcut to the same folder.
        let current = "file:///tmp/Documents Documents\nfile:///tmp/Documents My shortcut\n";
        let mine = vec![owned("/tmp/Documents", "Documents")];
        let r = reconcile(current, &[], &mine);
        assert_eq!(r.text, "file:///tmp/Documents My shortcut\n");
        assert!(r.owned.is_empty());
        // While still wanted, both lines stay and only ours is owned.
        let r = reconcile(current, &[entry("/tmp/Documents", "Documents")], &mine);
        assert!(!r.changed);
        assert_eq!(r.owned, mine);
        // The user's shortcut first, ours second: same outcome.
        let swapped = "file:///tmp/Documents My shortcut\nfile:///tmp/Documents Documents\n";
        let r = reconcile(swapped, &[], &mine);
        assert_eq!(r.text, "file:///tmp/Documents My shortcut\n");
    }

    #[test]
    fn a_differently_encoded_user_bookmark_still_counts_as_the_same_folder() {
        let current = "file:///home/alex/Docs%20(old)\n";
        let r = reconcile(current, &[entry("/home/alex/Docs (old)", "Old")], &[]);
        assert!(!r.changed);
        assert!(r.owned.is_empty());
    }

    #[test]
    fn unchanged_input_gives_no_write() {
        let current = "file:///home/alex/Music\nfile:///home/alex/Documents Documents\n";
        let mine = vec![owned(DOCS, "Documents")];
        let r = reconcile(current, &[entry(DOCS, "Documents")], &mine);
        assert!(!r.changed);
        assert_eq!(r.text, current);
        assert_eq!(r.owned, mine);
    }

    #[test]
    fn a_hand_deleted_owned_line_stays_gone_until_the_folder_is_no_longer_wanted() {
        // R1-002. The user removed our Documents line in the file manager.
        let mine = vec![owned(DOCS, "Documents")];
        let wanted = vec![entry(DOCS, "Documents")];
        let r = reconcile("file:///home/alex/Music\n", &wanted, &mine);
        assert!(!r.changed);
        assert_eq!(r.owned, mine, "still owned, so it is not put back");
        // Unrelated reconciles keep it out: the same inputs again, the user
        // adding a line of their own, another folder joining NeutronSync.
        let r = reconcile("file:///home/alex/Music\n", &wanted, &r.owned);
        assert!(!r.changed);
        assert_eq!(r.owned, mine);
        let r = reconcile(
            "file:///home/alex/Music\nfile:///home/alex/Videos\n",
            &wanted,
            &r.owned,
        );
        assert!(!r.changed);
        let more = vec![entry(DOCS, "Documents"), entry(PICS, "Photos")];
        let r = reconcile(
            "file:///home/alex/Music\nfile:///home/alex/Videos\n",
            &more,
            &r.owned,
        );
        assert_eq!(
            r.text,
            "file:///home/alex/Music\nfile:///home/alex/Videos\nfile:///home/alex/Pictures Photos\n"
        );
        assert_eq!(
            r.owned,
            vec![owned(DOCS, "Documents"), owned(PICS, "Photos")]
        );
        // Switch off: ownership ends and nothing of ours is left.
        let off = reconcile(&r.text, &[], &r.owned);
        assert_eq!(
            off.text,
            "file:///home/alex/Music\nfile:///home/alex/Videos\n"
        );
        assert!(off.owned.is_empty());
        // Switch on again: the line is added anew.
        let on = reconcile(&off.text, &more, &off.owned);
        assert!(on.text.contains("file:///home/alex/Documents Documents\n"));
        assert_eq!(on.owned.len(), 2);
    }

    #[test]
    fn removing_and_re_adding_a_folder_adds_its_line_anew() {
        let mine = vec![owned(DOCS, "Documents")];
        // Removed by hand, then the folder leaves NeutronSync: forgotten.
        let r = reconcile("", &[], &mine);
        assert!(r.owned.is_empty());
        // The folder comes back: bookmarked again.
        let r = reconcile("", &[entry(DOCS, "Documents")], &r.owned);
        assert_eq!(r.text, "file:///home/alex/Documents Documents\n");
    }

    #[test]
    fn a_renamed_folder_updates_the_label_of_an_owned_line() {
        let current = "file:///home/alex/Documents Documents\n";
        let r = reconcile(
            current,
            &[entry(DOCS, "Papers")],
            &[owned(DOCS, "Documents")],
        );
        assert!(r.changed);
        assert_eq!(r.text, "file:///home/alex/Documents Papers\n");
        assert_eq!(r.owned, vec![owned(DOCS, "Papers")]);
    }

    #[test]
    fn a_record_without_a_label_matches_only_a_sole_line_for_the_folder() {
        let legacy = vec![Owned {
            path: PathBuf::from(DOCS),
            label: None,
        }];
        // One line for the folder: it is ours and goes on switch off.
        let r = reconcile(
            "file:///home/alex/Music\nfile:///home/alex/Documents Documents\n",
            &[],
            &legacy,
        );
        assert_eq!(r.text, "file:///home/alex/Music\n");
        // Two lines for the folder: nothing can be told apart, nothing is removed.
        let two = "file:///home/alex/Documents Documents\nfile:///home/alex/Documents Mine\n";
        let r = reconcile(two, &[], &legacy);
        assert!(!r.changed);
        assert!(r.owned.is_empty());
        // Still wanted: the sole line is adopted with its label.
        let r = reconcile(
            "file:///home/alex/Documents Documents\n",
            &[entry(DOCS, "Documents")],
            &legacy,
        );
        assert!(!r.changed);
        assert_eq!(r.owned, vec![owned(DOCS, "Documents")]);
    }

    #[test]
    fn lines_that_are_not_file_uris_pass_through() {
        let current = "sftp://box/home\nfile:///home/alex/Music\n";
        let r = reconcile(current, &[], &[]);
        assert!(!r.changed);
        assert_eq!(line_path("sftp://box/home"), None);
        assert_eq!(line_path(""), None);
        assert_eq!(
            line_path("file://localhost/home/alex"),
            Some(PathBuf::from("/home/alex"))
        );
    }
}
