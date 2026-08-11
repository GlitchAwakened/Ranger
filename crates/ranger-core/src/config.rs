//! User configuration — TOML format, persisted in
//! `$XDG_CONFIG_HOME/ranger/config.toml`.
//!
//! It groups the application's global preferences: language, theme,
//! window dimensions, view behavior, and keyboard shortcuts.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::i18n::{Lang, Theme};

/// Default logical width of the main window (px).
pub const DEFAULT_WINDOW_WIDTH: u32 = 1100;
/// Default logical height of the main window (px).
pub const DEFAULT_WINDOW_HEIGHT: u32 = 700;

/// Stable identity of a reorderable sidebar section.
///
/// The order is a global application preference: the serialized names
/// stay readable in `config.toml` and the compact codes are shared with
/// the Slint UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum SidebarSection {
    Drives = 0,
    Shortcuts = 1,
    Favorites = 2,
    Network = 3,
}

impl SidebarSection {
    pub const COUNT: usize = 4;
    pub const DEFAULT_ORDER: [Self; Self::COUNT] = [
        Self::Drives,
        Self::Shortcuts,
        Self::Favorites,
        Self::Network,
    ];

    pub const fn index(self) -> usize {
        self as usize
    }

    pub const fn from_index(index: usize) -> Option<Self> {
        match index {
            0 => Some(Self::Drives),
            1 => Some(Self::Shortcuts),
            2 => Some(Self::Favorites),
            3 => Some(Self::Network),
            _ => None,
        }
    }

    /// Repairs an order containing duplicates (manually edited config) by
    /// keeping the first sections then appending the missing ones.
    /// The fixed-size array avoids any allocation.
    pub fn sanitized_order(order: [Self; Self::COUNT]) -> [Self; Self::COUNT] {
        let mut normalized = Self::DEFAULT_ORDER;
        let mut len = 0usize;
        for section in order.into_iter().chain(Self::DEFAULT_ORDER) {
            if !normalized[..len].contains(&section) {
                normalized[len] = section;
                len += 1;
                if len == Self::COUNT {
                    break;
                }
            }
        }
        normalized
    }

    /// Moves `section` to its final index in the global order.
    /// Returns `true` only if the order actually changed.
    pub fn reorder(order: &mut [Self; Self::COUNT], section: Self, target_index: usize) -> bool {
        *order = Self::sanitized_order(*order);
        let Some(source_index) = order.iter().position(|item| *item == section) else {
            return false;
        };
        let target_index = target_index.min(Self::COUNT - 1);
        if source_index == target_index {
            return false;
        }
        if source_index < target_index {
            for index in source_index..target_index {
                order[index] = order[index + 1];
            }
        } else {
            for index in (target_index + 1..=source_index).rev() {
                order[index] = order[index - 1];
            }
        }
        order[target_index] = section;
        true
    }
}

fn default_sidebar_section_order() -> [SidebarSection; SidebarSection::COUNT] {
    SidebarSection::DEFAULT_ORDER
}

