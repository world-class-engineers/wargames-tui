//! Shared SSH command construction for the embedded terminal and the verifier.
//! All host, port, user, and remote-command details come from the wargame.

use crate::credentials::{Secret, create_private_dir, write_restricted_file};
use crate::wargame::{SecretKind, Wargame};
use anyhow::{Context, Result, bail};
use portable_pty::CommandBuilder;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub const CONNECT_TIMEOUT_SECS: u8 = 10;

/// Probing `sshpass -V` is process work; the result cannot change during one
/// run, so cache it for both PTY spawns and verifier threads.
static SSHPASS_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Environment variable read by the askpass helper. The password is passed in
/// the environment only; it is never written into the helper script or logged.
pub const ASKPASS_ENV_VAR: &str = "WARGAMES_TUI_ASKPASS_SECRET";
const ASKPASS_SCRIPT_NAME: &str = "askpass.sh";
const ASKPASS_SCRIPT_BODY: &str = r#"#!/bin/sh
printf '%s\n' "$WARGAMES_TUI_ASKPASS_SECRET"
"#;

pub fn username(game: &Wargame, level: u16) -> String {
    game.username(level)
}

pub fn endpoint(game: &Wargame, level: u16) -> String {
    format!("{}@{}", username(game, level), game.ssh_host)
}

/// Expand a leading `~/` in a user-supplied key path.
pub fn expand_key_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

