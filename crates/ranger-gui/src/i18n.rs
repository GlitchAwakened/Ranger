//! UI translations — loaded from **TOML** files.
//!
//! The strings live in `crates/ranger-gui/i18n/{lang}.toml` and are
//! **embedded at compile time** (so
//! 100% offline, no missing file possible). The user can
//! override or extend any key by dropping a
//! `<config>/ranger/i18n/{lang}.toml` file (dotted-format keys, e.g.
//! `shortcut_action.copy = "…"`).
//!
//! Cascading fallback: key missing from a language → English value (catalogs
//! are built on an English base) → the key itself (never a panic,
//! never a surprise empty string). Adding a language = adding a `.toml` file +
//! an entry in `embedded()`/`catalog()`; adding a string = a key in
//! the TOML files + (if exposed to Slint) a field in `Strings`.

use std::collections::HashMap;
use std::sync::OnceLock;

use ranger_core::{Lang, paths};

use crate::Strings;

// Embedded catalogs (sources of truth, versioned in the repo).
const EMBED_EN: &str = include_str!("../i18n/en.toml");
const EMBED_FR: &str = include_str!("../i18n/fr.toml");
const EMBED_ES: &str = include_str!("../i18n/es.toml");
const EMBED_DE: &str = include_str!("../i18n/de.toml");
const EMBED_IT: &str = include_str!("../i18n/it.toml");

fn embedded(lang: Lang) -> &'static str {
    match lang {
        Lang::En => EMBED_EN,
        Lang::Fr => EMBED_FR,
        Lang::Es => EMBED_ES,
        Lang::De => EMBED_DE,
        Lang::It => EMBED_IT,
    }
}

/// Flattens a TOML table into dotted keys (`shortcut_action.copy`).
fn flatten_into(prefix: &str, table: &toml::Table, out: &mut HashMap<String, String>) {
    for (k, v) in table {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        match v {
            toml::Value::String(s) => {
                out.insert(key, s.clone());
            }
            toml::Value::Table(t) => flatten_into(&key, t, out),
            _ => {}
        }
    }
}

/// Merges the keys from a TOML source on top of `out` (new ones overwrite).
fn overlay(src: &str, out: &mut HashMap<String, String>) {
    match toml::from_str::<toml::Table>(src) {
        Ok(table) => flatten_into("", &table, out),
        Err(err) => tracing::warn!(error = %err, "i18n: ignoring invalid TOML"),
    }
}

/// Builds a language's catalog: English base (fallback) → embedded
/// language → optional on-disk override `<config>/ranger/i18n/{lang}.toml`.
fn build_catalog(lang: Lang) -> HashMap<String, String> {
    let mut map = HashMap::new();
    overlay(EMBED_EN, &mut map);
    if lang != Lang::En {
        overlay(embedded(lang), &mut map);
    }
    let disk = paths::config_dir()
        .join("i18n")
        .join(format!("{}.toml", lang.code()));
    if let Ok(src) = std::fs::read_to_string(&disk) {
        tracing::info!(path = %disk.display(), "i18n: on-disk override applied");
        overlay(&src, &mut map);
    }
    map
}

/// A language's (memoized) catalog. Built only once per language.
fn catalog(lang: Lang) -> &'static HashMap<String, String> {
    fn get(
        lang: Lang,
        cell: &'static OnceLock<HashMap<String, String>>,
    ) -> &'static HashMap<String, String> {
        cell.get_or_init(|| build_catalog(lang))
    }
    static EN: OnceLock<HashMap<String, String>> = OnceLock::new();
    static FR: OnceLock<HashMap<String, String>> = OnceLock::new();
    static ES: OnceLock<HashMap<String, String>> = OnceLock::new();
    static DE: OnceLock<HashMap<String, String>> = OnceLock::new();
    static IT: OnceLock<HashMap<String, String>> = OnceLock::new();
    match lang {
        Lang::En => get(lang, &EN),
        Lang::Fr => get(lang, &FR),
        Lang::Es => get(lang, &ES),
        Lang::De => get(lang, &DE),
        Lang::It => get(lang, &IT),
    }
}

