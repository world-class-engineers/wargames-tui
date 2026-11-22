//! TOML-driven wargame catalog and validation.
//!
//! The catalog shipped with the binary is embedded at compile time and seeded
//! into the user config directory on first run. After that the user's copy is
//! authoritative: it is never silently overwritten or merged.

use crate::credentials::{Paths, Secret, write_restricted_file};
use anyhow::{Context, Result, bail};
use scraper::Selector;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Catalog schema version understood by this build.
pub const CATALOG_VERSION: u32 = 1;
/// The catalog embedded in the binary and used to seed a new config directory.
pub const DEFAULT_CATALOG: &str = include_str!("../wargames.toml");

/// Authentication method accepted for a wargame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    Password,
    PrivateKeyPath,
}

impl SecretKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::PrivateKeyPath => "private key path",
        }
    }
}

/// Publicly known secret for a wargame's first playable level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitialSecret {
    pub kind: SecretKind,
    pub value: String,
}

/// CSS selectors used by the static-HTML source strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HtmlSelectors {
    pub content: String,
    pub heading: String,
    pub goal: String,
    pub commands: String,
    pub reading: String,
    pub text_tags: Vec<String>,
}

/// Validation of an HTML page against the requested source page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HtmlValidation {
    /// Expect a script call such as `renderLevelTitle("bandit", 1)`.
    ScriptCall {
        selector: String,
        marker: String,
        game_token: String,
        expected: String,
    },
}

/// Field names in the JS-embedded JSON object used by the SPA source strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsJsonFields {
    pub title: String,
    pub goal: String,
    pub notes: String,
}

/// How level instructions are discovered. Both variants are fully data-driven.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceSpec {
    Html {
        url_template: String,
        cache_version: u32,
        selectors: HtmlSelectors,
        validation: HtmlValidation,
    },
    JsJson {
        entry_url_template: String,
        asset_selector: String,
        asset_base_url: String,
        json_marker: String,
        game_key: String,
        cache_version: u32,
        fields: JsJsonFields,
    },
}

impl SourceSpec {
    pub fn cache_version(&self) -> u32 {
        match self {
            Self::Html { cache_version, .. } | Self::JsJson { cache_version, .. } => *cache_version,
        }
    }
}

/// One playable wargame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wargame {
    pub id: String,
    pub name: String,
    pub homepage: String,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub username_template: String,
    pub level_label: String,
    pub first_level: u16,
    pub final_level: u16,
    /// Added to the session level to obtain the source page number.
    pub page_offset: i32,
    pub allowed_secret_kinds: Vec<SecretKind>,
    /// Publicly known first-level secret, when one exists. Games whose first
    /// level hands out its password over a non-SSH service leave this unset;
    /// the user can still type the password at the SSH prompt.
    #[serde(default)]
    pub initial_secret: Option<InitialSecret>,
    pub verification_remote_command: String,
    pub completion_message: String,
    pub source: SourceSpec,
}

impl Wargame {
    /// Source page that teaches the task played at `level`.
    pub fn page_for_level(&self, level: u16) -> u16 {
        let page = i32::from(level).saturating_add(self.page_offset);
        page.clamp(0, i32::from(u16::MAX)) as u16
    }

    pub fn username(&self, level: u16) -> String {
        self.expand_template(&self.username_template, level, self.page_for_level(level))
    }

    pub fn level_label(&self, level: u16) -> String {
        self.expand_template(&self.level_label, level, self.page_for_level(level))
    }

    pub fn is_complete(&self, level: u16) -> bool {
        level >= self.final_level
    }

    /// Clamp a persisted level into this game's playable range.
    pub fn clamp_level(&self, level: u16) -> u16 {
        level.clamp(self.first_level, self.final_level)
    }

    pub fn initial_secret(&self) -> Option<Secret> {
        self.initial_secret
            .as_ref()
            .map(|secret| match secret.kind {
                SecretKind::Password => Secret::Password(secret.value.clone()),
                SecretKind::PrivateKeyPath => Secret::PrivateKeyPath(PathBuf::from(&secret.value)),
            })
    }

    pub fn allows(&self, kind: SecretKind) -> bool {
        self.allowed_secret_kinds.contains(&kind)
    }

