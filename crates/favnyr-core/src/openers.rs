//! "Open with" — model for **openers**: with which program, and
//! how, to open a file. **Cross-platform, with NO OS or UI dependency.**
//!
//! The core does ONLY: modeling, TOML persistence (dedicated `openers.toml`, see
//! [`crate::paths::openers_path`]), **tag substitution**, and **validation**.
//! The process `spawn` and the native dialog live on the GUI side (`actions.rs`).
//!
//! Security principle: arguments are a **list** (`Vec<String>`), one
//! element = one argument. Substitution is token-by-token → a path with
//! spaces/metacharacters stays ONE opaque argument. The GUI passes the `argv`
//! as-is to `std::process::Command` (never a shell, never concatenation).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::error::Error;

/// Built-in icon attached to a command created from a ready-made recipe.
///
/// The value is persisted with the opener rather than inferred from its
/// executable path: users remain free to replace `7z` or `tar` with a wrapper
/// script without losing the recipe's visual identity. `None` keeps commands
/// created manually and older `openers.toml` files unchanged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OpenerIcon {
    #[default]
    None,
    SevenZip,
    Archive,
    Email,
    Device,
}

impl OpenerIcon {
    pub fn is_none(&self) -> bool {
        *self == Self::None
    }

    /// Stable numeric value passed to Slint. Keep the matching UI component
    /// mapping in sync when adding a variant.
    pub fn as_i32(self) -> i32 {
        match self {
            Self::None => 0,
            Self::SevenZip => 1,
            Self::Archive => 2,
            Self::Email => 3,
            Self::Device => 4,
        }
    }

    pub fn from_i32(value: i32) -> Self {
        match value {
            1 => Self::SevenZip,
            2 => Self::Archive,
            3 => Self::Email,
            4 => Self::Device,
            _ => Self::None,
        }
    }
}

/// An "opener": a program + an argument template (with tags).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Opener {
    /// Stable identifier (generated).
    pub id: String,
    /// Displayed label ("GIMP", "VS Code"…).
    pub label: String,
    /// ABSOLUTE path of the executable (never interpreted by a shell). Can be
    /// empty if `assoc` is set (OS app with no directly launchable exe).
    pub program: String,
    /// Key of an OS association handler (Windows: ProgID/AUMID of a
    /// UWP/Store app; Linux: `.desktop` id) for apps WITHOUT a classic exe.
    /// If set, launching goes through the OS (`IAssocHandler::Invoke` / `.desktop`)
    /// rather than `Command` — see `ranger-gui::actions::run_opener`.
    #[serde(default)]
    pub assoc: Option<String>,
    /// Visual family inherited from a ready-made recipe. It has no influence
    /// on execution and is omitted for ordinary custom commands.
    #[serde(default, skip_serializing_if = "OpenerIcon::is_none")]
    pub icon: OpenerIcon,
    /// Argument templates BEFORE substitution (one element = one argument).
    /// Empty → `{file}` added at execution time.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extensions (no dot, lowercase) for which THIS opener is the
    /// default **within Ranger** (double-click / Enter), without touching the OS.
    #[serde(default)]
    pub default_exts: Vec<String>,
    /// Extensions (no dot, lowercase) **learned through use**: each time
    /// the opener is launched on a file, its extension is recorded here. Used
    /// to only offer, in the "Open with" flyout, programs that are ACTUALLY
    /// suited to the file's extension (no more Word suggested for a .mp4).
    #[serde(default)]
    pub used_exts: Vec<String>,
    /// Usage counter (MRU for "suggested applications").
    #[serde(default)]
    pub use_count: u32,
    /// Last used (epoch seconds; breaks ties in the MRU). 0 = never.
    #[serde(default)]
    pub last_used: u64,
    /// Launch the program as ADMINISTRATOR (UAC elevation via the
    /// "runas" verb) — **Windows only**. Ignored on other OSes (elevating
    /// a GUI app is not a standard mechanism there).
    #[serde(default)]
    pub elevated: bool,
    /// Pinning to the views' CONTEXT MENU: bitmask —
    /// 1 = right-click on a FILE, 2 = on a FOLDER, 4 = on the view's
    /// BACKGROUND (empty area; the command then targets the current folder).
    /// 0 = absent (default). Enables entries like "Git Bash here" without a
    /// shell extension.
    #[serde(default)]
    pub ctx_menu: u8,
    /// Extensions (no dot, lowercase) the FILE entry is restricted to, or
    /// [`CTX_EXT_ALL`] for every file. Only meaningful with [`CTX_FILE`] —
    /// folders and the view background have no extension.
    ///
    /// Empty means no restriction, so commands saved before this field existed
    /// keep showing up everywhere, as they did.
    ///
    /// Exists because an "Extract here" command is meaningless on a text file,
    /// yet used to appear on right-click for every file.
    #[serde(default)]
    pub ctx_exts: Vec<String>,
}

/// Bits of [`Opener::ctx_menu`].
pub const CTX_FILE: u8 = 1;
pub const CTX_DIR: u8 = 2;
pub const CTX_BACKGROUND: u8 = 4;

/// Wildcard accepted in [`Opener::ctx_exts`]: show the entry on every file.
pub const CTX_EXT_ALL: &str = "*";

