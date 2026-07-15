use crate::detect::{self, is_project_root};
use crate::git;
use crate::model::*;
use crate::rules;

use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Directory names discovery never descends into while looking for projects.
const DISCOVERY_SKIP: &[&str] = &[
    "node_modules", ".git", "Library", "Applications", ".Trash", "target", "vendor", "Pods",
    "DerivedData", ".venv", "venv", "dist", "build", ".next",
];

/// Ignored paths that are technically "not in git" but that nobody would miss.
/// Listing them as orphans would drown the signal we actually care about (.env,
/// uploads, local databases) in noise.
const ORPHAN_NOISE: &[&str] = &[".DS_Store", ".eslintcache", "Thumbs.db"];

#[derive(Default, Clone, Copy)]
struct Stats {
    bytes: u64,
    compressible: u64,
    incompressible: u64,
    files: u64,
    newest_mtime: i64,
    /// A file no build step produces was seen under here: a model checkpoint, a
    /// dataset, a database. Read per-artifact to keep such a directory out of the
    /// deletable set even when its name and build command say it is safe.
    has_user_data: bool,
}

impl Stats {
    fn add_file(&mut self, path: &Path, md: &fs::Metadata, in_git: bool) {
        let sz = disk_size(md);
        self.files += 1;
        self.bytes += sz;
        if in_git || detect::is_incompressible(path) {
            self.incompressible += sz;
        } else {
            self.compressible += sz;
        }
        if !in_git && detect::is_user_data(path) {
            self.has_user_data = true;
        }

        // How well a file compresses has nothing to do with when it was last
        // worked on, and tying the two together made a video edited this morning
        // count for nothing: a project whose newest file was a `.mp4` reported an
        // age of 900 days. Every real file dates the project, whatever its bytes
        // look like to zstd.
        //
        // Two exceptions, both because they are written by something other than
        // the person: Finder rewrites `.DS_Store` into folders nobody has opened
        // in a year, and anything under `.git` is touched by every git command
        // that runs - including the one in a shell prompt.
        let noise = path
            .file_name()
            .is_some_and(|n| n == ".DS_Store" || n == "Thumbs.db");
        if !in_git && !noise {
            self.newest_mtime = self.newest_mtime.max(md.mtime());
        }
    }

    fn merge(&mut self, o: &Stats) {
        self.bytes += o.bytes;
        self.compressible += o.compressible;
        self.incompressible += o.incompressible;
        self.files += o.files;
        self.newest_mtime = self.newest_mtime.max(o.newest_mtime);
        self.has_user_data |= o.has_user_data;
    }

    /// zstd -19 lands around 4x on source trees. Bytes that are already
    /// compressed (images, git packfiles, media) pass through at 1x, which is
    /// what keeps the estimate from lying about asset-heavy projects.
    fn estimate(&self) -> u64 {
        self.incompressible + self.compressible / 4
    }
}

/// Real space on disk, not apparent size - this is what deleting actually frees.
fn disk_size(md: &fs::Metadata) -> u64 {
    md.blocks() * 512
}

pub fn discover(roots: &[PathBuf], max_depth: usize) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue: Vec<(PathBuf, usize)> = roots.iter().map(|r| (r.clone(), 0)).collect();

    while let Some((dir, depth)) = queue.pop() {
        if is_project_root(&dir) {
            found.push(dir);
            continue; // a project owns everything beneath it, nested markers and all
        }
        if depth >= max_depth {
            continue;
        }
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if !ft.is_dir() || ft.is_symlink() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if DISCOVERY_SKIP.contains(&name.as_str()) {
                continue;
            }
            queue.push((e.path(), depth + 1));
        }
    }

    found.sort();
    found
}

struct Walked {
    stats: Stats,
    git_size: u64,
    /// (path relative to project root, directory name)
    candidates: Vec<(String, String)>,
    stacks: Vec<String>,
}