    pub fn allows_private_key(&self) -> bool {
        self.allows(SecretKind::PrivateKeyPath)
    }

    pub fn source_cache_version(&self) -> u32 {
        self.source.cache_version()
    }

    /// Expand the placeholders supported by templates in the catalog.
    pub fn expand_template(&self, template: &str, level: u16, page: u16) -> String {
        template
            .replace("{game}", &self.id)
            .replace("{level}", &level.to_string())
            .replace("{page}", &page.to_string())
            .replace("{host}", &self.ssh_host)
            .replace("{port}", &self.ssh_port.to_string())
    }

    /// Directory holding this game's level cache, namespaced by game id.
    pub fn cache_dir(&self, levels_dir: &Path) -> PathBuf {
        levels_dir.join(&self.id)
    }

    /// Cache file for a source page. Entries also carry the game id and the
    /// source cache version, which are checked before an entry is accepted.
    pub fn cache_file(&self, levels_dir: &Path, page: u16) -> PathBuf {
        self.cache_dir(levels_dir).join(format!("{page}.json"))
    }

    /// Identity of the cache namespace, including the source cache version.
    pub fn cache_namespace(&self) -> String {
        format!("{}/v{}", self.id, self.source_cache_version())
    }
}

/// Parsed, validated catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub version: u32,
    pub wargames: Vec<Wargame>,
}

