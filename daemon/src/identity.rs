use anyhow::{Context, Result};
use iroh::SecretKey;
use std::path::PathBuf;

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
        return Ok(SecretKey::from_bytes(&array));
    }

    let key = SecretKey::generate();

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, key.to_bytes())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(key)
}