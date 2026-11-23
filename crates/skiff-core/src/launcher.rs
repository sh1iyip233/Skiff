//! Registration with the official Epic Games Launcher.
//!
//! The launcher keeps installations in two local stores and reconciles them on every start:
//!
//! * `%ProgramData%\Epic\EpicGamesLauncher\Data\Manifests\<guid>.item` – one JSON description per
//!   installed app. This is what older launchers used and what 20.x still reads at start
//!   (`LoadItemFiles`).
//! * `%ProgramData%\Epic\EpicOnlineServicesShared\InstallHelper\InstalledItems\<guid>.egi` plus
//!   `Revision.json` – the EOS install helper (EOSH) store. Since launcher 20.x this one is
//!   authoritative: at start it logs
//!   `EOSH reconciliation: removing installation not reported by EOSH '<item>'` and deletes every
//!   `.item` the store does not know about. Writing only the `.item` therefore no longer
//!   registers anything; it disappears on the next launcher start.
//!
//! The store keeps a store-wide revision encoded as `"{timestamp:016X}{number:016X}"`, where
//! `timestamp` is the Windows tick count (100 ns since 0001-01-01) the store was created with and
//! `number` is bumped on every change. `Revision.json` holds `{"timestamp":…,"number":…}` and every
//! `.egi` record carries the current revision string; the launcher rewrites all records on each
//! change, and so does this module.
//!
//! Epic does not document any of this. The format below mirrors what the launcher itself wrote for
//! a paused download (state `Incomplete`) and what it accepted for a finished one (state
//! `Installed`), verified against launcher 20.3.10.

use crate::error::{Error, Result};
use crate::util;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// State value EOSH uses for a finished installation.
const STATE_INSTALLED: &str = "Installed";

/// One installed app that should be visible to the launcher.
///
/// `item` is the EGL `.item` JSON written into `.egstore`; every field the launcher store needs
/// (paths, catalog ids, version, launch executable, size) is already in there.
#[derive(Clone, Copy)]
pub struct Entry<'a> {
    pub guid: &'a str,
    pub item: &'a Value,
}

impl Entry<'_> {
    fn text(&self, key: &str) -> &str {
        self.item.get(key).and_then(Value::as_str).unwrap_or_default()
    }

    fn app_name(&self) -> &str {
        self.text("AppName")
    }

    fn install_dir(&self) -> String {
        self.text("InstallLocation").replace('/', "\\")
    }

    fn meta_dir(&self) -> String {
        self.text("ManifestLocation").replace('/', "\\")
    }

    fn manifest_path(&self) -> String {
        let meta = self.meta_dir();
        if meta.is_empty() {
            String::new()
        } else {
            format!("{meta}\\{}.manifest", self.guid)
        }
    }

    fn install_size(&self) -> u64 {
        self.item.get("InstallSize").and_then(Value::as_u64).unwrap_or(0)
    }
}

/// Where the launcher keeps one `.item` per installed app.
pub fn manifests_dir() -> PathBuf {
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
    Path::new(&program_data)
        .join("Epic")
        .join("EpicGamesLauncher")
        .join("Data")
        .join("Manifests")
}

/// Where the EOS install helper keeps its installation records.
pub fn installed_items_dir() -> PathBuf {
    let program_data = std::env::var("ProgramData").unwrap_or_else(|_| "C:\\ProgramData".into());
    Path::new(&program_data)
        .join("Epic")
        .join("EpicOnlineServicesShared")
        .join("InstallHelper")
        .join("InstalledItems")
}

/// True when an Epic Games Launcher process is running.
///
/// The launcher reads both stores only when it starts, so an install registered while it is
/// running stays invisible until it is fully exited. Closing the window is not enough: the
/// launcher keeps running in the notification area.
pub fn launcher_running() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let output = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq EpicGamesLauncher.exe", "/NH", "/FO", "CSV"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match output {
            Ok(output) => String::from_utf8_lossy(&output.stdout)
                .to_ascii_lowercase()
                .contains("epicgameslauncher.exe"),
            // Without a reliable answer stay quiet rather than nag about a restart.
            Err(_) => false,
        }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Write both stores for `entries`, replacing earlier registrations of the same apps.
