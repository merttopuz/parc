use crate::rules;

use std::fs;
use std::path::{Path, PathBuf};

/// Files that make a directory a project root. Discovery stops descending here.
pub const MARKERS: &[&str] = &[
    "package.json",
    "go.mod",
    "Cargo.toml",
    "pubspec.yaml",
    "pyproject.toml",
    "requirements.txt",
    "Gemfile",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "composer.json",
    "Package.swift",
    "mix.exs",
    "deno.json",
    "CMakeLists.txt",
];

pub fn is_project_root(dir: &Path) -> bool {
    if dir.join(".git").exists() {
        return true;
    }
    if MARKERS.iter().any(|m| dir.join(m).exists()) {
        return true;
    }
    has_ext_entry(dir, "xcodeproj") || has_ext_entry(dir, "xcworkspace")
}

fn has_ext_entry(dir: &Path, ext: &str) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    rd.flatten()
        .any(|e| e.path().extension().is_some_and(|x| x == ext))
}

/// The coarse stack a marker file implies. Cleanup rules key off these, and a
/// project's stacks are collected from the whole tree - a repo whose root holds
/// nothing but `.git` still has real `node_modules` two levels down.
pub fn marker_stack(file: &str) -> Option<&'static str> {
    Some(match file {
        "package.json" => "node",
        "Cargo.toml" => "rust",
        "go.mod" => "go",
        "pubspec.yaml" => "flutter",
        "pyproject.toml" | "requirements.txt" | "setup.py" => "python",
        "pom.xml" => "maven",
        "build.gradle" | "build.gradle.kts" => "gradle",
        "composer.json" => "php",
        "Gemfile" => "ruby",
        "Package.swift" => "swift",
        "CMakeLists.txt" => "cmake",
        _ => return None,
    })
}

pub fn has_lockfile_in(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|f| dir.join(f).exists())
}

/// Can this directory be put back after we delete it?
///
/// The single question `clean` and `archive` are betting the user's work on. It
/// used to be asked as "is there a lockfile somewhere above it", which answered
/// yes in two cases where the truth was no: when the lockfile belonged to a
/// different language, and when the rule had no lockfiles at all - an empty list
/// meant "nothing to pin", which every caller then read as "always safe".
pub fn reversible(root: &Path, artifact: &Path, rule: &rules::Rule) -> bool {
    match &rule.rebuild {
        rules::Rebuild::Cache => true,
        rules::Rebuild::Deps(owners) => deps_pinned(root, artifact, owners),
        rules::Rebuild::Build => rebuilder(root, artifact, rule.dir),
    }
}

/// Why this directory cannot be put back, named as specifically as we can name it.
///
/// Specifically matters. Told that "go.sum / composer.lock / Gemfile.lock" were
/// all missing from a PHP package, nobody learns anything: two of those three were
/// never going to be there. The directory is owned by composer, and the one file
/// that would have made deleting it reversible is `composer.lock`.
pub fn unreversible_note(root: &Path, artifact: &Path, rule: &rules::Rule) -> String {
    match &rule.rebuild {
        rules::Rebuild::Cache => String::new(),
        rules::Rebuild::Build => format!(
            "{}/ looks generated but there is no command to regenerate it - if deleted it will not come back",
            rule.dir
        ),
        rules::Rebuild::Deps(owners) => match owning(artifact, owners) {
            // We know whose directory this is, so we know exactly what is missing.
            Some((dir, owners)) => format!(
                "{} missing ({}/) - if {} is deleted, dependencies cannot be reinstalled at the same versions",
                owners
                    .iter()
                    .map(|o| o.lock_names())
                    .collect::<Vec<_>>()
                    .join(" / "),
                dir.strip_prefix(root)
                    .unwrap_or(&dir)
                    .to_string_lossy()
                    .trim_end_matches('/'),
                rule.dir
            ),
            // No manifest between the directory and the project root claims it. We
            // do not know what put it there, so we certainly do not know what would
            // put it back.
            None => format!(
                "{}/ looks like a dependency tree but no manifest owns it - I do not know what would rebuild it",
                rule.dir
            ),
        },
    }
}

/// Who owns this dependency directory, and by what manifest.
///
/// The manifest sits *beside* the directory, always: `packages/api/node_modules`
/// has `packages/api/package.json`, `ios/Pods` has `ios/Podfile`,
/// `services/payments/vendor` has `services/payments/composer.json`. So this looks
/// in exactly one place and does not go hunting upwards.
///
/// Hunting upwards is what a `research/legacy/venv` was killed by. It had a
/// `requirements.txt` and nothing else - `requirements.txt` names versions, it does
/// not pin them, which is why it is not a lockfile here. The search climbed past it
/// to the repo root, found a `pyproject.toml` with a `poetry.lock` beside it, and
/// called the venv reversible. That `poetry.lock` describes a different environment
/// entirely and `poetry install` at the root never creates that directory.
///
/// A lockfile is allowed to live above its manifest, and only then - see
/// `Owner::workspace`. A manifest is not allowed to live above its directory.
fn owning<'a>(
    artifact: &Path,
    owners: &'a [rules::Owner],
) -> Option<(PathBuf, Vec<&'a rules::Owner>)> {
    let dir = artifact.parent()?;
    let here: Vec<&rules::Owner> = owners
        .iter()
        .filter(|o| dir.join(o.manifest).exists())
        .collect();
    (!here.is_empty()).then(|| (dir.to_path_buf(), here))
}

