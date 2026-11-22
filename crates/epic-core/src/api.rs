//! Epic web API endpoints: entitlements, library, catalog details and launcher build assets.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::http::{Client, Request};
use crate::library;
use crate::util;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Entitlement service (what the account owns).
pub const ENTITLEMENT_HOST: &str = "https://entitlement-public-service-prod08.ol.epicgames.com";
/// Library service (app names and product ids).
pub const LIBRARY_HOST: &str = "https://library-service.live.use1a.on.epicgames.com";
/// Catalog service (titles, developers, folder names).
/// Third-party catalogue that mirrors Epic's catalog item ids with human readable titles.
pub const EGDATA_HOST: &str = "https://api.egdata.app";
pub const CATALOG_HOST: &str = "https://catalog-public-service-prod.ol.epicgames.com";
/// Launcher assets (builds and manifest URIs).
pub const LAUNCHER_HOST: &str = "https://launcher-public-service-prod.ol.epicgames.com";

const PAGE: usize = 1000;
const CATALOG_BATCH: usize = 20;

/// One entitlement returned by the entitlement service.
#[derive(Debug, Clone, Serialize)]
pub struct Entitlement {
    pub namespace: String,
    pub catalog_item_id: String,
    pub entitlement_name: String,
    pub entitlement_type: String,
    pub active: bool,
}

/// One owned item from the library service.
#[derive(Debug, Clone, Serialize)]
pub struct LibraryItem {
    pub namespace: String,
    pub catalog_item_id: String,
    pub app_name: String,
    pub product_id: String,
}

/// Catalog metadata for one item.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CatalogItem {
    pub id: String,
    pub namespace: String,
    pub title: String,
    pub developer: String,
    pub folder_name: String,
    pub categories: Vec<String>,
    /// `categories` contains `addons`.
    pub is_addon: bool,
    /// `mainGameItem.id` when Epic marks this item as an add-on.
    pub main_game_id: String,
    /// `mainGameItem.title`, used as a fallback when the id is not owned/not in the list.
    pub main_game_title: String,
    /// App names advertised by `mainGameItem.releaseInfo`, another parent-matching signal.
    pub main_game_app_names: Vec<String>,
    /// App names advertised by this item's own `releaseInfo`.
    pub release_app_names: Vec<String>,
}

/// A game the account can install, merged from the three services.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GameEntry {
    pub app_name: String,
    pub namespace: String,
    pub catalog_item_id: String,
    pub title: String,
    pub developer: String,
    pub folder_name: String,
    pub entitlement_type: String,
    /// True when this entry is an add-on of another entry.
    #[serde(default)]
    pub is_dlc: bool,
    /// `app_name` of the main game; empty for main games and unresolved add-ons.
    #[serde(default)]
    pub parent_app_name: String,
    /// Catalog categories, copied so `.egstore` can describe an add-on without another lookup.
    #[serde(default)]
    pub categories: Vec<String>,
}

/// One entry of the launcher's entitled-build list.
#[derive(Debug, Clone, Serialize)]
pub struct EntitledAsset {
    pub app_name: String,
    pub catalog_item_id: String,
    pub namespace: String,
    pub label_name: String,
    pub build_version: String,
}
/// Resolved build for one app on one platform/label.
#[derive(Debug, Clone, Serialize)]
pub struct BuildInfo {
    pub app_name: String,
    pub namespace: String,
    pub catalog_item_id: String,
    pub label_name: String,
    pub build_version: String,
    pub manifest_uri: String,
    pub manifest_hash: String,
    pub base_urls: Vec<String>,
}

/// Coarse progress observer for long multi-request listings.
///
/// Receives short, stable, UI-neutral stage ids; a front end is expected to map them to its own
/// wording. Reporting never affects the result, so a slow or noisy observer only costs the caller's
/// time — the listing itself is unchanged.
pub type StageHook = Arc<dyn Fn(&str) + Send + Sync>;

/// Stage ids emitted through [`StageHook`].
pub mod stage {
    /// Reading the account's library records.
    pub const LIBRARY: &str = "library";
    /// Reading entitlement records (fallback path).
    pub const ENTITLEMENTS: &str = "entitlements";
    /// Reading the launcher asset list (fallback path).
    pub const ASSETS: &str = "assets";
    /// Fetching catalog metadata for many namespaces.
    pub const CATALOG: &str = "catalog";
    /// Resolving store titles for codename-only entries.
    pub const TITLES: &str = "titles";
    /// Merging and sorting the final list.
    pub const MERGE: &str = "merge";
}

/// Bound API client for one account.
#[derive(Clone)]
pub struct Api<'a> {
    pub client: &'a Client,
    pub cfg: &'a Config,
    /// Pre-rendered `Authorization` header value (scheme included).
    pub auth: String,
    stage: Option<StageHook>,
    cancel: Option<Arc<AtomicBool>>,
}

impl<'a> Api<'a> {
    /// Create a client for one bearer token.
    pub fn new(client: &'a Client, cfg: &'a Config, token: &'a str) -> Api<'a> {
        Api { client, cfg, auth: format!("bearer {token}"), stage: None, cancel: None }
    }

    /// Report coarse progress while listing the library. Builder-style so existing call sites are
    /// unaffected.
    #[must_use]
    pub fn with_stage(mut self, hook: StageHook) -> Api<'a> {
        self.stage = Some(hook);
        self
    }

