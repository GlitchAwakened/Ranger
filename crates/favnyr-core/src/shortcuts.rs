//! **Configurable** keyboard shortcuts.
//!
//! The core only handles data: a catalogue of bindable actions (`ACTIONS`),
//! parsing/serialization of a combination (`Chord`), and resolution of the
//! effective map (defaults + user overrides) with conflict detection. The
//! GUI supplies the key name **already normalized** (special keys are
//! mapped via Slint's `Key.*` constants), executes the actions, and displays
//! the settings section. No network dependency, 100% local.

use std::collections::BTreeMap;

/// Action category — used for grouping in settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionGroup {
    Navigation,
    Selection,
    TabsViews,
    FileOps,
    ViewOptions,
    App,
}

impl ActionGroup {
    /// i18n key suffix of the group label (resolved on the GUI side).
    pub fn code(self) -> &'static str {
        match self {
            ActionGroup::Navigation => "navigation",
            ActionGroup::Selection => "selection",
            ActionGroup::TabsViews => "tabs_views",
            ActionGroup::FileOps => "file_ops",
            ActionGroup::ViewOptions => "view_options",
            ActionGroup::App => "app",
        }
    }
}

/// Static definition of a bindable action. `id` serves both as the config
/// key (overrides) and as the i18n key suffix of the displayed label.
#[derive(Debug, Clone, Copy)]
pub struct ActionDef {
    pub id: &'static str,
    pub group: ActionGroup,
    /// Default combination (canonical chord). `""` = unassigned by default.
    pub default: &'static str,
}