fn sidebar_section_order_is_default(order: &[SidebarSection; SidebarSection::COUNT]) -> bool {
    *order == SidebarSection::DEFAULT_ORDER
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub language: Lang,
    pub theme: Theme,
    pub window_width: u32,
    pub window_height: u32,
    /// Asks for confirmation before permanent deletion (bypassing the trash).
    pub confirm_permanent_delete: bool,
    /// Panel open in the left column: 0 = collapsed, 1 = Places,
    /// 2 = Favorites.
    #[serde(default = "default_left_panel")]
    pub left_panel: i32,
    /// Logical width (px) of the sidebar.
    pub sidebar_width: u32,
    /// Global order of the sidebar sections. Unlike their collapsed
    /// state, this order follows the user when they switch workspace.
    #[serde(
        default = "default_sidebar_section_order",
        skip_serializing_if = "sidebar_section_order_is_default"
    )]
    pub sidebar_section_order: [SidebarSection; SidebarSection::COUNT],
    /// Default columns — applied on 1st launch and on "Reset".
    #[serde(default = "crate::columns::default_columns")]
    pub default_columns: Vec<crate::columns::ColumnSpec>,
    /// Keyboard shortcut overrides: action_id → chord (`""` = not
    /// assigned). Actions not present use their default (`shortcuts::ACTIONS`).
    #[serde(default)]
    pub shortcut_overrides: std::collections::BTreeMap<String, String>,
    /// Depth for computing folders' **recursive** modification date.
    /// `0` = disabled (folder's own mtime, default); `N > 0` = show the
    /// most recent date among descendants up to `N` levels. Bounded →
    /// controlled cost. Computed in the background (never freezes the UI).
    #[serde(default)]
    pub recursive_mtime_depth: i32,
    /// Depth for computing folders' **recursive size** (sum of descendant
    /// files' bytes). `0` = disabled (default, folders show no size); `N > 0` =
    /// sum down to `N` levels. Bounded → controlled cost. Computed in the
    /// background, sharing ONE directory walk with the recursive mtime above.
    #[serde(default)]
    pub recursive_size_depth: i32,
    /// DEFAULT tab bar position for views created "from
    /// scratch" (new workspace, reset): 0 = top · 1 = left · 2 =
    /// right. Existing views keep their PER-VIEW setting (workspace);
    /// a split inherits from the view it split off, not this default.
    #[serde(default)]
    pub default_tab_bar_mode: u8,
    /// Shows the full path as a tooltip when hovering a tab. `true` by default
    /// (`default = true` → earlier configs without the field keep it on too);
    /// the user can uncheck it.
    #[serde(default = "default_true")]
    pub tab_path_tooltip: bool,
    /// Shows the "unsaved changes" warning dialog before loading
    /// another workspace. `true` by default (safeguard active); an
    /// experienced user can uncheck it to load without confirmation despite
    /// a modified current workspace. `default = true` →
    /// earlier configs (without the field) keep the safeguard.
    #[serde(default = "default_true")]
    pub warn_unsaved_workspace: bool,
    /// In Preview mode, keeps the enlarged row height only for types
    /// capable of showing a thumbnail (images, audio covers, videos, PDF, SVG). Folders
    /// and plain-icon files stay at the normal list height.
    /// Enabled by default, including for older configurations.
    #[serde(default = "default_true")]
    pub compact_icon_rows_in_preview: bool,
    /// Shows the "Modified" date in **UTC** rather than **local time**.
    /// `#[serde(default)]` = `false` keeps local time by default.
    #[serde(default)]
    pub clock_utc: bool,
    /// Shows Windows context-menu entries (shell extensions from
    /// installed programs: PeaZip, Git, TortoiseGit…) in views' context
    /// menu. This setting allows disabling their loading in case of a
    /// slow or unstable third-party extension, since they run in-process.
    #[serde(default = "default_true")]
    pub shell_ctx_menu: bool,
    /// Windows context-menu entries HIDDEN by the user,
    /// identified by their top-level LABEL (stable per machine/OS
    /// language). Checked/unchecked in Settings › Open with.
    #[serde(default)]
    pub shell_menu_disabled: Vec<String>,
    /// Linux: has the one-time "`ffmpeg` missing → no video
    /// thumbnails" warning already been shown? Set to `true` after the first
    /// display so it's never shown again. Not applicable on Windows (native video).
    #[serde(default)]
    pub ffmpeg_hint_shown: bool,
    /// UI zoom chosen by the user (factor MULTIPLIED by the screen's
    /// scale). `1.0` = OS-native scale. Applied at startup and on every
    /// change via `Window::dispatch_event(ScaleFactorChanged)`. Clamped
    /// to `[0.5, 2.0]` on application to avoid an unusable UI.
    #[serde(default = "default_ui_scale")]
    pub ui_scale: f32,
}

fn default_ui_scale() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

/// Default logical width of the sidebar (px).
pub const DEFAULT_SIDEBAR_WIDTH: u32 = 210;

/// Default left panel: Places (1).
fn default_left_panel() -> i32 {
    1
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: Lang::default(),
            theme: Theme::default(),
            window_width: DEFAULT_WINDOW_WIDTH,
            window_height: DEFAULT_WINDOW_HEIGHT,
            confirm_permanent_delete: true,
            left_panel: default_left_panel(),
            sidebar_width: DEFAULT_SIDEBAR_WIDTH,
            sidebar_section_order: SidebarSection::DEFAULT_ORDER,
            default_columns: crate::columns::default_columns(),
            shortcut_overrides: std::collections::BTreeMap::new(),
            recursive_mtime_depth: 0,
            recursive_size_depth: 0,
            default_tab_bar_mode: 0,
            tab_path_tooltip: true,
            warn_unsaved_workspace: true,
            compact_icon_rows_in_preview: true,
            clock_utc: false,
            shell_ctx_menu: true,
            shell_menu_disabled: Vec::new(),
            ffmpeg_hint_shown: false,
            ui_scale: default_ui_scale(),
        }
    }
}

