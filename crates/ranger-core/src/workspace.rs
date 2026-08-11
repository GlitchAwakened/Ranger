//! Serialization / deserialization of the workspace state.
//!
//! The workspace captures the **session layout**: the panel tree
//! (with their width ratios), the tabs open in each one
//! (current path + sort order), the active tab and panel, as well as
//! the collapsed state of the left sidebar sections. Their order is
//! a global preference kept in `config.toml`.
//!
//! Stored as TOML in `$XDG_DATA_HOME/ranger/workspace.toml` (see
//! `paths::workspace_path`). The window size, meanwhile, stays in
//! `config.toml` (a cosmetic preference persisted by `persist_window_size`).
//!
//! The **selection** and **history** (back/forward) are NOT persisted:
//! they are volatile session states, with no restoration value.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::fs::{GroupMode, SortColumn, SortOrder};
use crate::layout::LayoutNode;

/// Maximum number of closed tabs kept per workspace, oldest to most
/// recent. The last element is the one "Reopen" will restore.
pub const MAX_CLOSED_TABS: usize = 20;

/// Serializable state of a tab: current path + sort + display mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabState {
    pub path: String,
    #[serde(default = "default_sort_column")]
    pub sort_column: SortColumn,
    #[serde(default = "default_sort_order")]
    pub sort_order: SortOrder,
    /// Preview mode (thumbnails) active for this tab. When the field is
    /// absent, `#[serde(default)]` selects list mode.
    #[serde(default)]
    pub preview: bool,
    /// Display of hidden files for this tab. When the field is
    /// absent, `#[serde(default)]` hides hidden files.
    #[serde(default)]
    pub show_hidden: bool,
    /// Grouping by type for this tab. When the field is absent, the
    /// default value is "folders first".
    #[serde(default = "default_group_mode")]
    pub group_mode: GroupMode,
}

fn default_group_mode() -> GroupMode {
    GroupMode::FoldersFirst
}

fn default_sort_column() -> SortColumn {
    SortColumn::Name
}
fn default_sort_order() -> SortOrder {
    SortOrder::Asc
}

/// Serializable state of a panel: width ratio, active tab, tabs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PanelState {
    /// Width ratio relative to the other panels (see `panel_stretches`).
    #[serde(default = "default_stretch")]
    pub stretch: f32,
    #[serde(default)]
    pub active_tab: usize,
    pub tabs: Vec<TabState>,
    /// Columns of the view (order + visibility + width). When the field is
    /// absent, the full set is restored.
    #[serde(default = "crate::columns::default_columns")]
    pub columns: Vec<crate::columns::ColumnSpec>,
    /// Position of the view's tab bar: 0 = horizontal at the top,
    /// 1 = vertical on the left, 2 = vertical on the right. The default value is 0.
    #[serde(default)]
    pub tab_bar_mode: u8,
    /// USER width of the vertical tab bar (logical px), set by the
    /// resize handle. `0` = automatic (clamped 40% formula). Persisted per
    /// view.
    #[serde(default)]
    pub vbar_width: f32,
}

fn default_stretch() -> f32 {
    1.0
}

/// State of the four collapsible sections of the left sidebar. All
/// fields default to `false` so that an old workspace without this table
/// keeps its sections expanded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidebarSectionsState {
    #[serde(default)]
    pub shortcuts_collapsed: bool,
    #[serde(default)]
    pub favorites_collapsed: bool,
    #[serde(default)]
    pub drives_collapsed: bool,
    #[serde(default)]
    pub network_collapsed: bool,
}

