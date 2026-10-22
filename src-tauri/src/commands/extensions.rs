use crate::commands::extension_platform::{read_extension_manifest, ExtensionManifest};
use serde::Serialize;
use sidex_extensions::contributions::{parse_contributions, ContributionPoint};
use sidex_extensions::installer::{
    install_from_vsix as crate_install_from_vsix, uninstall as crate_uninstall,
};
use sidex_extensions::manifest::{is_version_greater, parse_manifest, sanitize_ext_id};
use sidex_extensions::marketplace::MarketplaceClient;
use sidex_extensions::paths::user_extensions_dir;
use sidex_extensions::vsix::{install_package, unpack_vsix, validate_vsix};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tauri::AppHandle;
use tokio::sync::Mutex;

/// Shared marketplace client — one HTTP connection pool per process,
/// so searches don't re-do TCP+TLS handshakes on every keystroke.
/// The client also owns the in-process query cache, which was
/// previously wiped every call because a fresh client was constructed.
pub struct MarketplaceClientState {
    inner: Mutex<MarketplaceClient>,
}

impl Default for MarketplaceClientState {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketplaceClientState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MarketplaceClient::new()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct InstalledExtension {
    pub id: String,
    pub name: String,
    pub version: String,
    pub path: String,
}

fn to_installed(
    manifest: &sidex_extensions::manifest::ExtensionManifest,
    path: &Path,
) -> InstalledExtension {
    InstalledExtension {
        id: manifest.canonical_id(),
        name: if manifest.display_name.is_empty() {
            manifest.name.clone()
        } else {
            manifest.display_name.clone()
        },
        version: manifest.version.clone(),
        path: path.to_string_lossy().to_string(),
    }
}

#[tauri::command]
pub async fn install_extension(vsix_path: String) -> Result<InstalledExtension, String> {
    let vsix = Path::new(&vsix_path);
    if !vsix.exists() {
        return Err(format!("VSIX not found: {vsix_path}"));
    }

    let target_dir = user_extensions_dir();
    let installed =
        crate_install_from_vsix(vsix, &target_dir).map_err(|e| format!("install: {e:#}"))?;
    let safe_id = sanitize_ext_id(&installed.canonical_id()).map_err(|e| format!("{e:#}"))?;
    let ext_dir = target_dir.join(&safe_id);

    log::info!("installed extension {safe_id} to {}", ext_dir.display());
    Ok(to_installed(&installed, &ext_dir))
}

#[tauri::command]
pub async fn install_extension_from_url(url: String) -> Result<InstalledExtension, String> {
    log::info!("downloading extension from {url}");
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("download: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download failed: HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| format!("read body: {e}"))?;

    let tmp_path = std::env::temp_dir().join(format!("sidex-{}.vsix", uuid::Uuid::new_v4()));
    fs::write(&tmp_path, &bytes).map_err(|e| format!("write tempfile: {e}"))?;

    let result = (|| -> Result<InstalledExtension, String> {
        let pkg = unpack_vsix(&tmp_path).map_err(|e| format!("unpack vsix: {e:#}"))?;
        let validation = validate_vsix(&pkg);
        if !validation.valid {
            return Err(format!(
                "vsix validation failed: {}",
                validation.errors.join("; ")
            ));
        }
        let target_dir = user_extensions_dir();
        let installed =
            install_package(&pkg, &target_dir).map_err(|e| format!("install: {e:#}"))?;
        log::info!(
            "installed extension {} to {}",
            installed.manifest.canonical_id(),
            installed.install_dir.display()
        );
        Ok(to_installed(&installed.manifest, &installed.install_dir))
    })();

    let _ = fs::remove_file(&tmp_path);
    result
}

#[tauri::command]
pub async fn uninstall_extension(extension_id: String) -> Result<(), String> {
    // Linked extensions live in VS Code / VSCodium's folder: only unlink, never delete there.
    let mut linked = read_linked_ids();
    if let Some(id) = linked
        .iter()
        .find(|l| l.eq_ignore_ascii_case(&extension_id))
        .cloned()
    {
        linked.remove(&id);
        write_linked_ids(&linked)?;
        log::info!("unlinked extension {id}");
        return Ok(());
    }
    let safe_id = sanitize_ext_id(&extension_id).map_err(|e| format!("{e:#}"))?;
    let target_dir = user_extensions_dir();
    let ext_dir = target_dir.join(&safe_id);
    if !ext_dir.exists() {
        return Err(format!("not installed: {extension_id}"));
    }
    crate_uninstall(&safe_id, &target_dir).map_err(|e| format!("remove: {e:#}"))?;
    log::info!("uninstalled {extension_id}");
    Ok(())
}

#[tauri::command]
pub async fn list_installed_extensions(app: AppHandle) -> Result<Vec<InstalledExtension>, String> {
    let dir = user_extensions_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let entries = fs::read_dir(&dir).map_err(|e| format!("readdir: {e}"))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(ExtensionManifest {
            id,
            display_name,
            version,
            path,
            ..
        }) = read_extension_manifest(&app, &path)
        {
            out.push(InstalledExtension {
                id,
                name: display_name,
                version,
                path,
            });
        }
    }
    let linked = read_linked_ids();
    if !linked.is_empty() {
        for (id, c) in scan_import_sources() {
            if linked.contains(&id) && !out.iter().any(|e| e.id == id) {
                out.push(InstalledExtension {
                    id,
                    name: c.name,
                    version: c.version,
                    path: c.path,
                });
            }
        }
    }
    Ok(out)
}

// Import from other editors = link by id. Their dirs are never added to the
// live search path (issue #50): only the ids the user picked (linked.json) are
// loaded in place by `scan_extensions`, and nothing is copied or deleted there.

fn linked_path() -> PathBuf {
    user_extensions_dir().join("linked.json")
}

/// Missing or invalid file -> empty set.
pub(crate) fn read_linked_ids() -> HashSet<String> {
    fs::read_to_string(linked_path())
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

fn write_linked_ids(ids: &HashSet<String>) -> Result<(), String> {
    let mut sorted: Vec<&String> = ids.iter().collect();
    sorted.sort();
    let json = serde_json::to_string_pretty(&sorted).map_err(|e| e.to_string())?;
    fs::write(linked_path(), json).map_err(|e| format!("write linked.json: {e}"))
}

#[derive(Debug, Serialize)]
pub struct ImportCandidate {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub path: String,
}

fn import_sources() -> Vec<(&'static str, PathBuf)> {
    let Some(home) = dirs::home_dir() else {
        return vec![];
    };
    vec![
        ("VS Code", home.join(".vscode").join("extensions")),
        ("VSCodium", home.join(".vscode-oss").join("extensions")),
    ]
}

/// Folder names listed in `<dir>/.obsolete` (uninstalled leftovers).
fn obsolete_folders(dir: &Path) -> HashSet<String> {
    fs::read_to_string(dir.join(".obsolete"))
        .ok()
        .and_then(|s| serde_json::from_str::<HashMap<String, serde_json::Value>>(&s).ok())
        .map(|m| m.into_keys().collect())
        .unwrap_or_default()
}

/// Newest version per canonical id across VS Code and the OSS build (VS Code wins a tie).
/// Shared by the import picker and `scan_extensions`.
pub(crate) fn scan_import_sources() -> HashMap<String, ImportCandidate> {
    let mut best: HashMap<String, ImportCandidate> = HashMap::new();

    for (source, dir) in import_sources() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let obsolete = obsolete_folders(&dir);
        for entry in entries.flatten() {
            let path = entry.path();
            let folder = entry.file_name().to_string_lossy().to_string();
            if !path.is_dir() || obsolete.contains(&folder) {
                continue;
            }
            let Ok(manifest) = parse_manifest(&path.join("package.json")) else {
                continue;
            };
            let id = manifest.canonical_id();
            if sanitize_ext_id(&id).is_err() {
                continue;
            }
            if let Some(cur) = best.get(&id) {
                if !is_version_greater(&manifest.version, &cur.version) {
                    continue;
                }
            }
            let info = to_installed(&manifest, &path);
            best.insert(
                id,
                ImportCandidate {
                    id: info.id,
                    name: info.name,
                    version: info.version,
                    source: source.to_string(),
                    path: info.path,
                },
            );
        }
    }

    best
}

#[tauri::command]
pub async fn vscode_import_candidates() -> Result<Vec<ImportCandidate>, String> {
    let linked = read_linked_ids();
    let installed_dir = user_extensions_dir();
    let mut out: Vec<ImportCandidate> = scan_import_sources()
        .into_values()
        .filter(|c| !linked.contains(&c.id))
        .filter(|c| {
            sanitize_ext_id(&c.id).is_ok_and(|safe| !installed_dir.join(safe).exists())
        })
        .collect();
    out.sort_by_key(|c| c.name.to_lowercase());
    Ok(out)
}

/// Links the picked ids: records them in `linked.json`, copies nothing.
/// Only ids found by `scan_import_sources` are accepted (trust boundary).
#[tauri::command]
pub async fn vscode_import_extensions(ids: Vec<String>) -> Result<Vec<InstalledExtension>, String> {
    let mut available = scan_import_sources();
    let mut linked = read_linked_ids();
    let mut out = Vec::new();
    for id in ids {
        match available.remove(&id) {
            Some(c) => {
                log::info!("linked extension {id} from {}", c.path);
                linked.insert(id);
                out.push(InstalledExtension {
                    id: c.id,
                    name: c.name,
                    version: c.version,
                    path: c.path,
                });
            }
            None => log::warn!("link failed: {id} not found in VS Code/VSCodium folders"),
        }
    }
    if !out.is_empty() {
        write_linked_ids(&linked)?;
    }
    Ok(out)
}

#[derive(Debug, Serialize)]
pub struct MarketplaceResult {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub version: String,
    pub publisher: String,
    pub install_count: u64,
    pub rating: f32,
    pub icon_url: Option<String>,
    pub download_url: String,
}

#[tauri::command]
pub async fn extension_search_marketplace(
    state: tauri::State<'_, Arc<MarketplaceClientState>>,
    query: String,
    page: u32,
) -> Result<Vec<MarketplaceResult>, String> {
    let mut client = state.inner.lock().await;
    let result = client
        .search(&query, page, 20)
        .await
        .map_err(|e| format!("marketplace search: {e}"))?;

    Ok(result
        .results
        .into_iter()
        .map(|ext| {
            let desc = if ext.short_description.is_empty() {
                ext.description.clone()
            } else {
                ext.short_description.clone()
            };
            MarketplaceResult {
                id: ext.id,
                name: ext.name,
                display_name: ext.display_name,
                description: desc,
                version: ext.version,
                publisher: ext.publisher.display_name,
                install_count: ext.install_count,
                rating: ext.rating,
                icon_url: ext.icon_url,
                download_url: ext.download_url,
            }
        })
        .collect())
}

#[derive(Debug, Serialize)]
pub struct ContributionInfo {
    pub kind: String,
    pub count: usize,
    pub details: Vec<String>,
}

fn summarize_point(point: &ContributionPoint) -> ContributionInfo {
    match point {
        ContributionPoint::Commands(v) => ContributionInfo {
            kind: "commands".into(),
            count: v.len(),
            details: v.iter().map(|c| c.title.clone()).collect(),
        },
        ContributionPoint::Languages(v) => ContributionInfo {
            kind: "languages".into(),
            count: v.len(),
            details: v.iter().map(|l| l.id.clone()).collect(),
        },
        ContributionPoint::Themes(v) => ContributionInfo {
            kind: "themes".into(),
            count: v.len(),
            details: v.iter().map(|t| t.label.clone()).collect(),
        },
        ContributionPoint::Grammars(v) => ContributionInfo {
            kind: "grammars".into(),
            count: v.len(),
            details: v.iter().map(|g| g.scope_name.clone()).collect(),
        },
        ContributionPoint::Keybindings(v) => ContributionInfo {
            kind: "keybindings".into(),
            count: v.len(),
            details: v.iter().map(|k| k.command.clone()).collect(),
        },
        ContributionPoint::Snippets(v) => ContributionInfo {
            kind: "snippets".into(),
            count: v.len(),
            details: v.iter().map(|s| s.path.clone()).collect(),
        },
        ContributionPoint::Debuggers(v) => ContributionInfo {
            kind: "debuggers".into(),
            count: v.len(),
            details: v.iter().map(|d| d.label.clone()).collect(),
        },
        ContributionPoint::Views(m) => ContributionInfo {
            kind: "views".into(),
            count: m.values().map(Vec::len).sum(),
            details: m.values().flatten().map(|v| v.id.clone()).collect(),
        },
        ContributionPoint::Configuration(v) => ContributionInfo {
            kind: "configuration".into(),
            count: v.len(),
            details: v.iter().filter_map(|c| c.title.clone()).collect(),
        },
        ContributionPoint::IconThemes(v) => ContributionInfo {
            kind: "iconThemes".into(),
            count: v.len(),
            details: v.iter().map(|t| t.label.clone()).collect(),
        },
        ContributionPoint::ViewsContainers(m) => ContributionInfo {
            kind: "viewsContainers".into(),
            count: m.values().map(Vec::len).sum(),
            details: m.values().flatten().map(|c| c.title.clone()).collect(),
        },
        ContributionPoint::Menus(m) => ContributionInfo {
            kind: "menus".into(),
            count: m.values().map(Vec::len).sum(),
            details: m.keys().cloned().collect(),
        },
        ContributionPoint::TaskDefinitions(v) => ContributionInfo {
            kind: "taskDefinitions".into(),
            count: v.len(),
            details: v.iter().map(|t| t.task_type.clone()).collect(),
        },
        ContributionPoint::ProblemMatchers(v) => ContributionInfo {
            kind: "problemMatchers".into(),
            count: v.len(),
            details: v.iter().map(|p| p.name.clone()).collect(),
        },
        ContributionPoint::Terminal(t) => ContributionInfo {
            kind: "terminal".into(),
            count: t.profiles.len(),
            details: t.profiles.iter().map(|p| p.title.clone()).collect(),
        },
    }
}

#[tauri::command]
pub async fn extension_get_contributions(
    extension_dir: String,
) -> Result<Vec<ContributionInfo>, String> {
    let pkg_path = Path::new(&extension_dir).join("package.json");
    let value: serde_json::Value = crate::commands::encoding::read_json_file(&pkg_path)?;

    let points = parse_contributions(&value);
    Ok(points.iter().map(summarize_point).collect())
}