/// Is the dependency tree at `artifact` pinned by the lockfile of the toolchain
/// that actually owns it?
///
/// Two questions, asked in order and never merged: *whose* directory is this
/// (`owning`), and is *that* toolchain's lockfile there (`pinned_from`). Merging
/// them into one walk that climbs until anything lock-shaped turns up is what let
/// a `go.sum` at a monorepo root vouch for a PHP `vendor/`, and a root
/// `poetry.lock` vouch for a nested `venv` that only ever had a `requirements.txt`.
fn deps_pinned(root: &Path, artifact: &Path, owners: &[rules::Owner]) -> bool {
    match owning(artifact, owners) {
        // Every toolchain with a manifest in the owning directory has to be pinned.
        // A `vendor/` sitting next to both a `go.mod` and a `composer.json` is two
        // dependency trees in one folder, and a lockfile speaks for exactly one of
        // them.
        Some((dir, here)) => here.iter().all(|o| pinned_from(root, &dir, o)),
        // Nothing between the directory and the project root claims it. Whatever it
        // is, we have no idea what would rebuild it.
        None => false,
    }
}

/// The lockfile for one owner: beside its manifest, or - only where the ecosystem
/// really has workspaces - above it.
fn pinned_from(root: &Path, manifest_dir: &Path, owner: &rules::Owner) -> bool {
    let mut dir = Some(manifest_dir);
    while let Some(d) = dir {
        if has_lockfile_in(d, owner.locks) {
            return true;
        }
        if !owner.workspace || d == root {
            return false;
        }
        dir = d.parent();
    }
    false
}

/// Does the project still carry the command that produces this build directory?
///
/// Nothing pins `dist/`, `out/`, `build/` or `target/`, and that was read for a
/// long time as "nothing needs pinning, so deleting is free". It is the reverse:
/// they carry no lockfile because a *command* rebuilds them, so the command is the
/// thing that has to survive. A directory called `out/` in a project with no build
/// script is not build output - it is somebody's results folder wearing the name,
/// and the archive that dropped one held three weeks of model checkpoints and a
/// restore recipe with nothing in it.
///
/// The command has to sit *beside* the directory it certifies: the `package.json`
/// whose `build` script writes this `dist/`, the `CMakeLists.txt` next to this
/// `build/`. It used to be enough for the signal to exist anywhere between the
/// directory and the repo root, and that let the root build script of a Node repo
/// vouch for a git-ignored `analysis/out/` deep in the tree - named `out`, full of
/// hand-computed data, and deleted with nothing to bring it back. A command
/// rebuilds the directory next to it, not one it has never heard of.
///
/// Gradle is the one real exception: a multi-module build declares its modules in a
/// root `settings.gradle`, so `app/build/` is genuine output even though `app/`
/// carries no build file of its own. Only the gradle markers are allowed to vouch
/// from an ancestor; every other toolchain writes its output beside its marker.
fn rebuilder(root: &Path, artifact: &Path, dir_name: &str) -> bool {
    let Some(parent) = artifact.parent() else {
        return false;
    };

    // Gradle, and only gradle, may own a module's `build/` from an ancestor up to
    // the repo root - the module directory itself often holds no build file.
    if dir_name == "build" {
        let gradle = |d: &Path| {
            d.join("gradlew").exists()
                || d.join("settings.gradle").exists()
                || d.join("settings.gradle.kts").exists()
                || d.join("build.gradle").exists()
                || d.join("build.gradle.kts").exists()
        };
        let mut dir = Some(parent);
        while let Some(d) = dir {
            if gradle(d) {
                return true;
            }
            if d == root {
                break;
            }
            dir = d.parent();
        }
    }

    // Every other toolchain writes its output next to the marker that builds it,
    // so the marker is looked for beside the directory and nowhere else.
    match dir_name {
        "target" => parent.join("Cargo.toml").exists() || parent.join("pom.xml").exists(),
        ".build" => parent.join("Package.swift").exists(),
        "storybook-static" => has_script(parent, "build-storybook"),
        // `build` also covers CMake and Flutter, whose files sit beside the dir.
        "build" => {
            parent.join("CMakeLists.txt").exists()
                || parent.join("pubspec.yaml").exists()
                || has_script(parent, "build")
        }
        // `dist`, `out`: node output, beside the package.json that builds it.
        _ => has_script(parent, "build"),
    }
}

