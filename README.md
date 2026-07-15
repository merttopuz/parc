# Project Archive (`parc`)

**Shrink and archive old dev projects without ever losing them.**

`parc` finds your development projects, strips out the parts that can be rebuilt (`node_modules`, `target`, `.venv`, `Pods`...), and archives the rest into a single compressed file, together with a recipe for putting everything back. It is not a disk cleaner: the goal is not to free space, it is to **shrink a project without losing it**.

**English** | [Türkçe](README.tr.md)

---

## The problem

A `~/Projects` folder fills up with things you finished months ago. Deleting them feels wrong: some are pushed to a remote, but many hold an `.env`, a local database, uploads, or uncommitted work that exists nowhere else. So they sit there, and the biggest thing in almost every one of them is a `node_modules` or a `target` you could rebuild in one command.

`parc` separates those two facts. What a lockfile or a build command can rebuild is removed and written down as a step to run later. What only exists on your disk is kept. The result is a small archive you can restore into a working project.

## Install

Built with Rust. From the repo root:

```bash
cargo install --path .        # installs `parc` into ~/.cargo/bin
# or, just build it:
cargo build --release         # binary at ./target/release/parc
```

## Quick start

```bash
parc scan ~/Projects          # what could be saved, read-only
parc backup ~/Projects        # archive everything, touch no project
parc archive ~/Projects/old-thing   # archive it, verify it, then Trash the original
parc list                     # your archive library
parc restore old-thing        # unpack it and rebuild what was stripped
```

Archives are written to `~/Archives` by default (override with `--out` or the `PARC_ARCHIVE_DIR` environment variable).

## Commands

```
parc scan <dir>          scan, report what could be saved         (read-only)
parc plan <project>      what goes into the archive, what doesn't (read-only)
parc clean <dir>         delete node_modules/dist, keep the project (no archive)
parc backup <dir>        archive everything, touch no project
parc archive <project>   archive, verify, then Trash the original (asks first)
parc verify <archive>    recompute every file's sha256
parc show <archive>      what was this: what was removed, what it needs (read-only)
parc restore <archive>   unpack + rebuild what was stripped (install, generate...)
parc list                the archive library
parc rm <archive>        remove an archive from the library (moves it to Trash)
```

`scan` and `clean` take `--older-than <days>`, e.g. `parc clean ~/Projects --older-than 180`.

## The four commands that touch your files (and how they differ)

Three separate questions, three separate answers. What sets them apart is **what happens to your project**:

| Command         | What happens to the project              | Writes an archive |
|-----------------|------------------------------------------|-------------------|
| `scan`, `plan`  | nothing                                  | no                |
| `backup`        | **nothing**                              | yes               |
| `clean`         | generated folders deleted, rest stays    | no                |
| `archive`       | **moved to the Trash**                    | yes               |

- **`clean`** is for projects you are still working on. `node_modules`, `dist`, `target` go; source, `.env`, `.git`, and lockfiles stay. The project keeps working and `pnpm install` brings everything back. This is where most of the space is, and it needs no archive: everything it deletes can be reinstalled from a lockfile.
- **`backup`** finds every project under a directory and archives each one. It never touches the projects, it just files a shrunken copy away. There is no delete flag, and no code path to one. You can run it weekly.
- **`archive`** is for projects you are done with: it archives, reads the archive back off the disk and verifies every byte, and **only then** moves the original to the Trash. It asks "continue?" first (skip with `--yes`). This command removes the project from your disk, which is the point.

Both destructive commands, `clean` and `archive`, do nothing without asking. `scan`, `plan`, and `backup` never touch your project under any circumstances.

## What gets removed, and how it comes back

Every archive **carries its own recipe** for what was stripped and what puts it back (`manifest.json` -> `removed` + `restore_plan`). You see it three times: when `archive` finishes, any time via `parc show <archive>`, and during `restore`.

```
$ parc show acme-backend
  acme-backend
  came from      ~/Desktop/acme/backend
  stack          node, nestjs, react  ·  pnpm
  runtime        node >=20
  size           546 KB archive  ·  727 MB original

  removed (720 MB):
    node_modules     720 MB  rule: node_modules

  to bring it back:
    1. pnpm install            -> node_modules (720 MB)
    2. pnpm prisma generate    -> Prisma client  (optional)
```

**`restore` runs those steps for you** - no extra flag needed. A restored project is ready to run, so you never have to reverse-engineer "what was this, what did I need to install?". To get only the files and skip the commands: `--no-setup`.

The recipe is written **at archive time**, because that is the only moment we can see the lockfile, the `package.json` scripts, the Podfile, and exactly what was removed. Lockfiles are never removed - a lockfile is what turns `pnpm install` from a hope into a promise. Only what can be regenerated from one is removed (`node_modules`, `dist`, `target`, `Pods`, `.venv`, `vendor`...).

What gets cleaned per stack, and what brings it back:

| Stack   | Cleaned                              | Brought back by                          |
|---------|--------------------------------------|------------------------------------------|
| Node    | node_modules, .next, dist, .turbo... | `pnpm install` (+ prisma generate, build)|
| Rust    | target/                              | `cargo build`                            |
| Python  | .venv, __pycache__...                | `uv sync` / `poetry install` / venv+pip  |
| PHP     | vendor/                              | `composer install`                       |
| Ruby    | vendor/                              | `bundle install`                         |
| Go      | vendor/                              | `go mod vendor`                          |
| Flutter | .dart_tool, build/                   | `flutter pub get`                        |
| iOS     | Pods/, DerivedData                   | `pod install`                            |

