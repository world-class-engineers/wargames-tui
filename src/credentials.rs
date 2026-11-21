//! Secret model and durable, permission-restricted application storage.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Highest real Bandit level (the game has 34 levels, 0 through 33; page
/// `bandit34.html` is only a placeholder). Used to sanity-check persisted state.
pub const MAX_LEVEL: u16 = 33;

/// A secret used to authenticate against the next Bandit level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Secret {
    Password(String),
    PrivateKeyPath(PathBuf),
}

impl Secret {
    /// A short, non-secret description suitable for status messages.
    pub fn summary(&self) -> String {
        match self {
            Secret::Password(_) => "password".to_owned(),
            Secret::PrivateKeyPath(path) => format!("private key {}", path.display()),
        }
    }
}

/// Persisted progress. Keys in `secrets` are the level the secret unlocks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Credentials {
    pub last_level: u16,
    pub secrets: BTreeMap<u16, Secret>,
}

impl Credentials {
    pub fn insert(&mut self, level: u16, secret: Secret) {
        self.secrets.insert(level, secret);
    }

    pub fn get(&self, level: u16) -> Option<&Secret> {
        self.secrets.get(&level)
    }
}

/// All filesystem locations used by the app, injectable for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    /// Resolve XDG-style directories with a home-directory fallback.
    pub fn from_env() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("bandit-tui");
        let cache_dir = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("bandit-tui");
        Self {
            config_dir,
            cache_dir,
        }
    }

    pub fn credentials_file(&self) -> PathBuf {
        self.config_dir.join("credentials.json")
    }

    /// Dedicated known-hosts file so SSH host keys are remembered between
    /// sessions instead of being thrown away in `/dev/null`.
    pub fn known_hosts_file(&self) -> PathBuf {
        self.config_dir.join("known_hosts")
    }

    pub fn levels_dir(&self) -> PathBuf {
        self.cache_dir.join("levels")
    }
}

/// Load credentials. A missing file is normal; a corrupt file is reported to
/// the caller so the UI can surface a non-fatal warning.
pub fn load_credentials(path: &Path) -> Result<Credentials> {
    if !path.exists() {
        return Ok(Credentials::default());
    }
    let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Atomically-ish write credentials with owner-only permissions on Unix.
pub fn save_credentials(path: &Path, credentials: &Credentials) -> Result<()> {
    let json = serde_json::to_string_pretty(credentials).context("serializing credentials")?;
    write_restricted_file(path, json.as_bytes())
}

/// Write a file that only the current user can read/write (Unix), replacing an
/// existing file via rename so readers never see a partially written document.
pub fn write_restricted_file(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }

    let mut temp = path.to_path_buf();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_owned());
    temp.set_file_name(format!(".{file_name}.{}.tmp", std::process::id()));

    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .with_context(|| format!("creating {}", temp.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing {}", temp.display()))?;
    file.flush()
        .with_context(|| format!("flushing {}", temp.display()))?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", temp.display()))?;
    }
    fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Create a directory (and parents) that only the current user can traverse.
pub fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_serde_round_trip() {
        let secrets = vec![
            Secret::Password("a p@ss word".to_owned()),
            Secret::PrivateKeyPath(PathBuf::from("/home/user/.ssh/sshkey.private")),
        ];
        for secret in secrets {
            let json = serde_json::to_string(&secret).unwrap();
            let decoded: Secret = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, secret);
        }
    }

    #[test]
    fn credentials_round_trip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/credentials.json");
        assert_eq!(load_credentials(&path).unwrap(), Credentials::default());

        let mut creds = Credentials {
            last_level: 14,
            ..Credentials::default()
        };
        creds.insert(14, Secret::Password("s3cret".to_owned()));
        creds.insert(
            15,
            Secret::PrivateKeyPath(PathBuf::from("~/sshkey.private")),
        );
        save_credentials(&path, &creds).unwrap();

        let loaded = load_credentials(&path).unwrap();
        assert_eq!(loaded.last_level, 14);
        assert_eq!(loaded.get(14), creds.get(14));
        assert_eq!(loaded.get(15), creds.get(15));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
            let dir_mode = fs::metadata(path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
    }

    #[test]
    fn paths_use_xdg_overrides() {
        let paths = Paths {
            config_dir: PathBuf::from("/tmp/cfg/bandit-tui"),
            cache_dir: PathBuf::from("/tmp/cache/bandit-tui"),
        };
        assert_eq!(
            paths.credentials_file(),
            PathBuf::from("/tmp/cfg/bandit-tui/credentials.json")
        );
        assert_eq!(
            paths.known_hosts_file(),
            PathBuf::from("/tmp/cfg/bandit-tui/known_hosts")
        );
        assert_eq!(
            paths.levels_dir(),
            PathBuf::from("/tmp/cache/bandit-tui/levels")
        );
    }
}
