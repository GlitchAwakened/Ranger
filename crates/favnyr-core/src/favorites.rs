//! Tree-structured favorites — **editable tree** of containers (favorite
//! folders) and favorites (each pointing to a path), with **shared**
//! storage (a single shared `favorites.toml`, cf. [`crate::paths::favorites_path`]).
//!
//! The core does ONLY data: model, persistence, and tree operations
//! (add / rename / delete / move / collapse), plus a
//! **flattening** step for indented-list rendering on the GUI side. No
//! UI or network dependency.
//!
//! The tree is stored **flat** (`Vec<FavNode>` with `parent` + order = position
//! in the vector among siblings) rather than recursively: TOML
//! serialization stays trivial (a single `[[nodes]]` array) and the GUI
//! rendering consumes a list directly.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::error::Error;

/// A node of the favorites tree. `path = Some` ⇒ **favorite** (leaf);
/// `path = None` ⇒ **container** (favorites folder, can have children).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavNode {
    /// Stable (generated) identifier.
    pub id: String,
    /// Parent (`""` = root).
    #[serde(default)]
    pub parent: String,
    /// Displayed name: favorite's alias, or container's name.
    pub name: String,
    /// Target path. `Some` ⇒ favorite; `None` ⇒ container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Is the container expanded? (persisted UI state; ignored for a favorite).
    #[serde(default)]
    pub expanded: bool,
}

impl FavNode {
    pub fn is_container(&self) -> bool {
        self.path.is_none()
    }
}

/// Favorites store (flat list; sibling order = order in `nodes`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FavStore {
    #[serde(default)]
    pub nodes: Vec<FavNode>,
}

/// Flattened row for GUI rendering (DFS, collapsed containers hide their
/// descendants). `depth` = indentation depth (root = 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlatFav {
    pub id: String,
    pub name: String,
    /// Favorite's path (`""` for a container).
    pub path: String,
    pub is_container: bool,
    pub depth: i32,
    pub expanded: bool,
    /// Does the container have at least one child? (drives chevron display)
    pub has_children: bool,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Stable, unique identifier even for creations within the same
/// nanosecond (e.g. "save all tabs") via a monotonic counter.
fn generate_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        now_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

