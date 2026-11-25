//! The bridge's place in OMP (DESIGN 10.6, 11.2, 12.2 C19): the extension source is compiled into
//! this binary; a release ships it as its own asset (`cataly extension` writes it), the installer
//! puts it next to the binary in the version folder, and OMP's global extension folder links it
//! through the program folder's `current` link. OMP loads it from there in every process; the hub
//! never passes `-e`. Health fails, and session starts are refused, when the link is missing or does
//! not hold this binary's bridge.

use std::path::{Path, PathBuf};

/// The bridge source as written in the repository.
const SOURCE: &str = include_str!("../../extension/cataly.ts");
/// The version line of the source, filled in by [`file`] (IR req. 6).
const VERSION_LINE: &str = "const VERSION = \"0.0.0-source\";";
/// The link's file name in OMP's extension folder.
pub(crate) const LINK_NAME: &str = "cataly.ts";

/// The extension file this binary installs: the source with the product version filled in.
pub(crate) fn file() -> String {
    SOURCE.replacen(
        VERSION_LINE,
        &format!("const VERSION = \"{}\";", crate::sys::release::VERSION),
        1,
    )
}

/// OMP's agent directory: `PI_CODING_AGENT_DIR` when set, else `~/.omp/agent` (OMP's default profile).
fn agent_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".omp").join("agent"))
}

/// Where the extension link lives.
pub(crate) fn link() -> Result<PathBuf, String> {
    let agent = agent_dir().ok_or("neither PI_CODING_AGENT_DIR nor HOME is set")?;
    Ok(agent.join("extensions").join(LINK_NAME))
}

/// `Ok` when the link exists and its target holds exactly this binary's bridge.
pub(crate) fn check() -> Result<(), String> {
    check_at(&link()?)
}

fn check_at(link: &Path) -> Result<(), String> {
    let hint = "see `cataly status --install`";
    let meta = std::fs::symlink_metadata(link)
        .map_err(|_| format!("the bridge link {} is missing; {hint}", link.display()))?;
    if !meta.file_type().is_symlink() {
        return Err(format!("{} is not the bridge link; {hint}", link.display()));
    }
    let text = std::fs::read(link)
        .map_err(|e| format!("the bridge link {} is broken ({e}); {hint}", link.display()))?;
    if text != file().as_bytes() {
        return Err(format!(
            "the bridge at {} is not this version's; {hint}",
            link.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::fs::tests::scratch;

    #[test]
    fn check_sees_missing_foreign_and_stale_links() {
        let dir = scratch("bridge-link");
        let link = dir.join("agent/extensions/cataly.ts");
        let target = dir.join("cataly.ts");
        assert!(check_at(&link).unwrap_err().contains("missing"));
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::fs::write(&target, file()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        check_at(&link).unwrap();
        std::fs::write(&target, SOURCE).unwrap();
        assert!(check_at(&link).unwrap_err().contains("not this version"));
        assert!(SOURCE.lines().count() < 600);
        assert!(file().contains(&format!(
            "const VERSION = \"{}\";",
            env!("CARGO_PKG_VERSION")
        )));
    }
}