    /// Abort control-plane requests (build lookup, manifest download, CDN probe) when `flag` is
    /// raised, so cancelling during "preparing" is not stuck behind a slow request.
    #[must_use]
    pub fn with_cancel(mut self, flag: Arc<AtomicBool>) -> Api<'a> {
        self.cancel = Some(flag);
        self
    }

    /// True when the caller has asked for cancellation.
    fn cancelled(&self) -> bool {
        self.cancel.as_ref().map(|f| f.load(Ordering::Relaxed)).unwrap_or(false)
    }

    /// Attach this client's cancellation flag to a request, if any.
    fn cancellable(&self, req: Request) -> Request {
        match &self.cancel {
            Some(flag) => req.cancellable(flag.clone()),
            None => req,
        }
    }

    /// Emit one stage id to the observer, if any.
    fn note(&self, stage: &str) {
        if let Some(hook) = &self.stage {
            hook(stage);
        }
    }

    fn get(&self, url: &str) -> Result<serde_json::Value> {
        // Retry the *same* URL once: DNS on this side of the network occasionally drops a lookup.
        // Never substitute a different host here - doing so hides the real first failure behind a
        // second, unrelated error. A cancellation is not a network failure and is never retried.
        let request = || {
            let req = Request::get(url).timeout(120).header("Authorization", &self.auth);
            self.cancellable(req)
        };
        if self.cancelled() {
            return Err(Error::net("request cancelled"));
        }
        match self.client.json_checked(request()) {
            Ok(value) => Ok(value),
            Err(first) if matches!(first, Error::Net(_)) => {
                if self.cancelled() {
                    return Err(Error::net("request cancelled"));
                }
                std::thread::sleep(std::time::Duration::from_millis(400));
                if self.cancelled() {
                    return Err(Error::net("request cancelled"));
                }
                match self.client.json_checked(request()) {
                    Ok(value) => Ok(value),
                    Err(second) => Err(Error::Net(format!("{first} / retry: {second}"))),
                }
            }
            Err(other) => Err(other),
        }
    }
    /// Every entitlement of the signed-in account.
    pub fn entitlements(&self, account_id: &str) -> Result<Vec<Entitlement>> {
        self.note(stage::ENTITLEMENTS);
        let mut out = Vec::new();
        let mut start = 0usize;
        loop {
            let url = format!(
                "{ENTITLEMENT_HOST}/entitlement/api/accounts/{account_id}/entitlements?start={start}&count={PAGE}"
            );
            let value = self.get(&url)?;
            let page = value
                .get("response")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if page.is_empty() {
                break;
            }
            for item in &page {
                let active = item.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
                out.push(Entitlement {
                    namespace: str_field(item, "namespace"),
                    catalog_item_id: str_field(item, "catalogItemId"),
                    entitlement_name: str_field(item, "entitlementName"),
                    entitlement_type: str_field(item, "entitlementType"),
                    active,
                });
            }
            if page.len() < PAGE {
                break;
            }
            start += PAGE;
        }
        Ok(out)
    }

    /// Library records (needed to map catalog ids to `appName`).
    ///
    /// This endpoint is **cursor** paginated: the next page token comes back in
    /// `responseMetadata.nextCursor`. Using `start=` (as an earlier version did) silently returns
    /// the same first page over and over.
    pub fn library(&self) -> Result<Vec<LibraryItem>> {
        self.note(stage::LIBRARY);
        let encode = |text: &str| -> String {
            let mut out = String::new();
            for byte in text.bytes() {
                match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        out.push(byte as char)
                    }
                    _ => out.push_str(&format!("%{byte:02X}")),
                }
            }
            out
        };
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..64 {
            let url = match &cursor {
                Some(token) => format!(
                    "{LIBRARY_HOST}/library/api/public/items?includeMetadata=true&count={PAGE}&cursor={}",
                    encode(token)
                ),
                None => format!(
                    "{LIBRARY_HOST}/library/api/public/items?includeMetadata=true&count={PAGE}"
                ),
            };
            let value = self.get(&url)?;
            let page = value
                .get("records")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            if page.is_empty() {
                break;
            }
            for item in &page {
                out.push(LibraryItem {
                    namespace: str_field(item, "namespace"),
                    catalog_item_id: str_field(item, "catalogItemId"),
                    app_name: str_field(item, "appName"),
                    product_id: str_field(item, "productId"),
                });
            }
            let next = value
                .get("responseMetadata")
                .and_then(|m| m.get("nextCursor"))
                .or_else(|| value.get("metadata").and_then(|m| m.get("nextCursor")))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|token| !token.is_empty());
            match next {
                Some(token) => cursor = Some(token),
                None => break,
            }
        }
        Ok(out)
    }

    /// Resolve remaining codename-style titles through the public egdata.app catalogue.
    ///
    /// Epic exposes no namespace->title mapping to third parties, but egdata indexes the same catalog
    /// item ids, so a per-item lookup recovers the store title. Best effort by design: any failure
    /// keeps the codename and never fails the listing, and resolved names are persisted so later runs
    /// need no network at all.
    fn enrich_titles_from_egdata(&self, games: &mut [GameEntry]) {
        let pending: Vec<(usize, String, String)> = games
            .iter()
            .enumerate()
            .filter(|(_, game)| game.title == game.app_name && !game.catalog_item_id.is_empty())
            .map(|(index, game)| (index, game.app_name.clone(), game.catalog_item_id.clone()))
            .take(80)
            .collect();
        if pending.is_empty() {
            return;
        }
        self.note(stage::TITLES);
        // Eight workers: each lookup is one small HTTPS request, and doing them serially dominated
        // `library list` wall time once most titles needed resolving.
        let cursor = AtomicUsize::new(0);
        let found: Mutex<Vec<(usize, String, String)>> = Mutex::new(Vec::new());
        let workers = pending.len().min(8);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let cursor = &cursor;
                let found = &found;
                let pending = &pending;
                let api = self;
                scope.spawn(move || loop {
                    let index = cursor.fetch_add(1, Ordering::AcqRel);
                    let Some((slot, app_name, catalog_item_id)) = pending.get(index) else {
                        break;
                    };
                    let url = format!("{EGDATA_HOST}/items/{catalog_item_id}");
                    let Ok(value) = api.client.get_json(&url, None) else {
                        continue;
                    };
                    let title = value.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    if title.is_empty() || looks_like_guid(title) || title == app_name {
                        continue;
                    }
                    if let Ok(mut guard) = found.lock() {
                        guard.push((*slot, app_name.clone(), title.to_string()));
                    }
                });
            }
        });

        let mut resolved: BTreeMap<String, String> = BTreeMap::new();
        for (slot, app_name, title) in found.into_inner().unwrap_or_default() {
            games[slot].title = title.clone();
            resolved.insert(app_name.to_lowercase(), title);
        }
        if !resolved.is_empty() {
            eprintln!(
                "note: resolved {} title(s) via egdata.app",
                resolved.len()
            );
            save_title_overrides(resolved);
        }
    }

    /// Build the game list from library records, resolving titles through the catalog.
    fn games_from_library(&self, items: &[LibraryItem]) -> Result<Vec<GameEntry>> {
        self.note(stage::MERGE);
        let mut by_namespace: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for item in items {
            if item.app_name.is_empty() {
                continue;
            }
            by_namespace
                .entry(item.namespace.clone())
                .or_default()
                .push(item.catalog_item_id.clone());
        }
        let catalog = self.catalog_groups(&mut by_namespace);

        let mut seen: HashSet<String> = HashSet::new();
        let mut games: Vec<GameEntry> = Vec::new();
        for item in items {
            if item.app_name.is_empty() || !seen.insert(item.app_name.clone()) {
                continue;
            }
            let meta = catalog.get(&(item.namespace.clone(), item.catalog_item_id.clone()));
            games.push(GameEntry {
                app_name: item.app_name.clone(),
                namespace: item.namespace.clone(),
                catalog_item_id: item.catalog_item_id.clone(),
                title: meta
                    .map(|m| m.title.clone())
                    .filter(|t| !t.is_empty())
                    .unwrap_or_else(|| item.app_name.clone()),
                developer: meta.map(|m| m.developer.clone()).unwrap_or_default(),
                folder_name: meta.map(|m| m.folder_name.clone()).unwrap_or_default(),
                entitlement_type: "EXECUTABLE".to_string(),
                ..Default::default()
            });
        }
        let overrides = load_title_overrides();
        for game in games.iter_mut() {
            if let Some(title) = overrides.get(&game.app_name.to_lowercase()) {
                if !title.is_empty() {
                    game.title = title.clone();
                }
            }
        }
        self.enrich_titles_from_egdata(&mut games);
        apply_relations(&mut games, &catalog);
        games.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
        Ok(games)
    }

    /// Catalog details for a batch of ids inside one namespace.
    pub fn catalog(&self, namespace: &str, ids: &[String]) -> Result<BTreeMap<String, CatalogItem>> {
        let mut out = BTreeMap::new();
        let ids: Vec<String> = ids.iter().filter(|id| !id.is_empty()).cloned().collect();
        for batch in ids.chunks(CATALOG_BATCH) {
            let url = catalog_url(namespace, batch, &self.cfg.country, &self.cfg.locale);
            let value = self.get(&url)?;
            parse_catalog(&value, namespace, &mut out);
        }
        Ok(out)
    }

    /// Merge entitlements + library + catalog into installable games.
    pub fn games(&self, account_id: &str) -> Result<Vec<GameEntry>> {
        // The library service is the authoritative source and is reachable on this network, so it is
        // tried first; the entitlement/asset path remains as the fallback.
        if let Ok(items) = self.library() {
            if !items.is_empty() {
                eprintln!("note: using library-service ({} record(s))", items.len());
                return self.games_from_library(&items);
            }
        }

        let entitlements = match self.entitlements(account_id) {
            Ok(list) => list,
            Err(e) => {
                // The entitlement route has changed shape over time (seen: HTTP 404). The launcher
                // asset list gives the same appName/catalogItemId mapping in one call, so use it
                // rather than leaving the user with an empty library.
                eprintln!("warning: entitlement service unavailable ({e}); deriving list from launcher assets");
                return self.games_from_assets();
            }
        };

        // The library service is the canonical source for `appName`, but it is a separate host and
        // has been seen to be unreachable; the launcher asset list carries the same mapping and only
        // needs the token we already hold, so it is a solid fallback.
        let mut app_by_item: BTreeMap<(String, String), String> = BTreeMap::new();
        match self.library() {
            Ok(library) => {
                for item in &library {
                    if !item.app_name.is_empty() {
                        app_by_item.insert(
                            (item.namespace.clone(), item.catalog_item_id.clone()),
                            item.app_name.clone(),
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("warning: library service unavailable ({e}); using launcher asset list");
                for asset in self.entitled_assets()? {
                    if !asset.app_name.is_empty() {
                        app_by_item.insert(
                            (asset.namespace.clone(), asset.catalog_item_id.clone()),
                            asset.app_name.clone(),
                        );
                    }
                }
            }
        }

        // Group the catalog ids per namespace so the bulk endpoint can be batched.
        let mut by_namespace: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for ent in &entitlements {
            if !ent.active || ent.catalog_item_id.is_empty() {
                continue;
            }
            let key = (ent.namespace.clone(), ent.catalog_item_id.clone());
            if !seen.insert(key) {
                continue;
            }
            by_namespace
                .entry(ent.namespace.clone())
                .or_default()
                .push(ent.catalog_item_id.clone());
        }

        let catalog = self.catalog_groups(&mut by_namespace);

        let mut games = Vec::new();
        for ent in &entitlements {
            if !ent.active || ent.catalog_item_id.is_empty() {
                continue;
            }
            let key = (ent.namespace.clone(), ent.catalog_item_id.clone());
            let app_name = app_by_item.get(&key).cloned().unwrap_or_default();
            if app_name.is_empty() {
                // No library record: not directly installable through the launcher API.
                continue;
            }
            let meta = catalog.get(&key).cloned().unwrap_or_default();
            let title = if meta.title.is_empty() { app_name.clone() } else { meta.title };
            games.push(GameEntry {
                app_name,
                namespace: ent.namespace.clone(),
                catalog_item_id: ent.catalog_item_id.clone(),
                title,
                developer: meta.developer,
                folder_name: meta.folder_name,
                entitlement_type: ent.entitlement_type.clone(),
                ..Default::default()
            });
        }
        // Assets without a catalog entry (UE samples, DLC placeholders, unreleased builds) end up
        // with their appName as the title, which is a bare GUID and useless in a library list.
        // Second pass: namespaces whose id lookup produced nothing get a full item sweep.
        let mut unresolved_namespaces: Vec<String> = Vec::new();
        for game in &games {
            if game.title == game.app_name && !unresolved_namespaces.contains(&game.namespace) {
                unresolved_namespaces.push(game.namespace.clone());
            }
        }
        if !unresolved_namespaces.is_empty() {
            eprintln!(
                "note: resolving {} namespace(s) by AppName",
                unresolved_namespaces.len()
            );
            for namespace in &unresolved_namespaces {
                let map = self.titles_by_app_name(namespace);
                if map.is_empty() {
                    continue;
                }
                for game in games.iter_mut().filter(|g| &g.namespace == namespace) {
                    if game.title != game.app_name {
                        continue;
                    }
                    if let Some((title, folder)) = map.get(&game.app_name.to_lowercase()) {
                        game.title = title.clone();
                        if game.folder_name.is_empty() && !folder.is_empty() {
                            game.folder_name = folder.clone();
                        }
                    }
                }
            }
        }
        // Entries without store metadata keep their slot in the list (they are real owned builds),
        // but get a readable placeholder instead of a bare GUID.
        // Resolve codenames before parent matching: `mainGameItem.title` is compared against the
        // final store title, so title resolution has to happen before the unnamed placeholder.
        self.enrich_titles_from_egdata(&mut games);
        let overrides = load_title_overrides();
        if !overrides.is_empty() {
            let mut applied = 0;
            for game in games.iter_mut() {
                if let Some(title) = overrides.get(&game.app_name.to_lowercase()) {
                    if !title.is_empty() {
                        game.title = title.clone();
                        applied += 1;
                    }
                }
            }
            if applied > 0 {
                eprintln!("note: applied {applied} local title override(s) from titles.json");
            }
        }

        let mut unnamed = 0;
        for game in games.iter_mut() {
            if looks_like_guid(&game.title) && game.title == game.app_name {
                unnamed += 1;
                game.title = "（无商店信息）".to_string();
            }
        }
        if unnamed > 0 {
            eprintln!("note: {unnamed} entr(ies) have no store metadata; shown as （无商店信息）");
        }

        apply_relations(&mut games, &catalog);

        games.sort_by(|a, b| {
            let a_unnamed = a.title == "（无商店信息）";
            let b_unnamed = b.title == "（无商店信息）";
            a_unnamed
                .cmp(&b_unnamed)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        games.dedup_by(|a, b| a.app_name == b.app_name);
        self.note(stage::MERGE);
        Ok(games)
    }

    /// Resolve titles by matching `customAttributes.AppName` across a namespace's full item list.
    ///
    /// Some assets (Cyberpunk 2077's `Ginger`, for one) point at a catalog id that the bulk lookup
    /// does not return, so id-based enrichment leaves the codename as the title. The namespace item
    /// list carries `AppName`, which maps back to the real store title.
    fn titles_by_app_name(&self, namespace: &str) -> BTreeMap<String, (String, String)> {
        self.note(stage::TITLES);
        let mut map = BTreeMap::new();
        let country = self.cfg.country.clone();
        let locale = self.cfg.locale.clone();
        let url = format!(
            "{CATALOG_HOST}/catalog/api/shared/namespace/{namespace}/bulk/items?country={country}&locale={locale}&includeMainGameDetails=true"
        );
        let Ok(value) = self.get(&url) else { return map };
        let Some(items) = value.as_object() else { return map };
        for item in items.values() {
            let app = item
                .get("customAttributes")
                .and_then(|c| c.get("AppName"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            if app.is_empty() || title.is_empty() {
                continue;
            }
            let folder = item
                .get("customAttributes")
                .and_then(|c| c.get("FolderName"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            map.insert(app.to_lowercase(), (title.to_string(), folder));
        }
        map
    }

    /// Build the game list purely from the launcher asset list plus catalog metadata.
    fn games_from_assets(&self) -> Result<Vec<GameEntry>> {
        self.note(stage::MERGE);
        let assets = self.entitled_assets()?;
        let mut seen: HashSet<String> = HashSet::new();
        let mut games: Vec<GameEntry> = Vec::new();
        for asset in assets {
            if asset.app_name.is_empty() || !seen.insert(asset.app_name.clone()) {
                continue;
            }
            games.push(GameEntry {
                app_name: asset.app_name.clone(),
                namespace: asset.namespace.clone(),
                catalog_item_id: asset.catalog_item_id.clone(),
                title: asset.app_name.clone(),
                developer: String::new(),
                folder_name: String::new(),
                entitlement_type: "EXECUTABLE".to_string(),
                ..Default::default()
            });
        }

        let mut by_namespace: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for game in &games {
            by_namespace
                .entry(game.namespace.clone())
                .or_default()
                .push(game.catalog_item_id.clone());
        }
        let catalog = self.catalog_groups(&mut by_namespace);
        for game in games.iter_mut() {
            let key = (game.namespace.clone(), game.catalog_item_id.clone());
            if let Some(item) = catalog.get(&key) {
                if !item.title.is_empty() {
                    game.title = item.title.clone();
                }
                if !item.developer.is_empty() {
                    game.developer = item.developer.clone();
                }
                if !item.folder_name.is_empty() {
                    game.folder_name = item.folder_name.clone();
                }
            }
        }
        // Assets without a catalog entry (UE samples, DLC placeholders, unreleased builds) end up
        // with their appName as the title, which is a bare GUID and useless in a library list.
        // Second pass: namespaces whose id lookup produced nothing get a full item sweep.
        let mut unresolved_namespaces: Vec<String> = Vec::new();
        for game in &games {
            if game.title == game.app_name && !unresolved_namespaces.contains(&game.namespace) {
                unresolved_namespaces.push(game.namespace.clone());
            }
        }
        if !unresolved_namespaces.is_empty() {
            eprintln!(
                "note: resolving {} namespace(s) by AppName",
                unresolved_namespaces.len()
            );
            for namespace in &unresolved_namespaces {
                let map = self.titles_by_app_name(namespace);
                if map.is_empty() {
                    continue;
                }
                for game in games.iter_mut().filter(|g| &g.namespace == namespace) {
                    if game.title != game.app_name {
                        continue;
                    }
                    if let Some((title, folder)) = map.get(&game.app_name.to_lowercase()) {
                        game.title = title.clone();
                        if game.folder_name.is_empty() && !folder.is_empty() {
                            game.folder_name = folder.clone();
                        }
                    }
                }
            }
        }
        // Entries without store metadata keep their slot in the list (they are real owned builds),
        // but get a readable placeholder instead of a bare GUID.
        // Resolve codenames before parent matching: `mainGameItem.title` is compared against the
        // final store title, so title resolution has to happen before the unnamed placeholder.
        self.enrich_titles_from_egdata(&mut games);
        let overrides = load_title_overrides();
        if !overrides.is_empty() {
            let mut applied = 0;
            for game in games.iter_mut() {
                if let Some(title) = overrides.get(&game.app_name.to_lowercase()) {
                    if !title.is_empty() {
                        game.title = title.clone();
                        applied += 1;
                    }
                }
            }
            if applied > 0 {
                eprintln!("note: applied {applied} local title override(s) from titles.json");
            }
        }

        let mut unnamed = 0;
        for game in games.iter_mut() {
            if looks_like_guid(&game.title) && game.title == game.app_name {
                unnamed += 1;
                game.title = "（无商店信息）".to_string();
            }
        }
        if unnamed > 0 {
            eprintln!("note: {unnamed} entr(ies) have no store metadata; shown as （无商店信息）");
        }

        apply_relations(&mut games, &catalog);

        games.sort_by(|a, b| {
            let a_unnamed = a.title == "（无商店信息）";
            let b_unnamed = b.title == "（无商店信息）";
            a_unnamed
                .cmp(&b_unnamed)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        Ok(games)
    }
    /// Fetch catalog metadata for many namespaces in parallel.
    ///
    /// A large library spans >100 namespaces and each lookup is a separate round trip, so serial
    /// fetching dominated `library list` wall time (tens of seconds). Eight workers is enough to
    /// hide latency without looking like a burst to Epic.
    fn catalog_groups(
        &self,
        groups: &mut BTreeMap<String, Vec<String>>,
    ) -> BTreeMap<(String, String), CatalogItem> {
        let work: Vec<(String, Vec<String>)> = std::mem::take(groups).into_iter().collect();
        if work.is_empty() {
            return BTreeMap::new();
        }
        self.note(stage::CATALOG);
        let workers = work.len().min(10);
        let cursor = AtomicUsize::new(0);
        let results: Mutex<BTreeMap<(String, String), CatalogItem>> = Mutex::new(BTreeMap::new());
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let cursor = &cursor;
                let results = &results;
                let work = &work;
                let api = self;
                scope.spawn(move || loop {
                    let index = cursor.fetch_add(1, Ordering::AcqRel);
                    let Some((namespace, ids)) = work.get(index) else { break };
                    let mut attempt = api.catalog(namespace, ids);
                    if attempt.is_err() {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        attempt = api.catalog(namespace, ids);
                    }
                    match attempt {
                        Ok(items) => {
                            let mut guard = results.lock().unwrap();
                            for (id, item) in items {
                                guard.insert((namespace.clone(), id), item);
                            }
                        }
                        Err(e) => eprintln!("warning: catalog lookup for {namespace} failed: {e}"),
                    }
                });
            }
        });
        results.into_inner().unwrap_or_default()
    }
    /// Every build the account is entitled to, straight from the launcher service.
    ///
    /// Used as a fallback for `appName` discovery when the library host is unreachable.
    pub fn entitled_assets(&self) -> Result<Vec<EntitledAsset>> {
        self.note(stage::ASSETS);
        let url = format!(
            "{LAUNCHER_HOST}/launcher/api/public/assets/{}?label={}",
            self.cfg.platform, self.cfg.label
        );
        let value = self.get(&url)?;
        let mut out = Vec::new();
        if let Some(items) = value.as_array() {
            for entry in items {
                out.push(EntitledAsset {
                    app_name: str_field(entry, "appName"),
                    catalog_item_id: str_field(entry, "catalogItemId"),
                    namespace: str_field(entry, "namespace"),
                    label_name: str_field(entry, "labelName"),
                    build_version: str_field(entry, "buildVersion"),
                });
            }
        }
        Ok(out)
    }
    /// Resolve the current build (manifest URI + CDN bases) for one app.
    pub fn build(&self, game: &GameEntry) -> Result<BuildInfo> {
        let label = &self.cfg.label;
        let platform = &self.cfg.platform;
        let v2 = format!(
            "{LAUNCHER_HOST}/launcher/api/public/assets/v2/platform/{platform}/namespace/{}/catalogItem/{}/app/{}/label/{label}",
            game.namespace, game.catalog_item_id, game.app_name
        );
        let mut last_error = None;
        for url in [
            v2,
            format!(
                "{LAUNCHER_HOST}/launcher/api/public/assets/{platform}/{}/{}/{}?label={label}",
                game.namespace, game.catalog_item_id, game.app_name
            ),
        ] {
            if self.cancelled() {
                return Err(Error::net("install cancelled"));
            }
            match self.get(&url) {
                Ok(value) => match parse_build(&value, game, label) {
                    Ok(build) => return Ok(build),
                    Err(e) => last_error = Some(e),
                },
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| {
            Error::Api {
                status: 404,
                code: "no_build".into(),
                message: format!("no {label} build for {} on {platform}", game.app_name),
            }
        }))
    }

    /// Download a manifest blob (no authentication required).
    pub fn manifest(&self, uri: &str) -> Result<Vec<u8>> {
        if self.cancelled() {
            return Err(Error::net("install cancelled"));
        }
        let req = self.cancellable(Request::get(uri).timeout(120));
        let (head, body) = self.client.request_vec(&req)?;
        if !head.is_success() {
            return Err(Error::Api {
                status: head.status,
                code: String::new(),
                message: format!("manifest download failed: HTTP {}", head.status),
            });
        }
        Ok(body)
    }
}

/// Build the bulk catalog URL.
///
/// The endpoint takes one `id` query parameter per item. A comma-separated list (`id=a,b`) is
/// silently answered with an empty object, which used to leave most games without folder names or
/// titles; repeating the parameter (`id=a&id=b`) is the form that actually works.
fn catalog_url(namespace: &str, ids: &[String], country: &str, locale: &str) -> String {
    let mut url = format!(
        "{CATALOG_HOST}/catalog/api/shared/namespace/{namespace}/bulk/items?"
    );
    let mut first = true;
    for id in ids.iter().filter(|id| !id.is_empty()) {
        if !first {
            url.push('&');
        }
        first = false;
        url.push_str("id=");
        url.push_str(id);
    }
    if first {
        // No usable ids: ask for the whole namespace's own fields rather than sending an empty id.
        url.push_str("country=");
    } else {
        url.push_str("&country=");
    }
    url.push_str(country);
    url.push_str("&locale=");
    url.push_str(locale);
    url.push_str("&includeMainGameDetails=true");
    url
}

fn str_field(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// `releaseInfo[].appId` values, used to match a catalog item to a library app name.
fn release_app_names(value: &serde_json::Value) -> Vec<String> {
    value
        .get("releaseInfo")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| entry.get("appId").and_then(|v| v.as_str()))
                .map(str::to_string)
                .filter(|name| !name.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn parse_catalog(value: &serde_json::Value, namespace: &str, out: &mut BTreeMap<String, CatalogItem>) {
    let map = value
        .get("items")
        .and_then(|v| v.as_object())
        .or_else(|| value.as_object());
    let Some(map) = map else { return };
    for (key, item) in map {
        // Skip envelope members that are not catalog items.
        if !item.is_object() || key == "errors" || key == "namespace" {
            continue;
        }
        let id = {
            let explicit = str_field(item, "id");
            if explicit.is_empty() { key.clone() } else { explicit }
        };
        let folder_name = item
            .get("customAttributes")
            .and_then(|v| v.get("FolderName"))
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.get("value").and_then(|x| x.as_str()).map(str::to_string))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let categories: Vec<String> = item
            .get("categories")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.get("path").and_then(|p| p.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // `mainGameItem` is Epic's own "this belongs to that main game" marker. `mainGameItemList`
        // deliberately is not used: on bundles it describes what the bundle contains, not a parent.
        let main_game = item.get("mainGameItem").filter(|v| v.is_object());
        out.insert(
            id.clone(),
            CatalogItem {
                id,
                namespace: namespace.to_string(),
                title: str_field(item, "title"),
                developer: str_field(item, "developer"),
                folder_name,
                is_addon: categories.iter().any(|path| path == "addons"),
                categories,
                main_game_id: main_game
                    .map(|main| str_field(main, "id"))
                    .filter(|id| !id.is_empty())
                    .unwrap_or_default(),
                main_game_title: main_game
                    .map(|main| str_field(main, "title"))
                    .unwrap_or_default(),
                main_game_app_names: main_game.map(release_app_names).unwrap_or_default(),
                release_app_names: release_app_names(item),
            },
        );
    }
}

/// Attach catalog add-ons to their main game.
///
/// Runs after titles are final (title-based parent matching needs the store title, not the codename)
/// and before sorting, so the persisted cache already contains the relation.
fn apply_relations(games: &mut [GameEntry], catalog: &BTreeMap<(String, String), CatalogItem>) {
    let overrides = library::load_parent_overrides();
    apply_relations_with(games, catalog, &overrides);
}

fn apply_relations_with(
    games: &mut [GameEntry],
    catalog: &BTreeMap<(String, String), CatalogItem>,
    overrides: &BTreeMap<String, String>,
) {
    if games.is_empty() {
        return;
    }

    struct Hint {
        is_dlc: bool,
        categories: Vec<String>,
        main_id: String,
        main_title: String,
        main_apps: Vec<String>,
        /// `parents.json` value; takes priority over catalog signals.
        override_parent: Option<String>,
    }

    let mut hints: Vec<Hint> = Vec::with_capacity(games.len());
    for game in games.iter() {
        let item = catalog.get(&(game.namespace.clone(), game.catalog_item_id.clone()));
        let mut is_dlc =
            item.map(|item| !item.main_game_id.is_empty() || item.is_addon).unwrap_or(false);
        // A self-referencing mainGameItem would otherwise hide a real game behind itself.
        if let Some(item) = item {
            if item.main_game_id == game.catalog_item_id {
                is_dlc = false;
            }
        }
        let override_parent = overrides
            .get(&game.app_name.to_lowercase())
            .or_else(|| overrides.get(&game.title.to_lowercase()))
            .cloned();
        if override_parent.is_some() {
            is_dlc = true;
        }
        hints.push(Hint {
            is_dlc,
            categories: item.map(|item| item.categories.clone()).unwrap_or_default(),
            main_id: item.map(|item| item.main_game_id.clone()).unwrap_or_default(),
            main_title: item.map(|item| item.main_game_title.clone()).unwrap_or_default(),
            main_apps: item.map(|item| item.main_game_app_names.clone()).unwrap_or_default(),
            override_parent,
        });
    }

    for (game, hint) in games.iter_mut().zip(hints.iter()) {
        game.is_dlc = hint.is_dlc;
        game.categories = hint.categories.clone();
        if !hint.is_dlc {
            game.parent_app_name.clear();
        }
    }

    // Unique-candidate maps over the main games only, so an add-on can never be a parent.
    let mut by_id: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_app: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_title: HashMap<String, Vec<usize>> = HashMap::new();
    let mut by_namespace: HashMap<String, Vec<usize>> = HashMap::new();
    for (index, game) in games.iter().enumerate() {
        if game.is_dlc {
            continue;
        }
        by_id.entry(game.catalog_item_id.clone()).or_default().push(index);
        by_app.entry(game.app_name.to_lowercase()).or_default().push(index);
        by_title
            .entry(library::normalize_title(&game.title))
            .or_default()
            .push(index);
        by_namespace.entry(game.namespace.clone()).or_default().push(index);
    }

    let mut attached = 0usize;
    let mut unresolved = 0usize;
    let mut bad_override = 0usize;

    for (index, hint) in hints.iter().enumerate() {
        if !hint.is_dlc {
            continue;
        }
        let namespace = games[index].namespace.clone();
        let parent = if let Some(value) = &hint.override_parent {
            let found = unique_index(&by_app, &value.to_lowercase()).or_else(|| {
                unique_index(&by_title, &library::normalize_title(value))
            });
            if found.is_none() {
                bad_override += 1;
                eprintln!(
                    "warning: parents.json maps '{}' to '{value}', but no main game matches",
                    games[index].app_name
                );
            }
            found
        } else {
            unique_index(&by_id, &hint.main_id)
                .or_else(|| {
                    hint.main_apps
                        .iter()
                        .find_map(|app| unique_index(&by_app, &app.to_lowercase()))
                })
                .or_else(|| {
                    if hint.main_title.is_empty() {
                        None
                    } else {
                        unique_index(&by_title, &library::normalize_title(&hint.main_title))
                    }
                })
                .or_else(|| {
                    by_namespace
                        .get(&namespace)
                        .filter(|slots| slots.len() == 1)
                        .map(|slots| slots[0])
                })
        };
        match parent {
            Some(parent) if parent != index => {
                games[index].parent_app_name = games[parent].app_name.clone();
                attached += 1;
            }
            _ => {
                games[index].parent_app_name.clear();
                unresolved += 1;
            }
        }
    }

    if attached > 0 {
        eprintln!("note: attached {attached} add-on(s) to their main game");
    }
    if unresolved > 0 {
        eprintln!(
            "warning: {unresolved} add-on(s) have no main game in the library (shown separately with --all)"
        );
    }
    if bad_override > 0 {
        eprintln!("warning: {bad_override} parents.json override(s) did not resolve");
    }
}

fn unique_index(map: &HashMap<String, Vec<usize>>, key: &str) -> Option<usize> {
    map.get(key).filter(|slots| slots.len() == 1).map(|slots| slots[0])
}

fn parse_build(value: &serde_json::Value, game: &GameEntry, label: &str) -> Result<BuildInfo> {
    let manifests = value
        .get("elements")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|first| first.get("manifests"))
        .and_then(|v| v.as_array())
        .or_else(|| value.get("manifests").and_then(|v| v.as_array()))
        .or_else(|| {
            value
                .get("items")
                .and_then(|v| v.as_array())
                .and_then(|arr| arr.first())
                .and_then(|first| first.get("manifests"))
                .and_then(|v| v.as_array())
        })
        .ok_or_else(|| Error::Api {
            status: 200,
            code: "no_manifests".into(),
            message: format!("build response for {} has no manifests", game.app_name),
        })?;

    let mut base_urls: Vec<String> = Vec::new();
    let mut manifest_uri = String::new();
    let mut manifest_hash = String::new();

    for entry in manifests {
        let uri = str_field(entry, "uri");
        if uri.is_empty() {
            continue;
        }
        let full = with_query_params(&uri, entry.get("queryParams"));
        if manifest_uri.is_empty() {
            manifest_uri = full;
            manifest_hash = str_field(entry, "hash");
        }
        let base = crate::cdn::base_from_manifest_uri(&uri);
        if !base_urls.contains(&base) {
            base_urls.push(base);
        }
    }

    if manifest_uri.is_empty() {
        return Err(Error::Api {
            status: 200,
            code: "no_manifest_uri".into(),
            message: format!("build for {} contains no manifest URI", game.app_name),
        });
    }

    Ok(BuildInfo {
        app_name: game.app_name.clone(),
        namespace: game.namespace.clone(),
        catalog_item_id: game.catalog_item_id.clone(),
        label_name: value
            .get("elements")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .map(|e| str_field(e, "labelName"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| label.to_string()),
        build_version: value
            .get("elements")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .map(|e| str_field(e, "buildVersion"))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| game.app_name.clone()),
        manifest_uri,
        manifest_hash,
        base_urls,
    })
}

fn with_query_params(uri: &str, params: Option<&serde_json::Value>) -> String {
    let Some(arr) = params.and_then(|v| v.as_array()) else {
        return uri.to_string();
    };
    let mut pairs = Vec::new();
    for p in arr {
        let name = str_field(p, "name");
        if name.is_empty() {
            continue;
        }
        let value = p.get("value").map(|v| v.to_string().trim_matches('"').to_string()).unwrap_or_default();
        pairs.push(format!("{name}={}", crate::http::urlencode(&value)));
    }
    if pairs.is_empty() {
        uri.to_string()
    } else if uri.contains('?') {
        format!("{uri}&{}", pairs.join("&"))
    } else {
        format!("{uri}?{}", pairs.join("&"))
    }
}

/// Convenience: unix timestamp helper used by callers that cache API results.
pub fn cache_deadline(seconds: u64) -> u64 {
    util::now_unix().saturating_add(seconds)
}

/// Read `titles.json` from the state directory, seeding it on first use.
///
/// A missing or malformed file means "no overrides" rather than an error: a broken optional file
/// must never break the library listing.
fn load_title_overrides() -> BTreeMap<String, String> {
    let path = crate::config::Config::dir().join("titles.json");
    if !path.exists() {
        let seed = "{\n  \"ginger\": \"Cyberpunk 2077\"\n}\n";
        let _ = std::fs::write(&path, seed);
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn save_title_overrides(additions: BTreeMap<String, String>) {
    let path = crate::config::Config::dir().join("titles.json");
    let mut current: BTreeMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let mut changed = false;
    for (key, value) in additions {
        if current.get(&key) != Some(&value) {
            current.insert(key, value);
            changed = true;
        }
    }
    if changed {
        match serde_json::to_string_pretty(&current) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    // A silently dropped cache write is what hid a state-directory bug for a while.
                    eprintln!("warning: could not persist title cache {}: {e}", path.display());
                }
            }
            Err(e) => eprintln!("warning: could not serialise title cache: {e}"),
        }
    }
}

/// True when the text is a bare 32-character hex id, i.e. an appName used as a fallback title.
fn looks_like_guid(text: &str) -> bool {
    text.len() == 32 && text.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn game() -> GameEntry {
        GameEntry {
            app_name: "MyGame".into(),
            namespace: "ns".into(),
            catalog_item_id: "cid".into(),
            title: "My Game".into(),
            developer: "Dev".into(),
            folder_name: "MyGame".into(),
            entitlement_type: "EXECUTABLE".into(),
            ..Default::default()
        }
    }

    #[test]
    fn parses_catalog_envelope() {
        let value = json!({
            "items": {
                "cid": {
                    "id": "cid",
                    "title": "My Game",
                    "developer": "Dev",
                    "customAttributes": { "FolderName": { "value": "MyGame" } },
                    "categories": [ { "path": "games" }, { "path": "applications" } ]
                }
            }
        });
        let mut out = BTreeMap::new();
        parse_catalog(&value, "ns", &mut out);
        let item = out.get("cid").unwrap();
        assert_eq!(item.title, "My Game");
        assert_eq!(item.folder_name, "MyGame");
        assert_eq!(item.categories, vec!["games", "applications"]);
    }

    #[test]
    fn parses_catalog_folder_string_variant() {
        let value = json!({ "items": { "cid": { "title": "T", "customAttributes": { "FolderName": "Plain" } } } });
        let mut out = BTreeMap::new();
        parse_catalog(&value, "ns", &mut out);
        assert_eq!(out.get("cid").unwrap().folder_name, "Plain");
    }

    #[test]
    fn parses_v2_build() {
        let value = json!({
            "elements": [{
                "appName": "MyGame",
                "labelName": "Live-Windows",
                "buildVersion": "++MyGame+1-Windows",
                "manifests": [
                    { "uri": "https://cdn.example/Builds/MyGame/CloudDir/abc.manifest",
                      "hash": "deadbeef",
                      "queryParams": [ { "name": "v", "value": 1 }, { "name": "dt", "value": "x y" } ] },
                    { "uri": "https://cdn2.example/Builds/MyGame/CloudDir/abc.manifest" }
                ]
            }]
        });
        let build = parse_build(&value, &game(), "Live").unwrap();
        assert_eq!(build.build_version, "++MyGame+1-Windows");
        assert_eq!(build.label_name, "Live-Windows");
        assert!(build.manifest_uri.contains("v=1"));
        assert!(build.manifest_uri.contains("dt=x%20y"));
        assert_eq!(build.base_urls.len(), 2);
        assert_eq!(build.base_urls[0], "https://cdn.example/Builds/MyGame/CloudDir");
        assert_eq!(build.manifest_hash, "deadbeef");
    }

    #[test]
    fn rejects_build_without_manifest() {
        let value = json!({ "elements": [ { "manifests": [] } ] });
        assert!(parse_build(&value, &game(), "Live").is_err());
    }

    #[test]
    fn appends_query_params_correctly() {
        assert_eq!(
            with_query_params("https://x/y", Some(&json!([{"name":"a","value":"1"}]))),
            "https://x/y?a=1"
        );
        assert_eq!(with_query_params("https://x/y?b=2", None), "https://x/y?b=2");
    }

    #[test]
    fn parses_catalog_main_game_details() {
        let value = json!({
            "items": {
                "dlc": {
                    "id": "dlc",
                    "title": "Phantom Liberty",
                    "categories": [ { "path": "addons" } ],
                    "mainGameItem": {
                        "id": "base",
                        "title": "Cyberpunk 2077",
                        "releaseInfo": [ { "appId": "Ginger" } ]
                    },
                    "mainGameItemList": [ { "id": "bundle-content" } ]
                },
                "bundle": {
                    "id": "bundle",
                    "title": "Complete Edition",
                    "categories": [ { "path": "games" } ],
                    "mainGameItemList": [ { "id": "included" } ]
                }
            }
        });
        let mut out = BTreeMap::new();
        parse_catalog(&value, "ns", &mut out);
        let dlc = out.get("dlc").unwrap();
        assert!(dlc.is_addon);
        assert_eq!(dlc.main_game_id, "base");
        assert_eq!(dlc.main_game_title, "Cyberpunk 2077");
        assert_eq!(dlc.main_game_app_names, vec!["Ginger"]);
        // A bundle's contents are not its parent.
        let bundle = out.get("bundle").unwrap();
        assert!(!bundle.is_addon);
        assert!(bundle.main_game_id.is_empty());
    }

    #[test]
    fn catalog_url_repeats_id_parameters() {
        let ids = vec!["aaa".to_string(), "bbb".to_string(), String::new()];
        let url = catalog_url("ns", &ids, "US", "en-US");
        assert!(url.contains("id=aaa&id=bbb"), "url was {url}");
        assert!(!url.contains("id=aaa,bbb"), "comma form is silently empty: {url}");
        assert!(url.contains("includeMainGameDetails=true"));
    }

    fn catalog_item(
        id: &str,
        title: &str,
        main_id: &str,
        main_title: &str,
        main_apps: &[&str],
    ) -> CatalogItem {
        CatalogItem {
            id: id.into(),
            namespace: "ns".into(),
            title: title.into(),
            main_game_id: main_id.into(),
            main_game_title: main_title.into(),
            main_game_app_names: main_apps.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    fn entry(app: &str, cid: &str, title: &str) -> GameEntry {
        GameEntry {
            app_name: app.into(),
            namespace: "ns".into(),
            catalog_item_id: cid.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    fn relation_map(items: &[(&CatalogItem, &str)]) -> BTreeMap<(String, String), CatalogItem> {
        items
            .iter()
            .map(|&(item, ns)| ((ns.to_string(), item.id.clone()), item.clone()))
            .collect()
    }

    #[test]
    fn relations_attach_addons_by_id_app_and_title() {
        let mut games = vec![
            entry("Ginger", "base-cp", "Cyberpunk 2077"),
            entry("phantom", "dlc-cp", "Phantom Liberty"),
            entry("dl", "base-dl", "Dying Light"),
            entry("bozak", "dlc-bozak", "Dying Light The Bozak"),
            entry("fortnite", "base-fn", "Fortnite"),
            entry("stw", "dlc-fn", "Fortnite Save the World Content"),
        ];
        let catalog = relation_map(&[
            (&catalog_item("base-cp", "Cyberpunk 2077", "", "", &[]), "ns"),
            (&catalog_item("dlc-cp", "Phantom Liberty", "base-cp", "Cyberpunk 2077", &[]), "ns"),
            (&catalog_item("base-dl", "Dying Light", "", "", &[]), "ns"),
            (
                &catalog_item("dlc-bozak", "Dying Light The Bozak", "unowned", "Dying Light", &[]),
                "ns",
            ),
            (&catalog_item("base-fn", "Fortnite", "", "", &[]), "ns"),
            (
                &catalog_item(
                    "dlc-fn",
                    "Fortnite Save the World Content",
                    "unowned",
                    "",
                    &["Fortnite"],
                ),
                "ns",
            ),
        ]);
        apply_relations_with(&mut games, &catalog, &BTreeMap::new());
        assert!(games[1].is_dlc);
        assert_eq!(games[1].parent_app_name, "Ginger");
        assert_eq!(games[3].parent_app_name, "dl");
        assert_eq!(games[5].parent_app_name, "fortnite");
    }

    #[test]
    fn relations_use_only_base_namespace_and_leave_orphans() {
        let mut games = vec![
            entry("base1", "b1", "Game One"),
            entry("base2", "b2", "Game Two"),
            entry("dlc1", "d1", "Pack One"),
        ];
        let mut catalog = relation_map(&[
            (&catalog_item("b1", "Game One", "", "", &[]), "ns"),
            (&catalog_item("b2", "Game Two", "", "", &[]), "ns"),
            (&catalog_item("d1", "Pack One", "", "", &[]), "ns"),
        ]);
        // The pack has no parent signal, and the namespace has two main games: stay unresolved.
        catalog.get_mut(&("ns".into(), "d1".into())).unwrap().is_addon = true;
        apply_relations_with(&mut games, &catalog, &BTreeMap::new());
        assert!(games[2].is_dlc);
        assert!(games[2].parent_app_name.is_empty());

        // A namespace with exactly one main game is a safe fallback.
        let mut games = vec![entry("base1", "b1", "Game One"), entry("dlc1", "d1", "Pack One")];
        let mut catalog = relation_map(&[
            (&catalog_item("b1", "Game One", "", "", &[]), "ns"),
            (&catalog_item("d1", "Pack One", "", "", &[]), "ns"),
        ]);
        catalog.get_mut(&("ns".into(), "d1".into())).unwrap().is_addon = true;
        apply_relations_with(&mut games, &catalog, &BTreeMap::new());
        assert_eq!(games[1].parent_app_name, "base1");
    }

    #[test]
    fn parents_override_can_group_an_extra() {
        let mut games = vec![entry("ginger", "base", "Cyberpunk 2077"), entry("bonus", "extra", "Bonus Content")];
        let catalog = relation_map(&[
            (&catalog_item("base", "Cyberpunk 2077", "", "", &[]), "ns"),
            (&catalog_item("extra", "Bonus Content", "", "", &[]), "ns"),
        ]);
        let overrides = BTreeMap::from([("bonus".to_string(), "ginger".to_string())]);
        apply_relations_with(&mut games, &catalog, &overrides);
        assert!(games[1].is_dlc);
        assert_eq!(games[1].parent_app_name, "ginger");
    }
}
