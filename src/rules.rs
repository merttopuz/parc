/// What it takes to put a directory back after deleting it.
///
/// This used to be a flat list of lockfiles, and an empty list meant "nothing
/// needs pinning here". Every caller read that as "always reversible", which is
/// the exact opposite of what it means for a build directory: nothing *pins*
/// `out/` because a *command* rebuilds it - and a project that no longer carries
/// that command cannot get it back at all. An `out/` holding three weeks of model
/// checkpoints, gitignored because of course it was, went that way: deleted as a
/// build artifact, with an empty restore recipe attached to the archive.
pub enum Rebuild {
    /// The tool that owns it refills it on next use, out of source alone.
    /// Deleting it costs time and never data.
    Cache,
    /// A dependency tree. Reversible only while the lockfile that pins it
    /// survives - and only the lockfile of the toolchain that owns *this*
    /// directory counts. See `Owner`.
    Deps(&'static [Owner]),
    /// Build output. Reversible only while the project still carries the command
    /// that produces it.
    Build,
}

/// Which toolchain owns a directory, and what pins it.
///
/// `vendor/` is why this exists. Go, PHP and Ruby all vendor into a directory of
/// that name, and their lockfiles were once pooled into one flat list - so a
/// `go.sum` at the repo root authorized deleting `services/payments/vendor`, a PHP
/// tree with no `composer.lock` anywhere, which `parc setup` then had no way to
/// reinstall. A lockfile only speaks for its own toolchain, and the manifest
/// sitting *with* the directory is what says whose the directory is.
pub struct Owner {
    /// The manifest whose presence means this toolchain owns the directory.
    pub manifest: &'static str,
    /// Any one of these pins it. Several of them are alternatives for one
    /// ecosystem (npm/pnpm/yarn/bun), never different languages.
    pub locks: &'static [&'static str],
    /// May a lockfile *above* the manifest vouch for it?
    ///
    /// True only where the ecosystem genuinely has workspaces: one `pnpm install`
    /// at the root of a monorepo really does fill `packages/api/node_modules`. A
    /// `poetry.lock` at the root does *not* create `research/legacy/venv` - that
    /// venv was pinned by a `requirements.txt`, which pins nothing - and a
    /// `composer.lock` upstairs does not vendor a different package.
    pub workspace: bool,
}

/// A directory that some toolchain regenerates from source.
///
/// Rules are an allowlist. A directory is only ever a deletion candidate if it
/// matches a rule here - size is never a reason to touch anything.
pub struct Rule {
    /// Exact directory name, matched anywhere inside the project.
    pub dir: &'static str,
    /// Only applies if the project was detected as one of these stacks.
    /// Empty means any stack.
    pub stacks: &'static [&'static str],
    /// The name also has legitimate non-generated uses (`dist/`, `build/`,
    /// `vendor/`). Requires corroboration from git before we trust it.
    pub ambiguous: bool,
    /// What would have to survive for deleting this to be reversible.
    pub rebuild: Rebuild,
    /// The toolchain owns every byte in here; nothing hand-written lives in it.
    ///
    /// This is the *only* thing that lets an ignored **ancestor** vouch for a
    /// directory. Expo gitignores `/ios` wholesale because it generates the whole
    /// native tree, and the `Pods/` inside it is still Pods with a `Podfile.lock`
    /// beside it - so `Pods` opts in.
    ///
    /// `vendor/` does not, and that is not a detail. Rails keeps real,
    /// hand-written source in `vendor/assets` and `vendor/javascript`, and an
    /// ignored ancestor plus any old `Gemfile.lock` was enough to delete it: a
    /// project sitting in a directory its parent repo ignored lost source that
    /// existed nowhere else.
    pub owned: bool,
}

impl Owner {
    /// "composer.lock" - or "poetry.lock / uv.lock" where either would do.
    pub fn lock_names(&self) -> String {
        self.locks.join(" / ")
    }
}

/// Whatever the project's package manager happens to be, one of these pins the
/// `node_modules` tree.
pub const NODE_LOCKS: &[&str] = &[
    "pnpm-lock.yaml",
    "bun.lockb",
    "bun.lock",
    "yarn.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
];

/// A `requirements.txt` is not on this list on purpose: it names versions, it
/// does not pin them. Rebuilding a venv from one gives you a different tree.
const PYTHON_OWNERS: &[Owner] = &[
    Owner {
        manifest: "pyproject.toml",
        locks: &["poetry.lock", "uv.lock"],
        workspace: false,
    },
    Owner {
        manifest: "Pipfile",
        locks: &["Pipfile.lock"],
        workspace: false,
    },
];

const NODE_OWNERS: &[Owner] = &[Owner {
    manifest: "package.json",
    locks: NODE_LOCKS,
    // The one ecosystem here that really does install from a root lockfile into a
    // package's own `node_modules`.
    workspace: true,
}];

const POD_OWNERS: &[Owner] = &[Owner {
    manifest: "Podfile",
    locks: &["Podfile.lock"],
    workspace: false,
}];

