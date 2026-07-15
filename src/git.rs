use crate::model::GitInfo;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct GitStatus {
    pub info: GitInfo,
    pub toplevel: Option<PathBuf>,
    /// Where the project sits inside the repo: `""` when it *is* the repo root,
    /// `apps/api` when it is one package of a monorepo. Everything below is
    /// scoped to this, because `git status` answers for the whole repository and
    /// the question being asked is about one project inside it.
    pub prefix: Option<String>,
    /// Paths git ignores, relative to `toplevel`. Directories keep a trailing
    /// slash - git collapses an ignored directory into a single entry.
    pub ignored: Vec<String>,
    /// Paths git has never seen, relative to `toplevel`.
    pub untracked: Vec<String>,
    /// git could not be *asked*. There is a `.git` here, but the command that
    /// would answer for it failed: no developer tools behind `/usr/bin/git`, a
    /// `safe.directory` refusal on a tree owned by another user, a cron `PATH`
    /// without git on it, a worktree whose parent repo has been deleted.
    ///
    /// Emphatically not the same fact as "not a repository", and collapsing the
    /// two is what this field exists to stop. Every deletion in this tool is
    /// vouched for by git: `tracks_anything_under` is what keeps `clean` off a
    /// committed `node_modules`, and the ignored/untracked lists are what produce
    /// the orphan list, the one that says which files exist nowhere else.
    ///
    /// Read as "no repo", a git that merely failed turns the first check off and
    /// empties the second, then reports both as fact: a tracked `node_modules`
    /// moves from UNTOUCHED to WILL BE DELETED, the `.env` and the `uploads/`
    /// vanish from "archive-only", and the tool says "not a git
    /// repository" about a directory with a `.git` in it.
    pub unavailable: bool,
}

/// Is this tree inside a git repository, according to the filesystem alone?
///
/// Asked without git on purpose, and that is the entire point. It is the only
/// thing that can tell "git says this is not a repository" apart from "git could
/// not be asked", and those two answers must never be given the same weight: one
/// is a fact, the other is the absence of one.
fn has_git_dir(dir: &Path) -> bool {
    std::iter::successors(Some(dir), |d| d.parent()).any(|d| d.join(".git").exists())
}

/// `--no-optional-locks` is not a nicety. Plain `git status` refreshes the
/// index's stat cache and writes `.git/index` back to disk - which would make
/// `scan` and `plan` modify every repo they look at, while claiming not to.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