impl Catalog {
    /// Parse and validate a TOML document.
    pub fn parse(text: &str) -> Result<Self> {
        let catalog: Self = toml::from_str(text).context("parsing catalog TOML")?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn get(&self, id: &str) -> Option<&Wargame> {
        self.wargames.iter().find(|game| game.id == id)
    }

    pub fn first(&self) -> Option<&Wargame> {
        self.wargames.first()
    }

    fn validate(&self) -> Result<()> {
        if self.version != CATALOG_VERSION {
            bail!(
                "unsupported catalog version {} (expected {CATALOG_VERSION})",
                self.version
            );
        }
        if self.wargames.is_empty() {
            bail!("catalog defines no wargames");
        }

        let mut ids = HashSet::new();
        for game in &self.wargames {
            let where_ = format!("wargame '{}'", game.id);
            if game.id.is_empty() {
                bail!("a wargame has an empty id");
            }
            if !ids.insert(game.id.as_str()) {
                bail!("duplicate wargame id '{}'", game.id);
            }
            for (field, value) in [
                ("name", &game.name),
                ("homepage", &game.homepage),
                ("ssh_host", &game.ssh_host),
                ("username_template", &game.username_template),
                ("level_label", &game.level_label),
                (
                    "verification_remote_command",
                    &game.verification_remote_command,
                ),
                ("completion_message", &game.completion_message),
            ] {
                if value.trim().is_empty() {
                    bail!("{where_}: {field} must not be empty");
                }
            }
            if game.ssh_port == 0 {
                bail!("{where_}: ssh_port must be non-zero");
            }
            if game.first_level > game.final_level {
                bail!(
                    "{where_}: first_level {} exceeds final_level {}",
                    game.first_level,
                    game.final_level
                );
            }
            if game.page_for_level(game.final_level) == 0 && game.page_offset < 0 {
                bail!("{where_}: page_offset maps the final level to an invalid page");
            }
            if game.allowed_secret_kinds.is_empty() {
                bail!("{where_}: allowed_secret_kinds must not be empty");
            }
            if let Some(initial) = &game.initial_secret {
                if initial.value.trim().is_empty() {
                    bail!("{where_}: initial_secret value must not be empty");
                }
                if !game.allows(initial.kind) {
                    bail!(
                        "{where_}: initial secret kind {} is not allowed",
                        initial.kind.label()
                    );
                }
            }
            validate_url(&game.homepage, &format!("{where_}: homepage"))?;
            validate_placeholders(&game.username_template, &where_, "{level}")?;
            validate_placeholders(&game.level_label, &where_, "{level}")?;
            validate_source(game)?;
        }

        let unknown: Vec<_> = self
            .wargames
            .iter()
            .filter(|game| {
                game.id
                    .chars()
                    .any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            })
            .map(|game| game.id.clone())
            .collect();
        if !unknown.is_empty() {
            bail!("wargame ids may only contain letters, digits, '-' and '_': {unknown:?}");
        }
        Ok(())
    }
}

fn validate_source(game: &Wargame) -> Result<()> {
    let where_ = format!("wargame '{}' source", game.id);
    match &game.source {
        SourceSpec::Html {
            url_template,
            cache_version,
            selectors,
            validation,
        } => {
            validate_url_template(
                url_template,
                &format!("{where_}: url_template"),
                true,
                false,
            )?;
            validate_cache_version(*cache_version, &where_)?;
            for (field, selector) in [
                ("content", &selectors.content),
                ("heading", &selectors.heading),
                ("goal", &selectors.goal),
                ("commands", &selectors.commands),
                ("reading", &selectors.reading),
            ] {
                Selector::parse(selector)
                    .with_context(|| format!("{where_}: invalid {field} selector '{selector}'"))?;
            }
            if selectors.text_tags.is_empty() {
                bail!("{where_}: text_tags must not be empty");
            }
            for tag in &selectors.text_tags {
                Selector::parse(tag)
                    .with_context(|| format!("{where_}: invalid text tag selector '{tag}'"))?;
            }
            match validation {
                HtmlValidation::ScriptCall {
                    selector,
                    marker,
                    game_token,
                    expected,
                } => {
                    Selector::parse(selector)
                        .with_context(|| format!("{where_}: invalid validation selector"))?;
                    for (field, value) in [
                        ("marker", marker),
                        ("game_token", game_token),
                        ("expected", expected),
                    ] {
                        if value.is_empty() {
                            bail!("{where_}: validation {field} must not be empty");
                        }
                    }
                    if expected != "page" && expected != "level" {
                        bail!("{where_}: validation expected must be 'page' or 'level'");
                    }
                }
            }
        }
        SourceSpec::JsJson {
            entry_url_template,
            asset_selector,
            asset_base_url,
            json_marker,
            game_key,
            cache_version,
            fields,
        } => {
            validate_url_template(
                entry_url_template,
                &format!("{where_}: entry_url_template"),
                true,
                true,
            )?;
            validate_url(asset_base_url, &format!("{where_}: asset_base_url"))?;
            Selector::parse(asset_selector)
                .with_context(|| format!("{where_}: invalid asset_selector"))?;
            for (field, value) in [
                ("json_marker", json_marker),
                ("game_key", game_key),
                ("title", &fields.title),
                ("goal", &fields.goal),
                ("notes", &fields.notes),
            ] {
                if value.is_empty() {
                    bail!("{where_}: {field} must not be empty");
                }
            }
            validate_cache_version(*cache_version, &where_)?;
        }
    }
    Ok(())
}

fn validate_cache_version(version: u32, where_: &str) -> Result<()> {
    if version == 0 {
        bail!("{where_}: cache_version must be non-zero");
    }
    Ok(())
}

fn validate_placeholders(template: &str, where_: &str, required: &str) -> Result<()> {
    if !template.contains(required) {
        bail!("{where_}: template '{template}' must contain {required}");
    }
    Ok(())
}

fn validate_url_template(template: &str, where_: &str, page: bool, game: bool) -> Result<()> {
    if page && !template.contains("{page}") {
        bail!("{where_}: template '{template}' must contain {{page}}");
    }
    if game && !template.contains("{game}") {
        bail!("{where_}: template '{template}' must contain {{game}}");
    }
    let sample = template
        .replace("{page}", "1")
        .replace("{game}", "sample")
        .replace("{level}", "1");
    if sample.contains('{') || sample.contains('}') {
        bail!("{where_}: template '{template}' has an unknown placeholder");
    }
    validate_url(&sample, where_)
}

fn validate_url(url: &str, where_: &str) -> Result<()> {
    let parsed =
        reqwest::Url::parse(url).with_context(|| format!("{where_}: invalid URL '{url}'"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("{where_}: URL '{url}' must use http or https");
    }
    Ok(())
}

/// Load the catalog, seeding the embedded default when the file is absent.
/// An invalid user file is reported with its path and is never overwritten.
pub fn load_catalog(paths: &Paths) -> Result<Catalog> {
    let path = paths.catalog_file();
    if !path.exists() {
        write_restricted_file(&path, DEFAULT_CATALOG.as_bytes())
            .with_context(|| format!("seeding catalog at {}", path.display()))?;
    }
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("reading catalog at {}", path.display()))?;
    Catalog::parse(&raw).with_context(|| format!("invalid wargame catalog at {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::parse(DEFAULT_CATALOG).unwrap()
    }

    #[test]
    fn default_catalog_has_unique_overthewire_and_underthewire_games() {
        let catalog = catalog();
        assert_eq!(catalog.version, CATALOG_VERSION);
        assert_eq!(catalog.wargames.len(), 16);
        let ids: Vec<_> = catalog
            .wargames
            .iter()
            .map(|game| game.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "bandit",
                "leviathan",
                "krypton",
                "narnia",
                "behemoth",
                "utumno",
                "maze",
                "vortex",
                "manpage",
                "drifter",
                "formulaone",
                "century",
                "cyborg",
                "groot",
                "oracle",
                "trebek",
            ]
        );
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 16);
        assert_eq!(catalog.first().unwrap().id, "bandit");
        assert_eq!(
            catalog.get("leviathan").unwrap().name,
            "OverTheWire Leviathan"
        );
        assert_eq!(catalog.get("trebek").unwrap().name, "UnderTheWire Trebek");
    }