/// Go, PHP and Ruby, kept apart on purpose. Each `vendor/` is put back by its own
/// toolchain's lockfile and by nothing else.
const VENDOR_OWNERS: &[Owner] = &[
    Owner {
        manifest: "go.mod",
        locks: &["go.sum"],
        workspace: false,
    },
    Owner {
        manifest: "composer.json",
        locks: &["composer.lock"],
        workspace: false,
    },
    Owner {
        manifest: "Gemfile",
        locks: &["Gemfile.lock"],
        workspace: false,
    },
];

use Rebuild::{Build, Cache, Deps};

const R: &[Rule] = &[
    // Node / JS
    Rule { dir: "node_modules",     stacks: &["node"],                  ambiguous: false, rebuild: Deps(NODE_OWNERS), owned: true  },
    Rule { dir: ".next",            stacks: &["node"],                  ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".nuxt",            stacks: &["node"],                  ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".svelte-kit",      stacks: &["node"],                  ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".astro",           stacks: &["node"],                  ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".angular",         stacks: &["node"],                  ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".turbo",           stacks: &["node"],                  ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".parcel-cache",    stacks: &["node"],                  ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".vite",            stacks: &["node"],                  ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: "coverage",         stacks: &["node", "ruby"],          ambiguous: true,  rebuild: Cache,             owned: false },
    Rule { dir: ".cache",           stacks: &["node"],                  ambiguous: true,  rebuild: Cache,             owned: false },
    Rule { dir: "storybook-static", stacks: &["node"],                  ambiguous: true,  rebuild: Build,             owned: false },
    Rule { dir: "dist",             stacks: &["node"],                  ambiguous: true,  rebuild: Build,             owned: false },
    Rule { dir: "out",              stacks: &["node"],                  ambiguous: true,  rebuild: Build,             owned: false },
    Rule { dir: "build",            stacks: &["node", "flutter", "gradle", "cmake"], ambiguous: true, rebuild: Build, owned: false },

    // Rust / Java
    Rule { dir: "target",           stacks: &["rust", "maven"],         ambiguous: true,  rebuild: Build,             owned: false },

    // Flutter / Dart
    Rule { dir: ".dart_tool",       stacks: &["flutter"],               ambiguous: false, rebuild: Cache,             owned: false },

    // Android / Gradle
    Rule { dir: ".gradle",          stacks: &["gradle"],                ambiguous: false, rebuild: Cache,             owned: false },

    // Apple
    Rule { dir: "DerivedData",      stacks: &["xcode"],                 ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".build",           stacks: &["swift"],                 ambiguous: false, rebuild: Build,             owned: false },
    Rule { dir: "Pods",             stacks: &["xcode"],                 ambiguous: true,  rebuild: Deps(POD_OWNERS),  owned: true  },

    // Python
    Rule { dir: "__pycache__",      stacks: &["python"],                ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".pytest_cache",    stacks: &["python"],                ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".mypy_cache",      stacks: &["python"],                ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".ruff_cache",      stacks: &["python"],                ambiguous: false, rebuild: Cache,             owned: false },
    Rule { dir: ".venv",            stacks: &["python"],                ambiguous: true,  rebuild: Deps(PYTHON_OWNERS), owned: true },
    Rule { dir: "venv",             stacks: &["python"],                ambiguous: true,  rebuild: Deps(PYTHON_OWNERS), owned: true },

    // Go / PHP / Ruby. All three vendor into `vendor/`, and in Ruby it is
    // `vendor/bundle` - but a rule is a directory *name*, and the walk stops at the
    // first name it recognises, so `vendor` is the only level at which any of them
    // can be caught. It is therefore also the level at which Rails' hand-written
    // `vendor/assets` lives, which is why this is `owned: false`: an ignored
    // ancestor may never vouch for it, and only git naming this exact directory
    // will do.
    Rule { dir: "vendor",           stacks: &["go", "php", "ruby"],     ambiguous: true,  rebuild: Deps(VENDOR_OWNERS), owned: false },
];

// Deliberately absent, and each for the same reason: generated is not the same
// thing as regenerable, and a rule is a directory *name* - it cannot reach inside
// and keep the half that matters.
//
//   log/        Ruby. No command rebuilds last year's production log.
//   tmp/        Ruby. Rails' cache and pids live here - and so do ActiveStorage's
//               disk blobs, in `tmp/storage`. Every file a user ever uploaded to a
//               dev instance, with nothing anywhere that puts them back.
//   .wrangler/  Cloudflare. `.wrangler/state` is the local D1 database, the KV
//               store, R2 and Durable Object state: months of seeded development
//               data that no command regenerates.
//   .vercel/    Holds `project.json`, which links the directory to a Vercel
//               project. Kilobytes, and re-linking is a manual step.
//
// Left out of the table they are not deletion candidates at all, and they surface
// as orphans - which is what they are: things git has no copy of, and the reason
// the archive exists.

pub fn match_dir(name: &str, stacks: &[String]) -> Option<&'static Rule> {
    R.iter().find(|r| {
        r.dir == name
            && (r.stacks.is_empty() || r.stacks.iter().any(|s| stacks.iter().any(|d| d == s)))
    })
}

/// Any rule with this directory name, regardless of stack. Used to tell an
/// ignored build directory apart from an ignored `.env` when listing orphans.
pub fn is_known_artifact_name(name: &str) -> bool {
    R.iter().any(|r| r.dir == name)
}