/// Does this directory's `package.json` declare the script that rebuilds it?
fn has_script(dir: &Path, name: &str) -> bool {
    let Ok(raw) = fs::read_to_string(dir.join("package.json")) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    v.pointer("/scripts")
        .and_then(|s| s.get(name))
        .is_some_and(|s| !s.is_null())
}

pub struct Detected {
    pub stacks: Vec<String>,
    pub package_manager: Option<String>,
}

pub fn detect(dir: &Path) -> Detected {
    let mut stacks: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        if !stacks.iter().any(|x| x == s) {
            stacks.push(s.to_string());
        }
    };

    let pkg_json = dir.join("package.json");
    if pkg_json.exists() {
        push("node");
        // Framework refinement is a substring check on the raw manifest rather
        // than a JSON parse: we only need a label, and this cannot fail.
        let raw = fs::read_to_string(&pkg_json).unwrap_or_default();
        for (needle, label) in [
            ("\"next\"", "nextjs"),
            ("@nestjs/core", "nestjs"),
            ("\"nuxt\"", "nuxt"),
            ("@angular/core", "angular"),
            ("\"svelte\"", "svelte"),
            ("\"astro\"", "astro"),
            ("\"vite\"", "vite"),
            ("react-native", "react-native"),
            ("\"react\"", "react"),
            ("\"vue\"", "vue"),
            ("\"express\"", "express"),
        ] {
            if raw.contains(needle) {
                push(label);
            }
        }
    }

    if dir.join("Cargo.toml").exists() {
        push("rust");
    }
    if dir.join("go.mod").exists() {
        push("go");
    }
    if dir.join("pubspec.yaml").exists() {
        push("flutter");
    }
    if dir.join("pyproject.toml").exists() || dir.join("requirements.txt").exists() {
        push("python");
    }
    if dir.join("pom.xml").exists() {
        push("maven");
    }
    if dir.join("build.gradle").exists() || dir.join("build.gradle.kts").exists() {
        push("gradle");
    }
    if dir.join("composer.json").exists() {
        push("php");
    }
    if dir.join("Gemfile").exists() {
        push("ruby");
    }
    if dir.join("Package.swift").exists() {
        push("swift");
    }
    if has_ext_entry(dir, "xcodeproj") || has_ext_entry(dir, "xcworkspace") {
        push("xcode");
    }
    if dir.join("CMakeLists.txt").exists() {
        push("cmake");
    }

    let package_manager = if dir.join("pnpm-lock.yaml").exists() {
        Some("pnpm")
    } else if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
        Some("bun")
    } else if dir.join("yarn.lock").exists() {
        Some("yarn")
    } else if dir.join("package-lock.json").exists() {
        Some("npm")
    } else if dir.join("package.json").exists() {
        Some("npm?")
    } else {
        None
    }
    .map(String::from);

    Detected {
        stacks,
        package_manager,
    }
}

/// Extensions whose bytes are already compressed. Feeding them to zstd buys
/// nothing, so the archive-size estimate must not pretend otherwise.
const INCOMPRESSIBLE: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "ico", "heic", "mp4", "mov", "avi", "mkv", "webm",
    "mp3", "wav", "m4a", "aac", "flac", "ogg", "zip", "gz", "tgz", "xz", "zst", "7z", "rar", "bz2",
    "pdf", "woff", "woff2", "jar", "dmg", "pkg", "ipa", "apk", "wasm", "dylib", "so",
];

pub fn is_incompressible(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| INCOMPRESSIBLE.contains(&e.as_str()))
}

/// Extensions a build step does not produce: model weights, datasets, databases,
/// spreadsheets. A directory named `dist/` or `out/` can be genuine build output
/// and still hold one of these, dropped in because the folder was already there.
/// Running the build rebuilds the folder, never this data, so its presence turns
/// an otherwise deletable build directory back into REVIEW.
///
/// Kept high-precision on purpose. A normal web `dist/` is `.js`, `.css`, `.html`,
/// `.map`, images and fonts, none of which are here - so the list stays quiet on
/// real build output, and speaks up only for the checkpoints, exports and local
/// databases that a rebuild could never bring back.
const USER_DATA: &[&str] = &[
    // model weights / ML artifacts
    "safetensors", "ckpt", "pt", "pth", "onnx", "gguf", "ggml", "h5", "hdf5", "pb", "tflite",
    "mlmodel", "mlpackage", "npy", "npz", "pkl", "pickle", "joblib", "caffemodel",
    // datasets / tabular
    "csv", "tsv", "parquet", "arrow", "feather", "avro", "orc", "jsonl", "ndjson", "xlsx", "xls",
    "ods", "dta", "sav", "rdata", "rds",
    // databases / dumps
    "db", "sqlite", "sqlite3", "mdb", "accdb", "dump",
];

/// Does this path name a file no build step would generate? See `USER_DATA`.
pub fn is_user_data(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| USER_DATA.contains(&e.as_str()))
}
