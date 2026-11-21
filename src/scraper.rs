//! Level instruction fetching, HTML parsing, and on-disk cache.

use anyhow::{Context, Result, bail};
use ego_tree::NodeRef;
use scraper::{ElementRef, Html, Selector, node::Node};
use serde::{Deserialize, Serialize};
use std::fs;
use std::ops::Deref;
use std::path::Path;
use std::time::Duration;

pub const BASE_URL: &str = "https://overthewire.org/wargames/bandit";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelInfo {
    /// Session level: the `banditN` account the user is logged into.
    pub level: u16,
    /// Source page number the instructions were read from (`level + 1`).
    pub page: u16,
    pub title: String,
    pub goal: String,
    pub commands: Vec<String>,
    pub reading: Vec<String>,
    pub notes: Vec<String>,
}

/// OverTheWire's "Level X" page (`banditX.html`) explains how to get from
/// bandit(X-1) into banditX. A user logged into banditL therefore needs page
/// L+1 to see the task they are actually playing. Saturating addition keeps
/// the helper total for every `u16` input.
pub fn page_for_level(level: u16) -> u16 {
    level.saturating_add(1)
}

pub fn level_url(level: u16) -> String {
    format!("{BASE_URL}/bandit{level}.html")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Intro,
    Goal,
    Commands,
    Reading,
    Other,
}

/// Parse the OverTheWire level page. The static HTML keeps content in
/// `main#content` under `<h2 id="level-goal">`, `<h2 id="commands-...">`, and
/// `<h2 id="helpful-reading-material">`; the title is injected by a script.
pub fn parse_level_html(html: &str, page: u16) -> Result<LevelInfo> {
    let document = Html::parse_document(html);
    let main_selector =
        Selector::parse("main#content").context("building selector for main#content")?;
    let main = document
        .select(&main_selector)
        .next()
        .context("page has no main#content element")?;

    if let Some(level_in_page) = level_from_script(&document)
        && level_in_page != page
    {
        bail!("page declares bandit{level_in_page} but bandit{page} was requested");
    }

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
        let name = element.value().name();
        if name == "h2" {
            let id = element.value().attr("id").unwrap_or_default();
            let heading = text_lines(&element).join(" ");
            section = if id.contains("level-goal") {
                Section::Goal
            } else if id.contains("commands") {
                Section::Commands
            } else if id.contains("reading") {
                Section::Reading
            } else {
                Section::Other
            };
            other_heading = heading;
            continue;
        }
        if !matches!(name, "p" | "ul" | "ol" | "pre" | "div") {
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
        bail!("no level content found in main#content");
    }

    // A bare parse has no session context: `level` and `page` both describe the
    // page that was parsed. `fetch_level` rewrites them to session/page values.
    Ok(LevelInfo {
        level: page,
        page,
        title: format!("Bandit Level {page}"),
        goal,
        commands,
        reading,
        notes,
    })
}

