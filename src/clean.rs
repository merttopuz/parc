use crate::model::{Class, Project};
use crate::render::human;

use anyhow::Result;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// How many times to go back around when a directory refuses to be empty.
const RETRIES: usize = 4;

/// Whether the failure is one that a second look might survive.
///
/// ENOTEMPTY is the whole reason this exists: Finder rewrites `.DS_Store` into
/// any folder it is displaying or has recently indexed - `node_modules` very much
/// included - and it does it *while we delete*. We empty a directory, call
/// `rmdir`, and in the microseconds between the two Finder drops a fresh
/// `.DS_Store` in it. The directory is not empty any more, and `rmdir` says so.
fn worth_retrying(e: &io::Error) -> bool {
    // ENOTEMPTY is 66 on macOS and 39 on Linux; ENOENT means somebody else got
    // there first, which is a success wearing an error's coat.
    matches!(e.raw_os_error(), Some(66) | Some(39))
        || e.kind() == io::ErrorKind::PermissionDenied
}

/// Delete a tree, and mean it.
///
/// `std::fs::remove_dir_all` gives up on the first error and reports it against
/// the path it was *asked* about, not the one that failed. So a single `.DS_Store`
/// reappearing three levels down surfaced as "node_modules: Directory not empty"
/// - a message that is true, useless, and leaves 687 MB on the disk.
///
/// This walks the tree itself, re-reads any directory that refuses to empty, and
/// names the exact path when it really cannot proceed.
fn purge(path: &Path) -> Result<(), (PathBuf, io::Error)> {
    let md = match fs::symlink_metadata(path) {
        Ok(md) => md,
        // Already gone. That is the outcome we wanted.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err((path.to_path_buf(), e)),
    };

    // A symlink is unlinked, never followed - a `node_modules/next` pointing into
    // `.pnpm` must not take the store with it.
    if !md.is_dir() {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err((path.to_path_buf(), e)),
        };
    }

    for attempt in 0.. {
        let rd = match fs::read_dir(path) {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err((path.to_path_buf(), e)),
        };
        for entry in rd.flatten() {
            purge(&entry.path())?;
        }

        match fs::remove_dir(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) if worth_retrying(&e) && attempt + 1 < RETRIES => {
                // Something reappeared, or a mode bit is in the way. Make the
                // directory writable and read it again - the entry we just missed
                // will be in the next listing.
                if let Ok(md) = fs::metadata(path) {
                    let mut perm = md.permissions();
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        perm.set_mode(perm.mode() | 0o700);
                    }
                    let _ = fs::set_permissions(path, perm);
                }
            }
            Err(e) => return Err((path.to_path_buf(), e)),
        }
    }

    unreachable!("the loop either deletes or returns an error")
}

pub struct Options {
    pub yes: bool,
    pub dry_run: bool,
}

/// Deletes regenerable directories in place. Nothing is archived, nothing is
/// moved, and nothing goes to the Trash.
///
/// This is the command for projects you are still working on: it frees the disk
/// without taking the project away from you. `pnpm install` undoes it.
///
/// The Trash rule that governs `archive` does not apply here, and that is on
/// purpose. A project is irreplaceable; an artifact is the opposite by
/// construction - `Class::Confirmed` already means a lockfile governs the
/// directory *and* git corroborates that it is generated. Routing 44 GB of
/// `node_modules` through ~/.Trash would also free exactly zero bytes until the
/// user empties it, which defeats the only reason to run this.
pub fn run(projects: &[Project], opt: &Options) -> Result<u64> {
    let targets: Vec<(&Project, Vec<&crate::model::Artifact>)> = projects
        .iter()
        .map(|p| {
            let arts: Vec<_> = p
                .artifacts
                .iter()
                .filter(|a| a.class == Class::Confirmed)
                .collect();
            (p, arts)
        })
        .filter(|(_, a)| !a.is_empty())
        .collect();

    if targets.is_empty() {
        println!("\n  Nothing to clean up.\n");
        return Ok(0);
    }

    let labels: Vec<String> = targets.iter().map(|(p, _)| p.label()).collect();
    let w = labels
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 38);

    println!();
    for ((_, arts), label) in targets.iter().zip(&labels) {
        let size: u64 = arts.iter().map(|a| a.size).sum();
        let mut names: Vec<&str> = arts.iter().map(|a| a.rule.as_str()).collect();
        names.sort_unstable();
        names.dedup();

        let short: String = label.chars().take(w).collect();
        println!(
            "  {:>9}  {:<w$}  {}",
            human(size),
            short,
            names.join(", "),
            w = w
        );
    }

    let total: u64 = targets
        .iter()
        .flat_map(|(_, a)| a.iter())
        .map(|a| a.size)
        .sum();

    println!();
    println!("  {} projects · {} to be deleted", targets.len(), human(total));
    println!("  these are generated files - rebuildable from the lockfile.");
    println!("  the projects themselves stay in place; the source code is untouched.");

    let held: u64 = projects.iter().map(|p| p.review_size).sum();
    if held > 0 {
        println!(
            "  {} more, but I can't be sure so I'm leaving it alone (check with parc plan).",
            human(held)
        );
    }

    if opt.dry_run {
        println!("\n  --dry-run: nothing was deleted.\n");
        return Ok(0);
    }

    // Destructive, so it asks - every time, not just the first.
    if !opt.yes {
        print!("\n  continue? [y/N] ");
        io::stdout().flush()?;
        let mut s = String::new();
        io::stdin().read_line(&mut s)?;
        if !matches!(s.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("  cancelled.\n");
            return Ok(0);
        }
    }

    let mut freed = 0u64;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    for (p, arts) in &targets {
        // Re-check against the project as it is *now*, not as the preview saw it.
        // The confirmation prompt can sit for minutes, and a lockfile deleted or
        // source dropped into a `dist/` in that window would turn a decision that
        // was safe when it was made into a destructive one. Re-analysing is one
        // git call per project, paid once, right before the only irreversible act
        // in this command. Anything that is no longer Confirmed is left alone.
        let fresh = crate::scan::analyze(&p.path);
        let still: std::collections::HashSet<&str> = fresh
            .artifacts
            .iter()
            .filter(|a| a.class == Class::Confirmed)
            .map(|a| a.path.as_str())
            .collect();

        for a in arts {
            let path = p.path.join(&a.path);
            if !still.contains(a.path.as_str()) {
                skipped += 1;
                eprintln!(
                    "  skipped {} - state changed after the scan, no longer safe",
                    path.display()
                );
                continue;
            }
            match purge(&path) {
                Ok(()) => freed += a.size,
                Err((at, e)) => {
                    failed += 1;
                    eprintln!("  could not delete {}", path.display());
                    // The path that actually refused, which is rarely the one we
                    // were asked to delete.
                    eprintln!("    stuck at: {}", at.display());
                    eprintln!("    reason: {e}");
                    if worth_retrying(&e) {
                        eprintln!(
                            "    something is writing to this folder - could be an open Finder \
                             window or a running dev server."
                        );
                    }
                }
            }
        }
    }

    println!();
    println!("  {} freed.", human(freed));
    if skipped > 0 {
        println!("  {skipped} folders skipped - state had changed after the scan.");
    }
    if failed > 0 {
        println!("  {failed} folders could not be deleted.");
    }
    println!();
    Ok(freed)
}
