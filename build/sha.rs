// Shared by build.rs and tests/build_sha.rs, which includes this file so the
// selection order is tested even though Cargo never runs tests inside a build
// script.

/// Pick the build identifier from the sources `build.rs` has to hand, in order
/// of trust: an explicit `NEUTRONSYNC_BUILD_SHA` from the environment, the git
/// checkout (`-dirty` when tracked files are modified), the `.build-sha` file
/// that `git archive` fills in through `export-subst`, else `unknown`.
///
/// `git_head` is the short hash git reported, if any. `build_sha_file` is the
/// raw content of `.build-sha`; it still holds the literal `$Format` when the
/// tree did not come from `git archive`, and is then ignored.
pub fn choose_build_sha(
    env_override: Option<&str>,
    git_head: Option<&str>,
    git_dirty: bool,
    build_sha_file: Option<&str>,
) -> String {
    if let Some(v) = env_override.map(str::trim).filter(|v| !v.is_empty()) {
        return v.to_string();
    }
    if let Some(h) = git_head.map(str::trim).filter(|h| !h.is_empty()) {
        return if git_dirty {
            format!("{h}-dirty")
        } else {
            h.to_string()
        };
    }
    if let Some(f) = build_sha_file.map(str::trim) {
        if !f.is_empty() && !f.contains("$Format") {
            // `.build-sha` carries the full hash so the length does not depend
            // on git's abbreviation setting; show the same 12 characters as a
            // git checkout does.
            return f.chars().take(12).collect();
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_wins_over_everything() {
        assert_eq!(
            choose_build_sha(Some("override"), Some("abc"), true, Some("def")),
            "override"
        );
        // Blank means unset.
        assert_eq!(
            choose_build_sha(Some("  "), Some("abc"), false, None),
            "abc"
        );
    }

    #[test]
    fn git_head_with_dirty_marker() {
        assert_eq!(
            choose_build_sha(None, Some("0123456789ab\n"), false, Some("ffff")),
            "0123456789ab"
        );
        assert_eq!(
            choose_build_sha(None, Some("0123456789ab"), true, None),
            "0123456789ab-dirty"
        );
    }

    #[test]
    fn archive_file_is_used_only_once_substituted() {
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            choose_build_sha(None, None, false, Some(full)),
            "0123456789ab"
        );
        assert_eq!(
            choose_build_sha(None, None, false, Some("$Format:%H$\n")),
            "unknown"
        );
        assert_eq!(choose_build_sha(None, None, false, Some("")), "unknown");
    }

    #[test]
    fn nothing_known_is_said_plainly() {
        assert_eq!(choose_build_sha(None, None, false, None), "unknown");
    }
}