/// Ordered catalogue of actions (order = display order in settings).
pub const ACTIONS: &[ActionDef] = &[
    // ----- Navigation -----
    ActionDef {
        id: "cursor-up",
        group: ActionGroup::Navigation,
        default: "ArrowUp",
    },
    ActionDef {
        id: "cursor-down",
        group: ActionGroup::Navigation,
        default: "ArrowDown",
    },
    ActionDef {
        id: "cursor-first",
        group: ActionGroup::Navigation,
        default: "Home",
    },
    ActionDef {
        id: "cursor-last",
        group: ActionGroup::Navigation,
        default: "End",
    },
    ActionDef {
        id: "nav-back",
        group: ActionGroup::Navigation,
        default: "Alt+ArrowLeft",
    },
    ActionDef {
        id: "nav-forward",
        group: ActionGroup::Navigation,
        default: "Alt+ArrowRight",
    },
    ActionDef {
        id: "nav-parent",
        group: ActionGroup::Navigation,
        default: "Alt+ArrowUp",
    },
    ActionDef {
        id: "nav-home",
        group: ActionGroup::Navigation,
        default: "Alt+Home",
    },
    ActionDef {
        id: "refresh",
        group: ActionGroup::Navigation,
        default: "F5",
    },
    ActionDef {
        id: "focus-address",
        group: ActionGroup::Navigation,
        default: "Ctrl+L",
    },
    // ----- Selection -----
    ActionDef {
        id: "select-all",
        group: ActionGroup::Selection,
        default: "Ctrl+A",
    },
    ActionDef {
        id: "extend-up",
        group: ActionGroup::Selection,
        default: "Shift+ArrowUp",
    },
    ActionDef {
        id: "extend-down",
        group: ActionGroup::Selection,
        default: "Shift+ArrowDown",
    },
    // ----- Tabs & views -----
    ActionDef {
        id: "tab-new",
        group: ActionGroup::TabsViews,
        default: "Ctrl+T",
    },
    ActionDef {
        id: "tab-close",
        group: ActionGroup::TabsViews,
        default: "Ctrl+W",
    },
    ActionDef {
        id: "tab-reopen-closed",
        group: ActionGroup::TabsViews,
        default: "Ctrl+Shift+T",
    },
    ActionDef {
        id: "tab-next",
        group: ActionGroup::TabsViews,
        default: "Ctrl+Tab",
    },
    ActionDef {
        id: "tab-prev",
        group: ActionGroup::TabsViews,
        default: "Ctrl+Shift+Tab",
    },
    ActionDef {
        id: "view-next",
        group: ActionGroup::TabsViews,
        default: "Tab",
    },
    ActionDef {
        id: "split-side",
        group: ActionGroup::TabsViews,
        default: "Ctrl+Backslash",
    },
    ActionDef {
        id: "split-stack",
        group: ActionGroup::TabsViews,
        default: "Ctrl+Shift+Backslash",
    },
    // ----- File operations -----
    ActionDef {
        id: "open",
        group: ActionGroup::FileOps,
        default: "Enter",
    },
    ActionDef {
        id: "copy",
        group: ActionGroup::FileOps,
        default: "Ctrl+C",
    },
    ActionDef {
        id: "cut",
        group: ActionGroup::FileOps,
        default: "Ctrl+X",
    },
    ActionDef {
        id: "paste",
        group: ActionGroup::FileOps,
        default: "Ctrl+V",
    },
    ActionDef {
        id: "rename",
        group: ActionGroup::FileOps,
        default: "F2",
    },
    ActionDef {
        id: "delete",
        group: ActionGroup::FileOps,
        default: "Delete",
    },
    ActionDef {
        id: "delete-permanent",
        group: ActionGroup::FileOps,
        default: "Shift+Delete",
    },
    ActionDef {
        id: "restore-trash",
        group: ActionGroup::FileOps,
        default: "Ctrl+Z",
    },
    ActionDef {
        id: "new-folder",
        group: ActionGroup::FileOps,
        default: "Ctrl+Shift+N",
    },
    ActionDef {
        id: "new-file",
        group: ActionGroup::FileOps,
        default: "Ctrl+Alt+N",
    },
    ActionDef {
        id: "terminal",
        group: ActionGroup::FileOps,
        default: "Ctrl+Alt+T",
    },
    ActionDef {
        id: "properties",
        group: ActionGroup::FileOps,
        default: "Alt+Enter",
    },
    // ----- View options -----
    ActionDef {
        id: "toggle-hidden",
        group: ActionGroup::ViewOptions,
        default: "Ctrl+H",
    },
    ActionDef {
        id: "cycle-group",
        group: ActionGroup::ViewOptions,
        default: "Ctrl+G",
    },
    ActionDef {
        id: "toggle-view-mode",
        group: ActionGroup::ViewOptions,
        default: "Ctrl+P",
    },
    // ----- Application -----
    ActionDef {
        id: "save-workspace",
        group: ActionGroup::App,
        default: "Ctrl+S",
    },
    ActionDef {
        id: "open-settings",
        group: ActionGroup::App,
        default: "Ctrl+Comma",
    },
    ActionDef {
        id: "open-workspaces",
        group: ActionGroup::App,
        default: "Ctrl+Q",
    },
];

/// Normalized combination. Structural equality means "same shortcut".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Canonical key name (e.g. `"A"`, `"ArrowUp"`, `"F2"`, `"Backslash"`).
    pub key: String,
}

impl Chord {
    /// From a keyboard event. `key` must already be a normalized name for
    /// special keys (supplied by the GUI via the `Key.*` constants).
    pub fn from_event(key: &str, ctrl: bool, alt: bool, shift: bool) -> Option<Chord> {
        Some(Chord {
            ctrl,
            alt,
            shift,
            key: normalize_key(key)?,
        })
    }

    /// Parses a text form like "Ctrl+Shift+N". Returns `None` if there's no key.
    pub fn parse(s: &str) -> Option<Chord> {
        let (mut ctrl, mut alt, mut shift) = (false, false, false);
        let mut key = None;
        for part in s.split('+') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            match p.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "alt" => alt = true,
                "shift" => shift = true,
                _ => key = Some(normalize_key(p)?),
            }
        }
        key.map(|key| Chord {
            ctrl,
            alt,
            shift,
            key,
        })
    }

    /// Canonical form "Ctrl+Alt+Shift+key" (fixed modifier order).
    pub fn serialize(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl+");
        }
        if self.alt {
            s.push_str("Alt+");
        }
        if self.shift {
            s.push_str("Shift+");
        }
        s.push_str(&self.key);
        s
    }
}