impl SidebarSectionsState {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Complete state of the workspace.
///
/// `panels` is the **flat** list of panel contents (each with its
/// tabs); `layout` describes their **arrangement** as a tree of splits
/// (leaves reference an index in `panels`). If `layout` is absent or
/// invalid, it is rebuilt as a horizontal chain covering all
/// panels (see [`Self::layout_tree`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceState {
    #[serde(default)]
    pub active_panel: usize,
    pub panels: Vec<PanelState>,
    /// Layout tree. `None` triggers its reconstruction on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<LayoutNode>,
    /// Name of the named workspace this state comes from. Shown in the
    /// window title ("{workspace} — Ranger"). `None` = ad hoc state,
    /// never attached to a saved workspace → title "Ranger".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// History of tabs actually closed by the user. When the field is
    /// absent from the TOML, the list is empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed_tabs: Vec<TabState>,
    /// Collapse state of the sidebar sections. The table is omitted when
    /// everything is expanded; `default` keeps old files perfectly readable.
    #[serde(default, skip_serializing_if = "SidebarSectionsState::is_default")]
    pub sidebar_sections: SidebarSectionsState,
}

impl WorkspaceState {
    /// Loads from `path`. Returns `Ok(None)` if the file does not exist
    /// (first launch) — the caller will then use a default workspace.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let ws: WorkspaceState = toml::from_str(&content).map_err(|e| {
                    crate::error::Error::Workspace(format!("parse {}: {e}", path.display()))
                })?;
                // Guard: a workspace without a panel is invalid.
                if ws.panels.is_empty() {
                    return Ok(None);
                }
                Ok(Some(ws.sanitized()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Writes the workspace to disk (creates parent folders as needed).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| {
            crate::error::Error::Workspace(format!("serialize {}: {e}", path.display()))
        })?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Returns the validated layout tree: the stored tree if it covers
    /// exactly `0..panels.len()`, otherwise a horizontal chain rebuilt
    /// from the available `stretch` values.
    pub fn layout_tree(&self) -> LayoutNode {
        if let Some(node) = &self.layout
            && node.is_valid_for(self.panels.len())
        {
            return node.clone();
        }
        let stretches: Vec<f32> = self.panels.iter().map(|p| p.stretch).collect();
        LayoutNode::row_chain(&stretches)
    }

    /// Normalizes out-of-bounds indices (robustness against a hand-edited
    /// or corrupted file): clamps `active_panel` and each `active_tab`,
    /// removes empty panels, and guarantees a `layout` consistent with the
    /// remaining panels.
    pub fn sanitized(mut self) -> Self {
        if self.closed_tabs.len() > MAX_CLOSED_TABS {
            self.closed_tabs
                .drain(..self.closed_tabs.len() - MAX_CLOSED_TABS);
        }
        self.panels.retain(|p| !p.tabs.is_empty());
        if self.panels.is_empty() {
            // Rebuild a minimal panel — the caller will replace the empty
            // path with $HOME via `effective_*`.
            self.panels.push(PanelState {
                stretch: 1.0,
                active_tab: 0,
                tabs: vec![TabState {
                    path: String::new(),
                    sort_column: SortColumn::Name,
                    sort_order: SortOrder::Asc,
                    preview: false,
                    show_hidden: false,
                    group_mode: GroupMode::FoldersFirst,
                }],
                columns: crate::columns::default_columns(),
                tab_bar_mode: 0,
                vbar_width: 0.0,
            });
        }
        // Unknown tab bar mode / invalid width (hand-edited file) → defaults.
        for p in &mut self.panels {
            if p.tab_bar_mode > 2 {
                p.tab_bar_mode = 0;
            }
            if !p.vbar_width.is_finite() || p.vbar_width < 0.0 {
                p.vbar_width = 0.0;
            }
        }
        for p in &mut self.panels {
            if p.active_tab >= p.tabs.len() {
                p.active_tab = p.tabs.len() - 1;
            }
            if !(p.stretch.is_finite() && p.stretch > 0.0) {
                p.stretch = 1.0;
            }
            // Normalize the columns (anchor `name`, widths, completion).
            p.columns = crate::columns::sanitize(std::mem::take(&mut p.columns));
        }
        if self.active_panel >= self.panels.len() {
            self.active_panel = self.panels.len() - 1;
        }
        // Guarantees a valid tree for the remaining panels (the removals
        // above may have reindexed the panels).
        let tree = self.layout_tree();
        self.layout = Some(tree);
        self
    }
}

