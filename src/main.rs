use parc::{archive, backup, clean, crypto, render, scan, setup};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "parc",
    version,
    about = "Project Archive - analyze, shrink and archive dev projects"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan projects and report how much space can be saved. Touches nothing.
    Scan {
        /// Directories to scan (default: current directory)
        paths: Vec<PathBuf>,
        /// How many levels deep to search for projects
        #[arg(long, default_value_t = 4)]
        depth: usize,
        /// Hide projects below this size
        #[arg(long, default_value_t = 10)]
        min_mb: u64,
        /// Only projects untouched for N days
        #[arg(long)]
        older_than: Option<i64>,
        /// Show all, including small projects
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },

    /// For one project: what gets deleted, what gets archived, what is lost. Touches nothing.
    Plan {
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },

    /// Delete generated files in place (node_modules, dist…). Doesn't archive or move the project.
    Clean {
        /// Projects or directories that contain projects (default: current directory)
        paths: Vec<PathBuf>,
        #[arg(long, default_value_t = 4)]
        depth: usize,
        /// Only projects untouched for N days
        #[arg(long)]
        older_than: Option<i64>,
        /// Show what would be deleted, don't delete
        #[arg(long)]
        dry_run: bool,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
    },

    /// Restore what clean deleted: runs the install for missing node_modules/vendor/….
    #[command(visible_alias = "install")]
    Setup {
        /// Projects or directories that contain projects (default: current directory)
        paths: Vec<PathBuf>,
        #[arg(long, default_value_t = 4)]
        depth: usize,
        /// Also run optional build steps (cargo build, gradlew build…)
        #[arg(long)]
        build: bool,
        /// Show what would run, don't run
        #[arg(long)]
        dry_run: bool,
    },

    /// Archive projects in bulk. Doesn't touch the projects - just shelves a copy.
    Backup {
        /// Projects or directories that contain projects (default: current directory)
        paths: Vec<PathBuf>,
        #[arg(long, default_value_t = 4)]
        depth: usize,
        /// Only projects untouched for N days
        #[arg(long)]
        older_than: Option<i64>,
        /// Directory to write archives to (default: ~/Archives, PARC_ARCHIVE_DIR)
        #[arg(long)]
        out: Option<PathBuf>,
        /// zstd compression level (1-19)
        #[arg(long, default_value_t = 12)]
        level: i32,
        /// Also archive the ones in REVIEW
        #[arg(long)]
        force: bool,
        /// Refresh archives for projects already in the library
        #[arg(long)]
        overwrite: bool,
        /// Show what would be archived, don't write
        #[arg(long)]
        dry_run: bool,
        /// Encrypt archives with a passphrase (age)
        #[arg(long)]
        encrypt: bool,
    },

    /// Archive the project, verify it, and move the original to the Trash. (Asks first.)
    Archive {
        path: PathBuf,
        /// Directory to write the archive to (default: ~/Archives, PARC_ARCHIVE_DIR)
        #[arg(long)]
        out: Option<PathBuf>,
        /// zstd compression level (1-19)
        #[arg(long, default_value_t = 12)]
        level: i32,
        /// Archive even if it's in REVIEW
        #[arg(long)]
        force: bool,
        /// Overwrite this project's existing archive in the library
        #[arg(long)]
        overwrite: bool,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
        /// Encrypt the archive with a passphrase (age). Lose the passphrase and the project is gone.
        #[arg(long)]
        encrypt: bool,
        /// Now the default - kept so old invocations don't break
        #[arg(long, hide = true)]
        delete: bool,
    },

    /// Check an archive's integrity: recompute each file's sha256.
    Verify {
        /// Archive name (as in parc list) or file path
        path: PathBuf,
    },

    /// What this archive was: what was deleted, what it ran on, what it takes to bring it back.
    #[command(visible_alias = "info")]
    Show {
        /// Archive name (as in parc list) or file path
        path: PathBuf,
    },

    /// Restore the archive. Rebuilds the removed files itself (install, generate, pod install…).
    Restore {
        /// Archive name (as in parc list) or file path
        path: PathBuf,
        /// Put it back in its original location without asking
        #[arg(long, conflicts_with = "into")]
        original: bool,
        /// Extract under this directory without asking
        #[arg(long)]
        into: Option<PathBuf>,
        /// Only extract the files; don't run the restore steps
        #[arg(long, conflicts_with = "setup")]
        no_setup: bool,
        /// Run the restore steps (already the default - kept so old invocations don't break)
        #[arg(long, alias = "install", hide = true)]
        setup: bool,
        /// Run the restore commands without asking (for scripts/pipelines)
        #[arg(long)]
        yes: bool,
    },

    /// List the archive library.
    List {
        #[arg(long)]
        dir: Option<PathBuf>,
    },

    /// Remove the archive from the library. The file is moved to the Trash, not deleted.
    #[command(visible_alias = "remove")]
    Rm {
        /// Archive names (as in parc list) or file paths
        #[arg(required = true)]
        names: Vec<PathBuf>,
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Also remove an archive whose original isn't on disk - i.e. the only copy
        #[arg(long)]
        force: bool,
        /// Don't ask for confirmation
        #[arg(long)]
        yes: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Scan {
            paths,
            depth,
            min_mb,
            older_than,
            all,
            json,
        } => {
            let mut projects = collect(paths, depth, older_than, !json)?;
            if !all {
                let floor = min_mb * 1024 * 1024;
                projects.retain(|p| p.total_size >= floor);
            }

            if json {
                println!("{}", serde_json::to_string_pretty(&projects)?);
            } else {
                render::scan_table(&projects);
            }
        }

        Cmd::Clean {
            paths,
            depth,
            older_than,
            dry_run,
            yes,
        } => {
            let projects = collect(paths, depth, older_than, true)?;
            clean::run(&projects, &clean::Options { yes, dry_run })?;
        }

        Cmd::Setup {
            paths,
            depth,
            build,
            dry_run,
        } => {
            // Deliberately not `collect()`: that analyzes every project, and
            // analysis measures artifacts that - the whole point here - are gone.
            let roots = if paths.is_empty() {
                vec![std::env::current_dir()?]
            } else {
                paths
            };
            for r in &roots {
                if !r.is_dir() {
                    bail!("not a directory: {}", r.display());
                }
            }
            let found = scan::discover(&roots, depth);
            if found.is_empty() {
                bail!("no projects found - try increasing --depth");
            }
            setup::run(&found, &setup::Options { build, dry_run })?;
        }

        Cmd::Backup {
            paths,
            depth,
            older_than,
            out,
            level,
            force,
            overwrite,
            dry_run,
            encrypt,
        } => {
            if !(1..=19).contains(&level) {
                bail!("--level must be between 1 and 19");
            }
            let projects = collect(paths, depth, older_than, true)?;
            let encrypt = if encrypt && !dry_run {
                Some(crypto::passphrase(true)?)
            } else {
                None
            };
            backup::run(
                &projects,
                &backup::Options {
                    out_dir: out.unwrap_or_else(archive::default_dir),
                    level,
                    force,
                    overwrite,
                    dry_run,
                    encrypt,
                },
            )?;
        }

        Cmd::Plan { path, json } => {
            let proj = scan::analyze(&canon_dir(&path)?);
            if json {
                println!("{}", serde_json::to_string_pretty(&proj)?);
            } else {
                render::plan_detail(&proj);
            }
        }

        Cmd::Archive {
            path,
            out,
            level,
            force,
            overwrite,
            yes,
            encrypt,
            delete: _,
        } => {
            if !(1..=19).contains(&level) {
                bail!("--level must be between 1 and 19");
            }
            // Resolved before the passphrase prompt: being asked to type a secret
            // twice and *then* told the path was a typo is a bad trade.
            let root = canon_dir(&path)?;
            // Asked for before a single byte is read: a passphrase typo must not
            // surface after twenty minutes of compression.
            let encrypt = if encrypt {
                Some(crypto::passphrase(true)?)
            } else {
                None
            };
            archive::create(
                &root,
                &archive::Options {
                    out_dir: out.unwrap_or_else(archive::default_dir),
                    level,
                    // This command's whole point: the project goes away. Keeping
                    // it is what `parc backup` is for.
                    delete: true,
                    force,
                    overwrite,
                    yes,
                    encrypt,
                },
            )?;
        }

        Cmd::Verify { path } => {
            let file = archive::resolve(&path)?;
            let pass = unlock(&file)?;
            let rep = archive::verify(&file, pass.as_ref())?;
            let mf = rep.manifest.as_ref().expect("verify guarantees the manifest");
            println!();
            println!("  {}  ({})", mf.name, mf.created_at_iso);
            if rep.ok() {
                println!("  {} files · sha256 verified · SOUND", rep.checked);
            } else {
                println!("  CORRUPT");
                print!("{}", rep.problems());
                std::process::exit(1);
            }
            println!();
        }

        Cmd::Show { path } => {
            let file = archive::resolve(&path)?;
            let pass = unlock(&file)?;
            let mf = archive::read_manifest(&file, pass.as_ref())?;
            render::archive_info(&file, &mf);
        }

        Cmd::Restore {
            path,
            original,
            into,
            no_setup,
            setup: _,
            yes,
        } => {
            let target = match (original, into) {
                (true, _) => archive::Target::Original,
                (_, Some(d)) => archive::Target::Into(d),
                _ => archive::Target::Ask,
            };
            let file = archive::resolve(&path)?;
            let pass = unlock(&file)?;
            // Rebuilding what we removed is the default: an archive you have to
            // reverse-engineer before it runs is a puzzle, not a backup.
            archive::restore(
                &file,
                target,
                archive::Setup {
                    run: !no_setup,
                    yes,
                },
                pass.as_ref(),
            )?;
        }

        Cmd::List { dir } => {
            let dir = dir.unwrap_or_else(archive::default_dir);
            let items = archive::list(&dir)?;
            render::archive_list(&dir, &items);
        }

        Cmd::Rm {
            names,
            dir,
            force,
            yes,
        } => {
            let dir = dir.unwrap_or_else(archive::default_dir);
            archive::remove(&dir, &names, &archive::RemoveOptions { force, yes })?;
        }
    }

    Ok(())
}