/// Tags describing ONE file. Their presence is what makes a command run once
/// per selected item. Order = order of the GUI insertion buttons.
pub const TAGS: &[&str] = &["file", "dir", "dirname", "name", "stem", "ext", "uri"];

/// Tag standing for the WHOLE selection. Unlike the tags above it does not
/// describe one file, so it does the opposite: the command runs a single time
/// and this tag expands, in place, into one argument per selected path.
///
/// It is what makes "compress everything into one archive" expressible:
/// `a {dir}/{dirname}.7z {files}`. It must be an argument on its own — anywhere
/// else it stays literal, like any unrecognized tag.
pub const LIST_TAG: &str = "{files}";

/// Like [`LIST_TAG`] but expanding to bare NAMES instead of full paths.
///
/// Exists for tools that take a base directory and then relative names —
/// `tar -C <dir> a.txt b.txt`. Handed absolute paths, `tar` stores the whole
/// tree that leads to each file, so the archive holds
/// `home/user/Docs/a.txt` rather than `a.txt`.
///
/// Pair it with `{dir}`, which in a batch run designates the folder the whole
/// selection shares.
pub const LIST_NAMES_TAG: &str = "{names}";

/// Context of a target file (all tag values pre-computed).
#[derive(Debug, Clone, Default)]
pub struct TagContext {
    pub file: String,
    pub dir: String,
    /// Name alone of the containing folder — `{dir}` gives its full path.
    /// Lets an archive be named after the folder rather than after one of the
    /// files it holds, which is what archivers do for a multiple selection.
    pub dirname: String,
    pub name: String,
    pub stem: String,
    pub ext: String,
    pub uri: String,
}

impl TagContext {
    /// Builds the context from a path (absolute preferred).
    pub fn from_path(path: &Path) -> Self {
        let file = path.display().to_string();
        let parent = path.parent();
        let dir = parent.map(|p| p.display().to_string()).unwrap_or_default();
        let dirname = parent
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = path
            .extension()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        let uri = file_uri(&file);
        Self {
            file,
            dir,
            dirname,
            name,
            stem,
            ext,
            uri,
        }
    }
}

/// Coarse `file://` URI (sufficient for editors/browsers). Encodes
/// the space; keeps other characters as-is (common case). Windows: `\`→`/`.
fn file_uri(path: &str) -> String {
    let norm = path.replace('\\', "/");
    let enc = norm.replace(' ', "%20");
    if enc.starts_with('/') {
        format!("file://{enc}")
    } else {
        // Windows path "C:/…" → file:///C:/…
        format!("file:///{enc}")
    }
}

/// Substitutes `{...}` tags in a template. `{{`/`}}` = literals; an
/// unknown tag is left as-is (never fails).
fn substitute(template: &str, ctx: &TagContext) -> String {
    let mut out = String::with_capacity(template.len());
    let mut it = template.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '{' if it.peek() == Some(&'{') => {
                it.next();
                out.push('{');
            }
            '}' if it.peek() == Some(&'}') => {
                it.next();
                out.push('}');
            }
            '{' => {
                let mut tag = String::new();
                let mut closed = false;
                for nc in it.by_ref() {
                    if nc == '}' {
                        closed = true;
                        break;
                    }
                    tag.push(nc);
                }
                match (closed, tag.as_str()) {
                    (true, "file") => out.push_str(&ctx.file),
                    (true, "dir") => out.push_str(&ctx.dir),
                    (true, "dirname") => out.push_str(&ctx.dirname),
                    (true, "name") => out.push_str(&ctx.name),
                    (true, "stem") => out.push_str(&ctx.stem),
                    (true, "ext") => out.push_str(&ctx.ext),
                    (true, "uri") => out.push_str(&ctx.uri),
                    // Unknown / unclosed → left literal.
                    (true, other) => {
                        out.push('{');
                        out.push_str(other);
                        out.push('}');
                    }
                    (false, rest) => {
                        out.push('{');
                        out.push_str(rest);
                    }
                }
            }
            other => out.push(other),
        }
    }
    out
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn generate_id() -> String {
    format!(
        "op-{}-{}",
        now_secs(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

impl Opener {
    /// Does at least one argument contain a tag (i.e. depend on the file)?
    ///
    /// `{files}` is deliberately NOT one of them: it stands for the whole
    /// selection, so it means the opposite — a single run. (It also contains no
    /// `{file}` substring, the closing brace differing, so it cannot be counted
    /// here by accident.)
    pub fn has_tag(&self) -> bool {
        self.args
            .iter()
            .any(|a| TAGS.iter().any(|t| a.contains(&format!("{{{t}}}"))))
    }

    /// Does this command apply to a file whose extension is `ext` (no dot,
    /// lowercase)? An empty list and the `*` wildcard both mean "every file";
    /// an extensionless file therefore only matches those two.
    ///
    /// Only consulted for the FILE entry — see [`Opener::ctx_exts`].
    pub fn matches_ctx_ext(&self, ext: &str) -> bool {
        self.ctx_exts.is_empty()
            || self
                .ctx_exts
                .iter()
                .any(|e| e == CTX_EXT_ALL || e.eq_ignore_ascii_case(ext))
    }

    /// Does the template expand the whole selection in place, i.e. run ONCE
    /// however many items are selected? True when `{files}` stands alone as an
    /// argument — see [`LIST_TAG`].
    pub fn expands_list(&self) -> bool {
        self.args
            .iter()
            .any(|a| a == LIST_TAG || a == LIST_NAMES_TAG)
    }

    /// Substituted `argv` for ONE file. If there is no tag, `{file}` is
    /// implicit (the path is appended at the end).
    pub fn render_for_file(&self, ctx: &TagContext) -> Vec<String> {
        if self.args.is_empty() || !self.has_tag() {
            let mut v: Vec<String> = self.args.iter().map(|a| substitute(a, ctx)).collect();
            v.push(ctx.file.clone());
            v
        } else {
            self.args.iter().map(|a| substitute(a, ctx)).collect()
        }
    }

    /// Substituted `argv` for a SINGLE run covering the whole selection.
    ///
    /// Each standalone `{files}` becomes one argument per path, keeping its
    /// position in the command — so the list can sit before a trailing option,
    /// unlike the historical fallback which always appended paths at the end.
    /// The remaining tags describe `ctx`, which the caller builds from the
    /// first selected item; `{dir}` and `{dirname}` therefore designate the
    /// folder the whole selection shares.
    /// `paths` are the selected items; each expands to its full path for
    /// [`LIST_TAG`] and to its bare name for [`LIST_NAMES_TAG`].
    pub fn render_for_batch(&self, ctx: &TagContext, paths: &[&Path]) -> Vec<String> {
        let mut out = Vec::with_capacity(self.args.len() + paths.len());
        for arg in &self.args {
            if arg == LIST_TAG {
                out.extend(paths.iter().map(|p| p.display().to_string()));
            } else if arg == LIST_NAMES_TAG {
                out.extend(paths.iter().map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| p.display().to_string())
                }));
            } else {
                out.push(substitute(arg, ctx));
            }
        }
        out
    }
}