/// Normalizes a key name to its canonical form. Single letter → uppercase;
/// special names are case-insensitive; unknown → returned as-is (robustness).
fn normalize_key(k: &str) -> Option<String> {
    let k = k.trim();
    if k.is_empty() {
        return None;
    }
    if k.chars().count() == 1 {
        let c = k.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase().to_string());
        }
        return Some(k.to_string()); // digit / literal punctuation
    }
    let lower = k.to_ascii_lowercase();
    // F1..F24
    if let Some(rest) = lower.strip_prefix('f')
        && let Ok(n) = rest.parse::<u8>()
        && (1..=24).contains(&n)
    {
        return Some(format!("F{n}"));
    }
    let canon = match lower.as_str() {
        "arrowup" | "up" => "ArrowUp",
        "arrowdown" | "down" => "ArrowDown",
        "arrowleft" | "left" => "ArrowLeft",
        "arrowright" | "right" => "ArrowRight",
        "tab" => "Tab",
        "enter" | "return" => "Enter",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "space" => "Space",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PageUp",
        "pagedown" | "pgdn" => "PageDown",
        "insert" | "ins" => "Insert",
        "backslash" => "Backslash",
        "comma" => "Comma",
        _ => return Some(k.to_string()),
    };
    Some(canon.to_string())
}

/// Effective map: action_id → serialized chord (override, else default; `""`
/// = unassigned). Built from the config overlays.
pub struct Keymap {
    effective: BTreeMap<String, String>,
}

impl Keymap {
    pub fn build(overrides: &BTreeMap<String, String>) -> Self {
        let mut effective = BTreeMap::new();
        for a in ACTIONS {
            let chord = overrides
                .get(a.id)
                .cloned()
                .unwrap_or_else(|| a.default.to_string());
            effective.insert(a.id.to_string(), chord);
        }
        Keymap { effective }
    }

    /// Effective chord of an action (`""` if unassigned).
    pub fn chord_of(&self, action_id: &str) -> &str {
        self.effective
            .get(action_id)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// Action triggered by this combination (first match in catalogue
    /// order), or `None`.
    pub fn action_for(&self, chord: &Chord) -> Option<&'static str> {
        for a in ACTIONS {
            let eff = self.chord_of(a.id);
            if eff.is_empty() {
                continue;
            }
            if Chord::parse(eff).as_ref() == Some(chord) {
                return Some(a.id);
            }
        }
        None
    }

