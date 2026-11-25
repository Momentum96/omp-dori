//! The program folder (DESIGN 11.2, 11.3; IR req. 13-31, 35): versions side by side, one `current`
//! link, a record per version, a lock and a staging folder; the command link and OMP's extension
//! link both point through `current`, so one rename activates a version.
//!
//! ```text
//! <root>/versions/<v>/{cataly, cataly.ts}   read-only after verification
//! <root>/current -> versions/<v>
//! <root>/records/<v>.json                    version, commit, target, hashes, source, time, role
//! <root>/lock, <root>/staging/, <root>/silenced, <root>/service
//! ```

use std::fs::{self, DirBuilder, Permissions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::fs::{sync_dir, write_atomic};
use super::release::{
    self, BINARY_MAX, EXTENSION_MAX, SIGNATURE, SIGNATURE_MAX, SUMS, SUMS_MAX, Sums, TARGET,
};

/// The two files of a version folder.
pub(crate) const BINARY: &str = "cataly";
pub(crate) const EXTENSION: &str = "cataly.ts";

/// Where everything lives.
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub(crate) root: PathBuf,
    /// `~/.local/bin/cataly`.
    pub(crate) command: PathBuf,
    /// `<OMP agent dir>/extensions/cataly.ts`.
    pub(crate) extension: PathBuf,
}

impl Layout {
    /// The layout for this user; an absolute `prefix` replaces the program folder (IR req. 13).
    pub(crate) fn from_env(prefix: Option<&Path>) -> Result<Self, String> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())
            .ok_or("HOME is not set to an absolute path")?;
        let root = match prefix {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => return Err(format!("{} is not an absolute path", p.display())),
            None => std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .filter(|d| d.is_absolute())
                .unwrap_or_else(|| home.join(".local/share"))
                .join("cataly"),
        };
        Ok(Self {
            root,
            command: home.join(".local/bin").join(BINARY),
            extension: crate::omp::extension::link()?,
        })
    }

    /// The layout of the running binary when it runs from a version folder (so an installed
    /// binary acts on its own program folder whatever the environment says), else the default.
    pub(crate) fn of_running() -> Result<Self, String> {
        let exe = std::env::current_exe()
            .and_then(fs::canonicalize)
            .ok()
            .and_then(|exe| {
                let versions = exe.parent()?.parent()?;
                (versions.file_name()? == "versions")
                    .then(|| versions.parent().map(Path::to_path_buf))?
            });
        Self::from_env(exe.as_deref())
    }

    pub(crate) fn versions(&self) -> PathBuf {
        self.root.join("versions")
    }
    pub(crate) fn version_dir(&self, version: &str) -> PathBuf {
        self.versions().join(version)
    }
    pub(crate) fn current(&self) -> PathBuf {
        self.root.join("current")
    }
    fn records(&self) -> PathBuf {
        self.root.join("records")
    }
    fn record_file(&self, version: &str) -> PathBuf {
        self.records().join(format!("{version}.json"))
    }
    pub(crate) fn lock(&self) -> PathBuf {
        self.root.join("lock")
    }
    pub(crate) fn staging(&self) -> PathBuf {
        self.root.join("staging")
    }
    /// The version the owner rolled back from: never announced again (IR req. 37).
    pub(crate) fn silenced(&self) -> PathBuf {
        self.root.join("silenced")
    }
    /// Present when `cataly service enable` created the user service (so uninstall removes only its own).
    pub(crate) fn service_marker(&self) -> PathBuf {
        self.root.join("service")
    }
    /// Where the command link must point.
    pub(crate) fn command_target(&self) -> PathBuf {
        self.current().join(BINARY)
    }
    pub(crate) fn extension_target(&self) -> PathBuf {
        self.current().join(EXTENSION)
    }

    /// The version the `current` link names, if it names one.
    pub(crate) fn current_version(&self) -> Option<String> {
        let target = fs::read_link(self.current()).ok()?;
        let name = target.file_name()?.to_str()?.to_string();
        (target == Path::new("versions").join(&name)).then_some(name)
    }

    /// Every record, by version.
    pub(crate) fn read_records(&self) -> Vec<Record> {
        let Ok(dir) = fs::read_dir(self.records()) else {
            return Vec::new();
        };
        dir.filter_map(Result::ok)
            .filter_map(|e| {
                let bytes = super::fs::read_private(&e.path(), 64 * 1024).ok()??;
                serde_json::from_slice::<Record>(&bytes).ok()
            })
            .collect()
    }

    pub(crate) fn record(&self, version: &str) -> Option<Record> {
        self.read_records()
            .into_iter()
            .find(|r| r.version == version)
    }

    pub(crate) fn write_record(&self, record: &Record) -> Result<(), String> {
        ensure_dir(&self.records(), 0o700)?;
        let bytes = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
        write_atomic(&self.record_file(&record.version), &bytes).map_err(|e| e.to_string())
    }
}