/// One pass over the project. Pruning is deliberately stack-*agnostic*: any
/// directory whose name appears in the rule table is cut here and classified
/// later, once the whole tree has told us which stacks are actually in play.
/// Deciding the stack up front is what made container repos - a bare `.git` at
/// the root with real projects underneath - come back with nothing to clean.
fn walk(root: &Path) -> Walked {
    let mut out = Walked {
        stats: Stats::default(),
        git_size: 0,
        candidates: Vec::new(),
        stacks: Vec::new(),
    };
    let mut queue = vec![root.to_path_buf()];

    while let Some(dir) = queue.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            let Ok(md) = fs::symlink_metadata(&path) else {
                continue;
            };
            let name = e.file_name().to_string_lossy().to_string();

            if md.is_dir() {
                if name == ".git" {
                    // Discovery skips `.git`; the walk must too. A directory inside
                    // `.git` whose name happens to match a rule (a hook template
                    // dir, a worktree's artifacts) is git's, not a candidate, and
                    // deleting anything under `.git` corrupts the repository. Its
                    // bytes are still counted; they are just measured, not walked
                    // for deletion.
                    let g = measure_in_git(&path);
                    out.git_size += g.bytes;
                    out.stats.merge(&g);
                    continue;
                }
                if rules::is_known_artifact_name(&name) {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    out.candidates.push((rel, name));
                    continue;
                }
                if name.ends_with(".xcodeproj") || name.ends_with(".xcworkspace") {
                    push_stack(&mut out.stacks, "xcode");
                }
                queue.push(path);
            } else {
                // Git objects are zlib streams already; counting them as
                // compressible would inflate every estimate.
                let in_git = path.components().any(|c| c.as_os_str() == ".git");
                if in_git {
                    out.git_size += disk_size(&md);
                }
                out.stats.add_file(&path, &md, in_git);

                if !in_git {
                    if let Some(s) = detect::marker_stack(&name) {
                        push_stack(&mut out.stacks, s);
                    }
                }
            }
        }
    }

    out
}

fn push_stack(v: &mut Vec<String>, s: &str) {
    if !v.iter().any(|x| x == s) {
        v.push(s.to_string());
    }
}

/// Does this `.gitignore` pattern name the directory itself?
///
/// `app/build/`, `build`, `**/build` and `/app/build` all name `build`.
/// `storage/` names `storage` - so it says nothing about the `out/` inside it,
/// and must not be read as saying anything. A pattern we cannot attribute (a
/// bare `*`, a glob like `build-*`) is not proof either, and answering "no"
/// costs a REVIEW while answering "yes" could cost the directory.
fn names_the_dir(pattern: &str, name: &str) -> bool {
    pattern
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .is_some_and(|last| last == name)
}

fn measure_dir(root: &Path) -> Stats {
    measure(root, false)
}

/// Sum a `.git` directory the way the walk used to when it descended into it:
/// every file incompressible (git objects are already compressed) and none of
/// them dating the project. Kept separate so the walk can count `.git` without
/// ever treating anything inside it as a deletion candidate.
fn measure_in_git(root: &Path) -> Stats {
    let mut stats = Stats::default();
    let mut queue = vec![root.to_path_buf()];

    while let Some(dir) = queue.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            let Ok(md) = fs::symlink_metadata(&path) else {
                continue;
            };
            if md.is_dir() {
                queue.push(path);
            } else {
                stats.add_file(&path, &md, true);
            }
        }
    }

    stats
}

fn measure(root: &Path, prune_artifacts: bool) -> Stats {
    let mut stats = Stats::default();
    let mut queue = vec![root.to_path_buf()];

    while let Some(dir) = queue.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            let Ok(md) = fs::symlink_metadata(&path) else {
                continue;
            };
            if md.is_dir() {
                if prune_artifacts
                    && rules::is_known_artifact_name(&e.file_name().to_string_lossy())
                {
                    continue;
                }
                queue.push(path);
            } else {
                stats.add_file(&path, &md, false);
            }
        }
    }

    stats
}

/// Size of an orphan. An untracked *directory* - a whole subproject git never
/// saw - must not be billed for the `node_modules` inside it, or the same bytes
/// get reported once as savings and again as content worth preserving.
fn orphan_size(p: &Path) -> u64 {
    match fs::symlink_metadata(p) {
        Ok(md) if md.is_dir() => measure(p, true).bytes,
        Ok(md) => disk_size(&md),
        Err(_) => 0,
    }
}

