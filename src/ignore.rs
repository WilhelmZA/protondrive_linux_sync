//! Built-in junk-file ignore list.
//!
//! Some files should never be synced no matter which folder they land in: an
//! office app's lock/temp files (`~$Budget.xlsx`, `.~lock.report.odt#`), an
//! editor's swap files, half-finished browser downloads, OS metadata
//! (`.DS_Store`, `Thumbs.db`, `desktop.ini`), and the private state folders of
//! *other* sync clients (`.sync`, `.stfolder`, …). Syncing them is pure noise —
//! they churn constantly, appear and vanish mid-save, and mean nothing on the
//! other machine.
//!
//! These patterns are matched against every path component (so both a junk file
//! and a whole junk directory are caught) and folded into [`crate::config::Pair::is_excluded`],
//! so they inherit the exact same non-destructive semantics as a user exclude:
//! an ignored path is never scanned, uploaded, downloaded, or deleted on either
//! side — it is simply invisible to the engine.
//!
//! The list follows the de-facto standard shared by Nextcloud/ownCloud,
//! Syncthing, Dropbox and friends. Matching is case-insensitive (so `.DS_Store`
//! and `.ds_store`, `Thumbs.db` and `thumbs.db` are one pattern each); patterns
//! are stored lower-cased for that reason.

/// Junk name patterns, matched (case-insensitively) against each path component.
/// `*` matches any run of characters (including none); `?` matches one.
pub const DEFAULT_IGNORE: &[&str] = &[
    // --- Microsoft Office lock / temp files --------------------------------
    "~$*",      // Word/Excel/PowerPoint owner file while a doc is open
    "*.tmp",    // Office (and others) write to a random *.tmp then rename
    "*.laccdb", // Access lock (current)
    "*.ldb",    // Access lock (legacy)
    // --- LibreOffice / OpenOffice lock files -------------------------------
    ".~lock.*#", // .~lock.report.odt#
    ".~lock.*",
    // --- Editor swap / backup files ----------------------------------------
    "*~",     // emacs/gedit/nano backup
    ".*.sw?", // vim swap: .report.swp / .swo / .swn
    ".*.*.sw?",
    "*.kate-swp",
    "*.autosave",
    // --- Partial / in-progress downloads -----------------------------------
    "*.part",
    "*.partial",
    "*.filepart",   // Firefox
    "*.crdownload", // Chrome/Chromium
    "*.download",   // Safari/others
    "*.tmp.drivedownload",
    // --- macOS metadata ----------------------------------------------------
    ".ds_store",
    "._*", // AppleDouble resource forks
    ".appledouble",
    ".lsoverride",
    ".documentrevisions-v100",
    ".fseventsd",
    ".spotlight-v100",
    ".temporaryitems",
    ".trashes",
    ".volumeicon.icns",
    ".apdisk",
    // --- Windows metadata --------------------------------------------------
    "thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    "$recycle.bin",
    "system volume information",
    // --- Linux desktop / filesystem ----------------------------------------
    ".directory", // KDE folder metadata
    ".trash-*",   // freedesktop per-uid trash
    ".nfs*",      // NFS silly-rename
    ".fuse_hidden*",
    // --- Other sync clients' private state ---------------------------------
    ".sync",        // Proton / ownCloud-style client metadata folder
    ".sync.ffs_db", // FreeFileSync
    ".synkron.*",
    ".stfolder",   // Syncthing marker
    ".stversions", // Syncthing versions
    ".stignore",
    "*.unison", // Unison
    ".symform",
    ".symform-store",
    ".dropbox",
    ".dropbox.cache",
    ".dropbox.attr",
    // NeutronSync keep-both copies — never re-sync them or they nest forever
    // ("file (conflict …) (conflict …).pdf").
    "*(conflict *)*",
];

