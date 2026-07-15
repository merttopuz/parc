use crate::crypto::{self, ENC_EXT};
use crate::manifest::*;
use crate::model::{Class, Verdict};
use crate::render::{human, plan_block, removed_block};
use crate::scan;

use age::secrecy::SecretString;
use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, IsTerminal, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const EXT: &str = ".parc.tar.zst";

pub fn default_dir() -> PathBuf {
    std::env::var_os("PARC_ARCHIVE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join("Archives")
        })
}

/// Accepts whatever `parc list` printed. That command shows names, so a name is
/// what people type - making them retype it as a path was a bug in this tool,
/// not a mistake on their part.
pub fn resolve(input: &Path) -> Result<PathBuf> {
    resolve_in(&default_dir(), input)
}

/// Same, against a library the caller names - `list` and `rm` both take `--dir`,
/// and looking a name up somewhere other than where it was just listed would
/// resolve to the wrong file or to nothing.
pub fn resolve_in(dir: &Path, input: &Path) -> Result<PathBuf> {
    if input.is_file() {
        return Ok(input.to_path_buf());
    }

    let raw = input.to_string_lossy();
    let stem = raw
        .strip_suffix(ENC_EXT)
        .or_else(|| raw.strip_suffix(EXT))
        .unwrap_or(&raw);

    // A name resolves whether or not the archive behind it is encrypted - that
    // is a property of the file, not something the user should have to remember.
    for cand in [
        dir.join(format!("{stem}{EXT}")),
        dir.join(format!("{stem}{ENC_EXT}")),
        dir.join(&*raw),
    ] {
        if cand.is_file() {
            return Ok(cand);
        }
    }

    let known = list(dir).unwrap_or_default();

    // The name carries a path fingerprint (`app-backend-a3f19c`) so that two
    // projects called `app/backend` cannot be the same file. Nobody should have
    // to type six hex digits for that: the label on its own resolves, as long as
    // it picks out exactly one archive.
    let matches: Vec<&Item> = known.iter().filter(|i| labelled(&i.name(), stem)).collect();
    match matches.as_slice() {
        [only] => return Ok(only.file.clone()),
        [] => {}
        many => bail!(
            "more than one archive matches `{}` - which one?\n{}",
            stem,
            many.iter()
                .map(|i| format!("    {}", i.name()))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }

    if known.is_empty() {
        bail!(
            "archive not found: {}\n  no archives in {}.",
            raw,
            dir.display()
        );
    }
    bail!(
        "archive not found: {}\n  contents of {}:\n{}",
        raw,
        dir.display(),
        known
            .iter()
            .map(|i| format!("    {}", i.name()))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Is `name` this archive's full name minus its path fingerprint?
fn labelled(name: &str, stem: &str) -> bool {
    name.strip_prefix(stem)
        .and_then(|s| s.strip_prefix('-'))
        .is_some_and(|fp| fp.len() == 6 && fp.chars().all(|c| c.is_ascii_hexdigit()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hash_file(p: &Path) -> Result<(String, u64)> {
    let mut f = BufReader::new(File::open(p)?);
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut n = 0u64;
    loop {
        let r = f.read(&mut buf)?;
        if r == 0 {
            break;
        }
        h.update(&buf[..r]);
        n += r as u64;
    }
    Ok((hex(&h.finalize()), n))
}

/// One entry of the tree, stamped with enough to notice it changing under us.
///
/// The size and mtime are not written to the manifest - they exist so the same
/// walk can be run twice and the two results compared. See `drift`.
#[derive(PartialEq, Eq)]
enum Kind {
    File { size: u64, mtime: i64, mtime_nsec: i64 },
    Dir,
    Link(String),
}

/// Files that Finder and friends rewrite behind everyone's back, and that nobody
/// would notice missing. Excluded from the drift check only: a `.DS_Store` landing
/// in the tree mid-archive must not be what stops a project being archived, and
/// losing one costs nothing.
const DRIFT_NOISE: &[&str] = &[".DS_Store", "Thumbs.db"];

/// Ceiling on the manifest read. The manifest is the first tar entry and is read
/// whole into memory - `list` does it for every archive in the library - so a
/// crafted archive whose first entry is a multi-gigabyte blob would OOM the one
/// command you'd run to survey what is still intact. A real manifest is JSON with
/// roughly one short line per file; 512 MiB covers a project of several million
/// files and stops a hostile entry well short of exhausting memory.
const MANIFEST_MAX: u64 = 512 << 20;

/// Everything that goes into the archive: the project tree minus the
/// directories `analyze` was willing to call regenerable. Pruning is by exact
/// relative path, not by name - a `dist/` classified REVIEW is kept even though
/// another `dist/` in the same project may be dropped.
///
/// A `BTreeMap`, so two walks of the same tree are directly comparable and come
/// out in the same order.
fn collect(root: &Path, drop: &HashSet<String>) -> Result<BTreeMap<String, Kind>> {
    let mut out = BTreeMap::new();
    let mut queue = vec![root.to_path_buf()];
    let mut specials: Vec<String> = Vec::new();

    while let Some(dir) = queue.pop() {
        // Not `.flatten()`: a per-entry read error must abort the archive, not be
        // silently skipped. This same walk builds the archive *and* the drift
        // comparison, so an entry it quietly dropped would be missing from both
        // sides - trashed with the project and never noticed gone.
        for entry in fs::read_dir(&dir)? {
            let e = entry?;
            let path = e.path();
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();
            if drop.contains(&rel) {
                continue;
            }
            let md = fs::symlink_metadata(&path)?;
            if md.is_symlink() {
                let target = fs::read_link(&path)?.to_string_lossy().to_string();
                out.insert(rel, Kind::Link(target));
            } else if md.is_dir() {
                out.insert(rel, Kind::Dir);
                queue.push(path);
            } else if md.is_file() {
                out.insert(
                    rel,
                    Kind::File {
                        size: md.len(),
                        mtime: md.mtime(),
                        // Whole-second mtime lets a file rewritten in place within
                        // the same second, at the same length, slip past the drift
                        // check and be trashed with a stale copy in the archive.
                        // The nanoseconds close that window.
                        mtime_nsec: md.mtime_nsec(),
                    },
                );
            } else {
                // A FIFO, a socket, a device node. `File::open` on a FIFO with no
                // writer blocks inside `open(2)` and never returns, which is how a
                // single `mkfifo` in a project tree hung `parc backup` forever and
                // took the rest of the nightly run with it.
                //
                // None of them carry content - a FIFO is a rendezvous point, not a
                // file - so there is nothing to lose by leaving them out, and we
                // say so rather than pretending the archive is a perfect copy.
                specials.push(rel);
            }
        }
    }

    if !specials.is_empty() {
        specials.sort();
        eprintln!(
            "  ! {} special files not archived (FIFO/socket/device - no contents):",
            specials.len()
        );
        for s in specials.iter().take(5) {
            eprintln!("      {s}");
        }
        if specials.len() > 5 {
            eprintln!("      … and {} more", specials.len() - 5);
        }
    }

    Ok(out)
}

/// Is the tree still the tree we archived?
///
/// The manifest and the tar are both built from one walk, and `verify` compares
/// them to each other. That proves the archive is internally consistent. It cannot
/// prove the archive is *complete*, because anything the walk never saw is missing
/// from both sides of the comparison and so cancels out - and `archive`'s next act
/// is to throw the original away on the strength of that proof.
///
/// An editor autosaving, a dev server writing, a sync client pulling, a background
/// `git` - any of them, during a compression that can run for twenty minutes, is
/// invisible to it. Measured: twenty-five files written during one run, of which
/// the archive held **one**. `parc verify` said SOUND. The other twenty-four went
/// to the Trash with the project.
///
/// So the tree is walked a second time, after the archive is on disk, and compared
/// with what we actually archived. The project is only allowed to move if it is
/// still the project in the archive.
fn drift(
    root: &Path,
    drop: &HashSet<String>,
    before: &BTreeMap<String, Kind>,
    hashes: &HashMap<&str, &str>,
) -> Result<()> {
    let after = collect(root, drop)?;

    let noise = |rel: &String| {
        Path::new(rel)
            .file_name()
            .is_some_and(|n| DRIFT_NOISE.iter().any(|x| n == *x))
    };

    let mut added: Vec<&String> = Vec::new();
    let mut changed: Vec<&String> = Vec::new();
    let mut gone: Vec<&String> = Vec::new();

    for (rel, kind) in &after {
        if noise(rel) {
            continue;
        }
        match before.get(rel) {
            None => added.push(rel),
            Some(was) if was != kind => changed.push(rel),
            Some(_) => {
                // Size and mtime match, but a filesystem with coarse timestamps
                // (FAT and exFAT report whole-second mtimes with a zero nanosecond
                // field, and an external archive drive is exactly that) hides a
                // same-length in-place rewrite from the check above. Where the
                // nanosecond field is zero, fall back to content: re-hash the file
                // and compare it against what the archive actually stored. A file
                // readable at archive time but not now is drift too, not something
                // to trash unverified.
                if let Kind::File { mtime_nsec: 0, .. } = kind {
                    if let Some(want) = hashes.get(rel.as_str()) {
                        match hash_file(&root.join(rel)) {
                            Ok((have, _)) if &have != want => changed.push(rel),
                            Ok(_) => {}
                            Err(_) => changed.push(rel),
                        }
                    }
                }
            }
        }
    }
    for rel in before.keys() {
        if !noise(rel) && !after.contains_key(rel) {
            gone.push(rel);
        }
    }

    if added.is_empty() && changed.is_empty() && gone.is_empty() {
        return Ok(());
    }

    let list = |label: &str, v: &[&String]| {
        if v.is_empty() {
            return String::new();
        }
        let mut s = format!("\n  {label} ({}):", v.len());
        for rel in v.iter().take(5) {
            s.push_str(&format!("\n      {rel}"));
        }
        if v.len() > 5 {
            s.push_str(&format!("\n      … and {} more", v.len() - 5));
        }
        s
    };

    bail!(
        "project changed while archiving - not deleting.{}{}{}\n\n  \
         The archive was written and verified, but it is no longer the whole project: the items \
         above were left outside it.\n  \
         The project was left untouched. Close whatever is writing (editor, dev server, sync) and \
         run again with `--overwrite`.",
        list("added after archiving", &added),
        list("changed after archiving", &changed),
        list("deleted after archiving", &gone),
    )
}

pub struct Options {
    pub out_dir: PathBuf,
    pub level: i32,
    pub delete: bool,
    /// Archive a project that came back REVIEW.
    pub force: bool,
    /// Replace an archive of *this same project* that is already there.
    ///
    /// Deliberately not the same flag as `force`. Every package of a monorepo is
    /// REVIEW - its `.git` is one level up - so `--force` is the flag people
    /// reach for constantly, and quietly handing it the power to overwrite is
    /// what let one archive land on top of another.
    pub overwrite: bool,
    pub yes: bool,
    /// Some(passphrase) writes an age-encrypted archive. Opt-in, per archive:
    /// most projects do not need it, and the ones that do cannot be told apart
    /// automatically.
    pub encrypt: Option<SecretString>,
}

/// The archive's name: the human label, plus a fingerprint of the path it came
/// from.
///
/// The label alone is the last two path components, which is what makes eight
/// projects called `backend` readable - and what made `acme/app/backend` and
/// `globex/app/backend` the same file. One overwrote the other, `backup` printed
/// "2 projects archived", and the library held one.
///
/// The fingerprint is of the *path*, not the contents or the time, so it is
/// stable: re-archiving a project rewrites its own archive rather than piling up
/// a new one beside it, and `parc restore app-backend-a3f19c` names the same
/// thing next year that it names today.
pub fn name_for(root: &Path, label: &str) -> String {
    let mut h = Sha256::new();
    h.update(root.to_string_lossy().as_bytes());
    let fp: String = hex(&h.finalize()).chars().take(6).collect();
    format!("{}-{fp}", label.replace('/', "-"))
}

/// An absolute, symlink-resolved form of `p` - one that does not require `p` to
/// exist yet.
///
/// `canonicalize` alone cannot do this: it fails outright on a directory we have
/// not created, and the fallback that papered over that returned the path *as
/// given*. So a relative `--out backups` stayed relative, compared false against
/// an absolute project root, and walked straight through the guard below - the
/// one guard whose entire job is to stop the archive being written inside the
/// tree that `archive` is about to move to the Trash.
///
/// The deepest ancestor that *does* exist is canonicalized and the rest
/// re-attached, which is also what resolves `/tmp` to `/private/tmp` and settles
/// any `..` in the middle.
fn absolutize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| p.to_path_buf(), |cwd| cwd.join(p))
    };

    let mut tail: Vec<OsString> = Vec::new();
    let mut cur = abs.as_path();
    loop {
        if let Ok(real) = cur.canonicalize() {
            return tail.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (cur.parent(), cur.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                cur = parent;
            }
            // Nothing on the way up exists. Absolute is still better than not.
            _ => return abs,
        }
    }
}

pub fn create(root: &Path, opt: &Options) -> Result<PathBuf> {
    let proj = scan::analyze(root);

    // An archive written inside the project it is an archive of is not a backup.
    // `parc archive x --out x/backups` put the file in the tree and then moved
    // the tree - archive included - to the Trash; `parc backup . --out ./backups`
    // made every run swallow the previous archive into the next one.
    let out = absolutize(&opt.out_dir);
    let here = root.canonicalize();
    let here = here.as_deref().unwrap_or(root);
    if out.starts_with(here) {
        bail!(
            "archive directory is inside the project: {}\n  \
             the archive would be deleted along with the project. use --out to give a location outside it.",
            out.display()
        );
    }

    if proj.verdict == Verdict::Review && !opt.force {
        bail!(
            "{} is REVIEW - refusing to archive automatically.\n  \
             look with `parc plan {}`, and if nothing looks wrong try again with --force.\n  \
             reason: {}",
            proj.label(),
            root.display(),
            proj.reasons.join("; ")
        );
    }

    // Where this archive will land, decided now - the name is a function of the
    // path and the label, and both are already in hand. Everything that can refuse
    // has to refuse here, before the project is hashed and before anyone is asked
    // whether it may be thrown away.
    let name = name_for(root, &proj.label());
    let (ext, other) = if opt.encrypt.is_some() {
        (ENC_EXT, EXT)
    } else {
        (EXT, ENC_EXT)
    };
    let final_path = out.join(format!("{name}{ext}"));

    // The same project archived once plain and once with `--encrypt` writes two
    // *files* that every other command reduces to one *name*: `list` printed two
    // rows both called `app-backend-a3f19c`, `resolve` silently preferred the
    // plaintext one, and `rm` could only ever reach that one. The encrypted
    // archive became a ghost - sitting in the library, counted in its totals, and
    // unreachable by the only name anybody had for it.
    //
    // So a project has one archive, however it happens to be encrypted. The old
    // one is replaced, not left beside the new one.
    let twin = out.join(format!("{name}{other}"));
    let twin = twin.exists().then_some(twin);

    if !opt.overwrite {
        if let Some(t) = &twin {
            bail!(
                "an archive of this project already exists ({}):\n  {}\n  \
                 --overwrite replaces it - the old one goes to the Trash.",
                if opt.encrypt.is_some() {
                    "unencrypted"
                } else {
                    "encrypted"
                },
                t.display()
            );
        }
        if final_path.exists() {
            bail!(
                "already exists: {}\n  --overwrite to write over it",
                final_path.display()
            );
        }
    }

    // The net under the name. `name_for` makes a collision between two different
    // projects essentially impossible - but "essentially" is not the standard for
    // a file that may be the last copy of someone's work. If what is sitting there
    // came from somewhere else, we do not write over it, whatever flags we were
    // given.
    if final_path.exists() {
        if let Ok(old) = read_manifest(&final_path, opt.encrypt.as_ref()) {
            let mine = root.display().to_string();
            if old.original_path != mine {
                bail!(
                    "there is another project's archive here - not overwriting it\n  \
                     {}\n  that archive: {}\n  this project: {}\n  \
                     give a different --out, or remove it first with `parc rm`.",
                    final_path.display(),
                    old.original_path,
                    mine
                );
            }
        }
    }

    // Asked before the work, not after it. This command takes the project away;
    // finding that out at the end of a twenty-minute compression - with the only
    // remaining question being "shall I also delete it?" - is not a choice, it is
    // an ambush.
    if opt.delete && !opt.yes {
        let q = format!(
            "\n  {} ({})\n  will be archived, verified, and the original moved to the Trash.\n  continue?",
            proj.label(),
            human(proj.total_size)
        );
        if !confirm(&q)? {
            bail!("cancelled - nothing was done");
        }
    }

    let drop: HashSet<String> = proj
        .artifacts
        .iter()
        .filter(|a| a.class == Class::Confirmed)
        .map(|a| a.path.clone())
        .collect();

    let removed: Vec<Removed> = proj
        .artifacts
        .iter()
        .filter(|a| a.class == Class::Confirmed)
        .map(|a| Removed {
            path: a.path.clone(),
            size: a.size,
            rule: a.rule.clone(),
        })
        .collect();

    eprintln!("  {}", proj.label());
    eprintln!("  collecting files…");
    let entries = collect(root, &drop)?;

    eprintln!("  hashing…");
    let mut files = Vec::new();
    let mut links = Vec::new();
    let mut archived_size = 0u64;
    for (rel, kind) in &entries {
        match kind {
            Kind::File { .. } => {
                let (sha256, size) = hash_file(&root.join(rel))
                    .with_context(|| format!("could not read: {rel}"))?;
                archived_size += size;
                files.push(FileEntry {
                    path: rel.clone(),
                    size,
                    sha256,
                });
            }
            Kind::Link(target) => links.push(LinkEntry {
                path: rel.clone(),
                target: target.clone(),
            }),
            Kind::Dir => {}
        }
    }

    let ts = now();
    let mf = Manifest {
        format_version: FORMAT_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_string(),
        name: name.clone(),
        created_at: ts,
        created_at_iso: iso(ts),
        original_path: root.display().to_string(),
        stacks: proj.stacks.clone(),
        package_manager: proj.package_manager.clone(),
        git: proj.git.clone(),
        restore_plan: crate::recipe::plan(root, &proj),
        runtime: crate::recipe::runtime(root),
        original_size: proj.total_size,
        archived_size,
        removed,
        files,
        links,
    };

    // `out`, not `opt.out_dir`: the guard above cleared *that* path, and a
    // relative one would otherwise be re-resolved against the working directory a
    // second time here.
    fs::create_dir_all(&out)?;

    // Write to a sidecar first: a half-written archive must never be able to
    // sit at the real path where a later run would trust it.
    let tmp_path = final_path.with_extension("partial");

    if opt.encrypt.is_some() {
        eprintln!("  compressing (zstd -{}) and encrypting (age)…", opt.level);
    } else {
        eprintln!("  compressing (zstd -{})…", opt.level);
    }
    write_tar(root, &entries, &mf, &tmp_path, opt.level, opt.encrypt.as_ref())?;

    eprintln!("  verifying…");
    // Verified by decrypting it again, not by trusting what we just wrote. A
    // passphrase that cannot open the archive has to fail *here*, while the
    // project is still on disk.
    let report = verify(&tmp_path, opt.encrypt.as_ref())?;
    if !report.ok() {
        let _ = fs::remove_file(&tmp_path);
        bail!(
            "archive verification FAILED - archive deleted, original untouched\n{}",
            report.problems()
        );
    }

    // Whatever this archive replaces - the previous archive of the same project at
    // the same name, or its other-encryption twin - is dealt with here, and only
    // here, with the replacement already on disk and every byte of it re-hashed.
    //
    // `rename(2)` straight over the old file is what this used to be, and it is the
    // one place the tool broke its own most-repeated rule: an archive can be the
    // last copy of a project, so an archive is never unlinked, it is moved to the
    // Trash. A weekly `parc backup --overwrite` cron ran once against a project
    // whose `src/` had been wiped off the disk, and a good forty-two-file archive
    // was replaced by a two-file one. The good one was not in the Trash. It was
    // nowhere.
    let replaced: Vec<PathBuf> = [twin, final_path.exists().then(|| final_path.clone())]
        .into_iter()
        .flatten()
        .collect();

    for old in &replaced {
        // Fewer files than last time is not necessarily wrong - you are allowed to
        // delete things - but it is the exact shape of the disaster above, so it is
        // said out loud and the old archive is kept where it can be fetched back.
        let shrank = read_manifest(old, opt.encrypt.as_ref())
            .ok()
            .filter(|prev| mf.files.len() < prev.files.len())
            .map(|prev| (prev.files.len(), mf.files.len()));

        if let Some((was, now)) = shrank {
            println!();
            println!("  ! this archive is smaller than the last: {was} files → {now} files.");
            println!("    the old one is in the Trash - if something happened to the project that shouldn't have, get it from there.");
        }

        // The new archive is still a `.partial` here and the project has not been
        // touched, so a failure can put the disk back exactly as it was - and has
        // to, because the alternative is what this used to do. On a library that is
        // not on the boot volume the trash move failed, and the finished archive was
        // left orphaned under a `.partial` name that `list` does not show, while the
        // library went on presenting the *stale* archive as current - a weekly
        // `backup --overwrite` cron refreshed nothing and said so nowhere. The error
        // then named the path holding the old archive as "the new archive", and told
        // the user to delete that path by hand.
        let dest = match move_to_trash(old) {
            Ok(dest) => dest,
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(e).with_context(|| {
                    format!(
                        "could not remove the old archive from the library - new archive not written, \
                         project untouched.\n  the old archive is still there as it was: {}",
                        old.display()
                    )
                });
            }
        };
        println!("  old archive moved to the Trash: {}", dest.display());
    }

    fs::rename(&tmp_path, &final_path)?;
    // The archive's bytes went to the disk in `write_tar`; this is what puts its
    // *name* there. Both are true from here on, which is the precondition for the
    // only irreversible thing this function does - moving the project to the Trash,
    // some fifteen lines below.
    sync_dir(&out)?;

    let compressed = fs::metadata(&final_path)?.len();

    println!();
    println!("  archive:    {}", final_path.display());
    println!(
        "  {} → {}  ({:.0}% smaller)",
        human(proj.total_size),
        human(compressed),
        (1.0 - compressed as f64 / proj.total_size.max(1) as f64) * 100.0
    );
    println!(
        "  {} files verified (sha256)",
        report.checked
    );

    // What is *not* in the archive is the thing worth being told, and the moment
    // to object to it is now - while the project is still on disk.
    println!();
    removed_block(&mf);
    if !mf.restore_plan.is_empty() {
        println!();
        plan_block(&mf);
        println!();
        println!("  `parc restore {}` runs these for you.", mf.name);
    }

    // Only now, with every byte read back off the disk and re-hashed - and with
    // the tree walked a second time to prove the archive is not just consistent
    // but *complete* - is the project allowed to move. Consent was taken before
    // any of this started.
    if opt.delete {
        eprintln!("  is the project still the project in the archive…");
        // What `verify` re-hashed off the disk, keyed by path - so `drift` can fall
        // back to content on any file whose timestamps are too coarse to trust.
        let hashes: HashMap<&str, &str> = mf
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.sha256.as_str()))
            .collect();
        drift(root, &drop, &entries, &hashes)?;

        let dest = move_to_trash(root)?;
        println!();
        println!("  original moved to the Trash: {}", dest.display());
        println!("  {} freed.", human(proj.total_size));
    }

    Ok(final_path)
}