/// What is known of one installed version (IR req. 18). It holds no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    pub(crate) version: String,
    pub(crate) commit: String,
    pub(crate) target: String,
    pub(crate) binary_sha256: String,
    pub(crate) extension_sha256: String,
    /// The release URL or the local folder.
    pub(crate) source: String,
    pub(crate) installed_ms: i64,
    /// `current`, `previous` or `kept` (about to be removed).
    pub(crate) role: String,
}

/// Creates `dir` (and missing parents) with `mode` on the last component when absent.
fn ensure_dir(dir: &Path, mode: u32) -> Result<(), String> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    match DirBuilder::new().mode(mode).create(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}

/// Creates the program folder (0700) when absent and checks it is a real folder owned by the user
/// and not group- or world-writable (IR req. 15).
pub(crate) fn prepare_root(layout: &Layout) -> Result<(), String> {
    ensure_dir(&layout.root, 0o700)?;
    check_root(&layout.root)
}

fn check_root(root: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(root).map_err(|e| format!("{}: {e}", root.display()))?;
    if !meta.is_dir() || meta.uid() != super::own_uid() || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} must be a folder owned by you and not writable by group or others (chmod 700)",
            root.display()
        ));
    }
    Ok(())
}

/// Reads at most `cap` bytes of a small file; larger is an error naming it.
fn read_small(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
    let name = path.display();
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| format!("{name}: {e}"))?
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{name}: {e}"))?;
    if bytes.len() as u64 > cap {
        return Err(format!("{name} is larger than {cap} bytes"));
    }
    Ok(bytes)
}

/// Copies `src` into a new owner-only file `dst`, at most `cap` bytes, returning its SHA-256
/// (IR req. 24: hashed while writing).
fn copy_hashed(src: &Path, dst: &Path, cap: u64) -> Result<String, String> {
    let mut input = fs::File::open(src)
        .map_err(|e| format!("{}: {e}", src.display()))?
        .take(cap + 1);
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)
        .map_err(|e| format!("{}: {e}", dst.display()))?;
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = input
            .read(&mut buf)
            .map_err(|e| format!("{}: {e}", src.display()))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            return Err(format!("{} is larger than {cap} bytes", src.display()));
        }
        let chunk = buf.get(..n).unwrap_or_default();
        ctx.update(chunk);
        out.write_all(chunk)
            .map_err(|e| format!("{}: {e}", dst.display()))?;
    }
    out.sync_all()
        .map_err(|e| format!("{}: {e}", dst.display()))?;
    Ok(super::hex(ctx.finish().as_ref()))
}

/// A verified candidate in its staging folder.
#[derive(Debug)]
pub(crate) struct Staged {
    pub(crate) dir: PathBuf,
    pub(crate) record: Record,
}

impl Staged {
    pub(crate) fn binary(&self) -> PathBuf {
        self.dir.join(BINARY)
    }

    /// Drops the staged files.
    pub(crate) fn discard(self) {
        let _ = make_writable_and_remove(&self.dir);
    }
}