    /// Another action already using `chord` (other than `action_id`) → conflict.
    pub fn conflict(&self, action_id: &str, chord: &str) -> Option<&'static str> {
        let c = Chord::parse(chord)?;
        for a in ACTIONS {
            if a.id == action_id {
                continue;
            }
            if Chord::parse(self.chord_of(a.id)).as_ref() == Some(&c) {
                return Some(a.id);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_serialize_round_trip() {
        for s in [
            "Ctrl+C",
            "Ctrl+Shift+N",
            "Alt+ArrowUp",
            "F2",
            "Enter",
            "Ctrl+Backslash",
        ] {
            let c = Chord::parse(s).unwrap();
            assert_eq!(c.serialize(), s, "round-trip {s}");
        }
    }

    #[test]
    fn parse_is_order_and_case_insensitive() {
        let a = Chord::parse("shift+CTRL+n").unwrap();
        assert_eq!(a.serialize(), "Ctrl+Shift+N");
        assert!(a.ctrl && a.shift && !a.alt && a.key == "N");
    }

    #[test]
    fn lowercase_letter_normalizes_to_upper() {
        assert_eq!(Chord::parse("Ctrl+c").unwrap().serialize(), "Ctrl+C");
        // Key from an event (a + shift) → "A" + shift.
        let ev = Chord::from_event("a", false, false, true).unwrap();
        assert_eq!(ev.serialize(), "Shift+A");
    }

    #[test]
    fn default_keymap_resolves_known_chords() {
        let km = Keymap::build(&BTreeMap::new());
        assert_eq!(
            km.action_for(&Chord::parse("Ctrl+C").unwrap()),
            Some("copy")
        );
        assert_eq!(
            km.action_for(&Chord::parse("Alt+ArrowUp").unwrap()),
            Some("nav-parent")
        );
        assert_eq!(
            km.action_for(&Chord::parse("ArrowDown").unwrap()),
            Some("cursor-down")
        );
        assert_eq!(
            km.action_for(&Chord::parse("Ctrl+Z").unwrap()),
            Some("restore-trash")
        );
    }

    #[test]
    fn override_takes_precedence_and_frees_default() {
        let mut ov = BTreeMap::new();
        ov.insert("copy".to_string(), "Ctrl+Insert".to_string());
        let km = Keymap::build(&ov);
        assert_eq!(
            km.action_for(&Chord::parse("Ctrl+Insert").unwrap()),
            Some("copy")
        );
        // Overriding a shortcut frees up its default combination.
        assert_eq!(km.action_for(&Chord::parse("Ctrl+C").unwrap()), None);
        assert_eq!(km.chord_of("copy"), "Ctrl+Insert");
    }

    /// An unassigned action (empty override, as produced by "Remove
    /// shortcut") resolves to `""` and can no longer be triggered, even if
    /// it has a default combination.
    #[test]
    fn unassigned_override_is_empty_and_inert() {
        let mut ov = BTreeMap::new();
        ov.insert("open-workspaces".to_string(), String::new());
        let km = Keymap::build(&ov);
        assert_eq!(km.chord_of("open-workspaces"), "");
        // The default chord no longer triggers anything.
        assert_eq!(km.action_for(&Chord::parse("Ctrl+Q").unwrap()), None);
    }

    #[test]
    fn workspaces_panel_has_ctrl_q_by_default() {
        let km = Keymap::build(&BTreeMap::new());
        assert_eq!(km.chord_of("open-workspaces"), "Ctrl+Q");
        assert_eq!(
            km.action_for(&Chord::parse("Ctrl+Q").unwrap()),
            Some("open-workspaces")
        );
    }

    #[test]
    fn reopen_closed_tab_has_firefox_default() {
        let km = Keymap::build(&BTreeMap::new());
        assert_eq!(km.chord_of("tab-reopen-closed"), "Ctrl+Shift+T");
        assert_eq!(
            km.action_for(&Chord::parse("Ctrl+Shift+T").unwrap()),
            Some("tab-reopen-closed")
        );
    }

    #[test]
    fn save_workspace_has_standard_default() {
        let km = Keymap::build(&BTreeMap::new());
        assert_eq!(km.chord_of("save-workspace"), "Ctrl+S");
        assert_eq!(
            km.action_for(&Chord::parse("Ctrl+S").unwrap()),
            Some("save-workspace")
        );
    }

    #[test]
    fn permanent_delete_has_standard_default() {
        let km = Keymap::build(&BTreeMap::new());
        assert_eq!(km.chord_of("delete-permanent"), "Shift+Delete");
        assert_eq!(
            km.action_for(&Chord::parse("Shift+Delete").unwrap()),
            Some("delete-permanent")
        );
    }

    #[test]
    fn restore_trash_has_standard_default() {
        let km = Keymap::build(&BTreeMap::new());
        assert_eq!(km.chord_of("restore-trash"), "Ctrl+Z");
        assert_eq!(
            km.action_for(&Chord::parse("Ctrl+Z").unwrap()),
            Some("restore-trash")
        );
    }

    #[test]
    fn conflict_detects_existing_binding() {
        let km = Keymap::build(&BTreeMap::new());
        // Ctrl+C is already "copy" → conflict for another action.
        assert_eq!(km.conflict("cut", "Ctrl+C"), Some("copy"));
        // A free combination → no conflict.
        assert_eq!(km.conflict("cut", "Ctrl+Insert"), None);
        // An action's own chord isn't a conflict with itself.
        assert_eq!(km.conflict("copy", "Ctrl+C"), None);
    }
}