pub fn analyze(root: &Path) -> Project {
    let d = detect::detect(root);
    let gs = git::status(root);
    let w = walk(root);

    // Root detection gives the pretty labels (nextjs, nestjs); the walk gives
    // the coarse stacks that rules key off. Neither alone is enough.
    let mut stacks = d.stacks.clone();
    for s in &w.stacks {
        push_stack(&mut stacks, s);
    }

    // Git reports paths relative to the repo root, which is not always the
    // project root - a project can sit inside a larger repo.
    let rel_to_top = gs.prefix.clone();
    let to_git_path = |rel: &str| match rel_to_top.as_deref() {
        Some("") | None => rel.to_string(),
        Some(prefix) => format!("{prefix}/{rel}"),
    };
    // The inverse: a repo-relative path back into the project, or None if it is
    // not inside this project at all.
    let to_project_path = |gitrel: &str| -> Option<String> {
        match rel_to_top.as_deref() {
            Some("") | None => Some(gitrel.to_string()),
            Some(prefix) => gitrel
                .strip_prefix(prefix)
                .and_then(|r| r.strip_prefix('/'))
                .map(str::to_string),
        }
    };

    let ignored: HashSet<String> = gs
        .ignored
        .iter()
        .map(|p| p.trim_end_matches('/').to_string())
        .collect();

    let top_dir = gs.toplevel.as_deref().unwrap_or(root);

    // git collapses its ignored list to the topmost directory that is entirely
    // ignored: an `android/app` holding nothing but `build/` is reported as
    // `android/app/`, never as `android/app/build/`. Matching the artifact path
    // exactly therefore missed it, and the biggest directory in the project came
    // back REVIEW.
    //
    // But "an ancestor of it is ignored" is *not* the same claim as "git says
    // this directory is generated", and treating it as one is how a `storage/`
    // full of user data - ignored, as it should be - vouched for deleting the
    // `storage/out` inside it. When the entry git gave us is an ancestor rather
    // than the directory itself, something else has to vouch for it, and there
    // are exactly two things that can:
    //
    //   1. The ignore *pattern*. `app/build/` names the build directory even
    //      though git collapsed the report to `app/`. A bare `storage/` does not
    //      name `out`, and never claimed to.
    //
    //   2. The package manager that owns every byte of it. An Expo project
    //      gitignores `/ios` wholesale because it generates the whole native tree
    //      - and the `Pods/` inside it is still Pods, with a `Podfile.lock` sitting
    //      next to it that puts it back exactly.
    //
    //      Only for directories nothing hand-written lives in (`rule.owned`:
    //      `node_modules`, `Pods`, `.venv`). It emphatically does not extend to
    //      `vendor/`, and *that* was the hole: Rails keeps real source in
    //      `vendor/assets`, so an ignored ancestor plus any old `Gemfile.lock`
    //      deleted hand-written JavaScript that existed in no repository anywhere.
    //      A generic name like `out` has nothing that owns it either, so it falls
    //      to rule 1 alone.
    let is_ignored = |gitrel: &str, name: &str, rule: &rules::Rule, reversible: bool| -> bool {
        if ignored.contains(gitrel) {
            return true;
        }
        let ancestor_ignored = std::iter::successors(Path::new(gitrel).parent(), |p| p.parent())
            .take_while(|p| !p.as_os_str().is_empty())
            .any(|p| ignored.contains(p.to_string_lossy().as_ref()));
        if !ancestor_ignored {
            return false;
        }
        if git::ignore_pattern(top_dir, gitrel).is_some_and(|pat| names_the_dir(&pat, name)) {
            return true;
        }
        rule.owned && reversible
    };

    let mut warnings: Vec<String> = Vec::new();

    // git is here but would not answer. Everything below that decides what may be
    // deleted (is it tracked, is it ignored, is it in git at all) is an answer from
    // the command that just failed, and the failure looks exactly like good news:
    // nothing tracked, nothing ignored, nothing untracked. Say it out loud, and let
    // the REVIEW that `warnings` forces keep every destructive command off this
    // project until a human has looked.
    if gs.unavailable {
        warnings.push(
            "git could not be run: there is a .git here but I cannot ask its status, \
             nothing will be deleted automatically"
                .into(),
        );
    }

    // The project is a subdirectory of a bigger repo: its `.git` lives above the
    // archive root, so an archive of this folder alone carries no history at
    // all. The git branch/commit we report belongs to the parent, which makes
    // the omission look like a decision rather than an accident.
    if let Some(top) = &gs.toplevel {
        if top != root {
            warnings.push(format!(
                "this project is a subdirectory of the {} repo - archive has no .git, history is not preserved",
                top.display()
            ));
        } else if !root.join(".git").is_dir() {
            // The project *is* its repo root - and its `.git` is a pointer file,
            // not a directory. That is a worktree or a submodule: the objects live
            // in the repository it was cut from, at an absolute path that this
            // archive does not carry and cannot carry. The archive holds a 171-byte
            // `gitdir:` line pointing at a machine that may not have that path any
            // more, and git answers "not a git repository".
            //
            // The check above cannot see this. Inside a worktree `git rev-parse
            // --show-toplevel` answers with the worktree itself, so `top == root`
            // and the subdirectory warning never fires - the tool reported full git
            // history, archived it, and `parc archive` moved the original to the
            // Trash. The source survived; two years of history did not.
            let real = git::git_dir(root);
            warnings.push(format!(
                "this is a git worktree/submodule - .git is a file, history lives in {} and does not go into the archive",
                real.as_deref().unwrap_or("another repo")
            ));
        }
    }

    let mut artifacts: Vec<Artifact> = Vec::new();
    let mut art_stats: Vec<(Class, Stats)> = Vec::new();

    for (rel, name) in &w.candidates {
        let st = measure_dir(&root.join(rel));
        let gitrel = to_git_path(rel);

        // The name is in the rule table, but no rule for it applies to *this*
        // project's stacks - `target/` in a Node repo, say. Not an artifact.
        let Some(rule) = rules::match_dir(name, &stacks) else {
            art_stats.push((Class::Blocked, st));
            artifacts.push(Artifact {
                path: rel.clone(),
                rule: name.clone(),
                size: st.bytes,
                class: Class::Blocked,
                note: Some("no generated-file rule for this stack - leaving it untouched".into()),
            });
            continue;
        };

        // Whether the thing that puts this directory back survives in the archive:
        // the lockfile of the toolchain that owns it, or the command that builds
        // it, or nothing at all because it is a cache. Asked once: it decides both
        // whether git's word is enough and whether deleting it is reversible at
        // all.
        let reversible = detect::reversible(root, &root.join(rel), rule);

        let (mut class, mut note) = if gs.unavailable {
            // Not "git says this is generated", and not "git says nothing": git was
            // never able to say. A `node_modules` here may be the committed,
            // hand-patched one that `tracks_anything_under` would have blocked, and
            // `clean` does not use the Trash. Nothing is confirmed on a guess.
            (
                Class::Review,
                Some("could not ask git: cannot verify it is generated".into()),
            )
        } else if gs.info.is_repo {
            if git::tracks_anything_under(top_dir, &gitrel) {
                (
                    Class::Blocked,
                    Some("git tracks files in this folder - this is source, not an artifact".into()),
                )
            } else if is_ignored(&gitrel, name, rule, reversible) {
                (Class::Confirmed, None)
            } else if rule.ambiguous {
                (
                    Class::Review,
                    Some(
                        "gitignore does not name this folder, and its name is ambiguous - manual confirmation needed"
                            .into(),
                    ),
                )
            } else {
                (Class::Confirmed, None)
            }
        } else if rule.ambiguous {
            (
                Class::Review,
                Some("no git, ambiguous name - cannot verify it is generated".into()),
            )
        } else {
            (Class::Confirmed, None)
        };

        // Nothing here can put it back: no lockfile for the toolchain that owns
        // it, or no command that builds it. Refuse to call it safe, whatever git
        // says - git is telling us the directory is *generated*, and generated is
        // not the same claim as regenerable.
        if !reversible && class == Class::Confirmed {
            class = Class::Review;
            note = Some(detect::unreversible_note(root, &root.join(rel), rule));
            warnings.push(format!(
                "nothing to bring it back, {rel} will not be auto-cleaned"
            ));
        }

        // A build directory can be real output and still hold files no build step
        // makes: model weights, a dataset, a database dropped into `out/` because
        // the folder was there. The command rebuilds the folder, not the data, and
        // deleting it loses work that exists nowhere else. The name and the build
        // command both say "safe"; the contents say otherwise, and the contents
        // win. Deps and Cache are unaffected - a lockfile actually reproduces a
        // dependency tree byte for byte, and nobody parks results inside `.next`.
        if class == Class::Confirmed
            && matches!(rule.rebuild, rules::Rebuild::Build)
            && st.has_user_data
        {
            class = Class::Review;
            note = Some(format!(
                "{}/ looks generated but holds data a build would not produce \
                 (model/data/database) - look before deleting",
                rule.dir
            ));
            warnings.push(format!(
                "{rel}: holds files that are not build output, will not be auto-cleaned"
            ));
        }

        // A cache is called reversible because a tool refills it - proven for a
        // `.next/` or a `__pycache__/`, whose name can mean nothing else. For an
        // *ambiguous* cache name the proof is missing: `coverage/` and `.cache/`
        // are also where people park real, hand-made data, and here the only thing
        // vouching for deletion is git ignoring the name. That is a fair signal but
        // not a proof, and this tool does not auto-delete on a signal it cannot
        // stand behind - so it asks. Deps and Build ambiguous names are unaffected:
        // their reversibility was actually established (a lockfile, an adjacent
        // build command), not assumed.
        if class == Class::Confirmed
            && rule.ambiguous
            && matches!(rule.rebuild, rules::Rebuild::Cache)
        {
            class = Class::Review;
            note = Some(format!(
                "{}/ is usually a generated cache, but its name is ambiguous and it may \
                 hold hand-placed data too - look before deleting",
                rule.dir
            ));
        }
        if class == Class::Blocked && rule.ambiguous {
            warnings.push(format!("{rel}: git tracks it, left out of cleanup"));
        }

        art_stats.push((class, st));
        artifacts.push(Artifact {
            path: rel.clone(),
            rule: rule.dir.to_string(),
            size: st.bytes,
            class,
            note,
        });
    }

    // Anything git has no copy of. This - not the byte count - is the reason a
    // project needs an archive rather than a delete.
    //
    // Scoped to this project, and named relative to it. `git status` answers for
    // the whole repository: run inside `apps/api` of a monorepo it reports
    // `apps/web/.env` too, and listing that under "these live only in the
    // archive" was a promise the archive could not keep - `apps/web` is not in
    // it. A file we are not going to preserve must never appear in the list of
    // things we preserve.
    let mut orphans: Vec<Orphan> = Vec::new();
    if gs.info.is_repo {
        for p in gs.ignored.iter().chain(gs.untracked.iter()) {
            let Some(rel) = to_project_path(p.trim_end_matches('/')) else {
                continue;
            };
            let base = rel.rsplit('/').next().unwrap_or(&rel);
            if rules::is_known_artifact_name(base)
                || ORPHAN_NOISE.contains(&base)
                || base.ends_with(".tsbuildinfo")
            {
                continue;
            }
            orphans.push(Orphan {
                size: orphan_size(&root.join(&rel)),
                path: rel,
            });
        }
        orphans.sort_by_key(|o| std::cmp::Reverse(o.size));
    }

    let confirmed: u64 = art_stats
        .iter()
        .filter(|(c, _)| *c == Class::Confirmed)
        .map(|(_, s)| s.bytes)
        .sum();
    let review: u64 = art_stats
        .iter()
        .filter(|(c, _)| *c == Class::Review)
        .map(|(_, s)| s.bytes)
        .sum();

    // Whatever is not confirmed-removable ends up inside the archive, so it has
    // to be estimated as archive content rather than counted as savings.
    let mut keep = w.stats;
    for (c, s) in &art_stats {
        if *c != Class::Confirmed {
            keep.merge(s);
        }
    }

    let total = w.stats.bytes + art_stats.iter().map(|(_, s)| s.bytes).sum::<u64>();
    let (verdict, reasons) = decide(&gs, &orphans, &warnings);

    Project {
        name: root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.display().to_string()),
        path: root.to_path_buf(),
        stacks,
        package_manager: d.package_manager,
        total_size: total,
        artifact_size: confirmed,
        review_size: review,
        source_size: keep.bytes,
        est_archive: keep.estimate(),
        git_size: w.git_size,
        artifacts,
        orphans,
        git: gs.info,
        last_modified: (w.stats.newest_mtime > 0).then_some(w.stats.newest_mtime),
        verdict,
        reasons,
        warnings,
    }
}