/// Put the archive's bytes on the disk, not merely into the kernel.
///
/// `verify` re-reads the archive it has just written and re-hashes every byte of
/// it. That proves the bytes are *right*. It proves nothing whatever about where
/// they are: a read that follows a write is served out of the same page cache the
/// write went into, so an archive still sitting entirely in memory verifies
/// perfectly - and `archive`'s next act is to move the project to the Trash on the
/// strength of that.
///
/// The gap is a power cut, or a hard reset, in the seconds before the kernel gets
/// round to flushing on its own. What survives it is the worst of both: the
/// project is gone, and the archive is a file of exactly the right length holding
/// zeroes. `verify` said SOUND about it, and was telling the truth at the time.
///
/// `sync_all`, not `sync_data`: the length and the mtime have to land with the
/// bytes. On macOS this is `F_FULLFSYNC`, which also drains the drive's own write
/// cache - a plain `fsync(2)` returns as soon as the disk has *promised* to write,
/// which is a promise a power cut does not keep.
pub fn sync_file(f: &File) -> Result<()> {
    f.sync_all()
        .context("archive could not be written to disk - original untouched")
}

/// Create a file only its owner can read or write.
///
/// An archive is a plaintext copy of every secret the project held: `.env`,
/// service keys, a `secrets/` directory. The default `File::create` leaves it
/// `0644` - readable by every other account on the machine, and by anything
/// running as another uid. Written `0600` from the first byte, the secrets are
/// never even briefly world-readable: the `.partial` is private too, and `rename`
/// carries the mode through to the final name. Encrypted archives get the same
/// mode, which costs nothing and keeps one code path for both.
pub fn create_private(path: &Path) -> io::Result<File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// Make the archive's *name* durable, once its bytes already are.
///
/// `sync_file` flushes the file. The directory entry that gives the file that name
/// is a separate object, in a separate block, and `rename(2)` only dirties it - so
/// the bytes can be on the platter under no name at all, or still under the
/// `.partial` one they were written with, while the project they came from is in
/// the Trash. The only way to flush a directory is to open it and sync it.
fn sync_dir(dir: &Path) -> Result<()> {
    let d = File::open(dir).with_context(|| format!("could not open directory: {}", dir.display()))?;
    match d.sync_all() {
        Ok(()) => Ok(()),
        // Not every filesystem will flush a directory on demand: a network mount or
        // a FAT-family volume - an external disk being archived onto is exactly
        // that - answers ENOTSUP (45 on macOS, 95 on Linux) or EINVAL (22), and
        // means it. That is not a reason to refuse to archive. The bytes are
        // already down; `sync_file` is not optional and has run. All that is at
        // risk here is the *name*, and the worst a power cut can then leave is the
        // finished archive under its `.partial` name - a rename away, not a loss.
        Err(e) if matches!(e.raw_os_error(), Some(45) | Some(95) | Some(22)) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("directory could not be written to disk: {}", dir.display())),
    }
}

