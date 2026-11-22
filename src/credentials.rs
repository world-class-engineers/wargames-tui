//! Secret model and durable, permission-restricted application storage.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Multi-game credentials schema version.
pub const SCHEMA_VERSION: u32 = 2;

/// A secret used to authenticate against the next level of a wargame.
/// The externally tagged JSON representation is preserved from schema v1.
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

/// Progress for one wargame. Keys in `secrets` are the level the secret unlocks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameProgress {
    pub last_level: u16,
    pub secrets: BTreeMap<u16, Secret>,
}

impl GameProgress {
    pub fn insert(&mut self, level: u16, secret: Secret) {
        self.secrets.insert(level, secret);
    }

    pub fn get(&self, level: u16) -> Option<&Secret> {
        self.secrets.get(&level)
    }
}

/// Persisted state for every wargame. Unknown game ids are preserved as-is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default = "schema_version")]
    pub version: u32,
    #[serde(default)]
    pub last_wargame: String,
    #[serde(default)]
    pub games: BTreeMap<String, GameProgress>,
}

fn schema_version() -> u32 {
    SCHEMA_VERSION
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            last_wargame: String::new(),
            games: BTreeMap::new(),
        }
    }
}

impl Credentials {
    /// Progress for a game, or an empty record if there is none yet.
    pub fn progress(&self, game_id: &str) -> GameProgress {
        self.games.get(game_id).cloned().unwrap_or_default()
    }

    /// Mutable progress for a game, creating it on demand.
    pub fn progress_mut(&mut self, game_id: &str) -> &mut GameProgress {
        self.games.entry(game_id.to_owned()).or_default()
    }

    pub fn set_last_wargame(&mut self, game_id: &str) {
        self.last_wargame = game_id.to_owned();
    }

    pub fn set_last_level(&mut self, game_id: &str, level: u16) {
        self.progress_mut(game_id).last_level = level;
    }

    pub fn store_secret(&mut self, game_id: &str, level: u16, secret: Secret) {
        self.progress_mut(game_id).insert(level, secret);
    }
}

/// All filesystem locations used by the app, injectable for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    /// Resolve XDG-style directories with a home-directory fallback. The old
    /// `bandit-tui` directories are deliberately never read or migrated.
    pub fn from_env() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let config_dir = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("wargames-tui");
        let cache_dir = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
            .join("wargames-tui");
        Self {
            config_dir,
            cache_dir,
        }
    }

    pub fn credentials_file(&self) -> PathBuf {
        self.config_dir.join("credentials.json")
    }

    pub fn catalog_file(&self) -> PathBuf {
        self.config_dir.join("wargames.toml")
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
    fn secret_serde_round_trip_preserves_external_tagging() {
        let secrets = vec![
            Secret::Password("a p@ss word".to_owned()),
            Secret::PrivateKeyPath(PathBuf::from("/home/user/.ssh/sshkey.private")),
        ];
        for secret in secrets {
            let json = serde_json::to_string(&secret).unwrap();
            let decoded: Secret = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, secret);
        }
        assert_eq!(
            serde_json::to_value(Secret::Password("x".to_owned())).unwrap(),
            serde_json::json!({ "Password": "x" })
        );
    }

    #[test]
    fn multi_game_schema_round_trip_and_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/credentials.json");
        let mut creds = Credentials::default();
        assert_eq!(creds.version, SCHEMA_VERSION);
        assert_eq!(load_credentials(&path).unwrap(), Credentials::default());

        creds.set_last_wargame("bandit");
        creds.set_last_level("bandit", 14);
        creds.store_secret("bandit", 14, Secret::Password("s3cret".to_owned()));
        creds.store_secret(
            "bandit",
            15,
            Secret::PrivateKeyPath(PathBuf::from("~/sshkey.private")),
        );
        creds.set_last_level("century", 3);
        creds.store_secret("century", 3, Secret::Password("century3".to_owned()));
        // Unknown ids are preserved, not interpreted or dropped.
        creds.games.insert(
            "future-game".to_owned(),
            GameProgress {
                last_level: 7,
                secrets: BTreeMap::new(),
            },
        );
        save_credentials(&path, &creds).unwrap();

        let loaded = load_credentials(&path).unwrap();
        assert_eq!(loaded.version, SCHEMA_VERSION);
        assert_eq!(loaded.last_wargame, "bandit");
        assert_eq!(loaded.progress("bandit").last_level, 14);
        assert_eq!(
            loaded.progress("bandit").get(14),
            Some(&Secret::Password("s3cret".to_owned()))
        );
        assert_eq!(loaded.progress("century").last_level, 3);
        assert_eq!(
            loaded.progress("century").get(3),
            Some(&Secret::Password("century3".to_owned()))
        );
        assert!(loaded.progress("bandit").get(3).is_none());
        assert_eq!(loaded.games.get("future-game").unwrap().last_level, 7);

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
    fn paths_use_xdg_names_and_wargames_suffix() {
        let paths = Paths {
            config_dir: PathBuf::from("/tmp/cfg/wargames-tui"),
            cache_dir: PathBuf::from("/tmp/cache/wargames-tui"),
        };
        assert_eq!(
            paths.credentials_file(),
            PathBuf::from("/tmp/cfg/wargames-tui/credentials.json")
        );
        assert_eq!(
            paths.catalog_file(),
            PathBuf::from("/tmp/cfg/wargames-tui/wargames.toml")
        );
        assert_eq!(
            paths.known_hosts_file(),
            PathBuf::from("/tmp/cfg/wargames-tui/known_hosts")
        );
        assert_eq!(
            paths.levels_dir(),
            PathBuf::from("/tmp/cache/wargames-tui/levels")
        );
        let text = format!("{:?}", Paths::from_env());
        assert!(text.contains("wargames-tui"), "{text}");
        assert!(!text.contains("bandit-tui"), "{text}");
    }
}
