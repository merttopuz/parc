use crate::manifest::Removed;
use crate::model::{Class, Project};

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// One command that brings back something the archive deliberately left out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    pub cmd: String,
    /// Directory to run it in, relative to the project root. `pod install` only
    /// works from wherever the `Podfile` is.
    #[serde(default)]
    pub dir: Option<String>,
    /// What this brings back, in the user's terms.
    pub why: String,
    /// Failing does not mean the restore failed. A `build` is nice to have; an
    /// `install` is not.
    pub optional: bool,
}

fn removed(items: &[Removed], rule: &str) -> Option<u64> {
    items
        .iter()
        .filter(|r| r.rule == rule)
        .map(|r| r.size)
        .reduce(|a, b| a + b)
}

/// The directory that owns a build artifact: the nearest ancestor holding one of
/// `markers`, relative to the project root (`""` for the root itself).
///
/// A Gradle `build/` is rarely at the top - in a React Native repo it is
/// `android/app/build`, and `./gradlew build` run from the project root does
/// nothing at all. The wrapper sits with the build script, so that is where the
/// command has to run.
fn owner_dir(root: &Path, artifact: &str, markers: &[&str]) -> Option<String> {
    let mut dir = Path::new(artifact).parent();

    while let Some(d) = dir {
        if markers.iter().any(|m| root.join(d).join(m).exists()) {
            return Some(d.to_string_lossy().to_string());
        }
        if d.as_os_str().is_empty() {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Projects ship their build tool as a wrapper script far more often than they
/// expect you to have the right version installed globally. Prefer it when it is
/// there - it is the only invocation the project is actually tested against.
fn wrapper(root: &Path, dir: &str, script: &str, fallback: &str) -> String {
    if root.join(dir).join(script).exists() {
        format!("./{script}")
    } else {
        fallback.to_string()
    }
}

fn scripts(root: &Path) -> Vec<String> {
    let Ok(raw) = fs::read_to_string(root.join("package.json")) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    v.get("scripts")
        .and_then(|s| s.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// The runtime the project expected. Recorded because a lockfile is only half
/// the story: the same `pnpm install` against Node 24 and Node 18 does not
/// produce the same tree, and native modules will not even build.
pub fn runtime(root: &Path) -> Option<String> {
    if let Ok(v) = fs::read_to_string(root.join(".nvmrc")) {
        let v = v.trim().trim_start_matches('v');
        if !v.is_empty() {
            return Some(format!("node {v}"));
        }
    }
    if let Ok(raw) = fs::read_to_string(root.join("package.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
            if let Some(n) = v.pointer("/engines/node").and_then(|x| x.as_str()) {
                return Some(format!("node {n}"));
            }
        }
    }
    if let Ok(v) = fs::read_to_string(root.join("rust-toolchain.toml")) {
        if let Some(line) = v.lines().find(|l| l.contains("channel")) {
            let ch = line.split('=').nth(1)?.trim().trim_matches('"');
            return Some(format!("rust {ch}"));
        }
    }
    None
}

/// Directories a search for project manifests never descends into.
const SKIP: &[&str] = &[
    "node_modules", ".git", "target", "vendor", "Pods", ".venv", "venv", "dist", "build", ".next",
    "DerivedData", ".dart_tool", "__pycache__", ".turbo", "tmp",
];

/// Every directory under `root` that could hold a manifest, root included.
fn manifest_dirs(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf()];
    let mut queue = vec![(root.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = queue.pop() {
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
            if SKIP.contains(&name.as_str()) {
                continue;
            }
            out.push(e.path());
            queue.push((e.path(), depth + 1));
        }
    }
    out
}

/// What this project is missing *right now*, and the command that brings it back.
///
/// The archive recipe is written from what we removed. This one is written from
/// what is absent - which is the only thing left to go on once `clean` has been
/// and gone. It reads the same evidence a developer would: the lockfile is still
/// here, `node_modules` is not, so somebody ran an install once and it needs
/// running again.
///
/// A missing directory is only ever reported when the thing that rebuilds it is
/// sitting right there. No lockfile, no claim.
pub fn needed(root: &Path) -> Vec<Step> {
    let mut steps = Vec::new();

    for dir in manifest_dirs(root, 5) {
        let rel = dir
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let at = |cmd: String, why: &str, optional: bool| Step {
            cmd,
            dir: Some(rel.clone()).filter(|d| !d.is_empty()),
            why: why.to_string(),
            optional,
        };

        // Node. Only where a lockfile lives: in a pnpm workspace the packages
        // have a `package.json` and no lock of their own, and the single install
        // at the root is what fills all of their `node_modules`.
        if dir.join("package.json").exists()
            && crate::detect::has_lockfile_in(&dir, crate::rules::NODE_LOCKS)
            && !dir.join("node_modules").exists()
        {
            let pm = crate::detect::detect(&dir)
                .package_manager
                .unwrap_or_else(|| "npm".into());
            let pm = pm.trim_end_matches('?').to_string();
            steps.push(at(format!("{pm} install"), "node_modules", false));

            let prisma =
                dir.join("prisma/schema.prisma").exists() || dir.join("prisma.config.ts").exists();
            if prisma && !scripts(&dir).iter().any(|s| s == "postinstall") {
                steps.push(at(format!("{pm} prisma generate"), "Prisma client", true));
            }
        }

        if dir.join("composer.json").exists()
            && dir.join("composer.lock").exists()
            && !dir.join("vendor").exists()
        {
            steps.push(at("composer install".into(), "Composer packages", false));
        }

        // Bundler only vendors into the project when it was told to, and the file
        // that told it is `.bundle/config` - which is source, so it survived.
        if dir.join("Gemfile.lock").exists()
            && dir.join(".bundle/config").exists()
            && !dir.join("vendor").exists()
        {
            steps.push(at("bundle install".into(), "Ruby gems", false));
        }

        if dir.join("pubspec.yaml").exists() && !dir.join(".dart_tool").exists() {
            steps.push(at("flutter pub get".into(), "Dart packages", false));
        }

        if dir.join("Podfile").exists()
            && dir.join("Podfile.lock").exists()
            && !dir.join("Pods").exists()
        {
            steps.push(at("pod install".into(), "CocoaPods", false));
        }

        if !dir.join(".venv").exists() && !dir.join("venv").exists() {
            if dir.join("uv.lock").exists() {
                steps.push(at("uv sync".into(), "Python virtual environment", false));
            } else if dir.join("poetry.lock").exists() {
                steps.push(at("poetry install".into(), "Python virtual environment", false));
            }
        }

        // Builds. Nothing here is required to have a working checkout, so they
        // are opt-in: `parc setup --build`.
        if dir.join("Cargo.toml").exists() && !dir.join("target").exists() {
            steps.push(at("cargo build".into(), "target/", true));
        }
        if dir.join("Package.swift").exists() && !dir.join(".build").exists() {
            steps.push(at("swift build".into(), ".build/", true));
        }
        if dir.join("pom.xml").exists() && !dir.join("target").exists() {
            let mvn = wrapper(root, &rel, "mvnw", "mvn");
            steps.push(at(format!("{mvn} package -DskipTests"), "target/", true));
        }
        // Only at the Gradle *root* - the directory holding the wrapper or the
        // settings file. Every module in an Android project has its own
        // `build.gradle` and its own `build/`, but `./gradlew build` is run once,
        // from the top; running it inside `android/app` builds nothing.
        let gradle_root = dir.join("gradlew").exists()
            || dir.join("settings.gradle").exists()
            || dir.join("settings.gradle.kts").exists();
        if gradle_root && !dir.join("build").exists() {
            let g = wrapper(root, &rel, "gradlew", "gradle");
            steps.push(at(format!("{g} build"), "Gradle output", true));
        }
    }

    let mut seen = std::collections::HashSet::new();
    steps.retain(|s| seen.insert((s.cmd.clone(), s.dir.clone())));
    steps
}

/// Programs a restore recipe is allowed to invoke.
///
/// `restore` runs the commands in an archive's manifest, and a manifest is
/// unsigned data: an archive that arrived from someone else could carry anything.
/// These are the only binaries `parc` itself ever writes into a recipe, so a
/// command whose program is not one of them means the archive was tampered with or
/// corrupted - and it is refused, not run.
const ALLOWED_PROGRAMS: &[&str] = &[
    "npm", "pnpm", "yarn", "bun", "npx", "node", "cargo", "go", "swift", "pod", "bundle",
    "composer", "flutter", "uv", "poetry", "python", "python3", "pip", "pip3", "cmake", "mvn",
    "mvnw", "gradle", "gradlew",
];

/// Is this a command `parc` could have written - a known build or package-manager
/// invocation, and nothing more?
///
/// Generated recipes join at most two steps with `&&` (CMake configures then
/// builds; a venv is created then filled). Every other shell metacharacter (a
/// `;`, a pipe, a `$(...)`, a redirection, a backgrounding `&`) is how an injected
/// command would chain itself onto a legitimate one, and none of them appear in
/// anything this tool writes. So the command must be spelled with only the
/// characters a real recipe uses, and each `&&`-joined step must start with a
/// program from `ALLOWED_PROGRAMS`.
pub fn is_safe_command(cmd: &str) -> bool {
    // Tool names, flags, version specifiers, relative paths and `&&` all fit in
    // this set; a metacharacter that could start an unrelated command does not.
    if !cmd
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || " ._/@:=+-&".contains(c))
    {
        return false;
    }
    // `&` is allowed only as the `&&` that joins two steps: a lone `&` backgrounds
    // a command, and `&&&` is not something a recipe writes.
    if cmd.replace("&&", "  ").contains('&') {
        return false;
    }
    // Every `&&`-joined step has to start with a program we recognise.
    cmd.split("&&").all(|part| {
        part.split_whitespace().next().is_some_and(|prog| {
            let base = prog.rsplit('/').next().unwrap_or(prog);
            ALLOWED_PROGRAMS.contains(&base)
        })
    })
}

/// Runs the steps, in order, from `root`.
///
/// Returns the index of the first *required* step that failed. Optional steps
/// that fail are reported and stepped over: a build that no longer compiles is a
/// nuisance, a missing dependency tree is a broken project, and the two do not
/// deserve the same reaction.
pub fn execute(root: &Path, steps: &[Step], run_optional: bool) -> Option<usize> {
    // A recipe comes out of an archive's manifest, which is not signed. Before
    // running any of it, refuse the whole recipe if a single command is not a
    // recognised build/package-manager invocation: running the safe steps first
    // and reaching the injected one last is still running it.
    if let Some(i) = steps.iter().position(|s| !is_safe_command(&s.cmd)) {
        eprintln!("  ✗ unrecognised command, not running: {}", steps[i].cmd);
        eprintln!(
            "    not a package-manager/build command - did the archive come from someone \
             else? run the steps by hand."
        );
        return Some(i);
    }

    for (i, s) in steps.iter().enumerate() {
        if s.optional && !run_optional {
            println!("  ▸ {}  (skipped - --build to run)", s.cmd);
            continue;
        }

        let cwd = match &s.dir {
            Some(d) => root.join(d),
            None => root.to_path_buf(),
        };
        println!("  ▸ {}{}", s.dir.as_deref().map(|d| format!("({d}) ")).unwrap_or_default(), s.cmd);

        let ok = std::process::Command::new("sh")
            .arg("-c")
            .arg(&s.cmd)
            .current_dir(&cwd)
            .status()
            .map(|st| st.success())
            .unwrap_or(false);

        if ok {
            continue;
        }
        if s.optional {
            println!("  ✗ {} failed - was optional, continuing.", s.cmd);
            continue;
        }
        return Some(i);
    }
    None
}

/// Whether the runtime on this machine can build what the archive was built
/// with. `engines` is a range, not a pin: ">=20" is *satisfied* by 24, and
/// warning about that trains people to ignore the warning that matters.
pub fn runtime_satisfied(want: &str, have: &str) -> bool {
    let major = |s: &str| -> Option<u32> {
        s.trim_start_matches(['v', '^', '~', '>', '<', '=', ' '])
            .split('.')
            .next()?
            .parse()
            .ok()
    };

    let (Some(want_major), Some(have_major)) = (major(want), major(have)) else {
        // Nothing parseable to compare - saying nothing beats crying wolf.
        return true;
    };

    let spec = want.trim_start();
    if spec.starts_with(">=") || spec.starts_with('>') {
        have_major >= want_major
    } else {
        have_major == want_major
    }
}

/// The commands that turn an extracted archive back into a working checkout.
///
/// Normally built at archive time, while the lockfile, the scripts, the Podfile
/// and exactly which directories we removed are all still in front of us. In two
/// years the person unpacking this can see none of that.
pub fn plan(root: &Path, proj: &Project) -> Vec<Step> {
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

    plan_for(root, proj.package_manager.as_deref(), &proj.stacks, &removed)
}

/// The same recipe, derived from a manifest instead of a live analysis.
///
/// This is what rescues archives written before the plan was recorded: the
/// manifest still says what was removed, and the extracted tree still holds the
/// lockfile and the scripts. An archive whose `restore_plan` is empty is not an
/// archive with nothing to rebuild - telling someone "ready" while their
/// `node_modules` is missing is the exact confusion this tool exists to end.
pub fn plan_for(
    root: &Path,
    package_manager: Option<&str>,
    stacks: &[String],
    items: &[Removed],
) -> Vec<Step> {
    let mut steps = Vec::new();
    let pm = package_manager
        .map(|p| p.trim_end_matches('?'))
        .unwrap_or("npm");
    let has = |s: &str| scripts(root).iter().any(|k| k == s);

    if let Some(size) = removed(items, "node_modules") {
        steps.push(Step {
            cmd: format!("{pm} install"),
            dir: None,
            why: format!("node_modules ({})", crate::render::human(size)),
            optional: false,
        });

        // `postinstall`/`prepare` already run generate for most setups, but
        // plenty of repos never wired it up and just remember to run it.
        let prisma = root.join("prisma/schema.prisma").exists()
            || root.join("prisma.config.ts").exists();
        if prisma && !has("postinstall") {
            steps.push(Step {
                cmd: format!("{pm} prisma generate"),
                dir: None,
                why: "Prisma client".into(),
                optional: true,
            });
        }
    }

    let built: u64 = ["dist", "build", ".next", "out"]
        .iter()
        .filter_map(|r| removed(items, r))
        .sum();
    if built > 0 && has("build") {
        steps.push(Step {
            cmd: format!("{pm} run build"),
            dir: None,
            why: format!("built output ({})", crate::render::human(built)),
            optional: true,
        });
    }

    // CocoaPods only works from the directory holding the Podfile, which is
    // wherever we found `Pods/`.
    for r in items.iter().filter(|r| r.rule == "Pods") {
        let dir = Path::new(&r.path)
            .parent()
            .map(|p| p.to_string_lossy().to_string());
        steps.push(Step {
            cmd: "pod install".into(),
            dir: dir.filter(|d| !d.is_empty()),
            why: format!("CocoaPods ({})", crate::render::human(r.size)),
            optional: false,
        });
    }

    if removed(items, ".dart_tool").is_some() {
        steps.push(Step {
            cmd: "flutter pub get".into(),
            dir: None,
            why: "Dart packages".into(),
            optional: false,
        });
    }

    // `target/` belongs to Cargo and to Maven, and the rule table lets both claim
    // it. Which stack the project is decides which command rebuilds it.
    if let Some(size) = removed(items, "target") {
        if stacks.iter().any(|s| s == "rust") {
            steps.push(Step {
                cmd: "cargo build".into(),
                dir: None,
                why: format!("target/ ({})", crate::render::human(size)),
                optional: true,
            });
        }
        if stacks.iter().any(|s| s == "maven") {
            let dir = items
                .iter()
                .find(|r| r.rule == "target")
                .and_then(|r| owner_dir(root, &r.path, &["pom.xml"]))
                .unwrap_or_default();
            let mvn = wrapper(root, &dir, "mvnw", "mvn");
            steps.push(Step {
                // Tests are not what the archive lost; the compiled output is.
                cmd: format!("{mvn} package -DskipTests"),
                dir: Some(dir).filter(|d| !d.is_empty()),
                why: format!("target/ ({})", crate::render::human(size)),
                optional: true,
            });
        }
    }

    // Gradle keeps its cache in `.gradle/` and its output in `build/`. One build
    // brings back both, so they get one step - but only one, however many nested
    // `build/` directories a multi-module Android project happens to have.
    if stacks.iter().any(|s| s == "gradle") {
        let gradle: Vec<&Removed> = items
            .iter()
            .filter(|r| r.rule == ".gradle" || r.rule == "build")
            .collect();

        if let Some(first) = gradle.first() {
            let size: u64 = gradle.iter().map(|r| r.size).sum();
            let dir = owner_dir(
                root,
                &first.path,
                &["gradlew", "build.gradle", "build.gradle.kts"],
            )
            .unwrap_or_default();
            let g = wrapper(root, &dir, "gradlew", "gradle");
            steps.push(Step {
                cmd: format!("{g} build"),
                dir: Some(dir).filter(|d| !d.is_empty()),
                why: format!("Gradle output ({})", crate::render::human(size)),
                optional: true,
            });
        }
    }

    if let Some(size) = removed(items, ".build") {
        if stacks.iter().any(|s| s == "swift") {
            steps.push(Step {
                cmd: "swift build".into(),
                dir: None,
                why: format!(".build/ ({})", crate::render::human(size)),
                optional: true,
            });
        }
    }

    // CMake builds out of source: the directory we removed is both the cache and
    // the output, and it has to be configured again before it can be built.
    if stacks.iter().any(|s| s == "cmake") {
        if let Some(r) = items.iter().find(|r| r.rule == "build") {
            let dir = owner_dir(root, &r.path, &["CMakeLists.txt"]).unwrap_or_default();
            let name = Path::new(&r.path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "build".into());
            steps.push(Step {
                cmd: format!("cmake -S . -B {name} && cmake --build {name}"),
                dir: Some(dir).filter(|d| !d.is_empty()),
                why: format!("{name}/ ({})", crate::render::human(r.size)),
                optional: true,
            });
        }
    }

    if removed(items, ".venv").is_some() || removed(items, "venv").is_some() {
        let cmd = if root.join("uv.lock").exists() {
            "uv sync"
        } else if root.join("poetry.lock").exists() {
            "poetry install"
        } else {
            "python3 -m venv .venv && .venv/bin/pip install -r requirements.txt"
        };
        steps.push(Step {
            cmd: cmd.into(),
            dir: None,
            why: "Python virtual environment".into(),
            optional: false,
        });
    }

    // `vendor/` means different things to Go and PHP, and the rule table lets
    // both claim it. Whichever stack the project actually is decides the command.
    if let Some(size) = removed(items, "vendor") {
        if stacks.iter().any(|s| s == "go") {
            steps.push(Step {
                cmd: "go mod vendor".into(),
                dir: None,
                why: "Go vendor/".into(),
                optional: false,
            });
        }
        if stacks.iter().any(|s| s == "php") {
            steps.push(Step {
                cmd: "composer install".into(),
                dir: None,
                why: format!("Composer packages ({})", crate::render::human(size)),
                optional: false,
            });
        }
        // Bundler puts the gems wherever `.bundle/config` says, and that config
        // file is source - it is still in the archive. So plain `bundle install`
        // lands them back exactly where they were taken from.
        if stacks.iter().any(|s| s == "ruby") {
            steps.push(Step {
                cmd: "bundle install".into(),
                dir: None,
                why: format!("Ruby gems ({})", crate::render::human(size)),
                optional: false,
            });
        }
    }

    // A recipe is a procedure someone follows at 2am two years from now. The same
    // command listed twice makes them wonder what they missed the first time.
    let mut seen = std::collections::HashSet::new();
    steps.retain(|s| seen.insert((s.cmd.clone(), s.dir.clone())));

    steps
}
