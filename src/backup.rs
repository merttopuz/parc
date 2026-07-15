use crate::archive;
use crate::model::{Project, Verdict};
use crate::render::human;

use age::secrecy::SecretString;
use anyhow::Result;
use std::path::PathBuf;

pub struct Options {
    pub out_dir: PathBuf,
    pub level: i32,
    /// Archive projects that came back REVIEW.
    pub force: bool,
    /// Refresh archives of projects already in the library.
    pub overwrite: bool,
    /// Report what would be archived; write nothing.
    pub dry_run: bool,
    pub encrypt: Option<SecretString>,
}

/// Archives every project it was pointed at, with the regenerable directories
/// left out - and changes nothing on disk.
///
/// This is the safe half of the tool. `archive` takes one project and takes it
/// away; this takes a tree of them and leaves every one of them exactly where it
/// was. There is no flag here that deletes, and no code path to one: a project
/// keeps working while a shrunken copy of it goes on the shelf.
///
/// `node_modules`, `dist` and `target` never enter the archive. Not to punish
/// them - because a lockfile rebuilds them, so storing them would be paying for
/// bytes that a single command regenerates.
pub fn run(projects: &[Project], opt: &Options) -> Result<u64> {
    if projects.is_empty() {
        println!("\n  No projects to archive.\n");
        return Ok(0);
    }

    let labels: Vec<String> = projects.iter().map(|p| p.label()).collect();
    let w = labels
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 38);

    println!();
    for (p, label) in projects.iter().zip(&labels) {
        let short: String = label.chars().take(w).collect();
        println!(
            "  {:>9}  {:<w$}  {} won't enter the archive",
            human(p.total_size),
            short,
            human(p.artifact_size),
            w = w
        );
    }

    let total: u64 = projects.iter().map(|p| p.total_size).sum();
    let stripped: u64 = projects.iter().map(|p| p.artifact_size).sum();

    println!();
    println!(
        "  {} projects · {} · {} of generated files won't be archived",
        projects.len(),
        human(total),
        human(stripped)
    );
    println!("  the projects themselves won't be touched.");

    if opt.dry_run {
        println!("\n  --dry-run: no archive was written.\n");
        return Ok(0);
    }

    let mut written = 0u64;
    let mut done = 0usize;
    let mut skipped: Vec<(String, String)> = Vec::new();

    for (p, label) in projects.iter().zip(&labels) {
        // One project needing a human must not abort the other forty. It is
        // reported at the end instead, where it can still be acted on.
        if p.verdict == Verdict::Review && !opt.force {
            skipped.push((label.clone(), format!("REVIEW - {}", p.reasons.join("; "))));
            continue;
        }

        println!();
        match archive::create(
            &p.path,
            &archive::Options {
                out_dir: opt.out_dir.clone(),
                level: opt.level,
                // Not a parameter. `backup` does not delete.
                delete: false,
                force: opt.force,
                overwrite: opt.overwrite,
                yes: true,
                encrypt: opt.encrypt.clone(),
            },
        ) {
            Ok(file) => {
                done += 1;
                written += std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
            }
            Err(e) => skipped.push((
                label.clone(),
                e.to_string().lines().next().unwrap_or("").to_string(),
            )),
        }
    }

    println!();
    // Counted, not inferred. Reporting `projects.len() - skipped.len()` is how
    // "2 projects archived" got printed over a library holding one archive.
    println!("  {done} projects archived · {}", human(written));
    println!("  {}", opt.out_dir.display());

    if !skipped.is_empty() {
        println!();
        println!("  skipped:");
        for (name, why) in &skipped {
            println!("    {name}  -  {why}");
        }
    }
    println!();

    Ok(written)
}
