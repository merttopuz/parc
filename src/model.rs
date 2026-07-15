use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// How confident we are that an artifact directory is safe to delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    /// Git ignores it, or the rule is unambiguous for this stack. Safe to remove.
    Confirmed,
    /// Looks generated but nothing corroborates it. Never removed automatically.
    Review,
    /// Git tracks files inside it. This is source, not an artifact.
    Blocked,
}

#[derive(Debug, Serialize)]
pub struct Artifact {
    pub path: String,
    pub rule: String,
    pub size: u64,
    pub class: Class,
    pub note: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GitInfo {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub commit: Option<String>,
    pub remote: Option<String>,
    pub dirty: usize,
    pub untracked: usize,
    /// None when the branch has no upstream to compare against.
    pub unpushed: Option<usize>,
    /// Unix seconds of the last commit. The honest answer to "when did I last
    /// work on this" - filesystem mtimes get bumped by Finder, Spotlight and
    /// every `pnpm install`, so they cannot be trusted to tell alive from dead.
    #[serde(default)]
    pub last_commit: Option<i64>,
}

/// A file git does not have: either ignored or untracked, and not a known
/// build artifact. These are the only reason archiving beats deleting.
#[derive(Debug, Serialize)]
pub struct Orphan {
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Fully present on a remote. Delete it; `git clone` brings it back.
    Redundant,
    /// Holds something git doesn't. Must be archived before removal.
    Archive,
    /// Something is off - needs a human before any destructive step.
    Review,
}

#[derive(Debug, Serialize)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub stacks: Vec<String>,
    pub package_manager: Option<String>,

    pub total_size: u64,
    /// Sum of `Class::Confirmed` artifacts - what a cleanup would actually free.
    pub artifact_size: u64,
    /// Sum of `Class::Review` artifacts - potential, but not without a human.
    pub review_size: u64,
    pub source_size: u64,
    pub est_archive: u64,
    /// Git history is often the whole story: a project can be huge with nothing
    /// to clean because `.git` ate the disk. Packfiles are already compressed,
    /// so this size survives archiving untouched.
    pub git_size: u64,

    pub artifacts: Vec<Artifact>,
    pub orphans: Vec<Orphan>,
    pub git: GitInfo,
    pub last_modified: Option<i64>,

    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub warnings: Vec<String>,
}

impl Project {
    /// Bytes reclaimed by archiving: everything except what ends up in the archive.
    pub fn savings(&self) -> u64 {
        self.total_size.saturating_sub(self.est_archive)
    }

    pub fn savings_pct(&self) -> f64 {
        if self.total_size == 0 {
            return 0.0;
        }
        (self.savings() as f64 / self.total_size as f64) * 100.0
    }

    /// Bytes we know we can delete because a toolchain regenerates them. These
    /// are measured, not predicted.
    pub fn cleanup_savings(&self) -> u64 {
        self.artifact_size
    }

    /// Bytes zstd is *expected* to squeeze out of what remains. A guess, and
    /// reported separately so it never gets mistaken for the measured number.
    pub fn compression_savings(&self) -> u64 {
        self.source_size.saturating_sub(self.est_archive)
    }

    /// Days since this project was last worked on - the *most recent* of its
    /// last commit and its newest source file.
    ///
    /// Both signals lie on their own, in opposite directions. Filesystem mtime
    /// alone calls a project alive because Finder rewrote a `.DS_Store` (so
    /// noise files are excluded from it upstream). The last commit alone calls a
    /// project dead when its owner simply has not committed in four years while
    /// editing it daily - a real repo here has a 2022 HEAD and 5,048 files
    /// touched this year. Whichever says "recent" wins: for a tool that archives
    /// things, a false "alive" costs disk, and a false "dead" costs work.
    pub fn age_days(&self) -> Option<i64> {
        let ts = match (self.git.last_commit, self.last_modified) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => return None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs() as i64;
        Some((now - ts) / 86_400)
    }

    /// Two path components, so eight projects named `backend` stay distinguishable.
    pub fn label(&self) -> String {
        let parts: Vec<_> = self
            .path
            .components()
            .rev()
            .take(2)
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        parts.into_iter().rev().collect::<Vec<_>>().join("/")
    }
}