fn write_tar(
    root: &Path,
    entries: &BTreeMap<String, Kind>,
    mf: &Manifest,
    dest: &Path,
    level: i32,
    pass: Option<&SecretString>,
) -> Result<()> {
    // tar → zstd → (age) → file. Encryption is the outermost layer, so an
    // encrypted archive is a normal `.tar.zst` the moment it is decrypted.
    match pass {
        Some(p) => {
            let sink = crypto::encrypt_to(dest, p)?;
            let sink = write_stream(root, entries, mf, sink, level)?;
            // Syncs too - see `crypto::finish`.
            crypto::finish(sink)?;
        }
        None => {
            let sink = BufWriter::new(create_private(dest)?);
            let sink = write_stream(root, entries, mf, sink, level)?;
            // `into_inner` is the flush, and it hands back the file so it can be
            // synced. A bare `flush` stopped at the kernel.
            let file = sink
                .into_inner()
                .map_err(|e| e.into_error())
                .context("could not write archive")?;
            sync_file(&file)?;
        }
    }
    Ok(())
}

/// The archive body, written into whatever sink it is handed. Returns the sink,
/// so an encrypted one can still be finalised by its owner.
fn write_stream<W: Write>(
    root: &Path,
    entries: &BTreeMap<String, Kind>,
    mf: &Manifest,
    sink: W,
    level: i32,
) -> Result<W> {
    let mut enc = zstd::stream::write::Encoder::new(sink, level)?;
    enc.multithread(std::thread::available_parallelism().map_or(1, |n| n.get() as u32))?;

    {
        let mut tar = tar::Builder::new(&mut enc);
        tar.follow_symlinks(false);

        let json = serde_json::to_vec_pretty(mf)?;
        let mut h = tar::Header::new_gnu();
        h.set_size(json.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(mf.created_at as u64);
        h.set_cksum();
        tar.append_data(&mut h, MANIFEST_PATH, &json[..])?;

        for rel in entries.keys() {
            tar.append_path_with_name(root.join(rel), rel)
                .with_context(|| format!("could not add to tar: {rel}"))?;
        }
        tar.finish()?;
    }

    Ok(enc.finish()?)
}

#[derive(Default)]
pub struct Report {
    pub checked: usize,
    pub mismatched: Vec<String>,
    pub missing: Vec<String>,
    pub unexpected: Vec<String>,
    pub manifest: Option<Manifest>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.mismatched.is_empty() && self.missing.is_empty() && self.unexpected.is_empty()
    }
    pub fn problems(&self) -> String {
        let mut s = String::new();
        for m in &self.mismatched {
            s.push_str(&format!("  hash mismatch: {m}\n"));
        }
        for m in &self.missing {
            s.push_str(&format!("  not in archive: {m}\n"));
        }
        for m in &self.unexpected {
            s.push_str(&format!("  not in manifest: {m}\n"));
        }
        s
    }
}