fn decide(gs: &git::GitStatus, orphans: &[Orphan], warnings: &[String]) -> (Verdict, Vec<String>) {
    let g = &gs.info;
    let mut reasons = Vec::new();

    // Every path through here must fall out the bottom. An early return for the
    // no-git case is what let a project with no lockfile come back ARCHIVE:
    // the warning was raised and then never looked at.
    //
    // Nothing is said about the repository when git could not be *asked* about it.
    // That case states itself, as a warning, and warnings are folded into `reasons`
    // below. What matters here is that neither of the other two arms gets to run.
    // "not a git repository" would be a false statement about a directory with a `.git`
    // in it. And every count in the last arm (dirty, untracked, unpushed, the orphan
    // list) is an answer from the command that just failed, so all of them come back
    // zero: a clean, fully-pushed repository holding nothing git does not already
    // have. Which is REDUNDANT. Which this tool prints as "deletable".
    match (gs.unavailable, g.is_repo) {
        (true, _) => {}
        (false, false) => {
            reasons.push("not a git repository - this copy on disk is the only copy".into());
        }
        (false, true) => {
            if g.dirty > 0 {
                reasons.push(format!("{} uncommitted changes", g.dirty));
            }
            if g.untracked > 0 {
                reasons.push(format!("{} untracked files", g.untracked));
            }
            if g.remote.is_none() {
                reasons.push("no remote - not pushed anywhere".into());
            } else {
                match g.unpushed {
                    Some(n) if n > 0 => reasons.push(format!("{n} unpushed commits")),
                    None => {
                        reasons.push("no upstream branch - push status cannot be verified".into())
                    }
                    _ => {}
                }
            }
            if !orphans.is_empty() {
                let names: Vec<_> = orphans.iter().take(3).map(|o| o.path.as_str()).collect();
                reasons.push(format!(
                    "{} files outside git ({}{})",
                    orphans.len(),
                    names.join(", "),
                    if orphans.len() > 3 { ", …" } else { "" }
                ));
            }
        }
    }

    if !warnings.is_empty() {
        reasons.extend(warnings.iter().cloned());
        return (Verdict::Review, reasons);
    }
    if reasons.is_empty() {
        return (
            Verdict::Redundant,
            vec!["fully present on the remote - no archive needed, deletable".into()],
        );
    }
    (Verdict::Archive, reasons)
}

pub fn analyze_all(paths: &[PathBuf], progress: bool) -> Vec<Project> {
    let done = AtomicUsize::new(0);
    let total = paths.len();

    let mut out: Vec<Project> = paths
        .par_iter()
        .map(|p| {
            let proj = analyze(p);
            if progress {
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                eprint!("\r\x1b[2K  scanning {n}/{total}  {}", proj.name);
            }
            proj
        })
        .collect();

    if progress {
        eprint!("\r\x1b[2K");
    }

    // By size on disk, which is the question being asked: "what is eating my
    // disk?" Sorting by *savings* buried a 40 GB project under a 2 GB one just
    // because more of the 2 GB was reclaimable.
    out.sort_by_key(|p| std::cmp::Reverse(p.total_size));
    out
}