    #[test]
    fn default_catalog_helpers_match_verified_game_facts() {
        let catalog = catalog();
        let bandit = catalog.get("bandit").unwrap();
        assert_eq!(bandit.page_for_level(0), 1);
        assert_eq!(bandit.page_for_level(33), 34);
        assert_eq!(bandit.username(7), "bandit7");
        assert_eq!(bandit.level_label(7), "bandit7");
        assert_eq!(bandit.final_level, 33);
        assert!(bandit.is_complete(33));
        assert!(!bandit.is_complete(32));
        assert_eq!(
            bandit.initial_secret(),
            Some(Secret::Password("bandit0".to_owned()))
        );
        assert!(bandit.allows_private_key());
        assert!(bandit.allows(SecretKind::Password));
        assert_eq!(
            bandit.cache_file(Path::new("/cache/levels"), 1),
            Path::new("/cache/levels/bandit/1.json")
        );
        assert_eq!(bandit.cache_namespace(), "bandit/v1");

        let overthewire = [
            ("leviathan", 2223, 0, 7, Some("leviathan0")),
            ("krypton", 2231, 1, 7, Some("KRYPTONISGREAT")),
            ("narnia", 2226, 0, 9, Some("narnia0")),
            ("behemoth", 2221, 0, 8, Some("behemoth0")),
            ("utumno", 2227, 0, 9, Some("utumno0")),
            ("maze", 2225, 0, 8, Some("maze0")),
            ("vortex", 2228, 1, 26, None),
            ("manpage", 2224, 0, 6, Some("manpage0")),
            ("drifter", 2230, 0, 15, None),
            ("formulaone", 2232, 0, 5, None),
        ];
        for (id, port, first, last, initial) in overthewire {
            let game = catalog.get(id).unwrap();
            assert_eq!(game.ssh_host, format!("{id}.labs.overthewire.org"));
            assert_eq!(game.ssh_port, port);
            assert_eq!(game.first_level, first);
            assert_eq!(game.final_level, last);
            assert_eq!(game.page_for_level(first), first);
            assert_eq!(game.page_for_level(last), last);
            assert_eq!(game.username(first), format!("{id}{first}"));
            assert_eq!(game.level_label(first), format!("{id}{first}"));
            assert_eq!(
                game.initial_secret(),
                initial.map(|value| Secret::Password(value.to_owned()))
            );
            assert!(!game.allows_private_key());
            assert!(game.allows(SecretKind::Password));
            assert_eq!(game.verification_remote_command, "true");
            assert!(matches!(game.source, SourceSpec::Html { .. }));
        }

        for id in ["century", "cyborg", "groot", "oracle", "trebek"] {
            let game = catalog.get(id).unwrap();
            assert_eq!(game.page_for_level(1), 1);
            assert_eq!(game.page_for_level(15), 15);
            assert_eq!(game.username(3), format!("{id}3"));
            assert_eq!(game.level_label(3), format!("{} 3", capitalize(id)));
            assert_eq!(game.final_level, 15);
            assert_eq!(game.ssh_host, format!("{id}.underthewire.tech"));
            assert_eq!(game.ssh_port, 22);
            assert_eq!(
                game.initial_secret(),
                Some(Secret::Password(format!("{id}1")))
            );
            assert!(!game.allows_private_key());
            assert!(game.allows(SecretKind::Password));
            assert_eq!(
                game.verification_remote_command,
                " -NoProfile -Command exit 0"
            );
            assert_eq!(
                game.cache_file(Path::new("/cache/levels"), 3),
                Path::new("/cache/levels").join(id).join("3.json")
            );
            let SourceSpec::JsJson { game_key, .. } = &game.source else {
                panic!("{id} must use the js_json source");
            };
            assert_eq!(game_key, id);
        }
    }