/// Re-reads the archive from disk and re-hashes every byte. Nothing else in the
/// tool is allowed to call an archive good - a manifest that agrees with itself
/// proves nothing.
pub fn verify(path: &Path, pass: Option<&SecretString>) -> Result<Report> {
    let dec = zstd::stream::read::Decoder::new(crypto::open(path, pass)?)?;
    let mut ar = tar::Archive::new(dec);

    let mut rep = Report::default();
    let mut expect: HashMap<String, (u64, String)> = HashMap::new();
    let mut links: HashMap<String, String> = HashMap::new();

    for entry in ar.entries()? {
        let mut e = entry?;
        let p = e.path()?.to_string_lossy().to_string();

        if p == MANIFEST_PATH {
            let mut s = String::new();
            (&mut e).take(MANIFEST_MAX).read_to_string(&mut s)?;
            let mf: Manifest = serde_json::from_str(&s).context("could not read manifest")?;
            if mf.format_version > FORMAT_VERSION {
                bail!(
                    "archive format v{} - this parc can read at most v{}",
                    mf.format_version,
                    FORMAT_VERSION
                );
            }
            expect = mf
                .files
                .iter()
                .map(|x| (x.path.clone(), (x.size, x.sha256.clone())))
                .collect();
            links = mf
                .links
                .iter()
                .map(|x| (x.path.clone(), x.target.clone()))
                .collect();
            rep.manifest = Some(mf);
            continue;
        }

        match e.header().entry_type() {
            tar::EntryType::Directory => {}
            tar::EntryType::Symlink => {
                let target = e
                    .link_name()?
                    .map(|t| t.to_string_lossy().to_string())
                    .unwrap_or_default();
                match links.remove(&p) {
                    Some(t) if t == target => {}
                    Some(_) => rep.mismatched.push(format!("{p} (symlink target)")),
                    None => rep.unexpected.push(p),
                }
            }
            _ => {
                let Some((size, sha)) = expect.remove(&p) else {
                    rep.unexpected.push(p);
                    continue;
                };
                let mut h = Sha256::new();
                let mut buf = vec![0u8; 128 * 1024];
                let mut n = 0u64;
                loop {
                    let r = e.read(&mut buf)?;
                    if r == 0 {
                        break;
                    }
                    h.update(&buf[..r]);
                    n += r as u64;
                }
                if hex(&h.finalize()) != sha || n != size {
                    rep.mismatched.push(p);
                } else {
                    rep.checked += 1;
                }
            }
        }
    }

    if rep.manifest.is_none() {
        bail!("no manifest - this is not a parc archive");
    }
    rep.missing.extend(expect.into_keys());
    rep.missing.extend(links.into_keys());
    Ok(rep)
}