**Generated does not mean regenerable.** Some folders are deliberately left off the list: they are generated, but no command rebuilds them from source, so they would be data loss wearing an artifact's coat. `log/` (last year's production log), `tmp/` (Rails caches and pids, but also ActiveStorage's uploaded blobs in `tmp/storage`), `.wrangler/state` (a local D1 database, KV, R2, months of seeded data), `.vercel/` (the link binding a directory to a Vercel project). These are exactly what an archive exists to carry, so they surface as **archive-only** files instead of deletion candidates.

If a required step fails (a two-year-old lockfile may not resolve today), `restore` says so and stops: **the source is unpacked in full**, nothing is lost.

## Restore

```bash
parc restore <archive>              # unpack, then run the recipe (asks where)
parc restore <archive> --original   # back where it came from, no prompt
parc restore <archive> --into <dir> # under this directory, no prompt
parc restore <archive> --no-setup   # files only, skip the commands
```

The folder always comes back under its **original name** (`health`), not the archive's filename (`acme-health`). In a pipeline or script the prompt is skipped and it unpacks into the current directory.

## Encryption

An archive carries the files git refused to keep - `.env`, service keys, `secrets/`. That is exactly why it beats a `git clone`, but it also means the file on disk is a plaintext copy of all a project's secrets. For the ones you care about:

```bash
parc archive <project> --encrypt      # prompts for a passphrase (twice)
```

`--encrypt` works on both `backup` and `archive`. It produces `<name>.parc.tar.zst.age` using the [age](https://age-encryption.org) format (passphrase-based, scrypt). It is not a made-up format: even if `parc` disappeared, `age -d project.parc.tar.zst.age | tar --zstd -x` gives everything back. Encryption is the outermost layer (tar -> zstd -> age), so what decrypts is an ordinary `.tar.zst`.

Encrypted or not, the archive file is written `0600` (owner-read only): a plaintext archive is a copy of every secret a project kept, and no other account on the machine has a reason to read it.

`show`, `verify`, and `restore` prompt for the passphrase; in scripts, pass it via `PARC_PASSPHRASE`. **`list` never prompts** - a library can hold archives with different passphrases. The cost: an encrypted archive shows nothing in `list` but its size.

> **Lose the passphrase and the project is gone.** There is no recovery path, and its absence is what encryption means.

## The archive format

`<name>.parc.tar.zst` - a plain tar.zst, no proprietary format. Even if `parc` vanished, `tar --zstd -xf` opens it. The first record is `.parc/manifest.json`: every file's sha256, what was removed, git status, the stack, the package manager.

The name is `<last-two-dirs>-<fingerprint>`, e.g. `app-backend-a3f19c`. The readable part tells you where the project sat; the fingerprint is a hash of its path, so that `acme/app/backend` and `globex/app/backend` do not become the same file. You never have to type the fingerprint - `parc restore app-backend` finds it as long as it names one archive.

Re-archiving a project overwrites its own archive (`--overwrite`) rather than stacking a second copy beside it. The old one goes to the **Trash**, never deleted outright.

## Deleting archives

`parc rm <archive>` removes an archive from the library - it does not delete the file, it moves it to the Trash. If `list` marks an archive **archive-only**, it is the only copy of that project left, and `rm` refuses:

```
$ parc rm acme-backend
Error: this archive is the only copy - the original is not on disk:
    acme-backend  (546 KB)
      came from: ~/Desktop/acme/backend  - no longer there
  Restore it first with `parc restore <name>`, or pass --force if you really mean it.
```

`--yes` does **not** override this; it only skips the confirmation prompt. Deleting a last copy needs a deliberate `--force`. If any one archive in a request is blocked, none are deleted.

## How it decides what is safe to delete

The design in one line: **rules are an allowlist, not a blocklist.** A folder is a deletion candidate only if its name is in the table in [`rules.rs`](src/rules.rs). Being "big" is never a reason.

- **`.gitignore` is a signal, not an exclude list.** It lists exactly what git does *not* have - `.env`, the local database, `uploads/`. Those are the reason the archive exists, not files to skip.
- **Git vouches for artifacts.** If git ignores a `dist/`, it is generated and can go. If git *tracks* what is inside it, that is source and is untouchable. Neither? A human decides (`REVIEW`).
- **Nothing is deleted unless something can put it back.** For every folder there is one question: *who rebuilds this?* A cache (nobody, the tool refills it), a dependency tree (the lockfile that owns it), or build output (the command that produces it). No answer, no deletion.
- **The order is non-negotiable.** Simulate cleanup -> write the archive -> read it back off disk and re-hash every byte -> walk the tree a second time and compare -> only then touch the original.
- **Deleting is not `rm`.** The original goes to the Trash. `parc rm` moves an archive to the Trash too. An archive can be a project's last copy, so no archive is ever deleted outright.

Every rule here exists because an earlier version got a real project wrong. The test suite (`cargo test`) is entirely safety cases - each one locks in a mistake the tool must never make again.

## Verdicts

| Verdict     | Meaning                                                                           |
|-------------|-----------------------------------------------------------------------------------|
| `REDUNDANT` | Complete on a remote. No archive needed - delete it, `git clone` brings it back.   |
| `ARCHIVE`   | Holds something git has no copy of. Cannot be deleted without archiving.           |
| `REVIEW`    | Could not decide automatically. `archive` refuses it without `--force`.            |

## Roadmap

- `.parcignore` - per-project rule overrides
- `parc scan --older-than 180d` + scheduled archiving via launchd
- content-addressed dedup (do not store the same `.git` twice)
- cloud targets (S3 / R2)

## License

MIT