/// Deletes leftover staging folders; call while holding the lock (IR req. 24).
pub(crate) fn clear_staging(layout: &Layout) -> Result<(), String> {
    let staging = layout.staging();
    if staging.exists() {
        make_writable_and_remove(&staging)?;
    }
    ensure_dir(&staging, 0o700)
}

/// A fresh private folder under staging.
pub(crate) fn fresh_staging(layout: &Layout, tag: &str) -> Result<PathBuf, String> {
    ensure_dir(&layout.staging(), 0o700)?;
    let dir = layout
        .staging()
        .join(format!("{tag}-{}-{}", std::process::id(), super::now_ms()));
    DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// Verifies the assets in `from` (signature, both hashes, executable format, the candidate's own
/// version report) and stages the two files (IR req. 23-24). Nothing outside staging changes.
pub(crate) async fn stage(layout: &Layout, from: &Path, source: &str) -> Result<Staged, String> {
    let sums_bytes = read_small(&from.join(SUMS), SUMS_MAX)?;
    let signature = read_small(&from.join(SIGNATURE), SIGNATURE_MAX)?;
    let key = release::release_key()?;
    release::verify_signature(&sums_bytes, &String::from_utf8_lossy(&signature), &key)?;
    let sums = Sums::parse(
        std::str::from_utf8(&sums_bytes).map_err(|_| format!("{SUMS} is not UTF-8 text"))?,
    )?;
    let version = sums.version()?;
    let (bin_name, ext_name) = (
        release::binary_asset(&version),
        release::extension_asset(&version),
    );
    let bin_hash = sums.one(&bin_name)?.to_string();
    let ext_hash = sums.one(&ext_name)?.to_string();
    let dir = fresh_staging(layout, "stage")?;
    let staged = async {
        let got = copy_hashed(&from.join(&bin_name), &dir.join(BINARY), BINARY_MAX)?;
        if got != bin_hash {
            return Err(format!("{bin_name} does not match its line in {SUMS}"));
        }
        let got = copy_hashed(&from.join(&ext_name), &dir.join(EXTENSION), EXTENSION_MAX)?;
        if got != ext_hash {
            return Err(format!("{ext_name} does not match its line in {SUMS}"));
        }
        let head = read_head(&dir.join(BINARY))?;
        release::check_format(&head)?;
        fs::set_permissions(dir.join(BINARY), Permissions::from_mode(0o500))
            .map_err(|e| e.to_string())?;
        let report = release::candidate_report(&dir.join(BINARY)).await?;
        if report.version != version || report.target != TARGET {
            return Err(format!(
                "the candidate reports {} for {}, expected {version} for {TARGET}",
                report.version, report.target
            ));
        }
        Ok(Record {
            version: version.clone(),
            commit: report.commit,
            target: report.target,
            binary_sha256: bin_hash.clone(),
            extension_sha256: ext_hash.clone(),
            source: source.to_string(),
            installed_ms: super::now_ms(),
            role: "kept".into(),
        })
    }
    .await;
    match staged {
        Ok(record) => Ok(Staged { dir, record }),
        Err(e) => {
            let _ = make_writable_and_remove(&dir);
            Err(e)
        }
    }
}

fn read_head(path: &Path) -> Result<Vec<u8>, String> {
    let mut head = Vec::with_capacity(64);
    fs::File::open(path)
        .and_then(|f| f.take(64).read_to_end(&mut head))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(head)
}

/// Re-hashes a version's two files against its record (IR req. 15, 30).
pub(crate) fn verify_files(layout: &Layout, record: &Record) -> Result<(), String> {
    let dir = layout.version_dir(&record.version);
    for (name, want, cap) in [
        (BINARY, &record.binary_sha256, BINARY_MAX),
        (EXTENSION, &record.extension_sha256, EXTENSION_MAX),
    ] {
        let path = dir.join(name);
        let got = release::sha256_file(&path, cap)?;
        if &got != want {
            return Err(format!("{} does not match its record", path.display()));
        }
    }
    Ok(())
}

/// The command and extension links are absent or already point into this program folder
/// (IR req. 21). Checked before anything changes.
pub(crate) fn check_links(layout: &Layout) -> Result<(), String> {
    for (link, target) in [
        (&layout.command, layout.command_target()),
        (&layout.extension, layout.extension_target()),
    ] {
        match fs::symlink_metadata(link) {
            Err(_) => {}
            Ok(meta)
                if meta.file_type().is_symlink()
                    && fs::read_link(link).ok() == Some(target.clone()) => {}
            Ok(_) => {
                return Err(format!(
                    "{} exists and is not Cataly's link; move it away first",
                    link.display()
                ));
            }
        }
    }
    Ok(())
}

/// Creates the links that are missing (first install). The extension folder is created 0700 when
/// absent, and nothing else of OMP's (IR req. 16-17).
pub(crate) fn create_links(layout: &Layout) -> Result<(), String> {
    check_links(layout)?;
    for (link, target, mode) in [
        (&layout.command, layout.command_target(), 0o755),
        (&layout.extension, layout.extension_target(), 0o700),
    ] {
        if fs::symlink_metadata(link).is_ok() {
            continue;
        }
        let dir = link.parent().ok_or("a link has no folder")?;
        ensure_dir(dir, mode)?;
        std::os::unix::fs::symlink(&target, link)
            .map_err(|e| format!("{}: {e}", link.display()))?;
    }
    Ok(())
}

/// Moves a staged candidate into its version folder and makes it read-only (folder 0555, binary
/// 0555, extension 0444). An existing folder of the same version is reused when its record matches.
pub(crate) fn place(layout: &Layout, staged: Staged) -> Result<Record, String> {
    let dest = layout.version_dir(&staged.record.version);
    let record = staged.record.clone();
    if dest.exists() {
        match layout.record(&record.version) {
            Some(r)
                if r.binary_sha256 == record.binary_sha256
                    && r.extension_sha256 == record.extension_sha256 =>
            {
                verify_files(layout, &r)?;
                staged.discard();
                return Ok(r);
            }
            _ if layout.current_version().as_deref() == Some(record.version.as_str()) => {
                staged.discard();
                return Err(format!(
                    "version {} is installed with other files; nothing changed",
                    record.version
                ));
            }
            _ => make_writable_and_remove(&dest)?,
        }
    }
    ensure_dir(&layout.versions(), 0o700)?;
    fs::set_permissions(staged.dir.join(BINARY), Permissions::from_mode(0o555))
        .and_then(|()| {
            fs::set_permissions(staged.dir.join(EXTENSION), Permissions::from_mode(0o444))
        })
        .map_err(|e| e.to_string())?;
    fs::rename(&staged.dir, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    fs::set_permissions(&dest, Permissions::from_mode(0o555)).map_err(|e| e.to_string())?;
    sync_dir(&layout.versions()).map_err(|e| e.to_string())?;
    layout.write_record(&record)?;
    Ok(record)
}

/// The activation (IR req. 27): a new link with a temporary name renamed over `current`, then the
/// program folder flushed.
pub(crate) fn switch(layout: &Layout, version: &str) -> Result<(), String> {
    let tmp = layout
        .root
        .join(format!("current.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    std::os::unix::fs::symlink(Path::new("versions").join(version), &tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, layout.current()).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", layout.current().display())
    })?;
    sync_dir(&layout.root).map_err(|e| e.to_string())
}

/// Sets the roles from the `current` link, the single source of truth (IR req. 28): the link's
/// version is current; `was` (the current before a switch, if any) becomes previous; a record
/// marked current that the link no longer names becomes previous too (an interrupted switch).
/// Returns what changed.
pub(crate) fn settle_records(layout: &Layout, was: Option<&str>) -> Result<Vec<String>, String> {
    let current = layout.current_version();
    let records = layout.read_records();
    let previous = was
        .filter(|w| Some(*w) != current.as_deref())
        .map(str::to_string)
        .or_else(|| {
            records
                .iter()
                .find(|r| r.role == "current" && Some(&r.version) != current.as_ref())
                .map(|r| r.version.clone())
        })
        .or_else(|| {
            records
                .iter()
                .find(|r| r.role == "previous" && Some(&r.version) != current.as_ref())
                .map(|r| r.version.clone())
        });
    let mut changed = Vec::new();
    for mut r in records {
        let role = if Some(&r.version) == current.as_ref() {
            "current"
        } else if Some(&r.version) == previous.as_ref() {
            "previous"
        } else {
            "kept"
        };
        if r.role != role {
            changed.push(format!("{} {} -> {role}", r.version, r.role));
            r.role = role.into();
            layout.write_record(&r)?;
        }
    }
    Ok(changed)
}

/// Deletes version folders and records that are neither current nor previous (IR req. 29).
pub(crate) fn retain(layout: &Layout) -> Result<(), String> {
    for r in layout.read_records() {
        if r.role == "kept" {
            let dir = layout.version_dir(&r.version);
            if dir.exists() {
                make_writable_and_remove(&dir)?;
            }
            let _ = fs::remove_file(layout.record_file(&r.version));
        }
    }
    if let Ok(dir) = fs::read_dir(layout.versions()) {
        let known: Vec<String> = layout
            .read_records()
            .into_iter()
            .map(|r| r.version)
            .collect();
        for entry in dir.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !known.contains(&name) {
                make_writable_and_remove(&entry.path())?;
            }
        }
    }
    Ok(())
}

