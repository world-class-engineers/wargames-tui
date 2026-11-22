//! Level instruction fetching, source-specific parsing, and on-disk cache.

use crate::credentials::create_private_dir;
use crate::wargame::{HtmlValidation, SourceSpec, Wargame};
use anyhow::{Context, Result, bail};
use ego_tree::NodeRef;
use scraper::{ElementRef, Html, Selector, node::Node};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::ops::Deref;
use std::path::Path;
use std::time::Duration;

/// Normalized instructions for one playable level. `page` is the source page
/// number the instructions came from; `game_id`/`source_cache_version` bind a
/// cached entry to the catalog record that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelInfo {
    pub game_id: String,
    pub source_cache_version: u32,
    pub level: u16,
    pub page: u16,
    pub title: String,
    pub goal: String,
    pub commands: Vec<String>,
    pub reading: Vec<String>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Intro,
    Goal,
    Commands,
    Reading,
    Other,
}

/// Parse a static-HTML level page using the selectors and validation configured
/// for `game`. `page` is the source page being parsed.
pub fn parse_level_html(game: &Wargame, html: &str, page: u16) -> Result<LevelInfo> {
    let SourceSpec::Html {
        selectors,
        validation,
        ..
    } = &game.source
    else {
        bail!("wargame '{}' does not use the html source", game.id);
    };

    let document = Html::parse_document(html);
    let main_selector = Selector::parse(&selectors.content)
        .with_context(|| format!("invalid content selector for '{}'", game.id))?;
    let heading_selector = Selector::parse(&selectors.heading)
        .with_context(|| format!("invalid heading selector for '{}'", game.id))?;
    let text_selector = Selector::parse(&selectors.text_tags.join(", "))
        .with_context(|| format!("invalid text tag selectors for '{}'", game.id))?;
    let main = document
        .select(&main_selector)
        .next()
        .with_context(|| format!("page has no '{}' element", selectors.content))?;

    validate_page(game, &document, validation, page)?;

    let mut section = Section::Intro;
    let mut goal_lines = Vec::new();
    let mut command_lines = Vec::new();
    let mut reading = Vec::new();
    let mut notes = Vec::new();
    let mut other_heading = String::new();

    for child in main.children() {
        let Some(element) = ElementRef::wrap(child) else {
            continue;
        };
        if heading_selector.matches(&element) {
            let id = element.value().attr("id").unwrap_or_default();
            let heading = text_lines(&element).join(" ");
            section = if id.contains(&selectors.goal) {
                Section::Goal
            } else if id.contains(&selectors.commands) {
                Section::Commands
            } else if id.contains(&selectors.reading) {
                Section::Reading
            } else {
                Section::Other
            };
            other_heading = heading;
            continue;
        }
        if !text_selector.matches(&element) {
            continue;
        }
        let lines = text_lines(&element);
        match section {
            Section::Goal => goal_lines.extend(lines),
            Section::Commands => command_lines.extend(lines),
            Section::Reading => reading.extend(lines),
            Section::Intro => notes.extend(lines),
            Section::Other => {
                for line in lines {
                    let prefixed = if other_heading.is_empty() {
                        line
                    } else {
                        format!("{other_heading}: {line}")
                    };
                    notes.push(prefixed);
                }
            }
        }
    }

    let commands: Vec<String> = command_lines
        .iter()
        .flat_map(|line| line.split(','))
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect();

    let goal = goal_lines.join("\n");
    if goal.is_empty() && commands.is_empty() && reading.is_empty() && notes.is_empty() {
        bail!("no level content found in '{}'", selectors.content);
    }

    Ok(LevelInfo {
        game_id: game.id.clone(),
        source_cache_version: game.source_cache_version(),
        level: page,
        page,
        title: format!("{} Level {page}", game.name),
        goal,
        commands,
        reading,
        notes,
    })
}

