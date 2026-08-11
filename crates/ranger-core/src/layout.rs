//! Panel layout tree and nested split management.
//!
//! Ranger displays an arbitrary number of panels organized into nested
//! horizontal/vertical splits, Blender-style. Slint cannot instantiate
//! components **recursively**, so the representation is kept separate from
//! rendering:
//!
//!  1. the layout lives here, in Rust, as a **binary tree** ([`LayoutNode`])
//!     — easy to manipulate (split / merge / resize);
//!  2. at render time, the tree is **flattened** into a list of absolutely
//!     positioned rectangles ([`Layout`]) that the GUI places directly. This
//!     approach avoids any recursion in Slint repeaters.
//!
//! **Leaves** reference a panel by index into
//! [`crate::workspace::WorkspaceState::panels`]: the tree describes *where*
//! each panel goes, while the `Vec<PanelState>` describes *what* each panel
//! contains. This separation lets the GUI consume a flat list of panels.

use serde::{Deserialize, Serialize};

/// Orientation of a split.
///
/// - [`SplitDir::Row`]: children **side by side** (**vertical** separator) —
///   this is the UI's "◧ Side by side".
/// - [`SplitDir::Column`]: children **stacked** (**horizontal** separator) —
///   this is the "⊟ One below another".
///
/// We deliberately ban "horizontal / vertical" from the exposed vocabulary
/// (ambiguous); these internal names describe the **axis along which
/// children are arranged**.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDir {
    /// Children arranged horizontally (left → right).
    Row,
    /// Children stacked vertically (top → bottom).
    Column,
}

/// Node of the layout tree (recursive, **in-memory** representation).
///
/// Directly serializable to TOML: the scalar fields (`dir`, `ratio`) are
/// declared **before** the subtrees (`first`, `second`) to comply with the
/// TOML "values before tables" rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LayoutNode {
    /// Leaf: an actual panel, designated by its index in `panels`.
    Leaf { panel: usize },
    /// Internal node: two children separated according to `dir`, `ratio`
    /// being the share (∈ ]0,1[) allotted to the **first** child.
    Split {
        dir: SplitDir,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

/// Step of a leaf's `panel` index during a traversal (see [`Side`]).
///
/// Path to a node from the root: a sequence of [`Side`]. The root has an
/// empty path.
pub type NodePath = Vec<Side>;

/// Side of a split, used to designate a child within a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// `first` child (left or top).
    First,
    /// `second` child (right or bottom).
    Second,
}

/// Rectangle in pixels (absolute coordinates within the panel container).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Geometry of a leaf panel after flattening.
#[derive(Debug, Clone, PartialEq)]
pub struct PanelGeom {
    /// Index of the panel in `panels`.
    pub panel: usize,
    pub rect: Rect,
}

/// Geometry of a separator (resize handle) after flattening. `path`
/// identifies the corresponding `Split` node, to adjust its `ratio` during a
/// resize drag; `area` is the **total** area that this split divides
/// (needed to convert a pointer position into a ratio local to this split).
#[derive(Debug, Clone, PartialEq)]
pub struct SplitterGeom {
    pub path: NodePath,
    pub dir: SplitDir,
    pub rect: Rect,
    pub area: Rect,
}

/// Result of flattening a [`LayoutNode`] over a given area.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Layout {
    pub panels: Vec<PanelGeom>,
    pub splitters: Vec<SplitterGeom>,
}

impl LayoutNode {
    /// Builds a default tree from a flat list of panels and their
    /// `stretch`: a left-leaning **chain of `Row` splits**, with all panels
    /// side by side. This shape is used as a fallback when a workspace
    /// doesn't contain an explicit tree.
    pub fn row_chain(stretches: &[f32]) -> LayoutNode {
        fn build(idx: usize, stretches: &[f32]) -> LayoutNode {
            let n = stretches.len();
            if idx + 1 >= n {
                return LayoutNode::Leaf { panel: idx };
            }
            let remaining: f32 = stretches[idx..].iter().filter(|s| s.is_finite()).sum();
            let here = stretches[idx].max(0.0);
            let ratio = if remaining > 0.0 {
                (here / remaining).clamp(0.05, 0.95)
            } else {
                0.5
            };
            LayoutNode::Split {
                dir: SplitDir::Row,
                ratio,
                first: Box::new(LayoutNode::Leaf { panel: idx }),
                second: Box::new(build(idx + 1, stretches)),
            }
        }
        if stretches.is_empty() {
            return LayoutNode::Leaf { panel: 0 };
        }
        build(0, stretches)
    }