pub enum Target {
    /// Back exactly where it was taken from, under its original folder name.
    Original,
    Into(PathBuf),
    /// Let the person who is restoring decide. We know whether the original
    /// spot is still free; guessing wastes that.
    Ask,
}

/// Expand a leading `~`, which a shell would have done for a flag but not for
/// something typed at our own prompt.
fn expand(s: &str) -> PathBuf {
    match s.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(s),
    }
}

fn choose(original: &Path, name: &OsStr) -> Result<PathBuf> {
    let here = std::env::current_dir()?.join(name);

    // Piped or scripted: never silently write outside the working directory.
    if !io::stdin().is_terminal() {
        return Ok(here);
    }

    // Both destinations can be occupied, and by different things. A project
    // archived from ~/Desktop/acme/backend restored from inside
    // ~/Desktop/projects/acme would land on top of a *different* backend.
    let orig_taken = original.exists();
    let here_taken = here.exists();

    let mark = |taken: bool| if taken { "  (TAKEN)" } else { "" };

    println!();
    println!("  Where should it be restored?");
    println!();
    println!(
        "    1) where it came from   {}{}",
        original.display(),
        mark(orig_taken)
    );
    println!("    2) here                 {}{}", here.display(), mark(here_taken));
    println!("    3) a different directory");
    println!();

    let default = if !orig_taken {
        "1"
    } else if !here_taken {
        "2"
    } else {
        "3"
    };

    loop {
        print!("  choice [{default}]: ");
        io::stdout().flush()?;
        let mut s = String::new();
        io::stdin().read_line(&mut s)?;

        let pick = match s.trim() {
            "" => default,
            other => other,
        };

        match pick {
            "1" if orig_taken => println!("  there is already a folder there - move it first."),
            "1" => return Ok(original.to_path_buf()),
            "2" if here_taken => println!("  there is already a folder here - move it first."),
            "2" => return Ok(here),
            "3" => {
                print!("  directory: ");
                io::stdout().flush()?;
                let mut d = String::new();
                io::stdin().read_line(&mut d)?;
                let d = d.trim();
                if !d.is_empty() {
                    return Ok(expand(d).join(name));
                }
            }
            _ => println!("  1, 2, or 3."),
        }
    }
}

