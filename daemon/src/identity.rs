use anyhow::{Context, Result};
use iroh::SecretKey;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn key_path() -> Result<PathBuf> {
    if let Ok(custom) = std::env::var("QUIX_KEY_PATH") {
        return Ok(PathBuf::from(custom));
    }
    let config_dir = dirs::config_dir().context("resolve config dir")?;
    Ok(config_dir.join("quix").join("key"))
}

/// Loads the persisted secret key, generating and saving a new one on first run.
pub fn load_or_create() -> Result<SecretKey> {
    let path = key_path()?;

    if let Ok(bytes) = std::fs::read(&path) {
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("key file has wrong length"))?;

        // Keys written before the file was locked down are still readable by
        // every local user on Windows. Tightening them here means an existing
        // install stops leaking on its next start, without waiting for the
        // operator to rotate. Best-effort: a key we can read but not re-permit
        // is still a key we can run with.
        #[cfg(windows)]
        if let Err(e) = crate::winacl::restrict_to_administrators(&path) {
            crate::warn!("warning: could not restrict access to the key: {e:#}");
        }

        return Ok(SecretKey::from_bytes(&array));
    }

    let key = SecretKey::generate();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_protected(&path, &key.to_bytes())
        .with_context(|| format!("writing the identity key to {}", path.display()))?;

    Ok(key)
}

/// Writes the key so it is never on disk readable by anyone else.
///
/// The permissions go on before the bytes do. Creating the file and tightening
/// it afterwards leaves a window in which the node's whole identity is world
/// readable — short, but a reader only needs one pass, and the file never
/// changes again.
///
/// `create_new` rather than a plain write: reaching here means the key could
/// not be read, which is *usually* because there is none. If it was instead a
/// permission or I/O error, overwriting would silently mint a new identity and
/// drop this node out of every network it belongs to. Failing is kinder.
fn write_protected(path: &Path, key: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path)?;

    #[cfg(windows)]
    crate::winacl::restrict_to_administrators(path)?;

    file.write_all(key)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key must never exist on disk in a state anyone else could read, so
    /// the test checks the permissions of the file that was actually written.
    #[test]
    fn a_freshly_written_key_is_not_readable_by_others() {
        let path = std::env::temp_dir().join(format!("quix-key-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);

        write_protected(&path, &[7u8; 32]).expect("should write");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "group or other can reach it: {mode:o}");
        }
        #[cfg(windows)]
        {
            let shown = std::process::Command::new("icacls").arg(&path).output().unwrap();
            let text = String::from_utf8_lossy(&shown.stdout);
            assert!(
                !text.contains("BUILTIN\\Users") && !text.contains("\\Everyone"),
                "readable by ordinary users:\n{text}"
            );
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_existing_key_is_never_overwritten() {
        // Losing the key loses the node's addresses and its place in every
        // network, so a write that could clobber one must fail instead.
        let path = std::env::temp_dir().join(format!("quix-key-keep-{}", std::process::id()));
        std::fs::write(&path, b"existing").unwrap();

        assert!(write_protected(&path, &[7u8; 32]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing", "left alone");

        let _ = std::fs::remove_file(&path);
    }
}