pub fn register(entries: &[Entry<'_>]) -> Result<()> {
    if entries.is_empty() {
        return Err(Error::usage("nothing to register"));
    }
    register_in(&manifests_dir(), &installed_items_dir(), entries)
}

/// Remove every registration of `app_names` (lowercase) from both stores.
pub fn unregister(app_names: &HashSet<String>) -> Result<()> {
    unregister_in(&manifests_dir(), &installed_items_dir(), app_names)
}

fn register_in(manifests: &Path, store_dir: &Path, entries: &[Entry<'_>]) -> Result<()> {
    let app_names: HashSet<String> =
        entries.iter().map(|entry| entry.app_name().to_lowercase()).collect();

    // A reinstall generates new GUIDs, so drop whatever an earlier run left for these apps. Without
    // this the launcher sees two installations of the same app and prunes one of them itself.
    remove_items(manifests, &app_names);
    remove_records(store_dir, &app_names);

    let mut store = Store::open(store_dir);
    let revision = store.next_revision();
    store.rewrite_records(&revision);

    for entry in entries {
        // Keep the revision in the `.item` in sync with the record, which is what the launcher
        // writes itself when it adopts an install.
        let mut item = entry.item.clone();
        if let Some(object) = item.as_object_mut() {
            object.insert("EoshRevision".to_string(), Value::String(revision.clone()));
        }
        let bytes = serde_json::to_vec_pretty(&item)
            .map_err(|e| Error::integrity(format!("cannot serialize launcher manifest: {e}")))?;
        util::atomic_write(&manifests.join(format!("{}.item", entry.guid)), &bytes)?;
        store.write_record(entry, &revision)?;
    }

    // Last, so a crash mid-registration leaves the previous revision in place.
    store.save_revision()
}

fn unregister_in(manifests: &Path, store_dir: &Path, app_names: &HashSet<String>) -> Result<()> {
    remove_items(manifests, app_names);
    let removed = remove_records(store_dir, app_names);
    if removed {
        // The store revision has to move when records disappear, otherwise the launcher keeps
        // reporting the previous revision and may re-add what was just removed.
        let mut store = Store::open(store_dir);
        let revision = store.next_revision();
        store.rewrite_records(&revision);
        store.save_revision()?;
    }
    Ok(())
}

fn remove_items(manifests: &Path, app_names: &HashSet<String>) {
    let Ok(read) = std::fs::read_dir(manifests) else { return };
    for entry in read.flatten() {
        let path = entry.path();
        if !path.extension().map(|ext| ext == "item").unwrap_or(false) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        let app = value.get("AppName").and_then(Value::as_str).unwrap_or_default().to_lowercase();
        if app_names.contains(&app) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Returns whether at least one record was removed.
fn remove_records(store_dir: &Path, app_names: &HashSet<String>) -> bool {
    let Ok(read) = std::fs::read_dir(store_dir) else { return false };
    let mut removed = false;
    for entry in read.flatten() {
        let path = entry.path();
        if !path.extension().map(|ext| ext == "egi").unwrap_or(false) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
        let app = value
            .get("v4")
            .and_then(|v4| v4.get("artifactId"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_lowercase();
        if app_names.contains(&app) {
            if std::fs::remove_file(&path).is_ok() {
                removed = true;
            }
        }
    }
    removed
}

/// The EOSH installation store: records plus the store-wide revision.
struct Store {
    dir: PathBuf,
    timestamp: u64,
    number: u64,
}

impl Store {
    fn open(dir: &Path) -> Store {
        let parsed = std::fs::read_to_string(dir.join("Revision.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        let timestamp = parsed
            .as_ref()
            .and_then(|value| value.get("timestamp"))
            .and_then(Value::as_u64)
            .unwrap_or_else(now_ticks);
        let number = parsed
            .as_ref()
            .and_then(|value| value.get("number"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        Store { dir: dir.to_path_buf(), timestamp, number }
    }

    /// Advance the store revision and return its new string form.
    fn next_revision(&mut self) -> String {
        self.number = self.number.saturating_add(1);
        revision_string(self.timestamp, self.number)
    }

    fn save_revision(&self) -> Result<()> {
        let bytes = serde_json::to_vec(&json!({
            "timestamp": self.timestamp,
            "number": self.number,
        }))
        .map_err(|e| Error::integrity(format!("cannot serialize EOSH revision: {e}")))?;
        util::atomic_write(&self.dir.join("Revision.json"), &bytes)
    }

    /// Stamp the new revision on every record already in the store, as the launcher does.
    fn rewrite_records(&self, revision: &str) {
        let Ok(read) = std::fs::read_dir(&self.dir) else { return };
        for entry in read.flatten() {
            let path = entry.path();
            if !path.extension().map(|ext| ext == "egi").unwrap_or(false) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let Ok(mut value) = serde_json::from_str::<Value>(&text) else { continue };
            let Some(v4) = value.get_mut("v4").and_then(|v4| v4.as_object_mut()) else { continue };
            v4.insert("revision".to_string(), Value::String(revision.to_string()));
            if let Ok(bytes) = serde_json::to_vec(&value) {
                let _ = util::atomic_write(&path, &bytes);
            }
        }
    }

    fn write_record(&self, entry: &Entry<'_>, revision: &str) -> Result<()> {
        let bytes = serde_json::to_vec(&record(entry, revision))
            .map_err(|e| Error::integrity(format!("cannot serialize EOSH record: {e}")))?;
        util::atomic_write(&self.dir.join(format!("{}.egi", entry.guid)), &bytes)
    }
}

/// Build one EOSH installation record for a finished install.
fn record(entry: &Entry<'_>, revision: &str) -> Value {
    json!({
        "v4": {
            "installationId": entry.guid,
            "state": STATE_INSTALLED,
            "revision": revision,
            "dir": entry.install_dir(),
            "metaDir": entry.meta_dir(),
            "manifestPath": entry.manifest_path(),
            "pendingManifestPath": "",
            "platform": "Windows",
            "sandboxId": entry.text("CatalogNamespace"),
            "itemId": entry.text("CatalogItemId"),
            "artifactId": entry.app_name(),
            "tags": [],
            "pendingTags": [],
            "stagedVersion": "",
            "manifestData": {
                "version": entry.text("AppVersionString"),
                "launchCommand": entry.text("LaunchCommand"),
                "launchExe": entry.text("LaunchExecutable"),
                "prereqSHA1Hash": entry.text("PrereqSHA1Hash"),
                "buildSize": entry.install_size(),
            },
            "pendingManifestData": {
                "version": "",
                "launchCommand": "",
                "launchExe": "",
                "prereqSHA1Hash": "",
                "buildSize": 0,
            },
            "manifestUris": [],
        }
    })
}

/// `"{timestamp:016X}{number:016X}"`, the revision encoding used by the EOSH store.
fn revision_string(timestamp: u64, number: u64) -> String {
    format!("{timestamp:016X}{number:016X}")
}

/// Windows tick count (100 ns since 0001-01-01), which is what `Revision.json` stores.
fn now_ticks() -> u64 {
    const UNIX_EPOCH_TICKS: u64 = 621_355_968_000_000_000;
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => {
            UNIX_EPOCH_TICKS + elapsed.as_secs() * 10_000_000 + u64::from(elapsed.subsec_nanos()) / 100
        }
        Err(_) => UNIX_EPOCH_TICKS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item() -> Value {
        json!({
            "AppName": "Ginger",
            "CatalogNamespace": "77f2b98e2cef40c8a7437518bf420e47",
            "CatalogItemId": "5beededaad9743df90e8f07d92df153f",
            "InstallLocation": "D:\\Games\\Ginger",
            "ManifestLocation": "D:\\Games\\Ginger\\.egstore",
            "AppVersionString": "Build_5813403Change_13555055_EP1_RedLauncher",
            "LaunchExecutable": "redprelauncher.exe",
            "LaunchCommand": "",
            "InstallSize": 119_115_441_766u64,
        })
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "skiff-launcher-{tag}-{}-{}",
            std::process::id(),
            fastrand::u64(..)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn revision_uses_store_timestamp_and_counter() {
        let mut store = Store {
            dir: PathBuf::new(),
            timestamp: 639_309_297_592_508_060,
            number: 8,
        };
        assert_eq!(store.next_revision(), "08DF486B409B1E9C0000000000000009");
        assert_eq!(store.number, 9);
    }

    #[test]
    fn record_describes_an_installed_app() {
        let item = sample_item();
        let entry = Entry { guid: "74456836EA7197E51883E01BBDED8390", item: &item };
        let value = record(&entry, "08DF486B409B1E9C0000000000000009");
        let v4 = &value["v4"];
        assert_eq!(v4["state"], "Installed");
        assert_eq!(v4["installationId"], "74456836EA7197E51883E01BBDED8390");
        assert_eq!(v4["dir"], "D:\\Games\\Ginger");
        assert_eq!(v4["metaDir"], "D:\\Games\\Ginger\\.egstore");
        assert_eq!(
            v4["manifestPath"],
            "D:\\Games\\Ginger\\.egstore\\74456836EA7197E51883E01BBDED8390.manifest"
        );
        assert_eq!(v4["sandboxId"], "77f2b98e2cef40c8a7437518bf420e47");
        assert_eq!(v4["itemId"], "5beededaad9743df90e8f07d92df153f");
        assert_eq!(v4["artifactId"], "Ginger");
        assert_eq!(v4["manifestData"]["version"], "Build_5813403Change_13555055_EP1_RedLauncher");
        assert_eq!(v4["manifestData"]["launchExe"], "redprelauncher.exe");
        assert_eq!(v4["manifestData"]["buildSize"], 119_115_441_766u64);
        assert_eq!(v4["revision"], "08DF486B409B1E9C0000000000000009");
    }

    #[test]
    fn register_writes_both_stores_and_bumps_the_revision() {
        let root = temp_dir("register");
        let manifests = root.join("Manifests");
        let store = root.join("InstalledItems");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::create_dir_all(&store).unwrap();

        let item = sample_item();
        let entry = Entry { guid: "74456836EA7197E51883E01BBDED8390", item: &item };
        register_in(&manifests, &store, &[entry]).unwrap();

        let item_path = manifests.join("74456836EA7197E51883E01BBDED8390.item");
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&item_path).unwrap()).unwrap();
        let revision = written["EoshRevision"].as_str().unwrap().to_string();
        assert_eq!(revision.len(), 32);

        let egi: Value = serde_json::from_str(
            &std::fs::read_to_string(store.join("74456836EA7197E51883E01BBDED8390.egi")).unwrap(),
        )
        .unwrap();
        assert_eq!(egi["v4"]["state"], "Installed");
        assert_eq!(egi["v4"]["revision"], revision);

        let revision_file: Value =
            serde_json::from_str(&std::fs::read_to_string(store.join("Revision.json")).unwrap())
                .unwrap();
        assert_eq!(revision_file["number"], 1);
        let timestamp = revision_file["timestamp"].as_u64().unwrap();
        assert_eq!(revision, format!("{timestamp:016X}{:016X}", 1u64));

        // Registering again must not multiply records and must advance the revision.
        register_in(&manifests, &store, &[entry]).unwrap();
        let revision_file: Value =
            serde_json::from_str(&std::fs::read_to_string(store.join("Revision.json")).unwrap())
                .unwrap();
        assert_eq!(revision_file["number"], 2);
        let egi: Value = serde_json::from_str(
            &std::fs::read_to_string(store.join("74456836EA7197E51883E01BBDED8390.egi")).unwrap(),
        )
        .unwrap();
        assert_eq!(egi["v4"]["revision"], format!("{timestamp:016X}{:016X}", 2u64));
        assert_eq!(std::fs::read_dir(&store).unwrap().count(), 2);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unregister_removes_both_stores() {
        let root = temp_dir("unregister");
        let manifests = root.join("Manifests");
        let store = root.join("InstalledItems");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::create_dir_all(&store).unwrap();

        let item = sample_item();
        register_in(&manifests, &store, &[Entry { guid: "ABC", item: &item }]).unwrap();
        unregister_in(&manifests, &store, &HashSet::from(["ginger".to_string()])).unwrap();

        assert!(!manifests.join("ABC.item").exists());
        assert!(!store.join("ABC.egi").exists());
        let revision_file: Value =
            serde_json::from_str(&std::fs::read_to_string(store.join("Revision.json")).unwrap())
                .unwrap();
        assert_eq!(revision_file["number"], 2);

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn keeps_records_of_other_apps() {
        let root = temp_dir("others");
        let manifests = root.join("Manifests");
        let store = root.join("InstalledItems");
        std::fs::create_dir_all(&manifests).unwrap();
        std::fs::create_dir_all(&store).unwrap();

        let other = json!({
            "v4": {"installationId": "OTHER", "artifactId": "OtherGame", "revision": "0"}
        });
        util::atomic_write(&store.join("OTHER.egi"), other.to_string().as_bytes()).unwrap();

        let item = sample_item();
        register_in(&manifests, &store, &[Entry { guid: "ABC", item: &item }]).unwrap();

        let other: Value =
            serde_json::from_str(&std::fs::read_to_string(store.join("OTHER.egi")).unwrap()).unwrap();
        assert_eq!(other["v4"]["artifactId"], "OtherGame");
        // Its revision is moved forward with the store, as the launcher does on every change.
        assert_ne!(other["v4"]["revision"], "0");

        std::fs::remove_dir_all(&root).ok();
    }
}