/// The record roles a status line needs.
pub(crate) fn role(layout: &Layout, role: &str) -> Option<Record> {
    layout.read_records().into_iter().find(|r| r.role == role)
}

/// One finding of `status --install` with its repair command (IR req. 31).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Problem {
    pub(crate) path: String,
    pub(crate) what: String,
    pub(crate) repair: String,
}

/// Checks the links and the current version's files; only the records are repaired.
pub(crate) fn inspect(layout: &Layout) -> (Vec<String>, Vec<Problem>) {
    let mut problems = Vec::new();
    let mut repaired = Vec::new();
    let Some(current) = layout.current_version() else {
        problems.push(Problem {
            path: layout.current().display().to_string(),
            what: "no version is active".into(),
            repair: "install with install.sh or `cataly install --from <folder>`".into(),
        });
        return (repaired, problems);
    };
    match settle_records(layout, None) {
        Ok(changed) => repaired.extend(changed),
        Err(e) => problems.push(Problem {
            path: layout.root.join("records").display().to_string(),
            what: e,
            repair: "check the folder's permissions".into(),
        }),
    }
    match layout.record(&current) {
        None => problems.push(Problem {
            path: layout.record_file(&current).display().to_string(),
            what: format!("version {current} has no record"),
            repair: "reinstall through install.sh".into(),
        }),
        Some(r) => {
            if let Err(e) = verify_files(layout, &r) {
                let repair = if role(layout, "previous").is_some() {
                    "`cataly rollback`, then `cataly update`"
                } else {
                    "reinstall through install.sh"
                };
                problems.push(Problem {
                    path: layout.version_dir(&current).display().to_string(),
                    what: e,
                    repair: repair.into(),
                });
            }
        }
    }
    for (link, target) in [
        (&layout.command, layout.command_target()),
        (&layout.extension, layout.extension_target()),
    ] {
        if fs::read_link(link).ok() != Some(target.clone()) {
            problems.push(Problem {
                path: link.display().to_string(),
                what: "does not link the active version".into(),
                repair: format!("ln -sfn {} {}", target.display(), link.display()),
            });
        }
    }
    (repaired, problems)
}