// Named workspaces ----------
//
// Each named workspace is a `<id>.toml` file in `paths::workspaces_dir()`.
// `id` is a stable generated identifier (timestamp), distinct from the
// displayed `name` (freely editable by the user). The file contains the
// `name` + the complete state (`WorkspaceState`).

use std::path::PathBuf;

/// Serialized content of a named workspace.
///
/// `saved_at` = nanoseconds since the UNIX epoch at the last save
/// (creation **or** update), used to sort the list by recency. When the
/// field is absent, `#[serde(default)]` produces 0 and the sort then falls
/// back to the file's modification date.
/// TOML note: scalar fields (`name`, `saved_at`) MUST precede the
/// `state` table, otherwise they would get swallowed into `[state]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct NamedWorkspaceFile {
    name: String,
    #[serde(default)]
    saved_at: u64,
    state: WorkspaceState,
}

/// Lightweight metadata of a named workspace (for list display).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedWorkspaceMeta {
    /// Stable identifier = file name without extension.
    pub id: String,
    /// Displayed name, editable by the user.
    pub name: String,
    /// Number of panels (for the metadata shown in the list).
    pub panels: usize,
    /// Total number of tabs (sum over all panels).
    pub tabs: usize,
    /// Date of the last save (epoch nanos) — sort key for recency.
    pub saved_at: u64,
}

