//! The bridge's place in OMP (DESIGN 10.6, 11.2, 12.2 C19): the extension source is compiled into
//! this binary, written once to the program folder and linked from OMP's global extension folder.
//! OMP loads it from there in every process; the hub never passes `-e`. Health fails, and session
//! starts are refused, when the link is missing or does not hold this binary's bridge.

use std::path::{Path, PathBuf};

use crate::sys::fs::write_atomic;

/// The bridge this binary speaks to (control protocol, bridge protocol 1).
pub(crate) const SOURCE: &str = include_str!("../../extension/cataly.ts");
const FILE: &str = "cataly.ts";

/// OMP's agent directory: `PI_CODING_AGENT_DIR` when set, else `~/.omp/agent` (OMP's default profile).
fn agent_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".omp").join("agent"))
}

/// The program folder: `$XDG_DATA_HOME/cataly` or `~/.local/share/cataly` (DESIGN 11.2).
fn program_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join("cataly"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/cataly"))
}

/// Where the link lives and where it points.
pub(crate) fn locations() -> Result<(PathBuf, PathBuf), String> {
    let agent = agent_dir().ok_or("neither PI_CODING_AGENT_DIR nor HOME is set")?;
    let program = program_dir().ok_or("neither XDG_DATA_HOME nor HOME is set")?;
    Ok((agent.join("extensions").join(FILE), program.join(FILE)))
}

/// `Ok` when the link exists and its target holds exactly this binary's bridge.
pub(crate) fn check() -> Result<(), String> {
    let (link, _) = locations()?;
    check_at(&link)
}

fn check_at(link: &Path) -> Result<(), String> {
    let hint = "run `cataly bridge install`";
    let meta = std::fs::symlink_metadata(link)
        .map_err(|_| format!("the bridge link {} is missing; {hint}", link.display()))?;
    if !meta.file_type().is_symlink() {
        return Err(format!("{} is not the bridge link; {hint}", link.display()));
    }
    let text = std::fs::read(link)
        .map_err(|e| format!("the bridge link {} is broken ({e}); {hint}", link.display()))?;
    if text != SOURCE.as_bytes() {
        return Err(format!(
            "the bridge at {} is not this version's; {hint}",
            link.display()
        ));
    }
    Ok(())
}

/// Writes the bridge to the program folder and points OMP's extension link at it. Touches nothing
/// else in OMP (IR requirement 20). Returns the link path.
pub(crate) fn install() -> Result<PathBuf, String> {
    let (link, target) = locations()?;
    install_at(&link, &target)?;
    Ok(link)
}

fn install_at(link: &Path, target: &Path) -> Result<(), String> {
    let parent = target.parent().ok_or("the program folder has no parent")?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    write_atomic(target, SOURCE.as_bytes()).map_err(|e| e.to_string())?;
    let dir = link.parent().ok_or("the extension folder has no parent")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(link).map_err(|e| format!("{}: {e}", link.display()))?;
        }
        Ok(_) => {
            return Err(format!(
                "{} exists and is not a link; move it away first",
                link.display()
            ));
        }
        Err(_) => {}
    }
    std::os::unix::fs::symlink(target, link).map_err(|e| format!("{}: {e}", link.display()))?;
    check_at(link)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::scratch;

    #[test]
    fn install_links_the_bridge_and_check_sees_staleness() {
        let dir = scratch("bridge-link");
        let link = dir.join("agent/extensions/cataly.ts");
        let target = dir.join("data/cataly/cataly.ts");
        assert!(check_at(&link).unwrap_err().contains("missing"));
        install_at(&link, &target).unwrap();
        check_at(&link).unwrap();
        std::fs::write(&target, "old").unwrap();
        assert!(check_at(&link).unwrap_err().contains("not this version"));
        install_at(&link, &target).unwrap();
        check_at(&link).unwrap();
        assert!(SOURCE.lines().count() < 600);
    }
}