/// Ask for the passphrase only when the archive in hand actually needs one.
///
/// By the age magic in the file, not by the `.age` on its name - which is what
/// `list` and `rm` have always done, and what this was the last place still
/// getting wrong. An encrypted archive that lost its suffix (a rename, a sync
/// client, a trip through a zip) became unopenable: `crypto::open` sniffs the
/// bytes, saw ciphertext, and asked for a passphrase that was never collected
/// because the *name* said there was nothing to unlock. `list` printed the file as
/// 🔒 encrypted and told you to run `parc show` on it; `parc show` then refused it -
/// with the right passphrase sitting in the environment. `rm` meanwhile guarded it
/// as a possible last copy. So the tool protected a file it could no longer open.
fn unlock(file: &std::path::Path) -> Result<Option<age::secrecy::SecretString>> {
    if crypto::is_encrypted_content(file) {
        return Ok(Some(crypto::passphrase(false)?));
    }
    Ok(None)
}

fn canon_dir(p: &std::path::Path) -> Result<PathBuf> {
    if !p.is_dir() {
        bail!("not a directory: {}", p.display());
    }
    Ok(p.canonicalize()?)
}

/// Discover and analyze. `scan` and `clean` must see exactly the same set of
/// projects, or "44 GB will be freed" and "44 GB was freed" stop matching.
fn collect(
    paths: Vec<PathBuf>,
    depth: usize,
    older_than: Option<i64>,
    progress: bool,
) -> Result<Vec<parc::model::Project>> {
    let roots = if paths.is_empty() {
        vec![std::env::current_dir()?]
    } else {
        paths
    };
    // Absolute from here on. `parc backup .` used to record `./foo` as the
    // project's home, and a relative path in a manifest is not an address: it is
    // whatever the next shell happens to be sitting in. `restore --original` put
    // the project somewhere else, and `rm` decided the original was gone and
    // called the archive the last copy.
    let roots: Vec<PathBuf> = roots
        .iter()
        .map(|r| canon_dir(r))
        .collect::<Result<Vec<_>>>()?;

    let found = scan::discover(&roots, depth);
    if found.is_empty() {
        bail!("no projects found - try increasing --depth");
    }
    if progress {
        eprintln!("  {} projects found, analyzing…", found.len());
    }

    let mut projects = scan::analyze_all(&found, progress);
    if let Some(days) = older_than {
        projects.retain(|p| p.age_days().is_some_and(|d| d >= days));
    }
    Ok(projects)
}