fn trimmed(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

pub fn status(dir: &Path) -> GitStatus {
    let mut st = GitStatus {
        info: GitInfo::default(),
        toplevel: None,
        prefix: None,
        ignored: Vec::new(),
        untracked: Vec::new(),
        unavailable: false,
    };

    let Some(top) = trimmed(dir, &["rev-parse", "--show-toplevel"]) else {
        // git declined to answer. Whether that means "there is no repository
        // here" or "I could not run" is the difference between a safe deletion
        // and a wrong one, and only the filesystem can say which.
        st.unavailable = has_git_dir(dir);
        return st;
    };
    let top = PathBuf::from(&top);
    st.info.is_repo = true;

    // `--show-toplevel` is absolute and symlink-resolved; `dir` may be neither.
    // Without this the strip below fails and the whole repo leaks into a report
    // that is supposed to be about one project inside it.
    let here = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    st.prefix = here
        .strip_prefix(&top)
        .ok()
        .map(|p| p.to_string_lossy().to_string());
    st.toplevel = Some(top);

    st.info.branch = trimmed(dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    st.info.commit = trimmed(dir, &["rev-parse", "--short", "HEAD"]);
    st.info.remote = trimmed(dir, &["remote", "get-url", "origin"]);
    st.info.unpushed = trimmed(dir, &["rev-list", "--count", "@{u}..HEAD"])
        .and_then(|s| s.parse::<usize>().ok());
    st.info.last_commit = trimmed(dir, &["log", "-1", "--format=%ct"])
        .and_then(|s| s.parse::<i64>().ok());

    let scope = Scope(st.prefix.clone());

    // The one command that produces the ignored list, the untracked list and the
    // dirty count: everything that vouches for a deletion, and everything that
    // argues against one. If it fails we know nothing, and an empty answer would
    // read as *good* news: no ignored files to preserve, no uncommitted work,
    // nothing untracked. That is a clean repo with a remote, which this tool calls
    // REDUNDANT and tells you to delete.
    //
    // -z keeps paths raw: without it git quotes and escapes anything unusual.
    let Some(raw) = git(dir, &["status", "--porcelain", "--ignored", "-z"]) else {
        st.unavailable = true;
        return st;
    };

    {
        let mut it = raw.split('\0').filter(|s| !s.is_empty());
        while let Some(entry) = it.next() {
            if entry.len() < 3 {
                continue;
            }
            let (code, path) = entry.split_at(2);
            let path = path.trim_start().to_string();

            // A rename entry is followed by its origin path as a second record.
            if code.starts_with('R') || code.starts_with('C') {
                it.next();
            }

            match code {
                // An ignored *ancestor* of the project is kept even though it is
                // not inside it: it is what tells us the project's own artifacts
                // are ignored, since git collapses a fully-ignored directory
                // into one entry and never mentions what is under it.
                "!!" if scope.inside(&path) || scope.covers(&path) => st.ignored.push(path),
                "!!" => {}
                "??" if scope.inside(&path) => {
                    st.info.untracked += 1;
                    st.untracked.push(path);
                }
                "??" => {}
                _ if scope.inside(&path) => st.info.dirty += 1,
                _ => {}
            }
        }
    }

    st
}

/// The project's slice of a repository that may be much larger than it.
struct Scope(Option<String>);

impl Scope {
    fn norm(p: &str) -> &str {
        p.trim_end_matches('/')
    }

    /// `path` is the project itself or something under it.
    fn inside(&self, path: &str) -> bool {
        let Some(pre) = self.0.as_deref().map(Self::norm).filter(|p| !p.is_empty()) else {
            return true; // the project *is* the repo
        };
        let p = Self::norm(path);
        p == pre || p.strip_prefix(pre).is_some_and(|r| r.starts_with('/'))
    }

    /// `path` is an ancestor of the project - an ignored directory the project
    /// happens to live inside.
    fn covers(&self, path: &str) -> bool {
        let Some(pre) = self.0.as_deref().map(Self::norm).filter(|p| !p.is_empty()) else {
            return false;
        };
        let p = Self::norm(path);
        pre.strip_prefix(p).is_some_and(|r| r.starts_with('/'))
    }
}

/// Whether git tracks any file inside `rel`. A tracked "build" directory is
/// somebody's source tree, not an artifact - this is what keeps us off it.
pub fn tracks_anything_under(top: &Path, rel: &str) -> bool {
    git(top, &["ls-files", "--", rel]).is_some_and(|s| !s.trim().is_empty())
}

/// Where the objects actually live. For an ordinary repo this is the `.git`
/// directory inside the project; for a worktree or a submodule it is somewhere
/// else entirely - which is the whole reason to ask.
///
/// `--git-common-dir`, not `--git-dir`: a worktree's own git dir is a per-worktree
/// scratch area under the parent's `.git/worktrees/`, while the history - every
/// object the archive would need - is in the common one.
pub fn git_dir(dir: &Path) -> Option<String> {
    trimmed(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
}

/// The `.gitignore` pattern that is the reason `rel` is ignored, if it is.
///
/// The distinction this exists to draw: git collapses a fully-ignored directory
/// into one entry, so an `app/` holding nothing but build output is reported as
/// `app/` and never as `app/build/`. Concluding from that alone that anything
/// under an ignored directory is generated is what let `clean` delete a `storage/out`
/// full of PDFs, because `storage/` was ignored. The pattern says which of the
/// two it is: `app/build/` names the build directory, `storage/` does not name
/// `out`.
pub fn ignore_pattern(top: &Path, rel: &str) -> Option<String> {
    use std::io::Write;
    use std::process::Stdio;

    // The path goes in on stdin, which is the only way `-z` is allowed - and `-z`
    // is what keeps a directory with a quote or a newline in its name from being
    // handed back to us escaped and unparseable.
    //
    // Not the `git()` helper either: `check-ignore` exits 1 to mean "not
    // ignored", which is an answer rather than a failure, while 128 is a real
    // error. Both are non-zero, and only one of them means "no pattern".
    let mut child = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(top)
        .args(["check-ignore", "--no-index", "-v", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    child.stdin.take()?.write_all(rel.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }

    // <source>\0<lineno>\0<pattern>\0<pathname>\0
    let s = String::from_utf8_lossy(&out.stdout);
    let pattern = s.split('\0').nth(2)?;
    (!pattern.is_empty()).then(|| pattern.to_string())
}