/// How `restore` should treat the recorded setup commands.
pub struct Setup {
    /// Run them at all (`--no-setup` clears this).
    pub run: bool,
    /// Run them without asking first (`--yes`, for scripts). Off by default,
    /// because the commands come from the archive's manifest, which is not
    /// authenticated - an archive from elsewhere could carry anything.
    pub yes: bool,
}

pub fn restore(
    path: &Path,
    target: Target,
    setup: Setup,
    pass: Option<&SecretString>,
) -> Result<PathBuf> {
    eprintln!("  verifying…");
    let rep = verify(path, pass)?;
    if !rep.ok() {
        bail!("archive is corrupt - not opening it\n{}", rep.problems());
    }
    let mut mf = rep.manifest.expect("verify guarantees the manifest");

    // The folder comes back under the name it had, not under the archive's
    // label - `health`, not `acme-health`. The label only ever existed to
    // keep eight projects called `backend` apart as *files*.
    let original = PathBuf::from(&mf.original_path);
    let name: OsString = original
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| OsString::from(&mf.name));

    let dest = match target {
        Target::Original => original.clone(),
        Target::Into(d) => d.join(&name),
        Target::Ask => choose(&original, &name)?,
    };

    if dest.exists() {
        bail!("already exists: {}", dest.display());
    }
    fs::create_dir_all(&dest)?;

    eprintln!("  extracting…");
    let dec = zstd::stream::read::Decoder::new(crypto::open(path, pass)?)?;
    let mut ar = tar::Archive::new(dec);

    for entry in ar.entries()? {
        let mut e = entry?;
        let p = e.path()?.to_path_buf();
        if p.to_string_lossy() == MANIFEST_PATH {
            continue;
        }
        // The tar crate refuses `..` on unpack, but this archive can name any
        // path it likes and we are the ones choosing where it lands.
        if p.components().any(|c| matches!(c, std::path::Component::ParentDir))
            || p.is_absolute()
        {
            bail!("unsafe path in archive: {}", p.display());
        }
        // A hard-link entry links to a file the archive names. parc never writes
        // one - it stores each linked file whole - so any hard link here came from
        // somewhere else, and one whose target is absolute or climbs out of the
        // tree is reaching for a file outside it. Symlinks are kept verbatim: a
        // relative `..` symlink is ordinary inside a project, and `unpack_in`
        // already refuses to write *through* a link that leaves the destination.
        if e.header().entry_type().is_hard_link() {
            let safe = e.link_name().ok().flatten().is_some_and(|t| {
                !t.is_absolute()
                    && !t
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
            });
            if !safe {
                bail!("unsafe hard link in archive: {}", p.display());
            }
        }
        e.unpack_in(&dest)?;
    }

    println!();
    println!("  restored: {}", dest.display());
    if dest != original {
        println!("  {}", format_args!("(original: {})", original.display()));
    }
    println!("  {} · {}", mf.stacks.join("/"), mf.created_at_iso);

    // Archives written before the recipe existed carry no plan - but they do say
    // what was removed, and the tree we just extracted still has the lockfile and
    // the scripts. Deriving it now is the difference between a restored project
    // and a folder with no node_modules and a cheerful "ready".
    if mf.restore_plan.is_empty() && !mf.removed.is_empty() {
        let derived = crate::recipe::plan_for(
            &dest,
            mf.package_manager.as_deref(),
            &mf.stacks,
            &mf.removed,
        );
        if !derived.is_empty() {
            eprintln!("  (old archive: no recipe recorded, recomputed)");
            mf.restore_plan = derived;
        }
    }
    if mf.runtime.is_none() {
        mf.runtime = crate::recipe::runtime(&dest);
    }

    if !mf.removed.is_empty() {
        println!();
        removed_block(&mf);
    }

    runtime_warning(&mf);

    if mf.restore_plan.is_empty() {
        println!("\n  Nothing to restore - the project is ready as is.\n");
        return Ok(dest);
    }

    println!();
    plan_block(&mf);

    if !setup.run {
        println!();
        println!("  --no-setup given, none were run.");
        println!();
        return Ok(dest);
    }

    // These are shell commands read straight out of the archive's manifest, and
    // the manifest is not signed - for an archive you made this is the whole
    // convenience, but one that arrived from somewhere else could carry anything.
    // The commands were just printed; run them only on an explicit yes, and in a
    // non-interactive context (no `--yes`) decline rather than execute blind.
    if !setup.yes && !confirm("\n  Shall I run the commands above?")? {
        println!();
        println!("  Not run. Add --yes if you approve, or run the steps by hand.");
        println!();
        return Ok(dest);
    }

    println!();
    if let Some(i) = crate::recipe::execute(&dest, &mf.restore_plan, true) {
        // Expected, not exceptional. A lockfile from two years ago routinely
        // fails to resolve: yanked packages, a Node major that no longer builds
        // the native modules. None of that is data loss.
        println!();
        println!("  ✗ {} failed.", mf.restore_plan[i].cmd);
        println!("  Source code was fully restored - nothing is lost.");
        println!("  The lockfile and git history came out of the archive; you'll need to sort out the setup by hand.");
        if let Some(rt) = &mf.runtime {
            println!("  This project ran on {rt}.");
        }
        println!();
        return Ok(dest);
    }

    println!();
    println!("  Ready: cd {}", dest.display());
    println!();
    Ok(dest)
}