pub fn sshpass_available() -> bool {
    *SSHPASS_AVAILABLE.get_or_init(|| {
        Command::new("sshpass")
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

/// Create (or repair) an OpenSSH askpass helper in the app config directory.
/// Scripts written by older `bandit-tui` builds are detected and replaced.
pub fn ensure_askpass_script(config_dir: &Path) -> Result<PathBuf> {
    #[cfg(not(unix))]
    {
        let _ = config_dir;
        anyhow::bail!(
            "the OpenSSH askpass fallback is only supported on Unix; install sshpass for password authentication"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        create_private_dir(config_dir)?;
        let path = config_dir.join(ASKPASS_SCRIPT_NAME);
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let contents_match = existing == ASKPASS_SCRIPT_BODY;
        let mode = fs::metadata(&path)
            .ok()
            .map(|metadata| metadata.permissions().mode() & 0o777);

        if !contents_match || mode != Some(0o700) {
            write_restricted_file(&path, ASKPASS_SCRIPT_BODY.as_bytes())
                .with_context(|| format!("creating askpass helper {}", path.display()))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("setting permissions on {}", path.display()))?;
        }
        Ok(path)
    }
}

/// Create a dedicated known-hosts file before SSH touches it. `accept-new`
/// then remembers first-seen keys instead of discarding them in `/dev/null`.
pub fn ensure_known_hosts(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    Ok(())
}

fn common_args(game: &Wargame, level: u16, known_hosts: &Path) -> Vec<String> {
    vec![
        "-p".to_owned(),
        game.ssh_port.to_string(),
        "-o".to_owned(),
        "StrictHostKeyChecking=accept-new".to_owned(),
        "-o".to_owned(),
        format!("UserKnownHostsFile={}", known_hosts.display()),
        "-o".to_owned(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "-o".to_owned(),
        "LogLevel=ERROR".to_owned(),
        endpoint(game, level),
    ]
}

fn remote_command(game: &Wargame) -> Vec<String> {
    // Send the configured command as one argument. UnderTheWire's SSH shell
    // appends the exec request directly to `powershell.exe -NoLogo`, so its
    // catalogs use a deliberate leading space before `-Command ...`. Splitting
    // on whitespace would discard that space and turn the request into
    // `-NoLogo-Command ...`.
    vec![game.verification_remote_command.clone()]
}

fn append_password_auth_args(
    game: &Wargame,
    level: u16,
    known_hosts: &Path,
    command: &mut Command,
) {
    command.arg("-o").arg("PreferredAuthentications=password");
    command.arg("-o").arg("PubkeyAuthentication=no");
    command.arg("-o").arg("NumberOfPasswordPrompts=1");
    // BatchMode=yes would suppress SSH_ASKPASS as well as tty prompts.
    command.arg("-o").arg("BatchMode=no");
    command.args(common_args(game, level, known_hosts));
    command.args(remote_command(game));
}

/// Command used for the interactive PTY. When `inject_password_in_pty` is set,
/// plain `ssh` is launched and the PTY reader injects the saved password at the
/// remote prompt; otherwise `sshpass` supplies it via `SSHPASS` (when present).
pub fn interactive_command(
    game: &Wargame,
    level: u16,
    secret: Option<&Secret>,
    known_hosts: &Path,
    inject_password_in_pty: bool,
) -> CommandBuilder {
    match secret {
        Some(Secret::Password(password)) if !inject_password_in_pty => {
            let mut cmd = CommandBuilder::new("sshpass");
            cmd.arg("-e");
            cmd.arg("ssh");
            cmd.arg("-o");
            cmd.arg("PubkeyAuthentication=no");
            cmd.args(common_args(game, level, known_hosts));
            cmd.env("SSHPASS", password.clone());
            cmd
        }
        Some(Secret::Password(_)) => {
            let mut cmd = CommandBuilder::new("ssh");
            cmd.arg("-o");
            cmd.arg("PreferredAuthentications=password");
            cmd.arg("-o");
            cmd.arg("PubkeyAuthentication=no");
            cmd.arg("-o");
            cmd.arg("NumberOfPasswordPrompts=1");
            cmd.arg("-o");
            cmd.arg("BatchMode=no");
            cmd.args(common_args(game, level, known_hosts));
            cmd
        }
        Some(Secret::PrivateKeyPath(path)) if game.allows(SecretKind::PrivateKeyPath) => {
            let mut cmd = CommandBuilder::new("ssh");
            cmd.arg("-i");
            cmd.arg(expand_key_path(path));
            cmd.arg("-o");
            cmd.arg("IdentitiesOnly=yes");
            cmd.args(common_args(game, level, known_hosts));
            cmd
        }
        _ => {
            let mut cmd = CommandBuilder::new("ssh");
            cmd.args(common_args(game, level, known_hosts));
            cmd
        }
    }
}

/// Non-interactive command used to check whether a secret unlocks a level.
/// `sshpass` is optional: without it, OpenSSH's askpass mechanism is used.
pub fn verification_command(
    game: &Wargame,
    level: u16,
    secret: &Secret,
    known_hosts: &Path,
    config_dir: &Path,
) -> Result<Command> {
    verification_command_with(
        game,
        level,
        secret,
        known_hosts,
        config_dir,
        sshpass_available(),
    )
}

/// Command construction with an explicit `sshpass` availability flag so tests
/// can force the OpenSSH fallback without touching `PATH` or starting SSH.
fn verification_command_with(
    game: &Wargame,
    level: u16,
    secret: &Secret,
    known_hosts: &Path,
    config_dir: &Path,
    sshpass: bool,
) -> Result<Command> {
    if matches!(secret, Secret::PrivateKeyPath(_)) && !game.allows(SecretKind::PrivateKeyPath) {
        bail!("wargame '{}' does not accept private key secrets", game.id);
    }
    match secret {
        Secret::Password(password) if sshpass => {
            let mut cmd = Command::new("sshpass");
            cmd.arg("-e").arg("ssh");
            append_password_auth_args(game, level, known_hosts, &mut cmd);
            cmd.env("SSHPASS", password);
            Ok(cmd)
        }
        Secret::Password(password) => {
            let script = ensure_askpass_script(config_dir)?;
            let mut cmd = Command::new("ssh");
            append_password_auth_args(game, level, known_hosts, &mut cmd);
            cmd.env("DISPLAY", ":0");
            cmd.env("SSH_ASKPASS", &script);
            cmd.env("SSH_ASKPASS_REQUIRE", "force");
            cmd.env(ASKPASS_ENV_VAR, password);
            Ok(cmd)
        }
        Secret::PrivateKeyPath(path) => {
            let mut cmd = Command::new("ssh");
            cmd.arg("-i");
            cmd.arg(expand_key_path(path));
            cmd.arg("-o");
            cmd.arg("BatchMode=yes");
            cmd.arg("-o");
            cmd.arg("IdentitiesOnly=yes");
            cmd.arg("-o");
            cmd.arg("PreferredAuthentications=publickey");
            cmd.args(common_args(game, level, known_hosts));
            cmd.args(remote_command(game));
            Ok(cmd)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wargame::Catalog;

    const TEST_SECRET: &str = "test-only-placeholder";

    fn games() -> Catalog {
        Catalog::parse(crate::wargame::DEFAULT_CATALOG).unwrap()
    }

    fn env_value(command: &Command, key: &str) -> Option<String> {
        command.get_envs().find_map(|(name, value)| {
            if name == std::ffi::OsStr::new(key) {
                value.map(|value| value.to_string_lossy().into_owned())
            } else {
                None
            }
        })
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn endpoint_and_tilde_expansion_use_catalog_values() {
        let games = games();
        let bandit = games.get("bandit").unwrap();
        let century = games.get("century").unwrap();
        assert_eq!(username(bandit, 0), "bandit0");
        assert_eq!(endpoint(bandit, 14), "bandit14@bandit.labs.overthewire.org");
        assert_eq!(endpoint(century, 3), "century3@century.underthewire.tech");
        let expanded = expand_key_path(Path::new("~/sshkey.private"));
        assert!(!expanded.to_string_lossy().starts_with("~/"));
        assert_eq!(
            expand_key_path(Path::new("/tmp/key.private")),
            PathBuf::from("/tmp/key.private")
        );
    }

    #[test]
    fn common_args_use_configured_port_and_dedicated_known_hosts() {
        let games = games();
        let bandit = games.get("bandit").unwrap();
        let century = games.get("century").unwrap();
        let path = Path::new("/tmp/wargames-tui-test-known-hosts");
        let args = common_args(bandit, 3, path);
        assert!(args.iter().any(|arg| arg == "2220"));
        assert!(
            args.iter()
                .any(|arg| arg == "StrictHostKeyChecking=accept-new")
        );
        assert!(
            args.iter()
                .any(|arg| arg == "UserKnownHostsFile=/tmp/wargames-tui-test-known-hosts")
        );
        assert!(!args.iter().any(|arg| arg.contains("/dev/null")));
        let args = common_args(century, 3, path);
        assert!(args.iter().any(|arg| arg == "22"));
        assert!(
            args.iter()
                .any(|arg| arg == "century3@century.underthewire.tech")
        );
    }

    #[test]
    fn known_hosts_is_created_without_truncating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg/wargames-tui/known_hosts");
        ensure_known_hosts(&path).unwrap();
        assert!(path.is_file());
        fs::write(&path, "example host key\n").unwrap();
        ensure_known_hosts(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "example host key\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn askpass_script_uses_new_env_var_and_replaces_old_content() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("cfg/wargames-tui");
        create_private_dir(&config_dir).unwrap();
        fs::write(
            config_dir.join(ASKPASS_SCRIPT_NAME),
            "#!/bin/sh\nprintf '%s\\n' \"$BANDIT_TUI_ASKPASS_SECRET\"\n",
        )
        .unwrap();

        let script = ensure_askpass_script(&config_dir).unwrap();
        assert_eq!(script, config_dir.join(ASKPASS_SCRIPT_NAME));
        let body = fs::read_to_string(&script).unwrap();
        assert_eq!(body, ASKPASS_SCRIPT_BODY);
        assert!(!body.contains("BANDIT_TUI_ASKPASS_SECRET"));
        assert!(body.contains(ASKPASS_ENV_VAR));
        assert_eq!(ensure_askpass_script(&config_dir).unwrap(), script);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&script).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn askpass_script_reads_secret_from_environment() {
        let dir = tempfile::tempdir().unwrap();
        let script = ensure_askpass_script(dir.path()).unwrap();
        let output = Command::new("sh")
            .arg(&script)
            .env(ASKPASS_ENV_VAR, "stub-only-value")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "stub-only-value\n");
    }

    #[test]
    fn password_verification_uses_sshpass_when_available() {
        let games = games();
        let game = games.get("century").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let command = verification_command_with(
            game,
            3,
            &Secret::Password(TEST_SECRET.to_owned()),
            Path::new("/tmp/known-hosts"),
            &config_dir,
            true,
        )
        .unwrap();

        assert_eq!(command.get_program(), "sshpass");
        assert_eq!(env_value(&command, "SSHPASS").as_deref(), Some(TEST_SECRET));
        assert!(env_value(&command, "SSH_ASKPASS").is_none());
        assert!(!config_dir.join(ASKPASS_SCRIPT_NAME).exists());
        assert!(args(&command).ends_with(&[" -NoProfile -Command exit 0".to_owned()]));
    }

    #[test]
    fn password_verification_falls_back_without_sshpass() {
        let games = games();
        let game = games.get("bandit").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        let command = verification_command_with(
            game,
            3,
            &Secret::Password(TEST_SECRET.to_owned()),
            Path::new("/tmp/known-hosts"),
            &config_dir,
            false,
        )
        .unwrap();

        assert_eq!(command.get_program(), "ssh");
        let command_args = args(&command);
        for option in [
            "PreferredAuthentications=password",
            "PubkeyAuthentication=no",
            "NumberOfPasswordPrompts=1",
            "BatchMode=no",
            "UserKnownHostsFile=/tmp/known-hosts",
        ] {
            assert!(
                command_args.iter().any(|arg| arg == option),
                "missing {option}"
            );
        }
        assert!(command_args.ends_with(&["true".to_owned()]));
        assert!(!command_args.iter().any(|arg| arg.contains(TEST_SECRET)));
        assert_eq!(env_value(&command, "DISPLAY").as_deref(), Some(":0"));
        assert_eq!(
            env_value(&command, "SSH_ASKPASS_REQUIRE").as_deref(),
            Some("force")
        );
        assert_eq!(
            env_value(&command, ASKPASS_ENV_VAR).as_deref(),
            Some(TEST_SECRET)
        );
        assert!(Path::new(&env_value(&command, "SSH_ASKPASS").unwrap()).is_file());
    }

    #[test]
    fn private_key_verification_respects_allowed_kinds() {
        let games = games();
        let bandit = games.get("bandit").unwrap();
        let century = games.get("century").unwrap();
        let secret = Secret::PrivateKeyPath(PathBuf::from("/tmp/sshkey.private"));
        let command = verification_command_with(
            bandit,
            15,
            &secret,
            Path::new("/tmp/known-hosts"),
            Path::new("/tmp/unused-config"),
            false,
        )
        .unwrap();
        assert_eq!(command.get_program(), "ssh");
        let command_args = args(&command);
        assert!(command_args.iter().any(|arg| arg == "BatchMode=yes"));
        assert!(command_args.iter().any(|arg| arg == "IdentitiesOnly=yes"));
        assert!(command_args.ends_with(&["true".to_owned()]));
        assert!(env_value(&command, "SSH_ASKPASS").is_none());

        assert!(
            verification_command_with(
                century,
                3,
                &secret,
                Path::new("/tmp/known-hosts"),
                Path::new("/tmp/unused-config"),
                false,
            )
            .is_err()
        );
    }
}