/// Flat store of openers (the Vec's order = base display order).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenerStore {
    #[serde(default)]
    pub openers: Vec<Opener>,
}

impl OpenerStore {
    /// Loads from `path`; empty store if missing/unreadable (never fatal).
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Writes the store as TOML (creates the parent folder).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let s = toml::to_string_pretty(self)
            .map_err(|e| Error::Openers(format!("serialization: {e}")))?;
        std::fs::write(path, s)?;
        Ok(())
    }

    /// Adds an opener (normalized label/program/args). Returns its `id`.
    pub fn add(&mut self, label: &str, program: &str, args: Vec<String>) -> String {
        let id = generate_id();
        self.openers.push(Opener {
            id: id.clone(),
            label: label.trim().to_string(),
            program: program.trim().to_string(),
            assoc: None,
            icon: OpenerIcon::None,
            args,
            default_exts: Vec::new(),
            used_exts: Vec::new(),
            use_count: 0,
            last_used: 0,
            elevated: false,
            ctx_menu: 0,
            ctx_exts: Vec::new(),
        });
        id
    }

    /// Adds an opener based on an **OS association handler** (app without
    /// a classic exe: Windows UWP/Store, Linux `.desktop`). Launched via the OS.
    /// Returns its `id`.
    pub fn add_assoc(&mut self, label: &str, assoc: &str) -> String {
        let id = generate_id();
        self.openers.push(Opener {
            id: id.clone(),
            label: label.trim().to_string(),
            program: String::new(),
            assoc: Some(assoc.trim().to_string()),
            icon: OpenerIcon::None,
            args: Vec::new(),
            default_exts: Vec::new(),
            used_exts: Vec::new(),
            use_count: 0,
            last_used: 0,
            elevated: false, // an OS association app is never launched elevated
            ctx_menu: 0,
            ctx_exts: Vec::new(),
        });
        id
    }

    pub fn get(&self, id: &str) -> Option<&Opener> {
        self.openers.iter().find(|o| o.id == id)
    }

    /// Updates an opener's label/program/args.
    pub fn update(&mut self, id: &str, label: &str, program: &str, args: Vec<String>) -> bool {
        if let Some(o) = self.openers.iter_mut().find(|o| o.id == id) {
            o.label = label.trim().to_string();
            o.program = program.trim().to_string();
            o.args = args;
            true
        } else {
            false
        }
    }

    /// (Un)marks opener `id` as "launch as administrator" (Windows).
    /// A separate setting, like `set_used_exts` / `set_default_ext` (the UI
    /// applies it after `add`/`update`). On other OSes the flag is simply
    /// ignored at launch.
    pub fn set_elevated(&mut self, id: &str, elevated: bool) -> bool {
        if let Some(o) = self.openers.iter_mut().find(|o| o.id == id) {
            o.elevated = elevated;
            true
        } else {
            false
        }
    }

    /// Attaches the visual family selected by a ready-made recipe.
    pub fn set_icon(&mut self, id: &str, icon: OpenerIcon) -> bool {
        if let Some(o) = self.openers.iter_mut().find(|o| o.id == id) {
            o.icon = icon;
            true
        } else {
            false
        }
    }

    /// Sets the context-menu pinning mask (bits [`CTX_FILE`] /
    /// [`CTX_DIR`] / [`CTX_BACKGROUND`]). Same style as `set_elevated`.
    pub fn set_ctx_menu(&mut self, id: &str, mask: u8) -> bool {
        if let Some(o) = self.openers.iter_mut().find(|o| o.id == id) {
            o.ctx_menu = mask;
            true
        } else {
            false
        }
    }

    /// Restricts the FILE context-menu entry to `exts` (no dot, lowercase).
    /// Empty, or a list holding [`CTX_EXT_ALL`], means every file.
    pub fn set_ctx_exts(&mut self, id: &str, exts: &[String]) -> bool {
        if let Some(o) = self.openers.iter_mut().find(|o| o.id == id) {
            o.ctx_exts = exts.to_vec();
            true
        } else {
            false
        }
    }

    /// Openers pinned for a given context (`ctx_menu` bit), in the
    /// store's display order.
    pub fn for_context(&self, bit: u8) -> Vec<&Opener> {
        self.openers
            .iter()
            .filter(|o| o.ctx_menu & bit != 0)
            .collect()
    }

    /// Duplicates opener `id`: the clone is inserted RIGHT AFTER the original,
    /// with a new `id`, a usage counter reset to zero, and EMPTY `default_exts`
    /// (only one "Ranger default" opener per extension — the clone doesn't
    /// steal the status). `used_exts` and `elevated` are kept. Returns the new
    /// `id`. Used by the "Duplicate" button (e.g. same command as admin /
    /// non-admin).
    pub fn duplicate(&mut self, id: &str) -> Option<String> {
        let pos = self.openers.iter().position(|o| o.id == id)?;
        let src = &self.openers[pos];
        let clone = Opener {
            id: generate_id(),
            label: src.label.clone(),
            program: src.program.clone(),
            assoc: src.assoc.clone(),
            icon: src.icon,
            args: src.args.clone(),
            default_exts: Vec::new(),
            used_exts: src.used_exts.clone(),
            use_count: 0,
            last_used: 0,
            elevated: src.elevated,
            ctx_menu: src.ctx_menu, // context-menu pinning follows the copy
            ctx_exts: src.ctx_exts.clone(), // …and so does its extension filter
        };
        let new_id = clone.id.clone();
        self.openers.insert(pos + 1, clone);
        Some(new_id)
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let n = self.openers.len();
        self.openers.retain(|o| o.id != id);
        self.openers.len() != n
    }

    /// Moves opener `id` right BEFORE `before` (or to the end if `None`).
    pub fn move_before(&mut self, id: &str, before: Option<&str>) -> bool {
        let Some(pos) = self.openers.iter().position(|o| o.id == id) else {
            return false;
        };
        let o = self.openers.remove(pos);
        let at = match before {
            Some(b) => self
                .openers
                .iter()
                .position(|x| x.id == b)
                .unwrap_or(self.openers.len()),
            None => self.openers.len(),
        };
        self.openers.insert(at, o);
        true
    }

    /// Increments the usage counter (called after a successful launch) and
    /// timestamps it. `last_used` is forced STRICTLY greater than all others
    /// → the most recently used exe ALWAYS moves to the top of suggestions,
    /// even if several launches fall within the same second (`now_secs`
    /// resolution). `ext` (if provided) is **learned**: the opener will from
    /// then on be offered for this extension in the "Open with" flyout.
    pub fn record_use(&mut self, id: &str, ext: Option<&str>) {
        let top = self.openers.iter().map(|o| o.last_used).max().unwrap_or(0);
        let stamp = now_secs().max(top.saturating_add(1));
        if let Some(o) = self.openers.iter_mut().find(|o| o.id == id) {
            o.use_count = o.use_count.saturating_add(1);
            o.last_used = stamp;
            if let Some(ext) = ext {
                let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
                if !ext.is_empty() && !o.used_exts.contains(&ext) {
                    o.used_exts.push(ext);
                }
            }
        }
    }

    /// Sets (or unsets) THIS opener as the Ranger default for `ext`. Only one
    /// opener per extension: `ext` is removed from all the others.
    pub fn set_default_ext(&mut self, id: &str, ext: &str, enabled: bool) {
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        if ext.is_empty() {
            return;
        }
        for o in self.openers.iter_mut() {
            o.default_exts
                .retain(|e| e != &ext || (o.id == id && enabled));
            if o.id == id && enabled && !o.default_exts.contains(&ext) {
                o.default_exts.push(ext.clone());
            }
        }
    }

    /// Replaces opener `id`'s list of **learned** extensions (`used_exts`)
    /// with `exts` (normalized: no dot, lowercase, deduplicated). MANUAL
    /// edit from Settings: the opener will from then on be offered in the
    /// "Open with" flyout for these extensions (see `suggested_for_ext`).
    /// Unlike `default_exts`, there is NO cross-opener uniqueness
    /// constraint — several programs can be suggested for the same extension.
    pub fn set_used_exts(&mut self, id: &str, exts: &[String]) {
        if let Some(o) = self.openers.iter_mut().find(|o| o.id == id) {
            let mut norm: Vec<String> = Vec::new();
            for e in exts {
                let e = e.trim().trim_start_matches('.').to_ascii_lowercase();
                if !e.is_empty() && !norm.contains(&e) {
                    norm.push(e);
                }
            }
            o.used_exts = norm;
        }
    }

    /// Opener set as the Ranger default for `ext` (the first one found), if any.
    pub fn default_for(&self, ext: &str) -> Option<&Opener> {
        let ext = ext.to_ascii_lowercase();
        self.openers
            .iter()
            .find(|o| o.default_exts.iter().any(|e| e == &ext))
    }

    /// "Suggested applications" list (the "Open with" flyout) sorted by
    /// **recency of use** (MRU): the most recently used exe moves to the TOP.
    /// Openers never used (`last_used == 0`) stay at the bottom, in their
    /// manual order (STABLE sort). The Settings list, meanwhile, keeps the
    /// manual order.
    pub fn suggested(&self, limit: usize) -> Vec<&Opener> {
        let mut v: Vec<&Opener> = self.openers.iter().collect();
        v.sort_by_key(|o| std::cmp::Reverse(o.last_used));
        v.truncate(limit);
        v
    }

    /// Suggestions ADAPTED to the `ext` extension: only openers
    /// **set** (`default_exts`) OR **already used** (`used_exts`) for this
    /// extension, sorted by recency (MRU). Avoids mixing in unrelated
    /// programs (e.g. Word suggested for a `.mp4`). If `ext` is empty (file
    /// with no extension), falls back to the global MRU.
    ///
    /// An empty list is possible (extension never opened) → the GUI keeps the
    /// "Choose an application…" entry, which itself lists the programs
    /// recommended by the OS; choosing one there FEEDS the learning (via
    /// `record_use`).
    pub fn suggested_for_ext(&self, ext: &str, limit: usize) -> Vec<&Opener> {
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        if ext.is_empty() {
            return self.suggested(limit);
        }
        let mut v: Vec<&Opener> = self
            .openers
            .iter()
            .filter(|o| {
                o.default_exts.iter().any(|e| e == &ext) || o.used_exts.iter().any(|e| e == &ext)
            })
            .collect();
        v.sort_by_key(|o| std::cmp::Reverse(o.last_used));
        v.truncate(limit);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx(p: &str) -> TagContext {
        TagContext::from_path(&PathBuf::from(p))
    }

    #[test]
    fn suggested_sorts_by_recency_and_keeps_unused_in_insertion_order() {
        // Guards the `sort_by` → `sort_by_key(Reverse(..))` rewrite (Clippy
        // `unnecessary_sort_by`, Rust 1.97): most-recent first, and — the part
        // an unstable or wrongly-directed sort would silently break — openers
        // never used (`last_used == 0`) keep their ORIGINAL relative order.
        let mut s = OpenerStore::default();
        let old = s.add("Old", "/bin/old", vec![]);
        let never1 = s.add("Never1", "/bin/n1", vec![]);
        let recent = s.add("Recent", "/bin/recent", vec![]);
        let never2 = s.add("Never2", "/bin/n2", vec![]);
        // Set directly rather than via `record_use` (which stamps the current
        // second): a deterministic gap removes any dependence on how fast the
        // two calls would run.
        s.openers
            .iter_mut()
            .find(|o| o.id == old)
            .unwrap()
            .last_used = 100;
        s.openers
            .iter_mut()
            .find(|o| o.id == recent)
            .unwrap()
            .last_used = 200;

        let ids: Vec<&str> = s.suggested(10).iter().map(|o| o.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                recent.as_str(),
                old.as_str(),
                never1.as_str(),
                never2.as_str()
            ],
            "recent first, then the never-used pair in the order they were added"
        );
    }

    #[test]
    fn elevated_defaults_false_and_round_trips() {
        // New opener: not elevated by default.
        let mut s = OpenerStore::default();
        let id = s.add("A", "/bin/a", vec![]);
        assert!(!s.get(&id).unwrap().elevated);
        // set_elevated toggles the flag (and fails on an unknown id).
        assert!(s.set_elevated(&id, true));
        assert!(s.get(&id).unwrap().elevated);
        assert!(!s.set_elevated("inconnu", true));
        // TOML round-trip preserved.
        let toml = toml::to_string_pretty(&s).unwrap();
        let back: OpenerStore = toml::from_str(&toml).unwrap();
        assert!(back.get(&id).unwrap().elevated);
        // Backward compat: an opener serialized WITHOUT the field → false.
        let legacy = "[[openers]]\nid = \"x\"\nlabel = \"X\"\nprogram = \"/bin/x\"\n";
        let parsed: OpenerStore = toml::from_str(legacy).unwrap();
        assert!(!parsed.get("x").unwrap().elevated);
    }

    #[test]
    fn recipe_icon_is_backward_compatible_and_follows_duplicates() {
        let mut store = OpenerStore::default();
        let id = store.add("Compress", "/usr/bin/7z", vec!["a".into()]);
        assert_eq!(store.get(&id).unwrap().icon, OpenerIcon::None);
        assert!(store.set_icon(&id, OpenerIcon::SevenZip));

        let duplicate = store.duplicate(&id).unwrap();
        assert_eq!(store.get(&duplicate).unwrap().icon, OpenerIcon::SevenZip);

        let serialized = toml::to_string_pretty(&store).unwrap();
        let restored: OpenerStore = toml::from_str(&serialized).unwrap();
        assert_eq!(restored.get(&id).unwrap().icon, OpenerIcon::SevenZip);

        let legacy = "[[openers]]\nid = \"x\"\nlabel = \"X\"\nprogram = \"/bin/x\"\n";
        let restored_legacy: OpenerStore = toml::from_str(legacy).unwrap();
        assert_eq!(restored_legacy.get("x").unwrap().icon, OpenerIcon::None);
        assert_eq!(OpenerIcon::from_i32(99), OpenerIcon::None);
    }

    #[test]
    fn the_extension_filter_treats_star_and_empty_as_every_file() {
        let mut o = Opener {
            id: "x".into(),
            label: "Extract".into(),
            program: "/usr/bin/7z".into(),
            assoc: None,
            icon: OpenerIcon::None,
            args: vec![],
            default_exts: vec![],
            used_exts: vec![],
            use_count: 0,
            last_used: 0,
            elevated: false,
            ctx_menu: CTX_FILE,
            ctx_exts: vec![],
        };
        // Empty: no restriction. This is what every opener saved before the
        // field existed carries, so they must keep appearing everywhere.
        assert!(o.matches_ctx_ext("zip"));
        assert!(o.matches_ctx_ext(""));

        // The wildcard means the same thing, explicitly.
        o.ctx_exts = vec![CTX_EXT_ALL.to_string()];
        assert!(o.matches_ctx_ext("zip"));
        assert!(o.matches_ctx_ext("txt"));
        assert!(o.matches_ctx_ext(""), "an extensionless file still matches");

        // A real list restricts, and comparison ignores case.
        o.ctx_exts = vec!["zip".into(), "7z".into()];
        assert!(o.matches_ctx_ext("zip"));
        assert!(o.matches_ctx_ext("ZIP"));
        assert!(o.matches_ctx_ext("7z"));
        assert!(!o.matches_ctx_ext("txt"));
        assert!(!o.matches_ctx_ext(""), "no extension is not in the list");

        // The wildcard wins even when mixed with specific entries.
        o.ctx_exts = vec!["zip".into(), CTX_EXT_ALL.to_string()];
        assert!(o.matches_ctx_ext("txt"));
    }

    #[test]
    fn the_extension_filter_survives_a_round_trip_and_a_duplicate() {
        let mut s = OpenerStore::default();
        let id = s.add("Extract", "/usr/bin/7z", vec!["x".into()]);
        assert!(s.set_ctx_menu(&id, CTX_FILE));
        assert!(s.set_ctx_exts(&id, &["zip".to_string(), "rar".to_string()]));

        let toml = toml::to_string_pretty(&s).unwrap();
        let back: OpenerStore = toml::from_str(&toml).unwrap();
        assert_eq!(back.get(&id).unwrap().ctx_exts, ["zip", "rar"]);

        // A duplicate keeps the filter: it would otherwise silently widen to
        // every file, which is the opposite of what the copy was made for.
        let dup = s.duplicate(&id).unwrap();
        assert_eq!(s.get(&dup).unwrap().ctx_exts, ["zip", "rar"]);
    }

    #[test]
    fn the_list_tag_runs_once_and_expands_where_it_stands() {
        let o = Opener {
            id: "x".into(),
            label: "7z".into(),
            program: "/usr/bin/7z".into(),
            assoc: None,
            icon: OpenerIcon::None,
            args: vec!["a".into(), "{dir}/{dirname}.7z".into(), "{files}".into()],
            default_exts: vec![],
            used_exts: vec![],
            use_count: 0,
            last_used: 0,
            elevated: false,
            ctx_menu: 0,
            ctx_exts: Vec::new(),
        };
        // `{files}` means ONE run, so it must not be mistaken for a per-file tag.
        assert!(o.expands_list());
        // `{dir}`/`{dirname}` are per-file tags, hence `has_tag` is true here;
        // what matters is that `{files}` alone never makes it true.
        let list_only = Opener {
            args: vec!["a".into(), "out.7z".into(), "{files}".into()],
            ..o.clone()
        };
        assert!(
            !list_only.has_tag(),
            "{{files}} must not be counted as a per-file tag"
        );

        let ctx = TagContext::from_path(Path::new("/home/u/Photos/a.png"));
        let paths = [
            Path::new("/home/u/Photos/a.png"),
            Path::new("/home/u/Photos/b.png"),
        ];
        let files = &paths[..];
        // The archive is named after the FOLDER, which is what archivers do for
        // a multiple selection, and both paths land where the tag stood.
        assert_eq!(
            o.render_for_batch(&ctx, files),
            [
                "a",
                "/home/u/Photos/Photos.7z",
                "/home/u/Photos/a.png",
                "/home/u/Photos/b.png"
            ]
        );

        // The list keeps its position: it can precede a trailing option, which
        // the historical "append at the end" fallback could never express.
        let trailing = Opener {
            args: vec!["-r".into(), "{files}".into(), "--verbose".into()],
            ..o.clone()
        };
        assert_eq!(
            trailing.render_for_batch(&ctx, files),
            [
                "-r",
                "/home/u/Photos/a.png",
                "/home/u/Photos/b.png",
                "--verbose"
            ]
        );
    }

    #[test]
    fn the_names_tag_archives_the_files_without_their_tree() {
        let o = Opener {
            id: "x".into(),
            label: "tar".into(),
            program: "tar".into(),
            assoc: None,
            icon: OpenerIcon::None,
            args: vec![
                "-czf".into(),
                "{dir}/{dirname}.tar.gz".into(),
                "-C".into(),
                "{dir}".into(),
                "{names}".into(),
            ],
            default_exts: vec![],
            used_exts: vec![],
            use_count: 0,
            last_used: 0,
            elevated: false,
            ctx_menu: 0,
            ctx_exts: vec![],
        };
        // Like `{files}`, it means a single run over the whole selection.
        assert!(o.expands_list());

        let ctx = TagContext::from_path(Path::new("/home/u/Docs/a.txt"));
        let paths = [
            Path::new("/home/u/Docs/a.txt"),
            Path::new("/home/u/Docs/b.txt"),
        ];
        assert_eq!(
            o.render_for_batch(&ctx, &paths),
            [
                "-czf",
                "/home/u/Docs/Docs.tar.gz",
                "-C",
                "/home/u/Docs",
                "a.txt",
                "b.txt"
            ],
            "bare names, so the archive holds a.txt and not home/u/Docs/a.txt"
        );

        // `{files}` still yields full paths — the two are not interchangeable.
        let with_paths = Opener {
            args: vec!["-czf".into(), "out.tar.gz".into(), "{files}".into()],
            ..o.clone()
        };
        assert_eq!(
            with_paths.render_for_batch(&ctx, &paths),
            [
                "-czf",
                "out.tar.gz",
                "/home/u/Docs/a.txt",
                "/home/u/Docs/b.txt"
            ]
        );
    }

    #[test]
    fn dirname_is_the_folder_name_while_dir_is_its_path() {
        let ctx = TagContext::from_path(Path::new("/home/u/Photos/a.png"));
        assert_eq!(ctx.dir, "/home/u/Photos");
        assert_eq!(ctx.dirname, "Photos");
        assert_eq!(ctx.name, "a.png");
        assert_eq!(ctx.stem, "a");
        // A path with no parent leaves it empty rather than guessing.
        let bare = TagContext::from_path(Path::new("a.png"));
        assert_eq!(bare.dirname, "");
    }

    #[test]
    fn ctx_menu_mask_round_trips_and_filters() {
        let mut s = OpenerStore::default();
        let a = s.add("Git Bash", "/bin/bash", vec!["--cd={dir}".into()]);
        let b = s.add("BC", "/bin/bc", vec![]);
        // Default: not pinned.
        assert!(s.for_context(CTX_BACKGROUND).is_empty());
        // Mask set + filter by bit.
        assert!(s.set_ctx_menu(&a, CTX_DIR | CTX_BACKGROUND));
        assert!(s.set_ctx_menu(&b, CTX_FILE));
        assert_eq!(s.for_context(CTX_BACKGROUND).len(), 1);
        assert_eq!(s.for_context(CTX_FILE)[0].id, b);
        assert_eq!(s.for_context(CTX_DIR)[0].id, a);
        // TOML round-trip + backward compat (field absent → 0).
        let toml = toml::to_string_pretty(&s).unwrap();
        let back: OpenerStore = toml::from_str(&toml).unwrap();
        assert_eq!(back.get(&a).unwrap().ctx_menu, CTX_DIR | CTX_BACKGROUND);
        let legacy = "[[openers]]\nid = \"x\"\nlabel = \"X\"\nprogram = \"/bin/x\"\n";
        let parsed: OpenerStore = toml::from_str(legacy).unwrap();
        assert_eq!(parsed.get("x").unwrap().ctx_menu, 0);
        // Duplication keeps the pinning.
        let dup = s.duplicate(&a).unwrap();
        assert_eq!(s.get(&dup).unwrap().ctx_menu, CTX_DIR | CTX_BACKGROUND);
    }

    #[test]
    fn duplicate_inserts_after_and_resets_default_exts() {
        let mut s = OpenerStore::default();
        let a = s.add("A", "/bin/a", vec!["{file}".into()]);
        let _b = s.add("B", "/bin/b", vec![]);
        s.set_elevated(&a, true);
        s.set_default_ext(&a, "txt", true);
        let dup = s.duplicate(&a).unwrap();
        // Inserted RIGHT AFTER the original.
        assert_eq!(s.openers[1].id, dup);
        let d = s.get(&dup).unwrap();
        assert_eq!(d.label, "A");
        assert_eq!(d.program, "/bin/a");
        assert_eq!(d.args, vec!["{file}".to_string()]);
        assert!(d.elevated); // kept
        assert!(d.default_exts.is_empty()); // doesn't steal the "default" status
        // The original keeps its default extension.
        assert_eq!(s.default_for("txt").map(|o| o.id.clone()), Some(a));
    }

    #[test]
    fn suggested_puts_last_used_first() {
        let mut s = OpenerStore::default();
        let a = s.add("A", "/bin/a", vec![]);
        let b = s.add("B", "/bin/b", vec![]);
        let _c = s.add("C", "/bin/c", vec![]);
        let labels = |s: &OpenerStore| {
            s.suggested(10)
                .iter()
                .map(|o| o.label.clone())
                .collect::<Vec<_>>()
        };
        // No usage → the Vec's manual order.
        assert_eq!(labels(&s), ["A", "B", "C"]);
        // A is used then B (same second possible) → B on top, then A, then C.
        s.record_use(&a, None);
        s.record_use(&b, None);
        assert_eq!(labels(&s), ["B", "A", "C"]);
        // Using A again moves it back to the top.
        s.record_use(&a, None);
        assert_eq!(labels(&s), ["A", "B", "C"]);
    }

    #[test]
    fn suggested_for_ext_filters_and_learns() {
        let mut s = OpenerStore::default();
        let vlc = s.add("VLC", "/bin/vlc", vec![]);
        let word = s.add("Word", "/bin/word", vec![]);
        let _gimp = s.add("GIMP", "/bin/gimp", vec![]);
        // Word used for a .docx, VLC for a .mp4 (learning).
        s.record_use(&word, Some("docx"));
        s.record_use(&vlc, Some("mp4"));
        let labels = |v: Vec<&Opener>| v.iter().map(|o| o.label.clone()).collect::<Vec<_>>();
        // Flyout for a .mp4: ONLY VLC (Word/GIMP never used for mp4).
        assert_eq!(labels(s.suggested_for_ext("mp4", 8)), ["VLC"]);
        // Flyout for a .docx: only Word.
        assert_eq!(labels(s.suggested_for_ext(".DOCX", 8)), ["Word"]); // dot + case normalized
        // Extension never opened → empty list (→ "Choose an application…").
        assert!(s.suggested_for_ext("png", 8).is_empty());
        // An explicit `default_ext` also counts.
        s.set_default_ext(&_gimp, "png", true);
        assert_eq!(labels(s.suggested_for_ext("png", 8)), ["GIMP"]);
    }

    #[test]
    fn tag_context_splits_path() {
        let c = ctx("/home/u/photos/chat.PNG");
        assert_eq!(c.name, "chat.PNG");
        assert_eq!(c.stem, "chat");
        assert_eq!(c.ext, "png"); // lowercase
        assert_eq!(c.dir, "/home/u/photos");
    }

    #[test]
    fn substitute_tags_and_escapes() {
        let c = ctx("/a b/x.txt");
        assert_eq!(substitute("--in={file}", &c), "--in=/a b/x.txt");
        assert_eq!(substitute("{stem}.{ext}", &c), "x.txt");
        // Escaping + unknown literal tag.
        assert_eq!(substitute("{{lit}} {nope}", &c), "{lit} {nope}");
    }

    #[test]
    fn render_appends_file_when_no_tag() {
        let mut s = OpenerStore::default();
        let id = s.add("ed", "/usr/bin/ed", vec!["-n".into()]);
        let o = s.get(&id).unwrap();
        assert!(!o.has_tag());
        assert_eq!(
            o.render_for_file(&ctx("/a b/x.txt")),
            vec!["-n".to_string(), "/a b/x.txt".to_string()]
        );
    }

    #[test]
    fn render_uses_tags_when_present() {
        let mut s = OpenerStore::default();
        let id = s.add("g", "/usr/bin/g", vec!["--file".into(), "{file}".into()]);
        let o = s.get(&id).unwrap();
        assert!(o.has_tag());
        assert_eq!(
            o.render_for_file(&ctx("/p/y.jpg")),
            vec!["--file".to_string(), "/p/y.jpg".to_string()]
        );
    }

    #[test]
    fn default_ext_is_unique_and_normalized() {
        let mut s = OpenerStore::default();
        let a = s.add("A", "/a", vec![]);
        let b = s.add("B", "/b", vec![]);
        s.set_default_ext(&a, ".PNG", true);
        assert_eq!(s.default_for("png").map(|o| o.id.clone()), Some(a.clone()));
        // B takes png → A loses it (uniqueness).
        s.set_default_ext(&b, "png", true);
        assert_eq!(s.default_for("png").map(|o| o.id.clone()), Some(b.clone()));
        // Removal.
        s.set_default_ext(&b, "png", false);
        assert!(s.default_for("png").is_none());
    }

    #[test]
    fn set_used_exts_normalizes_and_lists_in_flyout() {
        let mut s = OpenerStore::default();
        let a = s.add("A", "/a", vec![]);
        // Manual edit: ".PNG, jpg,, png" → normalized + deduplicated.
        s.set_used_exts(&a, &[".PNG".into(), "jpg".into(), "".into(), "png".into()]);
        assert_eq!(s.get(&a).unwrap().used_exts, vec!["png", "jpg"]);
        // The opener is offered for each normalized extension.
        let labels = |v: Vec<&Opener>| v.iter().map(|o| o.label.clone()).collect::<Vec<_>>();
        assert_eq!(labels(s.suggested_for_ext("png", 8)), ["A"]);
        assert_eq!(labels(s.suggested_for_ext("jpg", 8)), ["A"]);
        // Re-editing to empty → no longer offered.
        s.set_used_exts(&a, &[]);
        assert!(s.suggested_for_ext("png", 8).is_empty());
    }

    #[test]
    fn move_reorders() {
        let mut s = OpenerStore::default();
        let a = s.add("A", "/a", vec![]);
        let b = s.add("B", "/b", vec![]);
        s.move_before(&b, Some(&a)); // B before A
        assert_eq!(s.openers[0].id, b);
        assert_eq!(s.openers[1].id, a);
    }

    #[test]
    fn save_load_round_trip() {
        let mut s = OpenerStore::default();
        let id = s.add(
            "VS Code",
            "/usr/bin/code",
            vec!["-n".into(), "{file}".into()],
        );
        s.set_default_ext(&id, "rs", true);
        let dir = std::env::temp_dir().join(format!(
            "ranger-op-test-{}",
            now_secs() + COUNTER.load(Ordering::Relaxed)
        ));
        let path = dir.join("openers.toml");
        s.save(&path).unwrap();
        assert_eq!(OpenerStore::load(&path), s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