    #[test]
    fn initial_secret_is_optional() {
        let without = DEFAULT_CATALOG.replacen(
            "initial_secret = { kind = \"password\", value = \"leviathan0\" }\n",
            "",
            1,
        );
        let catalog = Catalog::parse(&without).unwrap();
        assert_eq!(catalog.get("leviathan").unwrap().initial_secret(), None);
    }

    fn capitalize(text: &str) -> String {
        let mut chars = text.chars();
        match chars.next() {
            Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
            None => String::new(),
        }
    }

    #[test]
    fn validation_rejects_duplicate_ids_bad_levels_and_unknown_sources() {
        let duplicate = DEFAULT_CATALOG.replacen("id = \"century\"", "id = \"bandit\"", 1);
        assert!(Catalog::parse(&duplicate).is_err());

        let bad_levels = DEFAULT_CATALOG.replacen("first_level = 0", "first_level = 34", 1);
        assert!(Catalog::parse(&bad_levels).is_err());

        let unknown_source = DEFAULT_CATALOG.replacen("kind = \"html\"", "kind = \"magic\"", 1);
        let error = format!("{:#}", Catalog::parse(&unknown_source).unwrap_err());
        assert!(error.contains("magic"), "{error}");

        let bad_selector =
            DEFAULT_CATALOG.replacen("content = \"main#content\"", "content = \"[\"", 1);
        assert!(Catalog::parse(&bad_selector).is_err());

        let bad_url = DEFAULT_CATALOG.replacen(
            "url_template = \"https://overthewire.org/wargames/bandit/bandit{page}.html\"",
            "url_template = \"not a url {page}\"",
            1,
        );
        assert!(Catalog::parse(&bad_url).is_err());

        let empty_kinds = DEFAULT_CATALOG.replacen(
            "allowed_secret_kinds = [\"password\"]",
            "allowed_secret_kinds = []",
            1,
        );
        assert!(Catalog::parse(&empty_kinds).is_err());
    }

    #[test]
    fn missing_catalog_is_seeded_from_the_embedded_default() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            cache_dir: dir.path().join("cache"),
        };
        let loaded = load_catalog(&paths).unwrap();
        assert_eq!(loaded, catalog());
        assert_eq!(
            fs::read_to_string(paths.catalog_file()).unwrap(),
            DEFAULT_CATALOG
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(paths.catalog_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        // A seeded file is authoritative: later changes to the embedded
        // default never overwrite it.
        fs::write(paths.catalog_file(), "version = 1\nwargames = []\n").unwrap();
        assert!(load_catalog(&paths).is_err());
        assert_eq!(
            fs::read_to_string(paths.catalog_file()).unwrap(),
            "version = 1\nwargames = []\n"
        );
    }

    #[test]
    fn invalid_catalog_error_includes_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            cache_dir: dir.path().join("cache"),
        };
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(paths.catalog_file(), "version = 99\n").unwrap();
        let error = load_catalog(&paths).unwrap_err().to_string();
        assert!(error.contains("wargames.toml"), "{error}");
        assert!(
            error.contains(paths.catalog_file().to_str().unwrap()),
            "{error}"
        );
    }
}