/// Check the configured script call (for example
/// `renderLevelTitle("bandit", 1)`) against the requested source identifier.
fn validate_page(
    game: &Wargame,
    document: &Html,
    validation: &HtmlValidation,
    page: u16,
) -> Result<()> {
    let HtmlValidation::ScriptCall {
        selector,
        marker,
        game_token,
        expected,
    } = validation;
    // Catalogs may declare that the script names the session level rather than
    // the source page. Convert when needed (page = level + page_offset).
    let expected_number = if expected == "level" {
        (i32::from(page) - game.page_offset).clamp(0, i32::from(u16::MAX)) as u16
    } else {
        page
    };
    let script_selector = Selector::parse(selector)
        .with_context(|| format!("invalid validation selector for '{}'", game.id))?;
    for script in document.select(&script_selector) {
        for text in script.text() {
            let Some(position) = text.find(marker) else {
                continue;
            };
            let rest = &text[position..];
            if let Some(token) = rest.find(game_token)
                && let Some(comma) = rest[token..].find(',')
            {
                let digits: String = rest[token + comma + 1..]
                    .chars()
                    .skip_while(|character| character.is_whitespace())
                    .take_while(|character| character.is_ascii_digit())
                    .collect();
                if let Ok(number) = digits.parse::<u16>()
                    && number != expected_number
                {
                    bail!(
                        "page declares {game_token}{number} but {game_token}{expected_number} was expected ({expected})"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Normalized text lines of an element, treating `<br>` and block children as
/// line breaks so adjacent nodes do not run together.
fn text_lines(element: &ElementRef) -> Vec<String> {
    let mut raw = String::new();
    collect_text(*element.deref(), &mut raw);
    raw.split('\n')
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect()
}

fn collect_text(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(text) => out.push_str(&text.text),
        Node::Element(element) => {
            let block = matches!(
                element.name(),
                "br" | "p" | "li" | "ul" | "ol" | "div" | "pre" | "h1" | "h2" | "h3"
            );
            if block && !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            for child in node.children() {
                collect_text(child, out);
            }
            if block && !out.ends_with('\n') {
                out.push('\n');
            }
        }
        _ => {}
    }
}

/// Find the versioned JS asset referenced by an UnderTheWire SPA page and
/// resolve it against the configured asset base URL.
pub fn discover_asset_url(game: &Wargame, entry_html: &str) -> Result<String> {
    let SourceSpec::JsJson {
        asset_selector,
        asset_base_url,
        ..
    } = &game.source
    else {
        bail!("wargame '{}' does not use the js_json source", game.id);
    };
    let document = Html::parse_document(entry_html);
    let selector = Selector::parse(asset_selector)
        .with_context(|| format!("invalid asset selector for '{}'", game.id))?;
    let src = document
        .select(&selector)
        .next()
        .and_then(|element| element.value().attr("src"))
        .with_context(|| format!("no JS asset matching '{asset_selector}'"))?;
    let base = reqwest::Url::parse(asset_base_url)
        .with_context(|| format!("invalid asset base URL '{asset_base_url}'"))?;
    let resolved = base
        .join(src)
        .with_context(|| format!("could not resolve JS asset '{src}'"))?;
    Ok(resolved.to_string())
}

/// Decode the single-quoted JS string following `JSON.parse(` in an asset.
pub fn decode_json_literal(js: &str, marker: &str) -> Result<String> {
    // Configured markers usually include the opening quote (`JSON.parse('`).
    // Search for that exact sequence: bundles also contain unrelated
    // `JSON.parse(...)` calls, and matching only the function name used to
    // stop at the first non-literal call.
    let body_start = if marker.ends_with('\'') {
        let start = js
            .find(marker)
            .with_context(|| format!("JS asset does not contain '{marker}'"))?;
        start + marker.len()
    } else {
        // Backward-compatible form without the opening quote: find the marker,
        // then the next single quote.
        let start = js
            .find(marker)
            .with_context(|| format!("JS asset does not contain '{marker}'"))?;
        let after = start + marker.len();
        let quote = js[after..].find('\'').with_context(|| {
            format!("JS asset does not contain a single-quoted JSON literal after '{marker}'")
        })?;
        after + quote + 1
    };
    let body = &js[body_start..];

    let mut decoded = String::new();
    let mut chars = body.chars();
    while let Some(character) = chars.next() {
        match character {
            '\'' => return Ok(decoded),
            '\\' => match chars.next() {
                Some('\'') => decoded.push('\''),
                Some('\\') => decoded.push('\\'),
                Some('n') => decoded.push('\n'),
                Some('r') => decoded.push('\r'),
                Some('t') => decoded.push('\t'),
                Some(other) => {
                    // Preserve JSON escapes such as `\"` and `\uXXXX` for the
                    // JSON parser to interpret.
                    decoded.push('\\');
                    decoded.push(other);
                }
                None => bail!("unterminated JS string literal"),
            },
            other => decoded.push(other),
        }
    }
    bail!("unterminated JS string literal")
}

/// Normalize one configured field from a JS-JSON level object.
fn json_field(object: &Value, key: &str) -> Option<String> {
    match object.get(key)? {
        Value::String(text) => Some(text.trim().to_owned()).filter(|text| !text.is_empty()),
        Value::Array(items) => {
            let lines: Vec<String> = items
                .iter()
                .filter_map(|item| item.as_str())
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
                .collect();
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Parse level data embedded in a versioned JS asset.
pub fn parse_js_json(game: &Wargame, js: &str, page: u16) -> Result<LevelInfo> {
    let SourceSpec::JsJson {
        json_marker,
        game_key,
        fields,
        ..
    } = &game.source
    else {
        bail!("wargame '{}' does not use the js_json source", game.id);
    };
    let decoded = decode_json_literal(js, json_marker)?;
    let root: Value = serde_json::from_str(&decoded).context("parsing JS-embedded JSON")?;
    let level = root
        .get(game_key)
        .and_then(|game| game.get(page.to_string()))
        .with_context(|| format!("{game_key} has no level {page} in JS-embedded JSON"))?;

    let title = json_field(level, &fields.title)
        .with_context(|| format!("level {page} has no '{}' field", fields.title))?;
    let goal = json_field(level, &fields.goal)
        .with_context(|| format!("level {page} has no '{}' field", fields.goal))?;
    let notes = json_field(level, &fields.notes).unwrap_or_default();

    Ok(LevelInfo {
        game_id: game.id.clone(),
        source_cache_version: game.source_cache_version(),
        level: page,
        page,
        title,
        goal,
        commands: Vec::new(),
        reading: Vec::new(),
        notes: notes.lines().map(str::to_owned).collect(),
    })
}

/// Load cached instructions, rejecting entries from another game, another
/// source version, or a different level/page pair.
pub fn load_cached(game: &Wargame, levels_dir: &Path, level: u16) -> Option<LevelInfo> {
    let page = game.page_for_level(level);
    let path = game.cache_file(levels_dir, page);
    let raw = fs::read_to_string(path).ok()?;
    let info: LevelInfo = serde_json::from_str(&raw).ok()?;
    (info.game_id == game.id
        && info.source_cache_version == game.source_cache_version()
        && info.level == level
        && info.page == page
        && !info.title.is_empty()
        && !info.goal.is_empty())
    .then_some(info)
}

/// Store a parsed level in the cache, keyed by game id and source page.
/// Cache write failures are non-fatal to the caller.
pub fn cache_level(game: &Wargame, levels_dir: &Path, info: &LevelInfo) -> Result<()> {
    let dir = game.cache_dir(levels_dir);
    create_private_dir(&dir)?;
    let path = game.cache_file(levels_dir, info.page);
    let json = serde_json::to_string_pretty(info).context("serializing level")?;
    fs::write(&path, json).with_context(|| {
        format!(
            "writing {} for cache namespace {}",
            path.display(),
            game.cache_namespace()
        )
    })?;
    Ok(())
}

fn fetch_text(client: &reqwest::blocking::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("fetching {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {} while fetching {url}", status.as_u16());
    }
    response
        .text()
        .with_context(|| format!("reading response from {url}"))
}

/// Cache first; fetch from the network on a miss or when `force` is set.
pub fn fetch_level(
    game: &Wargame,
    level: u16,
    levels_dir: &Path,
    force: bool,
) -> Result<LevelInfo> {
    if !force && let Some(cached) = load_cached(game, levels_dir, level) {
        return Ok(cached);
    }

    let page = game.page_for_level(level);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("wargames-tui/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    let mut info = match &game.source {
        SourceSpec::Html { url_template, .. } => {
            let url = game.expand_template(url_template, level, page);
            let html = fetch_text(&client, &url)?;
            let mut info = parse_level_html(game, &html, page)?;
            info.level = level;
            info.title = format!("{} — Level {level}", game.level_label(level));
            info
        }
        SourceSpec::JsJson {
            entry_url_template, ..
        } => {
            let url = game.expand_template(entry_url_template, level, page);
            let html = fetch_text(&client, &url)?;
            let asset_url = discover_asset_url(game, &html)?;
            let js = fetch_text(&client, &asset_url)?;
            let mut info = parse_js_json(game, &js, page)?;
            // The SPA asset is keyed by source page; the UI is playing `level`.
            info.level = level;
            info
        }
    };
    info.game_id = game.id.clone();
    info.source_cache_version = game.source_cache_version();
    info.page = page;
    // A cache failure must never fail a successful fetch.
    let _ = cache_level(game, levels_dir, &info);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wargame::Catalog;

    const LEVEL0_FIXTURE: &str = include_str!("../tests/fixtures/bandit0.html");
    const SPA_FIXTURE: &str = include_str!("../tests/fixtures/underthewire_spa.html");
    const JS_FIXTURE: &str = include_str!("../tests/fixtures/underthewire_app.js");

    fn catalog() -> Catalog {
        Catalog::parse(crate::wargame::DEFAULT_CATALOG).unwrap()
    }

    #[test]
    fn parses_bandit_fixture_with_configured_selectors() {
        let game = catalog().get("bandit").unwrap().clone();
        let info = parse_level_html(&game, LEVEL0_FIXTURE, 0).unwrap();
        assert_eq!(info.game_id, "bandit");
        assert_eq!(info.source_cache_version, 1);
        assert_eq!(info.level, 0);
        assert_eq!(info.page, 0);
        assert!(info.goal.contains("bandit.labs.overthewire.org"));
        assert!(info.goal.contains("port 2220"));
        assert_eq!(info.commands, vec!["ssh"]);
        assert!(
            info.reading
                .iter()
                .any(|line| line.contains("Secure Shell"))
        );
        assert!(info.notes.is_empty());
    }

    #[test]
    fn parses_notes_and_rejects_wrong_level() {
        let game = catalog().get("bandit").unwrap().clone();
        let html = r#"
            <main id="content">
              <script>renderLevelTitle("bandit", 14);</script>
              <h2 id="level-goal">Level Goal</h2><p>Use the key.<br />Read the hint.</p>
              <h2 id="commands-you-may-need-to-solve-this-level">Commands</h2><p>ssh, scp, nc</p>
              <h2 id="helpful-reading-material">Helpful Reading Material</h2>
              <ul><li>Keys</li><li>Transferring Files</li></ul>
              <h2 id="hint">Hint</h2><p>look in home</p>
            </main>
        "#;
        let info = parse_level_html(&game, html, 14).unwrap();
        assert_eq!(info.commands, vec!["ssh", "scp", "nc"]);
        assert_eq!(info.reading, vec!["Keys", "Transferring Files"]);
        assert_eq!(info.notes, vec!["Hint: look in home"]);
        assert!(info.goal.contains("Use the key."));
        assert!(info.goal.contains("Read the hint."));
        assert!(parse_level_html(&game, html, 13).is_err());
    }

    #[test]
    fn parses_newer_overthewire_pages_without_bandit_section_ids() {
        let games = catalog();
        let krypton = games.get("krypton").unwrap().clone();
        let html = r#"
            <main id="content">
              <div id="title"></div>
              <h2 id="level-info">Level Info</h2>
              <p>The password for level 2 is in the file 'krypton2'.</p>
              <script>renderLevelTitle("krypton", 1);</script>
            </main>
        "#;
        let info = parse_level_html(&krypton, html, 1).unwrap();
        assert!(info.goal.is_empty());
        assert!(
            info.notes
                .iter()
                .any(|line| line.contains("Level Info: The password"))
        );

        let vortex = games.get("vortex").unwrap().clone();
        let html = r#"
            <main id="content">
              <div id="title"></div>
              <h2 id="canary-values">Canary Values</h2>
              <p>We are looking for a specific value in ptr.</p>
              <h2 id="reading-material">Reading Material</h2>
              <ul><li>Smashing the Stack for Fun and Profit</li></ul>
              <script>renderLevelTitle("vortex", 1);</script>
            </main>
        "#;
        let info = parse_level_html(&vortex, html, 1).unwrap();
        assert_eq!(info.reading, vec!["Smashing the Stack for Fun and Profit"]);
        assert!(
            info.notes
                .iter()
                .any(|line| line.contains("Canary Values: We are looking"))
        );
    }

    #[test]
    fn validation_can_expect_the_session_level_instead_of_the_page() {
        let mut game = catalog().get("bandit").unwrap().clone();
        game.page_offset = 1;
        let html = r#"
            <main id="content">
              <script>renderLevelTitle("bandit", 3);</script>
              <h2 id="level-goal">Level Goal</h2><p>Use the key.</p>
            </main>
        "#;
        let document = Html::parse_document(html);
        let SourceSpec::Html { validation, .. } = &game.source else {
            unreachable!("bandit uses html sources");
        };
        let mut expected_level = validation.clone();
        let expected = match &mut expected_level {
            HtmlValidation::ScriptCall { expected, .. } => expected,
        };
        *expected = "level".to_owned();
        // Level 3 is served from page 4 with a +1 page offset.
        validate_page(&game, &document, &expected_level, 4).unwrap();
        validate_page(&game, &document, validation, 4).unwrap_err();
        let wrong_level = r#"
            <main id="content">
              <script>renderLevelTitle("bandit", 4);</script>
              <h2 id="level-goal">Level Goal</h2><p>Use the key.</p>
            </main>
        "#;
        validate_page(
            &game,
            &Html::parse_document(wrong_level),
            &expected_level,
            4,
        )
        .unwrap_err();
    }

    #[test]
    fn missing_content_is_an_error() {
        let game = catalog().get("bandit").unwrap().clone();
        assert!(parse_level_html(&game, "<html><body>nothing</body></html>", 1).is_err());
    }

    #[test]
    fn json_literal_search_skips_unrelated_json_parse_calls() {
        let js = r#"
            const parsed = JSON.parse(someVariable);
            const levels = JSON.parse('{"century":{"1":{"title":"T","prompt":"P"}}}');
        "#;
        let decoded = decode_json_literal(js, "JSON.parse('").unwrap();
        assert!(decoded.starts_with("{\"century\""));
    }

    #[test]
    fn discovers_hashed_asset_and_decodes_embedded_json() {
        let game = catalog().get("century").unwrap().clone();
        let asset = discover_asset_url(&game, SPA_FIXTURE).unwrap();
        assert_eq!(asset, "https://underthewire.tech/assets/main-4f3c9a.js");
        let decoded = decode_json_literal(JS_FIXTURE, "JSON.parse('").unwrap();
        assert!(decoded.starts_with("{\"century\""));

        let info = parse_js_json(&game, JS_FIXTURE, 1).unwrap();
        assert_eq!(info.game_id, "century");
        assert_eq!(info.source_cache_version, 1);
        assert_eq!(info.level, 1);
        assert_eq!(info.page, 1);
        assert_eq!(info.title, "Century Level 1");
        assert_eq!(
            info.goal,
            "Connect to century1 and read the welcome message."
        );
        assert_eq!(info.notes, vec!["Level 0 is onboarding."]);
        assert!(info.commands.is_empty());
        assert!(parse_js_json(&game, JS_FIXTURE, 3).is_err());
    }

    #[test]
    fn json_field_handles_arrays() {
        let value: Value = serde_json::from_str(r#"{"note":["first","second"]}"#).unwrap();
        assert_eq!(json_field(&value, "note").as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn cache_is_namespaced_by_game_and_validates_identity() {
        let dir = tempfile::tempdir().unwrap();
        let levels_dir = dir.path().join("levels");
        let games = catalog();
        let bandit = games.get("bandit").unwrap();
        let century = games.get("century").unwrap();

        let mut info = parse_level_html(bandit, LEVEL0_FIXTURE, 0).unwrap();
        info.level = 0;
        info.page = 1;
        cache_level(bandit, &levels_dir, &info).unwrap();
        assert_eq!(load_cached(bandit, &levels_dir, 0), Some(info.clone()));
        // The same page number for another game must not be served.
        assert_eq!(load_cached(century, &levels_dir, 0), None);

        let mut ui = parse_js_json(century, JS_FIXTURE, 1).unwrap();
        assert_eq!(load_cached(century, &levels_dir, 1), None);
        cache_level(century, &levels_dir, &ui).unwrap();
        assert_eq!(load_cached(century, &levels_dir, 1), Some(ui.clone()));
        assert_eq!(load_cached(bandit, &levels_dir, 1), None);

        // A different source cache version invalidates an otherwise valid entry.
        ui.source_cache_version += 1;
        cache_level(century, &levels_dir, &ui).unwrap();
        assert_eq!(load_cached(century, &levels_dir, 1), None);

        // Missing required fields are a cache miss, not a crash.
        let mut stale = ui;
        stale.source_cache_version = 1;
        stale.title.clear();
        cache_level(century, &levels_dir, &stale).unwrap();
        assert_eq!(load_cached(century, &levels_dir, 1), None);
    }

    #[test]
    fn cache_round_trip_needs_no_network() {
        let dir = tempfile::tempdir().unwrap();
        let levels_dir = dir.path().join("levels");
        let game = catalog().get("bandit").unwrap().clone();
        let mut info = parse_level_html(&game, LEVEL0_FIXTURE, 0).unwrap();
        info.level = 0;
        info.page = 1;
        cache_level(&game, &levels_dir, &info).unwrap();
        assert_eq!(fetch_level(&game, 0, &levels_dir, false).unwrap(), info);
        assert_eq!(
            fs::read_to_string(game.cache_file(&levels_dir, 1)).unwrap(),
            serde_json::to_string_pretty(&info).unwrap()
        );
    }
}