    /// Panel indices of the leaves, in infix traversal order.
    pub fn leaf_indices(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<usize>) {
        match self {
            LayoutNode::Leaf { panel } => out.push(*panel),
            LayoutNode::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    /// True if the tree covers **exactly** the panels `0..panel_count`, each
    /// exactly once (no missing, duplicated, or out-of-bounds index). Used
    /// as a safeguard before using a tree loaded from disk.
    pub fn is_valid_for(&self, panel_count: usize) -> bool {
        if panel_count == 0 {
            return false;
        }
        let mut seen = vec![false; panel_count];
        for i in self.leaf_indices() {
            if i >= panel_count || seen[i] {
                return false;
            }
            seen[i] = true;
        }
        seen.iter().all(|&b| b)
    }

    /// Mutable reference to the node designated by `path` (root if empty).
    pub fn node_at_mut(&mut self, path: &[Side]) -> Option<&mut LayoutNode> {
        let mut node = self;
        for side in path {
            match node {
                LayoutNode::Split { first, second, .. } => {
                    node = match side {
                        Side::First => first,
                        Side::Second => second,
                    };
                }
                LayoutNode::Leaf { .. } => return None,
            }
        }
        Some(node)
    }

    /// Adjusts the `ratio` of the split located at `path`. No-op if the path
    /// doesn't point to a `Split`. The value is clamped to `[min, 1 - min]`.
    pub fn set_ratio(&mut self, path: &[Side], ratio: f32, min: f32) -> bool {
        if let Some(LayoutNode::Split { ratio: r, .. }) = self.node_at_mut(path) {
            *r = ratio.clamp(min, 1.0 - min);
            true
        } else {
            false
        }
    }

    /// Splits the leaf of panel `panel`: it becomes a `Split`.
    /// `new_first = false` → the existing panel stays first (left/top) and
    /// `new_panel` comes second (right/bottom); `new_first = true` → the
    /// reverse (for a drop on the west/north edge). Returns `false` if no
    /// leaf `panel` exists. First match only (indices are unique).
    pub fn split_leaf(
        &mut self,
        panel: usize,
        dir: SplitDir,
        new_panel: usize,
        ratio: f32,
        new_first: bool,
    ) -> bool {
        match self {
            LayoutNode::Leaf { panel: p } if *p == panel => {
                let kept = LayoutNode::Leaf { panel: *p };
                let fresh = LayoutNode::Leaf { panel: new_panel };
                let (first, second, r) = if new_first {
                    // The new panel takes the `ratio` share (first child).
                    (fresh, kept, ratio)
                } else {
                    (kept, fresh, ratio)
                };
                *self = LayoutNode::Split {
                    dir,
                    ratio: r.clamp(0.05, 0.95),
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            LayoutNode::Leaf { .. } => false,
            LayoutNode::Split { first, second, .. } => {
                first.split_leaf(panel, dir, new_panel, ratio, new_first)
                    || second.split_leaf(panel, dir, new_panel, ratio, new_first)
            }
        }
    }

    /// Removes the leaf of panel `idx`: its parent `Split` is replaced by
    /// the sibling leaf (merge), then all leaf indices `> idx` are
    /// **decremented** to stay consistent with the caller removing
    /// `panels[idx]`. Returns `false` if the tree is reduced to a single
    /// leaf (the last panel is never removed).
    pub fn remove_panel(&mut self, idx: usize) -> bool {
        if matches!(self, LayoutNode::Leaf { .. }) {
            return false;
        }
        if Self::collapse_leaf(self, idx) {
            self.reindex_after_removal(idx);
            true
        } else {
            false
        }
    }

    fn collapse_leaf(node: &mut LayoutNode, idx: usize) -> bool {
        // If one of the direct children is the target leaf, this node
        // becomes the other child.
        let replacement = match node {
            LayoutNode::Split { first, second, .. } => {
                if matches!(**first, LayoutNode::Leaf { panel } if panel == idx) {
                    Some(std::mem::replace(
                        &mut **second,
                        LayoutNode::Leaf { panel: 0 },
                    ))
                } else if matches!(**second, LayoutNode::Leaf { panel } if panel == idx) {
                    Some(std::mem::replace(
                        &mut **first,
                        LayoutNode::Leaf { panel: 0 },
                    ))
                } else {
                    None
                }
            }
            LayoutNode::Leaf { .. } => return false,
        };
        if let Some(repl) = replacement {
            *node = repl;
            return true;
        }
        // Otherwise, descend into the subtrees.
        if let LayoutNode::Split { first, second, .. } = node {
            Self::collapse_leaf(first, idx) || Self::collapse_leaf(second, idx)
        } else {
            false
        }
    }

    fn reindex_after_removal(&mut self, removed: usize) {
        match self {
            LayoutNode::Leaf { panel } => {
                if *panel > removed {
                    *panel -= 1;
                }
            }
            LayoutNode::Split { first, second, .. } => {
                first.reindex_after_removal(removed);
                second.reindex_after_removal(removed);
            }
        }
    }

    /// Flattens the tree over the area `area`, reserving `gap` pixels for
    /// each separator. Returns the absolute position of each panel and each
    /// handle.
    pub fn compute(&self, area: Rect, gap: f32) -> Layout {
        let mut out = Layout::default();
        let mut path = Vec::new();
        self.compute_into(area, gap, &mut path, &mut out);
        out
    }

    fn compute_into(&self, area: Rect, gap: f32, path: &mut NodePath, out: &mut Layout) {
        match self {
            LayoutNode::Leaf { panel } => out.panels.push(PanelGeom {
                panel: *panel,
                rect: area,
            }),
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let r = ratio.clamp(0.0, 1.0);
                match dir {
                    SplitDir::Row => {
                        let avail = (area.w - gap).max(0.0);
                        let w1 = avail * r;
                        let w2 = avail - w1;
                        let a1 = Rect { w: w1, ..area };
                        let sep = Rect {
                            x: area.x + w1,
                            y: area.y,
                            w: gap,
                            h: area.h,
                        };
                        let a2 = Rect {
                            x: area.x + w1 + gap,
                            w: w2,
                            ..area
                        };
                        out.splitters.push(SplitterGeom {
                            path: path.clone(),
                            dir: *dir,
                            rect: sep,
                            area,
                        });
                        path.push(Side::First);
                        first.compute_into(a1, gap, path, out);
                        path.pop();
                        path.push(Side::Second);
                        second.compute_into(a2, gap, path, out);
                        path.pop();
                    }
                    SplitDir::Column => {
                        let avail = (area.h - gap).max(0.0);
                        let h1 = avail * r;
                        let h2 = avail - h1;
                        let a1 = Rect { h: h1, ..area };
                        let sep = Rect {
                            x: area.x,
                            y: area.y + h1,
                            w: area.w,
                            h: gap,
                        };
                        let a2 = Rect {
                            y: area.y + h1 + gap,
                            h: h2,
                            ..area
                        };
                        out.splitters.push(SplitterGeom {
                            path: path.clone(),
                            dir: *dir,
                            rect: sep,
                            area,
                        });
                        path.push(Side::First);
                        first.compute_into(a1, gap, path, out);
                        path.pop();
                        path.push(Side::Second);
                        second.compute_into(a2, gap, path, out);
                        path.pop();
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 0.5, "expected ~{b}, got {a}");
    }

    fn area() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            w: 1000.0,
            h: 600.0,
        }
    }

    #[test]
    fn single_leaf_fills_area() {
        let tree = LayoutNode::Leaf { panel: 0 };
        let l = tree.compute(area(), 8.0);
        assert_eq!(l.panels.len(), 1);
        assert!(l.splitters.is_empty());
        assert_eq!(l.panels[0].rect, area());
    }

    #[test]
    fn row_split_halves_minus_gap() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Leaf { panel: 1 }),
        };
        let l = tree.compute(area(), 8.0);
        assert_eq!(l.panels.len(), 2);
        assert_eq!(l.splitters.len(), 1);
        // avail = 992, half = 496
        approx(l.panels[0].rect.w, 496.0);
        approx(l.panels[1].rect.w, 496.0);
        approx(l.panels[0].rect.h, 600.0);
        // no overlap, separator between the two
        approx(l.splitters[0].rect.x, 496.0);
        approx(l.splitters[0].rect.w, 8.0);
        approx(l.panels[1].rect.x, 504.0);
        assert_eq!(l.splitters[0].dir, SplitDir::Row);
    }

    #[test]
    fn column_split_stacks() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Column,
            ratio: 0.25,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Leaf { panel: 1 }),
        };
        let l = tree.compute(area(), 8.0);
        // avail = 592, first = 148
        approx(l.panels[0].rect.h, 148.0);
        approx(l.panels[1].rect.h, 444.0);
        approx(l.panels[0].rect.w, 1000.0);
        approx(l.splitters[0].rect.y, 148.0);
        approx(l.splitters[0].rect.h, 8.0);
        assert_eq!(l.splitters[0].dir, SplitDir::Column);
    }

    #[test]
    fn nested_split_geometry() {
        // Root Row 0.5; right child re-split into Column 0.5.
        let tree = LayoutNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Split {
                dir: SplitDir::Column,
                ratio: 0.5,
                first: Box::new(LayoutNode::Leaf { panel: 1 }),
                second: Box::new(LayoutNode::Leaf { panel: 2 }),
            }),
        };
        let l = tree.compute(area(), 8.0);
        assert_eq!(l.panels.len(), 3);
        assert_eq!(l.splitters.len(), 2);
        // panel 0 = left half, full height
        let p0 = l.panels.iter().find(|p| p.panel == 0).unwrap();
        approx(p0.rect.w, 496.0);
        approx(p0.rect.h, 600.0);
        // panels 1 and 2 = right half, stacked (≈ (600-8)/2)
        let p1 = l.panels.iter().find(|p| p.panel == 1).unwrap();
        let p2 = l.panels.iter().find(|p| p.panel == 2).unwrap();
        approx(p1.rect.w, 496.0);
        approx(p1.rect.h, 296.0);
        approx(p2.rect.h, 296.0);
        approx(p1.rect.x, 504.0);
    }

    #[test]
    fn row_chain_matches_stretches() {
        // 3 panels with stretch 2,1,1 → the first one takes half.
        let tree = LayoutNode::row_chain(&[2.0, 1.0, 1.0]);
        assert_eq!(tree.leaf_indices(), vec![0, 1, 2]);
        let l = tree.compute(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 408.0,
                h: 100.0,
            },
            8.0,
        );
        // total stretch = 4; gaps = 2*8 = 16; remainder 392.
        // p0 = 2/4 ≈ 196 (at the first level ratio=0.5 over 400 available)
        let p0 = l.panels.iter().find(|p| p.panel == 0).unwrap();
        approx(p0.rect.w, 200.0);
    }

    #[test]
    fn validity_checks() {
        let good = LayoutNode::row_chain(&[1.0, 1.0, 1.0]);
        assert!(good.is_valid_for(3));
        assert!(!good.is_valid_for(2)); // index 2 out of bounds
        assert!(!good.is_valid_for(4)); // panel 3 missing

        let dup = LayoutNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Leaf { panel: 0 }),
        };
        assert!(!dup.is_valid_for(1));
    }

    #[test]
    fn set_ratio_via_path() {
        let mut tree = LayoutNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Split {
                dir: SplitDir::Column,
                ratio: 0.5,
                first: Box::new(LayoutNode::Leaf { panel: 1 }),
                second: Box::new(LayoutNode::Leaf { panel: 2 }),
            }),
        };
        // root
        assert!(tree.set_ratio(&[], 0.7, 0.05));
        // Column node = Second child of the root
        assert!(tree.set_ratio(&[Side::Second], 0.3, 0.05));
        // path to a leaf → failure
        assert!(!tree.set_ratio(&[Side::First], 0.5, 0.05));

        if let LayoutNode::Split { ratio, second, .. } = &tree {
            approx(*ratio, 0.7);
            if let LayoutNode::Split { ratio: r2, .. } = second.as_ref() {
                approx(*r2, 0.3);
            } else {
                panic!("expected Split");
            }
        } else {
            panic!("expected Split");
        }
    }

    #[test]
    fn ratio_clamped() {
        let mut tree = LayoutNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Leaf { panel: 1 }),
        };
        tree.set_ratio(&[], 0.001, 0.05);
        if let LayoutNode::Split { ratio, .. } = &tree {
            approx(*ratio, 0.05);
        }
        tree.set_ratio(&[], 0.999, 0.05);
        if let LayoutNode::Split { ratio, .. } = &tree {
            approx(*ratio, 0.95);
        }
    }

    #[test]
    fn split_leaf_creates_split() {
        let mut tree = LayoutNode::Leaf { panel: 0 };
        assert!(tree.split_leaf(0, SplitDir::Row, 1, 0.5, false));
        assert!(tree.is_valid_for(2));
        assert_eq!(tree.leaf_indices(), vec![0, 1]);
        assert!(matches!(
            tree,
            LayoutNode::Split {
                dir: SplitDir::Row,
                ..
            }
        ));

        // Re-split panel 1 (nested leaf) into Column.
        assert!(tree.split_leaf(1, SplitDir::Column, 2, 0.5, false));
        assert!(tree.is_valid_for(3));
        assert_eq!(tree.leaf_indices(), vec![0, 1, 2]);

        // Non-existent panel → false.
        assert!(!tree.split_leaf(9, SplitDir::Row, 3, 0.5, false));
    }

    #[test]
    fn split_leaf_new_first_puts_new_panel_first() {
        let mut tree = LayoutNode::Leaf { panel: 0 };
        assert!(tree.split_leaf(0, SplitDir::Row, 1, 0.5, true));
        // new_first → the new panel (1) is the FIRST child.
        assert_eq!(tree.leaf_indices(), vec![1, 0]);
    }

    #[test]
    fn remove_panel_collapses_and_reindexes() {
        // Chain of 3 panels: leaves [0,1,2].
        let mut tree = LayoutNode::row_chain(&[1.0, 1.0, 1.0]);
        // Remove panel 1 → [0, 2] remain, reindexed as [0, 1].
        assert!(tree.remove_panel(1));
        assert!(tree.is_valid_for(2));
        assert_eq!(tree.leaf_indices(), vec![0, 1]);

        // Remove again → 1 leaf.
        assert!(tree.remove_panel(0));
        assert!(tree.is_valid_for(1));
        assert_eq!(tree.leaf_indices(), vec![0]);

        // Last panel: refused.
        assert!(!tree.remove_panel(0));
    }

    #[test]
    fn remove_panel_keeps_sibling_subtree() {
        // Row { 0, Column { 1, 2 } }; removing 0 → only Column{1,2} remains,
        // reindexed as Column{0,1}.
        let mut tree = LayoutNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Split {
                dir: SplitDir::Column,
                ratio: 0.5,
                first: Box::new(LayoutNode::Leaf { panel: 1 }),
                second: Box::new(LayoutNode::Leaf { panel: 2 }),
            }),
        };
        assert!(tree.remove_panel(0));
        assert!(tree.is_valid_for(2));
        assert_eq!(tree.leaf_indices(), vec![0, 1]);
        assert!(matches!(
            tree,
            LayoutNode::Split {
                dir: SplitDir::Column,
                ..
            }
        ));
    }

    #[test]
    fn splitter_carries_area() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Leaf { panel: 1 }),
        };
        let l = tree.compute(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
            },
            0.0,
        );
        // Full fractional area, separator halfway.
        approx(l.splitters[0].area.w, 1.0);
        approx(l.splitters[0].rect.x, 0.5);
    }

    #[test]
    fn toml_round_trip() {
        let tree = LayoutNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf { panel: 0 }),
            second: Box::new(LayoutNode::Split {
                dir: SplitDir::Column,
                ratio: 0.6,
                first: Box::new(LayoutNode::Leaf { panel: 1 }),
                second: Box::new(LayoutNode::Leaf { panel: 2 }),
            }),
        };
        let s = toml::to_string_pretty(&tree).expect("serialize");
        let back: LayoutNode = toml::from_str(&s).expect("parse");
        assert_eq!(back, tree);
    }
}
