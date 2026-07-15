//! Optional passphrase encryption, for archives that are opt-in secret.
//!
//! An archive deliberately holds the files git refused to keep: `.env`, service
//! keys, a `secrets/` directory. That is the whole reason it beats `git clone` -
//! and it also means the file on disk is a plaintext copy of every credential a
//! project ever had. `--encrypt` is the answer for the archives where that
//! matters.
//!
//! The format is [age] with a passphrase (scrypt), not something invented here.
//! `parc` disappearing must never be what stands between someone and their
//! project: `age -d foo.parc.tar.zst.age | tar --zstd -x` gets everything back
//! with tools that will outlive this program.
//!
//! [age]: https://age-encryption.org

use age::secrecy::SecretString;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, IsTerminal, Read};
use std::path::Path;

/// Encrypted archives carry the plain extension plus age's own, so that what a
/// file is stays obvious to `file(1)`, to `age`, and to a human in two years.
pub const ENC_EXT: &str = ".parc.tar.zst.age";

/// For scripts and cron, where nobody is there to type anything.
pub const ENV_KEY: &str = "PARC_PASSPHRASE";

// There is deliberately no `is_encrypted(path) -> bool` that looks at the name.
// One existed, it had exactly one caller, and that caller was the passphrase
// prompt - so an encrypted archive whose `.age` had been renamed away could not be
// opened by `show`, `verify` or `restore` at all. The extension is a label; the
// magic is the fact. Ask `is_encrypted_content`.

/// Is this file actually age-encrypted, by its first bytes rather than its name?
///
/// The extension is a convenience; the magic is the truth. A last-copy archive
/// someone renamed away from `.age` is still encrypted, and `rm` treating it as
/// plaintext - failing to read it, then deleting it as "corrupt" without
/// `--force` - is how the only copy of a project could go. This is what `open`
/// keys off, so name-based and content-based callers agree.
pub fn is_encrypted_content(path: &Path) -> bool {
    let Ok(f) = File::open(path) else {
        return false;
    };
    let mut r = BufReader::new(f);
    matches!(r.fill_buf(), Ok(head) if head.starts_with(MAGIC))
}

/// The passphrase from the environment, if it is there. Never prompts - callers
/// that must not block on a human (like `rm`) use this and accept not knowing.
pub fn env_passphrase() -> Option<SecretString> {
    std::env::var(ENV_KEY)
        .ok()
        .filter(|v| !v.is_empty())
        .map(SecretString::from)
}

/// Ask for the passphrase: environment first, then a prompt that does not echo.
///
/// `confirm` is for the one moment it is worth double-typing - writing a new
/// archive. Getting it wrong there means the archive can never be opened, and
/// there is no second copy to check it against.
pub fn passphrase(confirm: bool) -> Result<SecretString> {
    if let Some(p) = env_passphrase() {
        return Ok(p);
    }

    if !io::stdin().is_terminal() {
        bail!(
            "passphrase required but no terminal - provide it via {ENV_KEY}",
        );
    }

    let pass = rpassword::prompt_password("  passphrase: ").context("passphrase could not be read")?;
    if pass.is_empty() {
        bail!("empty passphrase not accepted");
    }
    if confirm {
        let again = rpassword::prompt_password("  passphrase (again): ")?;
        if again != pass {
            bail!("passphrases do not match - archive not written");
        }
    }
    Ok(SecretString::from(pass))
}

/// A writer that encrypts everything written to it. Must be `finish`ed, or the
/// last chunk and the authentication tag never reach the disk.
pub fn encrypt_to(dest: &Path, pass: &SecretString) -> Result<age::stream::StreamWriter<BufWriter<File>>> {
    // Same `0600` as a plaintext archive. The ciphertext protects the contents,
    // but the file mode is one uniform rule for both so no archive is ever the
    // odd one out at `0644`.
    let out = BufWriter::new(crate::archive::create_private(dest)?);
    Ok(age::Encryptor::with_user_passphrase(pass.clone()).wrap_output(out)?)
}

/// Every age file starts with this. Sniffing the bytes beats trusting the name:
/// the archive is verified while it is still a `.partial` temp file, and a name
/// can be changed by anyone with a mouse.
const MAGIC: &[u8] = b"age-encryption.org/";

/// The archive's bytes, decrypted if they need to be. Everything downstream -
/// zstd, tar, the manifest - is identical either way, which is what keeps
/// encryption from forking every code path that reads an archive.
pub fn open(path: &Path, pass: Option<&SecretString>) -> Result<Box<dyn Read>> {
    let mut f = BufReader::new(File::open(path).with_context(|| format!("{}", path.display()))?);

    let encrypted = {
        let head = f.fill_buf().context("archive could not be read")?;
        head.starts_with(MAGIC)
    };

    if !encrypted {
        return Ok(Box::new(f));
    }

    let Some(pass) = pass else {
        bail!("encrypted archive - passphrase required");
    };

    let decryptor = age::Decryptor::new(f).context("age header could not be read - corrupt archive?")?;
    let mut identity = age::scrypt::Identity::new(pass.clone());
    // age benchmarks a scrypt work-factor ceiling on *this* machine at decrypt
    // time and refuses anything above it + 4. The archive's work factor was fixed
    // on the machine that wrote it, so one encrypted on a fast desktop and opened
    // on a slower or busy machine can exceed that ceiling and be refused *with the
    // right passphrase* - the exact case where the archive is the only copy and
    // parc must not be the thing that loses it. Raising the ceiling costs a real
    // archive nothing (it does the work its own factor dictates, ~1s), and still
    // caps a hostile file's scrypt cost.
    identity.set_max_work_factor(22);
    let reader = decryptor
        .decrypt(std::iter::once(&identity as _))
        .map_err(|e| match e {
            // Distinguished from a wrong passphrase on purpose: telling someone
            // their correct passphrase is wrong is how a recoverable archive gets
            // written off as dead.
            age::DecryptError::ExcessiveWork { .. } => anyhow::anyhow!(
                "archive is encrypted with a scrypt setting too heavy to open on this \
                 machine - try opening it on a more powerful machine"
            ),
            // Otherwise: age authenticates, so a wrong key is indistinguishable
            // from a corrupt file and both surface here.
            _ => anyhow::anyhow!("wrong passphrase (or corrupt archive)"),
        })?;

    Ok(Box::new(reader))
}

/// Flush an encrypted stream all the way down to the *disk*.
///
/// All the way down is the point, and it is why this does not stop at `flush`.
/// See `archive::sync_file`: a flush hands the bytes to the kernel, which is not
/// the same place as the platter, and `archive` throws the project away on the
/// strength of an archive it read back out of the page cache.
pub fn finish(w: age::stream::StreamWriter<BufWriter<File>>) -> Result<()> {
    let file = w
        .finish()?
        .into_inner()
        .map_err(|e| e.into_error())
        .context("encrypted archive could not be written")?;
    crate::archive::sync_file(&file)
}
