use crate::model::GitInfo;
use serde::{Deserialize, Serialize};

/// Bumped when the on-disk layout changes in a way older binaries cannot read.
pub const FORMAT_VERSION: u32 = 1;

/// Always the first entry in the tar, so a reader can learn what it is holding
/// without decompressing the whole stream.
pub const MANIFEST_PATH: &str = ".parc/manifest.json";

#[derive(Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LinkEntry {
    pub path: String,
    pub target: String,
}

/// A directory that was removed instead of archived. Kept so a restore can tell
/// the user exactly what it is about to regenerate - and so nobody has to guess
/// later whether the archive is missing something or never had it.
#[derive(Debug, Serialize, Deserialize)]
pub struct Removed {
    pub path: String,
    pub size: u64,
    pub rule: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format_version: u32,
    pub tool_version: String,
    pub name: String,
    pub created_at: i64,
    pub created_at_iso: String,
    pub original_path: String,

    pub stacks: Vec<String>,
    pub package_manager: Option<String>,
    pub git: GitInfo,

    /// The commands that rebuild what was removed, decided while we could still
    /// see the project. Empty for archives written before this existed.
    #[serde(default)]
    pub restore_plan: Vec<crate::recipe::Step>,
    /// e.g. "node 22" - from .nvmrc or engines. What it was built against.
    #[serde(default)]
    pub runtime: Option<String>,

    /// Size on disk before anything was removed.
    pub original_size: u64,
    /// Uncompressed size of what actually went into the archive.
    pub archived_size: u64,
    pub removed: Vec<Removed>,

    pub files: Vec<FileEntry>,
    pub links: Vec<LinkEntry>,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn iso(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Howard Hinnant's civil-from-days. Cheaper than pulling in a date crate for
/// the one timestamp we ever format.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