impl FavStore {
    /// Loads from `path`; empty store if missing/unreadable/corrupted (
    /// favorites must never prevent the app from starting).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Writes the store as TOML (creates the parent folder if needed).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let s = toml::to_string_pretty(self)
            .map_err(|e| Error::Favorites(format!("serialization: {e}")))?;
        std::fs::write(path, s)?;
        Ok(())
    }

    fn exists(&self, id: &str) -> bool {
        id.is_empty() || self.nodes.iter().any(|n| n.id == id)
    }

    /// `true` if `id` designates an existing container (or the root `""`).
    fn is_container_id(&self, id: &str) -> bool {
        id.is_empty() || self.nodes.iter().any(|n| n.id == id && n.is_container())
    }

    /// Adds a container under `parent` (expanded by default). Returns its `id`,
    /// or `None` if `parent` isn't a valid container.
    pub fn add_container(&mut self, parent: &str, name: &str) -> Option<String> {
        if !self.is_container_id(parent) {
            return None;
        }
        let id = generate_id("favc");
        self.nodes.push(FavNode {
            id: id.clone(),
            parent: parent.to_string(),
            name: name.trim().to_string(),
            path: None,
            expanded: true,
        });
        Some(id)
    }

    /// Does container `parent` ALREADY contain a favorite pointing to `path`?
    /// Used to avoid duplicates when saving a tab as a favorite.
    pub fn container_has_path(&self, parent: &str, path: &str) -> bool {
        self.nodes
            .iter()
            .any(|n| n.parent == parent && n.path.as_deref() == Some(path))
    }

    /// Adds a favorite (path) under `parent`. Returns its `id`, or `None` if
    /// `parent` isn't a valid container.
    pub fn add_favorite(&mut self, parent: &str, alias: &str, path: &str) -> Option<String> {
        if !self.is_container_id(parent) {
            return None;
        }
        let id = generate_id("favi");
        self.nodes.push(FavNode {
            id: id.clone(),
            parent: parent.to_string(),
            name: alias.trim().to_string(),
            path: Some(path.to_string()),
            expanded: false,
        });
        Some(id)
    }

    /// Renames a node (alias / container name).
    pub fn rename(&mut self, id: &str, new_name: &str) -> bool {
        if let Some(n) = self.nodes.iter_mut().find(|n| n.id == id) {
            n.name = new_name.trim().to_string();
            true
        } else {
            false
        }
    }

    /// Changes the path of a **favorite** (no effect on a container).
    pub fn set_path(&mut self, id: &str, new_path: &str) -> bool {
        if let Some(n) = self
            .nodes
            .iter_mut()
            .find(|n| n.id == id && n.path.is_some())
        {
            n.path = Some(new_path.to_string());
            true
        } else {
            false
        }
    }

    /// Expands / collapses a container.
    pub fn set_expanded(&mut self, id: &str, expanded: bool) {
        if let Some(n) = self
            .nodes
            .iter_mut()
            .find(|n| n.id == id && n.is_container())
        {
            n.expanded = expanded;
        }
    }

    /// Deletes a node AND all its descendants (for a container).
    pub fn delete(&mut self, id: &str) {
        let mut doomed = vec![id.to_string()];
        let mut i = 0;
        while i < doomed.len() {
            let cur = doomed[i].clone();
            for n in self.nodes.iter().filter(|n| n.parent == cur) {
                doomed.push(n.id.clone());
            }
            i += 1;
        }
        self.nodes.retain(|n| !doomed.contains(&n.id));
    }

    /// `true` if `node` is a (strict) descendant of `ancestor`.
    fn is_descendant(&self, ancestor: &str, node: &str) -> bool {
        let mut cur = node.to_string();
        loop {
            let Some(n) = self.nodes.iter().find(|n| n.id == cur) else {
                return false;
            };
            if n.parent == ancestor {
                return true;
            }
            if n.parent.is_empty() {
                return false;
            }
            cur = n.parent.clone();
        }
    }

    /// Moves `id` under `new_parent`, positioned **before** `before` (a sibling)
    /// or last if `None`. Rejects: nonexistent node, non-container target,
    /// or a cycle (moving a container into one of its own descendants).
    pub fn move_node(&mut self, id: &str, new_parent: &str, before: Option<&str>) -> bool {
        if id == new_parent || !self.exists(id) || !self.is_container_id(new_parent) {
            return false;
        }
        if self.is_descendant(id, new_parent) {
            return false;
        }
        let Some(pos) = self.nodes.iter().position(|n| n.id == id) else {
            return false;
        };
        let mut node = self.nodes.remove(pos);
        node.parent = new_parent.to_string();
        let insert_at = match before {
            Some(b) => self
                .nodes
                .iter()
                .position(|n| n.id == b)
                .unwrap_or(self.nodes.len()),
            None => self.nodes.len(),
        };
        self.nodes.insert(insert_at, node);
        true
    }

    /// Paths of ALL favorites under `container_id` (recursive, display
    /// order) — for "load all child favorites".
    pub fn descendant_paths(&self, container_id: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_paths(container_id, &mut out);
        out
    }

    fn collect_paths(&self, parent: &str, out: &mut Vec<String>) {
        for n in self.nodes.iter().filter(|n| n.parent == parent) {
            match &n.path {
                Some(p) => out.push(p.clone()),
                None => self.collect_paths(&n.id, out),
            }
        }
    }

    /// Path of a favorite by `id` (`None` if container / nonexistent).
    pub fn path_of(&self, id: &str) -> Option<String> {
        self.nodes
            .iter()
            .find(|n| n.id == id)
            .and_then(|n| n.path.clone())
    }

    /// Collapses ALL containers ("Collapse all" action).
    pub fn collapse_all(&mut self) {
        for n in self.nodes.iter_mut().filter(|n| n.path.is_none()) {
            n.expanded = false;
        }
    }

    /// Expands ALL containers ("Expand all" action).
    pub fn expand_all(&mut self) {
        for n in self.nodes.iter_mut().filter(|n| n.path.is_none()) {
            n.expanded = true;
        }
    }

    /// `true` if at least one container **with children** is expanded (drives
    /// the "Collapse all" / "Expand all" toggle).
    pub fn any_expanded(&self) -> bool {
        self.nodes
            .iter()
            .any(|n| n.path.is_none() && n.expanded && self.nodes.iter().any(|c| c.parent == n.id))
    }

    /// List of ALL containers (ignoring collapse state) in tree order:
    /// `(id, name, depth)`. For the popup's destination picker.
    pub fn containers(&self) -> Vec<(String, String, i32)> {
        let mut out = Vec::new();
        self.containers_into("", 0, &mut out);
        out
    }

    fn containers_into(&self, parent: &str, depth: i32, out: &mut Vec<(String, String, i32)>) {
        for n in self
            .nodes
            .iter()
            .filter(|n| n.parent == parent && n.is_container())
        {
            out.push((n.id.clone(), n.name.clone(), depth));
            self.containers_into(&n.id, depth + 1, out);
        }
    }

    /// DFS flattening for rendering: collapsed containers hide their
    /// descendants. Sibling order = order in `nodes`.
    pub fn flatten(&self) -> Vec<FlatFav> {
        let mut out = Vec::new();
        self.flatten_into("", 0, &mut out);
        out
    }

    /// Flattens the VISIBLE descendants of a container (respecting their
    /// collapse state), with a given starting depth. Used to insert a
    /// freshly-expanded subtree WITHOUT rebuilding the whole list (preserves
    /// an in-progress drag on the GUI side).
    pub fn flatten_children(&self, parent_id: &str, base_depth: i32) -> Vec<FlatFav> {
        let mut out = Vec::new();
        self.flatten_into(parent_id, base_depth, &mut out);
        out
    }

    fn flatten_into(&self, parent: &str, depth: i32, out: &mut Vec<FlatFav>) {
        for n in self.nodes.iter().filter(|n| n.parent == parent) {
            let is_container = n.is_container();
            let has_children = is_container && self.nodes.iter().any(|c| c.parent == n.id);
            out.push(FlatFav {
                id: n.id.clone(),
                name: n.name.clone(),
                path: n.path.clone().unwrap_or_default(),
                is_container,
                depth,
                expanded: n.expanded,
                has_children,
            });
            if is_container && n.expanded {
                self.flatten_into(&n.id, depth + 1, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_and_flatten_respects_tree_and_collapse() {
        let mut s = FavStore::default();
        let work = s.add_container("", "Travail").unwrap();
        let _f1 = s.add_favorite(&work, "Projet", "/home/u/proj").unwrap();
        let sub = s.add_container(&work, "Sous").unwrap();
        s.add_favorite(&sub, "Docs", "/home/u/docs").unwrap();

        // Expanded everywhere → 4 rows.
        assert_eq!(s.flatten().len(), 4);
        // Depths.
        let flat = s.flatten();
        assert_eq!(flat[0].depth, 0); // Travail
        assert_eq!(flat[1].depth, 1); // Projet
        assert!(flat[0].has_children);

        // Collapse "Travail" → only it stays visible.
        s.set_expanded(&work, false);
        assert_eq!(s.flatten().len(), 1);
    }

    #[test]
    fn container_has_path_detects_duplicate() {
        let mut s = FavStore::default();
        let c = s.add_container("", "C").unwrap();
        s.add_favorite(&c, "Projet", "/home/u/proj").unwrap();
        assert!(s.container_has_path(&c, "/home/u/proj"));
        // A different path → absent.
        assert!(!s.container_has_path(&c, "/home/u/autre"));
        // Same path but a DIFFERENT container → absent (per-container dedup).
        let c2 = s.add_container("", "C2").unwrap();
        assert!(!s.container_has_path(&c2, "/home/u/proj"));
    }

    #[test]
    fn delete_container_removes_descendants() {
        let mut s = FavStore::default();
        let c = s.add_container("", "C").unwrap();
        let sub = s.add_container(&c, "Sub").unwrap();
        s.add_favorite(&sub, "F", "/x").unwrap();
        assert_eq!(s.nodes.len(), 3);
        s.delete(&c);
        assert!(s.nodes.is_empty());
    }

    #[test]
    fn descendant_paths_collects_recursively_in_order() {
        let mut s = FavStore::default();
        let c = s.add_container("", "C").unwrap();
        s.add_favorite(&c, "A", "/a").unwrap();
        let sub = s.add_container(&c, "Sub").unwrap();
        s.add_favorite(&sub, "B", "/b").unwrap();
        assert_eq!(
            s.descendant_paths(&c),
            vec!["/a".to_string(), "/b".to_string()]
        );
    }

    #[test]
    fn move_rejects_cycle_and_non_container_target() {
        let mut s = FavStore::default();
        let a = s.add_container("", "A").unwrap();
        let b = s.add_container(&a, "B").unwrap();
        let fav = s.add_favorite("", "F", "/f").unwrap();
        // Moving A into B (its descendant) → rejected (cycle).
        assert!(!s.move_node(&a, &b, None));
        // Moving B under a favorite (not a container) → rejected.
        assert!(!s.move_node(&b, &fav, None));
        // Valid move: B to the root.
        assert!(s.move_node(&b, "", None));
        assert_eq!(s.nodes.iter().find(|n| n.id == b).unwrap().parent, "");
    }

    #[test]
    fn favorite_cannot_be_a_parent() {
        let mut s = FavStore::default();
        let fav = s.add_favorite("", "F", "/f").unwrap();
        // Can't add under a favorite.
        assert!(s.add_container(&fav, "X").is_none());
        assert!(s.add_favorite(&fav, "Y", "/y").is_none());
    }

    #[test]
    fn save_load_round_trip() {
        let mut s = FavStore::default();
        let c = s.add_container("", "C").unwrap();
        s.add_favorite(&c, "A", "/a").unwrap();
        let dir = std::env::temp_dir().join(format!("ranger-fav-test-{}", now_nanos()));
        let path = dir.join("favorites.toml");
        s.save(&path).unwrap();
        let loaded = FavStore::load(&path);
        assert_eq!(loaded, s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