/// The lockfile pins the packages; it does not pin the interpreter that has to
/// build them. A native module compiled against Node 18 will not load on 24.
fn runtime_warning(mf: &Manifest) {
    let Some(want) = &mf.runtime else { return };
    let Some((tool, ver)) = want.split_once(' ') else {
        return;
    };
    if tool != "node" {
        return;
    }

    let have = std::process::Command::new("node")
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let Some(have) = have else { return };

    if !crate::recipe::runtime_satisfied(ver, &have) {
        println!();
        println!("  ! this project was built with node {ver}, you have {have}.");
        println!("    native modules may not compile.");
    }
}

/// One archive in the library.
pub struct Item {
    pub file: PathBuf,
    pub encrypted: bool,
    /// None for an encrypted archive: the manifest is the first entry *inside*
    /// the ciphertext, and `list` must never ask for a passphrase - a library
    /// can hold archives locked with different ones, or with none.
    pub manifest: Option<Manifest>,
}

impl Item {
    /// What the user types to name this archive. From the manifest when we can
    /// read it, from the filename when we cannot.
    pub fn name(&self) -> String {
        match &self.manifest {
            Some(mf) => mf.name.clone(),
            None => name_of(&self.file),
        }
    }
}

/// The archive's name as `list` prints it and `resolve` accepts it - the file
/// name with our extensions taken off. The only name a user ever has to type.
pub fn name_of(file: &Path) -> String {
    let raw = file.file_name().unwrap_or_default().to_string_lossy();
    raw.strip_suffix(ENC_EXT)
        .or_else(|| raw.strip_suffix(EXT))
        .unwrap_or(&raw)
        .to_string()
}

/// The archive file's own path is kept alongside the manifest rather than
/// stuffed into `original_path` - that field means where the project *came
/// from*, and `restore` puts it back there.
pub fn list(dir: &Path) -> Result<Vec<Item>> {
    let mut out: Vec<Item> = Vec::new();
    let Ok(rd) = fs::read_dir(dir) else {
        return Ok(out);
    };

    for e in rd.flatten() {
        let p = e.path();
        let raw = p.to_string_lossy().to_string();

        if !raw.ends_with(EXT) && !raw.ends_with(ENC_EXT) {
            continue;
        }
        // By content, not by name: an encrypted archive renamed off its `.age`
        // suffix must still be shown as encrypted, not silently dropped as an
        // unreadable plaintext archive - a last copy you cannot see is a last copy
        // you cannot protect.
        if crypto::is_encrypted_content(&p) {
            out.push(Item {
                file: p,
                encrypted: true,
                manifest: None,
            });
            continue;
        }
        match read_manifest(&p, None) {
            Ok(mf) => out.push(Item {
                file: p,
                encrypted: false,
                manifest: Some(mf),
            }),
            Err(err) => eprintln!("  could not read {}: {err}", p.display()),
        }
    }

    // Encrypted archives have no readable date, so they sort by mtime - the one
    // thing about them the filesystem still knows.
    out.sort_by_key(|i| {
        let ts = match &i.manifest {
            Some(m) => m.created_at,
            None => fs::metadata(&i.file)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64),
        };
        std::cmp::Reverse(ts)
    });
    Ok(out)
}

pub struct RemoveOptions {
    pub force: bool,
    pub yes: bool,
}

/// One archive that `remove` is about to throw away, resolved before anything on
/// disk is touched.
pub struct Doomed {
    pub file: PathBuf,
    pub name: String,
    pub size: u64,
    /// The project this came from is no longer on disk - or is encrypted and we
    /// cannot tell, which gets the same protection. `list` prints the readable
    /// case as "archive-only": the archive *is* the project now.
    pub only_copy: bool,
    /// Encrypted, and we have no passphrase to look inside.
    pub opaque: bool,
    /// None when the manifest could not be read - a file this tool can no longer
    /// restore from either.
    pub original_path: Option<String>,
}

/// Deleting an archive is the most destructive thing in this tool, because an
/// archive can be the last copy of a project. So this is the one place that does
/// the opposite of `clean`: the file goes to ~/.Trash instead of away. `clean`
/// may unlink an artifact outright because a lockfile can rebuild it - nothing
/// rebuilds this.
///
/// Everything is resolved and checked before the first file moves. Half of
/// `parc rm a b c` succeeding, with the refusal reported for `c` after `a` is
/// already gone, would be its own kind of data loss.
/// Is the project this archive came from still on disk *as that project*?
///
/// The recorded `original_path` merely existing is not enough. An empty directory
/// re-created at the old path, or an unrelated project cloned into it, would make
/// a last-copy archive look safely backed up when it is the only copy left - and
/// `rm` would then delete it without `--force`, telling the user their project was
/// "still on disk" while the thing at that path was a stranger.
///
/// Identity is confirmed from the archive's own file list: if not one of the files
/// it recorded is still there, whatever occupies the path now is not this project.
/// A real project answers on the first file checked; only a gone one runs the loop
/// out. The `.parc/` manifest entry is skipped - it never existed in the project.
fn original_present(mf: &Manifest) -> bool {
    let root = Path::new(&mf.original_path);
    if !root.is_dir() {
        return false;
    }
    let mut checked = 0usize;
    for f in mf.files.iter().filter(|f| !f.path.starts_with(".parc/")) {
        if root.join(&f.path).exists() {
            return true;
        }
        checked += 1;
        if checked >= 64 {
            break;
        }
    }
    // A manifest that recorded no files has nothing to confirm identity with, so
    // fall back to the path existing - which it does, or we returned above.
    checked == 0
}