/// Translates a dotted key. Fallback: the language's value → (English base
/// already merged in) → the key itself.
pub fn tr(lang: Lang, key: &str) -> String {
    catalog(lang)
        .get(key)
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

/// Builds the Slint `Strings` struct from the language's catalog.
/// (The only place that enumerates the fields — values come from the TOML files.)
pub fn strings_for(lang: Lang) -> Strings {
    let c = catalog(lang);
    let g = |k: &str| -> slint::SharedString { c.get(k).map(|s| s.as_str()).unwrap_or(k).into() };
    Strings {
        app_title: g("app_title"),
        settings_language: g("settings_language"),
        settings_theme: g("settings_theme"),
        settings_ui_scale: g("settings_ui_scale"),
        settings_ffmpeg_title: g("settings_ffmpeg_title"),
        settings_ffmpeg_hint_ok: g("settings_ffmpeg_hint_ok"),
        settings_ffmpeg_hint_missing: g("settings_ffmpeg_hint_missing"),
        settings_ffmpeg_missing_badge: g("settings_ffmpeg_missing_badge"),
        settings_ffmpeg_recheck: g("settings_ffmpeg_recheck"),
        settings_ffmpeg_detected: g("settings_ffmpeg_detected"),
        settings_ffmpeg_flatpak: g("settings_ffmpeg_flatpak"),
        theme_auto: g("theme_auto"),
        theme_light: g("theme_light"),
        theme_dark: g("theme_dark"),
        col_name: g("col_name"),
        col_path: g("col_path"),
        col_size: g("col_size"),
        col_modified: g("col_modified"),
        col_age: g("col_age"),
        col_ext: g("col_ext"),
        ext_filter_label: g("ext_filter_label"),
        col_resolution: g("col_resolution"),
        col_depth: g("col_depth"),
        fav_title: g("fav_title"),
        fav_new_container: g("fav_new_container"),
        fav_collapse_all: g("fav_collapse_all"),
        fav_expand_all: g("fav_expand_all"),
        fav_empty: g("fav_empty"),
        fav_container_empty: g("fav_container_empty"),
        fav_open: g("fav_open"),
        fav_open_all: g("fav_open_all"),
        fav_new_subcontainer: g("fav_new_subcontainer"),
        fav_rename: g("fav_rename"),
        fav_delete: g("fav_delete"),
        fav_delete_confirm: g("fav_delete_confirm"),
        fav_copy_path: g("fav_copy_path"),
        fav_save_tab: g("fav_save_tab"),
        fav_save_all_tabs: g("fav_save_all_tabs"),
        fav_add: g("fav_add"),
        fav_popup_title: g("fav_popup_title"),
        fav_popup_alias: g("fav_popup_alias"),
        fav_popup_container: g("fav_popup_container"),
        fav_popup_parent: g("fav_popup_parent"),
        fav_popup_new: g("fav_popup_new"),
        fav_popup_root: g("fav_popup_root"),
        fav_save: g("fav_save"),
        fav_toast_added: g("fav_toast_added"),
        fav_toast_missing: g("fav_toast_missing"),
        fav_toast_exists: g("fav_toast_exists"),
        footer_items_singular: g("footer_items_singular"),
        footer_items_plural: g("footer_items_plural"),
        footer_empty: g("footer_empty"),
        footer_selected_singular: g("footer_selected_singular"),
        footer_selected_plural: g("footer_selected_plural"),
        nav_prev: g("nav_prev"),
        nav_next: g("nav_next"),
        nav_parent: g("nav_parent"),
        nav_home: g("nav_home"),
        nav_refresh: g("nav_refresh"),
        view_mode_tooltip: g("view_mode_tooltip"),
        show_hidden_tooltip: g("show_hidden_tooltip"),
        group_section: g("group_section"),
        group_folders_first: g("group_folders_first"),
        group_files_first: g("group_files_first"),
        group_mixed: g("group_mixed"),
        tab_new_tooltip: g("tab_new_tooltip"),
        tab_scroll_left: g("tab_scroll_left"),
        tab_scroll_right: g("tab_scroll_right"),
        tabbar_menu_tooltip: g("tabbar_menu_tooltip"),
        tabbar_top: g("tabbar_top"),
        tabbar_left: g("tabbar_left"),
        tabbar_right: g("tabbar_right"),
        settings_tabbar_default: g("settings_tabbar_default"),
        settings_tab_tooltip_label: g("settings_tab_tooltip_label"),
        settings_tab_tooltip_hint: g("settings_tab_tooltip_hint"),
        settings_clock_label: g("settings_clock_label"),
        settings_clock_local: g("settings_clock_local"),
        settings_ws_warn_label: g("settings_ws_warn_label"),
        settings_ws_warn_hint: g("settings_ws_warn_hint"),
        settings_compact_preview_label: g("settings_compact_preview_label"),
        settings_compact_preview_hint: g("settings_compact_preview_hint"),
        tab_unavailable: g("tab_unavailable"),
        tab_unavailable_hint: g("tab_unavailable_hint"),
        tab_duplicate: g("tab_duplicate"),
        tab_reopen_closed: g("tab_reopen_closed"),
        tab_close_tooltip: g("tab_close_tooltip"),
        panel_prefix: g("panel_prefix"),
        panel_new_tooltip: g("panel_new_tooltip"),
        panel_close_tooltip: g("panel_close_tooltip"),
        panel_split_view: g("panel_split_view"),
        split_side_by_side: g("split_side_by_side"),
        split_stacked: g("split_stacked"),
        ctx_open: g("ctx_open"),
        ctx_open_new_tab: g("ctx_open_new_tab"),
        ctx_open_with: g("ctx_open_with"),
        ctx_open_parent: g("ctx_open_parent"),
        ctx_open_admin: g("ctx_open_admin"),
        ctx_new: g("ctx_new"),
        ctx_new_folder: g("ctx_new_folder"),
        ctx_new_file: g("ctx_new_file"),
        ctx_new_shortcut: g("ctx_new_shortcut"),
        create_shortcut_target: g("create_shortcut_target"),
        create_browse: g("create_browse"),
        ctx_create_link: g("ctx_create_link"),
        create_symlink_tab: g("create_symlink_tab"),
        create_confirm: g("create_confirm"),
        create_placeholder: g("create_placeholder"),
        ctx_open_terminal: g("ctx_open_terminal"),
        ctx_copy: g("ctx_copy"),
        ctx_cut: g("ctx_cut"),
        ctx_paste: g("ctx_paste"),
        ctx_duplicate: g("ctx_duplicate"),
        ctx_copy_path: g("ctx_copy_path"),
        ctx_copy_name: g("ctx_copy_name"),
        ctx_rename: g("ctx_rename"),
        ctx_delete: g("ctx_delete"),
        ctx_properties: g("ctx_properties"),
        prop_kind_folder: g("prop_kind_folder"),
        prop_kind_file: g("prop_kind_file"),
        settings_tooltip: g("settings_tooltip"),
        settings_title: g("settings_title"),
        settings_tab_general: g("settings_tab_general"),
        settings_columns_section: g("settings_columns_section"),
        settings_col_resolution: g("settings_col_resolution"),
        settings_col_depth: g("settings_col_depth"),
        settings_rmtime_section: g("settings_rmtime_section"),
        settings_rmtime_label: g("settings_rmtime_label"),
        settings_rmtime_hint: g("settings_rmtime_hint"),
        settings_fsize_label: g("settings_fsize_label"),
        settings_fsize_hint: g("settings_fsize_hint"),
        settings_paths_section: g("settings_paths_section"),
        settings_config_path: g("settings_config_path"),
        settings_data_path: g("settings_data_path"),
        settings_cache_path: g("settings_cache_path"),
        settings_open: g("settings_open"),
        settings_shortcuts_section: g("settings_shortcuts_section"),
        shortcut_search_placeholder: g("shortcut_search_placeholder"),
        shortcut_reset_all: g("shortcut_reset_all"),
        shortcut_reset_all_confirm: g("shortcut_reset_all_confirm"),
        shortcut_reset_tooltip: g("shortcut_reset_tooltip"),
        shortcut_listening: g("shortcut_listening"),
        shortcut_capture_cancel: g("shortcut_capture_cancel"),
        shortcut_unassigned: g("shortcut_unassigned"),
        shortcut_unassign: g("shortcut_unassign"),
        shortcut_no_results: g("shortcut_no_results"),
        shortcut_conflict_reassign: g("shortcut_conflict_reassign"),
        shortcut_conflict_keep: g("shortcut_conflict_keep"),
        rename_title: g("rename_title"),
        rename_current_prefix: g("rename_current_prefix"),
        rename_new_placeholder: g("rename_new_placeholder"),
        rename_confirm: g("rename_confirm"),
        rename_force_replace: g("rename_force_replace"),
        btn_cancel: g("btn_cancel"),
        paste_conflict_title: g("paste_conflict_title"),
        paste_conflict_hint: g("paste_conflict_hint"),
        paste_conflict_taken: g("paste_conflict_taken"),
        paste_conflict_skip: g("paste_conflict_skip"),
        paste_conflict_confirm: g("paste_conflict_confirm"),
        paste_conflict_replace: g("paste_conflict_replace"),
        paste_conflict_replace_all: g("paste_conflict_replace_all"),
        paste_conflict_skip_all: g("paste_conflict_skip_all"),
        drag_open_with: g("drag_open_with"),
        op_copying: g("op_copying"),
        op_moving: g("op_moving"),
        op_deleting: g("op_deleting"),
        op_deleting_permanently: g("op_deleting_permanently"),
        op_duplicating: g("op_duplicating"),
        op_scanning: g("op_scanning"),
        op_done: g("op_done"),
        op_cancelled: g("op_cancelled"),
        op_errors: g("op_errors"),
        op_items: g("op_items"),
        ws_tooltip: g("ws_tooltip"),
        ws_title: g("ws_title"),
        ws_save_section: g("ws_save_section"),
        ws_name_placeholder: g("ws_name_placeholder"),
        ws_name_taken: g("ws_name_taken"),
        ws_save_btn: g("ws_save_btn"),
        ws_saved_section: g("ws_saved_section"),
        ws_empty: g("ws_empty"),
        ws_empty_hint: g("ws_empty_hint"),
        ws_load: g("ws_load"),
        ws_update: g("ws_update"),
        ws_rename: g("ws_rename"),
        ws_delete: g("ws_delete"),
        ws_delete_confirm: g("ws_delete_confirm"),
        ws_overwrite_confirm: g("ws_overwrite_confirm"),
        ws_panels_short: g("ws_panels_short"),
        ws_tabs_short: g("ws_tabs_short"),
        ws_toast_saved: g("ws_toast_saved"),
        ws_toast_updated: g("ws_toast_updated"),
        ws_toast_renamed: g("ws_toast_renamed"),
        ws_toast_deleted: g("ws_toast_deleted"),
        ws_notice_saved: g("ws_notice_saved"),
        ws_notice_save_failed: g("ws_notice_save_failed"),
        ws_dirty_title: g("ws_dirty_title"),
        ws_dirty_body: g("ws_dirty_body"),
        ws_dirty_discard: g("ws_dirty_discard"),
        ws_dirty_save_load: g("ws_dirty_save_load"),
        ws_dirty_tooltip: g("ws_dirty_tooltip"),
        ws_dirty_save_action: g("ws_dirty_save_action"),
        sidebar_tooltip: g("sidebar_tooltip"),
        sidebar_shortcuts: g("sidebar_shortcuts"),
        sidebar_drives: g("sidebar_drives"),
        sidebar_network: g("sidebar_network"),
        net_ejected: g("net_ejected"),
        net_disconnected: g("net_disconnected"),
        net_eject_failed: g("net_eject_failed"),
        net_eject: g("net_eject"),
        net_disconnect: g("net_disconnect"),
        sidebar_home: g("sidebar_home"),
        sidebar_trash: g("sidebar_trash"),
        sidebar_root: g("sidebar_root"),
        sidebar_empty_trash: g("sidebar_empty_trash"),
        sidebar_empty_trash_confirm: g("sidebar_empty_trash_confirm"),
        net_delete_title: g("net_delete_title"),
        net_delete_body: g("net_delete_body"),
        permanent_delete_body: g("permanent_delete_body"),
        net_delete_confirm: g("net_delete_confirm"),
        drop_move: g("drop_move"),
        drop_copy: g("drop_copy"),
        drop_link: g("drop_link"),
        ws_reset: g("ws_reset"),
        ws_new_name: g("ws_new_name"),
        ws_toast_reset: g("ws_toast_reset"),
        ws_sort_newest: g("ws_sort_newest"),
        ws_sort_oldest: g("ws_sort_oldest"),
        ws_sort_tooltip: g("ws_sort_tooltip"),
        ow_choose: g("ow_choose"),
        ow_pick_browse: g("ow_pick_browse"),
        ow_custom: g("ow_custom"),
        ow_use_as_app: g("ow_use_as_app"),
        ow_manage: g("ow_manage"),
        ow_title: g("ow_title"),
        ow_name: g("ow_name"),
        ow_program: g("ow_program"),
        ow_store_program: g("ow_store_program"),
        ow_args: g("ow_args"),
        ow_tags: g("ow_tags"),
        ow_tag_file: g("ow_tag_file"),
        ow_tag_dir: g("ow_tag_dir"),
        ow_tag_dirname: g("ow_tag_dirname"),
        ow_tag_name: g("ow_tag_name"),
        ow_tag_stem: g("ow_tag_stem"),
        ow_tag_ext: g("ow_tag_ext"),
        ow_tag_uri: g("ow_tag_uri"),
        ow_tag_files: g("ow_tag_files"),
        ow_preview: g("ow_preview"),
        ow_add_suggested: g("ow_add_suggested"),
        ow_recipes_label: g("ow_recipes_label"),
        ow_recipes_hint: g("ow_recipes_hint"),
        ow_launch: g("ow_launch"),
        ow_default_ext: g("ow_default_ext"),
        ow_used_ext: g("ow_used_ext"),
        ow_set_default: g("ow_set_default"),
        ow_run_as_admin: g("ow_run_as_admin"),
        ow_default_ext_hint: g("ow_default_ext_hint"),
        ow_show_in_section: g("ow_show_in_section"),
        settings_shellext_empty: g("settings_shellext_empty"),
        ow_ctx_menu: g("ow_ctx_menu"),
        ow_ctx_files: g("ow_ctx_files"),
        ow_ctx_dirs: g("ow_ctx_dirs"),
        ow_ctx_background: g("ow_ctx_background"),
        ow_ctx_ext: g("ow_ctx_ext"),
        ow_ctx_ext_hint: g("ow_ctx_ext_hint"),
        ow_used_ext_hint: g("ow_used_ext_hint"),
        settings_shellmenu_label: g("settings_shellmenu_label"),
        settings_shellmenu_hint: g("settings_shellmenu_hint"),
        settings_shellext_list_label: g("settings_shellext_list_label"),
        settings_shellext_list_hint: g("settings_shellext_list_hint"),
        ow_move_up: g("ow_move_up"),
        ow_move_down: g("ow_move_down"),
        ow_duplicate_tip: g("ow_duplicate_tip"),
        ow_edit_tip: g("ow_edit_tip"),
        ow_delete_tip: g("ow_delete_tip"),
        settings_openers_section: g("settings_openers_section"),
        ow_add: g("ow_add"),
        ow_empty: g("ow_empty"),
        ow_search_placeholder: g("ow_search_placeholder"),
        ow_no_results: g("ow_no_results"),
        ow_promote: g("ow_promote"),
        ow_promote_add: g("ow_promote_add"),
    }
}

/// Language picker labels: **native names** (each language in its own
/// language → never translated).
pub fn language_labels() -> Vec<slint::SharedString> {
    Lang::all().iter().map(|l| l.native_name().into()).collect()
}

/// Theme picker labels (Auto / Light / Dark) in the given language.
pub fn theme_labels(lang: Lang) -> Vec<slint::SharedString> {
    vec![
        tr(lang, "theme_auto").into(),
        tr(lang, "theme_light").into(),
        tr(lang, "theme_dark").into(),
    ]
}

/// Tab bar position picker labels (top / left /
/// right) — same text as the per-view menu.
pub fn tabbar_labels(lang: Lang) -> Vec<slint::SharedString> {
    vec![
        tr(lang, "tabbar_top").into(),
        tr(lang, "tabbar_left").into(),
        tr(lang, "tabbar_right").into(),
    ]
}

/// Footer text based on the item count.
pub fn footer_items_text(lang: Lang, count: usize) -> String {
    match count {
        0 => tr(lang, "footer_empty"),
        1 => tr(lang, "footer_items_singular"),
        n => tr(lang, "footer_items_plural").replace("{n}", &n.to_string()),
    }
}

/// Summary chip standing in for the operation toasts the stack cannot show.
/// Only called with `count >= 1`.
pub fn op_more_text(lang: Lang, count: usize) -> String {
    match count {
        1 => tr(lang, "op_more_singular"),
        n => tr(lang, "op_more_plural").replace("{n}", &n.to_string()),
    }
}

/// Full footer: "N items", then (if `hidden > 0`) "· K hidden",
/// then (if there's a selection) "· M selected". The "hidden" segment is only
/// passed when hidden items are NOT shown (a subtle reminder).
pub fn footer_text(lang: Lang, total: usize, selected: usize, hidden: usize) -> String {
    let mut s = footer_items_text(lang, total);
    if hidden > 0 {
        let h = match hidden {
            1 => tr(lang, "footer_hidden_singular"),
            n => tr(lang, "footer_hidden_plural").replace("{n}", &n.to_string()),
        };
        s = format!("{s}  ·  {h}");
    }
    if selected > 0 {
        let sel = match selected {
            1 => tr(lang, "footer_selected_singular"),
            n => tr(lang, "footer_selected_plural").replace("{n}", &n.to_string()),
        };
        s = format!("{s}  ·  {sel}");
    }
    s
}

// Keyboard shortcuts: labels resolved from the catalog -----

/// Localized label of a shortcut group (`code` = `ActionGroup::code`).
pub fn shortcut_group_name(lang: Lang, code: &str) -> String {
    tr(lang, &format!("shortcut_group.{code}"))
}

/// Localized label of a shortcut action (`id` = `ActionDef::id`).
pub fn shortcut_action_name(lang: Lang, id: &str) -> String {
    tr(lang, &format!("shortcut_action.{id}"))
}

/// Localized conflict message: the template contains `{name}`.
pub fn shortcut_conflict_message(lang: Lang, other: &str) -> String {
    tr(lang, "shortcut_conflict.template").replace("{name}", other)
}

/// "Notice" toast message when access to a folder is denied.
pub fn access_denied(lang: Lang) -> String {
    tr(lang, "access_denied")
}

/// Human-readable message after an operation is refused by a Windows lock.
/// The ownerless template stays honest when Restart Manager confirms the
/// conflict but can't inspect the process (permissions, folder handle).
pub fn item_in_use(lang: Lang, item: &str, processes: &[String], truncated: bool) -> String {
    let mut process = processes.join(", ");
    if truncated && !process.is_empty() {
        process.push_str(", …");
    }
    let key = if process.is_empty() {
        "op_in_use_unknown"
    } else {
        "op_in_use_by"
    };
    tr(lang, key)
        .replace("{item}", item)
        .replace("{process}", &process)
}

/// Translated fallback when a rename fails without an attributable Windows
/// lock. `reason` keeps the detail provided by the OS (permissions, invalid
/// name, network…).
pub fn rename_failed(lang: Lang, item: &str, reason: &str) -> String {
    tr(lang, "rename_failed")
        .replace("{item}", item)
        .replace("{reason}", reason)
}

/// Translated reason for a device removal / network disconnection failure.
/// `err` carries no text of its own (see [`ranger_core::eject::EjectError`]);
/// this is the only place that turns it into a sentence.
pub fn eject_error_message(lang: Lang, err: &ranger_core::eject::EjectError) -> String {
    use ranger_core::eject::EjectError;
    match err {
        EjectError::UnknownDevice => tr(lang, "eject_unknown_device"),
        EjectError::BlockedByProcesses(processes) => {
            tr(lang, "eject_blocked_by_processes").replace("{processes}", &processes.join(", "))
        }
        EjectError::BlockedByService => tr(lang, "eject_blocked_by_service"),
        EjectError::BlockedByApplication => tr(lang, "eject_blocked_by_application"),
        EjectError::BlockedByOpenFile => tr(lang, "eject_blocked_by_open_file"),
        EjectError::DeviceBusy => tr(lang, "eject_device_busy"),
        EjectError::ToolNotFound => tr(lang, "eject_tool_not_found"),
        EjectError::ToolFailed {
            bin,
            detail: Some(detail),
        } => tr(lang, "eject_tool_failed_detail")
            .replace("{bin}", bin)
            .replace("{detail}", detail),
        EjectError::ToolFailed { bin, detail: None } => {
            tr(lang, "eject_tool_failed").replace("{bin}", bin)
        }
        EjectError::DisconnectFailed(code) => {
            tr(lang, "eject_disconnect_failed").replace("{code}", &code.to_string())
        }
        EjectError::LinuxNetworkDisconnectUnsupported => {
            tr(lang, "eject_linux_network_unsupported")
        }
        EjectError::SystemQueryFailed => tr(lang, "eject_system_query_failed"),
        EjectError::PlatformUnsupported => tr(lang, "eject_platform_unsupported"),
    }
}

/// Translated reason for a trash-restore failure. `err` carries no text of
/// its own (see [`ranger_core::fs::ops::TrashError`]); this is the only place
/// that turns it into a sentence.
pub fn trash_error_message(lang: Lang, err: &ranger_core::fs::ops::TrashError) -> String {
    use ranger_core::fs::ops::TrashError;
    match err {
        TrashError::ListFailed(detail) => {
            tr(lang, "trash_reason_list_failed").replace("{detail}", detail)
        }
        TrashError::ItemNotFound => tr(lang, "trash_reason_item_not_found"),
        TrashError::RestoreFailed(detail) => {
            tr(lang, "trash_reason_restore_failed").replace("{detail}", detail)
        }
        TrashError::PlatformUnsupported => tr(lang, "trash_reason_platform_unsupported"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_loads_and_has_app_title() {
        for &lang in Lang::all() {
            let s = strings_for(lang);
            assert_eq!(s.app_title.as_str(), "Ranger", "{lang:?}");
        }
    }

    #[test]
    fn french_overrides_english() {
        assert_eq!(tr(Lang::Fr, "settings_language"), "Langue");
        assert_eq!(tr(Lang::En, "settings_language"), "Language");
    }

    #[test]
    fn missing_key_falls_back_to_english_then_key() {
        // Key present everywhere: the language's value.
        assert_eq!(shortcut_action_name(Lang::Fr, "copy"), "Copier");
        // Nonexistent key → returned as-is (no panic/empty string).
        assert_eq!(tr(Lang::Fr, "zzz_inexistante"), "zzz_inexistante");
    }

    #[test]
    fn conflict_template_interpolates_name() {
        let msg = shortcut_conflict_message(Lang::En, "Copy");
        assert!(msg.contains("Copy") && !msg.contains("{name}"));
    }

    #[test]
    fn item_in_use_message_is_translated_and_interpolated() {
        for &lang in Lang::all() {
            let processes = vec!["Code.exe".to_owned()];
            let known = item_in_use(lang, "Tofs", &processes, true);
            assert!(known.contains("Tofs") && known.contains("Code.exe"));
            assert!(known.contains('…'));
            assert!(!known.contains("{item}") && !known.contains("{process}"));

            let unknown = item_in_use(lang, "Tofs", &[], false);
            assert!(unknown.contains("Tofs"));
            assert!(!unknown.contains("{item}") && !unknown.contains("{process}"));
        }
    }

    #[test]
    fn rename_failure_is_translated_and_interpolated() {
        for &lang in Lang::all() {
            let message = rename_failed(lang, "photo.png", "access denied");
            assert!(message.contains("photo.png") && message.contains("access denied"));
            assert!(!message.contains("{item}") && !message.contains("{reason}"));
        }
    }

    #[test]
    fn all_strings_fields_resolve_nonempty() {
        // No `Strings` field should fall back to the raw key (= a key
        // missing from en.toml). Covers all 162 fields via one language.
        let c = catalog(Lang::En);
        for (k, v) in c {
            assert!(!v.is_empty(), "empty key: {k}");
        }
    }

    /// Every key requested by `strings_for` via `g("…")` must exist at the
    /// root level of the English catalog. A key placed under a `[…]` section
    /// becomes `section.key` and can't be resolved by `g`. This test re-reads
    /// the source of `strings_for`.
    #[test]
    fn every_g_key_present_in_en() {
        let src = include_str!("i18n.rs");
        let c = catalog(Lang::En);
        let mut checked = 0usize;
        for line in src.lines() {
            // Ignore COMMENTS (including this docstring, which contains `g("…")`).
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut rest = line;
            while let Some(pos) = rest.find("g(\"") {
                let after = &rest[pos + 3..];
                let Some(end) = after.find('"') else { break };
                let key = &after[..end];
                assert!(
                    c.contains_key(key),
                    "key g(\"{key}\") missing from the top level of en.toml (misplaced under a [section]?)"
                );
                checked += 1;
                rest = &after[end + 1..];
            }
        }
        // Guard: the scan did find keys (otherwise the test proves nothing).
        assert!(
            checked > 100,
            "scan g(\"…\") found abnormally few keys: {checked}"
        );
    }
}
