//! Atomic replacement for user work and private local data.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Stage beside the destination so replacement is atomic on the same filesystem.
/// NamedTempFile creates owner-only files on Unix, before any sensitive bytes
/// are written. A failed write leaves the previous destination intact.
pub fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent).context("creating destination directory")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).context("creating temporary file")?;
    staged.write_all(bytes).context("writing temporary file")?;
    staged
        .as_file()
        .sync_all()
        .context("syncing temporary file")?;
    staged
        .persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_existing_file_and_creates_private_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        write(&path, b"old").unwrap();
        write(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
