//! Locating Ranger's data, config, and cache folders — **cross-platform**
//! via the `dirs` crate:
//!   - **Linux**: XDG spec (`$XDG_DATA_HOME` → `~/.local/share`,
//!     `$XDG_CONFIG_HOME` → `~/.config`, `$XDG_CACHE_HOME` → `~/.cache`).
//!   - **Windows**: `%APPDATA%` (config + data, *Roaming*) and `%LOCALAPPDATA%`
//!     (cache).
//!
//! `dirs::*_dir()` returns `None` only if no base can be found
//! (e.g. `HOME`/`%APPDATA%` missing); we then fall back to the temp folder
//! so we never panic.

use std::path::PathBuf;

/// Name of the application subdirectory under each base.
const APP: &str = "ranger";

/// Joins `APP` to the given base, falling back to the temp folder if the
/// base can't be found (degenerate case).
fn app_dir(base: Option<PathBuf>) -> PathBuf {
    base.unwrap_or_else(std::env::temp_dir).join(APP)
}

/// Data: Linux `$XDG_DATA_HOME/ranger` (default `~/.local/share/ranger`);
/// Windows `%APPDATA%\ranger`.
pub fn data_dir() -> PathBuf {
    app_dir(dirs::data_dir())
}

/// Config: Linux `$XDG_CONFIG_HOME/ranger` (default `~/.config/ranger`);
/// Windows `%APPDATA%\ranger`.
pub fn config_dir() -> PathBuf {
    app_dir(dirs::config_dir())
}

/// Cache: Linux `$XDG_CACHE_HOME/ranger` (default `~/.cache/ranger`);
/// Windows `%LOCALAPPDATA%\ranger`.
pub fn cache_dir() -> PathBuf {
    app_dir(dirs::cache_dir())
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn workspace_path() -> PathBuf {
    data_dir().join("workspace.toml")
}

/// Folder for **named** workspaces (one `.toml` file per workspace).
/// Distinct from the automatic `workspace.toml` (current session state).
pub fn workspaces_dir() -> PathBuf {
    data_dir().join("workspaces")
}

/// SINGLE file for the favorites tree (shared/common storage).
pub fn favorites_path() -> PathBuf {
    data_dir().join("favorites.toml")
}

/// File for "openers" (Open with: programs + custom commands). Data,
/// not config: a growing collection (cf. favorites.toml).
pub fn openers_path() -> PathBuf {
    data_dir().join("openers.toml")
}

pub fn log_path() -> PathBuf {
    cache_dir().join("ranger.log")
}

/// Creates all the XDG directories if they don't already exist.
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::create_dir_all(config_dir())?;
    std::fs::create_dir_all(cache_dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_end_in_app_subdir() {
        assert!(data_dir().ends_with(APP));
        assert!(config_dir().ends_with(APP));
        assert!(cache_dir().ends_with(APP));
        assert!(config_path().ends_with("config.toml"));
        assert!(workspace_path().ends_with("workspace.toml"));
    }
}
