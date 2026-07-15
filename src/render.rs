use crate::archive::Item;
use crate::manifest::Manifest;
use crate::model::*;
use std::io::IsTerminal;
use std::path::Path;

pub fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else if v < 10.0 {
        format!("{:.1} {}", v, U[i])
    } else {
        format!("{:.0} {}", v, U[i])
    }
}

/// The date out of an ISO timestamp. Sliced with `get`, not `[..10]`: a manifest
/// whose timestamp is short or missing is a corrupt archive, and a corrupt
/// archive must not be able to panic `parc list` for the whole library - that is
/// the one command you would reach for to find out what is still intact.
fn day(iso: &str) -> &str {
    iso.get(..10).unwrap_or(iso)
}

fn days_since(ts: i64) -> Option<i64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((now - ts) / 86_400)
}

struct Paint(bool);

impl Paint {
    fn new() -> Self {
        Paint(std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn c(&self, code: &str, s: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    fn dim(&self, s: &str) -> String {
        self.c("2", s)
    }
    fn bold(&self, s: &str) -> String {
        self.c("1", s)
    }
}

fn age(proj: &Project) -> String {
    match proj.age_days() {
        None => "?".into(),
        Some(d) if d < 1 => "today".into(),
        Some(d) if d < 60 => format!("{d}d"),
        Some(d) if d < 730 => format!("{}mo", d / 30),
        Some(d) => format!("{}y", d / 365),
    }
}

/// Colour goes on *after* padding, never before.
///
/// `{:<11}` applied to a string that already carries escape codes counts
/// "\x1b[36m" as six characters of content: the field is over width before the
/// first letter, no padding is emitted, and every column to the right of it
/// walks. Which is why the table looked fine piped to a file and crooked in a
/// terminal - the only place anyone reads it.
fn verdict_tag(p: &Paint, v: Verdict) -> String {
    let (code, text) = match v {
        Verdict::Redundant => ("32", "REDUNDANT"),
        Verdict::Archive => ("36", "ARCHIVE"),
        Verdict::Review => ("33", "REVIEW"),
    };
    p.c(code, &format!("{text:<9}"))
}

pub fn scan_table(projects: &[Project]) {
    let p = Paint::new();

    if projects.is_empty() {
        println!("No projects found.");
        return;
    }

    let labels: Vec<String> = projects.iter().map(|x| x.label()).collect();
    let w = labels
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 38);

    println!();
    for (proj, label) in projects.iter().zip(&labels) {
        let short: String = label.chars().take(w).collect();
        let padded = format!("{short:<w$}");

        // .git dominating a project is the whole finding - nothing to clean,
        // and compression will not touch packfiles either.
        let note = if proj.git_size > proj.total_size / 2 && proj.git_size > 50 << 20 {
            format!("git history {}", human(proj.git_size))
        } else if proj.stacks.is_empty() {
            "-".into()
        } else {
            proj.stacks.join("/")
        };

        println!(
            "  {}  {:>9} → {:>9}  {:>4}  {}  {}  {}",
            p.bold(&padded),
            human(proj.total_size),
            human(proj.est_archive),
            format!("{:.0}%", proj.savings_pct()),
            p.dim(&format!("{:>6}", age(proj))),
            verdict_tag(&p, proj.verdict),
            p.dim(&note),
        );
    }

    let total: u64 = projects.iter().map(|x| x.total_size).sum();
    let after: u64 = projects.iter().map(|x| x.est_archive).sum();
    let cleanup: u64 = projects.iter().map(|x| x.cleanup_savings()).sum();
    let compress: u64 = projects.iter().map(|x| x.compression_savings()).sum();
    let review_bytes: u64 = projects.iter().map(|x| x.review_size).sum();

    println!();
    println!("  {} projects · {} → {}", projects.len(), human(total), human(after));
    println!();
    println!(
        "  {}  cleanup       generated files, measured",
        p.bold(&format!("{:>9}", human(cleanup)))
    );
    println!(
        "  {:>9}  {}",
        human(compress),
        p.dim("compression   zstd estimate of remaining source")
    );

    let redundant = projects
        .iter()
        .filter(|x| x.verdict == Verdict::Redundant)
        .count();
    // The projects that hold those bytes - not the ones whose *verdict* is
    // REVIEW, which is a different question and gave the line below its finest
    // moment: "+3.0 GB more, but I'm not sure - 0 projects waiting to be looked
    // at".
    let review = projects.iter().filter(|x| x.review_size > 0).count();

    println!();
    if review_bytes > 0 {
        println!(
            "  {}",
            p.dim(&format!(
                "+{} more, but I'm not sure - {} projects waiting to be looked at by hand",
                human(review_bytes),
                review
            ))
        );
    }
    if redundant > 0 {
        println!(
            "  {}",
            p.dim(&format!(
                "{redundant} projects sit complete on the remote - no need to archive at all"
            ))
        );
    }
    println!();
}

pub fn archive_list(dir: &Path, items: &[Item]) {
    let p = Paint::new();

    if items.is_empty() {
        println!("\n  no archives in {}\n", dir.display());
        return;
    }

    let w = items
        .iter()
        .map(|i| i.name().chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 38);

    println!();
    for item in items {
        let size = std::fs::metadata(&item.file).map(|md| md.len()).unwrap_or(0);
        let name = p.bold(&format!("{:<w$}", item.name(), w = w));

        // An encrypted archive tells us nothing but its size - not what it came
        // from, not how much it saved, not whether the project still exists. That
        // is what was asked of it, so the row says so instead of guessing.
        let Some(m) = &item.manifest else {
            println!(
                "  {}  {:>9}  {}  {}  {}",
                name,
                human(size),
                p.dim(&format!("{:<12}", "-?")),
                p.dim(&format!("{:>10}", "?")),
                p.c("35", "🔒 encrypted"),
            );
            println!(
                "  {}",
                p.dim(&format!("{:<w$}  to read its contents: parc show {}", "", item.name(), w = w))
            );
            continue;
        };

        let saved = m.original_size.saturating_sub(size);
        let home_gone = !Path::new(&m.original_path).exists();

        println!(
            "  {}  {:>9}  {}  {}  {}",
            name,
            human(size),
            p.dim(&format!("{:<12}", format!("-{}", human(saved)))),
            p.dim(day(&m.created_at_iso)),
            // The archive is the only copy left when the original is gone.
            if home_gone {
                p.c("36", "archive-only")
            } else {
                p.dim("on disk too")
            },
        );
        // Where it came from decides where `restore` offers to put it back, and
        // a name like `acme-backend` does not say which of two trees.
        println!("  {}", p.dim(&format!("{:<w$}  {}", "", m.original_path, w = w)));
    }

    let total: u64 = items
        .iter()
        .filter_map(|i| std::fs::metadata(&i.file).ok())
        .map(|md| md.len())
        .sum();
    // Savings can only be summed over archives whose original size we can read.
    let known: Vec<&Manifest> = items.iter().filter_map(|i| i.manifest.as_ref()).collect();
    let original: u64 = known.iter().map(|m| m.original_size).sum();
    let known_size: u64 = items
        .iter()
        .filter(|i| i.manifest.is_some())
        .filter_map(|i| std::fs::metadata(&i.file).ok())
        .map(|md| md.len())
        .sum();
    let locked = items.len() - known.len();

    println!();
    println!(
        "  {} archives · {} · {} saved{}",
        items.len(),
        human(total),
        p.bold(&human(original.saturating_sub(known_size))),
        if locked > 0 {
            p.dim(&format!("  ({locked} encrypted archives excluded)"))
        } else {
            String::new()
        }
    );
    println!("  {}", p.dim(&dir.display().to_string()));
    println!();
}

/// What was left *out of the archive*. Printed by `archive` (what just
/// happened), by `show` (what is in there) and by `restore` (what is coming
/// back) from one place, so the three answers can never drift apart.
///
/// The word matters: these directories are excluded from the archive, not
/// deleted from the project. Calling the list "removed items" is what made people
/// believe `clean` had emptied their project - it never had.
pub fn removed_block(mf: &Manifest) {
    let p = Paint::new();

    if mf.removed.is_empty() {
        println!(
            "  {}",
            p.dim("no generated files - the project is archived as-is.")
        );
        return;
    }

    let w = mf
        .removed
        .iter()
        .map(|r| r.path.chars().count())
        .max()
        .unwrap_or(10)
        .clamp(10, 38);

    let total: u64 = mf.removed.iter().map(|r| r.size).sum();
    println!(
        "  {} ({}):",
        p.bold("excluded from the archive"),
        human(total)
    );
    println!();
    for r in &mf.removed {
        println!(
            "    {:<w$}  {:>9}  {}",
            r.path,
            human(r.size),
            p.dim(&format!("rule: {}", r.rule)),
            w = w
        );
    }
}

/// The commands that turn the extracted files back into a working checkout.
/// Decided at archive time - see `recipe::plan`.
pub fn plan_block(mf: &Manifest) {
    let p = Paint::new();

    if mf.restore_plan.is_empty() {
        return;
    }

    println!("  {}:", p.bold("to restore"));
    println!();
    for (i, s) in mf.restore_plan.iter().enumerate() {
        let where_ = s
            .dir
            .as_deref()
            .map(|d| format!("(cd {d}) "))
            .unwrap_or_default();
        println!(
            "    {}. {}{:<44} {}",
            i + 1,
            where_,
            s.cmd,
            p.dim(&format!(
                "→ {}{}",
                s.why,
                if s.optional { "  (optional)" } else { "" }
            ))
        );
    }
    if let Some(rt) = &mf.runtime {
        println!();
        println!("  {}", p.dim(&format!("this project ran on {rt}")));
    }
}

/// Everything the archive knows about a project, without unpacking it: what it
/// was, what was stripped out, and what brings it back. The whole point is that
/// two years later nobody has to open the tar to answer "what was this?".
pub fn archive_info(file: &Path, mf: &Manifest) {
    let p = Paint::new();
    let size = std::fs::metadata(file).map(|m| m.len()).unwrap_or(0);

    println!();
    println!("  {}", p.bold(&mf.name));
    println!("  {}", p.dim(&file.display().to_string()));
    println!();

    let stack = if mf.stacks.is_empty() {
        "unknown".to_string()
    } else {
        mf.stacks.join(", ")
    };
    println!("  origin        {}", mf.original_path);
    println!("  archived      {}", day(&mf.created_at_iso));
    println!(
        "  stack         {}{}",
        stack,
        mf.package_manager
            .as_deref()
            .map(|pm| format!("  ·  {pm}"))
            .unwrap_or_default()
    );
    if let Some(rt) = &mf.runtime {
        println!("  runtime       {rt}");
    }

    if mf.git.is_repo {
        let branch = mf.git.branch.as_deref().unwrap_or("?");
        let commit = mf.git.commit.as_deref().unwrap_or("?");
        // `get`, not `[..8]`, for the same reason `day` uses it: this string comes
        // out of the manifest, and `[..8]` panics if byte 8 lands inside a
        // multi-byte character. A corrupt archive must not be able to take down
        // `parc show`, which is the command you would run to find out what it
        // still holds.
        let commit = commit.get(..8).unwrap_or(commit);
        println!("  git           {branch} @ {commit}");
        if let Some(remote) = &mf.git.remote {
            println!("  remote        {remote}");
        }
        // The reason the archive exists rather than a `git clone`: work that the
        // remote never saw.
        if mf.git.dirty > 0 || mf.git.untracked > 0 {
            println!(
                "  {}",
                p.c(
                    "33",
                    &format!(
                        "uncommitted  {} modified, {} untracked files",
                        mf.git.dirty, mf.git.untracked
                    )
                )
            );
        }
    } else {
        println!("  git           none");
    }

    println!(
        "  size          {} archive  ·  {} original",
        human(size),
        human(mf.original_size)
    );
    println!("  contents      {} files", mf.files.len());

    println!();
    removed_block(mf);
    println!();

    if mf.restore_plan.is_empty() && !mf.removed.is_empty() {
        // Written before the recipe was recorded. `restore` derives one from the
        // extracted tree - saying nothing here would read as "nothing to do".
        println!(
            "  {}",
            p.dim("recipe not written to this archive (old version) - will be computed during restore")
        );
    } else {
        plan_block(mf);
    }
    println!();
}

pub fn plan_detail(proj: &Project) {
    let p = Paint::new();

    println!();
    println!("  {}  {}", p.bold(&proj.name), p.dim(&proj.path.display().to_string()));

    let stack = if proj.stacks.is_empty() {
        "unknown".to_string()
    } else {
        proj.stacks.join(", ")
    };
    let pm = proj.package_manager.as_deref().unwrap_or("-");
    println!("  {}", p.dim(&format!("{stack} · {pm}")));

    if proj.git.is_repo {
        let branch = proj.git.branch.as_deref().unwrap_or("?");
        let commit = proj.git.commit.as_deref().unwrap_or("?");
        let remote = proj.git.remote.as_deref().unwrap_or("no remote");
        println!("  {}", p.dim(&format!("git: {branch}@{commit} · {remote}")));
    } else {
        println!("  {}", p.dim("git: none"));
    }

    if let Some(d) = proj.last_modified.and_then(days_since) {
        println!("  {}", p.dim(&format!("last modified: {d} days ago")));
    }

    println!();
    println!("  {:<16} {:>10}", "total", p.bold(&human(proj.total_size)));
    println!("  {:<16} {:>10}", "cleanup", human(proj.cleanup_savings()));
    println!(
        "  {:<16} {:>10}  {}",
        "compression",
        human(proj.compression_savings()),
        p.dim("(estimate)")
    );
    println!("  {:<16} {:>10}", "archive", human(proj.est_archive));
    println!(
        "  {:<16} {:>10}  ({:.0}%)",
        "saved",
        p.bold(&human(proj.savings())),
        proj.savings_pct()
    );

    if proj.git_size > proj.total_size / 2 && proj.git_size > 50 << 20 {
        println!();
        println!(
            "  {}",
            p.c(
                "33",
                &format!(
                    "{} of this project is git history ({} total, {:.0}% of it).",
                    human(proj.git_size),
                    human(proj.total_size),
                    (proj.git_size as f64 / proj.total_size as f64) * 100.0
                )
            )
        );
        println!(
            "  {}",
            p.dim("packfiles are already compressed - archiving won't shrink this. try `git gc --aggressive`.")
        );
    }

    let mut confirmed: Vec<&Artifact> = proj
        .artifacts
        .iter()
        .filter(|a| a.class == Class::Confirmed)
        .collect();
    confirmed.sort_by_key(|a| std::cmp::Reverse(a.size));

    if !confirmed.is_empty() {
        println!();
        println!("  {}", p.c("32", "WILL BE DELETED"));
        for a in confirmed.iter().take(15) {
            println!("    {:>9}  {}", human(a.size), p.dim(&a.path));
        }
        if confirmed.len() > 15 {
            println!("    {}", p.dim(&format!("… and {} more", confirmed.len() - 15)));
        }
    }

    let held: Vec<&Artifact> = proj
        .artifacts
        .iter()
        .filter(|a| a.class != Class::Confirmed)
        .collect();
    if !held.is_empty() {
        println!();
        println!("  {}", p.c("33", "UNTOUCHED"));
        for a in &held {
            println!("    {:>9}  {}", human(a.size), a.path);
            if let Some(n) = &a.note {
                println!("               {}", p.dim(n));
            }
        }
    }

    if !proj.orphans.is_empty() {
        println!();
        println!("  {}", p.c("35", "ARCHIVE-ONLY"));
        println!(
            "  {}",
            p.dim("not in git - without the archive these are lost for good")
        );
        for o in proj.orphans.iter().take(15) {
            println!("    {:>9}  {}", human(o.size), o.path);
        }
        if proj.orphans.len() > 15 {
            println!(
                "    {}",
                p.dim(&format!("… and {} more", proj.orphans.len() - 15))
            );
        }
    }

    println!();
    println!("  {}", verdict_tag(&p, proj.verdict));
    for r in &proj.reasons {
        println!("    · {r}");
    }

    if proj.verdict == Verdict::Redundant {
        if let Some(remote) = &proj.git.remote {
            println!();
            println!("  {}", p.dim("to get it back:"));
            println!("    git clone {remote}");
        }
    }
    println!();
}
