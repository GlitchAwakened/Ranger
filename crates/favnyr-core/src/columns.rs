//! Column model for the list — **order + visibility**, extensible.
//!
//! A view (panel) carries an ORDERED list of `ColumnSpec`: the vector's
//! order is the display order, `visible` drives display, `width` the
//! width. The "name" column is the **anchor**: always in 1st position and
//! always visible (cf. `sanitize`).
//!
//! Identifiers are **codes** (`&str`) rather than an enum, so future column
//! types can be added without breaking serialization. The known list
//! is `KNOWN_COLUMNS`; any unknown code is ignored on load.

use serde::{Deserialize, Serialize};

/// Known columns, in default order. `name` first (anchor).
/// `age` = time since modification, rendered in warm→cool colors.
/// `resolution`/`depth` = image metadata, **hidden by default**.
pub const KNOWN_COLUMNS: &[&str] = &[
    "name",
    "path",
    "size",
    "modified",
    "age",
    "ext",
    "resolution",
    "depth",
];

/// Default visibility of a column. The image metadata columns
/// and the extension (useful for sorting by type) are **hidden** by
/// default.
pub fn default_visible(code: &str) -> bool {
    // `path` is hidden by default: in a view, all rows share the
    // current folder (already shown in the breadcrumb), so the column is
    // redundant most of the time. The user can re-enable it (right-click
    // on the header, or Settings › default columns).
    !matches!(code, "resolution" | "depth" | "ext" | "path")
}

/// Code of the anchor column (never hideable / movable out of 1st pos).
pub const ANCHOR_COLUMN: &str = "name";

/// `true` if `code` designates a known column.
pub fn is_known(code: &str) -> bool {
    KNOWN_COLUMNS.contains(&code)
}

/// Default logical width (px) of a column. ALL columns are
/// resizable on the GUI side: the current width lives in
/// `ColumnSpec.width`, this function only provides the initial default.
pub fn default_width(code: &str) -> f32 {
    match code {
        "name" => 280.0,
        "path" => 220.0,
        "size" => 90.0,
        "modified" => 150.0,
        "ext" => 96.0,
        "age" => 80.0,
        "resolution" => 110.0,
        "depth" => 96.0,
        _ => 120.0,
    }
}

/// Specification of a column in a view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnSpec {
    /// Code (`"name"`, `"size"`, …).
    pub id: String,
    /// Displayed?
    #[serde(default = "ret_true")]
    pub visible: bool,
    /// Logical width (px). `0` → default width (resolved by `sanitize`).
    #[serde(default)]
    pub width: f32,
}

fn ret_true() -> bool {
    true
}

impl ColumnSpec {
    pub fn new(id: &str, visible: bool) -> Self {
        Self {
            id: id.to_string(),
            visible,
            width: default_width(id),
        }
    }
}

/// Default column set: all known ones in order, with their
/// default visibility (`resolution`/`depth` hidden).
pub fn default_columns() -> Vec<ColumnSpec> {
    KNOWN_COLUMNS
        .iter()
        .map(|c| ColumnSpec::new(c, default_visible(c)))
        .collect()
}

/// Normalizes a list of columns:
///   - removes unknown codes and duplicates;
///   - widths `<= 0` → default width;
///   - guarantees the anchor (`name`) is in 1st position AND visible;
///   - **adds** missing known columns (VISIBLE by default, like
///     `default_columns`) so the list ALWAYS contains every known
///     column, and a newly introduced column (e.g. `age`) is active by
///     default, even for an already-saved config.
pub fn sanitize(mut cols: Vec<ColumnSpec>) -> Vec<ColumnSpec> {
    let mut seen = std::collections::HashSet::new();
    cols.retain(|c| is_known(&c.id) && seen.insert(c.id.clone()));
    for c in &mut cols {
        if c.width <= 0.0 {
            c.width = default_width(&c.id);
        }
    }
    // Anchor first + visible.
    match cols.iter().position(|c| c.id == ANCHOR_COLUMN) {
        Some(pos) => {
            let mut anchor = cols.remove(pos);
            anchor.visible = true;
            cols.insert(0, anchor);
        }
        None => cols.insert(0, ColumnSpec::new(ANCHOR_COLUMN, true)),
    }
    // Fills in missing known columns (with their default visibility),
    // in the known order — a newly introduced column that's hidden by
    // default (resolution/depth) stays hidden for an already-saved config.
    for code in KNOWN_COLUMNS {
        if !cols.iter().any(|c| c.id == *code) {
            cols.push(ColumnSpec::new(code, default_visible(code)));
        }
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_columns_order_and_default_visibility() {
        let cols = default_columns();
        assert_eq!(cols.len(), KNOWN_COLUMNS.len());
        assert_eq!(cols[0].id, "name");
        // Standard columns visible; image metadata, extension AND path
        // (redundant with the breadcrumb) hidden by default.
        for c in &cols {
            match c.id.as_str() {
                "resolution" | "depth" | "ext" | "path" => {
                    assert!(!c.visible, "{} should be hidden", c.id)
                }
                _ => assert!(c.visible, "{} should be visible", c.id),
            }
        }
        // The `name` anchor always stays visible, no matter what.
        assert!(cols[0].visible);
    }

    #[test]
    fn sanitize_anchors_name_first_and_visible() {
        // `name` is always moved back to the front and made visible.
        let cols = sanitize(vec![
            ColumnSpec {
                id: "size".into(),
                visible: true,
                width: 0.0,
            },
            ColumnSpec {
                id: "name".into(),
                visible: false,
                width: 0.0,
            },
        ]);
        assert_eq!(cols[0].id, "name");
        assert!(cols[0].visible);
        // default width applied.
        assert_eq!(cols[0].width, default_width("name"));
    }

    #[test]
    fn sanitize_drops_unknown_and_completes_missing() {
        let cols = sanitize(vec![
            ColumnSpec::new("name", true),
            ColumnSpec {
                id: "bogus".into(),
                visible: true,
                width: 50.0,
            },
        ]);
        assert!(!cols.iter().any(|c| c.id == "bogus"));
        // all known columns present.
        for code in KNOWN_COLUMNS {
            assert!(cols.iter().any(|c| c.id == *code), "missing {code}");
        }
        // the missing ones (path/size/modified/age) are restored VISIBLE.
        assert!(cols.iter().find(|c| c.id == "size").unwrap().visible);
    }
}