/// Removes the command link and the extension link when they are Cataly's, then the program folder
/// (IR req. 35). Configuration, state, sessions and OMP are never touched.
pub(crate) fn remove_all(layout: &Layout) -> Result<(), String> {
    for (link, target) in [
        (&layout.command, layout.command_target()),
        (&layout.extension, layout.extension_target()),
    ] {
        if fs::read_link(link).ok() == Some(target) {
            fs::remove_file(link).map_err(|e| format!("{}: {e}", link.display()))?;
        }
    }
    make_writable_and_remove(&layout.root)
}

/// Removes a tree whose folders may be read-only.
fn make_writable_and_remove(path: &Path) -> Result<(), String> {
    fn unlock(path: &Path) {
        if let Ok(meta) = fs::symlink_metadata(path)
            && meta.is_dir()
        {
            let _ = fs::set_permissions(path, Permissions::from_mode(0o700));
            if let Ok(dir) = fs::read_dir(path) {
                for e in dir.filter_map(Result::ok) {
                    unlock(&e.path());
                }
            }
        }
    }
    unlock(path);
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::scratch;

    fn layout(dir: &Path) -> Layout {
        Layout {
            root: dir.join("data/cataly"),
            command: dir.join("home/.local/bin/cataly"),
            extension: dir.join("home/.omp/agent/extensions/cataly.ts"),
        }
    }

    fn fake_version(l: &Layout, v: &str) {
        let dir = l.version_dir(v);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(BINARY), format!("bin {v}")).unwrap();
        fs::write(dir.join(EXTENSION), format!("ext {v}")).unwrap();
        l.write_record(&Record {
            version: v.into(),
            commit: "c".into(),
            target: TARGET.into(),
            binary_sha256: release::sha256(format!("bin {v}").as_bytes()),
            extension_sha256: release::sha256(format!("ext {v}").as_bytes()),
            source: "test".into(),
            installed_ms: 0,
            role: "kept".into(),
        })
        .unwrap();
    }

    #[test]
    fn switch_settle_retain_and_repair() {
        let dir = scratch("install-layout");
        let l = layout(&dir);
        prepare_root(&l).unwrap();
        for v in ["1.0.0", "1.1.0", "1.2.0"] {
            fake_version(&l, v);
        }
        switch(&l, "1.0.0").unwrap();
        settle_records(&l, None).unwrap();
        create_links(&l).unwrap();
        assert_eq!(fs::read(&l.command).unwrap(), b"bin 1.0.0");
        switch(&l, "1.1.0").unwrap();
        settle_records(&l, Some("1.0.0")).unwrap();
        retain(&l).unwrap();
        assert_eq!(role(&l, "previous").unwrap().version, "1.0.0");
        assert!(
            !l.version_dir("1.2.0").exists(),
            "neither current nor previous"
        );
        assert_eq!(fs::read(&l.extension).unwrap(), b"ext 1.1.0");
        let (fixed, problems) = inspect(&l);
        assert!(fixed.is_empty() && problems.is_empty(), "{problems:?}");

        // Killed between rename and record: the link moved, the records did not.
        switch(&l, "1.0.0").unwrap();
        let (fixed, problems) = inspect(&l);
        assert_eq!(fixed.len(), 2, "{fixed:?}");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(role(&l, "current").unwrap().version, "1.0.0");
        assert_eq!(role(&l, "previous").unwrap().version, "1.1.0");

        // Tampered file and a foreign link are reported with a repair command.
        fs::write(l.version_dir("1.0.0").join(BINARY), "evil").unwrap();
        fs::remove_file(&l.command).unwrap();
        let (_, problems) = inspect(&l);
        assert_eq!(problems.len(), 2, "{problems:?}");
        fs::write(&l.command, "mine").unwrap();
        assert!(check_links(&l).unwrap_err().contains("move it away"));
        remove_all(&l).unwrap();
        assert!(!l.root.exists());
        assert!(l.command.exists(), "a file Cataly did not create stays");
    }

    #[test]
    fn root_must_be_private() {
        let dir = scratch("install-root");
        let l = layout(&dir);
        prepare_root(&l).unwrap();
        fs::set_permissions(&l.root, Permissions::from_mode(0o777)).unwrap();
        assert!(prepare_root(&l).unwrap_err().contains("chmod 700"));
    }
}
