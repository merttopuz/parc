use crate::recipe::{self, Step};

use anyhow::Result;
use std::path::{Path, PathBuf};

pub struct Options {
    /// Also run the optional steps: `cargo build`, `./gradlew build`, and the
    /// rest of the things that take minutes and are not needed to have a working
    /// checkout.
    pub build: bool,
    /// Say what would run; run nothing.
    pub dry_run: bool,
}

/// Puts back what `clean` took away - across every project it is pointed at.
///
/// `clean` deletes and keeps no journal, and it does not need one: the evidence
/// is in the project. A lockfile with no `node_modules` beside it is a project
/// that was installed once and is not installed now, and the same command that
/// installed it then installs it again. That reasoning survives a reboot, a
/// `parc` upgrade, and projects cleaned long before this command existed.
pub fn run(roots: &[PathBuf], opt: &Options) -> Result<usize> {
    let jobs: Vec<(PathBuf, Vec<Step>)> = roots
        .iter()
        .map(|r| (r.clone(), recipe::needed(r)))
        .filter(|(_, steps)| !steps.is_empty())
        .collect();

    if jobs.is_empty() {
        println!("\n  Nothing to install - nothing looks missing in the projects.\n");
        return Ok(0);
    }

    println!();
    for (root, steps) in &jobs {
        println!("  {}", label(root));
        for s in steps {
            let at = s
                .dir
                .as_deref()
                .map(|d| format!("({d}) "))
                .unwrap_or_default();
            // The directory prefix is part of the command column, not a thing
            // that shoves it sideways.
            println!(
                "    {:<52} {}{}",
                format!("{at}{}", s.cmd),
                s.why,
                if s.optional { "  (optional)" } else { "" }
            );
        }
    }

    let required: usize = jobs
        .iter()
        .flat_map(|(_, s)| s.iter())
        .filter(|s| !s.optional)
        .count();

    println!();
    println!("  {} projects · {} install steps", jobs.len(), required);
    if !opt.build {
        println!("  optional build steps will be skipped (run with --build).");
    }

    if opt.dry_run {
        println!("\n  --dry-run: no command was run.\n");
        return Ok(0);
    }

    let mut done = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();

    for (root, steps) in &jobs {
        println!();
        println!("  {}", label(root));
        // One project that will not install must not stop the other nine. It is
        // named at the end, where it can still be dealt with.
        match recipe::execute(root, steps, opt.build) {
            None => done += 1,
            Some(i) => failed.push((label(root), steps[i].cmd.clone())),
        }
    }

    println!();
    println!("  {done} projects installed.");
    if !failed.is_empty() {
        println!();
        println!("  could not install:");
        for (name, cmd) in &failed {
            println!("    {name}  -  `{cmd}` failed");
        }
        println!();
        println!("  Nothing lost in the source code: only the dependencies failed to install.");
    }
    println!();

    Ok(done)
}

fn label(p: &Path) -> String {
    let parts: Vec<_> = p
        .components()
        .rev()
        .take(2)
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}
