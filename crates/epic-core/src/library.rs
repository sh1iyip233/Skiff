//! Group the flat game list into main games and their add-ons.
//!
//! The services return one record per owned item, so a game with ten DLCs shows up as eleven
//! "games". [`library_view`] attaches every entry marked `is_dlc` to the entry named by its
//! `parent_app_name`; entries whose parent is missing stay in [`LibraryView::orphan_dlc`] instead
//! of pretending to be a playable game.
//!
//! Classification (which entries are DLC at all) happens in [`crate::api`] while catalog metadata is
//! available. This module is deliberately pure: it never touches the network, so it can be used by
//! the CLI and both GUIs on an already-cached list.

use crate::api::GameEntry;
use crate::config::Config;
use serde::Serialize;
use std::collections::BTreeMap;

/// One main game plus the add-ons owned for it.
#[derive(Debug, Clone, Serialize)]
pub struct GameGroup {
    pub game: GameEntry,
    pub dlc: Vec<GameEntry>,
}

/// The whole library after grouping.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LibraryView {
    /// Main games, in the order they appeared in the flat list.
    pub games: Vec<GameGroup>,
    /// DLC whose main game is not in the library (owned add-on without the base game).
    pub orphan_dlc: Vec<GameEntry>,
    /// Total number of attached (non-orphan) DLCs.
    pub dlc_count: usize,
}

impl GameGroup {
    /// Number of add-ons attached to this main game.
    pub fn dlc_count(&self) -> usize {
        self.dlc.len()
    }

    /// True when the needle matches the main game or any of its add-ons.
    ///
    /// `needle` must already be lower-cased by the caller.
    pub fn matches(&self, needle: &str) -> bool {
        entry_matches(&self.game, needle) || self.dlc.iter().any(|dlc| entry_matches(dlc, needle))
    }

    /// Find an add-on by app name or title (case-insensitive).
    pub fn find_dlc(&self, key: &str) -> Option<&GameEntry> {
        let key = key.trim().to_lowercase();
        self.dlc.iter().find(|dlc| entry_matches(dlc, &key))
    }
}

/// A selected install target: the main game plus the add-ons owned for it.
#[derive(Debug, Clone, Serialize)]
pub struct BundleSelection {
    pub base: GameEntry,
    pub dlc: Vec<GameEntry>,
    /// The entry the caller actually selected (main game or one of its add-ons).
    pub selected: GameEntry,
    /// The selection is an add-on whose main game is missing from the library.
    pub orphan: bool,
}

impl LibraryView {
    /// Number of main games (not counting add-ons).
    pub fn base_count(&self) -> usize {
        self.games.len()
    }

    /// Resolve an app name or title to a main game and all its owned add-ons.
    ///
    /// Matching is exact (case-insensitive), so a CLI/GUI selection never silently picks a
    /// similarly named game.
    pub fn select(&self, key: &str) -> Option<BundleSelection> {
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        for group in &self.games {
            if exact_match(&group.game, key) {
                return Some(BundleSelection {
                    base: group.game.clone(),
                    dlc: group.dlc.clone(),
                    selected: group.game.clone(),
                    orphan: false,
                });
            }
            if let Some(dlc) = group.dlc.iter().find(|dlc| exact_match(dlc, key)) {
                return Some(BundleSelection {
                    base: group.game.clone(),
                    dlc: group.dlc.clone(),
                    selected: dlc.clone(),
                    orphan: false,
                });
            }
        }
        if let Some(dlc) = self.orphan_dlc.iter().find(|dlc| exact_match(dlc, key)) {
            return Some(BundleSelection {
                base: dlc.clone(),
                dlc: Vec::new(),
                selected: dlc.clone(),
                orphan: true,
            });
        }
        None
    }

    /// Return a copy containing only groups / orphans matching `needle`.
    pub fn filtered(&self, needle: &str) -> LibraryView {
        let needle = needle.trim().to_lowercase();
        if needle.is_empty() {
            return self.clone();
        }
        let games: Vec<GameGroup> = self
            .games
            .iter()
            .filter(|group| group.matches(&needle))
            .cloned()
            .collect();
        let orphan_dlc: Vec<GameEntry> = self
            .orphan_dlc
            .iter()
            .filter(|game| entry_matches(game, &needle))
            .cloned()
            .collect();
        let dlc_count = games.iter().map(GameGroup::dlc_count).sum();
        LibraryView { games, orphan_dlc, dlc_count }
    }

    /// Find an orphan add-on by app name or title (case-insensitive).
    pub fn find_orphan(&self, key: &str) -> Option<&GameEntry> {
        let key = key.trim().to_lowercase();
        self.orphan_dlc.iter().find(|game| entry_matches(game, &key))
    }
}

/// Compare one entry against an already lower-cased needle.
fn entry_matches(entry: &GameEntry, needle: &str) -> bool {
    entry.app_name.to_lowercase().contains(needle) || entry.title.to_lowercase().contains(needle)
}