/// Extract the level number from `renderLevelTitle("bandit", N)` without a regex.
fn level_from_script(document: &Html) -> Option<u16> {
    let selector = Selector::parse("main#content script").ok()?;
    for script in document.select(&selector) {
        for text in script.text() {
            let Some(marker) = text.find("renderLevelTitle") else {
                continue;
            };
            let rest = &text[marker..];
            let Some(comma) = rest.find(',') else {
                continue;
            };
            let digits: String = rest[comma + 1..]
                .chars()
                .skip_while(|c| c.is_whitespace())
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(level) = digits.parse::<u16>() {
                return Some(level);
            }
        }
    }
    None
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

/// Load the cached instructions for a session level, if present. Entries that
/// are corrupt, use the old schema (no `page` field), or do not match the
/// expected session/page pair are ignored so they can be refetched.
pub fn load_cached(levels_dir: &Path, level: u16) -> Option<LevelInfo> {
    let page = page_for_level(level);
    let path = levels_dir.join(format!("bandit{page}.json"));
    let raw = fs::read_to_string(path).ok()?;
    let info: LevelInfo = serde_json::from_str(&raw).ok()?;
    (info.level == level && info.page == page).then_some(info)
}

/// Store a parsed level in the cache, keyed by its source page number.
/// Cache write failures are non-fatal.
pub fn cache_level(levels_dir: &Path, info: &LevelInfo) -> Result<()> {
    fs::create_dir_all(levels_dir).with_context(|| format!("creating {}", levels_dir.display()))?;
    let path = levels_dir.join(format!("bandit{}.json", info.page));
    let json = serde_json::to_string_pretty(info).context("serializing level")?;
    fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Cache first; fetch from the network on a miss or when `force` is set.
/// `level` is the session level; the page fetched/parsed is `page_for_level(level)`.
pub fn fetch_level(level: u16, levels_dir: &Path, force: bool) -> Result<LevelInfo> {
    if !force && let Some(cached) = load_cached(levels_dir, level) {
        return Ok(cached);
    }

    let page = page_for_level(level);
    let url = level_url(page);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("bandit-tui/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;
    let response = client
        .get(&url)
        .send()
        .with_context(|| format!("fetching {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("HTTP {} while fetching bandit{page}", status.as_u16());
    }
    let html = response.text().context("reading level page body")?;
    let mut info = parse_level_html(&html, page)?;
    // The page describes the transition into bandit(page), but the UI is
    // playing bandit(level): keep the session identity for title/cache checks.
    info.level = level;
    info.page = page;
    info.title = format!("Bandit Level {level}");
    // A cache failure must never fail a successful fetch.
    let _ = cache_level(levels_dir, &info);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEVEL0_FIXTURE: &str = include_str!("../tests/fixtures/bandit0.html");

    #[test]
    fn parses_level_zero_fixture() {
        let info = parse_level_html(LEVEL0_FIXTURE, 0).unwrap();
        assert_eq!(info.level, 0);
        assert_eq!(info.title, "Bandit Level 0");
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
        let info = parse_level_html(html, 14).unwrap();
        assert_eq!(info.commands, vec!["ssh", "scp", "nc"]);
        assert_eq!(info.reading, vec!["Keys", "Transferring Files"]);
        assert_eq!(info.notes, vec!["Hint: look in home"]);
        assert!(info.goal.contains("Use the key."));
        assert!(info.goal.contains("Read the hint."));
        assert!(parse_level_html(html, 13).is_err());
    }

    #[test]
    fn missing_content_is_an_error() {
        assert!(parse_level_html("<html><body>nothing</body></html>", 1).is_err());
    }

    #[test]
    fn page_for_level_maps_session_level_to_next_page() {
        assert_eq!(page_for_level(0), 1);
        assert_eq!(page_for_level(1), 2);
        assert_eq!(page_for_level(32), 33);
        assert_eq!(page_for_level(33), 34);
        assert_eq!(page_for_level(u16::MAX), u16::MAX);
    }

    #[test]
    fn cache_round_trip_and_force_fetch_path() {
        let dir = tempfile::tempdir().unwrap();
        let levels_dir = dir.path().join("levels");
        // Session level 0 is taught by page 1, as `fetch_level` would store it.
        let mut info = parse_level_html(LEVEL0_FIXTURE, 0).unwrap();
        info.page = 1;
        assert_eq!(info.level, 0);
        assert_eq!(load_cached(&levels_dir, 0), None);
        cache_level(&levels_dir, &info).unwrap();
        assert_eq!(load_cached(&levels_dir, 0), Some(info.clone()));
        // The cached copy is used without touching the network.
        assert_eq!(fetch_level(0, &levels_dir, false).unwrap(), info);
        assert_eq!(
            fs::read_to_string(levels_dir.join("bandit1.json")).unwrap(),
            serde_json::to_string_pretty(&info).unwrap()
        );
    }

    #[test]
    fn cache_rejects_old_schema_and_mismatched_identity() {
        let dir = tempfile::tempdir().unwrap();
        let levels_dir = dir.path().join("levels");
        fs::create_dir_all(&levels_dir).unwrap();

        // A pre-page entry at the page session level 0 now reads from is
        // unreadable and must be ignored, not served to the UI.
        fs::write(
            levels_dir.join("bandit1.json"),
            r#"{"level":0,"title":"Bandit Level 0","goal":"old","commands":[],"reading":[],"notes":[]}"#,
        )
        .unwrap();
        assert_eq!(load_cached(&levels_dir, 0), None);

        // Correctly rewritten info for session level 0 is accepted.
        let mut info = parse_level_html(LEVEL0_FIXTURE, 0).unwrap();
        info.page = 1;
        cache_level(&levels_dir, &info).unwrap();
        assert_eq!(load_cached(&levels_dir, 0), Some(info.clone()));

        // The same payload mis-keyed under another level's page is rejected.
        let mut mis_keyed = info;
        mis_keyed.level = 99;
        cache_level(&levels_dir, &mis_keyed).unwrap();
        assert_eq!(load_cached(&levels_dir, 0), None);
    }

    #[test]
    fn cache_entry_is_bound_to_its_session_level() {
        let dir = tempfile::tempdir().unwrap();
        let levels_dir = dir.path().join("levels");
        let mut info = parse_level_html(LEVEL0_FIXTURE, 0).unwrap();
        info.level = 1;
        info.page = 2;
        info.title = "Bandit Level 1".to_owned();
        cache_level(&levels_dir, &info).unwrap();

        assert_eq!(load_cached(&levels_dir, 1), Some(info.clone()));
        // Levels 0 and 2 read different page files, so they cannot pick up
        // level 1's instructions even with the same cache directory.
        assert_eq!(load_cached(&levels_dir, 0), None);
        assert_eq!(load_cached(&levels_dir, 2), None);
    }
}