pub fn remove(dir: &Path, inputs: &[PathBuf], opt: &RemoveOptions) -> Result<Vec<PathBuf>> {
    let mut doomed: Vec<Doomed> = Vec::new();

    for input in inputs {
        let file = resolve_in(dir, input)?;
        // `parc rm foo foo.parc.tar.zst` names one file twice; the second trash
        // move would fail on a path that is no longer there.
        if doomed.iter().any(|d| d.file == file) {
            continue;
        }

        let size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        // Encrypted archives only give up their manifest to whoever holds the
        // passphrase. `rm` never prompts for one - but it will use it if the
        // environment already has it, because knowing beats guessing.
        let pass = crypto::env_passphrase();
        let (name, original_path, present) = match read_manifest(&file, pass.as_ref()) {
            Ok(mf) => {
                let present = original_present(&mf);
                (mf.name, Some(mf.original_path), present)
            }
            // A corrupt archive is unreadable to `restore` as well. Failing to
            // parse it must not be the thing that stops you throwing it away -
            // that is most of the reason to run this command.
            Err(_) => (name_of(&file), None, false),
        };

        // We could not read it, and it is encrypted rather than broken: the
        // project it holds may well be gone from disk. Not knowing is not the
        // same as knowing it is safe, and only one of those may delete quietly.
        // Detected by content, not extension - a renamed encrypted archive must
        // still get the last-copy protection, never be waved through as "corrupt".
        let opaque = original_path.is_none() && crypto::is_encrypted_content(&file);
        let only_copy = opaque || (original_path.is_some() && !present);

        doomed.push(Doomed {
            file,
            name,
            size,
            only_copy,
            opaque,
            original_path,
        });
    }

    if doomed.is_empty() {
        println!("\n  No archives to delete.\n");
        return Ok(Vec::new());
    }

    let blocked: Vec<&Doomed> = doomed.iter().filter(|d| d.only_copy).collect();
    if !blocked.is_empty() && !opt.force {
        bail!(
            "deleting the following archive{}, the project could be lost entirely:\n{}\n  \
             Restore it first with `parc restore <name>`, or --force if you really want it gone.",
            if blocked.len() > 1 { "s" } else { "" },
            blocked
                .iter()
                .map(|d| if d.opaque {
                    format!(
                        "    {}  ({})\n      encrypted - cannot tell whether the project inside is still on disk\n      \
                         (give {} and I can check)",
                        d.name,
                        human(d.size),
                        crypto::ENV_KEY
                    )
                } else {
                    format!(
                        "    {}  ({})\n      where it came from: {}  - no longer there",
                        d.name,
                        human(d.size),
                        d.original_path.as_deref().unwrap_or("?")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    let w = doomed
        .iter()
        .map(|d| d.name.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 38);

    println!();
    println!("  Will be moved to the Trash:");
    println!();
    for d in &doomed {
        let note = match (&d.original_path, d.only_copy, d.opaque) {
            (_, _, true) => "ENCRYPTED - cannot look inside",
            (None, _, _) => "could not read manifest - corrupt archive",
            (Some(_), true, _) => "ARCHIVE-ONLY - the project's only copy",
            (Some(_), false, _) => "project is also on disk",
        };
        println!(
            "    {:<w$}  {:>9}  {}",
            d.name,
            human(d.size),
            note,
            w = w
        );
    }

    let total: u64 = doomed.iter().map(|d| d.size).sum();
    println!();
    println!("  {} archives · {}", doomed.len(), human(total));

    if !opt.yes && !confirm("\n  Delete?")? {
        println!("  cancelled - nothing was touched.\n");
        return Ok(Vec::new());
    }

    let mut gone = Vec::new();
    for d in &doomed {
        let dest = move_to_trash(&d.file)?;
        println!("  {} → {}", d.name, dest.display());
        gone.push(dest);
    }
    println!();
    Ok(gone)
}

/// Stops as soon as the manifest is read: listing a library must not pay to
/// decompress every archive in it, and neither should answering "what was this
/// project?" - `show` reads the first tar entry and stops.
pub fn read_manifest(path: &Path, pass: Option<&SecretString>) -> Result<Manifest> {
    let dec = zstd::stream::read::Decoder::new(crypto::open(path, pass)?)?;
    let mut ar = tar::Archive::new(dec);
    for entry in ar.entries()? {
        let mut e = entry?;
        if e.path()?.to_string_lossy() == MANIFEST_PATH {
            let mut s = String::new();
            (&mut e).take(MANIFEST_MAX).read_to_string(&mut s)?;
            return Ok(serde_json::from_str(&s)?);
        }
    }
    Err(anyhow!("no manifest"))
}

/// The device `p` lives on. Two paths with the same one can be `rename`d between;
/// two without cannot, whatever else is true of them.
fn device(p: &Path) -> Option<u64> {
    fs::metadata(p).ok().map(|md| md.dev())
}

/// The mount point `dir` sits on: climb until the device number changes.
fn volume_root(dir: &Path) -> Option<PathBuf> {
    let dev = device(dir)?;
    let mut root = dir.to_path_buf();
    while let Some(parent) = root.parent() {
        if device(parent) != Some(dev) {
            break;
        }
        root = parent.to_path_buf();
    }
    Some(root)
}

/// The trash directories to try for a file sitting in `dir`, best first.
///
/// `~/.Trash` is only reachable from the volume `$HOME` is on. `rename(2)` cannot
/// cross a filesystem, and an archive library on an external disk is on another
/// one - which is the obvious place to keep a library, and where this tool could
/// not remove an archive at all.
///
/// So the trash is picked on the volume the file is already on, which is what
/// Finder does: `<volume>/.Trashes/<uid>` for anything off the boot volume, shown
/// in the same Trash as everything else. If the volume will not have it - a FAT
/// stick, a network share with opinions about dotfiles - a `.parc-trash` beside
/// the file is on the same volume by construction, so it cannot fail for this
/// reason. What never happens, on any volume, is the file being unlinked.
fn trash_dirs(dir: &Path) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);

    // The user's own Trash, whenever a rename can actually reach it.
    if let Some(home) = &home {
        if device(dir).is_some() && device(dir) == device(home) {
            return vec![home.join(".Trash")];
        }
    }

    let mut out = Vec::new();
    // The uid off `$HOME` rather than `getuid(2)`, which std does not expose: the
    // home directory is owned by whoever we are.
    let uid = home.as_deref().and_then(device_uid);
    if let (Some(vol), Some(uid)) = (volume_root(dir), uid) {
        out.push(vol.join(".Trashes").join(uid.to_string()));
    }
    out.push(dir.join(".parc-trash"));
    out
}

fn device_uid(p: &Path) -> Option<u32> {
    fs::metadata(p).ok().map(|md| md.uid())
}

/// Never `unlink`. An archive can be the last copy of a project, and `archive`
/// puts the project itself through here too.
fn move_to_trash(p: &Path) -> Result<PathBuf> {
    let base = p
        .file_name()
        .ok_or_else(|| anyhow!("invalid path"))?
        .to_string_lossy()
        .to_string();
    let dir = p.parent().unwrap_or(Path::new("."));

    let mut last: Option<io::Error> = None;
    for trash in trash_dirs(dir) {
        if let Err(e) = fs::create_dir_all(&trash) {
            last = Some(e);
            continue;
        }
        let mut dest = trash.join(&base);
        let mut i = 1;
        while dest.exists() {
            dest = trash.join(format!("{base} {i}"));
            i += 1;
        }
        match fs::rename(p, &dest) {
            Ok(()) => return Ok(dest),
            Err(e) => last = Some(e),
        }
    }

    Err(last.map_or_else(
        || anyhow!("no trash directory found"),
        anyhow::Error::from,
    ))
    .with_context(|| format!("could not move to the Trash: {}", p.display()))
}

fn confirm(q: &str) -> Result<bool> {
    print!("{q} [y/N] ");
    io::stdout().flush()?;
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    Ok(matches!(s.trim().to_lowercase().as_str(), "y" | "yes"))
}