/// Exact (case-insensitive) app-name or title match, used for selection rather than search.
fn exact_match(entry: &GameEntry, key: &str) -> bool {
    entry.app_name.eq_ignore_ascii_case(key) || entry.title.eq_ignore_ascii_case(key)
}

/// Group a flat list into main games and add-ons.
///
/// Main games keep the input order (the API sorts by title); add-ons are sorted by title inside
/// their group, and orphans by title at the end.
pub fn library_view(games: &[GameEntry]) -> LibraryView {
    let mut groups: Vec<GameGroup> = Vec::new();
    let mut index: BTreeMap<String, usize> = BTreeMap::new();

    for game in games.iter().filter(|game| !game.is_dlc) {
        let key = game.app_name.to_lowercase();
        if index.contains_key(&key) {
            // Duplicate app names are not expected, but never lose a real entry: give it its own
            // group rather than silently dropping it.
            groups.push(GameGroup { game: game.clone(), dlc: Vec::new() });
            continue;
        }
        index.insert(key, groups.len());
        groups.push(GameGroup { game: game.clone(), dlc: Vec::new() });
    }

    let mut orphan_dlc: Vec<GameEntry> = Vec::new();
    for game in games.iter().filter(|game| game.is_dlc) {
        let parent = game.parent_app_name.to_lowercase();
        match index.get(&parent) {
            Some(&slot) => groups[slot].dlc.push(game.clone()),
            None => orphan_dlc.push(game.clone()),
        }
    }

    for group in groups.iter_mut() {
        group
            .dlc
            .sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    }
    orphan_dlc.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    let dlc_count = groups.iter().map(GameGroup::dlc_count).sum();

    LibraryView { games: groups, orphan_dlc, dlc_count }
}

/// Normalise a store title for parent matching: case, padding, repeated whitespace and the
/// trademark glyphs Epic appends to some titles.
pub fn normalize_title(text: &str) -> String {
    let replaced = text.replace(['\u{2122}', '\u{00ae}'], " ");
    replaced
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Optional manual overrides from `<state>/parents.json`.
///
/// Shape: `{ "<dlc appName or title>": "<main appName or title>" }`. Keys and values are matched
/// case-insensitively; an override also marks the entry as DLC, which is how catalog entries without
/// `mainGameItem` (bonus content, soundtracks, wallpapers) can be attached by hand.
pub fn load_parent_overrides() -> BTreeMap<String, String> {
    let path = Config::dir().join("parents.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str::<BTreeMap<String, String>>(&text) {
        Ok(map) => map
            .into_iter()
            .map(|(key, value)| (key.trim().to_lowercase(), value.trim().to_lowercase()))
            .filter(|(key, value)| !key.is_empty() && !value.is_empty())
            .collect(),
        Err(e) => {
            eprintln!("warning: ignoring {} ({e})", path.display());
            BTreeMap::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(app: &str, title: &str) -> GameEntry {
        GameEntry {
            app_name: app.into(),
            namespace: "ns".into(),
            catalog_item_id: format!("cid-{app}"),
            title: title.into(),
            ..Default::default()
        }
    }

    fn dlc(app: &str, title: &str, parent: &str) -> GameEntry {
        GameEntry {
            is_dlc: true,
            parent_app_name: parent.into(),
            ..game(app, title)
        }
    }

    #[test]
    fn groups_dlc_under_its_parent() {
        let games = vec![
            game("Ginger", "Cyberpunk 2077"),
            dlc("phantom", "Cyberpunk 2077: Phantom Liberty", "Ginger"),
            dlc("redmod", "Cyberpunk 2077 - REDmod", "Ginger"),
            game("Other", "Other Game"),
        ];
        let view = library_view(&games);
        assert_eq!(view.games.len(), 2);
        assert_eq!(view.games[0].dlc_count(), 2);
        assert_eq!(view.dlc_count, 2);
        assert!(view.orphan_dlc.is_empty());
        assert_eq!(view.games[0].dlc[0].title, "Cyberpunk 2077 - REDmod");
    }

    #[test]
    fn keeps_dlc_without_parent_as_orphan() {
        let games = vec![game("Ginger", "Cyberpunk 2077"), dlc("orphan", "Lost DLC", "Missing")];
        let view = library_view(&games);
        assert_eq!(view.games.len(), 1);
        assert_eq!(view.orphan_dlc.len(), 1);
        assert_eq!(view.dlc_count, 0);
    }

    #[test]
    fn filtering_matches_addon_titles() {
        let games = vec![
            game("Ginger", "Cyberpunk 2077"),
            dlc("phantom", "Cyberpunk 2077: Phantom Liberty", "Ginger"),
            game("Other", "Other Game"),
        ];
        let view = library_view(&games).filtered("phantom");
        assert_eq!(view.games.len(), 1);
        assert_eq!(view.games[0].game.app_name, "Ginger");
        assert!(view.games[0].find_dlc("phantom").is_some());
    }

    #[test]
    fn normalises_titles() {
        assert_eq!(normalize_title("  Some   Game\u{2122} "), "some game");
    }
}