/// Validates that an `id` is safe as a file name (anti path-traversal).
fn id_is_safe(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn workspace_file_path(dir: &Path, id: &str) -> Option<PathBuf> {
    if id_is_safe(id) {
        Some(dir.join(format!("{id}.toml")))
    } else {
        None
    }
}

/// Lists the named workspaces present in `dir`, sorted by **recency**
/// (last saved first). Ties are broken by name (case-insensitive).
/// Unreadable/corrupted files are ignored.
/// The caller (GUI) can reverse the order to offer "oldest first".
pub fn list_named_workspaces(dir: &Path) -> Vec<NamedWorkspaceMeta> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return Vec::new(), // missing folder = no workspace
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !id_is_safe(id) {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path)
            && let Ok(file) = toml::from_str::<NamedWorkspaceFile>(&content)
        {
            let panels = file.state.panels.len();
            let tabs = file.state.panels.iter().map(|p| p.tabs.len()).sum();
            // Recency: `saved_at`, falling back to the file's
            // modification date when the field is 0.
            let saved_at = if file.saved_at != 0 {
                file.saved_at
            } else {
                entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0)
            };
            out.push(NamedWorkspaceMeta {
                id: id.to_string(),
                name: file.name,
                panels,
                tabs,
                saved_at,
            });
        }
    }
    // Most recent first; ties → name (case-insensitive).
    out.sort_by(|a, b| {
        b.saved_at
            .cmp(&a.saved_at)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

/// Finds a named workspace by its user-facing label. The comparison is
/// the same as in the UI: surrounding whitespace ignored and comparison
/// in Unicode lowercase. The session file `workspace.toml`, located
/// elsewhere, never enters this search.
pub fn find_named_workspace(dir: &Path, name: &str) -> Option<NamedWorkspaceMeta> {
    let wanted = name.trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    list_named_workspaces(dir)
        .into_iter()
        .find(|meta| meta.name.trim().to_lowercase() == wanted)
}

/// Nanoseconds (u64) since the UNIX epoch. Precision sufficient to
/// distinguish two close saves and sort by recency.
fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Generates a stable identifier based on the clock (nanos since the epoch).
fn generate_id() -> String {
    format!("ws-{}", now_nanos())
}

/// Creates a new named workspace from `state`. Returns its `id`.
pub fn save_named_workspace(dir: &Path, name: &str, state: &WorkspaceState) -> Result<String> {
    std::fs::create_dir_all(dir)?;
    let id = generate_id();
    let file = NamedWorkspaceFile {
        name: name.trim().to_string(),
        saved_at: now_nanos(),
        state: state.clone(),
    };
    write_named(dir, &id, &file)?;
    Ok(id)
}

/// Overwrites the state of an existing named workspace (keeps its `name`).
pub fn overwrite_named_workspace(dir: &Path, id: &str, state: &WorkspaceState) -> Result<()> {
    let mut file = read_named(dir, id)?;
    file.state = state.clone();
    file.saved_at = now_nanos(); // update → moves to the top of the list
    write_named(dir, id, &file)
}

/// Loads a named workspace. Returns `(name, sanitized state)`.
pub fn load_named_workspace(dir: &Path, id: &str) -> Result<(String, WorkspaceState)> {
    let file = read_named(dir, id)?;
    Ok((file.name, file.state.sanitized()))
}

/// Renames a workspace (only changes the `name` field, keeps the `id`/file).
pub fn rename_named_workspace(dir: &Path, id: &str, new_name: &str) -> Result<()> {
    let mut file = read_named(dir, id)?;
    file.name = new_name.trim().to_string();
    write_named(dir, id, &file)
}

/// Deletes a named workspace's file.
pub fn delete_named_workspace(dir: &Path, id: &str) -> Result<()> {
    let path = workspace_file_path(dir, id)
        .ok_or_else(|| crate::error::Error::Workspace(format!("invalid workspace id: {id:?}")))?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn read_named(dir: &Path, id: &str) -> Result<NamedWorkspaceFile> {
    let path = workspace_file_path(dir, id)
        .ok_or_else(|| crate::error::Error::Workspace(format!("invalid workspace id: {id:?}")))?;
    let content = std::fs::read_to_string(&path)?;
    toml::from_str(&content)
        .map_err(|e| crate::error::Error::Workspace(format!("parse {}: {e}", path.display())))
}

fn write_named(dir: &Path, id: &str, file: &NamedWorkspaceFile) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = workspace_file_path(dir, id)
        .ok_or_else(|| crate::error::Error::Workspace(format!("invalid workspace id: {id:?}")))?;
    let text = toml::to_string_pretty(file)
        .map_err(|e| crate::error::Error::Workspace(format!("serialize: {e}")))?;
    std::fs::write(&path, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WorkspaceState {
        WorkspaceState {
            active_panel: 1,
            panels: vec![
                PanelState {
                    stretch: 1.5,
                    active_tab: 0,
                    tabs: vec![TabState {
                        path: "/home/u/Documents".into(),
                        sort_column: SortColumn::Size,
                        sort_order: SortOrder::Desc,
                        preview: false,
                        show_hidden: false,
                        group_mode: GroupMode::FoldersFirst,
                    }],
                    columns: crate::columns::default_columns(),
                    tab_bar_mode: 0,
                    vbar_width: 0.0,
                },
                PanelState {
                    stretch: 1.0,
                    active_tab: 1,
                    tabs: vec![
                        TabState {
                            path: "/tmp".into(),
                            sort_column: SortColumn::Name,
                            sort_order: SortOrder::Asc,
                            preview: true,
                            show_hidden: false,
                            group_mode: GroupMode::FoldersFirst,
                        },
                        TabState {
                            path: "/etc".into(),
                            sort_column: SortColumn::Modified,
                            sort_order: SortOrder::Desc,
                            preview: false,
                            show_hidden: false,
                            group_mode: GroupMode::FoldersFirst,
                        },
                    ],
                    columns: crate::columns::default_columns(),
                    tab_bar_mode: 0,
                    vbar_width: 0.0,
                },
            ],
            layout: None,
            workspace_name: None,
            closed_tabs: Vec::new(),
            sidebar_sections: SidebarSectionsState::default(),
        }
    }

    #[test]
    fn round_trip_toml() {
        let ws = sample();
        let s = toml::to_string_pretty(&ws).unwrap();
        let back: WorkspaceState = toml::from_str(&s).unwrap();
        assert_eq!(back, ws);
    }

    #[test]
    fn closed_tabs_are_backward_compatible_persisted_and_capped() {
        let serialized_without_history = toml::to_string_pretty(&sample()).unwrap();
        assert!(!serialized_without_history.contains("closed_tabs"));
        let restored: WorkspaceState = toml::from_str(&serialized_without_history).unwrap();
        assert!(restored.closed_tabs.is_empty());

        let mut ws = sample();
        ws.closed_tabs = (0..25)
            .map(|index| TabState {
                path: format!("/closed/{index}"),
                sort_column: SortColumn::Name,
                sort_order: SortOrder::Asc,
                preview: false,
                show_hidden: false,
                group_mode: GroupMode::FoldersFirst,
            })
            .collect();

        let ws = ws.sanitized();
        assert_eq!(ws.closed_tabs.len(), MAX_CLOSED_TABS);
        assert_eq!(ws.closed_tabs.first().unwrap().path, "/closed/5");
        assert_eq!(ws.closed_tabs.last().unwrap().path, "/closed/24");

        let serialized = toml::to_string_pretty(&ws).unwrap();
        let restored: WorkspaceState = toml::from_str(&serialized).unwrap();
        assert_eq!(restored.closed_tabs, ws.closed_tabs);
    }

    #[test]
    fn sidebar_sections_are_backward_compatible_and_persisted() {
        // The historical "all expanded" state stays absent from the TOML and
        // comes back by default: no old workspace needs migration.
        let without_sidebar = toml::to_string_pretty(&sample()).unwrap();
        assert!(!without_sidebar.contains("sidebar_sections"));
        let restored: WorkspaceState = toml::from_str(&without_sidebar).unwrap();
        assert_eq!(restored.sidebar_sections, SidebarSectionsState::default());

        let mut ws = sample();
        ws.sidebar_sections = SidebarSectionsState {
            shortcuts_collapsed: true,
            favorites_collapsed: false,
            drives_collapsed: true,
            network_collapsed: true,
        };
        let serialized = toml::to_string_pretty(&ws).unwrap();
        assert!(serialized.contains("sidebar_sections"));
        assert!(!serialized.contains("\norder ="));
        let restored: WorkspaceState = toml::from_str(&serialized).unwrap();
        assert_eq!(restored.sidebar_sections, ws.sidebar_sections);

        // Compatibility with the short-lived version that persisted the order
        // per workspace: Serde ignores this old field, the collapse states remain intact.
        let legacy = serialized.replacen(
            "[sidebar_sections]\n",
            "[sidebar_sections]\norder = [\"network\", \"drives\", \"shortcuts\", \"favorites\"]\n",
            1,
        );
        let restored_legacy: WorkspaceState = toml::from_str(&legacy).unwrap();
        assert_eq!(restored_legacy.sidebar_sections, ws.sidebar_sections);

        // Same guarantee through the real envelope of a named workspace
        // (`name` + `saved_at` + `[state]`), whose TOML structure is more
        // nested than the session file.
        let dir = temp_dir("sidebar-sections");
        let id = save_named_workspace(&dir, "Sidebar", &ws).unwrap();
        let (_, restored) = load_named_workspace(&dir, &id).unwrap();
        assert_eq!(restored.sidebar_sections, ws.sidebar_sections);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn save_load_file() {
        let dir = std::env::temp_dir().join(format!(
            "ranger-ws-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.toml");

        // Missing file → None.
        assert!(WorkspaceState::load(&path).unwrap().is_none());

        sample().save(&path).unwrap();
        let loaded = WorkspaceState::load(&path).unwrap().unwrap();
        // `load` sanitizes, which materializes the migrated `layout`.
        assert_eq!(loaded, sample().sanitized());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finds_named_workspace_by_trimmed_case_insensitive_label() {
        let dir = std::env::temp_dir().join(format!(
            "ranger-ws-find-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let id = save_named_workspace(&dir, "Projet Été", &sample()).unwrap();

        let found = find_named_workspace(&dir, "  PROJET ÉTÉ  ").unwrap();
        assert_eq!(found.id, id);
        assert_eq!(found.name, "Projet Été");
        assert!(find_named_workspace(&dir, "workspace.toml").is_none());

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sanitized_clamps_out_of_bounds() {
        let ws = WorkspaceState {
            active_panel: 99,
            panels: vec![PanelState {
                stretch: -3.0,  // invalid
                active_tab: 42, // out of bounds
                tabs: vec![TabState {
                    path: "/x".into(),
                    sort_column: SortColumn::Name,
                    sort_order: SortOrder::Asc,
                    preview: false,
                    show_hidden: false,
                    group_mode: GroupMode::FoldersFirst,
                }],
                columns: crate::columns::default_columns(),
                tab_bar_mode: 0,
                vbar_width: 0.0,
            }],
            layout: None,
            workspace_name: None,
            closed_tabs: Vec::new(),
            sidebar_sections: SidebarSectionsState::default(),
        }
        .sanitized();
        assert_eq!(ws.active_panel, 0);
        assert_eq!(ws.panels[0].active_tab, 0);
        assert_eq!(ws.panels[0].stretch, 1.0);
    }

    #[test]
    fn sanitized_drops_empty_panels() {
        let ws = WorkspaceState {
            active_panel: 0,
            panels: vec![
                PanelState {
                    stretch: 1.0,
                    active_tab: 0,
                    tabs: vec![],
                    columns: crate::columns::default_columns(),
                    tab_bar_mode: 0,
                    vbar_width: 0.0,
                },
                PanelState {
                    stretch: 1.0,
                    active_tab: 0,
                    tabs: vec![TabState {
                        path: "/y".into(),
                        sort_column: SortColumn::Name,
                        sort_order: SortOrder::Asc,
                        preview: false,
                        show_hidden: false,
                        group_mode: GroupMode::FoldersFirst,
                    }],
                    columns: crate::columns::default_columns(),
                    tab_bar_mode: 0,
                    vbar_width: 0.0,
                },
            ],
            layout: None,
            workspace_name: None,
            closed_tabs: Vec::new(),
            sidebar_sections: SidebarSectionsState::default(),
        }
        .sanitized();
        assert_eq!(ws.panels.len(), 1);
        assert_eq!(ws.panels[0].tabs[0].path, "/y");
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ranger-named-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn named_workspace_crud() {
        let dir = temp_dir("crud");

        // Empty list at the start.
        assert!(list_named_workspaces(&dir).is_empty());

        // Save → appears in the list.
        let id = save_named_workspace(&dir, "  Mon espace  ", &sample()).unwrap();
        let list = list_named_workspaces(&dir);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "Mon espace"); // trim applied
        assert_eq!(list[0].id, id);
        // Counters: sample() = 2 panels, 1+2 = 3 tabs.
        assert_eq!(list[0].panels, 2);
        assert_eq!(list[0].tabs, 3);

        // Load → name + state returned (sanitized: migrated layout materialized).
        let (name, state) = load_named_workspace(&dir, &id).unwrap();
        assert_eq!(name, "Mon espace");
        assert_eq!(state, sample().sanitized());

        // Rename → name changed, id stable.
        rename_named_workspace(&dir, &id, "Renommé").unwrap();
        let list = list_named_workspaces(&dir);
        assert_eq!(list[0].name, "Renommé");
        assert_eq!(list[0].id, id);

        // Overwrite → state replaced, name kept.
        let other = WorkspaceState {
            active_panel: 0,
            panels: vec![PanelState {
                stretch: 1.0,
                active_tab: 0,
                tabs: vec![TabState {
                    path: "/var".into(),
                    sort_column: SortColumn::Name,
                    sort_order: SortOrder::Asc,
                    preview: false,
                    show_hidden: false,
                    group_mode: GroupMode::FoldersFirst,
                }],
                columns: crate::columns::default_columns(),
                tab_bar_mode: 0,
                vbar_width: 0.0,
            }],
            layout: None,
            workspace_name: None,
            closed_tabs: Vec::new(),
            sidebar_sections: SidebarSectionsState::default(),
        };
        overwrite_named_workspace(&dir, &id, &other).unwrap();
        let (name, state) = load_named_workspace(&dir, &id).unwrap();
        assert_eq!(name, "Renommé"); // name preserved
        assert_eq!(state.panels.len(), 1);
        assert_eq!(state.panels[0].tabs[0].path, "/var");

        // Delete → empty list.
        delete_named_workspace(&dir, &id).unwrap();
        assert!(list_named_workspaces(&dir).is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unsafe_id() {
        let dir = temp_dir("unsafe");
        assert!(load_named_workspace(&dir, "../etc/passwd").is_err());
        assert!(delete_named_workspace(&dir, "a/b").is_err());
        assert!(!id_is_safe(""));
        assert!(!id_is_safe("a.b"));
        assert!(id_is_safe("ws-12345"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_sorted_newest_first() {
        let dir = temp_dir("sort");
        // Small sleep between each save → strictly increasing `saved_at`
        // (deterministic, no nanosecond collision).
        save_named_workspace(&dir, "Zeta", &sample()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        save_named_workspace(&dir, "alpha", &sample()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let beta_id = save_named_workspace(&dir, "Beta", &sample()).unwrap();
        // By default: the most recently saved comes first.
        let names: Vec<_> = list_named_workspaces(&dir)
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, ["Beta", "alpha", "Zeta"]);

        // An update (overwrite) moves the workspace to the top.
        std::thread::sleep(std::time::Duration::from_millis(3));
        overwrite_named_workspace(&dir, &beta_id, &sample()).unwrap(); // Beta already at the top
        save_named_workspace(&dir, "Gamma", &sample()).unwrap();
        let top = list_named_workspaces(&dir)
            .into_iter()
            .next()
            .map(|m| m.name)
            .unwrap();
        assert_eq!(top, "Gamma");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_rejects_empty_panels_as_none() {
        let dir = std::env::temp_dir().join(format!(
            "ranger-ws-empty-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("workspace.toml");
        std::fs::write(&path, "active_panel = 0\npanels = []\n").unwrap();
        assert!(WorkspaceState::load(&path).unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn layout_migrated_from_flat_panels() {
        // A workspace without `layout` produces a valid chain covering all
        // the panels.
        let ws = sample(); // layout: None, 2 panels
        let tree = ws.layout_tree();
        assert!(tree.is_valid_for(2));
        assert_eq!(tree.leaf_indices(), vec![0, 1]);

        // After sanitize, the layout is materialized and valid.
        let s = ws.sanitized();
        assert!(s.layout.is_some());
        assert!(s.layout.as_ref().unwrap().is_valid_for(s.panels.len()));
    }

    #[test]
    fn stored_valid_layout_survives_round_trip() {
        let mut ws = sample();
        ws.layout = Some(LayoutNode::Split {
            dir: crate::layout::SplitDir::Column,
            ratio: 0.4,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Leaf { panel: 1 }),
        });
        let text = toml::to_string_pretty(&ws).unwrap();
        assert!(text.contains("layout"));
        let back: WorkspaceState = toml::from_str(&text).unwrap();
        assert_eq!(back.layout, ws.layout);
        // The stored tree (Column) is kept by layout_tree (it is valid).
        assert!(matches!(
            back.layout_tree(),
            LayoutNode::Split {
                dir: crate::layout::SplitDir::Column,
                ..
            }
        ));
    }

    #[test]
    fn invalid_stored_layout_falls_back_to_chain() {
        let mut ws = sample(); // 2 panels
        // Layout referencing only panel 0 → invalid for 2 panels.
        ws.layout = Some(LayoutNode::Leaf { panel: 0 });
        let tree = ws.layout_tree();
        assert!(tree.is_valid_for(2)); // rebuilt into a valid chain
        assert_eq!(tree.leaf_indices(), vec![0, 1]);
    }
}