/// Whether `rel` (a POSIX path relative to a pair root) should be ignored as
/// junk — i.e. any of its path components matches a [`DEFAULT_IGNORE`] pattern.
/// Matching a component (not just the leaf) means a junk *directory* such as
/// `.sync` prunes everything beneath it, exactly like a real exclude.
pub fn is_ignored_junk(rel: &str) -> bool {
    rel.split('/').filter(|c| !c.is_empty()).any(|comp| {
        let lc = comp.to_ascii_lowercase();
        DEFAULT_IGNORE
            .iter()
            .any(|pat| glob_match(pat.as_bytes(), lc.as_bytes()))
    })
}

/// A minimal shell-style glob match supporting `*` (any run, incl. empty) and
/// `?` (exactly one). Iterative with backtracking, so no recursion blow-up on a
/// pathological pattern. Both inputs are expected already lower-cased by the
/// caller for case-insensitive matching.
fn glob_match(pat: &[u8], s: &[u8]) -> bool {
    let (mut p, mut i) = (0usize, 0usize);
    // Last place we saw a '*' and the input position to resume from on mismatch.
    let (mut star_p, mut star_i): (Option<usize>, usize) = (None, 0);
    while i < s.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == s[i]) {
            p += 1;
            i += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star_p = Some(p);
            star_i = i;
            p += 1;
        } else if let Some(sp) = star_p {
            // Backtrack: let the last '*' swallow one more input char.
            p = sp + 1;
            star_i += 1;
            i = star_i;
        } else {
            return false;
        }
    }
    // Trailing '*'s in the pattern match the empty remainder.
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::{glob_match, is_ignored_junk};

    #[test]
    fn glob_basics() {
        assert!(glob_match(b"~$*", b"~$budget.xlsx"));
        assert!(glob_match(b"*.tmp", b"a1b2.tmp"));
        assert!(glob_match(b".*.sw?", b".report.swp"));
        assert!(glob_match(b".~lock.*#", b".~lock.report.odt#"));
        assert!(glob_match(b"thumbs.db", b"thumbs.db"));
        assert!(!glob_match(b"*.tmp", b"report.tmpx"));
        assert!(!glob_match(b"~$*", b"budget.xlsx"));
    }

    #[test]
    fn ignores_common_junk() {
        // Office / LibreOffice locks and temps
        assert!(is_ignored_junk("_Work/~$Budget.xlsx"));
        assert!(is_ignored_junk("Notes/.~lock.report.odt#"));
        assert!(is_ignored_junk("x/8FA3C1.tmp"));
        // OS metadata (case-insensitive)
        assert!(is_ignored_junk(".DS_Store"));
        assert!(is_ignored_junk("deep/folder/Thumbs.db"));
        assert!(is_ignored_junk("a/Desktop.ini"));
        assert!(is_ignored_junk("._resourcefork"));
        // Partial downloads
        assert!(is_ignored_junk("Downloads/movie.mkv.part"));
        assert!(is_ignored_junk("Downloads/file.crdownload"));
        // Another sync client's state folder — prunes the whole subtree
        assert!(is_ignored_junk(".sync"));
        assert!(is_ignored_junk(".sync/FolderType"));
        assert!(is_ignored_junk("Documents/.stfolder"));
    }

    #[test]
    fn keeps_real_files() {
        assert!(!is_ignored_junk("_Work/Budget.xlsx"));
        assert!(!is_ignored_junk("_Personal/SARS/return.pdf"));
        assert!(!is_ignored_junk("photos/2025/img_1234.jpg"));
        assert!(!is_ignored_junk("notes/synced-plan.md")); // contains "sync" but not a component
        assert!(!is_ignored_junk("music/track.tmp3")); // not a .tmp extension
        assert!(!is_ignored_junk(""));
        // NeutronSync conflict copies must be ignored (or they re-sync and nest)
        assert!(is_ignored_junk(
            "_Actuarial/record (conflict 20260826-062245).pdf"
        ));
        assert!(is_ignored_junk(
            "a/b (conflict 20260826-052833) (conflict 20260826-053140).pdf"
        ));
        assert!(!is_ignored_junk("_Actuarial/record.pdf"));
    }
}