impl Config {
    /// Loads from `path`. If the file doesn't exist, writes the default
    /// config to disk then returns it.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                let cfg: Config = toml::from_str(&content).map_err(|e| {
                    crate::error::Error::Config(format!("parse {}: {e}", path.display()))
                })?;
                Ok(cfg.sanitized())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Self::default();
                cfg.save(path)?;
                Ok(cfg)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Serializes to TOML and writes to disk (creates parent folders).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| {
            crate::error::Error::Config(format!("serialize {}: {e}", path.display()))
        })?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Normalizes only the preferences a manually edited TOML could break
    /// the invariants of. The default order is stable and allocation-free.
    pub fn sanitized(mut self) -> Self {
        self.sidebar_section_order = SidebarSection::sanitized_order(self.sidebar_section_order);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_round_trip() {
        let original = Config::default();
        let s = toml::to_string_pretty(&original).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.language, original.language);
        assert_eq!(back.theme, original.theme);
        assert_eq!(back.window_width, original.window_width);
        assert_eq!(back.window_height, original.window_height);
        assert_eq!(
            back.confirm_permanent_delete,
            original.confirm_permanent_delete
        );
        assert_eq!(
            back.compact_icon_rows_in_preview,
            original.compact_icon_rows_in_preview
        );
        assert_eq!(back.sidebar_section_order, SidebarSection::DEFAULT_ORDER);
    }

    #[test]
    fn load_creates_default_when_missing() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let cfg = Config::load_or_default(&path).unwrap();
        assert_eq!(cfg.language, Lang::default());
        assert!(path.exists());
    }

    #[test]
    fn load_existing_file() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "language = \"fr\"\ntheme = \"dark\"").unwrap();
        let cfg = Config::load_or_default(&path).unwrap();
        assert_eq!(cfg.language, Lang::Fr);
        assert_eq!(cfg.theme, Theme::Dark);
        // Field absent from an earlier config → safeguard KEPT (true).
        assert!(cfg.warn_unsaved_workspace);
        // Same backward compatibility for compact rows in preview.
        assert!(cfg.compact_icon_rows_in_preview);
        // The global order absent from an old config falls back to the historical order.
        assert_eq!(cfg.sidebar_section_order, SidebarSection::DEFAULT_ORDER);
    }

    #[test]
    fn sidebar_order_is_global_round_trips_and_reorders() {
        let mut cfg = Config::default();
        assert!(SidebarSection::reorder(
            &mut cfg.sidebar_section_order,
            SidebarSection::Network,
            0
        ));
        assert_eq!(
            cfg.sidebar_section_order,
            [
                SidebarSection::Network,
                SidebarSection::Drives,
                SidebarSection::Shortcuts,
                SidebarSection::Favorites,
            ]
        );
        assert!(!SidebarSection::reorder(
            &mut cfg.sidebar_section_order,
            SidebarSection::Network,
            0
        ));
        assert!(SidebarSection::reorder(
            &mut cfg.sidebar_section_order,
            SidebarSection::Drives,
            3
        ));

        let serialized = toml::to_string_pretty(&cfg).unwrap();
        assert!(serialized.contains("sidebar_section_order"));
        let restored: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(restored.sidebar_section_order, cfg.sidebar_section_order);
    }

    #[test]
    fn sidebar_order_sanitization_repairs_duplicate_manual_config() {
        let cfg = Config {
            sidebar_section_order: [
                SidebarSection::Network,
                SidebarSection::Network,
                SidebarSection::Drives,
                SidebarSection::Shortcuts,
            ],
            ..Config::default()
        };
        let dir = tempdir();
        let path = dir.join("config.toml");
        cfg.save(&path).unwrap();
        let repaired = Config::load_or_default(&path).unwrap();
        assert_eq!(
            repaired.sidebar_section_order,
            [
                SidebarSection::Network,
                SidebarSection::Drives,
                SidebarSection::Shortcuts,
                SidebarSection::Favorites,
            ]
        );
        std::fs::remove_dir_all(dir).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("ranger-test-{}", rand_suffix()));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn rand_suffix() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}
