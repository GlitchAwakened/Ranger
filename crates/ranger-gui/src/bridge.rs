//! Bridge between Rust state and the Slint UI.
//!
//! Responsibilities:
//!   - filesystem navigation (open a folder, parent,
//!     home, back/forward via history),
//!   - column sorting (click on header, cyclic asc/desc),
//!   - refresh (F5 / button),
//!   - `notify` watcher on the current folder (auto-refresh),
//!   - preference persistence (language, theme, window size).
//!
//! Fast local listings can be handled on the UI thread. The
//! initial population, network paths, and expensive work (thumbnails,
//! recursive mtime, image metadata) are offloaded to background threads.

use std::cell::Cell;
use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use slint::{
    ComponentHandle, FilterModel, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer,
    SharedString, VecModel,
};
use tracing::{debug, error, info, warn};

use ranger_core::SidebarSection;
use ranger_core::favorites::{self, FlatFav};
use ranger_core::fs as rfs;
use ranger_core::fs::ops::{self};
use ranger_core::fs::{Entry, FileKind, GroupMode, SortColumn, SortOrder};
use ranger_core::layout::{Layout, LayoutNode, Rect, SplitDir};
use ranger_core::openers;
use ranger_core::shortcuts::{self, Chord};
use ranger_core::thumbnail::{self, Thumbnail};
use ranger_core::workspace::{self, PanelState, SidebarSectionsState, TabState, WorkspaceState};
use ranger_core::{Config, Lang, Theme, paths};

use crate::actions;
use crate::clipboard;
use crate::i18n;
use crate::openwith;
use ranger_core::columns::{self, ColumnSpec};

use crate::{
    ColumnInfo, Crumb, FavNode, FileRow, MainWindow, MenuShortcuts, OpProgress, OpenerItem,
    OwRecipe, PanelView, ShellCtxEntry, ShellExtRow, ShortcutCap, ShortcutGroup, ShortcutRow,
    SidebarPlace, SplitterView, TabInfo, WorkspaceEntry,
};

/// Minimum ratio for one side of a split (guards against degenerate panels).
const MIN_SPLIT_RATIO: f32 = 0.08;

/// Context remembered for the "Open with" picker between enumeration
/// (opening) and the user's choice.
struct OwPickCtx {
    ext: String,
    path: PathBuf,
    handlers: Vec<openwith::AppHandler>,
}

// ---------- Internal clipboard ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipOp {
    Copy,
    Cut,
}

/// Owns the private staging directory created for an incoming transient drop.
/// Keeping the guard in the paste/operation state guarantees cleanup on
/// success, error, conflict cancellation, or an early return.
struct TransientDropGuard(Option<PathBuf>);

impl TransientDropGuard {
    /// Only an OLE drop creates a staging directory, and that path is Windows
    /// only. The guard type itself stays unconditional: it is a field of state
    /// shared by both platforms, holding `None` elsewhere.
    #[cfg(windows)]
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }
}

impl Drop for TransientDropGuard {
    fn drop(&mut self) {
        let Some(path) = self.0.take() else { return };
        // Application-path captures can retain a complete hard-linked tree.
        // Delete it off the UI thread so finishing or cancelling a large drop
        // never stalls rendering.
        let cleanup_path = path.clone();
        if std::thread::Builder::new()
            .name("ranger-dnd-cleanup".into())
            .spawn(move || cleanup_transient_drop_dir(&cleanup_path))
            .is_err()
        {
            cleanup_transient_drop_dir(&path);
        }
    }
}

fn cleanup_transient_drop_dir(path: &Path) {
    if let Err(err) = std::fs::remove_dir_all(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        debug!(error = %err, path = %path.display(), "cleaning transient drop directory failed");
    }
}

/// Staging handed over by an OLE drop: the directory to clean up, and whether
/// its contents are copied out or moved out. Windows only — it is produced by
/// `winddrag` and consumed by `on_external_file_drop`, both gated the same way.
#[cfg(windows)]
struct IncomingDropStaging {
    cleanup: TransientDropGuard,
    op: ClipOp,
}

#[derive(Debug, Default)]
struct ClipboardState {
    paths: Vec<PathBuf>,
    op: Option<ClipOp>,
}

/// Paste operation currently resolving name conflicts.
/// Items with no conflict are directly "resolved" (original name); those
/// whose name is already taken wait for a user decision via the popup.
/// Once all conflicts are settled, `resolved` is executed.
struct PasteJob {
    op: ClipOp,
    dst_dir: PathBuf,
    /// Sources still waiting to be arbitrated (name conflict).
    pending: std::collections::VecDeque<PathBuf>,
    /// Triples (source, final target, overwrite) ready to execute. `overwrite =
    /// true` ("Replace" resolution) → the worker deletes the existing target
    /// before the copy/move; `false` (rename / no conflict) → the
    /// target is guaranteed to be free.
    resolved: Vec<(PathBuf, PathBuf, bool)>,
    /// Source whose conflict is currently shown in the popup.
    current: Option<PathBuf>,
    /// Keeps external staging data alive until this paste is resolved and its
    /// background copy/move has completed.
    transient_cleanup: Option<TransientDropGuard>,
}

impl PasteJob {
    /// `true` if this paste already sends one of its sources to `target`.
    /// Nothing exists on disk while the job is being arbitrated, so this is the
    /// only thing keeping two of its items off the same destination.
    fn claims(&self, target: &Path) -> bool {
        self.resolved.iter().any(|(_, dst, _)| dst == target)
    }
}

/// Context frozen at the moment of the drop, before the possible opening of the
/// Move/Copy/Link menu. Visual indices can change due to the watcher;
/// the paths, however, remain the ones the user actually targeted.
struct PendingFileDrop {
    sources: Vec<PathBuf>,
    destination: PathBuf,
}

// ---------- Navigation history ----------

#[derive(Debug, Default)]
struct NavHistory {
    stack: Vec<PathBuf>,
    cursor: usize,
}

impl NavHistory {
    fn current(&self) -> Option<&Path> {
        self.stack.get(self.cursor).map(|p| p.as_path())
    }

    /// Pushes a new path, truncating any "forward" entries.
    fn push(&mut self, path: PathBuf) {
        if self.current().map(|c| c == path).unwrap_or(false) {
            return;
        }
        if !self.stack.is_empty() {
            self.stack.truncate(self.cursor + 1);
        }
        self.stack.push(path);
        self.cursor = self.stack.len() - 1;
    }

    fn can_back(&self) -> bool {
        self.cursor > 0
    }
    fn can_forward(&self) -> bool {
        self.cursor + 1 < self.stack.len()
    }
    fn back(&mut self) -> Option<PathBuf> {
        if !self.can_back() {
            return None;
        }
        self.cursor -= 1;
        Some(self.stack[self.cursor].clone())
    }
    fn forward(&mut self) -> Option<PathBuf> {
        if !self.can_forward() {
            return None;
        }
        self.cursor += 1;
        Some(self.stack[self.cursor].clone())
    }
}

// ---------- Sort state ----------

#[derive(Debug, Clone, Copy)]
struct SortState {
    column: SortColumn,
    order: SortOrder,
}

impl Default for SortState {
    fn default() -> Self {
        Self {
            column: SortColumn::Name,
            order: SortOrder::Asc,
        }
    }
}

// Tabs ----------

/// A tab groups a current path, its navigation history, and its
/// sort order. Selection is not persisted between tabs; it is,
/// however, preserved when refreshing the same folder via the
/// `same-dir` mechanism of `refresh_listing`.
#[derive(Debug)]
struct Tab {
    current_path: PathBuf,
    history: NavHistory,
    sort: SortState,
    /// Selection anchor for Shift+click. `-1` if none.
    selection_anchor: i32,
    /// Display mode: `false` = list (icons), `true` = previews
    /// (thumbnails). DERIVED from `zoom` (`preview = zoom >= THUMB_ZOOM`) and kept
    /// because it's the one persisted per tab in the workspace TOML.
    preview: bool,
    /// Zoom level of entries (Ctrl+wheel). Mapped to a row
    /// height by `zoom_to_height`. Not persisted as-is: rebuilt from
    /// `preview` on restore (list vs thumbnails).
    zoom: i32,
    /// Whether hidden files are shown (dotfiles + Windows HIDDEN attribute).
    /// `false` by default. Persisted per tab in the workspace TOML.
    show_hidden: bool,
    /// Grouping by type (folders first / files first / mixed).
    /// Persisted per tab in the workspace TOML.
    group_mode: GroupMode,
    /// "Cursor" row (head of arrow-key keyboard navigation). `-1`
    /// = none. Session only (not persisted).
    cursor: i32,
    /// Counter incremented on each keyboard cursor movement → triggers
    /// scroll-into-view on the Slint side (without doing so on other refreshes).
    scroll_gen: i32,
    /// Extension filter: bar enabled from the columns menu.
    /// `ext_filter_on` = bar shown; `ext_filter` = free text ("jpg, png",
    /// loose syntax). Session only (not persisted in the workspace).
    ext_filter_on: bool,
    ext_filter: String,
}

impl Tab {
    fn new(initial: PathBuf) -> Self {
        let mut h = NavHistory::default();
        h.push(initial.clone());
        Self {
            current_path: initial,
            history: h,
            sort: SortState::default(),
            selection_anchor: -1,
            preview: false,
            zoom: LIST_DEFAULT_ZOOM,
            show_hidden: false,
            group_mode: GroupMode::FoldersFirst,
            cursor: -1,
            scroll_gen: 0,
            ext_filter_on: false,
            ext_filter: String::new(),
        }
    }

    /// Builds a tab restored from a workspace: path + sort + display
    /// mode. History restarts with a single entry (back/forward not
    /// persisted).
    fn restored(
        path: PathBuf,
        sort: SortState,
        preview: bool,
        show_hidden: bool,
        group_mode: GroupMode,
    ) -> Self {
        let mut h = NavHistory::default();
        h.push(path.clone());
        Self {
            current_path: path,
            history: h,
            sort,
            selection_anchor: -1,
            preview,
            zoom: if preview {
                THUMB_DEFAULT_ZOOM
            } else {
                LIST_DEFAULT_ZOOM
            },
            show_hidden,
            group_mode,
            cursor: -1,
            scroll_gen: 0,
            ext_filter_on: false,
            ext_filter: String::new(),
        }
    }
}

fn tab_to_state(tab: &Tab) -> TabState {
    TabState {
        path: tab.current_path.display().to_string(),
        sort_column: tab.sort.column,
        sort_order: tab.sort.order,
        preview: tab.preview,
        show_hidden: tab.show_hidden,
        group_mode: tab.group_mode,
    }
}

fn tab_from_state(tab: TabState) -> Tab {
    Tab::restored(
        resolve_restored_path(&tab.path),
        SortState {
            column: tab.sort_column,
            order: tab.sort_order,
        },
        tab.preview,
        tab.show_hidden,
        tab.group_mode,
    )
}

#[derive(Debug)]
struct TabBook {
    tabs: Vec<Tab>,
    active: usize,
}

impl TabBook {
    fn single(initial: PathBuf) -> Self {
        Self {
            tabs: vec![Tab::new(initial)],
            active: 0,
        }
    }

    fn open(&mut self, path: PathBuf) -> usize {
        self.tabs.push(Tab::new(path));
        self.active = self.tabs.len() - 1;
        self.active
    }

    /// Inserts `tab` right after tab `idx` and activates the new entry.
    /// Common primitive for contextual openings and duplication.
    fn insert_after(&mut self, idx: usize, tab: Tab) -> Option<usize> {
        if idx >= self.tabs.len() {
            return None;
        }
        let at = idx + 1;
        self.tabs.insert(at, tab);
        self.active = at;
        Some(at)
    }

    /// Opens a target right after the active tab. A `TabBook` always has
    /// at least one tab; the fallback at the end nonetheless protects this invariant.
    fn open_after_active(&mut self, path: PathBuf) -> usize {
        if self.active >= self.tabs.len() {
            return self.open(path);
        }
        self.insert_after(self.active, Tab::new(path))
            .expect("active tab was validated")
    }

    /// Duplicates tab `idx`: inserts a copy (same path + view settings)
    /// RIGHT AFTER it and activates it. `false` if the index is out of bounds.
    fn duplicate(&mut self, idx: usize) -> bool {
        let Some(src) = self.tabs.get(idx) else {
            return false;
        };
        let dup = Tab::restored(
            src.current_path.clone(),
            src.sort,
            src.preview,
            src.show_hidden,
            src.group_mode,
        );
        self.insert_after(idx, dup).is_some()
    }

    /// Closes tab `idx` and returns it. Refuses to close the last one.
    fn close(&mut self, idx: usize) -> Option<Tab> {
        if self.tabs.len() <= 1 || idx >= self.tabs.len() {
            return None;
        }
        let closed = self.tabs.remove(idx);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        } else if idx < self.active {
            self.active -= 1;
        }
        Some(closed)
    }

    fn select(&mut self, idx: usize) -> bool {
        if idx < self.tabs.len() && idx != self.active {
            self.active = idx;
            true
        } else {
            false
        }
    }

    /// Moves the tab at `from` to an **insertion position** `to_pre`
    /// computed BEFORE the removal: the user saw the cursor (and the
    /// preview line) at index position `to_pre` in the current list
    /// (so `to_pre ∈ [0, len]` — `len` = insert at the end).
    ///
    /// Internally, this is converted to a "post-remove index":
    ///   - if `to_pre > from`, the post-remove index is `to_pre - 1` (because
    ///     removing `from` shifted the elements to its right);
    ///   - otherwise, the post-remove index stays `to_pre`.
    ///
    /// Adjusts `active` so it keeps pointing at the same Tab.
    fn move_tab(&mut self, from: usize, to_pre: usize) -> bool {
        let n = self.tabs.len();
        if from >= n {
            return false;
        }
        let to = if to_pre > from { to_pre - 1 } else { to_pre };
        let to = to.min(n - 1);
        if to == from {
            return false;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        if self.active == from {
            self.active = to;
        } else if from < self.active && to >= self.active {
            self.active -= 1;
        } else if from > self.active && to <= self.active {
            self.active += 1;
        }
        true
    }
}

// View panel ----------

/// A panel represents an independent file view: tabs, row
/// model, and selection. A global watcher tracks the active panel and rearms on
/// each navigation.
struct Panel {
    tabs: TabBook,
    rows_model: Rc<VecModel<FileRow>>,
    /// Virtualized window over `rows_model`. The filter follows the
    /// technical field `FileRow.rendered`; operations keep using the
    /// full model and its stable indices.
    rendered_rows_model: ModelRc<FileRow>,
    /// Revision of order/geometry. Unlike content notifications
    /// (selection, thumbnail...), it forces hit-tests under a motionless pointer.
    rows_revision: Cell<i32>,
    /// Context change (folder/tab) requiring an exact return to the top.
    viewport_reset_gen: Cell<i32>,
    /// Last exact viewport published by Slint, in content coordinates.
    viewport_top: Cell<f32>,
    viewport_height: Cell<f32>,
    /// Half-open interval currently accepted by `rendered_rows_model`.
    rendered_first: Cell<usize>,
    rendered_end: Cell<usize>,
    /// Path actually loaded into `rows_model`. Lets us distinguish
    /// a refresh on the same folder (preserve selection) from a
    /// context change like a tab/panel switch (reset selection).
    displayed_path: PathBuf,
    /// Columns specific to the panel (order, visibility, and width).
    /// Independent from other panels; persisted per panel in the
    /// workspace .toml. Always normalized (`columns::sanitize`).
    columns: Vec<ColumnSpec>,
    /// Number of hidden entries in the displayed folder, recomputed on
    /// each listing. Feeds the "· K hidden" footer reminder when
    /// hidden entries aren't shown. Session only (not persisted).
    hidden_count: usize,
    /// The displayed folder is UNAVAILABLE (doesn't exist / network drive not
    /// started) → "not found" banner + automatic re-check. Session.
    unavailable: bool,
    /// Current scroll of the tab bar along its MAIN AXIS
    /// (`viewport-x` in horizontal mode, `viewport-y` in vertical mode, ≤ 0),
    /// REPORTED by the view (`panel-tabs-scrolled`). Used for the hit-test of the
    /// insertion gap for a tab received from another instance. Session.
    tabs_viewport_x: f32,
    /// Tab bar position: 0 = horizontal at the top,
    /// 1 = vertical on the left, 2 = vertical on the right. Persisted per view.
    tab_bar_mode: u8,
    /// USER width of the vertical bar (logical px), set via the
    /// handle. `0` = automatic (clamped 40% formula). Persisted per view.
    vbar_user_w: f32,
    /// INITIAL listing still awaited from the startup thread: the
    /// startup population is asynchronous so as not to block the display
    /// on a slow network path. Cleared by delivery or by any more
    /// recent listing (navigation, F5, watcher) so as to discard a stale result.
    pending_initial: bool,
    /// Ordinary network listing in progress. Distinct from `pending_initial` so
    /// that a stale initial result can't win a race during an
    /// A → B → A navigation.
    pending_listing: bool,
    /// Identifier of the network request awaited by this panel.
    listing_gen: u64,
    /// Child to select after an asynchronous upward navigation.
    pending_select: Option<String>,
}

impl Panel {
    /// New panel with an explicit tab bar position (settings
    /// default, inherited on split, instance detached via tear-off).
    fn with_mode(initial: PathBuf, columns: Vec<ColumnSpec>, tab_bar_mode: u8) -> Self {
        let (rows_model, rendered_rows_model) = new_row_models();
        Self {
            tabs: TabBook::single(initial.clone()),
            rows_model,
            rendered_rows_model,
            rows_revision: Cell::new(0),
            viewport_reset_gen: Cell::new(0),
            viewport_top: Cell::new(0.0),
            viewport_height: Cell::new(DEFAULT_RENDER_VIEWPORT_HEIGHT),
            rendered_first: Cell::new(0),
            rendered_end: Cell::new(0),
            displayed_path: initial,
            columns: columns::sanitize(columns),
            hidden_count: 0,
            unavailable: false,
            tabs_viewport_x: 0.0,
            tab_bar_mode: tab_bar_mode.min(2),
            vbar_user_w: 0.0,
            pending_initial: false,
            pending_listing: false,
            listing_gen: 0,
            pending_select: None,
        }
    }

    fn bump_rows_revision(&self) {
        self.rows_revision
            .set(self.rows_revision.get().wrapping_add(1));
    }

    fn reset_rows_viewport(&self) {
        self.viewport_top.set(0.0);
        self.viewport_reset_gen
            .set(self.viewport_reset_gen.get().wrapping_add(1));
    }

    /// Replaces the logical model, marking BEFORE the notification the small
    /// interval to render around the current viewport.
    fn replace_rows(&self, mut rows: Vec<FileRow>) {
        let (first, end) = mark_render_window(
            &mut rows,
            self.viewport_top.get(),
            self.viewport_height.get(),
        );
        self.rendered_first.set(first);
        self.rendered_end.set(end);
        self.rows_model.set_vec(rows);
        self.bump_rows_revision();
    }
}

fn file_row_is_rendered(row: &FileRow) -> bool {
    row.rendered
}

fn new_row_models() -> (Rc<VecModel<FileRow>>, ModelRc<FileRow>) {
    let rows = Rc::new(VecModel::<FileRow>::default());
    let rendered = FilterModel::new(rows.clone(), file_row_is_rendered as fn(&FileRow) -> bool);
    (rows, ModelRc::new(rendered))
}

type AsyncListingResult = std::result::Result<(Vec<Entry>, usize), bool>;

/// Send delivery of a network listing. `Err(true)` = access denied; other
/// errors become an unavailable path, same as in the synchronous path.
struct AsyncListingDelivery {
    panel: usize,
    r#gen: u64,
    path: PathBuf,
    result: AsyncListingResult,
    lang: Lang,
    big_icon: bool,
    preserved_selected: Vec<String>,
    preserved_anchor: Option<String>,
}

/// Send events produced by the trash workers then consumed on the
/// Slint thread. `PathBuf`s remain native: no non-UTF-8 name is lost
/// during a delete/restore on Linux.
enum TrashDelivery {
    Trashed(PathBuf),
    RestoreFinished {
        /// Registry entry to release once the restore is applied.
        op_id: i32,
        original_path: PathBuf,
        error: Option<ranger_core::fs::ops::TrashError>,
    },
}

/// A long operation in flight, tracked in `OpRegistry` under a unique id.
/// Keying by id (rather than a single "one is running" flag) lets the toast,
/// the cancel button and the completion handler each target one exact operation.
struct OpHandle {
    /// Cooperative cancellation flag shared with the worker thread. `None` for
    /// operations that cannot be interrupted (restoring from the trash).
    cancel: Option<Arc<AtomicBool>>,
    /// Item to highlight (selection + scroll) once THIS operation finishes: the
    /// final target of a paste/move. A full path, since the active view may have
    /// changed folder during the copy — in that case nothing is highlighted.
    pending_focus: Option<PathBuf>,
    /// Paths this operation WRITES to. Held until it completes so that another
    /// operation started meanwhile cannot resolve to the same destination.
    /// Deletions write nothing and reserve nothing.
    targets: Vec<PathBuf>,
    /// Source and destination names whose content-thumbnail identity may have
    /// changed when this operation finishes.
    thumbnail_invalidations: Vec<PathBuf>,
    /// Held until the worker reports completion, then dropped before refresh.
    transient_cleanup: Option<TransientDropGuard>,
}

/// The long operations currently running, each under a session-unique id.
#[derive(Default)]
struct OpRegistry {
    active: RefCell<HashMap<i32, OpHandle>>,
    /// Union of every in-flight operation's `targets`, for O(1) lookup.
    /// A destination is claimed as soon as its operation starts, well before the
    /// file physically exists — that is precisely the window in which a second
    /// operation would otherwise pick the very same name.
    reserved: RefCell<HashSet<PathBuf>>,
    /// Last id handed out; ids start at 1, so 0 always means "no operation".
    serial: Cell<i32>,
}

impl OpRegistry {
    /// `true` while at least one operation is in flight.
    fn in_flight(&self) -> bool {
        !self.active.borrow().is_empty()
    }

    /// Registers an operation and returns the id identifying it until it
    /// completes. Ids are never reused within a session, so a late completion
    /// can't release an operation started afterwards.
    fn register(&self, handle: OpHandle) -> i32 {
        let id = self.serial.get().wrapping_add(1).max(1);
        self.serial.set(id);
        self.reserved
            .borrow_mut()
            .extend(handle.targets.iter().cloned());
        self.active.borrow_mut().insert(id, handle);
        id
    }

    /// Removes a finished operation, releases the destinations it held and
    /// hands back its handle, whose `pending_focus` the caller consumes. `None`
    /// for an unknown id (already released — a completion delivered twice).
    fn finish(&self, id: i32) -> Option<OpHandle> {
        let handle = self.active.borrow_mut().remove(&id)?;
        let mut reserved = self.reserved.borrow_mut();
        for target in &handle.targets {
            reserved.remove(target);
        }
        Some(handle)
    }

    /// `true` if a running operation has already claimed this exact
    /// destination. Used alongside `Path::exists` wherever a free name is
    /// picked, so the answer covers files that are about to exist.
    fn is_reserved(&self, path: &Path) -> bool {
        self.reserved.borrow().contains(path)
    }

    /// Raises the cancellation flag of one operation, if it is still running
    /// and interruptible. Unknown or uninterruptible ids are a no-op.
    fn cancel(&self, id: i32) {
        if let Some(handle) = self.active.borrow().get(&id)
            && let Some(flag) = handle.cancel.as_ref()
        {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

/// Width (px) of a column by its id, or the default width.
fn col_width(cols: &[ColumnSpec], id: &str) -> f32 {
    cols.iter()
        .find(|c| c.id == id)
        .map(|c| c.width)
        .unwrap_or_else(|| columns::default_width(id))
}

/// Applies a width to column `id` (if present).
fn set_col_width(cols: &mut [ColumnSpec], id: &str, w: f32) {
    if let Some(c) = cols.iter_mut().find(|c| c.id == id) {
        c.width = w;
    }
}

/// Reorders column `id` based on a **horizontal move** `delta_px`
/// (signed). Computes the new slot from the widths of the
/// visible columns (the `name` anchor stays first). Hidden columns are
/// kept (pushed to the end of the list).
fn reorder_column_by_delta(cols: &mut Vec<ColumnSpec>, id: &str, delta_px: f32) {
    if id == "name" {
        return;
    }
    // Visible columns (id, effective width) in order.
    let vis: Vec<(String, f32)> = cols
        .iter()
        .filter(|c| c.visible)
        .map(|c| {
            let w = if c.width > 0.0 {
                c.width
            } else {
                columns::default_width(&c.id)
            };
            (c.id.clone(), w)
        })
        .collect();
    let Some(cur) = vis.iter().position(|(i, _)| i == id) else {
        return;
    };
    // Includes the inter-column gap (= `Tokens.col-gap`) to match the
    // offsets/preview on the GUI side.
    const COL_GAP: f32 = 8.0;
    // Current center of the dragged column + movement = target center.
    let left_before: f32 = vis[..cur].iter().map(|(_, w)| w + COL_GAP).sum();
    let new_center = left_before + vis[cur].1 / 2.0 + delta_px;

    // Target "gap" in the STATIC layout (columns in place, like the
    // preview): the 1st gap whose column midpoint is past the target center.
    let mut gap = vis.len();
    let mut x = 0.0_f32;
    for (k, (_, w)) in vis.iter().enumerate() {
        if new_center < x + w / 2.0 {
            gap = k;
            break;
        }
        x += w + COL_GAP;
    }
    let gap = gap.max(1); // ≥ 1 → always after the `name` anchor

    // Removes the dragged column then adjusts the gap, whose index shifts when
    // the source was before the target.
    let mut order: Vec<String> = vis.iter().map(|(i, _)| i.clone()).collect();
    order.remove(cur);
    let t = if gap > cur { gap - 1 } else { gap };
    let t = t.clamp(1, order.len());
    order.insert(t, id.to_string());

    // Rebuilds: visible columns in the new order, then the hidden ones.
    let mut new: Vec<ColumnSpec> = Vec::with_capacity(cols.len());
    for oid in &order {
        if let Some(spec) = cols.iter().find(|c| &c.id == oid) {
            new.push(spec.clone());
        }
    }
    for c in cols.iter().filter(|c| !c.visible) {
        new.push(c.clone());
    }
    *cols = columns::sanitize(new);
}

/// Pushes the DEFAULT columns (config) to the Settings panel.
fn push_settings_columns(window: &MainWindow, lang: Lang, cols: &[ColumnSpec]) {
    let strings = i18n::strings_for(lang);
    let infos: Vec<ColumnInfo> = columns::sanitize(cols.to_vec())
        .iter()
        .map(|c| {
            let mut info = column_info(&strings, c, 0.0);
            // Settings checkbox: MORE explicit label for the columns
            // reserved for images (the column header in the view stays short,
            // via `column_info`).
            match c.id.as_str() {
                "resolution" => info.label = strings.settings_col_resolution.clone(),
                "depth" => info.label = strings.settings_col_depth.clone(),
                _ => {}
            }
            info
        })
        .collect();
    window.set_settings_columns(ModelRc::new(VecModel::from(infos)));
}

/// Builds a `ColumnInfo` (id + i18n label + visible + offset) for the GUI.
fn column_info(strings: &crate::Strings, c: &ColumnSpec, offset: f32) -> ColumnInfo {
    let label = match c.id.as_str() {
        "name" => strings.col_name.clone(),
        "path" => strings.col_path.clone(),
        "size" => strings.col_size.clone(),
        "modified" => strings.col_modified.clone(),
        "age" => strings.col_age.clone(),
        "ext" => strings.col_ext.clone(),
        "resolution" => strings.col_resolution.clone(),
        "depth" => strings.col_depth.clone(),
        other => other.into(),
    };
    ColumnInfo {
        id: c.id.clone().into(),
        label,
        visible: c.visible,
        offset,
    }
}

// ---------- Shared application state ----------
//
// Since Slint is single-threaded, `AppState` uses `Rc<RefCell<…>>` and doesn't need
// to be `Send`/`Sync`. The `notify` watcher stays on its internal thread without
// accessing the state; it only schedules a callback on the Slint event loop.

/// Memoized image metadata: `(unix mtime, resolution, depth)`. The mtime
/// is the file VERSION for which resolution/depth were read.
type ImgMeta = (i64, String, String);

#[derive(Clone)]
pub struct AppState {
    pub config: Rc<RefCell<Config>>,
    panels: Rc<RefCell<Vec<Panel>>>,
    active_panel: Rc<RefCell<usize>>,
    /// Layout tree — source of truth for the arrangement. Leaves
    /// reference an index into `panels`. Always valid for
    /// `panels.len()`.
    layout: Rc<RefCell<LayoutNode>>,
    /// Watcher for the displayed folder. `Arc<Mutex>` allows its creation and
    /// installation on a background thread, since `watch` can suffer SMB latency.
    watcher: Arc<std::sync::Mutex<Option<RecommendedWatcher>>>,
    /// Watcher generation: incremented on each (re)installation or release
    /// → a STALE background install (more recent navigation, ejection) discards its
    /// watcher instead of installing it.
    watcher_gen: Arc<std::sync::atomic::AtomicU64>,
    /// Results of network listings produced off the UI thread then drained via a
    /// Slint callback. The queue is shared since `AppState` itself is `Rc`.
    async_listings: Arc<std::sync::Mutex<VecDeque<AsyncListingDelivery>>>,
    listing_serial: Arc<AtomicU64>,
    clipboard: Rc<RefCell<ClipboardState>>,
    /// Paths received from an OLE drop coming from another Windows instance/app.
    /// Kept until the Move/Copy/Link choice from the drop menu.
    external_drop_paths: Rc<RefCell<Vec<PathBuf>>>,
    pending_file_drop: Rc<RefCell<Option<PendingFileDrop>>>,
    /// Paste in progress (resolving name conflicts). `None` at rest.
    paste_job: Rc<RefCell<Option<PasteJob>>>,
    /// Exact source of the rename popup. A path, not the visual row
    /// index: a watcher/re-sort can rebuild the model while the
    /// dialog is open without ever causing a different entry to be renamed.
    rename_source: Rc<RefCell<Option<PathBuf>>>,
    /// Selection frozen at the moment the permanent-delete warning
    /// is opened. A watcher may re-list underneath the popup: the confirmation must
    /// always apply to the announced paths, never to a new row.
    delete_pending: Rc<RefCell<Vec<PathBuf>>>,
    /// Last item actually sent to a recoverable trash.
    /// History deliberately limited to one entry, session-only.
    last_trashed: Rc<RefCell<Option<PathBuf>>>,
    /// Deliveries from the trash workers to the UI thread.
    trash_deliveries: Arc<std::sync::Mutex<VecDeque<TrashDelivery>>>,
    /// Long operations currently in flight. Empty at rest.
    ops: Rc<OpRegistry>,
    /// Item the next re-listing should select and scroll to — the result of the
    /// last operation to finish. Staged here rather than applied on the spot:
    /// the row only exists once the views have been re-read.
    focus_after_refresh: Rc<RefCell<Option<PathBuf>>>,
    /// One row per operation toast on screen. Tracks `ops` loosely: a row only
    /// appears past the deferred-appearance threshold, and outlives its
    /// operation until the toast is dismissed.
    ops_model: Rc<VecModel<OpProgress>>,
    // Previews / thumbnails -----
    /// Decode scheduler. It deduplicates paths, drops
    /// requests that became unnecessary, and re-prioritizes the queue based on the
    /// visible row ranges published by each panel while scrolling.
    thumb_scheduler: Arc<ThumbScheduler>,
    /// In-memory LRU cache (path → image). Session only, nothing on disk.
    thumb_cache: Rc<RefCell<ThumbLru>>,
    /// Paths reported by the filesystem watcher whose cached content texture
    /// must be invalidated on the UI thread before the debounced re-list.
    thumb_invalidations: Arc<Mutex<HashSet<PathBuf>>>,
    // Recursive folder stats: modification date + size -----
    /// Queue of recursive folder-stats computations — mtime + size in ONE shared
    /// walk — consumed by the background worker.
    rmtime_tx: mpsc::Sender<RMtimeJob>,
    rmtime_rx: Rc<RefCell<Option<mpsc::Receiver<RMtimeJob>>>>,
    /// Generation: invalidates stale jobs (folder left / a depth changed).
    rmtime_gen: Arc<AtomicU64>,
    /// In-memory cache (folder path → recursive Unix mtime). Session only.
    rmtime_cache: Rc<RefCell<HashMap<String, i64>>>,
    /// In-memory cache (folder path → recursive total size in bytes). Session
    /// only. Separate from the mtime cache: the two options toggle independently.
    size_cache: Rc<RefCell<HashMap<String, u64>>>,
    // Image metadata: resolution / depth -----
    /// Queue of image header reads (background worker).
    imgmeta_tx: mpsc::Sender<ImgMetaJob>,
    imgmeta_rx: Rc<RefCell<Option<mpsc::Receiver<ImgMetaJob>>>>,
    /// Generation: invalidates stale jobs (folder left).
    imgmeta_gen: Arc<AtomicU64>,
    /// Cache (file path → (unix mtime, resolution, depth)). Session
    /// only. The mtime records the file VERSION for which the
    /// metadata was read: if the file is modified (image editing → new
    /// mtime), the cache is detected as stale and recomputed (resolution/depth
    /// columns update on their own, without F5).
    imgmeta_cache: Rc<RefCell<HashMap<String, ImgMeta>>>,
    /// "Type-ahead" filter of the active view (substring, case-insensitive).
    /// Cleared on navigation / active panel change. Session only.
    filter: Rc<RefCell<String>>,
    /// Bounded history of actually closed tabs, persisted in the
    /// current workspace. Order old → recent; `pop()` restores the last one.
    closed_tabs: Rc<RefCell<Vec<TabState>>>,
    /// Name of the named workspace the current state derives from, for the
    /// window title. `None` = ad hoc state → title "Ranger". Persisted in the
    /// current state file via `capture_workspace`.
    current_workspace: Rc<RefCell<Option<String>>>,
    /// SAVED snapshot of the current named workspace. This is the single
    /// reference for "modified" detection: both the title and the Update button
    /// query `current_workspace_is_dirty`, which compares this snapshot to the live state
    /// via `workspace_signature`. No I/O is done during normal use.
    saved_workspace: Rc<RefCell<Option<WorkspaceState>>>,
    /// Collapsed state of the Shortcuts/Favorites/Drives/Network sections. Copyable and
    /// purely in-memory: it feeds into `capture_workspace` and thus into the same
    /// dirty signature as the tabs, without reading the visual state from Slint at
    /// save time.
    sidebar_sections: Rc<Cell<SidebarSectionsState>>,
    /// Effective shortcut map: defaults + config overrides.
    /// Rebuilt on every rebind/reset.
    keymap: Rc<RefCell<shortcuts::Keymap>>,
    /// Shortcut conflict pending resolution:
    /// (targeted action, other conflicting action, serialized proposed chord).
    pending_conflict: Rc<RefCell<Option<(String, String, String)>>>,
    /// Search filter for the shortcuts list (settings).
    shortcut_filter: Rc<RefCell<String>>,
    /// Favorites tree, shared storage (`favorites.toml`).
    favorites: Rc<RefCell<favorites::FavStore>>,
    /// "Open with" openers, dedicated store `openers.toml`.
    openers: Rc<RefCell<openers::OpenerStore>>,
    /// Filter for the Settings > Open with > Open with list.
    opener_filter: Rc<RefCell<String>>,
    /// Full model already built (icons included). Typing in the filter
    /// only clones/filters these items: no new icon extraction
    /// nor path check is triggered on every keystroke.
    opener_settings_cache: Rc<RefCell<Vec<OpenerItem>>>,
    /// Application picker context: ext + path + handlers
    /// enumerated by the OS, remembered between opening the picker and the choice.
    ow_pick_ctx: Rc<RefCell<Option<OwPickCtx>>>,
    /// Paths waiting to be saved by the "Save as favorite" popup.
    fav_save_pending: Rc<RefCell<Vec<PathBuf>>>,
    /// Container ids in the popup dropdown order (index 0 = root).
    fav_container_ids: Rc<RefCell<Vec<String>>>,
    /// Last drives signature seen. The periodic poll of the
    /// "Drives" sidebar compares `places::drives_signature()` to this value
    /// and only rebuilds the model if it has changed (subst, USB, `net use`…).
    last_drives_sig: Rc<Cell<u64>>,
    /// EPHEMERAL instance (detached via tab tear-off): NEVER persists
    /// the shared workspace — otherwise it would overwrite the layout of
    /// the main instance on every mutation (sort, grouping…).
    ephemeral: Rc<Cell<bool>>,
    /// Windows SHELL context menu session: the `IContextMenu` object
    /// and its HMENU stay alive between opening the menu and invocation
    /// (the ids are only valid for this instance). UI thread only (STA).
    shell_menu: Rc<RefCell<Option<crate::shellmenu::ShellMenuSession>>>,
    /// Submenus (cascades) of the current shell menu: children ready to
    /// push into the flyout when hovering the parent (index = `ShellCtxEntry.sub`).
    shell_subs: Rc<RefCell<Vec<Vec<ShellCtxEntry>>>>,
    /// Top-level shell menu labels SEEN (lazy probe + right-clicks) —
    /// feeds the "Detected Windows entries" box in settings. Session.
    shell_known: Rc<RefCell<Vec<String>>>,
    /// Has the lazy shell menu probe already run? Only once
    /// per session: right-clicks then enrich the list.
    shell_scanned: Rc<Cell<bool>>,
    /// Screen (OS) scale captured ONCE at startup, BEFORE any application
    /// UI zoom. Stable reference for `apply_ui_scale` (scale = base ×
    /// factor). `0.0` = not yet captured.
    ui_base_scale: Rc<Cell<f32>>,
}

/// Maximum number of panels. Splits are "unlimited" by design;
/// this generous ceiling is just a safeguard against pathological
/// layouts (the minimum size bounds the useful depth anyway).
pub const MAX_PANELS: usize = 16;

impl AppState {
    /// Default state: one panel, one tab on `$HOME`. The tab bar
    /// takes the DEFAULT position from settings.
    pub fn new(config: Config) -> Self {
        let mode = config.default_tab_bar_mode.min(2);
        Self::new_at(config, home_dir(), mode)
    }

    /// "Blank" state starting on `initial` (one panel, one tab) — used
    /// by tab tear-off (`ranger --detached-tab <folder> [x y [mode]]`).
    /// `tab_bar_mode`: tab bar position inherited from the source view
    /// (0 top / 1 left / 2 right).
    pub fn new_at(config: Config, initial: PathBuf, tab_bar_mode: u8) -> Self {
        let (rmtime_tx, rmtime_rx) = mpsc::channel();
        let (imgmeta_tx, imgmeta_rx) = mpsc::channel();
        let default_cols = config.default_columns.clone();
        let keymap = shortcuts::Keymap::build(&config.shortcut_overrides);
        Self {
            config: Rc::new(RefCell::new(config)),
            panels: Rc::new(RefCell::new(vec![Panel::with_mode(
                initial,
                default_cols,
                tab_bar_mode,
            )])),
            active_panel: Rc::new(RefCell::new(0)),
            layout: Rc::new(RefCell::new(LayoutNode::Leaf { panel: 0 })),
            watcher: Arc::new(std::sync::Mutex::new(None)),
            watcher_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            async_listings: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            listing_serial: Arc::new(AtomicU64::new(0)),
            clipboard: Rc::new(RefCell::new(ClipboardState::default())),
            external_drop_paths: Rc::new(RefCell::new(Vec::new())),
            pending_file_drop: Rc::new(RefCell::new(None)),
            paste_job: Rc::new(RefCell::new(None)),
            rename_source: Rc::new(RefCell::new(None)),
            delete_pending: Rc::new(RefCell::new(Vec::new())),
            last_trashed: Rc::new(RefCell::new(None)),
            trash_deliveries: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            ops: Rc::new(OpRegistry::default()),
            focus_after_refresh: Rc::new(RefCell::new(None)),
            ops_model: Rc::new(VecModel::default()),
            thumb_scheduler: Arc::new(ThumbScheduler::new()),
            thumb_cache: Rc::new(RefCell::new(ThumbLru::new(512))),
            thumb_invalidations: Arc::new(Mutex::new(HashSet::new())),
            rmtime_tx,
            rmtime_rx: Rc::new(RefCell::new(Some(rmtime_rx))),
            rmtime_gen: Arc::new(AtomicU64::new(0)),
            rmtime_cache: Rc::new(RefCell::new(HashMap::new())),
            size_cache: Rc::new(RefCell::new(HashMap::new())),
            imgmeta_tx,
            imgmeta_rx: Rc::new(RefCell::new(Some(imgmeta_rx))),
            imgmeta_gen: Arc::new(AtomicU64::new(0)),
            imgmeta_cache: Rc::new(RefCell::new(HashMap::new())),
            filter: Rc::new(RefCell::new(String::new())),
            closed_tabs: Rc::new(RefCell::new(Vec::new())),
            current_workspace: Rc::new(RefCell::new(None)),
            saved_workspace: Rc::new(RefCell::new(None)),
            sidebar_sections: Rc::new(Cell::new(SidebarSectionsState::default())),
            keymap: Rc::new(RefCell::new(keymap)),
            pending_conflict: Rc::new(RefCell::new(None)),
            shortcut_filter: Rc::new(RefCell::new(String::new())),
            favorites: Rc::new(RefCell::new(favorites::FavStore::load(
                &paths::favorites_path(),
            ))),
            openers: Rc::new(RefCell::new(openers::OpenerStore::load(
                &paths::openers_path(),
            ))),
            opener_filter: Rc::new(RefCell::new(String::new())),
            opener_settings_cache: Rc::new(RefCell::new(Vec::new())),
            ow_pick_ctx: Rc::new(RefCell::new(None)),
            fav_save_pending: Rc::new(RefCell::new(Vec::new())),
            fav_container_ids: Rc::new(RefCell::new(Vec::new())),
            last_drives_sig: Rc::new(Cell::new(0)),
            ephemeral: Rc::new(Cell::new(false)),
            shell_menu: Rc::new(RefCell::new(None)),
            shell_subs: Rc::new(RefCell::new(Vec::new())),
            shell_known: Rc::new(RefCell::new(Vec::new())),
            shell_scanned: Rc::new(Cell::new(false)),
            ui_base_scale: Rc::new(Cell::new(0.0)),
        }
    }

    /// Builds application state from a restored workspace.
    /// Nonexistent paths (folder deleted/unmounted since the last
    /// session) are replaced with `$HOME` — a tab is never lost,
    /// it's just brought back to a safe point.
    pub fn from_workspace(config: Config, ws: WorkspaceState) -> Self {
        let current_workspace = ws.workspace_name.clone();
        let closed_tabs = ws.closed_tabs.clone();
        let sidebar_sections = ws.sidebar_sections;
        let keymap = shortcuts::Keymap::build(&config.shortcut_overrides);
        let (panels, layout, active_panel) = build_panels(ws);
        let (rmtime_tx, rmtime_rx) = mpsc::channel();
        let (imgmeta_tx, imgmeta_rx) = mpsc::channel();
        let state = Self {
            config: Rc::new(RefCell::new(config)),
            panels: Rc::new(RefCell::new(panels)),
            active_panel: Rc::new(RefCell::new(active_panel)),
            layout: Rc::new(RefCell::new(layout)),
            watcher: Arc::new(std::sync::Mutex::new(None)),
            watcher_gen: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            async_listings: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            listing_serial: Arc::new(AtomicU64::new(0)),
            clipboard: Rc::new(RefCell::new(ClipboardState::default())),
            external_drop_paths: Rc::new(RefCell::new(Vec::new())),
            pending_file_drop: Rc::new(RefCell::new(None)),
            paste_job: Rc::new(RefCell::new(None)),
            rename_source: Rc::new(RefCell::new(None)),
            delete_pending: Rc::new(RefCell::new(Vec::new())),
            last_trashed: Rc::new(RefCell::new(None)),
            trash_deliveries: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            ops: Rc::new(OpRegistry::default()),
            focus_after_refresh: Rc::new(RefCell::new(None)),
            ops_model: Rc::new(VecModel::default()),
            thumb_scheduler: Arc::new(ThumbScheduler::new()),
            thumb_cache: Rc::new(RefCell::new(ThumbLru::new(512))),
            thumb_invalidations: Arc::new(Mutex::new(HashSet::new())),
            rmtime_tx,
            rmtime_rx: Rc::new(RefCell::new(Some(rmtime_rx))),
            rmtime_gen: Arc::new(AtomicU64::new(0)),
            rmtime_cache: Rc::new(RefCell::new(HashMap::new())),
            size_cache: Rc::new(RefCell::new(HashMap::new())),
            imgmeta_tx,
            imgmeta_rx: Rc::new(RefCell::new(Some(imgmeta_rx))),
            imgmeta_gen: Arc::new(AtomicU64::new(0)),
            imgmeta_cache: Rc::new(RefCell::new(HashMap::new())),
            filter: Rc::new(RefCell::new(String::new())),
            closed_tabs: Rc::new(RefCell::new(closed_tabs)),
            current_workspace: Rc::new(RefCell::new(current_workspace)),
            saved_workspace: Rc::new(RefCell::new(None)),
            sidebar_sections: Rc::new(Cell::new(sidebar_sections)),
            keymap: Rc::new(RefCell::new(keymap)),
            pending_conflict: Rc::new(RefCell::new(None)),
            shortcut_filter: Rc::new(RefCell::new(String::new())),
            favorites: Rc::new(RefCell::new(favorites::FavStore::load(
                &paths::favorites_path(),
            ))),
            openers: Rc::new(RefCell::new(openers::OpenerStore::load(
                &paths::openers_path(),
            ))),
            opener_filter: Rc::new(RefCell::new(String::new())),
            opener_settings_cache: Rc::new(RefCell::new(Vec::new())),
            ow_pick_ctx: Rc::new(RefCell::new(None)),
            fav_save_pending: Rc::new(RefCell::new(Vec::new())),
            fav_container_ids: Rc::new(RefCell::new(Vec::new())),
            last_drives_sig: Rc::new(Cell::new(0)),
            ephemeral: Rc::new(Cell::new(false)),
            shell_menu: Rc::new(RefCell::new(None)),
            shell_subs: Rc::new(RefCell::new(Vec::new())),
            shell_known: Rc::new(RefCell::new(Vec::new())),
            shell_scanned: Rc::new(Cell::new(false)),
            ui_base_scale: Rc::new(Cell::new(0.0)),
        };
        // A single read at startup: subsequent live comparisons are
        // entirely in memory.
        state.reload_saved_workspace();
        state
    }

    /// Rebuilds the shortcut map from the config overrides.
    fn rebuild_keymap(&self) {
        let km = shortcuts::Keymap::build(&self.config.borrow().shortcut_overrides);
        *self.keymap.borrow_mut() = km;
    }

    /// Replaces the current layout **in place** (panels/tabs/
    /// stretches/active) with that of a loaded workspace. Mutates the content
    /// of the `Rc<RefCell<…>>` without changing the `Rc`s themselves, so that all
    /// shared clones (captured by closures) see the change.
    /// The caller must then repopulate the rows + refresh the UI.
    pub fn replace_with_workspace(&self, ws: WorkspaceState) {
        let closed_tabs = ws.closed_tabs.clone();
        let sidebar_sections = ws.sidebar_sections;
        let (panels, layout, active_panel) = build_panels(ws);
        *self.panels.borrow_mut() = panels;
        *self.layout.borrow_mut() = layout;
        *self.active_panel.borrow_mut() = active_panel;
        *self.closed_tabs.borrow_mut() = closed_tabs;
        self.sidebar_sections.set(sidebar_sections);
    }

    /// Resets the layout to the "blank" first-launch state:
    /// a single panel, a single tab on `$HOME`. Mutates the
    /// `Rc<RefCell<…>>` in place (same `Rc`s, so shared closures see the
    /// change). The caller repopulates the rows + refreshes the UI afterwards.
    pub fn reset_to_blank(&self) {
        // Reset → default columns and tab bar position from the
        // config (user settings).
        let default_cols = self.config.borrow().default_columns.clone();
        let default_mode = self.config.borrow().default_tab_bar_mode.min(2);
        *self.panels.borrow_mut() = vec![Panel::with_mode(home_dir(), default_cols, default_mode)];
        *self.layout.borrow_mut() = LayoutNode::Leaf { panel: 0 };
        *self.active_panel.borrow_mut() = 0;
        self.closed_tabs.borrow_mut().clear();
        self.external_drop_paths.borrow_mut().clear();
        *self.pending_file_drop.borrow_mut() = None;
        // Ad hoc state: no longer tied to a named workspace → title "Ranger".
        *self.current_workspace.borrow_mut() = None;
        *self.saved_workspace.borrow_mut() = None;
        self.sidebar_sections.set(SidebarSectionsState::default());
    }

    /// Reloads the saved snapshot of the current workspace from disk.
    /// Called at startup only; subsequent saves/loads then update
    /// the snapshot directly from the live state.
    fn reload_saved_workspace(&self) {
        let name = self.current_workspace.borrow().clone();
        let saved = name.as_deref().and_then(|name| {
            let dir = paths::workspaces_dir();
            let meta = workspace::list_named_workspaces(&dir)
                .into_iter()
                .find(|m| m.name == name)?;
            workspace::load_named_workspace(&dir, &meta.id)
                .ok()
                .map(|(_, ws)| ws)
        });
        *self.saved_workspace.borrow_mut() = saved;
    }

    /// The live state was just saved successfully: it becomes the new
    /// shared reference for the title and the Update button.
    fn remember_workspace_saved(&self) {
        *self.saved_workspace.borrow_mut() = Some(self.capture_workspace().sanitized());
    }

    /// Captures the current panels/tabs state into a serializable
    /// `WorkspaceState`.
    pub fn capture_workspace(&self) -> WorkspaceState {
        let panels = self.panels.borrow();
        let panel_states = panels
            .iter()
            .map(|p| PanelState {
                // The `layout` tree carries the proportions. `stretch` stays neutral
                // and is only used as a fallback if the tree is absent.
                stretch: 1.0,
                active_tab: p.tabs.active,
                tabs: p.tabs.tabs.iter().map(tab_to_state).collect(),
                columns: p.columns.clone(),
                tab_bar_mode: p.tab_bar_mode,
                vbar_width: p.vbar_user_w,
            })
            .collect();
        WorkspaceState {
            active_panel: *self.active_panel.borrow(),
            panels: panel_states,
            layout: Some(self.layout.borrow().clone()),
            workspace_name: self.current_workspace.borrow().clone(),
            closed_tabs: self.closed_tabs.borrow().clone(),
            sidebar_sections: self.sidebar_sections.get(),
        }
    }

    /// Marks this instance as ephemeral: all workspace persistence
    /// becomes no-ops so that a detached window never replaces the shared
    /// state of the main instance.
    pub fn set_ephemeral(&self) {
        self.ephemeral.set(true);
    }

    /// Serializes and writes the current workspace to disk. Errors are
    /// logged without preventing the application from closing.
    pub fn persist_workspace(&self) {
        if self.ephemeral.get() {
            return; // detached instance: never write the shared workspace
        }
        let ws = self.capture_workspace();
        if let Err(err) = ws.save(&paths::workspace_path()) {
            error!(error = %err, "saving workspace failed");
        }
    }

    pub fn snapshot_config(&self) -> Config {
        self.config.borrow().clone()
    }

    fn persist_config(&self, mutator: impl FnOnce(&mut Config)) {
        // Several instances share `config.toml`. Starting from the latest
        // on-disk version before each mutation prevents an older instance from
        // later overwriting its full snapshot and reverting, for example,
        // the global sidebar order chosen in another window.
        let path = paths::config_path();
        let mut cfg = match Config::load_or_default(&path) {
            Ok(latest) => latest,
            Err(err) => {
                error!(error = %err, "reloading config before update failed");
                self.config.borrow().clone()
            }
        };
        mutator(&mut cfg);
        let cfg = cfg.sanitized();
        *self.config.borrow_mut() = cfg.clone();
        if let Err(err) = cfg.save(&path) {
            error!(error = %err, "saving config failed");
        }
    }

    /// Current path of the active panel (clone of the active Tab).
    fn current_path(&self) -> PathBuf {
        self.with_tabs(|book| {
            let a = book.active;
            book.tabs[a].current_path.clone()
        })
    }

    /// Selection anchor of the active panel (-1 if none).
    fn selection_anchor(&self) -> i32 {
        self.with_tabs(|book| {
            let a = book.active;
            book.tabs[a].selection_anchor
        })
    }

    fn set_selection_anchor(&self, anchor: i32) {
        self.with_tabs_mut(|book| {
            let a = book.active;
            book.tabs[a].selection_anchor = anchor;
            // The keyboard cursor follows the selection made by the click, so
            // that arrow keys continue from the last clicked row.
            book.tabs[a].cursor = anchor;
        });
    }

    fn active_rows_model(&self) -> Rc<VecModel<FileRow>> {
        let panels = self.panels.borrow();
        let idx = *self.active_panel.borrow();
        panels[idx].rows_model.clone()
    }

    /// Read/write access to the active panel's TabBook.
    fn with_tabs<R>(&self, f: impl FnOnce(&TabBook) -> R) -> R {
        let panels = self.panels.borrow();
        let idx = *self.active_panel.borrow();
        f(&panels[idx].tabs)
    }
    fn with_tabs_mut<R>(&self, f: impl FnOnce(&mut TabBook) -> R) -> R {
        let mut panels = self.panels.borrow_mut();
        let idx = *self.active_panel.borrow();
        f(&mut panels[idx].tabs)
    }
    /// Is the active tab's "extension filter" mode enabled?
    /// Used to route keyboard input (shared with the type-ahead filter).
    fn active_ext_filter_on(&self) -> bool {
        self.with_tabs(|b| b.tabs[b.active].ext_filter_on)
    }

    fn remember_closed_tab(&self, tab: Tab) {
        let mut history = self.closed_tabs.borrow_mut();
        if history.len() >= workspace::MAX_CLOSED_TABS {
            history.remove(0);
        }
        history.push(tab_to_state(&tab));
    }

    fn pop_closed_tab(&self) -> Option<TabState> {
        self.closed_tabs.borrow_mut().pop()
    }

    fn has_closed_tabs(&self) -> bool {
        !self.closed_tabs.borrow().is_empty()
    }
}

/// Records all tabs of an explicitly closed panel. The tab that
/// was active is pushed last: "Reopen" therefore restores it first.
fn remember_closed_panel(state: &AppState, mut panel: Panel) {
    if panel.tabs.tabs.is_empty() {
        return;
    }
    let active = panel.tabs.active.min(panel.tabs.tabs.len() - 1);
    let active_tab = panel.tabs.tabs.remove(active);
    for tab in panel.tabs.tabs {
        state.remember_closed_tab(tab);
    }
    state.remember_closed_tab(active_tab);
}

// ---------- Callback installation ----------

pub fn install(window: &MainWindow, state: AppState) {
    // (The row model of the active panel no longer needs to be pushed as a
    // global `rows` — each panel carries its own model in
    // its PanelView via update_panels_ui.)

    // Initialization from config.
    window.on_filename_caret_offset(|name: SharedString, is_dir: bool| {
        filename_caret_offset(name.as_str(), is_dir)
    });
    let cfg = state.snapshot_config();
    apply_language(window, cfg.language);
    // One toast row per running operation, owned by the state so that the
    // worker threads can address a row by its operation id.
    window.set_ops(state.ops_model.clone().into());
    window.set_closed_tabs_available(state.has_closed_tabs());
    // The "Open with…" entry (native picker) only exists on Windows.
    window.set_platform_windows(cfg!(windows));
    window.set_theme_pref(match cfg.theme {
        Theme::Auto => 0,
        Theme::Light => 1,
        Theme::Dark => 2,
    });
    // Default tab bar position (settings).
    window.set_tabbar_default_pref(cfg.default_tab_bar_mode.min(2) as i32);
    // Tab path tooltip (settings) — unchecked by default.
    window.set_tab_tooltip_enabled(cfg.tab_path_tooltip);
    // "Unsaved changes" guard before loading, checked by
    // default in settings.
    window.set_ws_warn_unsaved(cfg.warn_unsaved_workspace);
    // Hybrid Previews mode: single-icon types stay compact.
    window.set_compact_preview_rows_enabled(cfg.compact_icon_rows_in_preview);
    // Timezone for the "Modified" column (settings): 0 = local time
    // (default), 1 = UTC. The effective offset is cached for the rows.
    window.set_clock_utc_pref(if cfg.clock_utc { 1 } else { 0 });
    refresh_mtime_offset(&state);
    // Windows shell context menu (shell extensions) — checked by default.
    window.set_shell_menu_enabled(cfg.shell_ctx_menu);
    // Left panel: any positive state opens the unified Places +
    // Favorites panel. This normalization also accepts the serialized value 2.
    window.set_left_panel(if cfg.left_panel >= 1 { 1 } else { 0 });
    window.set_sidebar_width(cfg.sidebar_width.max(140) as f32);
    push_sidebar_sections_ui(window, &state);
    refresh_sidebar(window);
    // The Slint chevrons immediately update the live workspace state.
    // The title then re-reads the single dirty source; the Workspaces panel
    // already resynchronizes each time it opens via `ws-refresh`.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_sidebar_section_state_changed(move |section: i32, collapsed: bool| {
            let mut sections = st.sidebar_sections.get();
            let target = match SidebarSection::from_index(section as usize) {
                Some(SidebarSection::Drives) => &mut sections.drives_collapsed,
                Some(SidebarSection::Shortcuts) => &mut sections.shortcuts_collapsed,
                Some(SidebarSection::Favorites) => &mut sections.favorites_collapsed,
                Some(SidebarSection::Network) => &mut sections.network_collapsed,
                _ => return,
            };
            if *target != collapsed {
                *target = collapsed;
                st.sidebar_sections.set(sections);
                if let Some(w) = weak.upgrade() {
                    update_window_title(&w, &st);
                }
            }
        });
    }
    // The target is the FINAL index (0..3) among the four sections. The order is
    // a GLOBAL preference: a single config write on drop, no
    // workspace snapshot change or churn during the moves.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_sidebar_section_reordered(move |section: i32, target_index: i32| {
            let Some(section) = usize::try_from(section)
                .ok()
                .and_then(SidebarSection::from_index)
            else {
                return;
            };
            let Ok(target_index) = usize::try_from(target_index) else {
                return;
            };
            if target_index >= SidebarSection::COUNT {
                return;
            }

            let mut order = st.snapshot_config().sidebar_section_order;
            if SidebarSection::reorder(&mut order, section, target_index) {
                st.persist_config(|cfg| cfg.sidebar_section_order = order);
                if let Some(w) = weak.upgrade() {
                    push_sidebar_sections_ui(&w, &st);
                }
            }
        });
    }
    // Initial drives signature, so the first poll doesn't trigger
    // an unnecessary rebuild.
    state.last_drives_sig.set(sidebar_drives_signature());
    // The Shell/WPD can block on a slow driver: the initial phone scan
    // only starts after the initial snapshot and off the UI thread.
    // Its revision will cause the sidebar to rebuild on the next poll if needed.
    #[cfg(windows)]
    crate::winportable::request_refresh();
    push_favorites_ui(window, &state);
    push_openers_ui(window, &state);
    push_recipes_ui(window, &state);
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_sidebar_place_clicked(move |path: SharedString, kind: i32| {
            let Some(w) = weak.upgrade() else { return };
            // kind 5 = WPD/MTP portable device. This is NOT a file
            // path: its parsing-name must stay opaque and be handed off to the
            // Windows Shell, in a dedicated Explorer window.
            #[cfg(windows)]
            if kind == 5 {
                if let Err(err) = crate::winportable::open(path.as_str()) {
                    error!(error = %err, "open portable device");
                }
                return;
            }
            // kind 3 = trash. On Windows it's virtual (empty path)
            // → open it in Explorer; elsewhere, navigate into it.
            #[cfg(windows)]
            if kind == 3 {
                if let Err(err) = actions::open_trash() {
                    error!(error = %err, "open recycle bin");
                }
                return;
            }
            let _ = kind;
            let p = PathBuf::from(path.to_string());
            if p.as_os_str().is_empty() {
                return;
            }
            // `is_dir()` on a NETWORK path = a full SMB round trip (possibly
            // even a session reconnect: seconds) ON THE UI THREAD, before the
            // listing even happens. We only pre-check LOCAL paths (instant); the network
            // case is resolved by the listing itself (failure → "unavailable" banner,
            // same safety net as Explorer).
            if ranger_core::places::is_network_path(&p) || p.is_dir() {
                load_directory(&w, &st, &p, true);
            } else {
                warn!(path = %p.display(), "sidebar: target no longer exists, ignoring");
            }
        });
    }
    // Middle-click on a sidebar shortcut / drive → opens the target
    // in a NEW tab of the active view (same pattern as `on_action_open_new_tab`).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_place_open_new_tab(move |path: SharedString, kind: i32| {
            let Some(w) = weak.upgrade() else { return };
            // Trash (3) and WPD device (5) have no path navigable
            // by Ranger: middle-click "new tab" doesn't apply.
            if kind == 3 || kind == 5 {
                return;
            }
            let p = PathBuf::from(path.to_string());
            // Same logic as a simple click: no blocking `is_dir()` on a
            // network path (the listing resolves it, banner on failure).
            if p.as_os_str().is_empty()
                || (!ranger_core::places::is_network_path(&p) && !p.is_dir())
            {
                return;
            }
            let opened = st.with_tabs_mut(|book| {
                let a = book.open_after_active(p);
                book.tabs[a].current_path.clone()
            });
            load_directory(&w, &st, &opened, false);
        });
    }
    // Re-scan of places (mounted/unmounted volumes) when the sidebar opens.
    {
        let weak = window.as_weak();
        window.on_sidebar_refresh(move || {
            if let Some(w) = weak.upgrade() {
                #[cfg(windows)]
                crate::winportable::request_refresh();
                refresh_sidebar(&w);
            }
        });
    }
    // Cross-instance IPC initialization: called by a Slint Timer shortly
    // after startup (the window exists → HWND available), RETRIED as long as
    // the window isn't found (returns `true` = initialized, the Timer
    // stops). Marks + subclasses the window to receive tabs
    // transferred from another Ranger instance. No-op outside Windows.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_init_ipc(move || -> bool {
            let tab_state = st.clone();
            let tab_weak = weak.clone();
            let ipc_ready = crate::winmsg::init(move |incoming| {
                let Some(w) = tab_weak.upgrade() else { return };
                match incoming {
                    crate::winmsg::Incoming::Transfer(payload) => {
                        // `on_tab_received` handles the favorites hover itself (replays
                        // the drop point then clears the preview).
                        on_tab_received(&w, &tab_state, &payload);
                    }
                    // Hover from another instance: simulates a local drag at
                    // this point → the insertion preview lights up in the targeted panel.
                    crate::winmsg::Incoming::Hover(sx, sy) => set_external_hover(&w, sx, sy),
                    crate::winmsg::Incoming::HoverEnd => clear_external_hover(&w),
                }
            });
            if !ipc_ready {
                return false;
            }

            // OLE file target: replaces the winit target (which doesn't surface
            // DragOver) to get a real-time folder/executable hover
            // and reuse the Move/Copy/Link menu between Ranger instances.
            #[cfg(windows)]
            {
                let Some(hwnd) = crate::winmsg::self_hwnd() else {
                    return false;
                };
                let file_state = st.clone();
                let file_weak = weak.clone();
                let registered = crate::winddrag::init_drop_target(hwnd, move |incoming| {
                    match incoming {
                        crate::winddrag::IncomingFileDrag::Hover {
                            screen_x,
                            screen_y,
                            copy,
                        } => {
                            if let Some(w) = file_weak.upgrade() {
                                set_external_file_hover(&w, screen_x, screen_y, copy);
                            }
                        }
                        crate::winddrag::IncomingFileDrag::Leave => {
                            if let Some(w) = file_weak.upgrade() {
                                clear_external_file_hover(&w);
                            }
                        }
                        crate::winddrag::IncomingFileDrag::Drop {
                            paths,
                            screen_x,
                            screen_y,
                            copy,
                            staging,
                        } => {
                            // Own the staging directory before deferring. If
                            // the window closes before the next UI tick, RAII
                            // still removes it instead of leaking temp data.
                            let staging = staging.map(|staging| IncomingDropStaging {
                                cleanup: TransientDropGuard::new(staging.temp_dir),
                                op: if staging.copy_from_staging {
                                    ClipOp::Copy
                                } else {
                                    ClipOp::Cut
                                },
                            });
                            // The OLE source still holds the capture until
                            // IDropTarget::Drop returns control. Focus and open
                            // the menu on the next tick, after OLE has released
                            // capture, so its first hover/click is usable.
                            let drop_state = file_state.clone();
                            let drop_weak = file_weak.clone();
                            defer(move || {
                                let Some(w) = drop_weak.upgrade() else { return };
                                crate::winmsg::focus_self();
                                on_external_file_drop(
                                    &w,
                                    &drop_state,
                                    paths,
                                    screen_x,
                                    screen_y,
                                    copy,
                                    staging,
                                );
                            });
                        }
                        crate::winddrag::IncomingFileDrag::ExternalDropFailed => {
                            let drop_state = file_state.clone();
                            let drop_weak = file_weak.clone();
                            defer(move || {
                                let Some(w) = drop_weak.upgrade() else { return };
                                crate::winmsg::focus_self();
                                clear_external_file_hover(&w);
                                let lang = drop_state.snapshot_config().language;
                                notice(
                                    &w,
                                    i18n::tr(lang, "virtual_drop_failed"),
                                    NoticeKind::Error,
                                );
                            });
                        }
                    }
                });
                if registered {
                    // This delay starts while the event loop is already
                    // running, unlike the declarative 250 ms startup timer.
                    // Rebinding once avoids winit reclaiming the HWND's minimal
                    // CF_HDROP target while the native window is finalized.
                    slint::Timer::single_shot(std::time::Duration::from_millis(500), move || {
                        crate::winddrag::rebind_drop_target(hwnd);
                    });
                }
                registered
            }
            #[cfg(not(windows))]
            true
        });
    }
    // Scrolling of a panel's tab bar: the view reports
    // `tabs-flick.viewport-x` (plain) on every change → used for the hit-test of the
    // insertion gap for a tab received from another instance.
    {
        let st = state.clone();
        window.on_panel_tabs_scrolled(move |idx: i32, vx: f32| {
            let mut panels = st.panels.borrow_mut();
            if let Some(p) = panels.get_mut(idx.max(0) as usize) {
                p.tabs_viewport_x = vx;
            }
        });
    }
    // LIGHTWEIGHT periodic poll of drives while the "Drives" sidebar is
    // open: external changes (subst, USB, `net use`, mount)
    // don't always emit a system event → we compare the signature and
    // only rebuild the model if it has moved. See the Timer on the Slint side.
    {
        let weak = window.as_weak();
        let sig = state.last_drives_sig.clone();
        window.on_poll_drives(move || {
            let Some(w) = weak.upgrade() else { return };
            // Triggers the next scan without waiting for its result; `cur` only
            // reads the atomic cache from the previous scan.
            #[cfg(windows)]
            crate::winportable::request_refresh();
            let cur = sidebar_drives_signature();
            if cur != sig.get() {
                sig.set(cur);
                refresh_sidebar(&w);
            }
        });
    }
    // Re-check of unavailable tabs after a drive change.
    // `drives_signature` avoids a potentially slow network `is_dir` as long
    // as no mount has changed. The Slint timer stays active if needed.
    {
        let st = state.clone();
        let weak = window.as_weak();
        let last_sig = std::cell::Cell::new(ranger_core::places::drives_signature());
        window.on_recheck_unavailable(move || {
            let Some(w) = weak.upgrade() else { return };
            let cur = ranger_core::places::drives_signature();
            if cur == last_sig.get() {
                return; // nothing changed on the drives side → no blocking test
            }
            last_sig.set(cur);
            recheck_unavailable_panels(&w, &st);
        });
    }

    // Eject / network disconnect -----
    // These operations can block (unmounting, power loss, network
    // I/O) → background thread, then back to the UI (toast + sidebar re-scan).
    window.on_place_copy_path(move |path: SharedString| {
        if let Err(err) = crate::actions::copy_to_clipboard(&path) {
            warn!(error = %err, "copy path failed");
        }
    });
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_drive_eject(move |device: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            spawn_eject(&w, &st, device.to_string(), EjectOp::SafeRemove);
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_drive_disconnect(move |device: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            spawn_eject(&w, &st, device.to_string(), EjectOp::Disconnect);
        });
    }

    // Tree favorites -----
    // Click on a row: container → collapses/expands; favorite → new tab.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_row_activate(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let id = id.to_string();
            let info = st
                .favorites
                .borrow()
                .nodes
                .iter()
                .find(|n| n.id == id)
                .map(|n| (n.is_container(), n.expanded));
            match info {
                Some((true, expanded)) => {
                    st.favorites.borrow_mut().set_expanded(&id, !expanded);
                    save_favorites(&st);
                    push_favorites_ui(&w, &st);
                }
                // LEFT click on a favorite (leaf) → navigates the ACTIVE tab
                // (overwrites it). MIDDLE click (on_fav_row_middle) keeps the new
                // tab.
                Some((false, _)) => fav_open_here(&w, &st, &id),
                None => {}
            }
        });
    }
    // Middle click: opens a favorite in a new tab.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_row_middle(move |id: SharedString| {
            if let Some(w) = weak.upgrade() {
                fav_open(&w, &st, &id);
            }
        });
    }
    // Open a favorite (context menu).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_open(move |id: SharedString| {
            if let Some(w) = weak.upgrade() {
                fav_open(&w, &st, &id);
            }
        });
    }
    // Container: open ALL descendant favorites (one tab each).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_open_all(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let paths = st.favorites.borrow().descendant_paths(&id);
            for p in paths {
                fav_open_path_new_tab(&w, &st, PathBuf::from(p));
            }
        });
    }
    // Naming or renaming a container/favorite via a modal popup. Validation
    // reuses `ops::is_valid_entry_name` (empty, separators, "." and "..").
    {
        let weak = window.as_weak();
        window.on_fav_name_check(move |name: SharedString| {
            if let Some(w) = weak.upgrade() {
                w.set_fav_name_invalid(!ops::is_valid_entry_name(name.trim()));
            }
        });
    }
    // Confirms the popup: creates a (sub-)container or renames a node, with the
    // same protections as the New folder / Rename popups.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_name_confirm(move || {
            let Some(w) = weak.upgrade() else { return };
            let raw = w.get_fav_name_value();
            let name = raw.trim();
            // Final guard (the button is already disabled on an invalid name).
            if !ops::is_valid_entry_name(name) {
                return;
            }
            let target = w.get_fav_name_target().to_string();
            if w.get_fav_name_rename() {
                st.favorites.borrow_mut().rename(&target, name);
            } else {
                st.favorites.borrow_mut().add_container(&target, name);
            }
            save_favorites(&st);
            push_favorites_ui(&w, &st);
            w.set_fav_name_open(false);
        });
    }
    // Collapse all.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_collapse_all(move || {
            let Some(w) = weak.upgrade() else { return };
            st.favorites.borrow_mut().collapse_all();
            save_favorites(&st);
            push_favorites_ui(&w, &st);
        });
    }
    // Expand all.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_expand_all(move || {
            let Some(w) = weak.upgrade() else { return };
            st.favorites.borrow_mut().expand_all();
            save_favorites(&st);
            push_favorites_ui(&w, &st);
        });
    }
    // Delete a node (+ descendants).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_delete(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            st.favorites.borrow_mut().delete(&id);
            save_favorites(&st);
            if w.get_fav_selected_id() == id {
                w.set_fav_selected_id(SharedString::new());
            }
            push_favorites_ui(&w, &st);
        });
    }
    // Copy a favorite's path to the clipboard.
    {
        let st = state.clone();
        window.on_fav_copy_path(move |id: SharedString| {
            if let Some(p) = st.favorites.borrow().path_of(&id)
                && let Err(err) = actions::copy_to_clipboard(&p)
            {
                error!(error = %err, "fav copy path failed");
            }
        });
    }
    // Save the current tab as a favorite (popup).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_save_tab(move |panel: i32, tab: i32| {
            let Some(w) = weak.upgrade() else { return };
            let path = {
                let panels = st.panels.borrow();
                panels.get(panel as usize).and_then(|p| {
                    p.tabs
                        .tabs
                        .get(tab as usize)
                        .map(|t| t.current_path.clone())
                })
            };
            if let Some(path) = path {
                open_fav_save_popup(&w, &st, vec![path]);
            }
        });
    }
    // Save ALL tabs of the view as favorites (multi popup).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_save_all_tabs(move |panel: i32| {
            let Some(w) = weak.upgrade() else { return };
            let paths: Vec<PathBuf> = {
                let panels = st.panels.borrow();
                panels
                    .get(panel as usize)
                    .map(|p| p.tabs.tabs.iter().map(|t| t.current_path.clone()).collect())
                    .unwrap_or_default()
            };
            open_fav_save_popup(&w, &st, paths);
        });
    }
    // Dropping a tab by DRAG onto a favorites folder: a "direct"
    // duplicate of the right-click "save as favorite" (the target container is
    // chosen by the drop point → no popup).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_tab_to_favorite(move |panel: i32, tab: i32, container: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let path = {
                let panels = st.panels.borrow();
                panels.get(panel.max(0) as usize).and_then(|p| {
                    p.tabs
                        .tabs
                        .get(tab.max(0) as usize)
                        .map(|t| t.current_path.clone())
                })
            };
            let Some(path) = path else { return };
            add_paths_to_favorite(&w, &st, std::slice::from_ref(&path), &container);
        });
    }
    // Dropping the SELECTION of a view (files/folders) onto a favorites
    // folder via DRAG. The file-drag target is a favorites container
    // → the selection is saved there (no file operation). Same paths
    // as the native drag (`panel_selected_paths`).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_file_to_favorite(move |panel: i32, container: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let paths = panel_selected_paths(&st, panel.max(0) as usize);
            add_paths_to_favorite(&w, &st, &paths, &container);
        });
    }
    // Add the current selection (files) to favorites (popup).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_add_selection(move || {
            if let Some(w) = weak.upgrade() {
                open_fav_save_popup(&w, &st, selected_paths(&st));
            }
        });
    }
    // Create a container "on the fly" from the popup and select it.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_save_new_container(move |name: SharedString, parent_index: i32| {
            let Some(w) = weak.upgrade() else { return };
            // Parent container chosen in the sub-popup (index 0 = root).
            let parent = st
                .fav_container_ids
                .borrow()
                .get(parent_index.max(0) as usize)
                .cloned()
                .unwrap_or_default();
            let id = st.favorites.borrow_mut().add_container(&parent, &name);
            save_favorites(&st);
            push_favorites_ui(&w, &st);
            // Selects the freshly created container as the destination.
            if let Some(id) = id {
                let idx = st
                    .fav_container_ids
                    .borrow()
                    .iter()
                    .position(|c| *c == id)
                    .unwrap_or(0) as i32;
                w.set_fav_container_index(idx);
            }
        });
    }
    // Confirm the save: creates the favorite(s) in the chosen container.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_save_commit(move |alias: SharedString, index: i32| {
            let Some(w) = weak.upgrade() else { return };
            let container = st
                .fav_container_ids
                .borrow()
                .get(index.max(0) as usize)
                .cloned()
                .unwrap_or_default();
            let paths = std::mem::take(&mut *st.fav_save_pending.borrow_mut());
            let multi = paths.len() > 1;
            // Deduplication: we do NOT add a path already present in the
            // target container (otherwise a silent duplicate). We count the additions
            // to choose the toast (added vs already present).
            let mut added = 0usize;
            {
                let mut fav = st.favorites.borrow_mut();
                for p in &paths {
                    let path_str = p.display().to_string();
                    if fav.container_has_path(&container, &path_str) {
                        continue; // already in this favorites folder → ignored
                    }
                    let alias = if !multi {
                        let a = alias.to_string();
                        if a.trim().is_empty() {
                            default_alias(p)
                        } else {
                            a
                        }
                    } else {
                        default_alias(p)
                    };
                    fav.add_favorite(&container, &alias, &path_str);
                    added += 1;
                }
            }
            let lang = st.config.borrow().language;
            if added > 0 {
                save_favorites(&st);
                push_favorites_ui(&w, &st);
                notice(
                    &w,
                    i18n::strings_for(lang).fav_toast_added.clone(),
                    NoticeKind::FavAdded,
                );
            } else {
                // Nothing added = everything already existed → neutral info (bookmark accent),
                // not an error.
                notice(
                    &w,
                    i18n::strings_for(lang).fav_toast_exists.clone(),
                    NoticeKind::FavExists,
                );
            }
        });
    }
    // Reorder drag: updates the insertion indicator.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_drag_move(move |row_idx: i32, cur_y: f32, row_top: f32| {
            let Some(w) = weak.upgrade() else { return };
            let flat = st.favorites.borrow().flatten();
            let count = flat.len() as i32;
            let (ti, zone) = fav_drag_target(cur_y, row_top, row_idx, count);
            w.set_fav_drag_active(true);
            w.set_fav_drag_target_index(ti);
            w.set_fav_drag_zone(zone);
            // Collapsed (non-empty) container targeted "inside" → auto-expand candidate.
            let hover = flat
                .get(ti.max(0) as usize)
                .filter(|n| n.is_container && n.has_children && !n.expanded && zone == 1)
                .map(|n| n.id.clone())
                .unwrap_or_default();
            w.set_fav_drag_hover_container(hover.into());
        });
    }
    // Reorder drag: drop → actual move of the node.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_drag_drop(move |row_idx: i32, cur_y: f32, row_top: f32| {
            let Some(w) = weak.upgrade() else { return };
            let (src_id, count) = {
                let fav = st.favorites.borrow();
                let flat = fav.flatten();
                (
                    flat.get(row_idx.max(0) as usize).map(|n| n.id.clone()),
                    flat.len() as i32,
                )
            };
            w.set_fav_drag_active(false);
            w.set_fav_drag_target_index(-1);
            w.set_fav_drag_hover_container(SharedString::new());
            if let Some(src_id) = src_id {
                let (ti, zone) = fav_drag_target(cur_y, row_top, row_idx, count);
                if fav_perform_move(&st, &src_id, ti, zone) {
                    save_favorites(&st);
                    push_favorites_ui(&w, &st);
                }
            }
        });
    }
    // Auto-expand during a drag: expands a container IN PLACE (inserting the
    // subtree into the existing VecModel → row components, including the
    // dragged row and its grab handle, survive; `push_favorites_ui` would destroy them).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_fav_expand(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let id = id.to_string();
            let model = w.get_fav_nodes();
            // Graceful no-op if the model isn't a VecModel (the drag stays sane).
            let Some(vm) = model.as_any().downcast_ref::<VecModel<FavNode>>() else {
                return;
            };
            // Locates the container's row in the visible list.
            let mut ti = None;
            for i in 0..vm.row_count() {
                if vm.row_data(i).map(|r| r.id == id).unwrap_or(false) {
                    ti = Some(i);
                    break;
                }
            }
            let Some(ti) = ti else { return };
            let Some(mut crow) = vm.row_data(ti) else {
                return;
            };
            if !crow.is_container || crow.expanded {
                return;
            }
            let base_depth = crow.depth + 1;
            let children = {
                let mut fav = st.favorites.borrow_mut();
                fav.set_expanded(&id, true);
                fav.flatten_children(&id, base_depth)
            };
            save_favorites(&st);
            crow.expanded = true;
            vm.set_row_data(ti, crow);
            for (k, fc) in children.iter().enumerate() {
                vm.insert(ti + 1 + k, flat_to_favnode(fc));
            }
            w.set_fav_drag_hover_container(SharedString::new());
        });
    }
    // default columns (Settings) — initial state + toggle.
    push_settings_columns(window, cfg.language, &cfg.default_columns);
    // recursive mtime depth (initial state + handler).
    window.set_rmtime_depth(cfg.recursive_mtime_depth);
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_rmtime_depth_changed(move |v: i32| {
            let Some(w) = weak.upgrade() else { return };
            let v = v.clamp(0, 8);
            st.persist_config(|c| c.recursive_mtime_depth = v);
            w.set_rmtime_depth(v);
            // Depth changed → the mtime values are stale. Re-list (restoring the
            // folders' OWN mtime), then `request_folder_stats` re-applies them if
            // depth > 0.
            st.rmtime_cache.borrow_mut().clear();
            refresh_all_panels(&w, &st);
        });
    }
    // recursive size depth (initial state + handler), mirroring the mtime one.
    window.set_size_depth(cfg.recursive_size_depth);
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_size_depth_changed(move |v: i32| {
            let Some(w) = weak.upgrade() else { return };
            let v = v.clamp(0, 8);
            st.persist_config(|c| c.recursive_size_depth = v);
            w.set_size_depth(v);
            // Depth changed → the recursive sizes are stale; re-list, then
            // `request_folder_stats` recomputes them if depth > 0.
            st.size_cache.borrow_mut().clear();
            refresh_all_panels(&w, &st);
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_default_column_toggle(move |id: SharedString, visible: bool| {
            let id = id.to_string();
            if id == "name" {
                return; // anchor can't be unchecked
            }
            st.persist_config(|c| {
                c.default_columns = columns::sanitize(std::mem::take(&mut c.default_columns));
                if let Some(col) = c.default_columns.iter_mut().find(|x| x.id == id) {
                    col.visible = visible;
                }
            });
            let (lang, cols) = {
                let c = st.config.borrow();
                (c.language, c.default_columns.clone())
            };
            if let Some(w) = weak.upgrade() {
                push_settings_columns(&w, lang, &cols);
            }
        });
    }
    // configurable shortcuts (settings) — list + rebind/reset/search.
    push_shortcuts_ui(window, &state);
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_shortcut_search(move |text: SharedString| {
            *st.shortcut_filter.borrow_mut() = text.to_string();
            if let Some(w) = weak.upgrade() {
                push_shortcuts_ui(&w, &st);
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_shortcut_rebind(move |action_id: SharedString, chord_raw: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            w.set_shortcut_capturing(SharedString::new());
            let action_id = action_id.to_string();
            // Normalizes the entered combination; ignores it if there's no real key.
            let Some(chord) = Chord::parse(chord_raw.as_str()).map(|c| c.serialize()) else {
                return;
            };
            // Conflict with another action?
            let other = st
                .keymap
                .borrow()
                .conflict(&action_id, &chord)
                .map(|s| s.to_string());
            if let Some(other_id) = other {
                let lang = st.config.borrow().language;
                let other_name = i18n::shortcut_action_name(lang, &other_id);
                let msg = i18n::shortcut_conflict_message(lang, &other_name);
                *st.pending_conflict.borrow_mut() = Some((action_id.clone(), other_id, chord));
                w.set_shortcut_conflict_action(action_id.into());
                w.set_shortcut_conflict_message(msg.into());
                return;
            }
            apply_shortcut_override(&st, &action_id, &chord);
            push_shortcuts_ui(&w, &st);
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_shortcut_resolve_conflict(move |reassign: bool| {
            let Some(w) = weak.upgrade() else { return };
            if let Some((action_id, other_id, chord)) = st.pending_conflict.borrow_mut().take()
                && reassign
            {
                apply_shortcut_override(&st, &action_id, &chord);
                // Frees the other action (marks it "unassigned").
                st.persist_config(|c| {
                    c.shortcut_overrides.insert(other_id.clone(), String::new());
                });
                st.rebuild_keymap();
            }
            w.set_shortcut_conflict_action(SharedString::new());
            w.set_shortcut_conflict_message(SharedString::new());
            push_shortcuts_ui(&w, &st);
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_shortcut_reset(move |action_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let id = action_id.to_string();
            // Resetting = (re)assigning the action ITS factory default — via the
            // SAME assignment path as capture (single uniqueness rule:
            // no path can create a duplicate). If this default is already held
            // by ANOTHER action, we open the same conflict banner
            // ("Reassign here" / "Keep") instead of silently restoring it
            // (which would produce two actions sharing the same combination).
            let default = shortcuts::ACTIONS
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.default)
                .unwrap_or("");
            if !default.is_empty() {
                let other = st
                    .keymap
                    .borrow()
                    .conflict(&id, default)
                    .map(|s| s.to_string());
                if let Some(other_id) = other {
                    let lang = st.config.borrow().language;
                    let other_name = i18n::shortcut_action_name(lang, &other_id);
                    let msg = i18n::shortcut_conflict_message(lang, &other_name);
                    *st.pending_conflict.borrow_mut() =
                        Some((id.clone(), other_id, default.to_string()));
                    w.set_shortcut_conflict_action(id.into());
                    w.set_shortcut_conflict_message(msg.into());
                    return;
                }
            }
            // Free default (or "unassigned") → apply it (removes the override).
            apply_shortcut_override(&st, &id, default);
            push_shortcuts_ui(&w, &st);
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_shortcut_unassign(move |action_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            // Unassigning = setting an EMPTY override (no conflict possible). If
            // the action has a factory default, `overridden` becomes true → the
            // "restore default" button (circular arrow) shows up in the list.
            apply_shortcut_override(&st, action_id.as_ref(), "");
            push_shortcuts_ui(&w, &st);
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_shortcut_reset_all(move || {
            let Some(w) = weak.upgrade() else { return };
            st.persist_config(|c| c.shortcut_overrides.clear());
            st.rebuild_keymap();
            w.set_shortcut_conflict_action(SharedString::new());
            push_shortcuts_ui(&w, &st);
        });
    }

    // Empty the trash (permanent — already confirmed on the UI side in 2 steps).
    // Background thread: enumerating + shell-purging a full
    // trash can take seconds — the UI thread doesn't wait.
    {
        window.on_empty_trash(move || {
            std::thread::spawn(|| match ranger_core::fs::ops::empty_trash() {
                Ok(()) => info!("trash emptied"),
                Err(err) => error!(error = %err, "empty_trash failed"),
            });
        });
    }
    // Sorting is stored per panel and pushed by `update_panels_ui`.

    // ----- Language -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_language_changed(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let lang = Lang::all().get(idx as usize).copied().unwrap_or_default();
            info!(lang = lang.code(), "language changed");
            // Commit the language to config BEFORE any refresh: `update_panels_ui`
            // (via `refresh_listing`) reads the language back from CONFIG for the
            // VIEW texts (footer, columns, title). Otherwise they stayed one
            // language behind ("you have to change it twice").
            st.persist_config(|c| c.language = lang);
            apply_language(&w, lang);
            // Refresh: footer, sizes AND column labels (panels
            // + Settings) depend on the language.
            let cur = st.current_path();
            if !cur.as_os_str().is_empty() {
                refresh_listing(&w, &st, &cur);
            }
            push_settings_columns(&w, lang, &st.config.borrow().default_columns);
            push_shortcuts_ui(&w, &st); // localized shortcut labels
            push_recipes_ui(&w, &st); // localized recipe labels
        });
    }

    // ----- Theme -----
    {
        let st = state.clone();
        window.on_theme_changed(move |idx: i32| {
            let theme = Theme::all().get(idx as usize).copied().unwrap_or_default();
            info!(theme = theme.code(), "theme changed");
            st.persist_config(|c| c.theme = theme);
        });
    }

    // UI zoom (settings): applies live + persists -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ui_scale_changed(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let factor = UI_SCALE_PRESETS
                .get(idx.max(0) as usize)
                .copied()
                .unwrap_or(1.0);
            info!(factor, "UI scale changed");
            st.persist_config(|c| c.ui_scale = factor);
            apply_ui_scale(&w, &st, factor);
        });
    }
    // Zoom picker: labels (language-independent) + index from the
    // config, then DEFERRED application — the window only has its real OS scale
    // once realized, not during this setup (before `run()`).
    {
        let ui_scale = state.config.borrow().ui_scale;
        window.set_ui_scale_labels(ModelRc::new(VecModel::from(ui_scale_labels())));
        window.set_ui_scale_index(ui_scale_nearest_index(ui_scale));
        let st = state.clone();
        let weak = window.as_weak();
        slint::Timer::single_shot(std::time::Duration::from_millis(50), move || {
            if let Some(w) = weak.upgrade() {
                apply_ui_scale(&w, &st, ui_scale);
            }
        });
    }
    // "Video thumbnails (ffmpeg)" section (Linux): initial detection + button
    // "Recheck" + "copy" button for an install command.
    apply_ffmpeg_info(window);
    {
        let weak = window.as_weak();
        window.on_ffmpeg_recheck(move || {
            if let Some(w) = weak.upgrade() {
                apply_ffmpeg_info(&w);
            }
        });
    }
    {
        window.on_settings_copy(move |text: SharedString| {
            if let Err(err) = actions::copy_to_clipboard(&text) {
                error!(error = %err, "clipboard copy (setting) failed");
            }
        });
    }

    // DEFAULT tab bar position (settings) -----
    // Only applies to views created "from scratch" (new workspace /
    // reset); existing views keep their PER-VIEW setting.
    {
        let st = state.clone();
        window.on_tabbar_default_changed(move |idx: i32| {
            let mode = idx.clamp(0, 2) as u8;
            info!(mode, "default tab bar position changed");
            st.persist_config(|c| c.default_tab_bar_mode = mode);
        });
    }
    // Tab path tooltip (settings): opt-in, persisted.
    {
        let st = state.clone();
        window.on_tab_tooltip_changed(move |on: bool| {
            info!(on, "tab path tooltip toggled");
            st.persist_config(|c| c.tab_path_tooltip = on);
        });
    }
    // Guard before loading another workspace (settings): persisted.
    {
        let st = state.clone();
        window.on_ws_warn_unsaved_changed(move |on: bool| {
            info!(on, "unsaved-workspace warning toggled");
            st.persist_config(|c| c.warn_unsaved_workspace = on);
        });
    }
    // Hybrid height of Previews mode: persists then only recomputes the
    // geometry of models already in memory (no relisting, no I/O).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_compact_preview_rows_changed(move |on: bool| {
            let Some(w) = weak.upgrade() else { return };
            info!(on, "compact icon rows in preview toggled");
            st.persist_config(|c| c.compact_icon_rows_in_preview = on);
            refresh_preview_panel_visuals(&st);
            update_panels_ui(&w, &st);
            request_thumbnails(&st);
        });
    }
    // Timezone for the "Modified" column (settings): 0 = local, 1 = UTC.
    // Persists, recomputes the offset, then rebuilds the rows of ALL
    // panels to reflect the new timezone immediately.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_clock_mode_changed(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            info!(idx, "modified-column time zone changed");
            st.persist_config(|c| c.clock_utc = idx == 1);
            refresh_mtime_offset(&st);
            refresh_all_panels(&w, &st);
        });
    }
    // Windows context menu (settings): persisted kill-switch.
    {
        let st = state.clone();
        window.on_shell_menu_changed(move |on: bool| {
            info!(on, "windows shell context menu toggled");
            st.persist_config(|c| c.shell_ctx_menu = on);
        });
    }

    // ----- Navigation -----
    install_nav_callback(window, &state, NavAction::Back);
    install_nav_callback(window, &state, NavAction::Forward);
    install_nav_callback(window, &state, NavAction::Parent);
    install_nav_callback(window, &state, NavAction::Home);
    install_nav_callback(window, &state, NavAction::Refresh);

    // ----- Editable address bar -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_navigate_to(move |path: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let raw = path.to_string();
            // Expansion of `~` → `$HOME` (user convenience).
            let p = if raw == "~" {
                home_dir()
            } else if let Some(rest) = raw.strip_prefix("~/") {
                home_dir().join(rest)
            } else {
                PathBuf::from(raw)
            };
            // NEVER pre-test a network path on the UI thread: `is_dir`
            // can block on SMB for several seconds. The listing worker
            // will decide and prompt for credentials if needed.
            let network = rfs::is_unc_path(&p) || ranger_core::places::is_network_path(&p);
            if network || rfs::is_listable(&p) {
                load_directory(&w, &st, &p, true);
            } else {
                // End of the silent failure (B-002): a toast instead of ignoring it.
                warn!(path = %p.display(), "URL bar: path not listable");
                let lang = st.config.borrow().language;
                notice(
                    &w,
                    i18n::strings_for(lang).fav_toast_missing.clone(),
                    NoticeKind::FavMissing,
                );
            }
        });
    }
    // URL bar right-click menu: paste a path / copy.
    {
        let weak = window.as_weak();
        window.on_url_paste(move || {
            let Some(w) = weak.upgrade() else { return };
            // Paste the clipboard INTO the URL field (without submitting): the
            // panel is already in edit mode (the right-click put it there), and
            // the user submits themselves with Enter.
            if let Some(text) = actions::read_clipboard() {
                w.set_url_edit_text(text.trim().to_string().into());
            }
        });
    }
    {
        let st = state.clone();
        window.on_url_copy(move || {
            let cur = st.current_path();
            if let Err(err) = actions::copy_to_clipboard(&cur.display().to_string()) {
                error!(error = %err, "url copy path failed");
            }
        });
    }

    // ----- Row activation (double-click) -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_row_activated(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let rows = st.active_rows_model();
            let Some(row) = slint::Model::row_data(&rows, idx as usize) else {
                return;
            };
            // `row.path` = parent folder path; we append the name to it.
            let target = PathBuf::from(row.path.to_string()).join(row.name.as_str());
            if row.is_dir {
                load_directory(&w, &st, &target, true);
            } else if !open_shortcut_as_tab(&w, &st, &target) {
                // File: SAME logic as Enter / the "Open" menu entry — Ranger's
                // per-extension default if set, otherwise the OS default. (Double-click
                // used to ignore the Ranger default, which made the Settings field
                // look like it did nothing.) A folder .lnk shortcut has already
                // been opened as a tab above.
                open_file_default(&st, &[target]);
            }
        });
    }

    // Selection: single / Ctrl / Shift-click -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_row_clicked(move |idx: i32| {
            let count = selection_set_only(&st.active_rows_model(), idx);
            st.set_selection_anchor(idx);
            if let Some(w) = weak.upgrade() {
                push_active_footer(&w, &st, count);
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_row_ctrl_clicked(move |idx: i32| {
            let count = selection_toggle(&st.active_rows_model(), idx);
            st.set_selection_anchor(idx);
            if let Some(w) = weak.upgrade() {
                push_active_footer(&w, &st, count);
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_row_shift_clicked(move |idx: i32| {
            let anchor = st.selection_anchor();
            let from = if anchor < 0 { idx } else { anchor };
            let count = selection_set_range(&st.active_rows_model(), from, idx);
            if anchor < 0 {
                st.set_selection_anchor(idx);
            }
            if let Some(w) = weak.upgrade() {
                push_active_footer(&w, &st, count);
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_select_all(move || {
            let count = selection_set_all(&st.active_rows_model(), true);
            st.set_selection_anchor(if count > 0 { 0 } else { -1 });
            if let Some(w) = weak.upgrade() {
                push_active_footer(&w, &st, count);
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_deselect_all(move || {
            let count = selection_set_all(&st.active_rows_model(), false);
            st.set_selection_anchor(-1);
            if let Some(w) = weak.upgrade() {
                push_active_footer(&w, &st, count);
            }
        });
    }

    // Rubber-band: we receive (x1, y1, x2, y2) in **content coordinates**
    // (the Slint side has already subtracted the exact Flickable `viewport-y`).
    // We only use y1/y2 (band selection). The lookup relies on the rows'
    // variable geometry, so it stays exact in a list mixing large
    // thumbnails and small icons, regardless of scroll position.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_rubber_band_update(move |_x1, y1, _x2, y2| {
            let model = st.active_rows_model();
            let (lo, hi) = row_band_for_content_range(&model, y1, y2);
            // Combine the band with the base captured at drag start, depending on
            // the mode (replace / add Shift / subtract Ctrl). Empty band (hi < lo):
            // lo > hi → no row is "in_band" → base kept (add/sub) or everything
            // deselected (replace).
            let (mode, count) = RB_STATE.with(|s| {
                let s = s.borrow();
                (s.0, selection_apply_band(&model, lo, hi, s.0, &s.1))
            });
            // The anchor follows the top edge of the band (for a future Shift+click) — only
            // in replace mode, where the band defines the whole selection.
            if mode == 0 && hi >= lo {
                st.set_selection_anchor(lo);
            }
            if let Some(w) = weak.upgrade() {
                push_active_footer(&w, &st, count);
            }
        });
    }
    // Start of a rubber-band: captures the base selection + the mode
    // (0 replace, 1 add [Shift], 2 subtract [Ctrl]).
    {
        let st = state.clone();
        window.on_rubber_band_begin(move |mode: i32| {
            let base = snapshot_selection(&st.active_rows_model());
            RB_STATE.with(|s| *s.borrow_mut() = (mode, base));
        });
    }

    // Configurable shortcuts -----
    // Resolves a key combination → action id according to the effective map. Called
    // on every keystroke by the Slint `key-scope`; returns "" if no action matches.
    {
        let st = state.clone();
        window.on_match_action(
            move |keyname: SharedString, ctrl: bool, alt: bool, shift: bool| -> SharedString {
                match Chord::from_event(&keyname, ctrl, alt, shift) {
                    Some(chord) => st
                        .keymap
                        .borrow()
                        .action_for(&chord)
                        .map(SharedString::from)
                        .unwrap_or_default(),
                    None => SharedString::new(),
                }
            },
        );
    }
    // Keyboard cursor navigation (arrows / Shift+arrows / Home / End).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_move_cursor(move |kind: i32, extend: bool| {
            let Some(w) = weak.upgrade() else { return };
            let model = st.active_rows_model();
            let n = model.row_count() as i32;
            if n == 0 {
                return;
            }
            let (cur, anchor) = st.with_tabs(|b| {
                let a = b.active;
                (b.tabs[a].cursor, b.tabs[a].selection_anchor)
            });
            let base = if cur < 0 { 0 } else { cur.min(n - 1) };
            let new_cursor = match kind {
                0 => (base - 1).max(0),     // up
                1 => (base + 1).min(n - 1), // down
                2 => 0,                     // start
                3 => n - 1,                 // end
                _ => base,
            };
            if extend {
                // Extend from the anchor (set if absent) to the new cursor.
                let anc = if anchor < 0 { base } else { anchor };
                let _ = selection_set_range(&model, anc, new_cursor);
                st.with_tabs_mut(|b| {
                    let a = b.active;
                    b.tabs[a].selection_anchor = anc;
                });
            } else {
                let _ = selection_set_only(&model, new_cursor);
                st.with_tabs_mut(|b| {
                    let a = b.active;
                    b.tabs[a].selection_anchor = new_cursor;
                });
            }
            st.with_tabs_mut(|b| {
                let a = b.active;
                b.tabs[a].cursor = new_cursor;
                b.tabs[a].scroll_gen += 1; // triggers the scroll-into-view on the Slint side
            });
            update_panels_ui(&w, &st);
        });
    }

    // : file operations -----

    // Right-click: selects the row if not already selected, then opens the menu.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_row_right_clicked(move |idx: i32, x: f32, y: f32| {
            let Some(w) = weak.upgrade() else { return };
            let rows = st.active_rows_model();
            let already_selected = rows
                .row_data(idx as usize)
                .map(|r| r.selected)
                .unwrap_or(false);
            if !already_selected {
                let count = selection_set_only(&rows, idx);
                st.set_selection_anchor(idx);
                push_active_footer(&w, &st, count);
            }
            // "Use as application": visible if the selection = 1 executable.
            let sel = selected_paths(&st);
            // The PRIMARY item treated as a folder: a real directory, a folder
            // symlink/junction (already reflected by `is_dir`), or a Windows
            // `.lnk` pointing at a folder (resolved on demand — one COM call,
            // and only for an actual `.lnk`). Shared by every file/folder choice
            // below, so a folder shortcut behaves like the folder it targets:
            // no "Open with", "new tab" instead of "open as admin", and
            // folder-context pinned commands.
            let primary_dir = sel.first().map(|p| acts_as_dir(p)).unwrap_or(false);
            let is_file = sel.len() == 1 && !primary_dir;
            w.set_selection_is_executable(sel.len() == 1 && is_executable_path(&sel[0]));
            // "Open as administrator" (Windows): target = a SINGLE real file.
            w.set_ctx_selection_is_file(is_file);
            // A single item (file OR folder) → "Create a shortcut" is offered.
            w.set_ctx_selection_single(sel.len() == 1);
            // A folder (or folder shortcut) as the primary item → "Open with" is
            // hidden (it hands a file to an application; a folder is opened by
            // navigating into it).
            w.set_ctx_selection_is_dir(primary_dir);
            // Pinned user commands, filtered by target: file (bit 1) or folder
            // (bit 2) — same folder rule as the built-in entries above.
            let bit = if primary_dir {
                openers::CTX_DIR
            } else {
                openers::CTX_FILE
            };
            let n_custom = push_ctx_custom_entries(&w, &st, bit, &sel);
            w.set_ctx_custom_bg(false);
            // Windows SHELL context menu for the selection.
            let n_shell = refresh_shell_menu(&w, &st, &sel);
            // Full menu height = same computation as the Slint binding.
            let h = 402.0
                + if is_file { 26.0 } else { 0.0 }
                + if n_custom > 0 {
                    n_custom as f32 * 26.0 + 1.0
                } else {
                    0.0
                }
                + if n_shell > 0 {
                    n_shell as f32 * 26.0 + 1.0
                } else {
                    0.0
                };
            let (x, y) = clamp_ctx_menu_pos(&w, x, y, h);
            // Refreshes the "Open with" flyout filtered by the TARGETED file's
            // extension → programs suited to this specific file.
            push_openers_ui(&w, &st);
            w.set_ctx_on_empty(false);
            w.set_ctx_menu_x(x);
            w.set_ctx_menu_y(y);
            w.set_ctx_menu_open(true);
        });
    }
    {
        let weak = window.as_weak();
        window.on_ctx_close(move || {
            if let Some(w) = weak.upgrade() {
                w.set_ctx_menu_open(false);
            }
        });
    }
    // Pinned user command in the context menu: on the view BACKGROUND
    // it targets the current folder ({dir} = current), otherwise the selection.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ctx_custom_run(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let Some(op) = st.openers.borrow().get(&id).cloned() else {
                return;
            };
            let result = if w.get_ctx_custom_bg() {
                actions::run_opener_dir(&op, &st.current_path())
            } else {
                actions::run_opener(&op, &selected_paths(&st))
            };
            if let Err(err) = result {
                error!(error = %err, label = op.label, "pinned context command failed");
                let lang = st.config.borrow().language;
                notice(
                    &w,
                    i18n::tr(lang, "ow_run_failed").replace("{name}", &op.label),
                    NoticeKind::Error,
                );
            }
        });
    }
    // Windows SHELL context menu entry: InvokeCommand on the live
    // session. The session is TAKEN OUT of the state during the call (some
    // handlers pump messages → re-entrancy must not find the RefCell
    // already borrowed), then put back.
    {
        let st = state.clone();
        window.on_ctx_shell_run(move |offset: i32| {
            let session = st.shell_menu.borrow_mut().take();
            if let Some(s) = session {
                let offset = offset.max(0) as u32;
                // The modern "Share" verb fails when invoked via the shell from an
                // unpackaged host (ERROR_INVALID_WINDOW_HANDLE): the share flyout
                // needs a DataTransferManager registered for the window first. We
                // drive it natively instead. "share" is the only standard verb
                // containing that token, so the match is language-independent.
                if crate::shellmenu::verb_for(&s, offset).contains("share") {
                    #[cfg(windows)]
                    {
                        let paths = selected_paths(&st);
                        if let Some(hwnd) = crate::winmsg::self_hwnd()
                            && !paths.is_empty()
                            && let Err(err) = crate::winshare::share_files(hwnd, &paths)
                        {
                            error!(error = %err, "native share failed");
                        }
                    }
                } else if let Err(err) = crate::shellmenu::invoke(&s, offset) {
                    error!(error = %err, "shell context command failed");
                }
                *st.shell_menu.borrow_mut() = Some(s);
            }
        });
    }
    // Hovering a shell CASCADE: pushes the children into the flyout.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ctx_shell_sub_hover(move |sub: i32| {
            let Some(w) = weak.upgrade() else { return };
            let subs = st.shell_subs.borrow();
            if let Some(children) = subs.get(sub.max(0) as usize) {
                w.set_ctx_shell_sub_entries(ModelRc::new(VecModel::from(children.clone())));
            }
        });
    }
    // Opening the "Detected Windows entries" sub-tab: lazy probe.
    // Deferred by one event-loop tick → the tab appears BEFORE the (potentially
    // slow) COM scan, the list fills in right after.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_shell_ext_scan(move || {
            let st = st.clone();
            let weak = weak.clone();
            defer(move || {
                if let Some(w) = weak.upgrade() {
                    scan_shell_ext(&w, &st);
                }
            });
        });
    }
    // Checkbox of the "detected Windows entries" panel: hides/shows
    // the entry (by label), persisted.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_shell_ext_toggle(move |label: SharedString, on: bool| {
            let Some(w) = weak.upgrade() else { return };
            let label = label.to_string();
            info!(label, on, "shell context entry toggled");
            st.persist_config(|c| {
                if on {
                    c.shell_menu_disabled.retain(|x| x != &label);
                } else if !c.shell_menu_disabled.contains(&label) {
                    c.shell_menu_disabled.push(label.clone());
                }
            });
            refresh_shell_ext_rows(&w, &st);
        });
    }

    // Open: for a folder, navigate; for a file, xdg-open.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_open(move || {
            let Some(w) = weak.upgrade() else { return };
            let paths = selected_paths(&st);
            let Some(first) = paths.first() else { return };
            let is_dir = first.is_dir();
            if is_dir {
                load_directory(&w, &st, first, true);
            } else if !open_shortcut_as_tab(&w, &st, first) {
                // RANGER's per-extension default if set, otherwise the OS default.
                // Logic shared with double-click (`open_file_default`).
                // A folder .lnk shortcut has already been opened as a tab.
                open_file_default(&st, &paths);
            }
        });
    }
    // Open in a new tab (of the ACTIVE panel). The selected folder
    // opens in a new tab; for a file, it's its parent folder.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_open_new_tab(move || {
            let Some(w) = weak.upgrade() else { return };
            let paths = selected_paths(&st);
            let target = match paths.first() {
                Some(p) if p.is_dir() => p.clone(),
                Some(p) => p
                    .parent()
                    .map(|pp| pp.to_path_buf())
                    .unwrap_or_else(|| st.current_path()),
                None => st.current_path(),
            };
            let opened = st.with_tabs_mut(|book| {
                let a = book.open_after_active(target);
                book.tabs[a].current_path.clone()
            });
            load_directory(&w, &st, &opened, false);
        });
    }
    // "Create shortcut": opens the popup (kind 3) pre-filled to create,
    // in the current folder, a link (.lnk or symlink) to the selected file.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_create_link(move || {
            let Some(w) = weak.upgrade() else { return };
            let Some(file) = selected_paths(&st).into_iter().next() else {
                return;
            };
            // Default name = the file's stem (avoids colliding with the
            // file itself for a symlink in the same folder).
            let default_name = file
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            w.set_create_kind(3);
            w.set_create_name(default_name.into());
            w.set_create_target(file.display().to_string().into());
            // Linux: no `.lnk` → Symlink tab forced.
            w.set_create_link_symlink(!cfg!(windows));
            w.set_create_popup_open(true); // arms focus via `changed create-popup-open`
        });
    }
    // "Open as administrator": launches the targeted file ELEVATED via
    // ShellExecuteW("runas") → UAC prompt. Windows only (the menu entry is
    // hidden elsewhere via `platform-windows`).
    {
        let st = state.clone();
        window.on_action_open_admin(move || {
            let paths = selected_paths(&st);
            let Some(first) = paths.first() else { return };
            if let Err(err) = actions::open_elevated(first) {
                error!(error = %err, path = %first.display(), "open as admin failed");
            }
        });
    }
    // "Open with…": the OS's native picker (Windows only; the menu
    // entry is hidden elsewhere via `platform-windows`).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_open_with(move || {
            let paths = selected_paths(&st);
            let Some(first) = paths.first() else { return };
            if let Err(err) = actions::open_with(first) {
                error!(error = %err, path = %first.display(), "open with failed");
            } else if let Some(w) = weak.upgrade() {
                // The native dialog doesn't reveal the chosen app → we offer to
                // register it afterwards via a non-blocking toast.
                w.set_ow_promote_visible(true);
            }
        });
    }
    // Open with: openers -----
    // Settings filter: doesn't rebuild the items and therefore doesn't re-extract
    // any icons while typing.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_opener_search(move |text: SharedString| {
            *st.opener_filter.borrow_mut() = text.to_string();
            if let Some(w) = weak.upgrade() {
                push_filtered_openers_ui(&w, &st);
            }
        });
    }
    // Launches an opener on the current selection.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_run_opener(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let opener = st.openers.borrow().get(&id).cloned();
            if let Some(op) = opener {
                let paths = selected_paths(&st);
                // Extension of the opened file → learned for the opener.
                let ext = paths.first().and_then(|p| {
                    p.extension()
                        .map(|e| e.to_string_lossy().to_ascii_lowercase())
                });
                match actions::run_opener(&op, &paths) {
                    Ok(()) => {
                        st.openers.borrow_mut().record_use(&op.id, ext.as_deref());
                        save_openers(&st);
                        push_openers_ui(&w, &st);
                    }
                    Err(err) => {
                        error!(error = %err, label = op.label, "run_opener failed");
                        let lang = st.config.borrow().language;
                        notice(
                            &w,
                            i18n::tr(lang, "ow_run_failed").replace("{name}", &op.label),
                            NoticeKind::Error,
                        );
                    }
                }
            }
        });
    }
    // Blank "Custom command" popup (menu / Settings).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_add(move || {
            if let Some(w) = weak.upgrade() {
                open_ow_create(&w, &st, "", "");
            }
        });
    }
    // Ready-made archiving command: opens the editor prefilled rather than
    // saving straight away, so the template stays reviewable and editable.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_recipe_add(move |index: i32| {
            let Some(w) = weak.upgrade() else { return };
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let Some(recipe) = RECIPES.get(index) else {
                return;
            };
            let Some(program) = actions::resolve_program(recipe.programs) else {
                return; // the tool disappeared since the list was built
            };
            let lang = st.snapshot_config().language;
            open_ow_create(&w, &st, &program, &recipe.label(lang));
            w.set_ow_popup_icon_kind(recipe.icon.as_i32());
            w.set_ow_popup_args(recipe.args.into());
            w.set_ow_popup_ctx_file(recipe.ctx & openers::CTX_FILE != 0);
            w.set_ow_popup_ctx_ext(recipe.ctx_exts.join(", ").into());
            w.set_ow_popup_ctx_dir(recipe.ctx & openers::CTX_DIR != 0);
            w.set_ow_popup_ctx_bg(recipe.ctx & openers::CTX_BACKGROUND != 0);
            recompute_ow_preview(&w, &st);
        });
    }
    // "…" button of the Program field: native exe picker.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_browse(move || {
            let Some(w) = weak.upgrade() else { return };
            let lang = st.snapshot_config().language;
            if let Some(path) = openwith::browse_for_exe(lang) {
                w.set_ow_popup_program(path.into());
                recompute_ow_preview(&w, &st);
            }
        });
    }
    // "Choose an application…": enumerates the OS's apps → custom picker.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_open_picker(move || {
            let Some(w) = weak.upgrade() else { return };
            let Some(path) = selected_paths(&st).into_iter().next() else {
                return;
            };
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            let mut handlers = openwith::handlers_for_ext(&ext);
            // Recommended first, then alphabetical order (already sorted on the Linux side).
            handlers.sort_by(|a, b| {
                b.recommended
                    .cmp(&a.recommended)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            let items: Vec<OpenerItem> = handlers
                .iter()
                .map(|h| OpenerItem {
                    id: h.key.clone().into(),
                    label: h.name.clone().into(),
                    available: true,
                    icon: opener_icon(h.exe.as_deref().unwrap_or_default()),
                    icon_kind: openers::OpenerIcon::None.as_i32(),
                })
                .collect();
            *st.ow_pick_ctx.borrow_mut() = Some(OwPickCtx {
                ext,
                path,
                handlers,
            });
            w.set_ow_picker_handlers(ModelRc::new(VecModel::from(items)));
            w.set_ow_picker_set_default(false); // unchecked on every opening
            w.set_ow_picker_open(true);
        });
    }
    // Choice in the picker: launches + CAPTURES the app (persisted as an opener).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_pick(move |key: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let key = key.to_string();
            // Extracted from the context (path/ext + chosen handler), then releases the borrow.
            let picked = {
                let ctx = st.ow_pick_ctx.borrow();
                let Some(ctx) = ctx.as_ref() else { return };
                let handler = ctx.handlers.iter().find(|h| h.key == key).cloned();
                (ctx.ext.clone(), ctx.path.clone(), handler)
            };
            let (ext, path, handler) = picked;
            match openwith::launch(&key, &ext, &path) {
                Ok(()) => {
                    // Capture: registers ANY chosen app (regular exe OR an OS
                    // app without an exe — Windows UWP/Store, Linux `.desktop`) and marks it
                    // as the MOST RECENTLY used → head of the suggestions.
                    if let Some(h) = handler {
                        // "Set as default" checked → the app becomes Ranger's
                        // default for this extension (takes priority over the OS).
                        let set_default = w.get_ow_picker_set_default();
                        {
                            let mut store = st.openers.borrow_mut();
                            let id = match h.exe {
                                Some(exe) => store
                                    .openers
                                    .iter()
                                    .find(|o| o.program == exe)
                                    .map(|o| o.id.clone())
                                    .unwrap_or_else(|| {
                                        store.add(&h.name, &exe, vec!["{file}".to_string()])
                                    }),
                                None => store
                                    .openers
                                    .iter()
                                    .find(|o| o.assoc.as_deref() == Some(h.key.as_str()))
                                    .map(|o| o.id.clone())
                                    .unwrap_or_else(|| store.add_assoc(&h.name, &h.key)),
                            };
                            store.record_use(&id, Some(&ext));
                            if set_default && !ext.is_empty() {
                                store.set_default_ext(&id, &ext, true);
                            }
                        }
                        save_openers(&st);
                        push_openers_ui(&w, &st);
                    }
                }
                Err(err) => {
                    error!(error = %err, "open-with launch failed");
                    let lang = st.config.borrow().language;
                    notice(
                        &w,
                        i18n::strings_for(lang).fav_toast_missing.clone(),
                        NoticeKind::FavMissing,
                    );
                }
            }
        });
    }
    // Picker's "Browse…": choose a custom exe from the OS (like the
    // Custom Command's "…"), REGISTER it as a reusable opener, then
    // launch it on the current selection. Symmetric across Windows (IFileOpenDialog) /
    // Linux (zenity/kdialog).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_pick_browse(move || {
            let Some(w) = weak.upgrade() else { return };
            // Picker context required (opened on a selection); we keep the
            // first path as a fallback if the selection emptied in the meantime.
            let ctx_path = {
                let ctx = st.ow_pick_ctx.borrow();
                let Some(ctx) = ctx.as_ref() else { return };
                ctx.path.clone()
            };
            // Cancelling the native dialog → we leave everything untouched (picker stays open).
            let lang = st.snapshot_config().language;
            let Some(exe) = openwith::browse_for_exe(lang) else {
                return;
            };
            let name = std::path::Path::new(&exe)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| exe.clone());
            // Registers the app (reuses the existing one if same program).
            let opener = {
                let mut store = st.openers.borrow_mut();
                match store.openers.iter().find(|o| o.program == exe).cloned() {
                    Some(o) => o,
                    None => {
                        let id = store.add(&name, &exe, vec!["{file}".to_string()]);
                        store.get(&id).cloned().expect("opener was just added")
                    }
                }
            };
            save_openers(&st);
            push_openers_ui(&w, &st);
            // Launches on the current selection (multi-file handled by run_opener).
            let mut paths = selected_paths(&st);
            if paths.is_empty() {
                paths.push(ctx_path);
            }
            let ext = paths.first().and_then(|p| {
                p.extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
            });
            match actions::run_opener(&opener, &paths) {
                Ok(()) => {
                    let set_default = w.get_ow_picker_set_default();
                    {
                        let mut store = st.openers.borrow_mut();
                        store.record_use(&opener.id, ext.as_deref());
                        if set_default && let Some(e) = ext.as_deref().filter(|e| !e.is_empty()) {
                            store.set_default_ext(&opener.id, e, true);
                        }
                    }
                    save_openers(&st);
                    push_openers_ui(&w, &st);
                }
                Err(err) => {
                    error!(error = %err, label = opener.label, "ow-pick-browse run_opener failed");
                    let lang = st.config.borrow().language;
                    notice(
                        &w,
                        i18n::tr(lang, "ow_run_failed").replace("{name}", &opener.label),
                        NoticeKind::Error,
                    );
                }
            }
            w.set_ow_picker_open(false);
        });
    }
    // "Use as application": pre-fills from the selected exe.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_use_as_app(move || {
            let Some(w) = weak.upgrade() else { return };
            let sel = selected_paths(&st);
            if let Some(p) = sel.first() {
                let name = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                open_ow_create(&w, &st, &p.display().to_string(), &name);
            }
        });
    }
    // Promotion toast accepted → blank popup (the user points to the exe).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_promote_accept(move || {
            if let Some(w) = weak.upgrade() {
                open_ow_create(&w, &st, "", "");
            }
        });
    }
    // Editing an existing opener.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_edit(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if let Some(op) = st.openers.borrow().get(&id) {
                w.set_ow_popup_id(op.id.clone().into());
                w.set_ow_popup_name(op.label.clone().into());
                w.set_ow_popup_program(op.program.clone().into());
                w.set_ow_popup_icon_kind(effective_opener_icon(op).as_i32());
                // OS/Store app (assoc, without exe) → Program field frozen.
                w.set_ow_popup_is_store(op.assoc.is_some());
                w.set_ow_popup_args(op.args.join(" ").into());
                w.set_ow_popup_default_ext(op.default_exts.join(", ").into());
                // Learned/manual extensions → offered in "Open with".
                w.set_ow_popup_used_ext(op.used_exts.join(", ").into());
                w.set_ow_popup_elevated(op.elevated); // "run as admin"
                // Pinning to the context menu.
                w.set_ow_popup_ctx_file(op.ctx_menu & openers::CTX_FILE != 0);
                // Empty in an opener saved before this field existed → show the
                // wildcard rather than a blank that would read as "none".
                w.set_ow_popup_ctx_ext(if op.ctx_exts.is_empty() {
                    openers::CTX_EXT_ALL.to_string().into()
                } else {
                    op.ctx_exts.join(", ").into()
                });
                w.set_ow_popup_ctx_dir(op.ctx_menu & openers::CTX_DIR != 0);
                w.set_ow_popup_ctx_bg(op.ctx_menu & openers::CTX_BACKGROUND != 0);
                w.set_ow_popup_add(true);
                w.set_ow_popup_open(true);
                w.set_ow_popup_focus_armed(true);
            }
            recompute_ow_preview(&w, &st);
        });
    }
    // Deleting an opener.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_delete(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            st.openers.borrow_mut().remove(&id);
            save_openers(&st);
            push_openers_ui(&w, &st);
        });
    }
    // Duplicating an entry: clone inserted right after the original.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_duplicate(move |id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if st.openers.borrow_mut().duplicate(&id).is_some() {
                save_openers(&st);
            }
            push_openers_ui(&w, &st);
        });
    }
    // Reordering (up/down) in Settings.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_move(move |id: SharedString, dir: i32| {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut s = st.openers.borrow_mut();
                let id = id.to_string();
                if let Some(pos) = s.openers.iter().position(|o| o.id == id) {
                    let n = s.openers.len();
                    let target = if dir < 0 {
                        pos.checked_sub(1)
                    } else if pos + 1 < n {
                        Some(pos + 1)
                    } else {
                        None
                    };
                    if let Some(t) = target {
                        s.openers.swap(pos, t);
                    }
                }
            }
            save_openers(&st);
            push_openers_ui(&w, &st);
        });
    }
    // Live preview (on every keystroke in the popup).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_popup_changed(move || {
            if let Some(w) = weak.upgrade() {
                recompute_ow_preview(&w, &st);
            }
        });
    }
    // Popup submission: saves (checkbox checked) OR launches one-shot.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ow_popup_commit(move || {
            let Some(w) = weak.upgrade() else { return };
            let id = w.get_ow_popup_id().to_string();
            let label = w.get_ow_popup_name().to_string();
            let program = w.get_ow_popup_program().to_string();
            let args = split_args(&w.get_ow_popup_args());
            let exts = parse_exts(&w.get_ow_popup_default_ext());
            // "Compatible" extensions (used_exts) edited by hand → the opener
            // is listed in the "Open with" flyout for these.
            let used = parse_exts(&w.get_ow_popup_used_ext());
            // Run as administrator (Windows) — checkbox checked in the popup.
            let elevated = w.get_ow_popup_elevated();
            let icon = openers::OpenerIcon::from_i32(w.get_ow_popup_icon_kind());
            // Pinning to the context menu: 3 checkboxes → bitmask.
            // A lone "*" is stored as-is: `matches_ctx_ext` reads it as the
            // wildcard, and an empty field means the same thing.
            let ctx_exts = parse_exts(&w.get_ow_popup_ctx_ext());
            let ctx_mask = (if w.get_ow_popup_ctx_file() {
                openers::CTX_FILE
            } else {
                0
            }) | (if w.get_ow_popup_ctx_dir() {
                openers::CTX_DIR
            } else {
                0
            }) | (if w.get_ow_popup_ctx_bg() {
                openers::CTX_BACKGROUND
            } else {
                0
            });
            if !id.is_empty() {
                st.openers.borrow_mut().update(&id, &label, &program, args);
                st.openers.borrow_mut().set_icon(&id, icon);
                apply_default_exts(&st, &id, &exts);
                st.openers.borrow_mut().set_used_exts(&id, &used);
                st.openers.borrow_mut().set_elevated(&id, elevated);
                st.openers.borrow_mut().set_ctx_menu(&id, ctx_mask);
                st.openers.borrow_mut().set_ctx_exts(&id, &ctx_exts);
                save_openers(&st);
            } else if w.get_ow_popup_add() {
                let new_id = st.openers.borrow_mut().add(&label, &program, args);
                st.openers.borrow_mut().set_icon(&new_id, icon);
                apply_default_exts(&st, &new_id, &exts);
                st.openers.borrow_mut().set_used_exts(&new_id, &used);
                st.openers.borrow_mut().set_elevated(&new_id, elevated);
                st.openers.borrow_mut().set_ctx_menu(&new_id, ctx_mask);
                st.openers.borrow_mut().set_ctx_exts(&new_id, &ctx_exts);
                save_openers(&st);
            } else {
                // One-shot: temporary opener, launched on the selection, not saved.
                let temp = openers::Opener {
                    id: String::new(),
                    label,
                    program,
                    assoc: None,
                    icon: openers::OpenerIcon::from_i32(w.get_ow_popup_icon_kind()),
                    args,
                    default_exts: Vec::new(),
                    used_exts: Vec::new(),
                    use_count: 0,
                    last_used: 0,
                    elevated,
                    ctx_menu: 0,
                    ctx_exts: Vec::new(),
                };
                if let Err(err) = actions::run_opener(&temp, &selected_paths(&st)) {
                    error!(error = %err, "one-shot opener failed");
                }
            }
            push_openers_ui(&w, &st);
        });
    }

    // ext: Open parent folder / Open terminal here -----
    {
        let st = state.clone();
        window.on_action_open_parent(move || {
            let paths = selected_paths(&st);
            // If there's a selection: open the parent of the first item.
            // Otherwise: open the parent of the current folder.
            let target = paths.first().cloned().unwrap_or_else(|| st.current_path());
            if let Err(err) = actions::open_parent(&target) {
                error!(error = %err, "open_parent failed");
            }
        });
    }
    {
        let st = state.clone();
        window.on_action_open_terminal(move || {
            let paths = selected_paths(&st);
            // Selection present → take the first path; otherwise → cwd.
            // `open_terminal` handles the path-or-parent mapping internally.
            let target = paths.first().cloned().unwrap_or_else(|| st.current_path());
            if let Err(err) = actions::open_terminal(&target) {
                error!(error = %err, "open_terminal failed");
            }
        });
    }

    // ext: Copy path / Copy name -----
    {
        let st = state.clone();
        window.on_action_copy_path(move || {
            let paths = selected_paths(&st);
            // If several are selected, join them with a newline.
            let text = if paths.is_empty() {
                st.current_path().display().to_string()
            } else {
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            if let Err(err) = actions::copy_to_clipboard(&text) {
                error!(error = %err, "copy_to_clipboard (path) failed");
            } else {
                info!(len = text.len(), "copied path(s)");
            }
        });
    }
    {
        let st = state.clone();
        window.on_action_copy_name(move || {
            let paths = selected_paths(&st);
            let text = if paths.is_empty() {
                // No selection → name of the current folder.
                st.current_path()
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            } else {
                paths
                    .iter()
                    .filter_map(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| s.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            if let Err(err) = actions::copy_to_clipboard(&text) {
                error!(error = %err, "copy_to_clipboard (name) failed");
            } else {
                info!(len = text.len(), "copied name(s)");
            }
        });
    }

    // Copy: snapshot of the selected paths into the internal clipboard.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_copy(move || {
            if weak.upgrade().is_none() {
                return;
            }
            let paths = selected_paths(&st);
            if paths.is_empty() {
                return;
            }
            // OS clipboard (Explorer / Wayland compositor) — best-effort.
            if let Err(err) = clipboard::write_files(&paths, false) {
                debug!(error = %err, "clipboard write (copy) failed");
            }
            {
                let mut clip = st.clipboard.borrow_mut();
                clip.paths = paths;
                clip.op = Some(ClipOp::Copy);
            }
            clear_cut_marks(&st.active_rows_model());
            info!(count = ?st.clipboard.borrow_mut().paths.len(), "copy");
        });
    }

    // Cut: same as Copy but with ClipOp::Cut + visual marking.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_cut(move || {
            if weak.upgrade().is_none() {
                return;
            }
            let paths = selected_paths(&st);
            if paths.is_empty() {
                return;
            }
            // OS clipboard (Explorer / Wayland) — "cut" = move.
            if let Err(err) = clipboard::write_files(&paths, true) {
                debug!(error = %err, "clipboard write (cut) failed");
            }
            {
                let mut clip = st.clipboard.borrow_mut();
                clip.paths = paths.clone();
                clip.op = Some(ClipOp::Cut);
            }
            // Visually marks the matching rows (by name).
            let cur_dir = st.current_path();
            let cut_names: std::collections::HashSet<String> = paths
                .iter()
                .filter_map(|p| {
                    if p.parent() == Some(cur_dir.as_path()) {
                        p.file_name()
                            .and_then(|n| n.to_str().map(|s| s.to_string()))
                    } else {
                        None
                    }
                })
                .collect();
            apply_cut_marks(&st.active_rows_model(), &cut_names);
            info!(count = paths.len(), "cut");
        });
    }

    // Paste: prepares a PasteJob. Items without a name conflict are
    // resolved directly; those whose name is already taken open the
    // conflict popup (one at a time). The actual execution happens once everything is resolved.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_paste(move || {
            let Some(w) = weak.upgrade() else { return };
            let cur = st.current_path();
            if cur.as_os_str().is_empty() {
                return;
            }
            // Priority to the OS clipboard (files copied/cut in
            // Explorer, Dolphin, or another app). Falls back to the
            // internal state if the clipboard doesn't contain files.
            let (paths, op) = match clipboard::read_files() {
                Some((paths, cut)) => (paths, Some(if cut { ClipOp::Cut } else { ClipOp::Copy })),
                None => {
                    let clip = st.clipboard.borrow();
                    (clip.paths.clone(), clip.op)
                }
            };
            let Some(op) = op else { return };
            if paths.is_empty() {
                return;
            }
            // A Cut pasted back into the folder its items already live in can
            // move nothing (each source would be skipped by `begin_paste`):
            // treat it as CANCELLING the cut rather than silently doing nothing.
            // Un-grey the rows now and drop the clipboard so the cancellation is
            // clean and no later paste keeps a stale move pending.
            if matches!(op, ClipOp::Cut) && paths.iter().all(|p| p.parent() == Some(cur.as_path()))
            {
                clear_cut_marks(&st.active_rows_model());
                let mut clip = st.clipboard.borrow_mut();
                clip.paths.clear();
                clip.op = None;
                return;
            }
            begin_paste(&w, &st, op, cur, paths);
        });
    }
    // Live validation of the name typed in the conflict popup.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_paste_conflict_check(move |name: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            w.set_paste_conflict_name_taken(paste_name_invalid(&st, &name));
        });
    }
    // Confirms the name for the current conflict, then moves to the next one.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_paste_conflict_confirm(move |name: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let name = name.to_string();
            if paste_name_invalid(&st, &name) {
                // Safeguard: name taken/invalid → we don't close, we flag it again.
                w.set_paste_conflict_name_taken(true);
                return;
            }
            {
                let mut guard = st.paste_job.borrow_mut();
                if let Some(job) = guard.as_mut()
                    && let Some(src) = job.current.take()
                {
                    // Rename → free target (no overwrite).
                    job.resolved.push((src, job.dst_dir.join(&name), false));
                }
            }
            advance_paste(&w, &st);
        });
    }
    // Skips the current conflicting item.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_paste_conflict_skip(move || {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut guard = st.paste_job.borrow_mut();
                if let Some(job) = guard.as_mut() {
                    job.current = None;
                }
            }
            advance_paste(&w, &st);
        });
    }
    // Replaces (overwrites) the CURRENT conflicting item, then moves to the next.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_paste_conflict_replace(move || {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut guard = st.paste_job.borrow_mut();
                if let Some(job) = guard.as_mut()
                    && let Some(src) = job.current.take()
                {
                    resolve_replace(job, src);
                }
            }
            advance_paste(&w, &st);
        });
    }
    // Replaces (overwrites) the current item AND ALL remaining conflicts, then
    // executes.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_paste_conflict_replace_all(move || {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut guard = st.paste_job.borrow_mut();
                if let Some(job) = guard.as_mut() {
                    if let Some(src) = job.current.take() {
                        resolve_replace(job, src);
                    }
                    while let Some(src) = job.pending.pop_front() {
                        resolve_replace(job, src);
                    }
                }
            }
            advance_paste(&w, &st);
        });
    }
    // Skips the current item AND ALL remaining conflicts, then executes.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_paste_conflict_skip_all(move || {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut guard = st.paste_job.borrow_mut();
                if let Some(job) = guard.as_mut() {
                    job.current = None;
                    job.pending.clear();
                }
            }
            advance_paste(&w, &st);
        });
    }
    // Cancels the whole paste operation (nothing has been executed yet).
    {
        let st = state.clone();
        window.on_paste_conflict_cancel(move || {
            *st.paste_job.borrow_mut() = None;
        });
    }
    // Cancels the ongoing long operation (raises the cooperative flag).
    {
        let st = state.clone();
        window.on_op_cancel(move |op_id: i32| {
            st.ops.cancel(op_id);
        });
    }
    // Creates or refreshes one operation's toast row (pushed from its worker).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_op_progress(move |row: OpProgress| {
            let Some(w) = weak.upgrade() else { return };
            upsert_op_row(&st, row);
            refresh_ops_ui(&w, &st);
        });
    }
    // Closes one operation's toast, by hand or once its auto-dismiss fires.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_op_dismiss(move |op_id: i32| {
            let Some(w) = weak.upgrade() else { return };
            remove_op_row(&st, op_id);
            refresh_ops_ui(&w, &st);
        });
    }
    // End of a long operation: resets the state (from the thread, via invoke).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_op_finished(move |op_id: i32| {
            let Some(handle) = st.ops.finish(op_id) else {
                return; // unknown id: already released
            };
            let Some(w) = weak.upgrade() else { return };
            sync_op_busy(&w, &st);
            // The item to highlight is handed to the upcoming re-listing: the row
            // only exists once the views have been re-read. Operations complete
            // independently, so the last one to finish is the one highlighted.
            if let Some(target) = handle.pending_focus {
                *st.focus_after_refresh.borrow_mut() = Some(target);
            }
            invalidate_thumbnail_paths(&st, &handle.thumbnail_invalidations);
            // The worker no longer needs its sources. Remove any virtual-file
            // staging tree before views are listed again.
            drop(handle.transient_cleanup);
            request_op_refresh(&w);
        });
    }

    // Duplicate: copies each selected row to a unique sibling
    // (background thread + progress bar).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_duplicate(move || {
            let Some(w) = weak.upgrade() else { return };
            let paths = selected_paths(&st);
            if paths.is_empty() {
                return;
            }
            let pairs: Vec<(PathBuf, PathBuf, bool)> = paths
                .iter()
                .cloned()
                .zip(plan_unique_targets(&paths, |p| target_taken(&st, p)))
                .map(|(src, dst)| (src, dst, false))
                .collect();
            let lang = st.snapshot_config().language;
            start_heavy_op(&w, &st, Heavy::Copy(pairs), lang, None, None);
        });
    }

    // Rename opens a modal popup pre-filled with the name of the first selected
    // row. The exact source path is captured in AppState: the row
    // index can become stale if the watcher re-lists while the dialog is open.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_rename(move || {
            let Some(w) = weak.upgrade() else { return };
            let rows = st.active_rows_model();
            let target = (0..rows.row_count())
                .find(|&i| rows.row_data(i).map(|r| r.selected).unwrap_or(false));
            let Some(idx) = target else { return };
            let Some(row) = rows.row_data(idx) else {
                return;
            };
            let source = st.current_path().join(row.name.as_str());
            *st.rename_source.borrow_mut() = Some(source);
            w.set_rename_current_name(row.name.clone());
            w.set_rename_current_is_dir(row.is_dir);
            w.set_rename_conflict(false); // current name == itself → no conflict
            w.set_rename_replace_available(false);
            w.set_rename_new_name(row.name);
            w.set_rename_popup_open(true);
        });
    }
    // Live check of the rename conflict (same spirit as paste).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_rename_check(move |name: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let status = st
                .rename_source
                .borrow()
                .as_deref()
                .map(|source| rename_name_status_now(&st, source, name.as_ref()))
                .unwrap_or(RenameNameStatus::Invalid);
            w.set_rename_conflict(status != RenameNameStatus::Valid);
            w.set_rename_replace_available(status == RenameNameStatus::ReplaceableFile);
        });
    }
    // "Type-ahead" filter of the active view — SHARED with the
    // by-extension filter: the same keystrokes feed one OR the other depending
    // on whether the active tab's "extension filter" mode is on. -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_filter_type(move |ch: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if st.active_ext_filter_on() {
                st.with_tabs_mut(|b| {
                    let a = b.active;
                    b.tabs[a].ext_filter.push_str(ch.as_ref());
                });
            } else {
                st.filter.borrow_mut().push_str(ch.as_ref());
            }
            let cur = st.current_path();
            refresh_listing(&w, &st, &cur);
            w.set_active_filter(st.filter.borrow().clone().into());
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_filter_backspace(move || {
            let Some(w) = weak.upgrade() else { return };
            if st.active_ext_filter_on() {
                st.with_tabs_mut(|b| {
                    let a = b.active;
                    b.tabs[a].ext_filter.pop();
                });
            } else {
                st.filter.borrow_mut().pop();
            }
            let cur = st.current_path();
            refresh_listing(&w, &st, &cur);
            w.set_active_filter(st.filter.borrow().clone().into());
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_filter_clear(move || {
            let Some(w) = weak.upgrade() else { return };
            // In extension mode: "close" = turn off the mode (hides the bar)
            // + clear it; otherwise clear the by-name type-ahead filter.
            if st.active_ext_filter_on() {
                st.with_tabs_mut(|b| {
                    let a = b.active;
                    b.tabs[a].ext_filter_on = false;
                    b.tabs[a].ext_filter.clear();
                });
            } else {
                st.filter.borrow_mut().clear();
            }
            let cur = st.current_path();
            refresh_listing(&w, &st, &cur);
            w.set_active_filter(st.filter.borrow().clone().into());
        });
    }
    // ----- Refreshes ALL panels (after a multi-view op: drag-drop…) -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_refresh_all(move || {
            let Some(w) = weak.upgrade() else { return };
            let _ = drain_thumbnail_invalidations(&st);
            // A file operation (copy/move/delete/trash/
            // duplicate/link/restore, or drop onto a folder) has modified the
            // CONTENTS of folders → their cached RECURSIVE mtime is stale. We
            // clear it, like F5, so the "folder modified date" column
            // recomputes on its own (otherwise F5 had to be pressed). Invisible: the
            // recompute is a separate background worker and thumbnails of
            // untouched paths still come from the LRU. No-op if the features
            // are off (empty caches).
            st.rmtime_cache.borrow_mut().clear();
            st.size_cache.borrow_mut().clear();
            refresh_all_panels(&w, &st);
            // The rows now exist, so a completed operation can point at its result.
            apply_focus_after_refresh(&w, &st);
        });
    }
    // File drag'n'drop between views -----
    // Drag start: ensures the grabbed row is selected in the SOURCE
    // view (if it wasn't, only it gets selected).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_file_drag_begin(move |src_panel: i32, row_idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            // A previous drop menu may have been closed without choosing an action.
            // No new gesture should keep its stale frozen paths.
            *st.pending_file_drop.borrow_mut() = None;
            let src = src_panel.max(0) as usize;
            let changed_count = {
                let mut panels = st.panels.borrow_mut();
                let Some(p) = panels.get_mut(src) else {
                    return;
                };
                let already = p
                    .rows_model
                    .row_data(row_idx.max(0) as usize)
                    .map(|r| r.selected)
                    .unwrap_or(false);
                if already {
                    None
                } else {
                    let count = selection_set_only(&p.rows_model, row_idx);
                    let active = p.tabs.active;
                    p.tabs.tabs[active].selection_anchor = row_idx;
                    p.tabs.tabs[active].cursor = row_idx;
                    Some(count)
                }
            };
            if let Some(count) = changed_count {
                // `sel-touch` activates the panel before starting the drag. The
                // footer and the Shift anchor must follow the implicit selection.
                if *st.active_panel.borrow() == src {
                    push_active_footer(&w, &st, count);
                }
            }
        });
    }
    // Real-time validation of the internal target. In the source view, the
    // `selected` flag allows an O(1) check of the common case (folder dropped onto itself).
    // Between two views, we compare paths to cover the same folder
    // displayed on both sides and a drop into a descendant.
    {
        let st = state.clone();
        window.on_file_drop_target_invalid(move |src_panel, target_panel, row| {
            file_drop_target_invalid(&st, src_panel, target_panel, row)
        });
    }
    // Switches to NATIVE DRAG (OLE) when the cursor leaves the window during a
    // file drag → external apps (Notepad++, VSCode, Explorer…)
    // receive the files as CF_HDROP. Blocking (modal loop) until the
    // drop. Windows only; no-op elsewhere.
    {
        let st = state.clone();
        window.on_start_native_drag(move |src_panel: i32| {
            let src = src_panel.max(0) as usize;
            let paths = panel_selected_paths(&st, src);
            if paths.is_empty() {
                return;
            }
            #[cfg(windows)]
            {
                let dropped = crate::winddrag::drag_files(&paths);
                debug!(count = paths.len(), dropped, "native OLE drag finished");
            }
            #[cfg(not(windows))]
            {
                let _ = paths; // no native drag outside Windows (v1)
            }
        });
    }
    // Drop ONTO an executable: if the target row is a .exe/.cmd/.bat…,
    // we LAUNCH it with the dropped files as ARGUMENTS (instead of copying). The
    // Slint side sets file-drop-src/target/row BEFORE calling this callback; `true` =
    // launched (no menu). No-op → `false` (normal Copy/Move/Link menu).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_file_drop_onto_exec(move || -> bool {
            let Some(w) = weak.upgrade() else { return false };
            let target = w.get_file_drop_target_panel().max(0) as usize;
            let row = w.get_file_drop_target_row();
            let src = w.get_file_drop_src_panel();
            let sources = if src < 0 {
                std::mem::take(&mut *st.external_drop_paths.borrow_mut())
            } else {
                panel_selected_paths(&st, src as usize)
            };
            if sources.is_empty() {
                *st.pending_file_drop.borrow_mut() = None;
                return false;
            }

            if row >= 0
                && let Some(exe) = panel_path_at_row(&st, target, row as usize)
                && is_drop_runnable(&exe)
            {
                // Don't launch on itself (the exe is part of the selection).
                let args: Vec<PathBuf> = sources
                    .iter()
                    .filter(|path| *path != &exe)
                    .cloned()
                    .collect();
                if !args.is_empty() {
                    *st.pending_file_drop.borrow_mut() = None;
                    match actions::run_with_files(&exe, &args) {
                        Ok(()) => info!(exe = %exe.display(), n = args.len(), "drop onto executable: run with args"),
                        Err(err) => error!(error = %err, "drop-onto-exe run failed"),
                    }
                    return true;
                }
            }

            let destination = if row >= 0 {
                panel_folder_at_row(&st, target, row as usize)
                    .unwrap_or_else(|| panel_dir(&st, target))
            } else {
                panel_dir(&st, target)
            };
            *st.pending_file_drop.borrow_mut() = if destination.as_os_str().is_empty() {
                None
            } else {
                Some(PendingFileDrop {
                    sources,
                    destination,
                })
            };
            false
        });
    }
    // Drop: executes the chosen action (0 move · 1 copy · 2 link) from the
    // source view to the target view's folder.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_file_drop_action(move |action: i32| {
            let Some(w) = weak.upgrade() else { return };
            let Some(PendingFileDrop {
                sources,
                destination: dst,
            }) = st.pending_file_drop.borrow_mut().take()
            else {
                return;
            };
            if sources.is_empty() || dst.as_os_str().is_empty() {
                return;
            }
            // Never drop a folder INTO itself or one of its
            // descendants (recursion), nor onto itself.
            let sources: Vec<PathBuf> = sources
                .into_iter()
                .filter(|s| s.as_path() != dst.as_path() && !dst.starts_with(s))
                .collect();
            if sources.is_empty() {
                return;
            }
            match action {
                0 => begin_paste(&w, &st, ClipOp::Cut, dst, sources), // move
                1 => begin_paste(&w, &st, ClipOp::Copy, dst, sources), // copy
                2 => {
                    // Link: symlink (Linux) / junction|hardlink (Windows). Sync
                    // (instant) → we refresh all views at the end.
                    let mut created = Vec::new();
                    for src in &sources {
                        match ops::link_into(src, &dst) {
                            Ok(path) => created.push(path),
                            Err(err) => {
                                error!(error = %err, src = %src.display(), "link failed");
                            }
                        }
                    }
                    invalidate_thumbnail_paths(&st, &created);
                    w.invoke_refresh_all();
                }
                _ => {}
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_rename_confirmed(move |new_name: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let new = new_name.to_string();
            let Some(from) = st.rename_source.borrow().clone() else {
                return;
            };
            let old = from
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if new.is_empty() || new == old {
                return;
            }
            // Final safeguard: never overwrite an existing item (the UI already
            // blocks this; this protects against a race / a programmatic call).
            if rename_name_status_now(&st, &from, &new) != RenameNameStatus::Valid {
                warn!(name = %new, "rename: name conflicts, ignoring");
                return;
            }
            // The name ACTUALLY obtained (authoritative: the path returned by the OS), to
            // find the row again after re-sorting.
            let renamed_to = match ops::rename_in_place(&from, &new) {
                Ok(to) => {
                    info!(from = %from.display(), to = %to.display(), "renamed");
                    Some(to)
                }
                Err(err) => {
                    error!(error = %err, "rename failed");
                    report_rename_failure(
                        weak.clone(),
                        vec![from.clone()],
                        st.snapshot_config().language,
                        err.to_string(),
                    );
                    None
                }
            };
            *st.rename_source.borrow_mut() = None;
            if let Some(to) = renamed_to.as_ref() {
                invalidate_thumbnail_paths(&st, &[from.clone(), to.clone()]);
            }
            // Refresh EVERY panel, not just the active one, so other views on the
            // same folder show the rename immediately — harmonized with
            // delete/copy/move. Also clears the recursive-mtime cache.
            w.invoke_refresh_all();
            // Sorting may have moved the entry (sometimes off-screen): we
            // re-select it and bring it into view.
            if let Some(name) = renamed_to.and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            }) {
                focus_entry_by_name(&w, &st, &name);
            }
        });
    }
    // Forced replace: offered only for a file↔file conflict.
    // The core primitive uses the OS's atomic replace; no prior
    // `remove` that could lose both files.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_rename_force_replace(move |new_name: SharedString| -> bool {
            let Some(w) = weak.upgrade() else {
                return false;
            };
            let new = new_name.to_string();
            let Some(from) = st.rename_source.borrow().clone() else {
                return false;
            };
            if rename_name_status_now(&st, &from, &new) != RenameNameStatus::ReplaceableFile {
                warn!(name = %new, "rename force replace: target cannot be replaced");
                return false;
            }
            let replaced_to = match ops::rename_in_place_replacing_file(&from, &new) {
                Ok(to) => {
                    info!(from = %from.display(), to = %to.display(), "renamed with replacement");
                    Some(to)
                }
                Err(err) => {
                    error!(error = %err, "rename force replace failed");
                    let mut candidates = vec![from.clone()];
                    if let Some(parent) = from.parent() {
                        candidates.push(parent.join(&new));
                    }
                    report_rename_failure(
                        weak.clone(),
                        candidates,
                        st.snapshot_config().language,
                        err.to_string(),
                    );
                    None
                }
            };
            let Some(replaced_to) = replaced_to else {
                return false;
            };
            *st.rename_source.borrow_mut() = None;
            invalidate_thumbnail_paths(&st, &[from, replaced_to]);
            // Refresh every panel (see `on_rename_confirmed`).
            w.invoke_refresh_all();
            true
        });
    }
    // Live name check for New folder / New file. It
    // reuses exactly the same source of truth as Rename; the
    // shortcut/link modes keep their specific rules (derived name allowed).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_create_check(move |kind: i32, name: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            if kind != 0 && kind != 1 {
                w.set_create_name_valid(true);
                w.set_create_conflict(false);
                return;
            }
            let cur = st.current_path();
            match entry_name_availability_now(&st, &cur, name.trim()) {
                EntryNameAvailability::Available => {
                    w.set_create_name_valid(true);
                    w.set_create_conflict(false);
                }
                EntryNameAvailability::Invalid => {
                    w.set_create_name_valid(false);
                    w.set_create_conflict(false);
                }
                // A reserved name is syntactically fine but already spoken for,
                // so it reads as a conflict just like an existing entry.
                EntryNameAvailability::ExistingNonDirectory
                | EntryNameAvailability::ExistingDirectory
                | EntryNameAvailability::Reserved => {
                    w.set_create_name_valid(true);
                    w.set_create_conflict(true);
                }
            }
        });
    }
    // Creation of a new folder / file / shortcut (empty-area popup) in
    // the active panel's current folder. `kind`: 0 = folder, 1 = file,
    // 2 = .lnk shortcut (Windows).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_create_confirmed(
            move |kind: i32, name: SharedString, target: SharedString| -> bool {
                let Some(w) = weak.upgrade() else {
                    return false;
                };
                let name = name.trim().to_string();
                let cur = st.current_path();
                let created = if kind == 2 {
                    create_shortcut_entry(&cur, &name, target.trim())
                } else if kind == 3 {
                    create_link_entry(&w, &cur, &name, target.trim())
                } else if name.is_empty() {
                    return false;
                } else {
                    // Final safeguard against an external navigation/change between the
                    // keystroke and the click. A creation never offers to replace.
                    if entry_name_availability_now(&st, &cur, &name)
                        != EntryNameAvailability::Available
                    {
                        w.set_create_name_valid(ops::is_valid_entry_name(&name));
                        w.set_create_conflict(ops::is_valid_entry_name(&name));
                        return false;
                    }
                    match ops::create_entry(&cur, &name, kind == 0) {
                        Ok(p) => {
                            info!(path = %p.display(), kind, "created entry");
                            p.file_name().map(|n| n.to_string_lossy().into_owned())
                        }
                        Err(err) => {
                            error!(error = %err, "create entry failed");
                            // An entry may have appeared after the guard check. The popup
                            // stays open and then reflects the newly
                            // observed conflict; `create_entry` guarantees it is left intact.
                            if matches!(
                                entry_name_availability_now(&st, &cur, &name),
                                EntryNameAvailability::ExistingNonDirectory
                                    | EntryNameAvailability::ExistingDirectory
                                    | EntryNameAvailability::Reserved
                            ) {
                                w.set_create_name_valid(true);
                                w.set_create_conflict(true);
                            }
                            None
                        }
                    }
                };
                // A newly created HIDDEN entry (dotfile) would land out of sight
                // when the view isn't showing hidden files — a user might create
                // `.clang-format` and never find it. So, ONLY when the created
                // name is a dotfile, turn on "show hidden" for the active view
                // before the refresh below reveals it.
                if created.as_deref().is_some_and(|n| n.starts_with('.')) {
                    let active = *st.active_panel.borrow();
                    if let Some(panel) = st.panels.borrow_mut().get_mut(active) {
                        let t = &mut panel.tabs.tabs[panel.tabs.active];
                        t.show_hidden = true;
                    }
                }
                if let Some(created_name) = created.as_ref() {
                    invalidate_thumbnail_paths(&st, &[cur.join(created_name)]);
                }
                // Refresh every panel: other views on the same folder must show
                // the new entry immediately (like delete/copy/move).
                w.invoke_refresh_all();
                // Targets the new entry (cursor + selection) → a simple Enter
                // opens it right after (especially a folder just created).
                if let Some(created_name) = created {
                    focus_entry_by_name(&w, &st, &created_name);
                    true
                } else {
                    false
                }
            },
        );
    }
    // "Browse…" (shortcut popup): native picker → fills in the target and,
    // if the name is empty, pre-fills it from the target's name.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_create_browse_target(move || {
            let Some(w) = weak.upgrade() else { return };
            // Folder tab → folder picker; File tab → file
            // picker ("All files").
            let picked = if w.get_create_shortcut_folder() {
                openwith::browse_for_folder()
            } else {
                openwith::browse_for_target(st.snapshot_config().language)
            };
            if let Some(path) = picked {
                if w.get_create_name().trim().is_empty() {
                    // Pre-fills the name: folder name OR file name without extension.
                    let p = Path::new(&path);
                    let derived = if w.get_create_shortcut_folder() {
                        p.file_name()
                    } else {
                        p.file_stem()
                    };
                    if let Some(n) = derived {
                        w.set_create_name(n.to_string_lossy().into_owned().into());
                    }
                }
                w.set_create_target(path.into());
            }
        });
    }

    // Right-click in a panel's empty area → "background" menu.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_empty_right_clicked(move |x: f32, y: f32| {
            let Some(w) = weak.upgrade() else { return };
            // After deselection, targetless actions (Paste, copy path,
            // etc.) apply to the current folder.
            selection_set_all(&st.active_rows_model(), false);
            st.set_selection_anchor(-1);
            // Commands pinned to the view BACKGROUND (bit 4) — target the
            // current folder (e.g. "Git Bash here").
            let n_custom = push_ctx_custom_entries(&w, &st, openers::CTX_BACKGROUND, &[]);
            w.set_ctx_custom_bg(true);
            // Shell menu of the CURRENT FOLDER "as an item":
            // exposes the full PeaZip menu, Git Bash here, Select left folder…
            // where Windows's "background" menu is much sparser.
            let cur = st.current_path();
            let n_shell = refresh_shell_menu(&w, &st, std::slice::from_ref(&cur));
            // Reduced menu (empty area): 194 px + pinned + shell.
            let h = 194.0
                + if n_custom > 0 {
                    n_custom as f32 * 26.0 + 1.0
                } else {
                    0.0
                }
                + if n_shell > 0 {
                    n_shell as f32 * 26.0 + 1.0
                } else {
                    0.0
                };
            let (x, y) = clamp_ctx_menu_pos(&w, x, y, h);
            w.set_ctx_on_empty(true);
            w.set_ctx_menu_x(x);
            w.set_ctx_menu_y(y);
            w.set_ctx_menu_open(true);
        });
    }

    // Deliveries from the trash workers. This single callback keeps the
    // native PathBufs until the UI thread and centralizes updating
    // the history, the notices, and the refresh.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_process_trash_events(move || {
            let Some(w) = weak.upgrade() else { return };
            let events: Vec<TrashDelivery> = {
                let mut queue = st
                    .trash_deliveries
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                queue.drain(..).collect()
            };
            for event in events {
                match event {
                    TrashDelivery::Trashed(path) => {
                        *st.last_trashed.borrow_mut() = Some(path);
                    }
                    TrashDelivery::RestoreFinished {
                        op_id,
                        original_path,
                        error,
                    } => {
                        if let Some(handle) = st.ops.finish(op_id) {
                            invalidate_thumbnail_paths(&st, &handle.thumbnail_invalidations);
                        }
                        sync_op_busy(&w, &st);
                        let lang = st.snapshot_config().language;
                        let name = original_path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| original_path.display().to_string());
                        if let Some(err) = error {
                            error!(
                                path = %original_path.display(),
                                error = ?err,
                                "restore from trash failed"
                            );
                            let reason = i18n::trash_error_message(lang, &err);
                            let message =
                                i18n::tr(lang, "trash_restore_failed").replace("{name}", &name);
                            show_notice(&w, format!("{message}: {reason}"));
                        } else {
                            if st.last_trashed.borrow().as_ref() == Some(&original_path) {
                                st.last_trashed.borrow_mut().take();
                            }
                            let message = i18n::tr(lang, "trash_restored").replace("{name}", &name);
                            notice(&w, message, NoticeKind::Success);
                            // Several views may be displaying the restored folder.
                            w.invoke_refresh_all();
                        }
                    }
                }
            }
        });
    }

    // Delete: move to trash (background thread + progress bar).
    // Windows + NETWORK volume: no trash → the deletion will be PERMANENT
    // (`ops::trash` switches to `permanent_delete`) → we confirm BEFORE,
    // like Explorer. `is_network_path` is a LOCAL (instant) check.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_delete(move || {
            let Some(w) = weak.upgrade() else { return };
            let paths = selected_paths(&st);
            if paths.is_empty() {
                return;
            }
            if cfg!(windows)
                && paths
                    .iter()
                    .any(|p| ranger_core::places::is_network_path(p))
            {
                *st.delete_pending.borrow_mut() = paths;
                w.set_delete_warning_permanent(false);
                w.set_delete_warning_open(true);
                return;
            }
            let lang = st.snapshot_config().language;
            start_heavy_op(&w, &st, Heavy::Trash(paths), lang, None, None);
        });
    }
    // Ctrl+Z (customizable) restores only the last deleted item,
    // and only if the active view still shows its original folder.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_restore_last_trashed(move || {
            let Some(w) = weak.upgrade() else { return };
            let Some(original_path) = st.last_trashed.borrow().clone() else {
                return;
            };
            let current_path = st.current_path();
            let Some(original_parent) = original_path.parent() else {
                return;
            };
            if !ops::paths_equal(original_parent, &current_path) {
                return;
            }

            // Registered like any long operation, even though it shows no toast:
            // that is what keeps it mutually exclusive with the others.
            // Restoring is a single atomic move, so it offers no cancellation.
            // Restoring writes the item back to where it came from: that path is
            // claimed like any other destination. Moving it back is a single
            // atomic operation, so it offers no cancellation.
            let op_id = st.ops.register(OpHandle {
                cancel: None,
                pending_focus: None,
                targets: vec![original_path.clone()],
                thumbnail_invalidations: vec![original_path.clone()],
                transient_cleanup: None,
            });
            sync_op_busy(&w, &st);
            let deliveries = st.trash_deliveries.clone();
            let weak = w.as_weak();
            std::thread::spawn(move || {
                let error = ops::restore_from_trash(&original_path).err();
                deliver_trash_event(
                    &weak,
                    &deliveries,
                    TrashDelivery::RestoreFinished {
                        op_id,
                        original_path,
                        error,
                    },
                );
            });
        });
    }
    // Shift+Delete: truly permanent deletion on both Windows AND Linux. The
    // combination is resolved by the configurable shortcut map; this
    // callback validates the selection before opening the same modal warning.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_delete_permanent(move || {
            let Some(w) = weak.upgrade() else { return };
            let paths = selected_paths(&st);
            if paths.is_empty() {
                return;
            }
            *st.delete_pending.borrow_mut() = paths;
            w.set_delete_warning_permanent(true);
            w.set_delete_warning_open(true);
        });
    }
    // Confirmation of the shared popup. We consume the selection frozen at
    // opening time: an asynchronous re-listing can't change the target. The explicit mode calls
    // `permanent_delete`; the network mode keeps the `trash` route, which performs
    // its permanent switch only for those Windows volumes.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_delete_warning_confirmed(move || {
            let Some(w) = weak.upgrade() else { return };
            let paths = std::mem::take(&mut *st.delete_pending.borrow_mut());
            if paths.is_empty() {
                return;
            }
            let lang = st.snapshot_config().language;
            let work = if w.get_delete_warning_permanent() {
                Heavy::PermanentDelete(paths)
            } else {
                Heavy::Trash(paths)
            };
            start_heavy_op(&w, &st, work, lang, None, None);
        });
    }

    // Properties: the system's **native** dialog, on both platforms —
    // Windows via the shell (Security/Details/Versions tabs…), Linux via
    // `org.freedesktop.FileManager1` (Dolphin, Nautilus, Nemo, Caja…). There is
    // no more internal panel: as with the other desktop integrations
    // (opening via `xdg-open`, `zenity`/`kdialog` picker…), a failure is
    // simply reported to the user.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_action_properties(move || {
            let Some(w) = weak.upgrade() else { return };
            let paths = selected_paths(&st);
            let Some(first) = paths.first().cloned() else {
                return;
            };
            if let Err(err) = actions::show_native_properties(&first) {
                error!(error = %err, path = %first.display(), "native properties failed");
                let lang = st.snapshot_config().language;
                notice(&w, i18n::tr(lang, "prop_native_failed"), NoticeKind::Error);
            }
        });
    }

    // ----- Sort by clicking a header -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_sort_clicked(move |col_id: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let Some(new_col) = SortColumn::from_code(&col_id) else {
                warn!(col = %col_id, "unknown sort column id");
                return;
            };
            st.with_tabs_mut(|book| {
                let a = book.active;
                let s = &mut book.tabs[a].sort;
                if s.column == new_col {
                    s.order = s.order.flip();
                } else {
                    s.column = new_col;
                    s.order = SortOrder::Asc;
                }
                // sort-column / sort-asc will be pushed via update_panels_ui
                // in the refresh_listing that follows (see below).
                let _ = s;
            });
            let cur = st.current_path();
            if !cur.as_os_str().is_empty() {
                refresh_listing(&w, &st, &cur);
            }
        });
    }

    // Grouping mode: header menu + Ctrl+G -----
    // The header menu and Ctrl+G first activate the target panel → `idx` is
    // the active panel. `refresh_listing` re-sorts (with the new group_mode read
    // from the active tab), applies the type-ahead filter, and preserves the selection.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_set_group_mode(move |idx: i32, mode: SharedString| {
            let Some(w) = weak.upgrade() else { return };
            let Some(gm) = GroupMode::from_code(&mode) else {
                warn!(mode = %mode, "unknown group mode");
                return;
            };
            {
                let mut panels = st.panels.borrow_mut();
                let Some(p) = panels.get_mut(idx as usize) else {
                    return;
                };
                let a = p.tabs.active;
                p.tabs.tabs[a].group_mode = gm;
            }
            let cur = st.current_path();
            if !cur.as_os_str().is_empty() {
                refresh_listing(&w, &st, &cur);
            }
            st.persist_workspace();
        });
    }
    // Extension filter — toggled from the columns menu: shows /
    // hides panel `idx`'s input bar (turning it off clears the text).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ext_filter_toggle(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let p = idx.max(0) as usize;
            let turned_on = {
                let mut panels = st.panels.borrow_mut();
                let Some(pn) = panels.get_mut(p) else { return };
                let a = pn.tabs.active;
                let t = &mut pn.tabs.tabs[a];
                t.ext_filter_on = !t.ext_filter_on;
                if !t.ext_filter_on {
                    t.ext_filter.clear();
                }
                t.ext_filter_on
            };
            // The two filters (name / extension) are MUTUALLY EXCLUSIVE: turning one on clears
            // the other → a single bar, a single keyboard stream.
            if turned_on {
                st.filter.borrow_mut().clear();
            }
            *st.active_panel.borrow_mut() = p;
            switch_active_panel(&w, &st);
            w.set_active_filter(st.filter.borrow().clone().into());
        });
    }

    // : Tabs -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_tab_new(move || {
            let Some(w) = weak.upgrade() else { return };
            let target = st.with_tabs_mut(|book| {
                // New UX: the new tab inherits the path of the
                // previously active tab (instead of always $HOME).
                let inherited = book.tabs[book.active].current_path.clone();
                book.open(inherited);
                let a = book.active;
                book.tabs[a].current_path.clone()
            });
            load_directory(&w, &st, &target, false);
        });
    }
    // Duplicate a tab (tab context menu): copy inserted right
    // after the targeted tab, in the targeted panel (may not be the active one).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_duplicate_tab(move |panel: i32, tab: i32| {
            let Some(w) = weak.upgrade() else { return };
            let p = (panel.max(0) as usize).min(st.panels.borrow().len().saturating_sub(1));
            let done = {
                let mut panels = st.panels.borrow_mut();
                panels[p].tabs.duplicate(tab.max(0) as usize)
            };
            if done {
                *st.active_panel.borrow_mut() = p;
                switch_active_panel(&w, &st);
            }
        });
    }
    // Scroll target of the tab bar (overflow): exact
    // geometric computation on the Rust side (`TabInfo` widths), returns the `viewport-x` in px.
    {
        let st = state.clone();
        window.on_panel_scroll_target(
            move |panel: i32, idx: i32, forward: bool, viewport_x: f32, view_w: f32| {
                tab_scroll_target(&st, panel.max(0) as usize, idx, forward, viewport_x, view_w)
            },
        );
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_tab_closed(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            // Special case: if this is the active panel's last tab AND
            // there's > 1 panel, close the panel instead of refusing.
            let only_tab_left = st.with_tabs(|book| book.tabs.len() == 1);
            let multi_panel = st.panels.borrow().len() > 1;
            if only_tab_left && multi_panel {
                let active_panel = *st.active_panel.borrow();
                let closed_panel = {
                    let mut panels = st.panels.borrow_mut();
                    if active_panel >= panels.len()
                        || !st.layout.borrow_mut().remove_panel(active_panel)
                    {
                        None
                    } else {
                        let closed = panels.remove(active_panel);
                        let mut a = st.active_panel.borrow_mut();
                        if *a >= panels.len() {
                            *a = panels.len() - 1;
                        }
                        Some(closed)
                    }
                };
                if let Some(panel) = closed_panel {
                    remember_closed_panel(&st, panel);
                    w.set_closed_tabs_available(true);
                    switch_active_panel(&w, &st);
                    st.persist_workspace();
                }
                return;
            }
            // Standard case: close a tab among several.
            let closed_and_target: Option<(Tab, PathBuf)> = st.with_tabs_mut(|book| {
                if let Some(closed) = book.close(idx as usize) {
                    let a = book.active;
                    Some((closed, book.tabs[a].current_path.clone()))
                } else {
                    None
                }
            });
            if let Some((closed, p)) = closed_and_target {
                st.remember_closed_tab(closed);
                w.set_closed_tabs_available(true);
                load_directory(&w, &st, &p, false);
                st.persist_workspace();
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_tab_clicked(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let target: Option<PathBuf> = st.with_tabs_mut(|book| {
                if book.select(idx as usize) {
                    let a = book.active;
                    Some(book.tabs[a].current_path.clone())
                } else {
                    None
                }
            });
            if let Some(p) = target {
                load_directory(&w, &st, &p, false);
            }
        });
    }

    // : Panels (split / close of the layout tree) -----
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_panel_split(move |dir: i32| {
            let Some(w) = weak.upgrade() else { return };
            let inherited = st.current_path();
            let new_active = {
                let mut panels = st.panels.borrow_mut();
                if panels.len() >= MAX_PANELS {
                    None
                } else {
                    let active = *st.active_panel.borrow();
                    let new_idx = panels.len();
                    let dir = if dir == 1 {
                        SplitDir::Column
                    } else {
                        SplitDir::Row
                    };
                    let split_ok = st
                        .layout
                        .borrow_mut()
                        .split_leaf(active, dir, new_idx, 0.5, false);
                    if split_ok {
                        // The new view inherits the columns AND the tab bar
                        // position of the source view (the chrome
                        // is duplicated, as the user expects on split).
                        let cols = panels[active].columns.clone();
                        let mode = panels[active].tab_bar_mode;
                        panels.push(Panel::with_mode(inherited, cols, mode));
                        Some(new_idx)
                    } else {
                        None
                    }
                }
            };
            if let Some(idx) = new_active {
                *st.active_panel.borrow_mut() = idx;
                switch_active_panel(&w, &st);
            }
        });
    }
    // A view's tab bar position: 0 top · 1 left ·
    // 2 right. Persisted per view in the workspace.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_set_tab_bar_mode(move |panel: i32, mode: i32| {
            let Some(w) = weak.upgrade() else { return };
            {
                let mut panels = st.panels.borrow_mut();
                let Some(p) = panels.get_mut(panel.max(0) as usize) else {
                    return;
                };
                let m = mode.clamp(0, 2) as u8;
                if p.tab_bar_mode == m {
                    return;
                }
                p.tab_bar_mode = m;
                // The scroll axis changes → the reported offset is stale (the
                // view resets to 0; auto-reveal re-centers the active tab).
                p.tabs_viewport_x = 0.0;
            }
            st.persist_workspace();
            update_panels_ui(&w, &st);
        });
    }
    // Vertical bar width set via the handle — persisted
    // per view. The view has already applied the LIVE resize; we store the
    // EFFECTIVE value (already clamped by the shared formula).
    {
        let st = state.clone();
        window.on_panel_vbar_resized(move |panel: i32, w: f32| {
            {
                let mut panels = st.panels.borrow_mut();
                let Some(p) = panels.get_mut(panel.max(0) as usize) else {
                    return;
                };
                p.vbar_user_w = w.clamp(110.0, 800.0);
            }
            st.persist_workspace();
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_panel_closed(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let closed_panel = {
                let mut panels = st.panels.borrow_mut();
                let idx = idx as usize;
                if panels.len() <= 1 || idx >= panels.len() {
                    None
                } else if !st.layout.borrow_mut().remove_panel(idx) {
                    // Safety: if the tree refuses (last panel), we don't
                    // touch the Vec, to preserve consistency.
                    None
                } else {
                    let closed = panels.remove(idx);
                    let mut active = st.active_panel.borrow_mut();
                    if *active >= panels.len() {
                        *active = panels.len() - 1;
                    } else if idx < *active {
                        *active -= 1;
                    }
                    Some(closed)
                }
            };
            if let Some(panel) = closed_panel {
                remember_closed_panel(&st, panel);
                w.set_closed_tabs_available(true);
                switch_active_panel(&w, &st);
                st.persist_workspace();
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_panel_clicked(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let idx = idx as usize;
            let changed = {
                let panels = st.panels.borrow();
                let mut active = st.active_panel.borrow_mut();
                if idx < panels.len() && idx != *active {
                    *active = idx;
                    true
                } else {
                    false
                }
            };
            if changed {
                // The type-ahead filter is specific to the active view → reset.
                st.filter.borrow_mut().clear();
                w.set_active_filter(SharedString::new());
                switch_active_panel(&w, &st);
            }
        });
    }

    // Previews / thumbnails -----
    // Starts the scheduler's worker POOL for this application state.
    // The scheduler is already concurrency-safe (queue + `in_flight` under a mutex,
    // `take_next` skips an already-taken path, per-path wait/completion); the
    // heavy decoding runs outside the lock and never touches `AppState`. Going from
    // 1 to N threads therefore parallelizes the decoding without changing the logic.
    if state.thumb_scheduler.start_once() {
        let workers = thumb_worker_count();
        for _ in 0..workers {
            spawn_thumb_worker(state.thumb_scheduler.clone(), window.as_weak());
        }
        debug!(workers, "thumbnail pool started");
    }
    // The view publishes its exact viewport after every scroll/layout. The bridge
    // adjusts the rendered sub-model with an overscan screen, then re-prioritizes the
    // next thumbnail. No listing or folder scan is triggered.
    {
        let st = state.clone();
        let scheduler = state.thumb_scheduler.clone();
        window.on_panel_viewport_changed(move |panel: i32, top: f32, height: f32| {
            if panel < 0 {
                return;
            }
            let (first, last) = update_panel_render_window(&st, panel as usize, top, height);
            scheduler.update_viewport(panel as usize, first, last);
        });
    }
    // All interactive uses (hover, click, drag, visible range) go
    // through this single hit-test on the rows' precomputed geometry.
    {
        let weak = window.as_weak();
        window.on_panel_row_at_content_y(move |panel: i32, y: f32| -> i32 {
            if panel < 0 {
                return -1;
            }
            // Reading the model already exposed to Slint avoids borrowing `AppState`
            // during a synchronous notification from `VecModel::set_vec` (listings
            // apply their rows under a borrow_mut of the panels).
            let Some(window) = weak.upgrade() else {
                return -1;
            };
            window
                .get_panels()
                .row_data(panel as usize)
                .map(|view| row_index_at_content_y(&view.rows, y))
                .unwrap_or(-1)
        });
    }
    // Background worker for the recursive mtime — same pattern.
    if let Some(rx) = state.rmtime_rx.borrow_mut().take() {
        spawn_rmtime_worker(rx, state.rmtime_gen.clone(), window.as_weak());
    }
    // Background worker for image metadata — same pattern.
    if let Some(rx) = state.imgmeta_rx.borrow_mut().take() {
        spawn_imgmeta_worker(rx, state.imgmeta_gen.clone(), window.as_weak());
    }
    // Image metadata ready (pushed by the worker): cache + apply it to the
    // rows at the matching path.
    {
        let st = state.clone();
        window.on_imgmeta_ready(
            move |panel_idx: i32,
                  row_idx: i32,
                  path: SharedString,
                  resolution: SharedString,
                  depth: SharedString,
                  mtime: SharedString| {
                let key = path.to_string();
                let mtime = mtime.parse::<i64>().unwrap_or(0);
                st.imgmeta_cache.borrow_mut().insert(
                    key.clone(),
                    (mtime, resolution.to_string(), depth.to_string()),
                );
                let target = PathBuf::from(&key);
                let panels = st.panels.borrow();
                let Some(panel) = usize::try_from(panel_idx)
                    .ok()
                    .and_then(|index| panels.get(index))
                else {
                    return;
                };
                let Some(row_index) = usize::try_from(row_idx).ok() else {
                    return;
                };
                let dir = &panel.tabs.tabs[panel.tabs.active].current_path;
                let model = &panel.rows_model;
                let Some(mut row) = model.row_data(row_index) else {
                    return;
                };
                // A watcher/re-sort can recycle the index while reading:
                // the full path remains authoritative before any mutation.
                if row.resolution.is_empty() && dir.join(row.name.as_str()) == target {
                    row.resolution = resolution.clone();
                    row.depth = depth.clone();
                    model.set_row_data(row_index, row);
                }
            },
        );
    }
    // Toggles list <-> previews for panel `idx`'s active tab.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_toggle_preview_mode(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let idx = idx as usize;
            let compact = st.config.borrow().compact_icon_rows_in_preview;
            let preview_now = {
                let mut panels = st.panels.borrow_mut();
                if idx >= panels.len() {
                    return;
                }
                let active = panels[idx].tabs.active;
                let (preview, zoom) = {
                    let t = &mut panels[idx].tabs.tabs[active];
                    t.preview = !t.preview;
                    // The button toggles between the list/thumbnail default levels.
                    t.zoom = if t.preview {
                        THUMB_DEFAULT_ZOOM
                    } else {
                        LIST_DEFAULT_ZOOM
                    };
                    (t.preview, t.zoom)
                };
                refresh_panel_visuals(&panels[idx], preview, zoom, compact);
                preview
            };
            update_panels_ui(&w, &st);
            request_thumbnails(&st);
            // Turning on Previews is when thumbnails matter: it's
            // when we warn (once) if `ffmpeg` is missing for video (Linux).
            if preview_now {
                maybe_warn_ffmpeg_missing(&w, &st);
            }
        });
    }
    // Entry zoom (Ctrl+wheel): adjusts the level of panel `idx`'s
    // active tab, derives the thumbnail mode from it, and recomputes in memory the
    // useful geometry/resolution without re-reading the folder.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_zoom_view(
            move |idx: i32, delta: i32, top: f32, height: f32, pointer_y: f32| {
                let Some(w) = weak.upgrade() else {
                    return top.max(0.0);
                };
                let idx = idx as usize;
                let crossed_mode;
                {
                    let mut panels = st.panels.borrow_mut();
                    if idx >= panels.len() {
                        return top.max(0.0);
                    }
                    let active = panels[idx].tabs.active;
                    let t = &mut panels[idx].tabs.tabs[active];
                    let new_zoom = (t.zoom + delta).clamp(MIN_ZOOM, MAX_ZOOM);
                    if new_zoom == t.zoom {
                        return top.max(0.0); // already at the bounds → nothing to do
                    }
                    let was_preview = t.preview;
                    t.zoom = new_zoom;
                    t.preview = new_zoom >= THUMB_ZOOM;
                    crossed_mode = t.preview != was_preview;
                }
                let compact = st.config.borrow().compact_icon_rows_in_preview;
                {
                    let panels = st.panels.borrow();
                    if let Some(panel) = panels.get(idx) {
                        let tab = &panel.tabs.tabs[panel.tabs.active];
                        // Captures the anchor on the old geometry and performs the
                        // relayout in the SAME pass over the rows. A single visible
                        // selection takes priority; otherwise the row under the pointer, then
                        // the viewport's center. No listing or disk access.
                        let anchored_top = zoom_panel_visuals(
                            panel,
                            tab.preview,
                            tab.zoom,
                            compact,
                            crossed_mode,
                            ZoomViewport {
                                top,
                                height,
                                pointer_y,
                            },
                        );
                        // The final geometry is already available: publishing its
                        // visible range before even returning control prevents a
                        // worker from picking up a stale priority job during the
                        // very short interval preceding the Slint callback.
                        let (first, end) = row_range_for_content_span(
                            &*panel.rows_model,
                            anchored_top,
                            anchored_top + height.max(0.0),
                        );
                        if first < end {
                            st.thumb_scheduler.update_viewport(
                                idx,
                                first as i32,
                                end.saturating_sub(1) as i32,
                            );
                        } else {
                            st.thumb_scheduler.update_viewport(idx, -1, -1);
                        }
                        drop(panels);
                        update_panels_ui(&w, &st);
                        if crossed_mode {
                            request_thumbnails(&st);
                        }
                        return anchored_top;
                    }
                }
                top.max(0.0)
            },
        );
    }
    // Shows / hides hidden files for panel `idx`'s active tab
    //. The button calls `activate` beforehand → `idx` is the panel
    // active one, so `refresh_listing` (which honors the type-ahead filter and preserves
    // the selection) targets the right panel.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_toggle_show_hidden(move |idx: i32| {
            let Some(w) = weak.upgrade() else { return };
            let idx = idx as usize;
            let path = {
                let mut panels = st.panels.borrow_mut();
                if idx >= panels.len() {
                    return;
                }
                let active = panels[idx].tabs.active;
                let t = &mut panels[idx].tabs.tabs[active];
                t.show_hidden = !t.show_hidden;
                t.current_path.clone()
            };
            refresh_listing(&w, &st, &path);
        });
    }
    // Thumbnail ready (pushed by the worker): we cache it + apply it
    // to the row(s) at the matching path, in preview panels.
    {
        let st = state.clone();
        window.on_thumb_ready(move |path: SharedString, serial: i32, img: Image| {
            let key = path.to_string();
            let target = PathBuf::from(&key);
            let locations = st.thumb_scheduler.in_flight_locations(&target, serial);
            if locations.is_empty() {
                // The path was invalidated or superseded while decoding. Never
                // let an old generation repopulate the cache under a reused name.
                st.thumb_scheduler.complete(&target, serial);
                return;
            }
            st.thumb_cache.borrow_mut().put(key.clone(), img.clone());
            let panels = st.panels.borrow();
            for location in locations {
                let Some(panel) = panels.get(location.panel) else {
                    continue;
                };
                let tab = &panel.tabs.tabs[panel.tabs.active];
                if !tab.preview {
                    continue;
                }
                let dir = tab.current_path.clone();
                let model = &panel.rows_model;
                let Some(mut row) = model.row_data(location.row) else {
                    continue;
                };
                // The index may have been recycled by a watcher: the path remains
                // authoritative. A row outside the window keeps only the LRU.
                if row.rendered
                    && row.thumbnail.size().width == 0
                    && dir.join(row.name.as_str()) == target
                {
                    row.thumbnail = img.clone();
                    model.set_row_data(location.row, row);
                }
            }
            // The worker waits for this acknowledgment before choosing the next job:
            // a scroll that happened during the decoding can therefore re-prioritize the
            // queue before the next thumbnail is started.
            st.thumb_scheduler.complete(Path::new(&key), serial);
        });
    }

    // Recursive folder stats ready: cache the mtime and/or size and apply them
    // to the "modified"/"age" cells and the "size" cell of the matching FOLDER
    // row. An empty string for a metric means "not computed" for this job.
    {
        let st = state.clone();
        window.on_folder_stats_ready(
            move |panel_idx: i32,
                  row_idx: i32,
                  path: SharedString,
                  mtime_str: SharedString,
                  size_str: SharedString| {
                let m = mtime_str.parse::<i64>().ok();
                let s = size_str.parse::<u64>().ok();
                if m.is_none() && s.is_none() {
                    return;
                }
                let key = path.to_string();
                if let Some(m) = m {
                    st.rmtime_cache.borrow_mut().insert(key.clone(), m);
                }
                if let Some(s) = s {
                    st.size_cache.borrow_mut().insert(key.clone(), s);
                }
                let lang = st.config.borrow().language;
                let now = now_unix();
                let target = PathBuf::from(&key);
                let panels = st.panels.borrow();
                let Some(panel) = usize::try_from(panel_idx)
                    .ok()
                    .and_then(|index| panels.get(index))
                else {
                    return;
                };
                let Some(row_index) = usize::try_from(row_idx).ok() else {
                    return;
                };
                let dir = &panel.tabs.tabs[panel.tabs.active].current_path;
                let model = &panel.rows_model;
                let Some(mut row) = model.row_data(row_index) else {
                    return;
                };
                if row.is_dir && dir.join(row.name.as_str()) == target {
                    if let Some(m) = m {
                        apply_rmtime_to_row(&mut row, m, now, lang);
                    }
                    if let Some(s) = s {
                        row.size = rfs::format_size(s, lang).into();
                    }
                    model.set_row_data(row_index, row);
                }
            },
        );
    }

    // ----- Column resizing: PER PANEL -----
    // The panel has already applied the widths locally (Slint in-out
    // property); we remember them on the Rust side so they survive
    // model rebuilds (navigation, sort, split, panel switch).
    {
        let st = state.clone();
        // End of a column resize: stores its width
        // per panel (survives model rebuilds). Uniform for any
        // resizable column (name/path/size/modified/ext/resolution/depth).
        window.on_panel_column_resized(move |idx: i32, id: SharedString, width: f32| {
            let mut panels = st.panels.borrow_mut();
            if let Some(p) = panels.get_mut(idx as usize) {
                set_col_width(&mut p.columns, &id, width);
            }
        });
    }
    // check/uncheck a column (right-click header → menu).
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_column_toggle(move |idx: i32, id: SharedString, visible: bool| {
            let Some(w) = weak.upgrade() else { return };
            let id = id.to_string();
            {
                let mut panels = st.panels.borrow_mut();
                if let Some(p) = panels.get_mut(idx as usize) {
                    // The "name" anchor column is never hideable.
                    if id != "name"
                        && let Some(c) = p.columns.iter_mut().find(|c| c.id == id)
                    {
                        c.visible = visible;
                    }
                }
            }
            update_panels_ui(&w, &st);
            // resolution/depth column enabled → computes the missing image
            // metadata (no-op if no relevant column is visible).
            request_imgmeta(&st);
            st.persist_workspace();
        });
    }
    // Reordering a column by drag-and-drop.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_column_moved(move |idx: i32, id: SharedString, delta: i32| {
            let Some(w) = weak.upgrade() else { return };
            let id = id.to_string();
            {
                let mut panels = st.panels.borrow_mut();
                if let Some(p) = panels.get_mut(idx as usize) {
                    reorder_column_by_delta(&mut p.columns, &id, delta as f32);
                }
            }
            update_panels_ui(&w, &st);
            st.persist_workspace();
        });
    }

    // App version (displayed in the Settings header).
    window.set_app_version(env!("CARGO_PKG_VERSION").into());

    // ----- XDG paths (informational in the Settings panel) -----
    window.set_config_dir_path(paths::config_path().display().to_string().into());
    window.set_data_dir_path(paths::data_dir().display().to_string().into());
    window.set_cache_dir_path(paths::cache_dir().display().to_string().into());
    // Windows: `config_dir() == data_dir()` (%APPDATA%\ranger) → `config.toml`
    // lives in the Data dir, so the "Configuration file" row is
    // redundant and hidden. Linux: separate folders → row kept.
    window.set_config_path_redundant(paths::config_dir() == paths::data_dir());

    // Open an XDG folder in the OS's file manager (xdg-open). For config,
    // we open the **folder** (not the toml file) to stay consistent with
    // the other two entries.
    window.on_open_config_dir(|| {
        if let Err(err) = actions::open_path(&paths::config_dir()) {
            error!(error = %err, "xdg_open(config_dir) failed");
        }
    });
    window.on_open_data_dir(|| {
        if let Err(err) = actions::open_path(&paths::data_dir()) {
            error!(error = %err, "xdg_open(data_dir) failed");
        }
    });
    window.on_open_cache_dir(|| {
        if let Err(err) = actions::open_path(&paths::cache_dir()) {
            error!(error = %err, "xdg_open(cache_dir) failed");
        }
    });

    // ----- Tab drag'n'drop -----
    // The intra-instance target index computation (preview + drop) is done on the
    // SLINT SIDE (same formula → visual/logic consistency). The bridge handles multi-
    // instance: on every progress update, if ANOTHER Ranger window is under the
    // cursor, we send it the hover point → it shows its insertion
    // preview. `hover_target` (shared with `tab-drag-completed`) remembers the
    // last hovered instance so it can be sent a "hover end" at the
    // right moment (target change OR end of drag).
    let hover_target = Rc::new(std::cell::Cell::new(0isize));
    {
        let weak = window.as_weak();
        let last = hover_target.clone();
        window.on_tab_drag_progress(move |_from_idx: i32, ax: f32, ay: f32| {
            let Some(w) = weak.upgrade() else { return };
            // Logical client → physical screen (`WindowFromPoint`'s frame of reference).
            let (sx, sy) = window_logical_to_screen(&w, ax, ay);
            let target = crate::winmsg::target_at(sx, sy).unwrap_or(0);
            let prev = last.get();
            if prev != target && prev != 0 {
                crate::winmsg::hover_end(prev); // left the previous instance
            }
            if target != 0 {
                crate::winmsg::hover(target, sx, sy);
            }
            last.set(target);
        });
    }
    {
        let last_hover = hover_target.clone();
        let st = state.clone();
        let weak = window.as_weak();
        window.on_tab_drag_completed(
            move |src_panel: i32, from_idx: i32, target_intra: i32, abs_x: f32, abs_y: f32| {
                let Some(w) = weak.upgrade() else { return };
                // End of drag: clears any hover preview still displayed in
                // another instance. For a TRANSFER, the target instance clears it
                // too upon receiving it; this hover_end covers the other cases (tear-off,
                // cancellation, or moving the cursor out of the hovered instance).
                let hovered = last_hover.replace(0);
                if hovered != 0 {
                    crate::winmsg::hover_end(hovered);
                }

                // The SOURCE panel of the drag is passed explicitly (it may
                // NOT be the active panel — we no longer activate mid-drag since that
                // would rebuild the model and kill the drag).
                let source_panel =
                    (src_panel.max(0) as usize).min(st.panels.borrow().len().saturating_sub(1));

                // --- Drop OUTSIDE this window: transfer to another Ranger
                // instance OR tear-off. `abs_x/abs_y` are in
                // (logical) WINDOW coordinates; we compare against the client dimensions.
                // 24px margin: avoids a tear-off on a simple edge overshoot
                // (and covers most of the title bar at the top → no false
                // positive when going a bit too far up).
                {
                    let sz = w.window().size();
                    let scale = w.window().scale_factor().max(0.1);
                    let ww = sz.width as f32 / scale;
                    let wh = sz.height as f32 / scale;
                    const MARGIN: f32 = 24.0;
                    let outside = abs_x < -MARGIN
                        || abs_y < -MARGIN
                        || abs_x > ww + MARGIN
                        || abs_y > wh + MARGIN;
                    // Physical SCREEN position of the cursor at drop, converted from
                    // the real Win32 CLIENT frame of reference (no borders/title bar).
                    let at = window_logical_to_screen(&w, abs_x, abs_y);
                    let from = from_idx.max(0) as usize;
                    // 1) ANOTHER Ranger window under the cursor → TRANSFER the tab
                    // into that instance. Tested BEFORE `outside`: `WindowFromPoint`
                    // returns the window at the TOP of the z-order — if instance B overlaps
                    // A and the user sees B under the cursor, we do transfer
                    // to B even if the point is still within A's bounds.
                    if let Some(target) = crate::winmsg::target_at(at.0, at.1) {
                        w.set_drag_target_panel(-1);
                        w.set_drag_target_zone(0);
                        w.set_drag_target_gap(-1);
                        w.set_drag_target_gap_panel(-1);
                        match transfer_tab_to(&st, source_panel, from, target, at) {
                            Transfer::Moved => {
                                refresh_all_panels(&w, &st);
                                return;
                            }
                            // Instance emptied → in the process of closing: don't touch anything.
                            Transfer::Emptied => return,
                            // Target frozen/gone → tear off if outside the window, otherwise abandon.
                            Transfer::Failed => {
                                if !outside {
                                    return;
                                }
                            }
                        }
                    }
                    // 2) Outside the window, no target instance → tear-off: new
                    // Ranger instance on the tab's folder.
                    if outside {
                        w.set_drag_target_panel(-1);
                        w.set_drag_target_zone(0);
                        w.set_drag_target_gap(-1);
                        w.set_drag_target_gap_panel(-1);
                        if tear_off_tab(&st, source_panel, from, Some(at)) {
                            refresh_all_panels(&w, &st);
                        }
                        return;
                    }
                }
                // Target + zone computed by the hovered panel (Slint).
                let dtp = w.get_drag_target_panel();
                let zone = w.get_drag_target_zone();
                w.set_drag_target_panel(-1);
                w.set_drag_target_zone(0);
                // Last claimed insertion gap (header) + panel claiming
                // full geometry on the Slint side, exact for any width.
                let gap = w.get_drag_target_gap();
                let gap_panel = w.get_drag_target_gap_panel();
                w.set_drag_target_gap(-1);
                w.set_drag_target_gap_panel(-1);

                // --- Drop on an EDGE → split of the target panel + adoption of the tab.
                if dtp >= 0 && (2..=5).contains(&zone) {
                    let target_panel = dtp as usize;
                    let dir = if zone == 4 || zone == 5 {
                        SplitDir::Column
                    } else {
                        SplitDir::Row
                    };
                    // West/North → the new panel goes first (left/top).
                    let new_first = zone == 2 || zone == 4;
                    let from = from_idx.max(0) as usize;
                    if split_with_tab(&st, source_panel, from, target_panel, dir, new_first) {
                        refresh_all_panels(&w, &st);
                    }
                    return;
                }

                // Drop outside any panel zone (gap, splitter, outside the window):
                // the drag didn't hover any panel → we cancel (the tab stays put).
                if dtp < 0 {
                    return;
                }
                let target_panel = (dtp as usize).min(st.panels.borrow().len().saturating_sub(1));
                let from = from_idx.max(0) as usize;

                if target_panel == source_panel {
                    // Same panel: reorder if an insertion position is
                    // known (drop on the tab bar), otherwise no-op.
                    if target_intra >= 0 {
                        let moved = {
                            let mut panels = st.panels.borrow_mut();
                            source_panel < panels.len()
                                && panels[source_panel]
                                    .tabs
                                    .move_tab(from, target_intra as usize)
                        };
                        if moved {
                            update_panels_ui(&w, &st);
                        }
                    }
                    // The drag (even a no-op one) activates the source panel.
                    *st.active_panel.borrow_mut() = source_panel;
                    update_panels_ui(&w, &st);
                } else {
                    // Cross-view: move the tab into the target panel. If the
                    // source empties out (last tab), it gets closed (merge).
                    // Insertion position: EXACT gap claimed by the TARGET's
                    // header (drop on its tab bar); otherwise — drop in the body
                    // ("move" zone) — at the END of the bar, predictably.
                    let insert_at = if gap >= 0 && gap_panel == dtp {
                        gap as usize
                    } else {
                        st.panels.borrow()[target_panel].tabs.tabs.len()
                    };
                    if let Some(active) =
                        move_tab_between(&st, source_panel, from, target_panel, insert_at)
                    {
                        *st.active_panel.borrow_mut() = active;
                        refresh_all_panels(&w, &st);
                    }
                }
            },
        );
    }

    // Resizing a splitter -----
    // `frac` = the pointer's absolute position as a fraction [0,1] of the container along
    // the split axis. We convert it to a ratio local to the split via the area computed
    // from the tree, then adjust the node. This absolute computation requires
    // no snapshot and doesn't accumulate drift.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_splitter_resized(move |idx: i32, frac: f32| {
            let Some(w) = weak.upgrade() else { return };
            let geom = current_geom(&st);
            let Some(sp) = geom.splitters.get(idx as usize) else {
                return;
            };
            let (area_start, area_len) = match sp.dir {
                SplitDir::Row => (sp.area.x, sp.area.w),
                SplitDir::Column => (sp.area.y, sp.area.h),
            };
            if area_len <= f32::EPSILON {
                return;
            }
            let ratio =
                ((frac - area_start) / area_len).clamp(MIN_SPLIT_RATIO, 1.0 - MIN_SPLIT_RATIO);
            let path = sp.path.clone();
            let changed = st
                .layout
                .borrow_mut()
                .set_ratio(&path, ratio, MIN_SPLIT_RATIO);
            if changed {
                push_geometry_inplace(&w, &st);
            }
        });
    }

    // : Named workspaces -----
    //
    // IMPORTANT: all these callbacks are triggered from an element of the
    // `workspaces` list (click on a row, on an icon, or submitting the
    // rename TextInput). But they modify Slint models
    // (`set_workspaces` via refresh_workspaces_ui, `set_panels` via
    // refresh_listing) — recreating the model DESTROYS the element whose callback
    // is currently executing → "Recursion detected".
    //
    // So we defer the work via `slint::Timer::single_shot(0, …)`: it
    // runs on the next event-loop tick, outside the item's callback.
    // (`invoke_from_event_loop` isn't usable here since it requires `Send`,
    // incompatible with `AppState` = `Rc<RefCell<…>>`.)
    refresh_workspaces_ui(window, &state);
    // Live validation of the "Workspace name" field. Names are compared after
    // trimming and case-insensitively: "Project" and "project" would be
    // impossible to properly distinguish in the title and the dirty indicator.
    {
        let weak = window.as_weak();
        window.on_ws_name_check(move |name: SharedString| {
            if let Some(w) = weak.upgrade() {
                w.set_ws_name_taken(workspace_name_exists(name.as_str()));
            }
        });
    }
    // Reopens the last tab actually closed in the panel whose dead
    // zone opened the menu. Transfers/tear-offs never go through
    // the history and therefore can't be accidentally duplicated.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_reopen_last_closed_tab(move |panel: i32| {
            let Some(w) = weak.upgrade() else { return };
            let Some(saved) = st.pop_closed_tab() else {
                w.set_closed_tabs_available(false);
                return;
            };
            let restored = tab_from_state(saved);
            let target = restored.current_path.clone();
            let panel = {
                let mut panels = st.panels.borrow_mut();
                let panel = (panel.max(0) as usize).min(panels.len().saturating_sub(1));
                let tabs = &mut panels[panel].tabs;
                tabs.tabs.push(restored);
                tabs.active = tabs.tabs.len() - 1;
                panel
            };
            *st.active_panel.borrow_mut() = panel;
            w.set_closed_tabs_available(st.has_closed_tabs());
            load_directory(&w, &st, &target, false);
            st.persist_workspace();
        });
    }
    // Delivery of regular network listings. The worker never captures
    // the AppState (`Rc`): it pushes a Send value into the queue then simply
    // wakes Slint; all model mutation stays on the UI thread.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_async_listing_drain(move || {
            let Some(w) = weak.upgrade() else { return };
            loop {
                let next = st
                    .async_listings
                    .lock()
                    .ok()
                    .and_then(|mut queue| queue.pop_front());
                let Some(delivery) = next else { break };
                apply_async_listing(&w, &st, delivery);
            }
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_save_new(move |name: SharedString| {
            let st = st.clone();
            let weak = weak.clone();
            let name = name.trim().to_string();
            defer(move || {
                let Some(w) = weak.upgrade() else { return };
                if name.is_empty() {
                    return;
                }
                // Final safeguard against a programmatic call or a concurrent
                // creation after the live validation.
                if workspace_name_exists(&name) {
                    w.set_ws_name_taken(true);
                    return;
                }
                let ws = st.capture_workspace();
                match workspace::save_named_workspace(&paths::workspaces_dir(), &name, &ws) {
                    Ok(id) => {
                        info!(id, name, "workspace saved");
                        // The saved workspace becomes the current reference.
                        *st.current_workspace.borrow_mut() = Some(name.clone());
                        st.remember_workspace_saved();
                        st.persist_workspace();
                        update_window_title(&w, &st);
                        w.set_ws_toast(w.get_strings().ws_toast_saved);
                        w.set_ws_name_taken(false);
                    }
                    Err(err) => error!(error = %err, "save named workspace failed"),
                }
                refresh_workspaces_ui(&w, &st);
            });
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_load(move |id: SharedString| {
            let st = st.clone();
            let weak = weak.clone();
            let id = id.to_string();
            defer(move || {
                if let Some(w) = weak.upgrade() {
                    load_named_into(&w, &st, &id);
                }
            });
        });
    }
    // Safeguard: Load goes through here. Clean/ad-hoc current → loads
    // directly; modified current → opens the warning dialog, UNLESS
    // the user has unchecked the safeguard in the settings.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_load_guarded(move |target_id: SharedString| {
            let st = st.clone();
            let weak = weak.clone();
            let target_id = target_id.to_string();
            defer(move || {
                let Some(w) = weak.upgrade() else { return };
                // Safeguard disabled (settings) → direct load, as if
                // the current workspace were clean.
                let dirty = if w.get_ws_warn_unsaved() {
                    dirty_current_workspace(&st)
                } else {
                    None
                };
                match dirty {
                    Some((_cur_id, cur_name)) => {
                        let target_name =
                            workspace::list_named_workspaces(&paths::workspaces_dir())
                                .into_iter()
                                .find(|m| m.id == target_id)
                                .map(|m| m.name)
                                .unwrap_or_default();
                        let s = w.get_strings();
                        w.set_ws_dirty_title_text(
                            s.ws_dirty_title.replace("{name}", &cur_name).into(),
                        );
                        w.set_ws_dirty_body_text(
                            s.ws_dirty_body
                                .replace("{name}", &cur_name)
                                .replace("{target}", &target_name)
                                .into(),
                        );
                        w.set_ws_dirty_target_id(target_id.into());
                        w.set_ws_dirty_reset(false); // pending action = load `target_id`
                        w.set_ws_dirty_warn_open(true);
                    }
                    None => {
                        load_named_into(&w, &st, &target_id);
                        w.set_workspaces_open(false);
                    }
                }
            });
        });
    }
    // "Save and…": overwrites the current workspace with the
    // live state, THEN executes the pending action — load `ws-dirty-target-id`, or
    // start over blank if `ws-dirty-reset`.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_save_and_load(move || {
            let st = st.clone();
            let weak = weak.clone();
            defer(move || {
                let Some(w) = weak.upgrade() else { return };
                if let Some((cur_id, _)) = dirty_current_workspace(&st) {
                    match overwrite_named_from_live(&st, &cur_id) {
                        Ok(()) => info!(id = %cur_id, "workspace saved before switching"),
                        Err(err) => error!(error = %err, "save-and-switch: overwrite failed"),
                    }
                }
                if w.get_ws_dirty_reset() {
                    reset_into(&w, &st);
                } else {
                    let target_id = w.get_ws_dirty_target_id();
                    load_named_into(&w, &st, target_id.as_ref());
                }
                w.set_ws_dirty_warn_open(false);
                w.set_workspaces_open(false);
            });
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_overwrite(move |id: SharedString| {
            let st = st.clone();
            let weak = weak.clone();
            let id = id.to_string();
            defer(move || {
                let Some(w) = weak.upgrade() else { return };
                match overwrite_named_from_live(&st, &id) {
                    Ok(()) => {
                        info!(id = %id, "workspace overwritten");
                        update_window_title(&w, &st);
                        w.set_ws_toast(w.get_strings().ws_toast_updated);
                        refresh_workspaces_ui(&w, &st); // counters up to date
                    }
                    Err(err) => error!(error = %err, id = %id, "overwrite workspace failed"),
                }
            });
        });
    }
    // Quick save of the current named workspace (customizable action,
    // Ctrl+S by default). An ad-hoc state opens the panel to ask for a name.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_save_current(move || {
            let st = st.clone();
            let weak = weak.clone();
            defer(move || {
                let Some(w) = weak.upgrade() else { return };
                let Some(name) = st.current_workspace.borrow().clone() else {
                    // Workspace still unnamed: we open the panel AND arm the
                    // "name" field's focus — the user wanted to SAVE,
                    // so the cursor awaits them directly in the field.
                    w.set_workspaces_open(true);
                    w.set_ws_name_focus_armed(true);
                    return;
                };
                let Some(id) = current_workspace_id(&st) else {
                    let msg = w
                        .get_strings()
                        .ws_notice_save_failed
                        .replace("{name}", &name);
                    notice(&w, msg, NoticeKind::Error);
                    w.set_workspaces_open(true);
                    return;
                };
                match overwrite_named_from_live(&st, &id) {
                    Ok(()) => {
                        info!(id = %id, name, "workspace saved from shortcut");
                        update_window_title(&w, &st);
                        let msg = w.get_strings().ws_notice_saved.replace("{name}", &name);
                        notice_for(&w, msg, NoticeKind::Success, 2_000);
                    }
                    Err(err) => {
                        error!(error = %err, id = %id, "workspace shortcut save failed");
                        let msg = w
                            .get_strings()
                            .ws_notice_save_failed
                            .replace("{name}", &name);
                        notice(&w, msg, NoticeKind::Error);
                    }
                }
            });
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_delete(move |id: SharedString| {
            let st = st.clone();
            let weak = weak.clone();
            let id = id.to_string();
            defer(move || {
                let Some(w) = weak.upgrade() else { return };
                let deleting_current = current_workspace_id(&st).as_deref() == Some(id.as_str());
                match workspace::delete_named_workspace(&paths::workspaces_dir(), &id) {
                    Ok(()) => {
                        info!(id = %id, "workspace deleted");
                        if deleting_current {
                            *st.current_workspace.borrow_mut() = None;
                            *st.saved_workspace.borrow_mut() = None;
                            st.persist_workspace();
                            update_window_title(&w, &st);
                        }
                        w.set_ws_toast(w.get_strings().ws_toast_deleted);
                    }
                    Err(err) => error!(error = %err, id = %id, "delete workspace failed"),
                }
                refresh_workspaces_ui(&w, &st);
            });
        });
    }
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_rename(move |id: SharedString, new_name: SharedString| {
            let st = st.clone();
            let weak = weak.clone();
            let id = id.to_string();
            let new_name = new_name.trim().to_string();
            defer(move || {
                let Some(w) = weak.upgrade() else { return };
                if new_name.is_empty() {
                    return;
                }
                let renaming_current = current_workspace_id(&st).as_deref() == Some(id.as_str());
                match workspace::rename_named_workspace(&paths::workspaces_dir(), &id, &new_name) {
                    Ok(()) => {
                        info!(id = %id, name = new_name, "workspace renamed");
                        if renaming_current {
                            *st.current_workspace.borrow_mut() = Some(new_name.clone());
                            st.persist_workspace();
                            update_window_title(&w, &st);
                        }
                        w.set_ws_toast(w.get_strings().ws_toast_renamed);
                    }
                    Err(err) => error!(error = %err, id = %id, "rename workspace failed"),
                }
                refresh_workspaces_ui(&w, &st);
            });
        });
    }
    // Resets the current layout to the blank state (1 panel, $HOME).
    // DIRECT path (no safeguard): used by the "load without
    // saving" dialog when the target is "new workspace".
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_reset(move || {
            let st = st.clone();
            let weak = weak.clone();
            defer(move || {
                if let Some(w) = weak.upgrade() {
                    reset_into(&w, &st);
                }
            });
        });
    }
    // "Start a new workspace" from the menu: goes through the safeguard
    //. Clean/ad-hoc current or safeguard disabled → direct reset + closes
    // the overlay; modified current → dialog (target = "New workspace").
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_reset_guarded(move || {
            let st = st.clone();
            let weak = weak.clone();
            defer(move || {
                let Some(w) = weak.upgrade() else { return };
                let dirty = if w.get_ws_warn_unsaved() {
                    dirty_current_workspace(&st)
                } else {
                    None
                };
                match dirty {
                    Some((_cur_id, cur_name)) => {
                        let s = w.get_strings();
                        let target_name = s.ws_new_name;
                        w.set_ws_dirty_title_text(
                            s.ws_dirty_title.replace("{name}", &cur_name).into(),
                        );
                        w.set_ws_dirty_body_text(
                            s.ws_dirty_body
                                .replace("{name}", &cur_name)
                                .replace("{target}", &target_name)
                                .into(),
                        );
                        w.set_ws_dirty_reset(true); // the pending action is a reset
                        w.set_ws_dirty_warn_open(true);
                    }
                    None => {
                        reset_into(&w, &st);
                        w.set_workspaces_open(false);
                    }
                }
            });
        });
    }
    // Reverses the sort order of the workspace list.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_toggle_sort(move || {
            let Some(w) = weak.upgrade() else { return };
            w.set_ws_sort_newest_first(!w.get_ws_sort_newest_first());
            refresh_workspaces_ui(&w, &st);
        });
    }
    // Recomputed when the menu opens (is-current/is-dirty flags from the
    // live state). Deferred: `changed workspaces-open` fires during
    // the processing of an event → we update the model on the next tick.
    {
        let st = state.clone();
        let weak = window.as_weak();
        window.on_ws_refresh(move || {
            let st = st.clone();
            let weak = weak.clone();
            defer(move || {
                if let Some(w) = weak.upgrade() {
                    refresh_workspaces_ui(&w, &st);
                }
            });
        });
    }

    // Initial async population of all panels: the window is displayed
    // immediately, then a background thread delivers each listing as it comes in.
    // Delays from various network paths thus never block the
    // interface's startup.
    initial_populate_async(window, &state);
    // The shell menu probe is lazy and only starts when the
    // "Detected Windows entries" sub-tab opens; see `scan_shell_ext`.
    // Window title: "{workspace} — Ranger" (or "Ranger") depending on the
    // named workspace the restored state comes from.
    update_window_title(window, &state);
}

/// Defers `f` to the next tick of the Slint event loop (single-shot 0 ms Timer).
/// Avoids "Recursion detected" re-entrancy when modifying a model from
/// one of its item's callbacks. Doesn't require `Send` (unlike
/// `invoke_from_event_loop`), so it's compatible with `AppState` (`Rc<RefCell>`).
fn defer(f: impl FnOnce() + 'static) {
    slint::Timer::single_shot(std::time::Duration::from_millis(0), f);
}

/// Resolves the id of the currently active named workspace by looking it up
/// by name in the on-disk list. `None` if no named workspace is active, or
/// it can no longer be found.
fn current_workspace_id(state: &AppState) -> Option<String> {
    let name = state.current_workspace.borrow().clone()?;
    workspace::list_named_workspaces(&paths::workspaces_dir())
        .into_iter()
        .find(|meta| meta.name == name)
        .map(|meta| meta.id)
}

/// Overwrites a named workspace with the live state and synchronizes, in this
/// order, the current identity, the single dirty snapshot, and the session workspace.
/// Shared by the Update button, Ctrl+S, and the "save and…" safeguard.
fn overwrite_named_from_live(state: &AppState, id: &str) -> ranger_core::Result<()> {
    let ws = state.capture_workspace();
    workspace::overwrite_named_workspace(&paths::workspaces_dir(), id, &ws)?;
    if let Some(meta) = workspace::list_named_workspaces(&paths::workspaces_dir())
        .into_iter()
        .find(|meta| meta.id == id)
    {
        *state.current_workspace.borrow_mut() = Some(meta.name);
    }
    state.remember_workspace_saved();
    state.persist_workspace();
    Ok(())
}

/// Loads the named workspace `id` (replaces the layout, repopulates, refreshes,
/// persists, updates the title). Shared by `ws-load`, `ws-load-guarded`, and
/// `ws-save-and-load`.
fn load_named_into(window: &MainWindow, state: &AppState, id: &str) {
    match workspace::load_named_workspace(&paths::workspaces_dir(), id) {
        Ok((name, ws)) => {
            info!(id = %id, name, "workspace loaded");
            state.replace_with_workspace(ws);
            *state.current_workspace.borrow_mut() = Some(name);
            state.remember_workspace_saved();
            push_sidebar_sections_ui(window, state);
            window.set_closed_tabs_available(state.has_closed_tabs());
            refresh_all_panels(window, state);
            state.persist_workspace();
            update_window_title(window, state);
        }
        Err(err) => error!(error = %err, id = %id, "load named workspace failed"),
    }
}

/// Starts over with a BLANK workspace (1 panel, `$HOME`, no longer attached to any name).
/// Shared by `ws-reset`, `ws-reset-guarded`, and the "reset" branch of
/// `ws-save-and-load`.
fn reset_into(window: &MainWindow, state: &AppState) {
    state.reset_to_blank();
    push_sidebar_sections_ui(window, state);
    window.set_closed_tabs_available(false);
    refresh_all_panels(window, state);
    state.persist_workspace();
    update_window_title(window, state);
    info!("workspace reset to blank");
}

/// "Content" signature of a workspace for "modified" detection.
///
/// We compare ONLY what matters to the user: **the tabs open per
/// view** (path + sort + per-tab display settings), each view's **tab bar
/// position**, and the four **collapsible sections of the left
/// sidebar**. We deliberately IGNORE volatile fields or ones not faithful to a
/// capture∘load round-trip, which caused false positives: split
/// ratios (`stretch` — always 1.0 at capture time — and ratios carried by `layout`),
/// column widths, vertical bar width, as well as the ACTIVE
/// tab/panel (simple focus, not an open/close).
///
/// All the retained fields are captured DIRECTLY from the live state, written
/// as-is into the TOML, and faithfully reconstructed by `build_panels` → the
/// comparison is stable in both directions.
type TabSig = (String, SortColumn, SortOrder, bool, bool, GroupMode);
type WorkspaceSig = (Vec<(Vec<TabSig>, u8)>, SidebarSectionsState);
fn workspace_signature(ws: &WorkspaceState) -> WorkspaceSig {
    let panels = ws
        .panels
        .iter()
        .map(|p| {
            let tabs: Vec<TabSig> = p
                .tabs
                .iter()
                .map(|t| {
                    (
                        t.path.clone(),
                        t.sort_column,
                        t.sort_order,
                        t.preview,
                        t.show_hidden,
                        t.group_mode,
                    )
                })
                .collect();
            (tabs, p.tab_bar_mode)
        })
        .collect();
    (panels, ws.sidebar_sections)
}

/// Do two workspaces differ in their relevant CONTENT (tabs per view +
/// display settings)? We ignore `workspace_name`, the ratios, and the focus.
fn workspaces_differ(a: &WorkspaceState, b: &WorkspaceState) -> bool {
    workspace_signature(a) != workspace_signature(b)
}

/// The SINGLE source of truth for the "modified workspace" state.
///
/// The saved snapshot is kept in memory; this function never touches
/// disk. The window title AND the `WorkspaceEntry.is_dirty` flag go
/// exclusively through here, so any future change to the criteria stays confined
/// to `workspace_signature` above.
fn current_workspace_is_dirty(state: &AppState) -> bool {
    if state.current_workspace.borrow().is_none() {
        return false;
    }
    let saved = state.saved_workspace.borrow();
    let Some(saved) = saved.as_ref() else {
        return false;
    };
    let live = state.capture_workspace().sanitized();
    workspaces_differ(&live, saved)
}

/// Is the CURRENT named workspace "dirty" (live state ≠ state saved on
/// disk)? Returns `Some((id, name))` if YES, `None` otherwise — ad-hoc current
/// (never named), not found on disk, or identical to the saved one.
fn dirty_current_workspace(state: &AppState) -> Option<(String, String)> {
    let name = state.current_workspace.borrow().clone()?;
    if !current_workspace_is_dirty(state) {
        return None;
    }
    let dir = paths::workspaces_dir();
    let meta = workspace::list_named_workspaces(&dir)
        .into_iter()
        .find(|m| m.name == name)?;
    Some((meta.id, name))
}

/// Reloads the list of named workspaces from disk and pushes it to the UI.
/// The core sorts by recency (most recent first); we reverse it here if
/// the user has switched the sort to "oldest first".
fn refresh_workspaces_ui(window: &MainWindow, state: &AppState) {
    let dir = paths::workspaces_dir();
    let mut metas = workspace::list_named_workspaces(&dir);
    let cur_name = state.current_workspace.borrow().clone();
    // Same source of truth as the title's asterisk — no second comparison
    // logic should appear here.
    let dirty_id: Option<String> = current_workspace_is_dirty(state)
        .then(|| {
            cur_name
                .as_ref()
                .and_then(|name| metas.iter().find(|m| &m.name == name))
                .map(|m| m.id.clone())
        })
        .flatten();
    if !window.get_ws_sort_newest_first() {
        metas.reverse();
    }
    let entries: Vec<WorkspaceEntry> = metas
        .into_iter()
        .map(|m| {
            let is_current = cur_name.as_deref() == Some(m.name.as_str());
            let is_dirty = dirty_id.as_deref() == Some(m.id.as_str());
            WorkspaceEntry {
                id: m.id.into(),
                name: m.name.into(),
                panels: m.panels as i32,
                tabs: m.tabs as i32,
                is_current,
                is_dirty,
            }
        })
        .collect();
    window.set_workspaces(ModelRc::new(VecModel::from(entries)));
}

fn normalized_workspace_name(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Does a workspace already have this name? File ids remain unique,
/// but the UI treats the name as the visible identity; rejecting duplicate names also
/// avoids any ambiguity when tracking the current workspace.
fn workspace_name_exists(name: &str) -> bool {
    let wanted = normalized_workspace_name(name);
    !wanted.is_empty()
        && workspace::list_named_workspaces(&paths::workspaces_dir())
            .iter()
            .any(|meta| normalized_workspace_name(&meta.name) == wanted)
}

/// Kind of a "notice" toast: determines its tone and its icon.
#[derive(Clone, Copy)]
enum NoticeKind {
    /// Danger + closed padlock: access denied, eject failure.
    Error,
    /// Success + open padlock: successful removal/disconnection.
    EjectOk,
    /// Generic success + checkmark: save, validation, etc.
    Success,
    /// Success + bookmark: successfully added to favorites.
    FavAdded,
    /// Info (accent) + bookmark: already present in favorites (neither error nor success).
    FavExists,
    /// Danger + bookmark: favorite whose target cannot be found.
    FavMissing,
}

impl NoticeKind {
    /// `(tone, icon)` — MUST match the Slint codes: tone 0 danger /
    /// 1 success / 2 info; icon 0 padlock / 1 open padlock / 2 checkmark / 3 bookmark.
    fn codes(self) -> (i32, i32) {
        match self {
            NoticeKind::Error => (0, 0),
            NoticeKind::EjectOk => (1, 1),
            NoticeKind::Success => (1, 2),
            NoticeKind::FavAdded => (1, 3),
            NoticeKind::FavExists => (2, 3),
            NoticeKind::FavMissing => (0, 3),
        }
    }
}

/// Displays a "notice" toast with a reading duration **adaptive to the
/// message's length**: ~2.6 s + 55 ms/character, clamped to [2.8 s; 8 s]. A
/// long message (e.g. "Removal failed: 'service X' is using the device")
/// thus stays displayed long enough to be read.
fn show_notice(w: &MainWindow, text: impl Into<SharedString>) {
    notice(w, text, NoticeKind::Error);
}

fn path_notice_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Shared source of truth for deletions and renames refused by a
/// Windows lock. The diagnostic is only triggered AFTER the actual failure.
fn locked_item_notice(path: &Path, lang: Lang) -> Option<String> {
    let lock = ranger_core::process_lock::diagnose(path)?;
    Some(i18n::item_in_use(
        lang,
        &path_notice_name(path),
        &lock.processes,
        lock.truncated,
    ))
}

/// A locked folder may require a bounded probe of its children: never
/// perform this diagnostic on the Slint thread. `candidates` contains the
/// source then, for "Force replace", the target which can also be locked.
fn report_rename_failure(
    weak: slint::Weak<MainWindow>,
    candidates: Vec<PathBuf>,
    lang: Lang,
    reason: String,
) {
    let item = candidates
        .first()
        .map(|path| path_notice_name(path))
        .unwrap_or_default();
    let fallback = i18n::rename_failed(lang, &item, &reason);

    // Restart Manager doesn't exist outside Windows: don't create a thread that
    // could only immediately return the generic message already prepared.
    #[cfg(not(windows))]
    {
        if let Some(window) = weak.upgrade() {
            show_notice(&window, fallback);
        }
    }

    #[cfg(windows)]
    let fallback = Arc::new(fallback);
    #[cfg(windows)]
    let fallback_worker = fallback.clone();
    #[cfg(windows)]
    let weak_worker = weak.clone();
    #[cfg(windows)]
    let spawn = std::thread::Builder::new()
        .name("ranger-lock-diagnose".to_owned())
        .spawn(move || {
            let message = candidates
                .iter()
                .find_map(|path| locked_item_notice(path, lang))
                .unwrap_or_else(|| (*fallback_worker).clone());
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(window) = weak_worker.upgrade() {
                    show_notice(&window, message);
                }
            });
        });
    #[cfg(windows)]
    if let Err(error) = spawn {
        error!(error = %error, "rename lock diagnostic worker unavailable");
        if let Some(window) = weak.upgrade() {
            show_notice(&window, (*fallback).clone());
        }
    }
}

/// "Success" variant (green, open padlock) — e.g. successful safe removal.
fn show_notice_ok(w: &MainWindow, text: impl Into<SharedString>) {
    notice(w, text, NoticeKind::EjectOk);
}

fn notice(w: &MainWindow, text: impl Into<SharedString>, kind: NoticeKind) {
    let text = text.into();
    let chars = text.chars().count() as f32;
    let ms = (2600.0 + 55.0 * chars).clamp(2800.0, 8000.0) as i64;
    notice_for(w, text, kind, ms);
}

/// Variant with an explicit duration for very short confirmations. The visual
/// route remains strictly the same as for other notices.
fn notice_for(w: &MainWindow, text: impl Into<SharedString>, kind: NoticeKind, duration_ms: i64) {
    let (tone, icon) = kind.codes();
    w.set_notice_tone(tone);
    w.set_notice_icon(icon);
    w.set_notice_duration(duration_ms);
    w.set_notice_text(text.into());
}

/// Non-blocking startup-by-workspace-name warning. Exposed to the
/// binary only to reuse exactly the global toast route.
pub fn show_workspace_not_found_notice(window: &MainWindow, state: &AppState, name: &str) {
    let lang = state.config.borrow().language;
    let message = i18n::tr(lang, "ws_not_found").replace("{name}", name);
    notice(window, message, NoticeKind::Error);
}

/// Warns ONCE (state persisted in the config) that `ffmpeg` is missing
/// → no video thumbnails. No-op if already shown, or if `ffmpeg` is present —
/// always the case on Windows, where video goes through the native shell, so no
/// warning there. Called when the user turns on Previews (the moment when
/// thumbnails become relevant). Reuses the global toast route.
fn maybe_warn_ffmpeg_missing(window: &MainWindow, state: &AppState) {
    if state.config.borrow().ffmpeg_hint_shown || actions::ffmpeg_available() {
        return;
    }
    let lang = state.config.borrow().language;
    notice(window, i18n::tr(lang, "ffmpeg_missing"), NoticeKind::Error);
    state.persist_config(|c| c.ffmpeg_hint_shown = true);
}

// ---------- UI zoom (global setting) ----------

/// Proposed UI zoom factors (× the screen scale). `1.0` = the OS's
/// native scale. Startup default = the index of `1.0`.
const UI_SCALE_PRESETS: [f32; 6] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5];

/// Picker labels ("80%", "100%", …) — language-independent.
fn ui_scale_labels() -> Vec<SharedString> {
    UI_SCALE_PRESETS
        .iter()
        .map(|f| format!("{}%", (f * 100.0).round() as i32).into())
        .collect()
}

/// Index of the preset closest to a stored factor (robust to a config
/// value outside the list).
fn ui_scale_nearest_index(factor: f32) -> i32 {
    UI_SCALE_PRESETS
        .iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| (**a - factor).abs().total_cmp(&(**b - factor).abs()))
        .map(|(i, _)| i as i32)
        .unwrap_or(2)
}

/// Applies the UI zoom: (SCREEN scale captured once) × `factor`, clamped, via
/// the public `dispatch_event(ScaleFactorChanged)` API → Slint re-scales and
/// re-lays-out the whole UI (fonts, margins, icons). Doesn't redispatch if
/// the scale is already correct (avoids an unnecessary relayout). The raw OS scale is
/// remembered on the 1st call so it stays the reference even after our own zooms.
fn apply_ui_scale(window: &MainWindow, state: &AppState, factor: f32) {
    let base = state.ui_base_scale.get();
    let base = if base > 0.0 {
        base
    } else {
        let os = window.window().scale_factor().max(0.1);
        state.ui_base_scale.set(os);
        os
    };
    let target = (base * factor).clamp(0.5, 4.0);
    let win = window.window();
    if (win.scale_factor() - target).abs() <= 0.001 {
        return;
    }
    win.dispatch_event(slint::platform::WindowEvent::ScaleFactorChanged {
        scale_factor: target,
    });
    // `ScaleFactorChanged` only SETS the factor: it realigns neither the
    // geometry nor the rendering. Outside fullscreen, a spontaneous `Resized` follows and
    // everything realigns; MAXIMIZED/fullscreen, the physical size is locked by
    // the OS → no `Resized` → the zoom used to only apply after un-maximizing.
    // So we force a `Resized` at the SAME physical size (logical = physical /
    // scale): Slint recomputes the layout and redraws at the new scale WITHOUT
    // resizing the OS window (the event only affects Slint's internal state).
    let phys = win.size();
    if phys.width > 0 && phys.height > 0 {
        win.dispatch_event(slint::platform::WindowEvent::Resized {
            size: slint::LogicalSize::new(
                (phys.width as f32 / target).max(1.0),
                (phys.height as f32 / target).max(1.0),
            ),
        });
    }
}

/// Detects ffmpeg (Linux) and pushes the state to the "Video thumbnails"
/// settings section. Called at startup and on every (re)opening of settings / click on
/// "Recheck". Effectively a no-op on Windows (section hidden, `ffmpeg` unused).
fn apply_ffmpeg_info(window: &MainWindow) {
    let info = actions::ffmpeg_info();
    window.set_ffmpeg_found(info.available);
    window.set_ffmpeg_version(info.version.into());
    window.set_ffmpeg_flatpak(info.flatpak);
    window.set_ffmpeg_detected_index(info.detected_distro);
}

/// Eject operation requested from the drive menu.
#[derive(Clone, Copy)]
enum EjectOp {
    /// Safe removal of a removable drive (USB/CD).
    SafeRemove,
    /// Disconnection of a mapped network drive.
    Disconnect,
}

/// Runs the eject/disconnect on a background thread (may block) then
/// comes back to the UI: result toast + sidebar re-scan on
/// success.
///
/// Before the removal, Ranger releases its own handles on the volumes. The active
/// panel's `notify` watcher holds a directory handle that can block
/// the eject. It is therefore dropped unconditionally, then `invoke_refresh()`
/// re-lists the active panel and re-arms the watcher on return.
fn spawn_eject(window: &MainWindow, state: &AppState, device: String, op: EjectOp) {
    // First invalidate any watcher installation still in flight — otherwise
    // it could resurrect a handle on the volume being ejected.
    state.watcher_gen.fetch_add(1, Ordering::SeqCst);
    if let Ok(mut w) = state.watcher.lock() {
        *w = None; // releases the OS handle (otherwise it self-vetoes)
    }
    let weak = window.as_weak();
    let lang = state.snapshot_config().language;
    std::thread::spawn(move || {
        let res = match op {
            EjectOp::SafeRemove => ranger_core::eject::safe_remove(&device),
            EjectOp::Disconnect => ranger_core::eject::disconnect(&device),
        };
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = weak.upgrade() else { return };
            let s = w.get_strings();
            match res {
                Ok(()) => {
                    show_notice_ok(
                        &w,
                        match op {
                            EjectOp::SafeRemove => s.net_ejected,
                            EjectOp::Disconnect => s.net_disconnected,
                        },
                    );
                    refresh_sidebar(&w); // the drive is gone
                }
                Err(err) => {
                    let reason = i18n::eject_error_message(lang, &err);
                    show_notice(&w, format!("{} : {reason}", s.net_eject_failed));
                }
            }
            w.invoke_refresh(); // re-lists the active panel + re-arms the watcher
        });
    });
}

/// (Re)builds the "Places" sidebar model (drives, shortcuts,
/// network, trash) from `ranger-core::places` and pushes it to the UI.
fn refresh_sidebar(window: &MainWindow) {
    use ranger_core::places::{self, PlaceKind};
    // Icon code (see SidebarItem): 0 folder · 1 home · 2 drive · 3
    // trash · 4 network · 5 phone.
    fn kind_code(k: PlaceKind) -> i32 {
        match k {
            PlaceKind::Folder => 0,
            PlaceKind::Home => 1,
            PlaceKind::Drive => 2,
            PlaceKind::Trash => 3,
            PlaceKind::Network => 4,
        }
    }
    fn place_item(p: places::Place) -> SidebarPlace {
        SidebarPlace {
            label: p.name.into(),
            path: p.path.display().to_string().into(),
            kind: kind_code(p.kind),
            removable: p.removable,
            device: p.device.into(),
        }
    }
    let shortcuts = places::user_places()
        .into_iter()
        .map(place_item)
        .collect::<Vec<_>>();
    // Keep sections strictly grouped: local, Windows portable
    // devices, then network. An MTP isn't a core `Place`/`PathBuf`.
    let filesystem = places::drives();
    let filesystem_drives = filesystem
        .iter()
        .filter(|p| p.kind != PlaceKind::Network)
        .cloned()
        .map(place_item)
        .collect::<Vec<_>>();
    #[cfg(windows)]
    let drives = {
        let mut drives = filesystem_drives;
        for device in crate::winportable::devices() {
            drives.push(SidebarPlace {
                label: device.name.into(),
                path: device.shell_path.into(),
                kind: 5,
                removable: false,
                device: SharedString::new(),
            });
        }
        drives
    };
    #[cfg(not(windows))]
    let drives = filesystem_drives;
    let network = filesystem
        .into_iter()
        .filter(|p| p.kind == PlaceKind::Network)
        .map(place_item)
        .collect::<Vec<_>>();
    let trash = vec![place_item(places::trash_place())];

    // The four models remain semantic and stable; only their Slint rank
    // varies with the global preference. Trash stays outside this ordering.
    window.set_sidebar_places_drives(ModelRc::new(VecModel::from(drives)));
    window.set_sidebar_places_shortcuts(ModelRc::new(VecModel::from(shortcuts)));
    window.set_sidebar_places_network(ModelRc::new(VecModel::from(network)));
    window.set_sidebar_places_trash(ModelRc::new(VecModel::from(trash)));
}

/// Pushes the workspace-specific collapse state and the global config order to Slint.
/// Called at startup, when loading a workspace, and on reset; no
/// parallel visual logic decides default values.
fn push_sidebar_sections_ui(window: &MainWindow, state: &AppState) {
    let sections = state.sidebar_sections.get();
    window.set_sidebar_shortcuts_collapsed(sections.shortcuts_collapsed);
    window.set_fav_section_collapsed(sections.favorites_collapsed);
    window.set_sidebar_drives_collapsed(sections.drives_collapsed);
    window.set_sidebar_network_collapsed(sections.network_collapsed);
    window.set_sidebar_section_order(ModelRc::new(VecModel::from(
        state
            .snapshot_config()
            .sidebar_section_order
            .into_iter()
            .map(|section| section.index() as i32)
            .collect::<Vec<_>>(),
    )));
}

/// Combined sidebar signature: core drive letters/mounts + Windows WPD
/// cache revision. The read is instant and never touches COM.
fn sidebar_drives_signature() -> u64 {
    let filesystem = ranger_core::places::drives_signature();
    #[cfg(windows)]
    {
        filesystem ^ crate::winportable::signature().rotate_left(32)
    }
    #[cfg(not(windows))]
    {
        filesystem
    }
}

// Tree-structured favorites ----------

/// Persists the favorites tree (logs on failure; never fatal).
fn save_favorites(state: &AppState) {
    if let Err(err) = state.favorites.borrow().save(&paths::favorites_path()) {
        error!(error = %err, "save favorites failed");
    }
}

/// Converts a flattened core node into a Slint struct (existence checked for
/// favorites → grayed out if the path is missing).
fn flat_to_favnode(f: &FlatFav) -> FavNode {
    let available = f.is_container || f.path.is_empty() || Path::new(&f.path).exists();
    FavNode {
        id: f.id.clone().into(),
        label: f.name.clone().into(),
        path: f.path.clone().into(),
        is_container: f.is_container,
        depth: f.depth,
        expanded: f.expanded,
        has_children: f.has_children,
        available,
    }
}

// "Open with" openers ----------

/// Persists the openers store (logs on failure; never fatal).
fn save_openers(state: &AppState) {
    if let Err(err) = state.openers.borrow().save(&paths::openers_path()) {
        error!(error = %err, "save openers failed");
    }
}

/// Clamped position of the context menu so it doesn't overflow the window.
/// The dimensions follow the Slint template (`width: 240px`; height 402 px
/// for the full menu, 220 px for the "empty area" menu, see `ctx-on-empty`).
fn clamp_ctx_menu_pos(w: &MainWindow, x: f32, y: f32, menu_h: f32) -> (f32, f32) {
    const MENU_W: f32 = 240.0;
    let win_size = w.window().size();
    let scale = w.window().scale_factor().max(0.01);
    let win_w = win_size.width as f32 / scale;
    let win_h = win_size.height as f32 / scale;
    (
        x.min(win_w - MENU_W - 4.0).max(4.0),
        y.min(win_h - menu_h - 4.0).max(4.0),
    )
}

/// Is the selection an executable (→ "Use as application")?
fn is_executable_path(p: &Path) -> bool {
    #[cfg(windows)]
    {
        matches!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("exe") | Some("com")
        )
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        p.is_file()
            && p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
}

/// "Launchable by drop" target: broader than `is_executable_path` —
/// includes batch SCRIPTS, which are launched with the dropped files as arguments.
/// Windows: exe/com/cmd/bat. Other OSes: executable bit AND a type plausibly a
/// program (`FileKind::can_be_program`) → an "executable" image/doc (mode
/// 0777) is NOT a target. MUST stay consistent with the rows' `drop_runnable`
/// field (same hover feedback as the actual action).
fn is_drop_runnable(p: &Path) -> bool {
    #[cfg(windows)]
    {
        matches!(
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("exe") | Some("com") | Some("cmd") | Some("bat")
        )
    }
    #[cfg(not(windows))]
    {
        is_executable_path(p)
            && rfs::classify_kind(p.extension().and_then(|e| e.to_str()), false).can_be_program()
    }
}

/// Builds a `slint::Image` from the exe's icon (empty if unavailable).
fn opener_icon(path: &str) -> Image {
    match openwith::icon_rgba(path) {
        Some((rgba, w, h)) => Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            &rgba, w, h,
        )),
        None => Image::default(),
    }
}

fn opener_matches_filter(opener: &openers::Opener, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let haystack = format!(
        "{} {} {} {} {} {}",
        opener.label,
        opener.program,
        opener.assoc.as_deref().unwrap_or_default(),
        opener.args.join(" "),
        opener.default_exts.join(" "),
        opener.used_exts.join(" ")
    )
    .to_lowercase();
    haystack.contains(needle)
}

/// Returns the persisted recipe icon, with a lightweight fallback for commands
/// created by Ranger versions that predate the `icon` field. Matching the
/// executable leaf also gives manually written commands for these well-known
/// tools the unsurprising icon, without probing the filesystem.
fn effective_opener_icon(opener: &openers::Opener) -> openers::OpenerIcon {
    if opener.icon != openers::OpenerIcon::None {
        return opener.icon;
    }
    let program = opener.program.trim().to_ascii_lowercase();
    let leaf = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program.as_str());
    let leaf = leaf.strip_suffix(".exe").unwrap_or(leaf);
    match leaf {
        "7z" | "7zz" | "7za" => openers::OpenerIcon::SevenZip,
        "tar" => openers::OpenerIcon::Archive,
        "xdg-email" => openers::OpenerIcon::Email,
        "kdeconnect-handler" | "bluetooth-sendto" | "blueman-sendto" => openers::OpenerIcon::Device,
        _ => openers::OpenerIcon::None,
    }
}

/// Builds the shared Slint row for Settings, the Open-with flyout and pinned
/// context-menu commands. Built-in SVGs need no executable-icon extraction.
fn opener_to_item(opener: &openers::Opener) -> OpenerItem {
    let icon = effective_opener_icon(opener);
    OpenerItem {
        id: opener.id.clone().into(),
        label: opener.label.clone().into(),
        available: opener.assoc.is_some() || Path::new(&opener.program).is_file(),
        icon: if icon == openers::OpenerIcon::None {
            opener_icon(&opener.program)
        } else {
            Image::default()
        },
        icon_kind: icon.as_i32(),
    }
}

/// Applies the filter to the cache of already-built items. Cloning a Slint `Image`
/// is a resource share; no Shell extraction or `Path::is_file` happens here.
fn push_filtered_openers_ui(window: &MainWindow, state: &AppState) {
    let needle = state.opener_filter.borrow().trim().to_lowercase();
    let store = state.openers.borrow();
    let cache = state.opener_settings_cache.borrow();
    let visible: Vec<OpenerItem> = cache
        .iter()
        .filter(|item| {
            store
                .get(item.id.as_str())
                .is_some_and(|opener| opener_matches_filter(opener, &needle))
        })
        .cloned()
        .collect();
    window.set_openers_all(ModelRc::new(VecModel::from(visible)));
}

/// Pushes the opener lists to the GUI (suggested submenu + Settings list).
/// The "Open with" flyout is filtered by the selected file's EXTENSION
/// → only suited programs, no more mixing. The Settings
/// list is built once, cached, then filtered in memory.
fn push_openers_ui(window: &MainWindow, state: &AppState) {
    // Extension of the 1st selected file (empty → falls back to global MRU).
    let ext = selected_paths(state)
        .first()
        .and_then(|p| {
            p.extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
        })
        .unwrap_or_default();
    let (suggested, all) = {
        let st = state.openers.borrow();
        let suggested: Vec<OpenerItem> = st
            .suggested_for_ext(&ext, 8)
            .into_iter()
            .map(opener_to_item)
            .collect();
        let all: Vec<OpenerItem> = st.openers.iter().map(opener_to_item).collect();
        (suggested, all)
    };
    window.set_openers_suggested(ModelRc::new(VecModel::from(suggested)));
    *state.opener_settings_cache.borrow_mut() = all;
    push_filtered_openers_ui(window, state);
}

/// Pushes into `ctx-custom-entries` the commands pinned for context
/// `bit` (CTX_FILE / CTX_DIR / CTX_BACKGROUND) and returns their count — used
/// on every context menu OPENING for both content AND height.
fn push_ctx_custom_entries(
    window: &MainWindow,
    state: &AppState,
    bit: u8,
    targets: &[PathBuf],
) -> usize {
    let store = state.openers.borrow();
    let items: Vec<OpenerItem> = store
        .for_context(bit)
        .into_iter()
        .filter(|o| ctx_entry_applies(o, bit, targets))
        .map(opener_to_item)
        .collect();
    let n = items.len();
    window.set_ctx_custom_entries(ModelRc::new(VecModel::from(items)));
    n
}

/// Does a pinned command belong in the menu for `targets`?
///
/// Only the FILE entry is filtered — folders and the view background have no
/// extension. EVERY selected file must match: the command runs on all of them,
/// so offering it when some would fail is a trap. A single selection, the
/// common case, is unaffected either way.
fn ctx_entry_applies(opener: &openers::Opener, bit: u8, targets: &[PathBuf]) -> bool {
    if bit != openers::CTX_FILE || opener.ctx_exts.is_empty() {
        return true;
    }
    targets.iter().all(|p| {
        let ext = p
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        opener.matches_ctx_ext(&ext)
    })
}

/// (Re)builds the Windows SHELL context menu for `targets` (selection, or
/// [current folder] for the dead zone — queried "as an item",
/// the item-context approach) and pushes the entries to the UI. The `IContextMenu`
/// session is kept in the state (the ids are only valid for it). Returns the
/// number of entries (menu height). Disabled / failed / non-Windows → 0.
fn refresh_shell_menu(window: &MainWindow, state: &AppState, targets: &[PathBuf]) -> usize {
    window.set_ctx_shell_sub_open(false); // resets the flyout from a previous opening
    let enabled = state.config.borrow().shell_ctx_menu;
    if !enabled || targets.is_empty() {
        *state.shell_menu.borrow_mut() = None;
        window.set_ctx_shell_entries(ModelRc::new(VecModel::from(Vec::<ShellCtxEntry>::new())));
        return 0;
    }
    let work_dir = state.current_path();
    match crate::shellmenu::build_for_paths(targets, &work_dir) {
        Some((session, entries)) => {
            // Feeds the settings' "detected entries" list.
            {
                let mut known = state.shell_known.borrow_mut();
                let mut grew = false;
                for e in &entries {
                    if !known.iter().any(|k| k == &e.label) {
                        known.push(e.label.clone());
                        grew = true;
                    }
                }
                drop(known);
                if grew {
                    refresh_shell_ext_rows(window, state);
                }
            }
            // Filters out entries HIDDEN by the user (by label).
            let disabled = state.config.borrow().shell_menu_disabled.clone();
            let mut items: Vec<ShellCtxEntry> = Vec::new();
            let mut subs: Vec<Vec<ShellCtxEntry>> = Vec::new();
            for e in &entries {
                if disabled.iter().any(|d| d == &e.label) {
                    continue;
                }
                let (has_sub, sub) = if e.children.is_empty() {
                    (false, -1)
                } else {
                    subs.push(
                        e.children
                            .iter()
                            .map(|c| ShellCtxEntry {
                                id: c.id as i32,
                                label: c.label.clone().into(),
                                icon: shell_icon(&c.icon),
                                monochrome: shell_icon_monochrome(&c.icon),
                                has_sub: false,
                                sub: -1,
                            })
                            .collect(),
                    );
                    (true, subs.len() as i32 - 1)
                };
                items.push(ShellCtxEntry {
                    id: e.id as i32,
                    label: e.label.clone().into(),
                    icon: shell_icon(&e.icon),
                    monochrome: shell_icon_monochrome(&e.icon),
                    has_sub,
                    sub,
                });
            }
            let n = items.len();
            *state.shell_menu.borrow_mut() = Some(session);
            *state.shell_subs.borrow_mut() = subs;
            window.set_ctx_shell_entries(ModelRc::new(VecModel::from(items)));
            n
        }
        None => {
            *state.shell_menu.borrow_mut() = None;
            *state.shell_subs.borrow_mut() = Vec::new();
            window.set_ctx_shell_entries(ModelRc::new(VecModel::from(Vec::<ShellCtxEntry>::new())));
            0
        }
    }
}

/// RGBA bitmap of a shell menu item → `slint::Image` (empty if absent).
fn shell_icon(raw: &Option<(Vec<u8>, u32, u32)>) -> Image {
    match raw {
        Some((rgba, w, h)) => Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            rgba, *w, *h,
        )),
        None => Image::default(),
    }
}

/// A shell menu bitmap is "monochrome" when every OPAQUE pixel is effectively
/// grayscale (R≈G≈B). Windows system glyphs (e.g. the Win11 "Share" icon) are a
/// near-black single hue → unreadable on a dark menu; such icons are re-tinted to
/// the theme foreground via Slint `colorize`. COLOUR app icons (7-Zip, Git…) fail
/// the test and are left untouched.
fn shell_icon_monochrome(raw: &Option<(Vec<u8>, u32, u32)>) -> bool {
    let Some((rgba, _, _)) = raw else {
        return false;
    };
    let mut seen_opaque = false;
    for px in rgba.chunks_exact(4) {
        if px[3] < 24 {
            continue; // near-transparent → ignored (antialiasing edges)
        }
        seen_opaque = true;
        let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
        if r.max(g).max(b) - r.min(g).min(b) > 24 {
            return false; // a coloured pixel → not a monochrome glyph
        }
    }
    seen_opaque // only if the icon has at least one opaque pixel (not empty)
}

/// First FILE (non-folder) of the active view, from the listing ALREADY in
/// memory — no disk I/O. Serves as a probe target for the "file context".
fn first_file_in_active_panel(state: &AppState) -> Option<PathBuf> {
    let active = *state.active_panel.borrow();
    let panels = state.panels.borrow();
    let p = panels.get(active)?;
    let dir = p.tabs.tabs[p.tabs.active].current_path.clone();
    (0..p.rows_model.row_count())
        .filter_map(|i| p.rows_model.row_data(i))
        .find(|r| !r.is_dir)
        .map(|r| dir.join(r.name.as_str()))
}

/// LAZY probe of the shell menu — called when the "Detected Windows
/// entries" sub-tab opens, never at startup (the in-process COM scan
/// cost a wait cursor for a rarely-consulted list).
///
/// Probes BOTH registry contexts, which do NOT expose the same handlers
/// (`Directory\shell` vs `*\shell`, same on the COM side) — hence "file" entries
/// (Convert to PDF, Select left file…) missing from a "folder" probe:
///   - the current FOLDER;
///   - the first FILE of the active view, taken from the listing ALREADY loaded
///     (no I/O, and above all NO temporary file to create).
///
/// Only once per session; right-clicks keep enriching the list
/// (handlers specific to a file type only appear this way — see the
/// note displayed in the panel).
fn scan_shell_ext(window: &MainWindow, state: &AppState) {
    if !cfg!(windows) || state.shell_scanned.get() || !state.config.borrow().shell_ctx_menu {
        return;
    }
    state.shell_scanned.set(true);
    let dir = state.current_path();
    let mut targets: Vec<PathBuf> = vec![dir.clone()];
    if let Some(f) = first_file_in_active_panel(state) {
        targets.push(f);
    }
    let mut grew = false;
    for t in &targets {
        // The session (IContextMenu + HMENU) is discarded immediately: here we only want
        // the LABELS, not something to invoke.
        if let Some((_session, entries)) =
            crate::shellmenu::build_for_paths(std::slice::from_ref(t), &dir)
        {
            let mut known = state.shell_known.borrow_mut();
            for e in entries {
                if !known.iter().any(|k| k == &e.label) {
                    known.push(e.label);
                    grew = true;
                }
            }
        }
    }
    if grew {
        refresh_shell_ext_rows(window, state);
    }
    info!(targets = targets.len(), "shell context menu probed (lazy)");
}

/// Pushes the "detected Windows entries" settings panel (labels seen, sorted,
/// checked unless hidden by the user).
fn refresh_shell_ext_rows(window: &MainWindow, state: &AppState) {
    let disabled = state.config.borrow().shell_menu_disabled.clone();
    let mut labels = state.shell_known.borrow().clone();
    labels.sort_by_key(|l| l.to_lowercase());
    let rows: Vec<ShellExtRow> = labels
        .into_iter()
        .map(|l| ShellExtRow {
            enabled: !disabled.iter().any(|d| d == &l),
            label: l.into(),
        })
        .collect();
    window.set_shell_ext_rows(ModelRc::new(VecModel::from(rows)));
}

/// Splits the "arguments" field (a single line) into individual arguments.
///
/// Spaces separate arguments, and quotes — single or double — group a run into
/// ONE argument, the shell convention every user already knows. Without them a
/// literal containing a space, such as an archive named `Mon Archive.7z`, could
/// not be expressed at all: the quotes would reach the program verbatim and it
/// would create two mangled files.
///
/// Quotes may open mid-argument (`-o{dir}/"My Folder"`), and an unclosed quote
/// simply runs to the end of the line rather than being reported as an error —
/// the field is edited live, so it is briefly unbalanced on almost every
/// keystroke.
///
/// TAGS are substituted AFTER this split, so a path containing spaces stays one
/// argument without any quoting from the user.
fn split_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false; // distinguishes "" (an empty quoted argument) from no argument
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// Renders an `argv` for display, quoting whatever contains a space so that
/// argument boundaries stay readable. A plain join would show a single path
/// containing spaces exactly like two separate arguments.
fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.contains(char::is_whitespace) {
                format!("\"{a}\"")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Parses a list of extensions "png, jpg ; .gif" → `["png","jpg","gif"]`.
fn parse_exts(s: &str) -> Vec<String> {
    s.split([',', ';', ' '])
        .map(|x| {
            let x = x.trim();
            // `*.zip` is the shell spelling everyone reaches for, and it used to
            // be stored verbatim — matching nothing, with the entry silently
            // vanishing from the menu. A lone `*` is left alone: it IS the
            // wildcard (see `openers::CTX_EXT_ALL`).
            x.strip_prefix("*.")
                .unwrap_or(x)
                .trim_start_matches('.')
                .to_ascii_lowercase()
        })
        .filter(|x| !x.is_empty())
        .collect()
}

/// A ready-made command the user can install in one click.
struct Recipe {
    /// Tool the command drives, used to report unavailable recipe families.
    /// Never translated because it is a program name. It is deliberately not
    /// included in the visible label: the built-in icon already identifies the
    /// family without repeating "7z —" or "tar —" before every action.
    tool: &'static str,
    /// i18n key of the ACTION, e.g. "extract here". Deliberately shared between
    /// tools that do the same thing, so one translation serves all of them.
    label_key: &'static str,
    /// Program candidates, first launchable one wins — see
    /// [`actions::resolve_program`]. Entries that make no sense on the running
    /// platform are simply skipped, so one list covers Linux and Windows.
    programs: &'static [&'static str],
    args: &'static str,
    /// Menus the command is pinned to by default (see `openers::CTX_*`).
    ctx: u8,
    /// Extensions the FILE entry is restricted to; a lone [`openers::CTX_EXT_ALL`]
    /// means every file. The extract recipes narrow it to archives — offering
    /// "Extract here" on a text file is noise in the right-click menu.
    ctx_exts: &'static [&'static str],
    /// Visual family persisted with commands created from this template.
    icon: openers::OpenerIcon,
}

impl Recipe {
    /// Label shown on the row and used as the created command's name.
    fn label(&self, lang: Lang) -> String {
        i18n::tr(lang, self.label_key)
    }
}

/// 7-Zip candidates. The installer registers a shell extension but adds
/// nothing to PATH on Windows, hence the absolute paths alongside the bare
/// names used on Linux (`7zz` is the upstream build, `7z`/`7za` come p7zip).
const SEVEN_ZIP: &[&str] = &[
    "7z",
    "7zz",
    "7za",
    r"C:\Program Files\7-Zip\7z.exe",
    r"C:\Program Files (x86)\7-Zip\7z.exe",
];

/// Ready-made commands.
///
/// Archiving and sharing are the two jobs the custom-command system gets asked
/// to do most, and hand-writing the command line is precisely what these spare
/// the user. They stay editable templates: picking one opens the editor
/// prefilled instead of saving anything silently, so they double as a worked
/// example of what the tags can express.
///
/// Order matters — it is the display order: archivers first, grouped by tool,
/// then sharing.
///
/// Only Ark offers a graphical variant. 7-Zip's command line has no
/// format-chooser dialog on any platform (on Windows that dialog belongs to the
/// shell extension, which Ranger already exposes through the native context
/// menu), and `tar` has no interface at all.
const RECIPES: &[Recipe] = &[
    // ----- 7-Zip -----
    // One archive per selected item, named after it, created alongside it.
    Recipe {
        tool: "7z",
        label_key: "ow_recipe_compress_each",
        programs: SEVEN_ZIP,
        args: "a {dir}/{stem}.7z {file}",
        ctx_exts: &[openers::CTX_EXT_ALL],
        ctx: openers::CTX_FILE | openers::CTX_DIR,
        icon: openers::OpenerIcon::SevenZip,
    },
    // Everything in ONE archive named after the folder — what archivers do for
    // a multiple selection, and what `{files}` exists to make expressible.
    Recipe {
        tool: "7z",
        label_key: "ow_recipe_compress_zip",
        programs: SEVEN_ZIP,
        args: "a {dir}/{dirname}.zip {files}",
        ctx_exts: &[openers::CTX_EXT_ALL],
        ctx: openers::CTX_FILE | openers::CTX_DIR,
        icon: openers::OpenerIcon::SevenZip,
    },
    Recipe {
        tool: "7z",
        label_key: "ow_recipe_extract_here",
        programs: SEVEN_ZIP,
        args: "x -o{dir} {file}",
        ctx_exts: rfs::ARCHIVE_EXTENSIONS,
        ctx: openers::CTX_FILE,
        icon: openers::OpenerIcon::SevenZip,
    },
    // Into a subfolder named after the archive, so a messy archive does not
    // scatter its contents over the current folder. 7-Zip creates the folder.
    Recipe {
        tool: "7z",
        label_key: "ow_recipe_extract_folder",
        programs: SEVEN_ZIP,
        args: "x -o{dir}/{stem} {file}",
        ctx_exts: rfs::ARCHIVE_EXTENSIONS,
        ctx: openers::CTX_FILE,
        icon: openers::OpenerIcon::SevenZip,
    },
    // ----- tar -----
    // `-C {dir}` plus bare names: handed absolute paths, tar strips the leading
    // "/" and stores the whole tree leading to each file, so the archive held
    // `home/user/Docs/a.txt` instead of `a.txt`.
    Recipe {
        tool: "tar",
        label_key: "ow_recipe_compress_targz",
        programs: &["tar"],
        args: "-czf {dir}/{dirname}.tar.gz -C {dir} {names}",
        ctx_exts: &[openers::CTX_EXT_ALL],
        ctx: openers::CTX_FILE | openers::CTX_DIR,
        icon: openers::OpenerIcon::Archive,
    },
    // Own label key rather than the one 7z's identical-looking recipe uses:
    // this family carries a "Tar - " prefix in its wording, on top of the icon,
    // so its own args do not accidentally inherit changes made for 7z.
    // `-xf` alone detects gzip/bzip2/xz; `-C` needs the folder to exist, hence
    // the current one rather than a new subfolder.
    Recipe {
        tool: "tar",
        label_key: "ow_recipe_extract_here_tar",
        programs: &["tar"],
        args: "-xf {file} -C {dir}",
        ctx_exts: rfs::ARCHIVE_EXTENSIONS,
        ctx: openers::CTX_FILE,
        icon: openers::OpenerIcon::Archive,
    },
    // `--one-top-level` (bare) both creates the destination folder — unlike
    // `-C`, which needs it to exist — and derives its name from the archive.
    // That derivation is why this reads `{file}` rather than a manufactured
    // path: Rust's `{stem}` strips only the LAST extension, turning
    // "archive.tar.gz" into "archive.tar"; tar's own suffix stripping gets the
    // compound ".tar.gz" right. Verified against a real archive, including a
    // name with extra dots in it, not assumed from the option's description.
    Recipe {
        tool: "tar",
        label_key: "ow_recipe_extract_folder_tar",
        programs: &["tar"],
        args: "-C {dir} --one-top-level -xf {file}",
        ctx_exts: rfs::ARCHIVE_EXTENSIONS,
        ctx: openers::CTX_FILE,
        icon: openers::OpenerIcon::Archive,
    },
    // ----- Sharing -----
    // No cross-desktop standard exists on Linux: the entry Dolphin offers comes
    // from KDE's Purpose framework, a library with no command-line entry point.
    // These call the usual tools directly and read as actions, like the archive
    // recipes whose icon now carries the tool identity.
    //
    // One mail composer per item: `--attach` takes a single file, so a multiple
    // selection opens several windows — the run count shown under the preview
    // says so before anything is saved.
    Recipe {
        tool: "",
        label_key: "ow_recipe_share_email",
        programs: &["xdg-email"],
        args: "--attach {file}",
        ctx_exts: &[openers::CTX_EXT_ALL],
        ctx: openers::CTX_FILE,
        icon: openers::OpenerIcon::Email,
    },
    // The GUI handler, not `kdeconnect-cli`: it opens a picker limited to
    // paired, reachable devices, whereas the CLI demands a device id up front.
    Recipe {
        tool: "",
        label_key: "ow_recipe_share_kdeconnect",
        programs: &["kdeconnect-handler"],
        args: "{file}",
        ctx_exts: &[openers::CTX_EXT_ALL],
        ctx: openers::CTX_FILE,
        icon: openers::OpenerIcon::Device,
    },
    // Both take the files last and show a device chooser when none is given,
    // so the whole selection goes in one run.
    Recipe {
        tool: "",
        label_key: "ow_recipe_share_bluetooth",
        programs: &["bluetooth-sendto", "blueman-sendto"],
        args: "{files}",
        ctx_exts: &[openers::CTX_EXT_ALL],
        ctx: openers::CTX_FILE,
        icon: openers::OpenerIcon::Device,
    },
];

/// Publishes the recipes whose tool is actually installed. The others are left
/// out entirely rather than shown as unusable, and the block disappears when
/// none remains. Each row carries its index in [`RECIPES`], since filtering
/// makes the displayed order no longer match the table's.
///
/// Rows are dealt into two balanced COLUMNS, filled top to bottom so a tool's
/// recipes stay adjacent. The split lives here because Slint's `for` cannot
/// carry an inline slice.
///
/// Also words the tools that resolved to nothing. Naming them is what answers
/// "why is my archiver missing?", and deriving the list from the table keeps it
/// correct on both platforms — outside Windows a bare command resolves through
/// PATH, on Windows it never can, so a hard-coded list would promise tools that
/// are structurally unreachable there.
fn push_recipes_ui(window: &MainWindow, state: &AppState) {
    let lang = state.snapshot_config().language;
    let mut rows: Vec<OwRecipe> = Vec::new();
    let mut missing: Vec<&str> = Vec::new();
    for (index, r) in RECIPES.iter().enumerate() {
        if actions::resolve_program(r.programs).is_some() {
            rows.push(OwRecipe {
                label: r.label(lang).into(),
                index: index as i32,
                icon_kind: r.icon.as_i32(),
            });
        } else if !r.tool.is_empty() && !missing.contains(&r.tool) {
            missing.push(r.tool);
        }
    }
    let mid = rows.len().div_ceil(2);
    let (col1, col2) = rows.split_at(mid);
    window.set_ow_recipes_col1(ModelRc::new(VecModel::from(col1.to_vec())));
    window.set_ow_recipes_col2(ModelRc::new(VecModel::from(col2.to_vec())));
    let hint = if missing.is_empty() {
        String::new()
    } else {
        i18n::tr(lang, "ow_recipes_missing").replace("{tools}", &missing.join(", "))
    };
    window.set_ow_recipes_missing(hint.into());
}

/// Recomputes the command's live preview + the program's validity (popup).
fn recompute_ow_preview(window: &MainWindow, state: &AppState) {
    let program = window.get_ow_popup_program().to_string();
    let args = split_args(&window.get_ow_popup_args());
    // An OS association app (Windows UWP/Store, Linux `.desktop`) has no exe
    // to validate; otherwise the program must exist (path) OR be a PATH
    // command on Linux — see `program_is_valid`.
    window
        .set_ow_popup_valid(window.get_ow_popup_is_store() || actions::program_is_valid(&program));
    let selection = selected_paths(state);
    // Sample used to resolve the tags. A bare "example.txt" would leave `{dir}`
    // empty, so an argument like `{dir}/out.7z` would render as "/out.7z" and
    // suggest the file lands at the filesystem root. The stand-in is therefore
    // built inside the current folder, which is what the command will really see.
    let sample = selection.first().cloned().unwrap_or_else(|| {
        let dir = state.current_path();
        if dir.as_os_str().is_empty() {
            PathBuf::from("example.txt")
        } else {
            dir.join("example.txt")
        }
    });
    let temp = openers::Opener {
        id: String::new(),
        label: String::new(),
        program: program.clone(),
        assoc: None,
        icon: openers::OpenerIcon::None,
        args,
        default_exts: Vec::new(),
        used_exts: Vec::new(),
        use_count: 0,
        last_used: 0,
        elevated: false, // has no effect on the preview (renders the argv only)
        ctx_menu: 0,
        ctx_exts: Vec::new(),
    };
    let ctx = openers::TagContext::from_path(&sample);
    // `{files}` runs once over the whole selection, so the preview must show
    // the expanded list rather than a single file — otherwise it would suggest
    // the wrong mode entirely.
    let argv = if temp.expands_list() {
        let refs: Vec<&Path> = if selection.is_empty() {
            vec![sample.as_path()]
        } else {
            selection.iter().map(PathBuf::as_path).collect()
        };
        temp.render_for_batch(&ctx, &refs)
    } else {
        temp.render_for_file(&ctx)
    };
    let prog_name = Path::new(&program)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.clone());
    window.set_ow_popup_preview(format!("{prog_name} {}", display_argv(&argv)).into());
    window.set_ow_popup_runs(run_count_text(
        state.snapshot_config().language,
        temp.has_tag() && !temp.expands_list(),
        selection.len(),
    ));
}

/// Sentence telling how many processes the command will start.
///
/// The count is not a detail: an argument template that mentions a tag runs
/// ONCE PER SELECTED FILE, otherwise a single process receives them all. That
/// rule was previously invisible — the preview always rendered one file, so a
/// command about to start twenty processes looked exactly like one starting a
/// single process.
fn run_count_text(lang: Lang, per_file: bool, selected: usize) -> SharedString {
    if !per_file {
        return i18n::tr(lang, "ow_runs_once").into();
    }
    match selected {
        // Nothing selected (typically from Settings): state the rule instead of
        // a count that would only be true right now.
        0 => i18n::tr(lang, "ow_runs_per_file"),
        1 => i18n::tr(lang, "ow_runs_once"),
        n => i18n::tr(lang, "ow_runs_n_times").replace("{n}", &n.to_string()),
    }
    .into()
}

/// Replaces an opener's list of "Ranger default" extensions.
fn apply_default_exts(state: &AppState, id: &str, exts: &[String]) {
    let mut s = state.openers.borrow_mut();
    if let Some(o) = s.openers.iter_mut().find(|o| o.id == id) {
        o.default_exts.clear();
    }
    for e in exts {
        s.set_default_ext(id, e, true);
    }
}

/// Creates a Windows `<name>.lnk` shortcut in `cur` pointing to `target`.
/// Empty `name` → derived from the target's name. Adds the `.lnk` extension if
/// missing; refuses an invalid name or an already-existing target/`.lnk`. Returns the
/// created file name (for the cursor), or `None` on failure.
fn create_shortcut_entry(cur: &Path, name: &str, target: &str) -> Option<String> {
    if target.is_empty() {
        return None;
    }
    let target_path = PathBuf::from(target);
    // Entered name, otherwise the target's name (without extension).
    let base = if name.is_empty() {
        target_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    } else {
        name.to_string()
    };
    if base.is_empty() || !ops::is_valid_entry_name(&base) {
        error!(name = base, "invalid shortcut name");
        return None;
    }
    let file_name = if base.to_ascii_lowercase().ends_with(".lnk") {
        base
    } else {
        format!("{base}.lnk")
    };
    let lnk_path = cur.join(&file_name);
    if lnk_path.exists() {
        error!(path = %lnk_path.display(), "shortcut already exists");
        return None;
    }
    match openwith::create_shortcut(&lnk_path, &target_path) {
        Ok(()) => {
            info!(lnk = %lnk_path.display(), target, "created shortcut");
            Some(file_name)
        }
        Err(err) => {
            error!(error = %err, "create shortcut failed");
            None
        }
    }
}

/// "Create shortcut" (file context menu): creates in `cur` a
/// link named `name` to `target`. Symlink tab → `ops::link_as` (Unix symlink
/// / Windows junction|hardlink, like "Link here"); Shortcut tab → `.lnk`
/// (reuses `create_shortcut_entry`). Returns the created name (for the cursor).
fn create_link_entry(window: &MainWindow, cur: &Path, name: &str, target: &str) -> Option<String> {
    if name.is_empty() || target.is_empty() {
        return None;
    }
    // Symlink tab checked → direct link with the exact name; otherwise .lnk shortcut.
    if window.get_create_link_symlink() {
        if !ops::is_valid_entry_name(name) {
            error!(name, "invalid link name");
            return None;
        }
        let dst = cur.join(name);
        match ops::link_as(&PathBuf::from(target), &dst) {
            Ok(()) => {
                info!(link = %dst.display(), target, "created symlink");
                Some(name.to_string())
            }
            Err(err) => {
                error!(error = %err, "create symlink failed");
                None
            }
        }
    } else {
        create_shortcut_entry(cur, name, target)
    }
}

/// Opens `dir` in a NEW tab of the active view (same mechanics as
/// `on_action_open_new_tab`). Used for folder `.lnk` shortcuts.
// Only used by the `.lnk` path (see `open_shortcut_as_tab`, Windows).
#[cfg(windows)]
fn open_dir_in_new_tab(window: &MainWindow, state: &AppState, dir: &Path) {
    let opened = state.with_tabs_mut(|book| {
        let a = book.open_after_active(dir.to_path_buf());
        book.tabs[a].current_path.clone()
    });
    load_directory(window, state, &opened, false);
}

/// If `path` is a Windows `.lnk` shortcut pointing to a FOLDER, opens it
/// in a new tab of the active view and returns `true` (the caller stops
/// there); otherwise `false` (default opening). A `.lnk` to a FILE/app
/// falls back to normal opening (the shell launches the target). Always `false`
/// outside Windows: symbolic links there are followed natively by the listing
/// (a folder symlink is navigated like a folder).
fn open_shortcut_as_tab(window: &MainWindow, state: &AppState, path: &Path) -> bool {
    #[cfg(windows)]
    {
        let is_lnk = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false);
        if is_lnk
            && let Some(target) = crate::openwith::resolve_shortcut(path)
            && target.is_dir()
        {
            info!(lnk = %path.display(), target = %target.display(), "open .lnk folder in new tab");
            open_dir_in_new_tab(window, state, &target);
            return true;
        }
        false
    }
    #[cfg(not(windows))]
    {
        let _ = (window, state, path);
        false
    }
}

/// Whether `p` should be treated as a FOLDER in the context menu: a real
/// directory, a folder symlink/junction (already followed by `Path::is_dir`), or
/// a Windows `.lnk` whose target is a directory. The `.lnk` case is a COM
/// resolution, so the cheap extension test gates it and it runs only for the
/// single right-clicked item — never in a hot loop.
fn acts_as_dir(p: &Path) -> bool {
    if p.is_dir() {
        return true;
    }
    #[cfg(windows)]
    {
        if p.extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            return crate::openwith::resolve_shortcut(p)
                .map(|t| t.is_dir())
                .unwrap_or(false);
        }
    }
    false
}

/// Opens FILE(s) (never a folder): RANGER default by extension (opener
/// `default_for` of the 1st file) if set, otherwise the OS default. Shared by the
/// double-click AND Enter/"Open" menu → identical behavior.
fn open_file_default(state: &AppState, paths: &[PathBuf]) {
    let Some(first) = paths.first() else { return };
    let ext = first
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let opener = state.openers.borrow().default_for(&ext).cloned();
    if let Some(op) = opener {
        if let Err(err) = actions::run_opener(&op, paths) {
            error!(error = %err, "run default opener failed");
        } else {
            state.openers.borrow_mut().record_use(&op.id, Some(&ext));
            save_openers(state);
        }
    } else {
        #[cfg(windows)]
        if should_try_image_gallery(&ext) {
            match actions::try_open_image_gallery(first, &ext) {
                Ok(true) => return,
                Ok(false) => {}
                Err(err) => {
                    // Unreadable association, incompatible URI, or Photos protocol
                    // unavailable: the standard OS opening below still
                    // preserves access to the file, possibly without the gallery.
                    debug!(error = %err, path = %first.display(), "Photos gallery activation unavailable");
                }
            }
        }
        if let Err(err) = actions::open_path(first) {
            error!(error = %err, path = %first.display(), "open file failed");
        }
    }
}

/// The Photos gallery attempt is strictly a Windows enhancement for
/// images opened via the OS association. Ranger openers are processed before
/// this predicate, so Linux keeps its historical `xdg-open` path.
#[cfg(windows)]
fn should_try_image_gallery(extension: &str) -> bool {
    rfs::classify_kind(Some(extension), false) == FileKind::Image
}

/// Opens the "Custom command" popup in CREATE mode (optional
/// pre-filling of program/name). Shared by "Custom command", "Use as
/// application", and the promotion toast.
fn open_ow_create(window: &MainWindow, state: &AppState, program: &str, name: &str) {
    window.set_ow_popup_id(SharedString::new());
    window.set_ow_popup_name(name.into());
    window.set_ow_popup_program(program.into());
    window.set_ow_popup_icon_kind(openers::OpenerIcon::None.as_i32());
    window.set_ow_popup_is_store(false); // creation = command with an executable
    window.set_ow_popup_args(SharedString::new());
    window.set_ow_popup_default_ext(SharedString::new());
    window.set_ow_popup_used_ext(SharedString::new());
    window.set_ow_popup_elevated(false); // unchecked by default
    window.set_ow_popup_ctx_file(false); // not pinned by default
    // Every file, so ticking "Files" changes nothing about who sees the entry
    // until the user narrows it down deliberately.
    window.set_ow_popup_ctx_ext(openers::CTX_EXT_ALL.into());
    window.set_ow_popup_ctx_dir(false);
    window.set_ow_popup_ctx_bg(false);
    window.set_ow_popup_add(true);
    recompute_ow_preview(window, state);
    window.set_ow_popup_open(true);
    window.set_ow_popup_focus_armed(true);
}

/// Pushes the flattened tree + the container dropdown to the GUI.
fn push_favorites_ui(window: &MainWindow, state: &AppState) {
    let fav = state.favorites.borrow();
    let rows: Vec<FavNode> = fav.flatten().iter().map(flat_to_favnode).collect();
    window.set_fav_nodes(ModelRc::new(VecModel::from(rows)));
    // Toggles "Collapse all" (if ≥1 container is expanded) / "Expand all".
    window.set_fav_can_collapse(fav.any_expanded());

    // Popup dropdown: "Root" + all indented containers.
    let s = i18n::strings_for(state.config.borrow().language);
    let mut labels: Vec<SharedString> = vec![s.fav_popup_root.clone()];
    let mut ids: Vec<String> = vec![String::new()];
    for (id, name, depth) in fav.containers() {
        let indent = "   ".repeat(depth.max(0) as usize);
        labels.push(format!("{indent}{name}").into());
        ids.push(id);
    }
    window.set_fav_container_labels(ModelRc::new(VecModel::from(labels)));
    *state.fav_container_ids.borrow_mut() = ids;
}

/// Resolves the target FOLDER of a favorite `p`: the folder itself, or the parent
/// of a file. `None` + "not found" toast if absent/inaccessible.
fn resolve_fav_dir(window: &MainWindow, p: &Path, lang: Lang) -> Option<PathBuf> {
    let target = if p.is_dir() {
        p.to_path_buf()
    } else if p.is_file() {
        p.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| p.to_path_buf())
    } else {
        notice(
            window,
            i18n::strings_for(lang).fav_toast_missing.clone(),
            NoticeKind::FavMissing,
        );
        return None;
    };
    if !target.is_dir() {
        notice(
            window,
            i18n::strings_for(lang).fav_toast_missing.clone(),
            NoticeKind::FavMissing,
        );
        return None;
    }
    Some(target)
}

/// Opens a favorite path in a NEW tab of the active view: folder →
/// tab on the folder; file → tab on its parent (we locate it, we
/// don't execute it). Path not found → "notice" toast.
fn fav_open_path_new_tab(window: &MainWindow, state: &AppState, p: PathBuf) {
    let lang = state.config.borrow().language;
    let Some(target) = resolve_fav_dir(window, &p, lang) else {
        return;
    };
    let opened = state.with_tabs_mut(|book| {
        let a = book.open_after_active(target);
        book.tabs[a].current_path.clone()
    });
    load_directory(window, state, &opened, false);
}

/// Opens a favorite IN the active tab (overwrites the current view) — LEFT
/// click in the sidebar (MIDDLE click keeps opening in a new tab).
fn fav_open_here(window: &MainWindow, state: &AppState, id: &str) {
    let lang = state.config.borrow().language;
    let Some(p) = state.favorites.borrow().path_of(id).map(PathBuf::from) else {
        return;
    };
    if let Some(target) = resolve_fav_dir(window, &p, lang) {
        load_directory(window, state, &target, true);
    }
}

/// Opens a favorite by its `id` in a NEW tab (no-op if container / unknown).
fn fav_open(window: &MainWindow, state: &AppState, id: &str) {
    let path = state.favorites.borrow().path_of(id);
    if let Some(p) = path {
        fav_open_path_new_tab(window, state, PathBuf::from(p));
    }
}

/// Default alias of a path = its last component (otherwise the path).
fn default_alias(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Saves `paths` into the `container` favorites folder: deduplication
/// (already-present path ignored) + default alias, then saves + refreshes
/// the UI + appropriate toast (added / already present). SHARED core for drag
/// drops onto a favorites folder — intra/inter-instance tab and
/// a view's file selection — and aligned with `on_fav_save_commit`.
fn add_paths_to_favorite(
    window: &MainWindow,
    state: &AppState,
    paths: &[PathBuf],
    container: &str,
) {
    // EMPTY `container` = root (""): drop on the "no favorites" zone / favorites
    // dead zone. The model handles "" as the root container; we only
    // reject the absence of paths now.
    if paths.is_empty() {
        return;
    }
    let mut added = 0usize;
    {
        let mut fav = state.favorites.borrow_mut();
        for p in paths {
            let path_str = p.display().to_string();
            if fav.container_has_path(container, &path_str) {
                continue; // already in this favorites folder → ignored
            }
            if fav
                .add_favorite(container, &default_alias(p), &path_str)
                .is_some()
            {
                added += 1;
            }
        }
    }
    let lang = state.config.borrow().language;
    if added > 0 {
        save_favorites(state);
        push_favorites_ui(window, state);
        notice(
            window,
            i18n::strings_for(lang).fav_toast_added.clone(),
            NoticeKind::FavAdded,
        );
    } else {
        // Nothing added = everything already existed (the container is guaranteed valid on
        // hover) → neutral info, not an error.
        notice(
            window,
            i18n::strings_for(lang).fav_toast_exists.clone(),
            NoticeKind::FavExists,
        );
    }
}

/// Opens the "Save as favorite" popup for a set of paths.
fn open_fav_save_popup(window: &MainWindow, state: &AppState, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let multi = paths.len() > 1;
    let target = if multi {
        i18n::footer_items_text(state.snapshot_config().language, paths.len())
    } else {
        paths[0].display().to_string()
    };
    let alias = if multi {
        String::new()
    } else {
        default_alias(&paths[0])
    };
    *state.fav_save_pending.borrow_mut() = paths;
    push_favorites_ui(window, state); // refreshes the container dropdown
    window.set_fav_save_multi(multi);
    window.set_fav_save_target(target.into());
    window.set_fav_save_alias(alias.into());
    window.set_fav_container_index(0);
    window.set_fav_save_open(true);
    window.set_fav_save_focus_armed(true);
}

/// Height of a favorites tree row (30px) + spacing (1px) = the vertical
/// pitch, MUST match the rendering (FavPanel: height 30 + spacing 1).
const FAV_ROW_PITCH: f32 = 31.0;

/// From the drag geometry (cursor y, source row top y, source
/// index), computes `(target index, zone)` — zone 0 = before · 1 = inside · 2 = after.
fn fav_drag_target(cur_y: f32, row_top: f32, row_idx: i32, count: i32) -> (i32, i32) {
    if count <= 0 {
        return (0, 1);
    }
    let list_origin = row_top - row_idx as f32 * FAV_ROW_PITCH;
    let hovered = (cur_y - list_origin) / FAV_ROW_PITCH;
    let ti = (hovered.floor() as i32).clamp(0, count - 1);
    let frac = hovered - hovered.floor();
    let zone = if frac < 0.33 {
        0
    } else if frac > 0.66 {
        2
    } else {
        1
    };
    (ti, zone)
}

/// Applies a node move based on the computed target/zone. Returns
/// `true` if the tree changed.
fn fav_perform_move(state: &AppState, src_id: &str, target_index: i32, zone: i32) -> bool {
    let mut fav = state.favorites.borrow_mut();
    let (target_id, target_is_container) = {
        let flat = fav.flatten();
        match flat.get(target_index.max(0) as usize) {
            Some(t) => (t.id.clone(), t.is_container),
            None => return false,
        }
    };
    if target_id == src_id {
        return false;
    }
    let target_parent = fav
        .nodes
        .iter()
        .find(|n| n.id == target_id)
        .map(|n| n.parent.clone())
        .unwrap_or_default();
    if zone == 1 && target_is_container {
        // Drop INSIDE the container (at the end) + expand it to see the result.
        let ok = fav.move_node(src_id, &target_id, None);
        if ok {
            fav.set_expanded(&target_id, true);
        }
        ok
    } else if zone == 0 {
        // Insert BEFORE the target (same parent).
        fav.move_node(src_id, &target_parent, Some(&target_id))
    } else {
        // Insert AFTER the target = before its next sibling (or at the end).
        let siblings: Vec<String> = fav
            .nodes
            .iter()
            .filter(|n| n.parent == target_parent)
            .map(|n| n.id.clone())
            .collect();
        let before = siblings
            .iter()
            .position(|id| *id == target_id)
            .and_then(|p| siblings.get(p + 1))
            .cloned();
        fav.move_node(src_id, &target_parent, before.as_deref())
    }
}

/// ASYNCHRONOUS initial population of the panels. The window is displayed
/// immediately (empty panels); ONE background thread lists the current folder
/// of each panel (the ACTIVE one first — perceived priority) and delivers the
/// results to the UI thread via a channel + `invoke_from_event_loop` (the drain
/// callback has captured the non-Send `AppState`). Each delivery is applied
/// only if it's still FRESH (`Panel.pending_initial`, turned off by any
/// synchronous listing that occurred in the meantime: navigation, F5, watcher…).
fn initial_populate_async(window: &MainWindow, state: &AppState) {
    // Jobs frozen on the UI thread (path + view settings of the active tab).
    let active = *state.active_panel.borrow();
    let mut jobs: Vec<(usize, PathBuf, bool, SortState, GroupMode)> = Vec::new();
    {
        let mut panels = state.panels.borrow_mut();
        for (i, p) in panels.iter_mut().enumerate() {
            p.pending_initial = true;
            let t = &p.tabs.tabs[p.tabs.active];
            jobs.push((
                i,
                t.current_path.clone(),
                t.show_hidden,
                t.sort,
                t.group_mode,
            ));
        }
    }
    if active < jobs.len() {
        let a = jobs.remove(active);
        jobs.insert(0, a);
    }
    // Empty panels pushed right away → the display no longer waits on the network.
    update_panels_ui(window, state);

    // Delivery: the worker PUSHES (idx, path, sorted result, denied?) then
    // wakes the UI thread. The (pure) sort is done on the worker; the conversion
    // to rows (`entry_to_row`: Slint images + icon cache) stays on the UI side.
    type Delivery = (
        usize,
        PathBuf,
        std::result::Result<(Vec<Entry>, usize), bool>,
    );
    let (tx, rx) = mpsc::channel::<Delivery>();
    {
        let st = state.clone();
        let weak = window.as_weak();
        let rx = Rc::new(RefCell::new(rx));
        window.on_initial_listing_drain(move || {
            let Some(w) = weak.upgrade() else { return };
            while let Ok((idx, path, res)) = rx.borrow().try_recv() {
                apply_initial_listing(&w, &st, idx, &path, res);
            }
        });
    }
    let weak = window.as_weak();
    std::thread::spawn(move || {
        for (idx, path, show_hidden, sort, group) in jobs {
            let res = match rfs::list_dir_counted(&path, show_hidden) {
                Ok((mut entries, hidden)) => {
                    rfs::sort(&mut entries, sort.column, sort.order, group);
                    Ok((entries, hidden))
                }
                Err(err) => {
                    error!(error = %err, path = %path.display(), "initial list_dir failed");
                    // Same distinction as refresh_listing: access denied → toast;
                    // otherwise → "unavailable" tab (banner + re-check).
                    Err(matches!(
                        &err,
                        ranger_core::Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied
                    ))
                }
            };
            if tx.send((idx, path, res)).is_err() {
                return; // receiver gone (window closed)
            }
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_initial_listing_drain();
                }
            });
        }
    });
}

/// Applies (UI thread) an initial listing delivered by the startup thread —
/// ONLY if the panel is still waiting for it (`pending_initial`) and its
/// active tab still points to the listed path.
fn apply_initial_listing(
    window: &MainWindow,
    state: &AppState,
    idx: usize,
    path: &Path,
    res: std::result::Result<(Vec<Entry>, usize), bool>,
) {
    let (lang, compact_icon_rows) = {
        let config = state.config.borrow();
        (config.language, config.compact_icon_rows_in_preview)
    };
    let is_active = idx == *state.active_panel.borrow();
    {
        let mut panels = state.panels.borrow_mut();
        let Some(p) = panels.get_mut(idx) else { return };
        let tab = &p.tabs.tabs[p.tabs.active];
        if !p.pending_initial || tab.current_path != path {
            return; // stale delivery (the user has already navigated / listed)
        }
        let big_icon = tab.preview;
        let zoom = tab.zoom;
        p.pending_initial = false;
        match res {
            Ok((entries, hidden_count)) => {
                let parent_display = path.display().to_string();
                let now = now_unix();
                let rows = entries_to_rows(
                    &entries,
                    &parent_display,
                    lang,
                    now,
                    big_icon,
                    zoom,
                    compact_icon_rows,
                );
                p.replace_rows(rows);
                p.hidden_count = hidden_count;
                p.unavailable = false;
            }
            Err(denied) => {
                p.replace_rows(Vec::new());
                p.hidden_count = 0;
                p.unavailable = !denied;
                if denied && is_active {
                    show_notice(window, i18n::access_denied(lang));
                }
            }
        }
        p.displayed_path = path.to_path_buf();
    }
    update_panels_ui(window, state);
    // Any delivery may belong to a simultaneously visible view: the
    // global preview request must therefore be rebuilt even if this panel
    // isn't active (its listing may have arrived after the active panel's).
    request_thumbnails(state);
    // ACTIVE panel: watcher + other background work tied to navigation.
    if is_active {
        install_watcher(state, window, path);
        request_folder_stats(state);
        request_imgmeta(state);
    }
}

/// Refreshes each panel after a potentially multi-view operation.
/// Local folders are re-read directly; any network path, even in
/// a non-active split, goes through the async worker. The active panel keeps the
/// full route (`refresh_listing`) for its filter and its watcher.
fn refresh_all_panels(window: &MainWindow, state: &AppState) {
    let active_idx = *state.active_panel.borrow();
    let paths: Vec<PathBuf> = state
        .panels
        .borrow()
        .iter()
        .map(|panel| panel.tabs.tabs[panel.tabs.active].current_path.clone())
        .collect();

    // First re-read the local panels without publishing the UI between each one.
    for (index, path) in paths.iter().enumerate() {
        if index != active_idx
            && !ranger_core::places::is_network_path(path)
            && !rfs::is_unc_path(path)
        {
            relist_panel(state, index);
        }
    }

    // The network panels then start independently. The function publishes
    // their "in progress" state but performs no I/O on the UI thread.
    for (index, path) in paths.iter().enumerate() {
        if index != active_idx
            && (ranger_core::places::is_network_path(path) || rfs::is_unc_path(path))
        {
            request_async_panel_listing(window, state, index, path, String::new());
        }
    }

    let active = paths.get(active_idx).cloned().unwrap_or_default();
    if active.as_os_str().is_empty() {
        update_panels_ui(window, state);
        request_thumbnails(state);
        request_folder_stats(state);
        request_imgmeta(state);
    } else {
        let active_is_network =
            ranger_core::places::is_network_path(&active) || rfs::is_unc_path(&active);
        refresh_listing(window, state, &active);
        if active_is_network {
            // The active listing will arrive later; the local panels already
            // re-read shouldn't wait on this network share for their visual work.
            request_thumbnails(state);
            request_folder_stats(state);
            request_imgmeta(state);
        }
    }
}

/// Re-lists ONE panel (its active tab) — used when an "unavailable"
/// folder becomes accessible again. Does NOT touch the watcher or the
/// thumbnails (the caller handles that for the active panel). Leaves the panel
/// unavailable if the listing still fails.
fn relist_panel(state: &AppState, i: usize) {
    let (lang, compact_icon_rows) = {
        let config = state.config.borrow();
        (config.language, config.compact_icon_rows_in_preview)
    };
    let mut panels = state.panels.borrow_mut();
    let Some(panel) = panels.get_mut(i) else {
        return;
    };
    panel.pending_initial = false; // fresher than the initial population
    panel.pending_listing = false;
    panel.pending_select = None;
    let path = panel.tabs.tabs[panel.tabs.active].current_path.clone();
    let sort = panel.tabs.tabs[panel.tabs.active].sort;
    let show_hidden = panel.tabs.tabs[panel.tabs.active].show_hidden;
    let group_mode = panel.tabs.tabs[panel.tabs.active].group_mode;
    let big_icon = panel.tabs.tabs[panel.tabs.active].preview;
    let zoom = panel.tabs.tabs[panel.tabs.active].zoom;
    let ext_on = panel.tabs.tabs[panel.tabs.active].ext_filter_on;
    let ext_txt = panel.tabs.tabs[panel.tabs.active].ext_filter.clone();
    let same_dir = panel.displayed_path == path;
    if !same_dir {
        panel.reset_rows_viewport();
    }
    let selected: std::collections::HashSet<String> = if same_dir {
        (0..panel.rows_model.row_count())
            .filter_map(|index| panel.rows_model.row_data(index))
            .filter(|row| row.selected)
            .map(|row| row.name.to_string())
            .collect()
    } else {
        std::collections::HashSet::new()
    };
    let anchor_name = if same_dir {
        let anchor = panel.tabs.tabs[panel.tabs.active].selection_anchor;
        usize::try_from(anchor)
            .ok()
            .and_then(|index| panel.rows_model.row_data(index))
            .map(|row| row.name.to_string())
    } else {
        None
    };
    match rfs::list_dir_counted(&path, show_hidden) {
        Ok((mut entries, hidden_count)) => {
            rfs::sort(&mut entries, sort.column, sort.order, group_mode);
            apply_ext_filter(&mut entries, ext_on, &ext_txt);
            let parent_display = path.display().to_string();
            let now = now_unix();
            let mut rows = entries_to_rows(
                &entries,
                &parent_display,
                lang,
                now,
                big_icon,
                zoom,
                compact_icon_rows,
            );
            for row in &mut rows {
                row.selected = selected.contains(row.name.as_str());
            }
            {
                let clipboard = state.clipboard.borrow();
                if matches!(clipboard.op, Some(ClipOp::Cut)) {
                    let cut_names: std::collections::HashSet<&str> = clipboard
                        .paths
                        .iter()
                        .filter(|candidate| candidate.parent() == Some(path.as_path()))
                        .filter_map(|candidate| {
                            candidate.file_name().and_then(|name| name.to_str())
                        })
                        .collect();
                    for row in &mut rows {
                        row.cut = cut_names.contains(row.name.as_str());
                    }
                }
            }
            let anchor = anchor_name
                .as_deref()
                .and_then(|name| rows.iter().position(|row| row.name.as_str() == name))
                .map(|index| index as i32)
                .unwrap_or(-1);
            panel.replace_rows(rows);
            let active = panel.tabs.active;
            panel.tabs.tabs[active].selection_anchor = anchor;
            panel.tabs.tabs[active].cursor = anchor;
            panel.hidden_count = hidden_count;
            panel.unavailable = false;
            panel.displayed_path = path;
        }
        Err(err) => {
            let denied = matches!(
                &err,
                ranger_core::Error::Io(io)
                    if io.kind() == std::io::ErrorKind::PermissionDenied
            );
            error!(error = %err, path = %path.display(), "panel relist failed");
            panel.replace_rows(Vec::new());
            let active = panel.tabs.active;
            panel.tabs.tabs[active].selection_anchor = -1;
            panel.tabs.tabs[active].cursor = -1;
            panel.hidden_count = 0;
            panel.unavailable = !denied;
            panel.displayed_path = path;
        }
    }
}

/// Re-checks "unavailable" panels (network/missing folder): those whose
/// path has BECOME accessible again are re-listed and the banner disappears.
/// Called from the Slint poll, gated by a drive change (non-blocking:
/// `is_dir` is only tested if a mount changed, see `on_recheck_unavailable`).
fn recheck_unavailable_panels(window: &MainWindow, state: &AppState) {
    let recovered: Vec<usize> = {
        let panels = state.panels.borrow();
        panels
            .iter()
            .enumerate()
            .filter(|(_, p)| p.unavailable)
            .filter(|(_, p)| p.tabs.tabs[p.tabs.active].current_path.is_dir())
            .map(|(i, _)| i)
            .collect()
    };
    if recovered.is_empty() {
        return;
    }
    for &i in &recovered {
        relist_panel(state, i);
    }
    update_panels_ui(window, state);
    // A recovered panel stays visible even if it doesn't have focus.
    request_thumbnails(state);
    // The active panel has recovered → re-arms its watcher and its other work.
    let active = *state.active_panel.borrow();
    if recovered.contains(&active) {
        let path = state.with_tabs(|book| book.tabs[book.active].current_path.clone());
        install_watcher(state, window, &path);
        request_folder_stats(state);
        request_imgmeta(state);
    }
}

/// Called when the active panel changes: refreshes the view on the new
/// active panel's current path (the model rebinding is implicit,
/// via update_panels_ui which pushes the updated PanelViews).
fn switch_active_panel(window: &MainWindow, state: &AppState) {
    let path = state.with_tabs(|book| book.tabs[book.active].current_path.clone());
    refresh_listing(window, state, &path);
}

#[derive(Clone, Copy)]
enum NavAction {
    Back,
    Forward,
    Parent,
    Home,
    Refresh,
}

fn install_nav_callback(window: &MainWindow, state: &AppState, action: NavAction) {
    let st = state.clone();
    let weak = window.as_weak();
    let cb = move || {
        let Some(w) = weak.upgrade() else { return };
        match action {
            NavAction::Back => {
                // Folder we're LEAVING: if it's a direct child of the target, we
                // re-select it there ("where we came from", like Explorer).
                let left = st.current_path();
                let target = st.with_tabs_mut(|book| {
                    let a = book.active;
                    book.tabs[a].history.back()
                });
                if let Some(p) = target {
                    load_directory(&w, &st, &p, false);
                    select_child_from(&w, &st, &p, &left);
                }
            }
            NavAction::Forward => {
                let target = st.with_tabs_mut(|book| {
                    let a = book.active;
                    book.tabs[a].history.forward()
                });
                if let Some(p) = target {
                    load_directory(&w, &st, &p, false);
                }
            }
            NavAction::Parent => {
                let cur = st.current_path();
                // `Path::parent()`, or server root for a share root
                // `\\HOST\share`, for which `Path::parent()` returns `None`.
                let target = cur
                    .parent()
                    .map(Path::to_path_buf)
                    .or_else(|| rfs::unc_share_parent(&cur));
                if let Some(target) = target {
                    load_directory(&w, &st, &target, true);
                    // We select there the folder we just left.
                    select_child_from(&w, &st, &target, &cur);
                }
            }
            NavAction::Home => {
                let home = home_dir();
                load_directory(&w, &st, &home, true);
            }
            NavAction::Refresh => {
                // F5 = force a refresh: clears the recursive mtime + size caches
                // → recompute. Navigation, on the other hand, keeps them (no flicker).
                if !drain_thumbnail_invalidations(&st) {
                    // A manual F5 has no watcher paths to target. Treat it as
                    // an explicit request to refresh content textures in the
                    // active view while leaving every other cached folder hot.
                    let paths = active_thumbnail_paths(&st);
                    invalidate_thumbnail_paths(&st, &paths);
                }
                st.rmtime_cache.borrow_mut().clear();
                st.size_cache.borrow_mut().clear();
                let cur = st.current_path();
                if !cur.as_os_str().is_empty() {
                    refresh_listing(&w, &st, &cur);
                }
            }
        }
    };
    match action {
        NavAction::Back => window.on_go_back(cb),
        NavAction::Forward => window.on_go_forward(cb),
        NavAction::Parent => window.on_go_parent(cb),
        NavAction::Home => window.on_go_home(cb),
        NavAction::Refresh => window.on_refresh(cb),
    }
}

// ---------- Navigation and listing ----------

fn load_directory(window: &MainWindow, state: &AppState, target: &Path, push_history: bool) {
    // Any navigation resets the active view's "type-ahead" filter.
    state.filter.borrow_mut().clear();
    window.set_active_filter(SharedString::new());
    // The exact Flickable isn't recreated on every listing: we therefore explicitly
    // request scrolling back to top for any new navigation context,
    // including two distinct tabs pointing to the same folder.
    let active_panel = *state.active_panel.borrow();
    if let Some(panel) = state.panels.borrow().get(active_panel) {
        panel.reset_rows_viewport();
    }
    if push_history {
        state.with_tabs_mut(|book| {
            let a = book.active;
            book.tabs[a].history.push(target.to_path_buf());
        });
    }
    refresh_listing(window, state, target);
}

/// Moves the `from` tab of panel `src` to panel `target` (at
/// position `insert_at`). If `src` becomes empty, it is **closed** (panel removed
/// and its area reclaimed by its sibling). Returns the new panel index to
/// activate, or `None` if the operation is invalid. `src` ≠ `target` required.
/// Tab tear-off (browser-style tear-off): opens a NEW
/// Ranger instance on the folder of panel `src`'s `from` tab, then
/// removes that tab from here. Refuses if it's the sole tab of the sole panel
/// (otherwise the window would end up empty for a simple duplicate — Firefox
/// behavior). The new instance is launched BEFORE the removal: on failure,
/// the tab isn't lost. If the source panel becomes empty, it is closed (like
/// a cross-view move). Returns `true` if the tab was torn off.
fn tear_off_tab(state: &AppState, src: usize, from: usize, at: Option<(i32, i32)>) -> bool {
    let (path, mode) = {
        let panels = state.panels.borrow();
        if src >= panels.len() || from >= panels[src].tabs.tabs.len() {
            return false;
        }
        // Last tab of the only panel → don't empty the window.
        if panels.len() == 1 && panels[src].tabs.tabs.len() == 1 {
            return false;
        }
        // The new instance inherits the tab bar position of the
        // source view — the chrome follows the torn-off tab.
        (
            panels[src].tabs.tabs[from].current_path.clone(),
            panels[src].tab_bar_mode,
        )
    };
    if actions::spawn_new_instance(&path, at, mode).is_err() {
        return false;
    }
    // The removal and pruning shared with the cross-instance transfer also
    // handle re-indexing the active panel. The guard above forbids an
    // empty instance.
    let _ = remove_tab_pruning(state, src, from);
    state.persist_workspace();
    true
}

// Cross-INSTANCE tab transfer (Windows) ----------

static SUPPRESS_WORKSPACE_PERSIST: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// True if the workspace persistence on close must be SKIPPED: instance
/// emptied by transferring ALL its tabs to another instance → otherwise we'd
/// overwrite the shared workspace with an empty state.
pub fn suppress_workspace_persist() -> bool {
    SUPPRESS_WORKSPACE_PERSIST.load(std::sync::atomic::Ordering::SeqCst)
}

/// Serializes a tab for cross-instance transfer: path + view state +
/// drop point (screen), separated by `\t` (invalid in a Windows
/// file name → unambiguous).
fn serialize_tab(tab: &Tab, drop: (i32, i32)) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        tab.current_path.display(),
        tab.sort.column.code(),
        u8::from(matches!(tab.sort.order, SortOrder::Asc)),
        u8::from(tab.preview),
        u8::from(tab.show_hidden),
        tab.group_mode.code(),
        drop.0,
        drop.1,
    )
}

/// Rebuilds a tab from a serialized payload, with the (physical) SCREEN
/// drop point if present. `None` if malformed.
fn deserialize_tab(payload: &str) -> Option<(Tab, Option<(i32, i32)>)> {
    let p: Vec<&str> = payload.split('\t').collect();
    if p.len() < 6 || p[0].is_empty() {
        return None;
    }
    let column = SortColumn::from_code(p[1]).unwrap_or(SortColumn::Name);
    let order = if p[2] == "1" {
        SortOrder::Asc
    } else {
        SortOrder::Desc
    };
    let tab = Tab::restored(
        PathBuf::from(p[0]),
        SortState { column, order },
        p[3] == "1",
        p[4] == "1",
        GroupMode::from_code(p[5]).unwrap_or(GroupMode::FoldersFirst),
    );
    let drop = match (p.get(6), p.get(7)) {
        (Some(x), Some(y)) => x.parse().ok().zip(y.parse().ok()),
        _ => None,
    };
    Some((tab, drop))
}

/// Removes the `from` tab of panel `src` and prunes the panel if it empties out.
/// Returns `true` — WITHOUT REMOVING ANYTHING — if it was the last tab of the last
/// panel (empty instance): the caller quits. We NEVER leave an empty
/// `TabBook` in the state: the event loop still processes a few events
/// before stopping, and any `with_tabs` access would panic.
fn remove_tab_pruning(state: &AppState, src: usize, from: usize) -> bool {
    let mut panels = state.panels.borrow_mut();
    if src >= panels.len() || from >= panels[src].tabs.tabs.len() {
        return false;
    }
    if panels.len() == 1 && panels[src].tabs.tabs.len() == 1 {
        return true; // empty instance → quit (the state stays consistent to the end)
    }
    panels[src].tabs.tabs.remove(from);
    {
        let b = &mut panels[src].tabs;
        if !b.tabs.is_empty() && b.active >= b.tabs.len() {
            b.active = b.tabs.len() - 1;
        }
    }
    if panels[src].tabs.tabs.is_empty() && state.layout.borrow_mut().remove_panel(src) {
        panels.remove(src);
        // Re-indexing the active panel (same rule as panel closing):
        // clamp if the active one pointed past the end, decrement if it was AFTER `src`
        // (the indices shifted by one).
        let mut a = state.active_panel.borrow_mut();
        if *a >= panels.len() {
            *a = panels.len().saturating_sub(1);
        } else if src < *a {
            *a -= 1;
        }
    }
    false
}

/// Outcome of a cross-instance tab transfer attempt.
enum Transfer {
    /// Transferred; the instance stays open → the caller refreshes the UI.
    Moved,
    /// Transferred; the instance has EMPTIED and is closing → refresh nothing.
    Emptied,
    /// Failed (invalid target) → the caller falls back to tear-off.
    Failed,
}

/// Transfers the `from` tab of panel `src` to the Ranger instance `target_hwnd`
/// (window under the cursor), then removes it from here. If the instance empties out,
/// it quits (without overwriting the shared workspace).
fn transfer_tab_to(
    state: &AppState,
    src: usize,
    from: usize,
    target_hwnd: isize,
    drop: (i32, i32),
) -> Transfer {
    let payload = {
        let panels = state.panels.borrow();
        if src >= panels.len() || from >= panels[src].tabs.tabs.len() {
            return Transfer::Failed;
        }
        serialize_tab(&panels[src].tabs.tabs[from], drop)
    };
    if !crate::winmsg::send(target_hwnd, &payload) {
        return Transfer::Failed;
    }
    if remove_tab_pruning(state, src, from) {
        SUPPRESS_WORKSPACE_PERSIST.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = slint::quit_event_loop();
        Transfer::Emptied
    } else {
        // The target now owns the tab: persist the source immediately so
        // an unexpected shutdown doesn't restore it on the next launch.
        state.persist_workspace();
        Transfer::Moved
    }
}

/// Slint coordinates (logical client) → physical screen. On Windows we
/// must go through `ClientToScreen`: `Window::position()` describes the outer
/// window, and therefore introduces the border thickness + the title bar.
fn window_logical_to_screen(window: &MainWindow, x: f32, y: f32) -> (i32, i32) {
    let scale = window.window().scale_factor().max(0.1);
    let client = ((x * scale).round() as i32, (y * scale).round() as i32);
    #[cfg(windows)]
    if let Some(screen) = crate::winmsg::client_to_screen(client.0, client.1) {
        return screen;
    }
    let outer = window.window().position();
    (outer.x + client.0, outer.y + client.1)
}

/// Physical screen → logical Slint client coordinates. The fallback is only used
/// before HWND initialization or outside Windows.
fn screen_to_window_logical(window: &MainWindow, x: i32, y: i32) -> (f32, f32) {
    let scale = window.window().scale_factor().max(0.1);
    #[cfg(windows)]
    if let Some((client_x, client_y)) = crate::winmsg::screen_to_client(x, y) {
        return (client_x as f32 / scale, client_y as f32 / scale);
    }
    let outer = window.window().position();
    ((x - outer.x) as f32 / scale, (y - outer.y) as f32 / scale)
}

/// Insertion point (panel, gap) for a received tab, from the screen
/// drop point `(sx, sy)` in physical pixels. The geometry relies solely on the
/// container exported by Slint, the fractional rectangles (`panel_rects`),
/// the tab widths (`tab_layout`), and the scroll reported by
/// `panel-tabs-scrolled`. `gap = None` denotes an insertion at the end of the bar.
fn locate_tab_drop(
    window: &MainWindow,
    state: &AppState,
    sx: i32,
    sy: i32,
) -> (usize, Option<usize>) {
    let active = *state.active_panel.borrow();
    // Physical screen → logical window → panel container frame of reference.
    let (wx, wy) = screen_to_window_logical(window, sx, sy);
    let cx = wx - window.get_panels_cont_x();
    let cy = wy - window.get_panels_cont_y();
    let (cw, ch) = (window.get_panels_cont_w(), window.get_panels_cont_h());
    if cw <= 0.0 || ch <= 0.0 || !(0.0..cw).contains(&cx) || !(0.0..ch).contains(&cy) {
        return (active, None); // outside the panels area → active panel, at the end
    }
    let panels = state.panels.borrow();
    let rects = panel_rects(&current_geom(state), panels.len());
    let Some(p) = rects.iter().position(|r| {
        cx >= r.x * cw && cx < (r.x + r.w) * cw && cy >= r.y * ch && cy < (r.y + r.h) * ch
    }) else {
        return (active, None);
    };
    // Position local to the panel's RECT (the PanelComponent is inset by
    // +panel-gap=4px within its rect; the bands below absorb that).
    let lx = cx - rects[p].x * cw;
    let ly = cy - rects[p].y * ch;
    let mode = panels[p].tab_bar_mode;
    // Does the drop target the panel's TAB BAR (→ precise gap) or its
    // body (→ predictable insertion at the end)? The band depends on the mode.
    let in_bar = match mode {
        // Horizontal: gap 4 + panel padding 7 + toolbar padding 6 + strip 26,
        // + tolerance → 44 px below the TOP of the panel.
        0 => ly <= 44.0,
        // Vertical: bar column — gap 4 + panel padding 7 + clamped
        // width (shared `vbar_width` formula), + 4 px tolerance.
        _ => {
            let rect_w = rects[p].w * cw;
            let bw = vbar_width(
                rect_w - 2.0 * 4.0, /* panel-gap */
                panels[p].vbar_user_w,
            );
            match mode {
                1 => lx <= 4.0 + 7.0 + bw + 4.0,
                _ => lx >= rect_w - 4.0 - 7.0 - bw - 4.0,
            }
        }
    };
    if !in_bar {
        return (p, None);
    }
    // Gap in the bar: coordinate along the MAIN AXIS, local to the
    // start of the tab list (insets below), converted to the Flickable's
    // CONTENT frame of reference (viewport ≤ 0, reported by the view). Same rule as
    // reordering: first half of a tab → before it, second half →
    // after. Insets: horizontal = 13 px (padding 7 + toolbar 6, see
    // `tabs-row-x`); vertical = gap 4 + padding 7 + bar padding 4 + "+"
    // head 24 + spacing 4 + rule 1 + spacing 4 = 48 px (MUST == `vtabs-col-y`
    // on the Slint side, 44 px excluding panel-gap).
    let strip_pos = match mode {
        0 => lx - 13.0 - panels[p].tabs_viewport_x,
        _ => ly - 48.0 - panels[p].tabs_viewport_x,
    };
    let titles: Vec<String> = panels[p]
        .tabs
        .tabs
        .iter()
        .map(|t| tab_title(&t.current_path))
        .collect();
    let geo = tab_layout_for(mode, &titles);
    let gap = geo
        .iter()
        .position(|(w, off)| strip_pos < off + w / 2.0)
        .unwrap_or(geo.len());
    (p, Some(gap))
}

/// Cross-instance hover: simulates a LOCAL tab drag at the SCREEN point
/// `(sx, sy)` received from the other instance → the existing insertion preview
/// machinery (identical to the intra-instance drag) lights up in the targeted panel.
/// `drag-source-panel` stays -1 (EXTERNAL drag, no source tab here).
fn set_external_hover(window: &MainWindow, sx: i32, sy: i32) {
    // Physical screen → logical window (`drag-abs-x/y`'s frame of reference, see tab-drag-progress).
    let (wx, wy) = screen_to_window_logical(window, sx, sy);
    window.set_drag_source_panel(-1);
    window.set_drag_abs_x(wx);
    window.set_drag_abs_y(wy);
    window.set_drag_active(true);
}

/// End of cross-instance hover / drop: turns off the simulated drag → clears the
/// insertion preview.
fn clear_external_hover(window: &MainWindow) {
    if window.get_drag_active() && window.get_drag_source_panel() < 0 {
        window.set_drag_active(false);
    }
}

/// OLE hover for files coming from another Windows instance/application.
/// OLE coordinates are in physical screen space; we convert them into the same
/// logical frame of reference as the internal Slint drag, so we can reuse without divergence
/// the panel/row hit-test, the hover, and the action ghost.
#[cfg(windows)]
fn set_external_file_hover(window: &MainWindow, screen_x: i32, screen_y: i32, copy: bool) {
    let (wx, wy) = screen_to_window_logical(window, screen_x, screen_y);
    window.set_file_drag_source_panel(-1);
    window.set_file_drag_target_invalid(false);
    window.set_file_drag_copy(copy);
    window.set_file_drag_abs_x(wx);
    window.set_file_drag_abs_y(wy);
    window.set_file_drag_active(true);
}

#[cfg(windows)]
fn clear_external_file_hover(window: &MainWindow) {
    if window.get_file_drag_active() && window.get_file_drag_source_panel() < 0 {
        window.set_file_drag_active(false);
        window.set_file_drag_target_panel(-1);
        window.set_file_drag_target_row(-1);
        window.set_file_drag_target_folder(false);
        window.set_file_drag_target_exec(false);
        window.set_file_drag_target_invalid(false);
        window.set_file_drag_copy(false);
    }
}

/// Finishes an OLE drop received by Ranger. The final target is re-read after
/// replaying the authoritative point: executable → ShellExecute, Ctrl → direct copy,
/// otherwise the same Move/Copy/Link menu as for a drag between views.
#[cfg(windows)]
fn on_external_file_drop(
    window: &MainWindow,
    state: &AppState,
    paths: Vec<PathBuf>,
    screen_x: i32,
    screen_y: i32,
    copy: bool,
    staging: Option<IncomingDropStaging>,
) {
    if paths.is_empty() {
        clear_external_file_hover(window);
        return;
    }
    set_external_file_hover(window, screen_x, screen_y, copy);
    let target_panel = window.get_file_drag_target_panel();
    if target_panel < 0 {
        clear_external_file_hover(window);
        return;
    }
    let target_row = window.get_file_drag_target_row();
    let target = target_panel as usize;
    let target_info = if target_row >= 0 {
        panel_path_at_row(state, target, target_row as usize).map(|path| {
            let is_dir = panel_folder_at_row(state, target, target_row as usize).is_some();
            (path, is_dir)
        })
    } else {
        Some((panel_dir(state, target), true))
    };
    if let Some(staging) = staging {
        // Ranger already owns this staging tree before the OLE call returns.
        // Ranger-owned data goes directly through the normal paste pipeline,
        // name conflicts included. No Move/Copy/Link menu is shown because
        // virtual attachments and temporary application paths are copy-only.
        let dest = match &target_info {
            Some((path, true)) => path.clone(),
            _ => panel_dir(state, target),
        };
        clear_external_file_hover(window);
        begin_paste_with_cleanup(
            window,
            state,
            staging.op,
            dest,
            paths,
            Some(staging.cleanup),
        );
        return;
    }
    if let Some((target_path, target_is_dir)) = target_info
        && paths_conflict_with_drop_target(&paths, &target_path, target_is_dir)
    {
        clear_external_file_hover(window);
        return;
    }
    *state.external_drop_paths.borrow_mut() = paths;
    window.set_file_drop_src_panel(-1);
    window.set_file_drop_target_panel(target_panel);
    window.set_file_drop_target_row(target_row);

    let (wx, wy) = screen_to_window_logical(window, screen_x, screen_y);
    window.set_file_drop_menu_open(false);
    if window.invoke_file_drop_onto_exec() {
        // The callback consumed `external_drop_paths`.
    } else if copy {
        window.invoke_file_drop_action(1);
    } else {
        window.set_file_drop_menu_x(wx);
        window.set_file_drop_menu_y(wy);
        window.set_file_drop_menu_open(true);
    }
    clear_external_file_hover(window);
}

/// Receives a tab transferred from ANOTHER instance: inserts it at the drop
/// point (panel + gap in the bar, see `locate_tab_drop`) and activates it. The
/// window has already been brought to the foreground by the sender.
fn on_tab_received(window: &MainWindow, state: &AppState, payload: &str) {
    let Some((tab, drop)) = deserialize_tab(payload) else {
        clear_external_hover(window);
        return;
    };
    // Case B: drop onto a FAVORITES folder of THIS instance. The async
    // hover may LAG BEHIND the final position → we REPLAY the
    // drop point (authoritative, carried in the payload) via
    // `set_external_hover`, then READ `fav-hover-container` PULL-BASED (the binding
    // is re-evaluated on read → reflects that exact point). Non-empty = we save it
    // as a favorite (the tab was already removed from the source by the transfer: it
    // "merges into" the favorites folder instead of attaching to a panel).
    let fav_container = match drop {
        Some((sx, sy)) => {
            set_external_hover(window, sx, sy);
            window.get_fav_hover_container().to_string()
        }
        None => String::new(),
    };
    clear_external_hover(window); // clears the insertion preview in all cases
    if !fav_container.is_empty() {
        add_paths_to_favorite(
            window,
            state,
            std::slice::from_ref(&tab.current_path),
            &fav_container,
        );
        return;
    }
    let (panel, gap) = match drop {
        Some((sx, sy)) => locate_tab_drop(window, state, sx, sy),
        None => (*state.active_panel.borrow(), None),
    };
    let path = tab.current_path.clone();
    let panel = panel.min(state.panels.borrow().len().saturating_sub(1));
    {
        let mut panels = state.panels.borrow_mut();
        let book = &mut panels[panel].tabs;
        let at = gap.unwrap_or(book.tabs.len()).min(book.tabs.len());
        book.tabs.insert(at, tab);
        book.active = at;
    }
    *state.active_panel.borrow_mut() = panel;
    info!(path = %path.display(), panel, "tab received from another instance");
    load_directory(window, state, &path, false);
    state.persist_workspace();
}

fn move_tab_between(
    state: &AppState,
    src: usize,
    from: usize,
    mut target: usize,
    insert_at: usize,
) -> Option<usize> {
    let mut panels = state.panels.borrow_mut();
    let n = panels.len();
    if src >= n || target >= n || src == target {
        return None;
    }
    if from >= panels[src].tabs.tabs.len() {
        return None;
    }

    // Extract the tab from the source.
    let tab = panels[src].tabs.tabs.remove(from);
    {
        let b = &mut panels[src].tabs;
        if !b.tabs.is_empty() && b.active >= b.tabs.len() {
            b.active = b.tabs.len() - 1;
        }
    }
    let src_empty = panels[src].tabs.tabs.is_empty();

    // Insert into the target.
    {
        let dst = &mut panels[target].tabs;
        let at = insert_at.min(dst.tabs.len());
        dst.tabs.insert(at, tab);
        dst.active = at;
    }

    // Source emptied (it was its last tab) → close the source panel.
    if src_empty && state.layout.borrow_mut().remove_panel(src) {
        panels.remove(src);
        if src < target {
            target -= 1;
        }
    }
    Some(target.min(panels.len().saturating_sub(1)))
}

/// Dropping a tab on the EDGE of a panel: splits
/// `target_panel` according to `dir` and places the `from_tab` tab (moved from
/// `source_panel`) into the newly created panel. `new_first` = the new
/// panel is the first child (west/north edge). If the source panel becomes
/// empty, it is removed (merge). Returns `true` if the operation took place.
fn split_with_tab(
    state: &AppState,
    source_panel: usize,
    from_tab: usize,
    target_panel: usize,
    dir: SplitDir,
    new_first: bool,
) -> bool {
    let mut panels = state.panels.borrow_mut();
    let n = panels.len();
    if source_panel >= n || target_panel >= n {
        return false;
    }
    if from_tab >= panels[source_panel].tabs.tabs.len() {
        return false;
    }
    // Splitting your own panel with its only tab doesn't make sense.
    if target_panel == source_panel && panels[source_panel].tabs.tabs.len() <= 1 {
        return false;
    }
    // Cap: a split creates +1 panel. But if the source then empties out
    // (last tab → closes), the net change is zero → allowed even at 16.
    let source_will_close =
        target_panel != source_panel && panels[source_panel].tabs.tabs.len() == 1;
    if n >= MAX_PANELS && !source_will_close {
        return false;
    }

    // 1. Extract the tab from the source panel.
    let tab = panels[source_panel].tabs.tabs.remove(from_tab);
    let path = tab.current_path.clone();
    {
        let b = &mut panels[source_panel].tabs;
        if !b.tabs.is_empty() && b.active >= b.tabs.len() {
            b.active = b.tabs.len() - 1;
        }
    }
    let source_empty = panels[source_panel].tabs.tabs.is_empty();

    // 2. New panel containing the moved tab. It inherits the columns
    //    and the tab bar position of the source view (visual
    // consistency during a cross-view drag).
    let new_idx = panels.len();
    let inherited_cols = panels[source_panel].columns.clone();
    let inherited_mode = panels[source_panel].tab_bar_mode;
    let inherited_vbar = panels[source_panel].vbar_user_w;
    let (rows_model, rendered_rows_model) = new_row_models();
    panels.push(Panel {
        tabs: TabBook {
            tabs: vec![tab],
            active: 0,
        },
        rows_model,
        rendered_rows_model,
        rows_revision: Cell::new(0),
        viewport_reset_gen: Cell::new(0),
        viewport_top: Cell::new(0.0),
        viewport_height: Cell::new(DEFAULT_RENDER_VIEWPORT_HEIGHT),
        rendered_first: Cell::new(0),
        rendered_end: Cell::new(0),
        displayed_path: path,
        columns: inherited_cols,
        hidden_count: 0,
        unavailable: false,
        tabs_viewport_x: 0.0,
        tab_bar_mode: inherited_mode,
        vbar_user_w: inherited_vbar,
        pending_initial: false,
        pending_listing: false,
        listing_gen: 0,
        pending_select: None,
    });

    // 3. Split the target leaf in the tree.
    let ok = state
        .layout
        .borrow_mut()
        .split_leaf(target_panel, dir, new_idx, 0.5, new_first);
    if !ok {
        // Rollback: remove the created panel, put the tab back into the source.
        if let Some(p) = panels.pop()
            && let Some(t) = p.tabs.tabs.into_iter().next()
        {
            let dst = &mut panels[source_panel].tabs;
            let at = from_tab.min(dst.tabs.len());
            dst.tabs.insert(at, t);
        }
        return false;
    }

    // 4. Source emptied → remove the source panel (merge).
    let mut final_active = new_idx;
    if source_empty && state.layout.borrow_mut().remove_panel(source_panel) {
        panels.remove(source_panel);
        if source_panel < final_active {
            final_active -= 1;
        }
    }

    *state.active_panel.borrow_mut() = final_active.min(panels.len().saturating_sub(1));
    true
}

/// True if `name` can't be used as a copy name: empty,
/// a path separator, `.`/`..`, or already present in the target folder.
fn paste_name_invalid(state: &AppState, name: &str) -> bool {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return true;
    }
    match state.paste_job.borrow().as_ref() {
        Some(job) => {
            let target = job.dst_dir.join(name);
            target_taken(state, &target) || job.claims(&target)
        }
        None => true,
    }
}

/// `true` if a destination is unavailable: already on disk, or claimed by an
/// operation still running. Both are needed — checking only the filesystem
/// would let two concurrent pastes resolve to the very same path.
fn target_taken(state: &AppState, path: &Path) -> bool {
    path.exists() || state.ops.is_reserved(path)
}

/// Distinct destinations for a whole batch, in order.
///
/// Each name is picked against `taken` AND against what earlier items of the
/// same batch already took. That last part matters: nothing is on disk yet
/// while the batch is being planned, so duplicating `x.txt` together with
/// `x - Copy01.txt` would otherwise hand both of them `x - Copy02.txt` and one
/// result would overwrite the other.
fn plan_unique_targets(sources: &[PathBuf], taken: impl Fn(&Path) -> bool) -> Vec<PathBuf> {
    let mut planned: Vec<PathBuf> = Vec::with_capacity(sources.len());
    // Set rather than a scan of `planned`: picking a name already probes the
    // filesystem candidate by candidate, and a batch of same-named items would
    // make a linear lookup grow on top of that.
    let mut claimed: HashSet<PathBuf> = HashSet::with_capacity(sources.len());
    for src in sources {
        let dst = ops::unique_sibling_where(src, |p| taken(p) || claimed.contains(p));
        claimed.insert(dst.clone());
        planned.push(dst);
    }
    planned
}

/// State of a name proposed in the Rename dialog. The distinction between
/// `Conflict` and `ReplaceableFile` avoids displaying "Force replace"
/// for a syntactically invalid name or for a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RenameNameStatus {
    Valid,
    Invalid,
    Conflict,
    ReplaceableFile,
}

/// Availability of a name in a folder, shared by the Create and
/// Rename dialogs. `ExistingNonDirectory` includes files and links: a
/// directory entry, even a broken link, always occupies its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryNameAvailability {
    Available,
    Invalid,
    ExistingNonDirectory,
    ExistingDirectory,
    /// Nothing on disk yet, but a running operation will write there. Occupied
    /// for every purpose, and never replaceable: there is no entry to replace.
    Reserved,
}

/// `entry_name_availability` widened to the destinations of operations in
/// flight. A name a running copy is about to write is not free either, even
/// though nothing occupies it on disk yet — creating or renaming onto it would
/// have that entry overwritten as soon as the operation reaches it.
fn entry_name_availability_now(
    state: &AppState,
    parent: &Path,
    name: &str,
) -> EntryNameAvailability {
    let on_disk = entry_name_availability(parent, name);
    with_reservation(on_disk, state.ops.is_reserved(&parent.join(name)))
}

/// Folds a reservation into a filesystem-only verdict. Only a name that was
/// otherwise free becomes `Reserved`: a real entry keeps its own kind, which is
/// what the dialogs report on.
fn with_reservation(on_disk: EntryNameAvailability, reserved: bool) -> EntryNameAvailability {
    match on_disk {
        EntryNameAvailability::Available if reserved => EntryNameAvailability::Reserved,
        other => other,
    }
}

fn entry_name_availability(parent: &Path, name: &str) -> EntryNameAvailability {
    if !ops::is_valid_entry_name(name) {
        return EntryNameAvailability::Invalid;
    }
    match std::fs::symlink_metadata(parent.join(name)) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => EntryNameAvailability::Available,
        Err(_) => EntryNameAvailability::ExistingNonDirectory,
        Ok(meta) if meta.is_dir() => EntryNameAvailability::ExistingDirectory,
        Ok(_) => EntryNameAvailability::ExistingNonDirectory,
    }
}

/// `rename_name_status` accounting for the operations in flight. Kept apart
/// from the pure form so the naming rules stay testable without app state.
fn rename_name_status_now(state: &AppState, source: &Path, name: &str) -> RenameNameStatus {
    let on_disk = rename_name_status(source, name);
    let reserved = source
        .parent()
        .is_some_and(|parent| state.ops.is_reserved(&parent.join(name)));
    rename_with_reservation(on_disk, reserved)
}

/// Folds a reservation into a filesystem-only rename verdict. A destination a
/// running operation will write offers no entry to replace, so it downgrades to
/// a plain conflict and "Force replace" is never proposed for it.
fn rename_with_reservation(on_disk: RenameNameStatus, reserved: bool) -> RenameNameStatus {
    match on_disk {
        RenameNameStatus::Valid if reserved => RenameNameStatus::Conflict,
        other => other,
    }
}

fn rename_name_status(source: &Path, name: &str) -> RenameNameStatus {
    let Some(parent) = source.parent() else {
        return RenameNameStatus::Invalid;
    };
    let old = source.file_name().map(|n| n.to_string_lossy());
    if old.as_deref() == Some(name) {
        return RenameNameStatus::Valid;
    }
    // Case change on Windows: the "target" is the entry itself,
    // this isn't a destructive replacement. Purely lexical comparison,
    // without `canonicalize`: this check runs on every keystroke and shouldn't
    // add a network round-trip. (The `target` computation is ONLY used here → it
    // stays inside the Windows block so it isn't "unused" on Linux.)
    #[cfg(windows)]
    {
        let target = parent.join(name);
        if source
            .to_string_lossy()
            .eq_ignore_ascii_case(&target.to_string_lossy())
        {
            return RenameNameStatus::Valid;
        }
    }

    match entry_name_availability(parent, name) {
        EntryNameAvailability::Available => RenameNameStatus::Valid,
        EntryNameAvailability::Invalid => RenameNameStatus::Invalid,
        EntryNameAvailability::ExistingDirectory => RenameNameStatus::Conflict,
        EntryNameAvailability::Reserved => RenameNameStatus::Conflict,
        EntryNameAvailability::ExistingNonDirectory => {
            let Ok(source_meta) = std::fs::symlink_metadata(source) else {
                return RenameNameStatus::Conflict;
            };
            if source_meta.is_dir() {
                RenameNameStatus::Conflict
            } else {
                RenameNameStatus::ReplaceableFile
            }
        }
    }
}

/// UTF-8 offset expected by `TextInput::set_selection_offsets`.
///
/// For a file, the caret is placed before the extension recognized by the same
/// rule as display and duplication (`fs::ops::split_name`). Dotfiles
/// and purely numeric suffixes therefore remain whole names. A
/// folder is never split, even if its name contains a period.
fn filename_caret_offset(name: &str, is_dir: bool) -> i32 {
    let offset = if is_dir {
        name.len()
    } else {
        let (stem, extension) = ops::split_name(name);
        if extension.is_empty() {
            name.len()
        } else {
            stem.len()
        }
    };
    offset.min(i32::MAX as usize) as i32
}

/// Advances the `PasteJob`: opens the popup for the next conflict, or
/// executes the operation if all conflicts are resolved.
/// Resolves a conflict by **overwriting**: the target keeps its original name and will be
/// replaced (the worker removes the existing item before the copy). Safeguard: a
/// "self-overwrite" (copying a file into its own folder) is
/// ignored — never delete the source.
fn resolve_replace(job: &mut PasteJob, src: PathBuf) {
    let Some(name) = src.file_name() else { return };
    let target = job.dst_dir.join(name);
    if target == src {
        return; // copy onto itself → we don't overwrite (the item is simply skipped)
    }
    job.resolved.push((src, target, true));
}

fn advance_paste(window: &MainWindow, state: &AppState) {
    enum Step {
        Conflict {
            original: String,
            suggested: String,
            rename_only: bool,
            is_dir: bool,
        },
        Execute(PasteJob),
    }
    let step = {
        let mut guard = state.paste_job.borrow_mut();
        if guard.is_none() {
            return;
        }
        let has_pending = guard
            .as_ref()
            .map(|j| !j.pending.is_empty())
            .unwrap_or(false);
        if has_pending {
            let job = guard.as_mut().unwrap();
            let src = job.pending.pop_front().unwrap();
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // The suggestion must also clear the destinations this same paste
            // has already resolved, none of which exist on disk yet.
            let base = job.dst_dir.join(&name);
            let suggested =
                ops::unique_sibling_where(&base, |p| target_taken(state, p) || job.claims(p))
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| name.clone());
            // Copy WITHIN THE SAME FOLDER (src already in dst_dir): overwrite =
            // destroy the source (neutralized), skip = cancel → only
            // renaming makes sense. We therefore hide Replace/Skip.
            let rename_only = src.parent() == Some(job.dst_dir.as_path());
            // `is_dir()` deliberately follows a symlink if present: in the
            // dialog, a link to a folder is handled like a folder.
            let is_dir = src.is_dir();
            job.current = Some(src);
            Step::Conflict {
                original: name,
                suggested,
                rename_only,
                is_dir,
            }
        } else {
            Step::Execute(guard.take().unwrap())
        }
    };
    match step {
        Step::Conflict {
            original,
            suggested,
            rename_only,
            is_dir,
        } => {
            window.set_paste_conflict_original(original.into());
            window.set_paste_conflict_name(suggested.into());
            window.set_paste_conflict_name_taken(false);
            window.set_paste_conflict_rename_only(rename_only);
            window.set_paste_conflict_is_dir(is_dir);
            window.set_paste_conflict_open(true);
            window.set_paste_conflict_focus_gen(
                window.get_paste_conflict_focus_gen().wrapping_add(1),
            );
        }
        Step::Execute(job) => {
            window.set_paste_conflict_open(false);
            execute_paste(window, state, job);
        }
    }
}

/// Starts a paste operation (`op`) from `sources` to `dst_dir`:
/// splits into `resolved` (no conflict) / `pending` (conflict → popup), then
/// `advance_paste`. Shared by paste (clipboard) and drag'n'drop.
fn begin_paste(
    window: &MainWindow,
    state: &AppState,
    op: ClipOp,
    dst_dir: PathBuf,
    sources: Vec<PathBuf>,
) {
    begin_paste_with_cleanup(window, state, op, dst_dir, sources, None);
}

fn begin_paste_with_cleanup(
    window: &MainWindow,
    state: &AppState,
    op: ClipOp,
    dst_dir: PathBuf,
    sources: Vec<PathBuf>,
    transient_cleanup: Option<TransientDropGuard>,
) {
    if dst_dir.as_os_str().is_empty() || sources.is_empty() {
        return;
    }
    // Refuses a second paste while one is still being resolved.
    if state.paste_job.borrow().is_some() {
        return;
    }
    let mut job = PasteJob {
        op,
        dst_dir: dst_dir.clone(),
        pending: std::collections::VecDeque::new(),
        resolved: Vec::new(),
        current: None,
        transient_cleanup,
    };
    for src in sources {
        // Ignores drops of an item onto itself / into its own folder.
        if src.parent() == Some(dst_dir.as_path()) && matches!(op, ClipOp::Cut) {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        let target = dst_dir.join(name);
        // Occupied means: on disk, claimed by a running operation, or already
        // taken by an earlier item of this very paste. The last case happens
        // with same-named sources from different folders (OS clipboard, drop
        // from another app) — without it both would land on the same path.
        if target_taken(state, &target) || job.claims(&target) {
            job.pending.push_back(src);
        } else {
            job.resolved.push((src, target, false));
        }
    }
    *state.paste_job.borrow_mut() = Some(job);
    advance_paste(window, state);
}

/// Executes the resolved copies/moves (targets guaranteed free) on a
/// background thread with a progress bar.
fn execute_paste(window: &MainWindow, state: &AppState, job: PasteJob) {
    if job.resolved.is_empty() {
        // Everything was skipped/cancelled: nothing to execute, just refresh.
        refresh_listing(window, state, &job.dst_dir);
        return;
    }
    let lang = state.snapshot_config().language;
    // Item to highlight once the copy/move finishes: the
    // 1st target (the order of `resolved` follows the original selection's order).
    // Sorting often places the newcomer off-screen → `op-finished` will bring it back into view.
    let pending_focus = job.resolved.first().map(|(_, dst, _)| dst.clone());
    let cut = matches!(job.op, ClipOp::Cut);
    let work = match job.op {
        ClipOp::Copy => Heavy::Copy(job.resolved),
        ClipOp::Cut => Heavy::Move(job.resolved),
    };
    start_heavy_op(
        window,
        state,
        work,
        lang,
        pending_focus,
        job.transient_cleanup,
    );
    // The cut is consumed once the operation is under way, never before: its
    // paths are what the operation moves, and dropping them early would lose
    // the pending selection if the paste did not start.
    if cut {
        let mut clip = state.clipboard.borrow_mut();
        clip.paths.clear();
        clip.op = None;
    }
}

// ---------- Long background operations (progress bar) ----------

const OP_RUNNING: i32 = 1;
const OP_SCAN: i32 = 2;
const OP_SUCCESS: i32 = 3;
const OP_ERROR: i32 = 4;
const OP_CANCELLED: i32 = 5;

/// Work handed to the background thread. For Copy/Move, each item is
/// `(source, target, overwrite)`: `overwrite = true` → the existing target is
/// removed before the operation ("Replace" resolution).
enum Heavy {
    Copy(Vec<(PathBuf, PathBuf, bool)>),
    Move(Vec<(PathBuf, PathBuf, bool)>),
    Trash(Vec<PathBuf>),
    PermanentDelete(Vec<PathBuf>),
}

/// i18n labels captured (owned → Send) for the thread.
#[derive(Clone)]
struct OpLabels {
    running: String,
    scanning: String,
    done: String,
    cancelled: String,
    errors: String,
    items: String,
}

/// Toasts drawn in the stack; the rest are summarised by the "+N others" chip.
/// Mirrors the bound used by the `for` loop over `ops` in the .slint.
const MAX_VISIBLE_TOASTS: usize = 3;

/// Number of operation toasts the stack cannot display.
fn hidden_toast_count(total: usize) -> usize {
    total.saturating_sub(MAX_VISIBLE_TOASTS)
}

/// Pushes a toast update from a worker thread to the UI thread.
///
/// `visible` carries the deferred-appearance threshold: below it the operation
/// gets no row at all, so a quick copy never flashes a toast.
#[allow(clippy::too_many_arguments)]
fn push_op(
    weak: &slint::Weak<MainWindow>,
    op_id: i32,
    state_i: i32,
    visible: bool,
    progress: f32,
    title: String,
    detail: String,
    percent: String,
) {
    if !visible {
        return;
    }
    let row = OpProgress {
        id: op_id,
        state: state_i,
        progress,
        indeterminate: state_i == OP_SCAN,
        title: title.into(),
        detail: detail.into(),
        percent: percent.into(),
    };
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.invoke_op_progress(row);
        }
    });
}

/// Inserts or refreshes one operation's toast row, keyed by its id.
fn upsert_op_row(state: &AppState, row: OpProgress) {
    let model = &state.ops_model;
    let existing =
        (0..model.row_count()).find(|&i| model.row_data(i).map(|r| r.id) == Some(row.id));
    match existing {
        Some(i) => model.set_row_data(i, row),
        None => model.push(row),
    }
}

/// Removes one operation's toast row, if it still has one.
fn remove_op_row(state: &AppState, op_id: i32) {
    let model = &state.ops_model;
    if let Some(i) =
        (0..model.row_count()).find(|&i| model.row_data(i).map(|r| r.id) == Some(op_id))
    {
        model.remove(i);
    }
}

/// Refreshes what depends on the toast set as a whole: the "+N others" chip.
fn refresh_ops_ui(window: &MainWindow, state: &AppState) {
    let hidden = hidden_toast_count(state.ops_model.row_count());
    let text = if hidden == 0 {
        String::new()
    } else {
        i18n::op_more_text(state.snapshot_config().language, hidden)
    };
    window.set_ops_more_text(text.into());
}

/// Publishes whether anything heavy is in flight. Drives the deferred
/// refreshes, which must not re-list while an operation is writing.
fn sync_op_busy(window: &MainWindow, state: &AppState) {
    window.set_op_busy(state.ops.in_flight());
}

/// Settling delay before re-listing once operations complete. Short enough to
/// read as instant, long enough to absorb a burst of completions.
const OP_REFRESH_DEBOUNCE_MS: u64 = 150;

thread_local! {
    static OP_REFRESH_DEBOUNCE: slint::Timer = slint::Timer::default();
}

/// Requests the re-listing that follows a completed operation.
///
/// `refresh_all_panels` re-reads every local panel synchronously, so running it
/// once per completion would be wasteful now that operations finish
/// independently. Restarting a single-shot timer collapses a burst into one
/// pass. A stream of completions closer together than the delay keeps pushing
/// it back, which is the intended trade: the views stay untouched while things
/// are still moving, then catch up in a single re-listing.
fn request_op_refresh(window: &MainWindow) {
    let weak = window.as_weak();
    OP_REFRESH_DEBOUNCE.with(|timer| {
        timer.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(OP_REFRESH_DEBOUNCE_MS),
            move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_refresh_all();
                }
            },
        );
    });
}

/// Selects and scrolls to the item left behind by the last operation to finish,
/// once the views have been re-listed. Only acts if the ACTIVE view is really
/// showing its folder: the user may have navigated elsewhere during a long
/// copy, and the selection of an unrelated folder is never moved.
fn apply_focus_after_refresh(window: &MainWindow, state: &AppState) {
    let Some(target) = state.focus_after_refresh.borrow_mut().take() else {
        return;
    };
    let (Some(dir), Some(name)) = (target.parent(), target.file_name()) else {
        return;
    };
    if state.current_path() == dir {
        focus_entry_by_name(window, state, &name.to_string_lossy());
    }
}

/// Deposits a trash result into the Send queue then wakes its single
/// Slint consumer. Recovering from a poisoned mutex avoids permanently
/// losing the ability to cancel a deletion.
fn deliver_trash_event(
    weak: &slint::Weak<MainWindow>,
    deliveries: &Arc<std::sync::Mutex<VecDeque<TrashDelivery>>>,
    event: TrashDelivery,
) {
    deliveries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push_back(event);
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak.upgrade() {
            w.invoke_process_trash_events();
        }
    });
}

/// Starts a long operation: registers it, prepares the cancellation flag + the
/// toast, then launches the background thread. Operations run concurrently —
/// each gets its own thread, its own toast and its own cancel button — so this
/// never turns work away.
fn start_heavy_op(
    window: &MainWindow,
    state: &AppState,
    work: Heavy,
    lang: Lang,
    pending_focus: Option<PathBuf>,
    transient_cleanup: Option<TransientDropGuard>,
) {
    // Destinations are claimed for the whole run: a paste started meanwhile
    // must see these names as taken even though nothing exists on disk yet.
    // Deletions write nowhere, so they claim nothing.
    let targets = match &work {
        Heavy::Copy(items) | Heavy::Move(items) => {
            items.iter().map(|(_, dst, _)| dst.clone()).collect()
        }
        Heavy::Trash(_) | Heavy::PermanentDelete(_) => Vec::new(),
    };
    let thumbnail_invalidations = match &work {
        Heavy::Copy(items) => items.iter().map(|(_, dst, _)| dst.clone()).collect(),
        Heavy::Move(items) => items
            .iter()
            .flat_map(|(src, dst, _)| [src.clone(), dst.clone()])
            .collect(),
        Heavy::Trash(paths) | Heavy::PermanentDelete(paths) => paths.clone(),
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let op_id = state.ops.register(OpHandle {
        cancel: Some(cancel.clone()),
        pending_focus,
        targets,
        thumbnail_invalidations,
        transient_cleanup,
    });

    let s = i18n::strings_for(lang);
    let running = match work {
        Heavy::Copy(_) => s.op_copying,
        Heavy::Move(_) => s.op_moving,
        Heavy::Trash(_) => s.op_deleting,
        Heavy::PermanentDelete(_) => s.op_deleting_permanently,
    }
    .to_string();
    let labels = OpLabels {
        running,
        scanning: s.op_scanning.to_string(),
        done: s.op_done.to_string(),
        cancelled: s.op_cancelled.to_string(),
        errors: s.op_errors.to_string(),
        items: s.op_items.to_string(),
    };

    // No row until the deferred appearance threshold (400 ms) — `push_op`
    // creates it on the first update that passes it.
    sync_op_busy(window, state);

    let weak = window.as_weak();
    let trash_deliveries = state.trash_deliveries.clone();
    std::thread::spawn(move || {
        run_heavy(op_id, work, weak, cancel, lang, labels, trash_deliveries)
    });
}

fn run_heavy(
    op_id: i32,
    work: Heavy,
    weak: slint::Weak<MainWindow>,
    cancel: Arc<AtomicBool>,
    lang: Lang,
    labels: OpLabels,
    trash_deliveries: Arc<std::sync::Mutex<VecDeque<TrashDelivery>>>,
) {
    let start = Instant::now();
    let shown = || start.elapsed() >= Duration::from_millis(400);
    let is_cancelled = || cancel.load(Ordering::Relaxed);
    let mut last = Instant::now() - Duration::from_millis(500);
    let mut had_error = false;
    let mut cancelled = false;
    let mut lock_notice: Option<String> = None;

    // Breaks down into (kind, copy/move items, delete paths).
    // kind 2 = trash; kind 3 = explicit permanent deletion.
    let (kind, items, delete_paths): (u8, Vec<(PathBuf, PathBuf, bool)>, Vec<PathBuf>) = match work
    {
        Heavy::Copy(v) => (0, v, Vec::new()),
        Heavy::Move(v) => (1, v, Vec::new()),
        Heavy::Trash(v) => (2, Vec::new(), v),
        Heavy::PermanentDelete(v) => (3, Vec::new(), v),
    };

    if kind >= 2 {
        // Trash / permanent deletion — progress in items.
        let total = delete_paths.len();
        let mut done = 0usize;
        for p in &delete_paths {
            if is_cancelled() {
                cancelled = true;
                break;
            }
            let result = if kind == 3 {
                ops::permanent_delete(p)
            } else {
                match ops::trash_with_disposition(p) {
                    Ok(ops::TrashDisposition::Trashed) => {
                        deliver_trash_event(
                            &weak,
                            &trash_deliveries,
                            TrashDelivery::Trashed(p.clone()),
                        );
                        Ok(())
                    }
                    Ok(ops::TrashDisposition::PermanentlyDeleted) => Ok(()),
                    Err(error) => Err(error),
                }
            };
            if let Err(err) = result {
                had_error = true;
                error!(error = %err, path = %p.display(), permanent = kind == 3, "delete failed");
                // Diagnostic triggered ONLY after the failure: no probe,
                // enumeration, or Restart Manager session on the normal path.
                // A single toast is enough even for a multi-selection.
                if lock_notice.is_none() {
                    lock_notice = locked_item_notice(p, lang);
                }
            }
            done += 1;
            if last.elapsed() >= Duration::from_millis(80) || done == total {
                last = Instant::now();
                let pct = (done * 100).checked_div(total).unwrap_or(100);
                push_op(
                    &weak,
                    op_id,
                    OP_RUNNING,
                    shown(),
                    if total > 0 {
                        done as f32 / total as f32
                    } else {
                        1.0
                    },
                    labels.running.clone(),
                    format!("{done} / {total} {}", labels.items),
                    format!("{pct} %"),
                );
            }
        }
    } else {
        // Copy / move — progress in bytes after a pre-scan.
        push_op(
            &weak,
            op_id,
            OP_SCAN,
            shown(),
            0.0,
            labels.scanning.clone(),
            String::new(),
            String::new(),
        );
        let mut sizes = Vec::with_capacity(items.len());
        let mut total = 0u64;
        for (src, _, _) in &items {
            if is_cancelled() {
                cancelled = true;
                break;
            }
            let sz = ops::path_size(src);
            sizes.push(sz);
            total += sz;
            if last.elapsed() >= Duration::from_millis(80) {
                last = Instant::now();
                push_op(
                    &weak,
                    op_id,
                    OP_SCAN,
                    shown(),
                    0.0,
                    labels.scanning.clone(),
                    String::new(),
                    String::new(),
                );
            }
        }

        let mut done: u64 = 0;
        if !cancelled {
            for (i, (src, target, overwrite)) in items.iter().enumerate() {
                if is_cancelled() {
                    cancelled = true;
                    break;
                }
                // Overwrite ("Replace" resolution) → remove the existing
                // target BEFORE the operation, otherwise `rename`/`create_dir`
                // fail on an occupied target. Trash (recoverable) with a
                // fallback to permanent deletion. Both failing → skip
                // the item (never copy over a target that wasn't removed).
                if *overwrite
                    && target.exists()
                    && let Err(err) = ops::trash(target).or_else(|_| ops::permanent_delete(target))
                {
                    had_error = true;
                    error!(error = %err, target = %target.display(), "overwrite: remove existing failed");
                    continue;
                }
                let mut on_bytes = |delta: u64| {
                    done += delta;
                    if last.elapsed() >= Duration::from_millis(80) {
                        last = Instant::now();
                        let pct = (done.min(total) * 100).checked_div(total).unwrap_or(0) as u32;
                        push_op(
                            &weak,
                            op_id,
                            OP_RUNNING,
                            shown(),
                            if total > 0 {
                                (done as f32 / total as f32).min(1.0)
                            } else {
                                0.0
                            },
                            labels.running.clone(),
                            format!(
                                "{} / {}",
                                rfs::format_size(done, lang),
                                rfs::format_size(total, lang)
                            ),
                            format!("{pct} %"),
                        );
                    }
                };
                if kind == 1 {
                    // Move: fast rename, otherwise copy+delete.
                    match std::fs::rename(src, target) {
                        Ok(()) => {
                            done += sizes.get(i).copied().unwrap_or(0);
                        }
                        Err(_) => {
                            match ops::copy_tree_progress(src, target, &mut on_bytes, &is_cancelled)
                            {
                                Ok(ops::OpStatus::Cancelled) => cancelled = true,
                                Ok(ops::OpStatus::Done) => {
                                    if let Err(err) = ops::permanent_delete(src) {
                                        error!(error = %err, "move: delete source failed");
                                    }
                                }
                                Err(err) => {
                                    had_error = true;
                                    error!(error = %err, src = %src.display(), "move failed");
                                }
                            }
                        }
                    }
                } else {
                    // Copy.
                    match ops::copy_tree_progress(src, target, &mut on_bytes, &is_cancelled) {
                        Ok(ops::OpStatus::Cancelled) => cancelled = true,
                        Ok(ops::OpStatus::Done) => {}
                        Err(err) => {
                            had_error = true;
                            error!(error = %err, src = %src.display(), "copy failed");
                        }
                    }
                }
                if cancelled {
                    break;
                }
            }
        }
    }

    // Final state.
    let final_state = if cancelled {
        OP_CANCELLED
    } else if had_error {
        OP_ERROR
    } else {
        OP_SUCCESS
    };
    let final_visible = final_state == OP_ERROR || shown();
    let title = match final_state {
        OP_CANCELLED => labels.cancelled.clone(),
        OP_ERROR => labels.errors.clone(),
        _ => labels.done.clone(),
    };
    let weak2 = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(w) = weak2.upgrade() {
            if final_visible {
                w.invoke_op_progress(OpProgress {
                    id: op_id,
                    state: final_state,
                    progress: 1.0,
                    indeterminate: false,
                    title: title.into(),
                    detail: SharedString::default(),
                    percent: if final_state == OP_SUCCESS {
                        SharedString::from("100 %")
                    } else {
                        SharedString::default()
                    },
                });
            } else {
                // Finished below the appearance threshold: make sure no row lingers.
                w.invoke_op_dismiss(op_id);
            }
            if let Some(message) = lock_notice {
                show_notice(&w, message);
            }
            // Releases the operation, which then schedules the re-listing of ALL
            // views (an op can involve 2 panels: drag-drop source+target, or the
            // same folder open in 2 views).
            w.invoke_op_finished(op_id);
        }
    });
}

/// Connection target for the network credentials prompt: a
/// `\\HOST` server root authenticates via `\\HOST\IPC$`; otherwise the
/// resource is connected as-is (`\\HOST\share`).
fn net_connect_target(path: &Path) -> String {
    let s = path.to_string_lossy();
    if rfs::unc_server_root(path).is_some() {
        format!(r"{}\IPC$", s.trim_end_matches('\\'))
    } else {
        s.into_owned()
    }
}

/// Is a listing result an "access denied"? (→ attempt a network login.)
fn listing_denied(r: &Result<(Vec<Entry>, usize), ranger_core::Error>) -> bool {
    matches!(
        r,
        Err(ranger_core::Error::Io(e)) if e.kind() == std::io::ErrorKind::PermissionDenied
    )
}

/// Routes network paths to a worker. Local listings keep the
/// proven synchronous path, limiting the behavior change to the only
/// I/O likely to block for a long time (SMB, mapped drive, UNC).
fn refresh_listing(window: &MainWindow, state: &AppState, path: &Path) {
    if ranger_core::places::is_network_path(path) || rfs::is_unc_path(path) {
        let panel_idx = *state.active_panel.borrow();
        let name_filter = state.filter.borrow().clone();
        request_async_panel_listing(window, state, panel_idx, path, name_filter);
    } else {
        refresh_listing_sync(window, state, path);
    }
}

fn request_async_panel_listing(
    window: &MainWindow,
    state: &AppState,
    panel_idx: usize,
    path: &Path,
    name_filter: String,
) {
    let path = path.to_path_buf();
    let lang = state.snapshot_config().language;

    let (
        same_dir,
        show_hidden,
        sort,
        group,
        big_icon,
        ext_on,
        ext_text,
        preserved_selected,
        preserved_anchor,
    ) = {
        let panels = state.panels.borrow();
        let Some(panel) = panels.get(panel_idx) else {
            return;
        };
        let tab = &panel.tabs.tabs[panel.tabs.active];
        let same_dir = panel.displayed_path == path;
        let model = panel.rows_model.clone();
        let selected = if same_dir {
            (0..model.row_count())
                .filter_map(|i| model.row_data(i))
                .filter(|row| row.selected)
                .map(|row| row.name.to_string())
                .collect()
        } else {
            Vec::new()
        };
        let anchor = if same_dir && tab.selection_anchor >= 0 {
            model
                .row_data(tab.selection_anchor as usize)
                .map(|row| row.name.to_string())
        } else {
            None
        };
        (
            same_dir,
            tab.show_hidden,
            tab.sort,
            tab.group_mode,
            tab.preview,
            tab.ext_filter_on,
            tab.ext_filter.clone(),
            selected,
            anchor,
        )
    };

    let r#gen = state.listing_serial.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut panels = state.panels.borrow_mut();
        let Some(panel) = panels.get_mut(panel_idx) else {
            return;
        };
        panel.pending_initial = false;
        panel.pending_listing = true;
        panel.listing_gen = r#gen;
        panel.pending_select = None;
        panel.displayed_path = path.clone();
        panel.unavailable = false;
        let active = panel.tabs.active;
        panel.tabs.tabs[active].current_path = path.clone();
        if !same_dir {
            panel.reset_rows_viewport();
            panel.replace_rows(Vec::new());
            panel.hidden_count = 0;
            panel.tabs.tabs[active].selection_anchor = -1;
        }
    }
    // Only the active panel has a watcher. Refreshing a non-active
    // split must not invalidate the one used by the active view.
    if panel_idx == *state.active_panel.borrow() {
        state.watcher_gen.fetch_add(1, Ordering::SeqCst);
    }
    update_panels_ui(window, state);

    let queue = state.async_listings.clone();
    let weak = window.as_weak();
    std::thread::spawn(move || {
        let mut listing = rfs::list_dir_counted(&path, show_hidden);
        if listing_denied(&listing)
            && rfs::is_unc_path(&path)
            && ranger_core::places::net_connect_prompt(&net_connect_target(&path))
        {
            listing = rfs::list_dir_counted(&path, show_hidden);
        }
        let result = match listing {
            Ok((mut entries, hidden)) => {
                rfs::sort(&mut entries, sort.column, sort.order, group);
                apply_name_filter(&mut entries, &name_filter);
                if !ext_text.is_empty() || ext_on {
                    apply_ext_filter(&mut entries, ext_on, &ext_text);
                }
                Ok((entries, hidden))
            }
            Err(err) => {
                let denied = matches!(
                    &err,
                    ranger_core::Error::Io(e)
                        if e.kind() == std::io::ErrorKind::PermissionDenied
                );
                error!(error = %err, path = %path.display(), "async network list_dir failed");
                Err(denied)
            }
        };
        let delivery = AsyncListingDelivery {
            panel: panel_idx,
            r#gen,
            path,
            result,
            lang,
            big_icon,
            preserved_selected,
            preserved_anchor,
        };
        let queued = queue
            .lock()
            .map(|mut pending| pending.push_back(delivery))
            .is_ok();
        if queued {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_async_listing_drain();
                }
            });
        }
    });
}

fn apply_async_listing(window: &MainWindow, state: &AppState, delivery: AsyncListingDelivery) {
    let is_active = delivery.panel == *state.active_panel.borrow();
    let compact_icon_rows = state.config.borrow().compact_icon_rows_in_preview;
    let mut denied_notice = false;
    let mut count = 0usize;
    let pending_focus = {
        let mut panels = state.panels.borrow_mut();
        let Some(panel) = panels.get_mut(delivery.panel) else {
            return;
        };
        let active = panel.tabs.active;
        if !async_listing_is_current(panel, delivery.r#gen, &delivery.path) {
            return; // stale response: a more recent navigation has won
        }
        let zoom = panel.tabs.tabs[active].zoom;
        panel.pending_listing = false;
        let pending_select = panel.pending_select.take();
        let pending_focus = pending_select.clone();
        match delivery.result {
            Ok((entries, hidden_count)) => {
                let parent_display = delivery.path.display().to_string();
                let now = now_unix();
                let mut rows = entries_to_rows(
                    &entries,
                    &parent_display,
                    delivery.lang,
                    now,
                    delivery.big_icon,
                    zoom,
                    compact_icon_rows,
                );

                let selected: std::collections::HashSet<&str> = delivery
                    .preserved_selected
                    .iter()
                    .map(String::as_str)
                    .collect();
                for row in &mut rows {
                    if selected.contains(row.name.as_str()) {
                        row.selected = true;
                    }
                }
                let select_pending = pending_select.is_some();
                let anchor_name = pending_select.or(delivery.preserved_anchor);
                let anchor = anchor_name
                    .as_deref()
                    .and_then(|name| rows.iter().position(|row| row.name.as_str() == name))
                    .map(|idx| idx as i32)
                    .unwrap_or(-1);
                if select_pending {
                    for row in &mut rows {
                        row.selected = Some(row.name.as_str()) == anchor_name.as_deref();
                    }
                }

                let clip = state.clipboard.borrow();
                if matches!(clip.op, Some(ClipOp::Cut)) {
                    let cut_names: std::collections::HashSet<&str> = clip
                        .paths
                        .iter()
                        .filter(|p| p.parent() == Some(delivery.path.as_path()))
                        .filter_map(|p| p.file_name().and_then(|name| name.to_str()))
                        .collect();
                    for row in &mut rows {
                        if cut_names.contains(row.name.as_str()) {
                            row.cut = true;
                        }
                    }
                }
                count = rows.len();
                panel.replace_rows(rows);
                panel.tabs.tabs[active].selection_anchor = anchor;
                panel.tabs.tabs[active].cursor = anchor;
                panel.hidden_count = hidden_count;
                panel.unavailable = false;
            }
            Err(denied) => {
                panel.replace_rows(Vec::new());
                panel.tabs.tabs[active].selection_anchor = -1;
                panel.tabs.tabs[active].cursor = -1;
                panel.hidden_count = 0;
                panel.unavailable = !denied;
                denied_notice = denied && is_active;
            }
        }
        panel.displayed_path = delivery.path.clone();
        pending_focus
    };
    if denied_notice {
        show_notice(window, i18n::access_denied(delivery.lang));
    }
    update_panels_ui(window, state);
    if let Some(name) = pending_focus {
        schedule_focus_entry_by_name(window, state, delivery.path.clone(), name);
    }
    // The panel may have become inactive during the network listing while
    // remaining visible in the split: its previews must still start.
    request_thumbnails(state);
    request_folder_stats(state);
    request_imgmeta(state);
    if is_active {
        install_watcher(state, window, &delivery.path);
    }
    debug!(path = %delivery.path.display(), count, "async network listing applied");
}

fn async_listing_is_current(panel: &Panel, r#gen: u64, path: &Path) -> bool {
    panel.pending_listing
        && panel.listing_gen == r#gen
        && panel
            .tabs
            .tabs
            .get(panel.tabs.active)
            .is_some_and(|tab| tab.current_path == path)
}

fn refresh_listing_sync(window: &MainWindow, state: &AppState, path: &Path) {
    let cfg = state.snapshot_config();
    let lang = cfg.language;

    // Same-dir = the active panel's `displayed_path` hasn't changed.
    let active_idx = *state.active_panel.borrow();
    let same_dir = {
        let mut panels = state.panels.borrow_mut();
        // Synchronous listing is fresher than the deferred initial population.
        panels[active_idx].pending_initial = false;
        panels[active_idx].pending_listing = false;
        panels[active_idx].pending_select = None;
        let same_dir = panels[active_idx].displayed_path == path;
        if !same_dir {
            panels[active_idx].reset_rows_viewport();
        }
        same_dir
    };

    // Preservation: we read from the active panel's model if same_dir.
    let model = state.active_rows_model();
    let (preserved_selected, preserved_anchor_name) = if same_dir {
        let preserved: Vec<String> = (0..model.row_count())
            .filter_map(|i| model.row_data(i))
            .filter(|r| r.selected)
            .map(|r| r.name.to_string())
            .collect();
        let anchor_name = state.with_tabs(|book| {
            let a = book.active;
            let anchor = book.tabs[a].selection_anchor;
            if anchor >= 0 {
                model.row_data(anchor as usize).map(|r| r.name.to_string())
            } else {
                None
            }
        });
        (preserved, anchor_name)
    } else {
        (Vec::new(), None)
    };

    let show_hidden = state.with_tabs(|book| book.tabs[book.active].show_hidden);
    // Folder listing. On a NETWORK access denial (protected share, or
    // authentication required to enumerate `\\HOST`), we offer the system login
    // (like Explorer) then retry ONCE.
    let mut listing = rfs::list_dir_counted(path, show_hidden);
    if listing_denied(&listing)
        && rfs::is_unc_path(path)
        && ranger_core::places::net_connect_prompt(&net_connect_target(path))
    {
        listing = rfs::list_dir_counted(path, show_hidden);
    }
    let (mut entries, hidden_count) = match listing {
        Ok(ec) => ec,
        Err(err) => {
            error!(error = %err, path = %path.display(), "list_dir failed");
            // An access denial on a protected folder produces a notification
            // instead of being presented as an empty list. Detected via io::Error.
            let denied = matches!(
                &err,
                ranger_core::Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied
            );
            // Everything that ISN'T an access denial (missing path, network drive
            // not started, mount gone) → "unavailable" tab: we keep the
            // path + a persistent banner + auto re-check.
            let unavailable = !denied;
            if denied {
                show_notice(window, i18n::access_denied(lang));
            }
            {
                let panels = state.panels.borrow();
                panels[active_idx].replace_rows(Vec::new());
            }
            state.with_tabs_mut(|book| {
                let a = book.active;
                book.tabs[a].selection_anchor = -1;
                book.tabs[a].current_path = path.to_path_buf();
            });
            {
                let mut panels = state.panels.borrow_mut();
                panels[active_idx].displayed_path = path.to_path_buf();
                panels[active_idx].hidden_count = 0;
                panels[active_idx].unavailable = unavailable;
            }
            install_watcher(state, window, path);
            update_panels_ui(window, state);
            return;
        }
    };

    let sort_state = state.with_tabs(|book| {
        let a = book.active;
        book.tabs[a].sort
    });
    let group_mode = state.with_tabs(|book| book.tabs[book.active].group_mode);
    let big_icon = state.with_tabs(|book| book.tabs[book.active].preview);
    let zoom = state.with_tabs(|book| book.tabs[book.active].zoom);
    rfs::sort(
        &mut entries,
        sort_state.column,
        sort_state.order,
        group_mode,
    );

    // Active view's "type-ahead" filter: searches for the fragment anywhere
    // in the name (case-insensitive).
    apply_name_filter(&mut entries, &state.filter.borrow());
    // Extension filter — orthogonal to type-ahead; doesn't touch
    // folders. Read from the active tab.
    {
        let (on, txt) = state.with_tabs(|book| {
            let a = book.active;
            (book.tabs[a].ext_filter_on, book.tabs[a].ext_filter.clone())
        });
        apply_ext_filter(&mut entries, on, &txt);
    }

    let parent_display = path.display().to_string();
    let now = now_unix();
    let mut rows = entries_to_rows(
        &entries,
        &parent_display,
        lang,
        now,
        big_icon,
        zoom,
        cfg.compact_icon_rows_in_preview,
    );

    if !preserved_selected.is_empty() {
        let set: std::collections::HashSet<&str> =
            preserved_selected.iter().map(|s| s.as_str()).collect();
        for r in rows.iter_mut() {
            if set.contains(r.name.as_str()) {
                r.selected = true;
            }
        }
    }
    {
        let clip = state.clipboard.borrow();
        if matches!(clip.op, Some(ClipOp::Cut)) {
            let cut_names: std::collections::HashSet<&str> = clip
                .paths
                .iter()
                .filter(|p| p.parent() == Some(path))
                .filter_map(|p| p.file_name().and_then(|n| n.to_str()))
                .collect();
            for r in rows.iter_mut() {
                if cut_names.contains(r.name.as_str()) {
                    r.cut = true;
                }
            }
        }
    }

    let count = rows.len();
    let new_anchor: i32 = preserved_anchor_name
        .and_then(|n| rows.iter().position(|r| r.name.as_str() == n))
        .map(|p| p as i32)
        .unwrap_or(-1);

    state.with_tabs_mut(|book| {
        let a = book.active;
        let t = &mut book.tabs[a];
        t.current_path = path.to_path_buf();
        t.selection_anchor = new_anchor;
        t.cursor = new_anchor;
    });
    {
        let mut panels = state.panels.borrow_mut();
        panels[active_idx].replace_rows(rows);
        panels[active_idx].displayed_path = path.to_path_buf();
        panels[active_idx].hidden_count = hidden_count;
        panels[active_idx].unavailable = false; // listing OK → available again
    }

    install_watcher(state, window, path);
    update_panels_ui(window, state);
    request_thumbnails(state);
    request_folder_stats(state);
    request_imgmeta(state);

    debug!(path = %path.display(), count, "listing refreshed");
}

/// Flattens the current tree into [0,1] fractions of the container (gap=0: the panels
/// touch, the splitters are overlays on the seams).
fn current_geom(state: &AppState) -> Layout {
    state.layout.borrow().compute(
        Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        },
        0.0,
    )
}

/// Fractional rectangle of each panel, indexed by panel index.
fn panel_rects(geom: &Layout, n: usize) -> Vec<Rect> {
    let full = Rect {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };
    let mut rects = vec![full; n];
    for pg in &geom.panels {
        if pg.panel < n {
            rects[pg.panel] = pg.rect;
        }
    }
    rects
}

/// Converts the flattened splitters into `SplitterView` for Slint. The `idx`
/// (position in `geom.splitters`) is used to find the node again on the resize side.
fn splitter_views(geom: &Layout) -> Vec<SplitterView> {
    geom.splitters
        .iter()
        .enumerate()
        .map(|(i, sp)| {
            let vertical = matches!(sp.dir, SplitDir::Row);
            let (pos, cross_start, cross_len) = if vertical {
                (sp.rect.x, sp.rect.y, sp.rect.h)
            } else {
                (sp.rect.y, sp.rect.x, sp.rect.w)
            };
            SplitterView {
                idx: i as i32,
                vertical,
                pos,
                cross_start,
                cross_len,
            }
        })
        .collect()
}

/// Mutates the geometry **in place** (panels' fx/fy/fw/fh + splitters'
/// pos/cross) without replacing any model. Crucial during a resize: a
/// `set_splitters(new model)` would recreate the `PanelSplitterAbs` element
/// currently being dragged (losing the `pressed` state → drag interrupted after a few
/// pixels). We therefore only replace the model if the **structure** changes
/// (different number of splitters) — which never happens mid-resize.
fn push_geometry_inplace(window: &MainWindow, state: &AppState) {
    let geom = current_geom(state);
    let panels_model = window.get_panels();
    let rects = panel_rects(&geom, panels_model.row_count());
    for (i, r) in rects.iter().enumerate() {
        if let Some(mut view) = panels_model.row_data(i)
            && ((view.fx - r.x).abs() > 1e-5
                || (view.fy - r.y).abs() > 1e-5
                || (view.fw - r.w).abs() > 1e-5
                || (view.fh - r.h).abs() > 1e-5)
        {
            view.fx = r.x;
            view.fy = r.y;
            view.fw = r.w;
            view.fh = r.h;
            panels_model.set_row_data(i, view);
        }
    }

    let new_sv = splitter_views(&geom);
    let sp_model = window.get_splitters();
    if sp_model.row_count() == new_sv.len() {
        // In-place update: doesn't recreate the elements → the drag survives.
        for (i, v) in new_sv.iter().enumerate() {
            if let Some(cur) = sp_model.row_data(i)
                && ((cur.pos - v.pos).abs() > 1e-5
                    || (cur.cross_start - v.cross_start).abs() > 1e-5
                    || (cur.cross_len - v.cross_len).abs() > 1e-5)
            {
                sp_model.set_row_data(i, v.clone());
            }
        }
    } else {
        window.set_splitters(ModelRc::new(VecModel::from(new_sv)));
    }
}

/// Pushes the full list of panels to Slint as `[PanelView]`
/// (each panel carries its row model, its tabs, its current
/// path, its pre-formatted footer, etc.).
fn update_panels_ui(window: &MainWindow, state: &AppState) {
    let panels = state.panels.borrow();
    let lang = state.config.borrow().language;
    let strings = i18n::strings_for(lang);
    let prefix = strings.panel_prefix.to_string();
    let geom = current_geom(state);
    let rects = panel_rects(&geom, panels.len());
    // View footers DECOUPLED from `PanelView`: accumulated separately and pushed into
    // the parallel `panel-footers` model. Writing the selection counter
    // there (rather than into the panel struct) avoids rewriting the whole
    // `PanelView` on every rubber-band step, which would invalidate
    // `rendered-rows` and break the selection's incremental rendering.
    let (views, footers): (Vec<PanelView>, Vec<SharedString>) = panels
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let tab = &p.tabs.tabs[p.tabs.active];
            let leaf = tab_title(&tab.current_path);
            let total = p.rows_model.row_count();
            let selected = count_selected(&*p.rows_model) as usize;
            // "Hidden" reminder only when hidden items are NOT shown.
            let hidden = if tab.show_hidden { 0 } else { p.hidden_count };
            // Initial listing still in flight (async startup):
            // "…" rather than a misleading "Empty folder".
            let footer: SharedString = if p.pending_initial || p.pending_listing {
                "…".into()
            } else {
                i18n::footer_text(lang, total, selected, hidden).into()
            };
            // Tabs: titles + FLAT geometry along the bar's main
            // axis (imposed width + cumulative offset — not uniform in Y
            // for a vertical bar). Source of truth for the drag on the Slint side.
            let tabs_infos: Vec<TabInfo> = {
                let titles: Vec<String> = p
                    .tabs
                    .tabs
                    .iter()
                    .map(|t| tab_title(&t.current_path))
                    .collect();
                let geo = tab_layout_for(p.tab_bar_mode, &titles);
                titles
                    .into_iter()
                    .zip(geo)
                    .zip(&p.tabs.tabs)
                    .map(|((title, (width, offset)), t)| TabInfo {
                        title: title.into(),
                        width,
                        offset,
                        // Full path → tooltip on hover.
                        path: t.current_path.display().to_string().into(),
                    })
                    .collect()
            };
            let r = rects[i];
            // Total width of VISIBLE columns (px) — precise (accounts for
            // visibility AND the fixed resolution/depth columns). Drives the
            // horizontal scroll on the Slint side (`content-width`).
            let content_w: f32 = {
                const COL_GAP: f32 = 8.0; // MUST == Tokens.col-gap (slint)
                // Widths of visible columns and the gaps between them (n - 1),
                // with no gap after the last column.
                let widths: Vec<f32> = p
                    .columns
                    .iter()
                    .filter(|c| c.visible)
                    .map(|c| col_width(&p.columns, &c.id))
                    .collect();
                let gaps = COL_GAP * widths.len().saturating_sub(1) as f32;
                widths.iter().sum::<f32>() + gaps
            };
            let view = PanelView {
                rows: ModelRc::from(p.rows_model.clone()),
                rendered_rows: p.rendered_rows_model.clone(),
                rows_revision: p.rows_revision.get(),
                viewport_reset_gen: p.viewport_reset_gen.get(),
                tabs: ModelRc::new(VecModel::from(tabs_infos)),
                current_path: tab.current_path.display().to_string().into(),
                total_count: total as i32,
                can_go_back: tab.history.can_back(),
                can_go_forward: tab.history.can_forward(),
                sort_column: tab.sort.column.code().into(),
                sort_asc: matches!(tab.sort.order, SortOrder::Asc),
                active_tab_idx: p.tabs.active as i32,
                title: format!("{prefix}{}  ·  {leaf}", i + 1).into(),
                fx: r.x,
                fy: r.y,
                fw: r.w,
                fh: r.h,
                crumbs: ModelRc::new(VecModel::from(breadcrumbs(&tab.current_path))),
                col_name_w: col_width(&p.columns, "name"),
                col_path_w: col_width(&p.columns, "path"),
                col_size_w: col_width(&p.columns, "size"),
                col_modified_w: col_width(&p.columns, "modified"),
                col_ext_w: col_width(&p.columns, "ext"),
                col_resolution_w: col_width(&p.columns, "resolution"),
                col_depth_w: col_width(&p.columns, "depth"),
                col_age_w: col_width(&p.columns, "age"),
                columns: ModelRc::new(VecModel::from({
                    // Cumulative offsets on VISIBLE columns (px), for the
                    // GUI-side drag target computation (without absolute-position).
                    // Includes the inter-column gap (= `Tokens.col-gap`) so that
                    // offset-space matches screen-space (otherwise the
                    // preview drifts by ~8px per column crossed).
                    const COL_GAP: f32 = 8.0; // MUST == Tokens.col-gap (slint)
                    let mut off = 0.0_f32;
                    let mut v = Vec::new();
                    for c in p.columns.iter().filter(|c| c.visible) {
                        v.push(column_info(&strings, c, off));
                        off += col_width(&p.columns, &c.id) + COL_GAP;
                    }
                    v
                })),
                columns_menu: ModelRc::new(VecModel::from(
                    p.columns
                        .iter()
                        .map(|c| column_info(&strings, c, 0.0))
                        .collect::<Vec<_>>(),
                )),
                preview_mode: tab.preview,
                content_width: content_w,
                show_hidden: tab.show_hidden,
                group_mode: tab.group_mode.code().into(),
                cursor_row: tab.cursor,
                scroll_gen: tab.scroll_gen,
                unavailable: p.unavailable, // folder unreachable → banner
                ext_filter_on: tab.ext_filter_on,
                ext_filter: tab.ext_filter.as_str().into(),
                tab_bar_mode: p.tab_bar_mode as i32, // 0 top · 1 left · 2 right
                vbar_user_w: p.vbar_user_w,          // handle width, 0 = auto
            };
            (view, footer)
        })
        .unzip();
    let active = *state.active_panel.borrow() as i32;
    let can_add = panels.len() < MAX_PANELS;
    // Active panel's "extension filter" mode → drives the keyboard
    // routing (Backspace/Escape) on the Slint side, shared with type-ahead.
    let active_ext_on = panels
        .get(active as usize)
        .map(|p| p.tabs.tabs[p.tabs.active].ext_filter_on)
        .unwrap_or(false);
    drop(panels);
    // IN-PLACE update if the number of panels is unchanged (active view
    // switch, refresh, sort, navigation…): each `PanelView` is mutated via
    // `set_row_data` instead of replacing the whole model. Otherwise `set_panels`
    // recreates ALL the `PanelComponent`s → an in-progress gesture (file drag,
    // middle-scroll, rubber-band) started in a NON-active view would die the
    // instant it becomes active. The model is only replaced for a
    // STRUCTURAL change (split/close → the number of panels changes).
    let existing = window.get_panels();
    if existing.row_count() == views.len() {
        for (i, v) in views.into_iter().enumerate() {
            existing.set_row_data(i, v);
        }
    } else {
        window.set_panels(ModelRc::new(VecModel::from(views)));
    }
    // View footers in their PARALLEL model. In-place update when the
    // panel count is unchanged → the model instance is preserved, so
    // `push_active_footer` keeps writing into the same live model.
    let existing_footers = window.get_panel_footers();
    if existing_footers.row_count() == footers.len() {
        for (i, f) in footers.into_iter().enumerate() {
            existing_footers.set_row_data(i, f);
        }
    } else {
        window.set_panel_footers(ModelRc::new(VecModel::from(footers)));
    }
    window.set_splitters(ModelRc::new(VecModel::from(splitter_views(&geom))));
    window.set_active_panel_idx(active);
    window.set_active_ext_filter_on(active_ext_on);
    window.set_can_add_panel(can_add);
    // Is there at least one "unavailable" panel? Drives the auto
    // re-check Timer on the Slint side.
    let any_unavail = state.panels.borrow().iter().any(|p| p.unavailable);
    window.set_any_unavailable(any_unavail);
    // This central path is taken by structural mutations and
    // tab state changes. The comparison is purely in-memory, so the
    // title stays reactive without hooking any I/O into UI interactions.
    update_window_title(window, state);
}

/// Updates the window's OS title (taskbar / Alt-Tab):.
/// "{workspace} — Ranger[*]" (em dash U+2014) if the current state comes from
/// a named workspace, otherwise "Ranger". The asterisk is fed by THE SAME
/// source of truth as the golden Update button (`current_workspace_is_dirty`).
/// The workspace name comes first so it survives taskbar label
/// truncation (from the end).
fn update_window_title(window: &MainWindow, state: &AppState) {
    let brand = i18n::strings_for(state.config.borrow().language).app_title;
    let current = state.current_workspace.borrow().clone();
    let title = match current.as_deref() {
        Some(name) if !name.trim().is_empty() => {
            let marker = if current_workspace_is_dirty(state) {
                "*"
            } else {
                ""
            };
            format!("{name} — {brand}{marker}")
        }
        _ => brand.to_string(),
    };
    window.set_window_title(title.into());
}

// Configurable shortcuts ----------

/// Display label of a canonical key (arrows handled separately via `kind`).
fn cap_label(key: &str) -> &str {
    match key {
        "Backslash" => "\\",
        "Comma" => ",",
        "PageUp" => "PgUp",
        "PageDown" => "PgDn",
        other => other,
    }
}

/// Display "caps" of a serialized chord (empty if unassigned/unreadable).
fn chord_to_caps(chord: &str) -> Vec<ShortcutCap> {
    let mut caps = Vec::new();
    let Some(c) = Chord::parse(chord) else {
        return caps;
    };
    if c.ctrl {
        caps.push(ShortcutCap {
            label: "Ctrl".into(),
            kind: 0,
        });
    }
    if c.alt {
        caps.push(ShortcutCap {
            label: "Alt".into(),
            kind: 0,
        });
    }
    if c.shift {
        caps.push(ShortcutCap {
            label: "Shift".into(),
            kind: 0,
        });
    }
    let (label, kind) = match c.key.as_str() {
        "ArrowUp" => (String::new(), 1),
        "ArrowDown" => (String::new(), 2),
        "ArrowLeft" => (String::new(), 3),
        "ArrowRight" => (String::new(), 4),
        k => (cap_label(k).to_string(), 0),
    };
    caps.push(ShortcutCap {
        label: label.into(),
        kind,
    });
    caps
}

/// Builds the shortcut groups (filtered by the current search) +
/// a boolean "at least one override exists" (enables "Reset all").
fn build_shortcut_groups(state: &AppState) -> (Vec<ShortcutGroup>, bool) {
    let lang = state.config.borrow().language;
    let overrides = state.config.borrow().shortcut_overrides.clone();
    let has_overrides = !overrides.is_empty();
    let km = state.keymap.borrow();
    let filter = state.shortcut_filter.borrow().to_lowercase();

    let mut groups: Vec<ShortcutGroup> = Vec::new();
    let mut cur_code: Option<&'static str> = None;
    let mut cur_name = String::new();
    let mut cur_rows: Vec<ShortcutRow> = Vec::new();
    for a in shortcuts::ACTIONS {
        let code = a.group.code();
        if cur_code != Some(code) {
            if !cur_rows.is_empty() {
                groups.push(ShortcutGroup {
                    name: cur_name.as_str().into(),
                    rows: ModelRc::new(VecModel::from(std::mem::take(&mut cur_rows))),
                });
            }
            cur_code = Some(code);
            cur_name = i18n::shortcut_group_name(lang, code);
        }
        let name = i18n::shortcut_action_name(lang, a.id);
        let chord = km.chord_of(a.id);
        if !filter.is_empty()
            && !name.to_lowercase().contains(&filter)
            && !chord.to_lowercase().contains(&filter)
        {
            continue;
        }
        cur_rows.push(ShortcutRow {
            action_id: a.id.into(),
            name: name.into(),
            caps: ModelRc::new(VecModel::from(chord_to_caps(chord))),
            assigned: !chord.is_empty(),
            overridden: overrides.contains_key(a.id),
        });
    }
    if !cur_rows.is_empty() {
        groups.push(ShortcutGroup {
            name: cur_name.as_str().into(),
            rows: ModelRc::new(VecModel::from(cur_rows)),
        });
    }
    (groups, has_overrides)
}

/// Pushes the shortcut list + the overrides state to the UI.
fn push_shortcuts_ui(window: &MainWindow, state: &AppState) {
    let (groups, has_overrides) = build_shortcut_groups(state);
    window.set_shortcut_groups(ModelRc::new(VecModel::from(groups)));
    window.set_shortcut_has_overrides(has_overrides);
    // Context menus reflect the SAME effective map (dynamic).
    push_menu_shortcuts(window, state);
}

/// Renders a serialized chord ("Ctrl+Shift+N") into a SHORT label for a context
/// menu. Empty if the chord is empty (unassigned action → no shortcut
/// displayed). Consistent text style: "Ctrl+"/"Alt+"/"Shift+" + key.
fn chord_display(chord: &str) -> String {
    let Some(c) = shortcuts::Chord::parse(chord) else {
        return String::new();
    };
    let mut s = String::new();
    if c.ctrl {
        s.push_str("Ctrl+");
    }
    if c.alt {
        s.push_str("Alt+");
    }
    if c.shift {
        s.push_str("Shift+");
    }
    s.push_str(match c.key.as_str() {
        "Delete" => "Del",
        "Backslash" => "\\",
        "Comma" => ",",
        "ArrowUp" => "↑",
        "ArrowDown" => "↓",
        "ArrowLeft" => "←",
        "ArrowRight" => "→",
        "PageUp" => "PgUp",
        "PageDown" => "PgDn",
        k => k,
    });
    s
}

/// Pushes the EFFECTIVE shortcut labels displayed in the menus and the
/// rail tooltips — recomputed on every map change (rebind,
/// reset, unassignment) via `push_shortcuts_ui`.
fn push_menu_shortcuts(window: &MainWindow, state: &AppState) {
    let km = state.keymap.borrow();
    let d = |id: &str| SharedString::from(chord_display(km.chord_of(id)));
    window.set_menu_shortcuts(MenuShortcuts {
        open: d("open"),
        terminal: d("terminal"),
        copy: d("copy"),
        cut: d("cut"),
        paste: d("paste"),
        rename: d("rename"),
        delete: d("delete"),
        properties: d("properties"),
        new_folder: d("new-folder"),
        new_file: d("new-file"),
        split_side: d("split-side"),
        split_stack: d("split-stack"),
        tab_reopen_closed: d("tab-reopen-closed"),
        open_settings: d("open-settings"),
        open_workspaces: d("open-workspaces"),
    });
}

/// Applies an override (or removes it if it equals the default) + rebuilds the map.
fn apply_shortcut_override(state: &AppState, action_id: &str, chord: &str) {
    let default = shortcuts::ACTIONS
        .iter()
        .find(|a| a.id == action_id)
        .map(|a| a.default)
        .unwrap_or("");
    let id = action_id.to_string();
    let chord = chord.to_string();
    state.persist_config(|c| {
        if chord == default {
            c.shortcut_overrides.remove(&id);
        } else {
            c.shortcut_overrides.insert(id.clone(), chord.clone());
        }
    });
    state.rebuild_keymap();
}

/// Updates the active panel's footer ("N items · M selected") without
/// rebuilding the whole UI. The logical model and its rendered sub-model
/// remain the same `Rc`s, so no geometry or scroll position
/// is lost. `selected` is the counter already computed by the operation.
fn push_active_footer(window: &MainWindow, state: &AppState, selected: i32) {
    let lang = state.config.borrow().language;
    let idx = *state.active_panel.borrow();
    let (total, hidden) = {
        let panels = state.panels.borrow();
        match panels.get(idx) {
            Some(p) => {
                let show_hidden = p.tabs.tabs[p.tabs.active].show_hidden;
                (
                    p.rows_model.row_count(),
                    if show_hidden { 0 } else { p.hidden_count },
                )
            }
            None => return,
        }
    };
    let footer = i18n::footer_text(lang, total, selected.max(0) as usize, hidden);
    // We write ONLY into the parallel view-footers model: NEVER
    // touch the `panels` model here. Rewriting a `PanelView` (even just for the
    // label) would invalidate the Repeater's `rendered-rows` model property and,
    // on every rubber-band step, selection `row_changed`s would be
    // deferred until a re-list (scroll) — resulting in "skipped" entries.
    let footers = window.get_panel_footers();
    if let Some(cur) = footers.row_data(idx)
        && cur.as_str() != footer.as_str()
    {
        footers.set_row_data(idx, footer.into());
    }
}

// Previews / thumbnails ----------

/// Decoding job for a thumbnail, handed to the background worker.
struct ThumbJob {
    path: PathBuf,
    kind: FileKind,
    /// SVGs are loaded by Slint (resvg) on the event loop; other
    /// formats go through `generate_thumb` on the worker.
    svg: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThumbLocation {
    panel: usize,
    row: usize,
    row_count: usize,
}

struct ThumbRequest {
    job: ThumbJob,
    locations: Vec<ThumbLocation>,
}

struct ScheduledThumb {
    job: ThumbJob,
    locations: Vec<ThumbLocation>,
    serial: i32,
}

struct InFlightThumb {
    locations: Vec<ThumbLocation>,
    serial: i32,
}

struct ThumbWork {
    job: ThumbJob,
    serial: i32,
}

impl std::ops::Deref for ThumbWork {
    type Target = ThumbJob;

    fn deref(&self) -> &Self::Target {
        &self.job
    }
}

/// Sort key of the queue. Visible rows come before the window's
/// neighborhood, which itself comes before the rest of the folder. `distance` stabilizes the order
/// around the visible zone; `panel` and `row` make ties deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ThumbPriority {
    tier: u8,
    distance: usize,
    panel: usize,
    row: usize,
}

#[derive(Default)]
struct ThumbQueue {
    pending: HashMap<PathBuf, ScheduledThumb>,
    ready: BinaryHeap<Reverse<(ThumbPriority, i32, PathBuf)>>,
    /// Current locations of an in-progress decode. They can be enriched
    /// if another view displays the same path while the worker is working.
    in_flight: HashMap<PathBuf, InFlightThumb>,
    next_serial: i32,
}

/// Priority queue shared with the worker. Unlike a FIFO channel,
/// it can promote newly visible rows and remove paths
/// from a folder that was left before their decoding starts.
struct ThumbScheduler {
    queue: Mutex<ThumbQueue>,
    /// Small state independent of the heavy queue: the scroll callback can
    /// never afford to wait for a heap of thousands of entries to be rebuilt.
    viewports: Mutex<HashMap<usize, (usize, usize)>>,
    priorities_dirty: AtomicBool,
    wake: Condvar,
    started: AtomicBool,
}

impl ThumbScheduler {
    fn new() -> Self {
        Self {
            queue: Mutex::new(ThumbQueue::default()),
            viewports: Mutex::new(HashMap::new()),
            priorities_dirty: AtomicBool::new(false),
            wake: Condvar::new(),
            started: AtomicBool::new(false),
        }
    }

    fn start_once(&self) -> bool {
        self.started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn priority_for(
        scheduled: &ScheduledThumb,
        viewports: &HashMap<usize, (usize, usize)>,
    ) -> ThumbPriority {
        scheduled
            .locations
            .iter()
            .map(|loc| {
                let fallback = ThumbPriority {
                    tier: 2,
                    distance: loc.row,
                    panel: loc.panel,
                    row: loc.row,
                };
                let Some(&(raw_first, raw_last)) = viewports.get(&loc.panel) else {
                    return fallback;
                };
                if loc.row_count == 0 {
                    return fallback;
                }
                let first = raw_first.min(loc.row_count - 1);
                let last = raw_last.max(first).min(loc.row_count - 1);
                if loc.row >= first && loc.row <= last {
                    return ThumbPriority {
                        tier: 0,
                        distance: loc.row - first,
                        panel: loc.panel,
                        row: loc.row,
                    };
                }
                let distance = if loc.row < first {
                    first - loc.row
                } else {
                    loc.row - last
                };
                // Preloads one viewport height on each side. This
                // absorbs a normal scroll without delaying a far-away destination.
                let visible_len = last - first + 1;
                ThumbPriority {
                    tier: if distance <= visible_len { 1 } else { 2 },
                    distance,
                    panel: loc.panel,
                    row: loc.row,
                }
            })
            .min()
            .unwrap_or(ThumbPriority {
                tier: 2,
                distance: usize::MAX,
                panel: usize::MAX,
                row: usize::MAX,
            })
    }

    fn rebuild_ready(queue: &mut ThumbQueue, viewports: &HashMap<usize, (usize, usize)>) {
        queue.ready = queue
            .pending
            .iter()
            .map(|(path, scheduled)| {
                Reverse((
                    Self::priority_for(scheduled, viewports),
                    scheduled.serial,
                    path.clone(),
                ))
            })
            .collect();
    }

    fn ensure_ready(&self, queue: &mut ThumbQueue) {
        // An update can arrive during the rebuild. The loop then re-reads
        // the most recent viewport before letting the next job go.
        while self.priorities_dirty.swap(false, Ordering::AcqRel) {
            let viewports = self
                .viewports
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            Self::rebuild_ready(queue, &viewports);
        }
    }

    /// Replaces the full request with that of the panels currently in
    /// preview mode. Paths shared across several views are decoded
    /// only once, while keeping each of their positions for sorting.
    fn replace_pending(&self, requests: Vec<ThumbRequest>) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::mem::take(&mut queue.pending);
        let mut next: HashMap<PathBuf, ScheduledThumb> = HashMap::new();

        for request in requests {
            let path = request.job.path.clone();
            if let Some(in_flight) = queue.in_flight.get_mut(&path) {
                for location in request.locations {
                    if !in_flight.locations.contains(&location) {
                        in_flight.locations.push(location);
                    }
                }
                continue;
            }
            if let Some(existing) = next.get_mut(&path) {
                existing.locations.extend(request.locations);
                continue;
            }
            let serial = old.get(&path).map(|s| s.serial).unwrap_or_else(|| {
                let serial = queue.next_serial;
                queue.next_serial = queue.next_serial.wrapping_add(1).max(1);
                serial
            });
            next.insert(
                path,
                ScheduledThumb {
                    job: request.job,
                    locations: request.locations,
                    serial,
                },
            );
        }

        queue.pending = next;
        queue.ready.clear();
        self.priorities_dirty.store(true, Ordering::Release);
        drop(queue);
        self.wake.notify_all();
    }

    /// Adds a few requests that became visible without re-scanning/rebuilding
    /// the whole gallery. Paths already pending/in-flight are simply
    /// enriched with their new location.
    fn merge_pending(&self, requests: Vec<ThumbRequest>) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut priorities_changed = false;
        for request in requests {
            let path = request.job.path.clone();
            if let Some(in_flight) = queue.in_flight.get_mut(&path) {
                for location in request.locations {
                    if !in_flight.locations.contains(&location) {
                        in_flight.locations.push(location);
                    }
                }
                continue;
            }
            if let Some(scheduled) = queue.pending.get_mut(&path) {
                for location in request.locations {
                    if !scheduled.locations.contains(&location) {
                        scheduled.locations.push(location);
                        priorities_changed = true;
                    }
                }
                continue;
            }
            let serial = queue.next_serial;
            queue.next_serial = queue.next_serial.wrapping_add(1).max(1);
            queue.pending.insert(
                path,
                ScheduledThumb {
                    job: request.job,
                    locations: request.locations,
                    serial,
                },
            );
            priorities_changed = true;
        }
        if priorities_changed {
            queue.ready.clear();
            self.priorities_dirty.store(true, Ordering::Release);
        }
        drop(queue);
        if priorities_changed {
            self.wake.notify_all();
        }
    }

    /// Updates the only piece of information that varies during a scroll. No
    /// filesystem access or Slint model access happens here.
    fn update_viewport(&self, panel: usize, first: i32, last: i32) {
        let mut viewports = self.viewports.lock().unwrap_or_else(|e| e.into_inner());
        let changed = if first < 0 || last < first {
            viewports.remove(&panel).is_some()
        } else {
            let range = (first as usize, last as usize);
            if viewports.get(&panel).copied() == Some(range) {
                false
            } else {
                viewports.insert(panel, range);
                true
            }
        };
        drop(viewports);
        if changed {
            // The O(n) recomputation is deliberately deferred to the worker: the
            // scroll callback stays O(1), regardless of the folder's size.
            self.priorities_dirty.store(true, Ordering::Release);
            self.wake.notify_all();
        }
    }

    fn take_next(&self) -> ThumbWork {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            self.ensure_ready(&mut queue);
            while let Some(Reverse((_, _, path))) = queue.ready.pop() {
                if let Some(scheduled) = queue.pending.remove(&path) {
                    let serial = scheduled.serial;
                    queue.in_flight.insert(
                        path,
                        InFlightThumb {
                            locations: scheduled.locations,
                            serial,
                        },
                    );
                    return ThumbWork {
                        job: scheduled.job,
                        serial,
                    };
                }
            }
            queue = self.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(test)]
    fn try_take_next(&self) -> Option<ThumbWork> {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        self.ensure_ready(&mut queue);
        while let Some(Reverse((_, _, path))) = queue.ready.pop() {
            if let Some(scheduled) = queue.pending.remove(&path) {
                let serial = scheduled.serial;
                queue.in_flight.insert(
                    path,
                    InFlightThumb {
                        locations: scheduled.locations,
                        serial,
                    },
                );
                return Some(ThumbWork {
                    job: scheduled.job,
                    serial,
                });
            }
        }
        None
    }

    /// Acknowledges a UI success or a decode failure, then frees the worker.
    fn complete(&self, path: &Path, serial: i32) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if queue
            .in_flight
            .get(path)
            .is_some_and(|in_flight| in_flight.serial == serial)
        {
            queue.in_flight.remove(path);
        }
        drop(queue);
        self.wake.notify_all();
    }

    fn wait_until_complete(&self, path: &Path, serial: i32) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        while queue
            .in_flight
            .get(path)
            .is_some_and(|in_flight| in_flight.serial == serial)
        {
            queue = self.wake.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn in_flight_locations(&self, path: &Path, serial: i32) -> Vec<ThumbLocation> {
        self.queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .in_flight
            .get(path)
            .filter(|in_flight| in_flight.serial == serial)
            .map(|in_flight| in_flight.locations.clone())
            .unwrap_or_default()
    }

    /// Drops queued/in-flight generations for paths whose contents changed.
    /// A worker may still finish an old decode, but its serial can no longer
    /// match a later request for the same path, so the UI safely discards it.
    fn invalidate_paths(&self, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        for path in paths {
            changed |= queue.pending.remove(path).is_some();
            changed |= queue.in_flight.remove(path).is_some();
        }
        if changed {
            queue.ready.clear();
            self.priorities_dirty.store(true, Ordering::Release);
        }
        drop(queue);
        if changed {
            self.wake.notify_all();
        }
    }
}

/// **In-memory** LRU cache (full path → image), bounded by entry count.
/// Session only: nothing is written to disk (privacy constraint).
// Paths whose contents may have changed are evicted explicitly, while
// unrelated folders stay hot.
struct ThumbLru {
    map: HashMap<String, Image>,
    order: VecDeque<String>,
    cap: usize,
}

impl ThumbLru {
    fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap,
        }
    }
    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_string());
    }
    fn get(&mut self, key: &str) -> Option<Image> {
        let img = self.map.get(key).cloned()?;
        self.touch(key);
        Some(img)
    }
    /// Test without cloning or promoting the image. Global scans can thus
    /// skip off-screen rows already in cache without skewing LRU recency:
    /// only textures actually reused on screen call `get`.
    fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
    fn put(&mut self, key: String, img: Image) {
        if !self.map.contains_key(&key)
            && self.map.len() >= self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.map.remove(&old);
        }
        self.map.insert(key.clone(), img);
        self.touch(&key);
    }

    fn remove_path(&mut self, path: &Path) {
        let key = path.to_string_lossy();
        self.map.remove(key.as_ref());
        if let Some(position) = self.order.iter().position(|item| item == key.as_ref()) {
            self.order.remove(position);
        }
    }
}

/// Invalidates only the content paths touched by an operation or watcher
/// event. Untouched thumbnails stay hot in the bounded LRU. The scheduler is
/// invalidated at the same time so an older asynchronous decode cannot restore
/// a stale texture after a path has been renamed or replaced.
fn invalidate_thumbnail_paths(state: &AppState, paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let unique: HashSet<PathBuf> = paths.iter().cloned().collect();
    {
        let mut cache = state.thumb_cache.borrow_mut();
        for path in &unique {
            cache.remove_path(path);
        }
    }
    let paths: Vec<PathBuf> = unique.into_iter().collect();
    state.thumb_scheduler.invalidate_paths(&paths);
}

fn drain_thumbnail_invalidations(state: &AppState) -> bool {
    let paths: Vec<PathBuf> = state
        .thumb_invalidations
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .drain()
        .collect();
    let changed = !paths.is_empty();
    invalidate_thumbnail_paths(state, &paths);
    changed
}

fn active_thumbnail_paths(state: &AppState) -> Vec<PathBuf> {
    let active = *state.active_panel.borrow();
    let panels = state.panels.borrow();
    let Some(panel) = panels.get(active) else {
        return Vec::new();
    };
    let dir = &panel.tabs.tabs[panel.tabs.active].current_path;
    (0..panel.rows_model.row_count())
        .filter_map(|index| panel.rows_model.row_data(index))
        // `.lnk` rows can preview an image/audio/video/PDF target even though the
        // shortcut itself is not classified as preview-capable.
        .filter(|row| row.preview_capable || row.ext.eq_ignore_ascii_case("lnk"))
        .map(|row| dir.join(row.name.as_str()))
        .collect()
}

/// Builds a `slint::Image` from a decoded thumbnail (RGBA8).
fn image_from_thumb(t: &Thumbnail) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(t.width, t.height);
    buf.make_mut_bytes().copy_from_slice(&t.rgba);
    Image::from_rgba8(buf)
}

/// Selects the thumbnail source based on the platform and type. On
/// **Windows**, audio/video/PDF fallbacks go through native APIs
/// (`winthumb`): OS providers → standalone (no ffmpeg/poppler) and
/// **no console window**. Images remain decoded in Rust (`image`).
/// On **Linux**, everything goes through `thumbnail::generate` (images and
/// MP3/FLAC covers are in-process; video/PDF use their best-effort CLIs).
fn generate_thumb(path: &Path, kind: FileKind, max_px: u32) -> Option<Thumbnail> {
    #[cfg(windows)]
    {
        // A `.lnk` shortcut → the shell thumbnail API on the link itself: the
        // shell resolves the target and returns ITS thumbnail (an image shortcut
        // previews the image) or its icon, exactly like Explorer.
        if path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("lnk"))
            .unwrap_or(false)
        {
            return crate::winthumb::shell_thumbnail(path, max_px);
        }
        match kind {
            // MP3/FLAC: embedded cover first; the shell can supply artwork for
            // tags/providers outside Ranger's deliberately small parsers.
            FileKind::Audio => {
                return thumbnail::generate(path, kind, max_px)
                    .or_else(|| crate::winthumb::shell_thumbnail(path, max_px));
            }
            // Video: shell thumbnail API (frame via OS providers).
            FileKind::Video => return crate::winthumb::shell_thumbnail(path, max_px),
            // PDF: Windows's built-in PDF engine (WinRT) → no external binary.
            FileKind::Document => return crate::winthumb::pdf_thumbnail(path, max_px),
            // Image: Rust decoding first (common formats plus bounded embedded
            // PSD/Affinity previews); on failure, fall back to the shell API.
            FileKind::Image => {
                return thumbnail::generate(path, kind, max_px)
                    .or_else(|| crate::winthumb::shell_thumbnail(path, max_px));
            }
            _ => {}
        }
    }
    thumbnail::generate(path, kind, max_px)
}

/// Size of the thumbnail worker pool. The dominant cost (image decoding, or
/// launching ffmpeg/shell) is CPU-bound and parallelizable per file. Capped at
/// **4**: beyond that, applying the textures (single UI thread), memory
/// bandwidth, and I/O cap the gain, a visible folder shows only
/// ~15 rows, and N simultaneous full-resolution decodes bound the peak
/// memory (~4x the peak of a single image). We also leave one core for the UI thread on
/// small machines (`- 1`). Safe fallback to 1 if introspection fails.
fn thumb_worker_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .saturating_sub(1)
        .clamp(1, 4)
}

/// Background worker: picks the best job at the moment it becomes available,
/// then waits for it to be applied by the event loop before picking the next one.
/// This way, a scroll that happens during decoding immediately influences that choice.
/// Several instances run in parallel (see `thumb_worker_count`): each
/// takes a DISTINCT job (the scheduler removes the path from `pending` under
/// the lock) and waits only for ITS OWN completion — the others keep decoding.
fn spawn_thumb_worker(scheduler: Arc<ThumbScheduler>, weak: slint::Weak<MainWindow>) {
    // 256 px: covers the tallest row height at max zoom (≈220 px) without blur.
    const MAX_PX: u32 = 256;
    std::thread::spawn(move || {
        loop {
            let work = scheduler.take_next();
            let serial = work.serial;
            let job = work.job;
            let wait_path = job.path.clone();
            let path_str = job.path.to_string_lossy().to_string();
            let weak_for_ui = weak.clone();
            let scheduler_for_ui = scheduler.clone();

            if job.svg {
                let ui_path = job.path;
                let posted = slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak_for_ui.upgrade() {
                        match Image::load_from_path(&ui_path) {
                            Ok(img) => w.invoke_thumb_ready(path_str.into(), serial, img),
                            Err(_) => scheduler_for_ui.complete(&ui_path, serial),
                        }
                    } else {
                        scheduler_for_ui.complete(&ui_path, serial);
                    }
                });
                if posted.is_err() {
                    scheduler.complete(&wait_path, serial);
                } else {
                    scheduler.wait_until_complete(&wait_path, serial);
                }
                continue;
            }

            let Some(thumb) = generate_thumb(&job.path, job.kind, MAX_PX) else {
                scheduler.complete(&wait_path, serial);
                continue;
            };
            let completion_path = wait_path.clone();
            let posted = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak_for_ui.upgrade() {
                    w.invoke_thumb_ready(path_str.into(), serial, image_from_thumb(&thumb));
                } else {
                    scheduler_for_ui.complete(&completion_path, serial);
                }
            });
            if posted.is_err() {
                scheduler.complete(&wait_path, serial);
            } else {
                scheduler.wait_until_complete(&wait_path, serial);
            }
        }
    });
}

/// For each panel in preview mode: applies the cached thumbnails to
/// image/audio-cover/video/PDF/SVG rows, then atomically replaces the request for the
/// missing ones. To be called after a (re)population of fresh rows.
fn thumbnail_kind_for_row(kind: i32, ext: &str) -> Option<(FileKind, bool)> {
    let svg = kind == 6 && ext.eq_ignore_ascii_case("svg");
    let file_kind = match kind {
        4 if ext.eq_ignore_ascii_case("mp3") || ext.eq_ignore_ascii_case("flac") => FileKind::Audio,
        6 => FileKind::Image,
        7 => FileKind::Video,
        5 if ext.eq_ignore_ascii_case("pdf") => FileKind::Document,
        _ => return None,
    };
    Some((file_kind, svg))
}

/// Merges a local request before touching the shared scheduler. The same
/// image can appear in several panels; all its locations are
/// kept, but the decoding stays unique.
fn push_thumbnail_request(
    requests: &mut HashMap<PathBuf, ThumbRequest>,
    path: PathBuf,
    kind: FileKind,
    svg: bool,
    location: ThumbLocation,
) {
    if let Some(existing) = requests.get_mut(&path) {
        if !existing.locations.contains(&location) {
            existing.locations.push(location);
        }
    } else {
        requests.insert(
            path.clone(),
            ThumbRequest {
                job: ThumbJob { path, kind, svg },
                locations: vec![location],
            },
        );
    }
}

fn request_thumbnails(state: &AppState) {
    let mut requests: HashMap<PathBuf, ThumbRequest> = HashMap::new();
    let panels = state.panels.borrow();
    for (panel_idx, panel) in panels.iter().enumerate() {
        let tab = &panel.tabs.tabs[panel.tabs.active];
        if !tab.preview {
            continue;
        }
        let dir = tab.current_path.clone();
        let model = &panel.rows_model;
        for i in 0..model.row_count() {
            let Some(mut row) = model.row_data(i) else {
                continue;
            };
            if row.thumbnail.size().width > 0 {
                if row.rendered {
                    continue; // texture already attached to a visible row
                }
                // A rebuilt geometry may have briefly kept a
                // an off-screen texture. We detach it, then actually check
                // the LRU instead of assuming it still lives there.
                row.thumbnail = Image::default();
                model.set_row_data(i, row.clone());
            }
            let full = dir.join(row.name.as_str());
            let key = full.to_string_lossy().to_string();

            // Shared source of truth with the row geometry: only these
            // types are capable of receiving a content texture.
            let (kind, svg) = match thumbnail_kind_for_row(row.kind, row.ext.as_str()) {
                Some(k) => k,
                // A `.lnk` to an image/audio/video/PDF previews the TARGET (the shell
                // thumbnail API resolves the link). Only `.lnk` rows pay the lookup.
                None if row.ext.eq_ignore_ascii_case("lnk") => match lnk_thumbnail_kind(&full) {
                    Some(kind) => (kind, false),
                    None => continue,
                },
                None => continue,
            };
            if row.rendered {
                if let Some(img) = state.thumb_cache.borrow_mut().get(&key) {
                    row.thumbnail = img;
                    model.set_row_data(i, row);
                    continue;
                }
            } else if state.thumb_cache.borrow().contains(&key) {
                // Don't clone/promote every off-screen image: they
                // remain available, but LRU recency reflects only the actually
                // visible usages. Above all, no unnecessary re-decoding.
                continue;
            }
            push_thumbnail_request(
                &mut requests,
                full,
                kind,
                svg,
                ThumbLocation {
                    panel: panel_idx,
                    row: i,
                    row_count: model.row_count(),
                },
            );
        }
    }
    drop(panels);
    state
        .thumb_scheduler
        .replace_pending(requests.into_values().collect());
}

/// Adjusts a panel's delegate window. The filtered model then automatically
/// relays selection, cut, metadata, and thumbnails of the admitted rows.
/// Returns the STRICTLY visible range for the worker's priority.
fn update_panel_render_window(
    state: &AppState,
    panel_idx: usize,
    top: f32,
    height: f32,
) -> (i32, i32) {
    let top = top.max(0.0);
    let height = height.max(0.0);
    let Some((model, dir, preview, old_first, old_end, new_first, new_end, visible)) =
        state.panels.try_borrow().ok().and_then(|panels| {
            let panel = panels.get(panel_idx)?;
            panel.viewport_top.set(top);
            if height > 0.0 {
                panel.viewport_height.set(height);
            }
            let effective_height = panel.viewport_height.get().max(1.0);
            let low = (top - effective_height).max(0.0);
            let high = top + effective_height * 2.0;
            let (new_first, new_end) = row_range_for_content_span(&*panel.rows_model, low, high);
            let visible = if height > 0.0 {
                row_range_for_content_span(&*panel.rows_model, top, top + height)
            } else {
                (0, 0)
            };
            let old_first = panel.rendered_first.replace(new_first);
            let old_end = panel.rendered_end.replace(new_end);
            Some((
                panel.rows_model.clone(),
                panel.tabs.tabs[panel.tabs.active].current_path.clone(),
                panel.tabs.tabs[panel.tabs.active].preview,
                old_first,
                old_end,
                new_first,
                new_end,
                visible,
            ))
        })
    else {
        return (-1, -1);
    };

    // The symmetric difference is enough to materialize/dematerialize the
    // delegates. We ALWAYS add to it the strictly visible range: after a
    // zoom relayout, `replace_rows` has already pre-marked the new window and
    // old == new on the next callback. Without this bounded reconciliation, a
    // missing texture (LRU evicted, request cancelled while switching to list mode)
    // could stay visible without being either pending or in-flight.
    let mut inspected = if (old_first, old_end) != (new_first, new_end) {
        render_window_changed_indices(old_first, old_end, new_first, new_end)
    } else {
        Vec::new()
    };
    inspected.extend(visible.0..visible.1);
    inspected.sort_unstable();
    inspected.dedup();

    let row_count = model.row_count();
    let mut requests: HashMap<PathBuf, ThumbRequest> = HashMap::new();
    for index in inspected {
        if index >= row_count {
            continue;
        }
        let should_render = index >= new_first && index < new_end;
        let Some(mut row) = model.row_data(index) else {
            continue;
        };
        let mut row_changed = false;
        if row.rendered != should_render {
            row.rendered = should_render;
            row_changed = true;
        }

        if !should_render {
            // The texture stays in the LRU, not in the thousands of off-screen
            // rows. This makes the cache's memory bound actually effective.
            if row.thumbnail.size().width > 0 {
                row.thumbnail = Image::default();
                row_changed = true;
            }
        } else if preview && row.preview_capable && row.thumbnail.size().width == 0 {
            let full = dir.join(row.name.as_str());
            let key = full.to_string_lossy().to_string();
            if let Some(img) = state.thumb_cache.borrow_mut().get(&key) {
                row.thumbnail = img;
                row_changed = true;
            } else if let Some((kind, svg)) = thumbnail_kind_for_row(row.kind, row.ext.as_str()) {
                push_thumbnail_request(
                    &mut requests,
                    full,
                    kind,
                    svg,
                    ThumbLocation {
                        panel: panel_idx,
                        row: index,
                        row_count,
                    },
                );
            }
        }
        if row_changed {
            model.set_row_data(index, row);
        }
    }
    if !requests.is_empty() {
        state
            .thumb_scheduler
            .merge_pending(requests.into_values().collect());
    }

    if visible.0 < visible.1 {
        (visible.0 as i32, visible.1 as i32 - 1)
    } else {
        (-1, -1)
    }
}

// Recursive folder modification date ----------

struct RMtimeJob {
    path: PathBuf,
    /// Recursive mtime depth for this job (`0` = don't compute the date).
    mtime_depth: u32,
    /// Recursive size depth for this job (`0` = don't compute the size).
    size_depth: u32,
    r#gen: u64,
    panel: usize,
    row: usize,
}

/// Applies a (recursive) mtime to a row's "modified" + "age" cells.
fn apply_rmtime_to_row(row: &mut FileRow, m: i64, now: i64, lang: Lang) {
    row.modified = rfs::format_mtime(m, mtime_offset()).into();
    row.age = rfs::format_age(m, now, lang).into();
    row.age_bucket = rfs::age_bucket(m, now);
}

/// Background worker: computes a folder's recursive mtime AND size off the UI
/// thread (one shared walk) and pushes the result via
/// `folder-stats-ready(path, mtime, size)`. Ignores stale jobs (outdated
/// generation = folder left / a depth changed).
fn spawn_rmtime_worker(
    rx: mpsc::Receiver<RMtimeJob>,
    r#gen: Arc<AtomicU64>,
    weak: slint::Weak<MainWindow>,
) {
    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            if job.r#gen != r#gen.load(Ordering::Relaxed) {
                continue;
            }
            // One walk yields both metrics; each is `None` when its depth is 0.
            let (mtime, size) =
                rfs::recursive_folder_stats(&job.path, job.mtime_depth, job.size_depth);
            if mtime.is_none() && size.is_none() {
                continue;
            }
            let path_str = job.path.to_string_lossy().to_string();
            // An empty string means "not computed" for that metric.
            let mtime_str = mtime.map(|m| m.to_string()).unwrap_or_default();
            let size_str = size.map(|s| s.to_string()).unwrap_or_default();
            let panel = job.panel as i32;
            let row = job.row as i32;
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(w) = weak.upgrade() {
                    w.invoke_folder_stats_ready(
                        panel,
                        row,
                        path_str.into(),
                        mtime_str.into(),
                        size_str.into(),
                    );
                }
            });
        }
    });
}

/// If either "folder date" or "folder size" is enabled (its depth > 0): applies
/// the cached recursive mtime and/or size to each panel's FOLDER rows and queues
/// the missing computations — a single shared walk covers both. To be called
/// after any (re)population of rows. Increments the generation.
fn request_folder_stats(state: &AppState) {
    let (mtime_depth, size_depth) = {
        let c = state.config.borrow();
        (
            c.recursive_mtime_depth.max(0) as u32,
            c.recursive_size_depth.max(0) as u32,
        )
    };
    if mtime_depth == 0 && size_depth == 0 {
        return;
    }
    let lang = state.config.borrow().language;
    let now = now_unix();
    let r#gen = state.rmtime_gen.fetch_add(1, Ordering::Relaxed) + 1;
    let panels = state.panels.borrow();
    for (panel_idx, panel) in panels.iter().enumerate() {
        let dir = panel.tabs.tabs[panel.tabs.active].current_path.clone();
        // NETWORK folder: a recursive walk through the share would burst a stat
        // per subfolder — Explorer doesn't either. Keep folders' own mtime and
        // no recursive size.
        if ranger_core::places::is_network_path(&dir) {
            continue;
        }
        let model = &panel.rows_model;
        for i in 0..model.row_count() {
            let Some(mut row) = model.row_data(i) else {
                continue;
            };
            if !row.is_dir {
                continue; // only folders have recursive stats
            }
            let full = dir.join(row.name.as_str());
            let key = full.to_string_lossy().to_string();
            let m_cached = (mtime_depth > 0)
                .then(|| state.rmtime_cache.borrow().get(&key).copied())
                .flatten();
            let s_cached = (size_depth > 0)
                .then(|| state.size_cache.borrow().get(&key).copied())
                .flatten();
            if m_cached.is_some() || s_cached.is_some() {
                if let Some(m) = m_cached {
                    apply_rmtime_to_row(&mut row, m, now, lang);
                }
                if let Some(s) = s_cached {
                    row.size = rfs::format_size(s, lang).into();
                }
                model.set_row_data(i, row);
            }
            // Queue only what is BOTH enabled and not yet cached, so a job never
            // recomputes a metric already known.
            let need_mtime = mtime_depth > 0 && m_cached.is_none();
            let need_size = size_depth > 0 && s_cached.is_none();
            if need_mtime || need_size {
                let _ = state.rmtime_tx.send(RMtimeJob {
                    path: full,
                    mtime_depth: if need_mtime { mtime_depth } else { 0 },
                    size_depth: if need_size { size_depth } else { 0 },
                    r#gen,
                    panel: panel_idx,
                    row: i,
                });
            }
        }
    }
}

// Image metadata: resolution / depth ----------

struct ImgMetaJob {
    path: PathBuf,
    r#gen: u64,
    panel: usize,
    row: usize,
}

/// Formats metadata into a displayable (resolution, depth). Resolution
/// "LxH"; color depth "N-bit" (+ "+ alpha" if present). The
/// alpha channel's bits are not included in `bits`: RGBA8 is therefore displayed as
/// "24-bit + alpha", not the misleading "32-bit +A".
fn format_img_meta(w: u32, h: u32, bits: u16, alpha: bool) -> (String, String) {
    let resolution = format!("{w}x{h}");
    let depth = if alpha {
        format!("{bits}-bit + alpha")
    } else {
        format!("{bits}-bit")
    };
    (resolution, depth)
}

/// Unix mtime (seconds) of a file, or `None` if unreadable. Used to remember
/// an image's VERSION alongside its metadata (resolution/depth) and to detect
/// a later modification (new mtime → stale cache → recompute).
fn file_mtime_unix(path: &Path) -> Option<i64> {
    let mt = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(match mt.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // File dated before 1970 (rare): negative mtime.
        Err(e) => -(e.duration().as_secs() as i64),
    })
}

/// Background worker: reads the image header (dimensions + color type) off the
/// UI thread and pushes `(path, resolution, depth, mtime)` via `imgmeta-ready`. The
/// mtime is read as close as possible to the header → it matches the measured version.
/// Ignores stale jobs (folder left).
fn spawn_imgmeta_worker(
    rx: mpsc::Receiver<ImgMetaJob>,
    r#gen: Arc<AtomicU64>,
    weak: slint::Weak<MainWindow>,
) {
    std::thread::spawn(move || {
        while let Ok(job) = rx.recv() {
            if job.r#gen != r#gen.load(Ordering::Relaxed) {
                continue;
            }
            let Some((w, h, bits, alpha)) = thumbnail::image_meta(&job.path) else {
                continue;
            };
            let (resolution, depth) = format_img_meta(w, h, bits, alpha);
            let mtime = file_mtime_unix(&job.path).unwrap_or(0).to_string();
            let path_str = job.path.to_string_lossy().to_string();
            let panel = job.panel as i32;
            let row = job.row as i32;
            let weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(win) = weak.upgrade() {
                    win.invoke_imgmeta_ready(
                        panel,
                        row,
                        path_str.into(),
                        resolution.into(),
                        depth.into(),
                        mtime.into(),
                    );
                }
            });
        }
    });
}

/// `true` if a panel displays the `resolution` or `depth` column.
fn imgmeta_columns_active(state: &AppState) -> bool {
    state.panels.borrow().iter().any(|p| {
        p.columns
            .iter()
            .any(|c| c.visible && (c.id == "resolution" || c.id == "depth"))
    })
}

/// If a resolution/depth column is visible: applies the cached metadata
/// to IMAGE rows and queues the missing ones. To be called after any
/// (re)population of rows or column change. Increments the generation.
fn request_imgmeta(state: &AppState) {
    if !imgmeta_columns_active(state) {
        return; // no relevant column → no cost
    }
    let r#gen = state.imgmeta_gen.fetch_add(1, Ordering::Relaxed) + 1;
    let panels = state.panels.borrow();
    for (panel_idx, panel) in panels.iter().enumerate() {
        let shows = panel
            .columns
            .iter()
            .any(|c| c.visible && (c.id == "resolution" || c.id == "depth"));
        if !shows {
            continue;
        }
        let dir = panel.tabs.tabs[panel.tabs.active].current_path.clone();
        let model = &panel.rows_model;
        for i in 0..model.row_count() {
            let Some(mut row) = model.row_data(i) else {
                continue;
            };
            if row.kind != 6 || !row.resolution.is_empty() {
                continue; // images only, and not already filled in
            }
            let full = dir.join(row.name.as_str());
            let key = full.to_string_lossy().to_string();
            let cached = state.imgmeta_cache.borrow().get(&key).cloned();
            // The cache is only valid if the mtime STILL matches the file
            // (the stat only happens on a hit). A retouched image has a
            // different mtime → stale → recompute → columns up to date without F5.
            let fresh =
                matches!(&cached, Some((mtime, _, _)) if file_mtime_unix(&full) == Some(*mtime));
            if let Some((_, res, depth)) = cached.filter(|_| fresh) {
                row.resolution = res.into();
                row.depth = depth.into();
                model.set_row_data(i, row);
            } else {
                let _ = state.imgmeta_tx.send(ImgMetaJob {
                    path: full,
                    r#gen,
                    panel: panel_idx,
                    row: i,
                });
            }
        }
    }
}

// ===== Entry zoom =====
// Zoom level (Ctrl+wheel) → row height. LIST mode is a floor
// at a single level (zoom 0): Ctrl+wheel down no longer zooms out from there. The FIRST notch
// upward (zoom == THUMB_ZOOM) switches DIRECTLY to thumbnails — we
// removed the old "smaller" (−1) and "larger without
// thumbnail" (+1) list levels, which added nothing and were confusing.
const THUMB_ZOOM: i32 = 1; // 1st notch above the list = thumbnails
const MIN_ZOOM: i32 = 0; // floor = list mode (no zooming out below)
const MAX_ZOOM: i32 = 8; // 220px ≤ MAX_PX (256) → sharp thumbnails
const LIST_DEFAULT_ZOOM: i32 = 0; // → 28px (list; coincides with the floor)
const THUMB_DEFAULT_ZOOM: i32 = 2; // → 76px ("Previews" button's default)
// Scale invariants verified at COMPILE TIME: the list is the
// single floor and the first notch above enters thumbnail mode.
const _: () = {
    assert!(MIN_ZOOM == LIST_DEFAULT_ZOOM); // no list level below the default
    assert!(LIST_DEFAULT_ZOOM + 1 == THUMB_ZOOM); // one notch = thumbnails (no enlarged list)
    assert!(THUMB_DEFAULT_ZOOM >= THUMB_ZOOM); // the "Previews" button is indeed in thumbnail mode
    assert!(MAX_ZOOM >= THUMB_DEFAULT_ZOOM);
};
/// Historical height of a normal row, kept for single-icon
/// entries when compact Previews mode is active.
const COMPACT_ICON_ROW_HEIGHT: f32 = 28.0;
/// Starting height used before Slint publishes the body's actual size.
/// A one-viewport margin is rendered on each side, so there's no cold screen.
const DEFAULT_RENDER_VIEWPORT_HEIGHT: f32 = 900.0;

/// Maps a zoom level to a row height in logical px. Must remain
/// the SOLE source (pushed to Slint via `PanelView.row_h`, and reused for the
/// rubber-band hit-test). List: 0 (single floor); thumbnails: ≥ 1.
fn zoom_to_height(zoom: i32) -> f32 {
    match zoom.clamp(MIN_ZOOM, MAX_ZOOM) {
        0 => 28.0,                                  // list (floor)
        z => 52.0 + (z - THUMB_ZOOM) as f32 * 24.0, // 1→52, 2→76, … 8→220
    }
}

fn effective_row_height(preview_capable: bool, zoom: i32, compact_icon_rows: bool) -> f32 {
    let zoomed = zoom_to_height(zoom);
    if zoom >= THUMB_ZOOM && compact_icon_rows && !preview_capable {
        COMPACT_ICON_ROW_HEIGHT
    } else {
        zoomed
    }
}

/// Computes the full vertical geometry once. The same values are
/// then consumed by Slint and by all the Rust hit-tests: no parallel
/// formula can drift when the heights become heterogeneous.
fn layout_rows(rows: &mut [FileRow], zoom: i32, compact_icon_rows: bool) {
    let mut y = 0.0_f32;
    for (index, row) in rows.iter_mut().enumerate() {
        row.model_index = index as i32;
        row.visual_y = y;
        row.visual_h = effective_row_height(row.preview_capable, zoom, compact_icon_rows);
        row.rendered = false;
        y += row.visual_h;
    }
}

/// Half-open interval of rows that intersect `[low_y, high_y)`.
/// The search is exact because `layout_rows` produces contiguous, monotonic
/// bands. It serves both virtualized rendering and invariant tests.
fn row_range_for_content_span<M: Model<Data = FileRow>>(
    model: &M,
    low_y: f32,
    high_y: f32,
) -> (usize, usize) {
    let n = model.row_count();
    if n == 0 || high_y <= low_y {
        return (0, 0);
    }
    let mut first = 0usize;
    let mut right = n;
    while first < right {
        let mid = first + (right - first) / 2;
        let row = model.row_data(mid).expect("row geometry model is dense");
        if row.visual_y + row.visual_h <= low_y {
            first = mid + 1;
        } else {
            right = mid;
        }
    }
    let mut end = first;
    let mut end_right = n;
    while end < end_right {
        let mid = end + (end_right - end) / 2;
        let row = model.row_data(mid).expect("row geometry model is dense");
        if row.visual_y < high_y {
            end = mid + 1;
        } else {
            end_right = mid;
        }
    }
    (first.min(n), end.min(n).max(first.min(n)))
}

fn row_range_for_slice(rows: &[FileRow], low_y: f32, high_y: f32) -> (usize, usize) {
    if rows.is_empty() || high_y <= low_y {
        return (0, 0);
    }
    let first = rows.partition_point(|row| row.visual_y + row.visual_h <= low_y);
    let end = rows.partition_point(|row| row.visual_y < high_y);
    (first, end.max(first))
}

/// Marks a viewport + one overscan screen before/after. The number of delegates
/// stays proportional to what can actually be displayed, never to the folder.
fn mark_render_window(rows: &mut [FileRow], top: f32, height: f32) -> (usize, usize) {
    let height = height.max(1.0);
    let low = (top - height).max(0.0);
    let high = top.max(0.0) + height * 2.0;
    let (first, end) = row_range_for_slice(rows, low, high);
    for row in &mut rows[first..end] {
        row.rendered = true;
    }
    (first, end)
}

fn render_window_changed_indices(
    old_first: usize,
    old_end: usize,
    new_first: usize,
    new_end: usize,
) -> Vec<usize> {
    (old_first..old_end)
        .filter(|index| *index < new_first || *index >= new_end)
        .chain((new_first..new_end).filter(|index| *index < old_first || *index >= old_end))
        .collect()
}

/// Semantic point preserved during a zoom relayout. `row_fraction` also
/// keeps the exact point within a row whose height changes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ZoomAnchor {
    row_index: usize,
    row_fraction: f32,
    viewport_y: f32,
}

#[derive(Debug, Clone, Copy)]
struct ZoomViewport {
    top: f32,
    height: f32,
    pointer_y: f32,
}

fn row_index_at_content_y_slice(rows: &[FileRow], y: f32) -> Option<usize> {
    if y < 0.0 {
        return None;
    }
    let index = rows.partition_point(|row| row.visual_y + row.visual_h <= y);
    rows.get(index)
        .filter(|row| y >= row.visual_y && y < row.visual_y + row.visual_h)
        .map(|_| index)
}

fn zoom_anchor_on_row(
    rows: &[FileRow],
    row_index: usize,
    content_y: f32,
    viewport_top: f32,
) -> Option<ZoomAnchor> {
    let row = rows.get(row_index)?;
    let row_fraction = if row.visual_h > 0.0 {
        ((content_y - row.visual_y) / row.visual_h).clamp(0.0, 1.0)
    } else {
        0.0
    };
    Some(ZoomAnchor {
        row_index,
        row_fraction,
        viewport_y: content_y - viewport_top,
    })
}

/// Chooses the zoom anchor without depending on virtualized rendering:
/// 1. the single selection if it is currently visible;
/// 2. the point under the pointer;
/// 3. the viewport's center.
///
/// A single off-screen selection deliberately causes no jump: the
/// zoom stays anchored to the context the user is actually
/// looking at.
fn capture_zoom_anchor(
    rows: &[FileRow],
    viewport_top: f32,
    viewport_height: f32,
    pointer_y: f32,
) -> Option<ZoomAnchor> {
    if rows.is_empty() {
        return None;
    }
    let top = viewport_top.max(0.0);
    let height = viewport_height.max(1.0);
    let bottom = top + height;

    let mut selected = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.selected)
        .map(|(index, _)| index);
    let first_selected = selected.next();
    let unique_selected = first_selected.filter(|_| selected.next().is_none());
    if let Some(index) = unique_selected {
        let row = &rows[index];
        let visible_top = row.visual_y.max(top);
        let visible_bottom = (row.visual_y + row.visual_h).min(bottom);
        if visible_bottom > visible_top {
            // The middle of the visible portion is always strictly within the
            // row, even if it's partially cut off by the viewport.
            let content_y = (visible_top + visible_bottom) * 0.5;
            return zoom_anchor_on_row(rows, index, content_y, top);
        }
    }

    // An event landing exactly on the bottom edge still belongs to this
    // TouchArea, but `top + height` is already outside the semi-open viewport.
    let pointer_viewport_y = pointer_y.clamp(0.0, (height - 0.5).max(0.0));
    let pointer_content_y = top + pointer_viewport_y;
    if let Some(index) = row_index_at_content_y_slice(rows, pointer_content_y) {
        return zoom_anchor_on_row(rows, index, pointer_content_y, top);
    }

    let center_y = top + height * 0.5;
    row_index_at_content_y_slice(rows, center_y)
        .and_then(|index| zoom_anchor_on_row(rows, index, center_y, top))
}

fn restore_zoom_viewport_top(
    rows: &[FileRow],
    anchor: Option<ZoomAnchor>,
    fallback_top: f32,
    viewport_height: f32,
) -> f32 {
    let content_height = rows
        .last()
        .map(|row| row.visual_y + row.visual_h)
        .unwrap_or(0.0);
    let max_top = (content_height - viewport_height.max(1.0)).max(0.0);
    let desired = anchor
        .and_then(|anchor| {
            rows.get(anchor.row_index)
                .map(|row| row.visual_y + row.visual_h * anchor.row_fraction - anchor.viewport_y)
        })
        .unwrap_or(fallback_top);
    desired.clamp(0.0, max_top)
}

/// Specialized relayout for Ctrl+wheel. The anchor capture, any
/// mode-change icons, and the new geometry share the same
/// vector: no second model clone, no listing, and no I/O added
/// to the hot path of notches staying within the same mode.
fn zoom_panel_visuals(
    panel: &Panel,
    preview: bool,
    zoom: i32,
    compact_icon_rows: bool,
    crossed_mode: bool,
    viewport: ZoomViewport,
) -> f32 {
    let mut rows: Vec<FileRow> = (0..panel.rows_model.row_count())
        .filter_map(|index| panel.rows_model.row_data(index))
        .collect();
    let anchor = capture_zoom_anchor(&rows, viewport.top, viewport.height, viewport.pointer_y);
    let previous: Vec<(f32, f32)> = rows
        .iter()
        .map(|row| (row.visual_y, row.visual_h))
        .collect();

    if crossed_mode {
        refresh_rows_visuals(&mut rows, preview, compact_icon_rows);
    }

    layout_rows(&mut rows, zoom, compact_icon_rows);
    let geometry_changed = rows.iter().zip(previous).any(|(row, (y, h))| {
        (row.visual_y - y).abs() > f32::EPSILON || (row.visual_h - h).abs() > f32::EPSILON
    });
    let anchored_top = restore_zoom_viewport_top(&rows, anchor, viewport.top, viewport.height);

    // `replace_rows` directly pre-marks the virtualized window around the
    // destination, not around the old, now-stale scroll.
    panel.viewport_top.set(anchored_top);
    panel.viewport_height.set(viewport.height.max(1.0));
    if crossed_mode || geometry_changed {
        panel.replace_rows(rows);
    }
    anchored_top
}

fn refresh_rows_visuals(rows: &mut [FileRow], preview: bool, compact_icon_rows: bool) {
    for row in rows {
        let use_large_icon = preview && (!compact_icon_rows || row.preview_capable);
        let (app_icon, link_folder) = row_app_icon(
            row.path.as_str(),
            row.name.as_str(),
            row.ext.as_str(),
            row.is_dir,
            use_large_icon,
        );
        row.app_icon = app_icon;
        row.link_folder = link_folder;
        if !preview {
            row.thumbnail = Image::default();
        }
    }
}

fn refresh_panel_visuals(panel: &Panel, preview: bool, zoom: i32, compact_icon_rows: bool) {
    let mut rows: Vec<FileRow> = (0..panel.rows_model.row_count())
        .filter_map(|index| panel.rows_model.row_data(index))
        .collect();
    refresh_rows_visuals(&mut rows, preview, compact_icon_rows);
    layout_rows(&mut rows, zoom, compact_icon_rows);
    panel.replace_rows(rows);
}

fn refresh_preview_panel_visuals(state: &AppState) {
    let compact = state.config.borrow().compact_icon_rows_in_preview;
    let panels = state.panels.borrow();
    for panel in panels.iter() {
        let tab = &panel.tabs.tabs[panel.tabs.active];
        if tab.preview {
            refresh_panel_visuals(panel, true, tab.zoom, compact);
        }
    }
}

/// Exact index of the row containing `y` in the vertical content.
/// O(log n) binary search, used on every drag/hover move.
fn row_index_at_content_y<M: Model<Data = FileRow>>(model: &M, y: f32) -> i32 {
    if y < 0.0 {
        return -1;
    }
    let mut lo = 0usize;
    let mut hi = model.row_count();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let Some(row) = model.row_data(mid) else {
            return -1;
        };
        if y < row.visual_y {
            hi = mid;
        } else if y >= row.visual_y + row.visual_h {
            lo = mid + 1;
        } else {
            return mid as i32;
        }
    }
    -1
}

/// Indices covered by a vertical content band. The ends outside the
/// content are naturally clamped, unlike the single-point hit-test.
fn row_band_for_content_range(model: &VecModel<FileRow>, y1: f32, y2: f32) -> (i32, i32) {
    let n = model.row_count();
    if n == 0 {
        return (0, -1);
    }
    let low_y = y1.min(y2);
    let high_y = y1.max(y2);

    let mut lo = 0usize;
    let mut right = n;
    while lo < right {
        let mid = lo + (right - lo) / 2;
        let row = model.row_data(mid).expect("row geometry model is dense");
        if row.visual_y + row.visual_h <= low_y {
            lo = mid + 1;
        } else {
            right = mid;
        }
    }

    let mut upper = 0usize;
    let mut upper_right = n;
    while upper < upper_right {
        let mid = upper + (upper_right - upper) / 2;
        let row = model.row_data(mid).expect("row geometry model is dense");
        if row.visual_y <= high_y {
            upper = mid + 1;
        } else {
            upper_right = mid;
        }
    }
    let hi = upper.saturating_sub(1);
    if lo >= n || upper == 0 || hi < lo {
        (lo as i32, lo as i32 - 1)
    } else {
        (lo as i32, hi as i32)
    }
}

/// Local<->UTC offset (seconds) applied to the "Modified" column. Set
/// by the GUI at startup and on every timezone setting change; read by
/// the row builders (`entry_to_row`, `apply_rmtime_to_row`, preview).
/// `0` = UTC. Atomic global: the initial listing can build rows
/// from a background thread.
static MTIME_OFFSET_SECS: AtomicI64 = AtomicI64::new(0);

fn mtime_offset() -> i64 {
    MTIME_OFFSET_SECS.load(Ordering::Relaxed)
}

/// Recomputes the offset from `clock_utc`: `0` (UTC) or the current local offset.
fn refresh_mtime_offset(state: &AppState) {
    let off = if state.config.borrow().clock_utc {
        0
    } else {
        actions::local_utc_offset_secs()
    };
    MTIME_OFFSET_SECS.store(off, Ordering::Relaxed);
}

/// Unix seconds for "now" (0 if the clock precedes the epoch). Computed ONCE
/// per listing, then passed to `entry_to_row` for the "age" column.
fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

thread_local! {
    /// Cache of OS app icons by `(lowercase extension, jumbo?)` → a single shell
    /// extraction per extension/size and per session, regardless of the
    /// folder's size. `None` is also memoized (no retry). Two sizes:
    /// 32 px (list) and 256 px (previews). UI thread only.
    static EXT_ICON_CACHE: RefCell<HashMap<(String, bool), Option<Image>>> =
        RefCell::new(HashMap::new());
}

/// Icon of the OS's default application for extension `ext` (without the dot),
/// cached by extension + size. `big` = preview mode → 256 px icon (sharp
/// even enlarged); otherwise 32 px (list mode). `None` (→ empty `Image`) if
/// unavailable: the view falls back to the SVG type icon. Windows (Linux stub).
fn ext_app_icon(ext: &str, big: bool) -> Image {
    if ext.is_empty() {
        return Image::default();
    }
    let key = (ext.to_ascii_lowercase(), big);
    EXT_ICON_CACHE.with(|c| {
        if let Some(v) = c.borrow().get(&key) {
            return v.clone().unwrap_or_default();
        }
        let img = openwith::icon_rgba_for_ext(&key.0, big).map(|(rgba, w, h)| {
            Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
                &rgba, w, h,
            ))
        });
        c.borrow_mut().insert(key, img.clone());
        img.unwrap_or_default()
    })
}

thread_local! {
    /// Cache of icons SPECIFIC to a path by `(path, jumbo?)` — a single
    /// shell extraction per file/size and per session. Essential:
    /// unlike the per-extension cache, this path does disk I/O (reading
    /// the binary's resources or Shell-resolving a `.lnk`) and `entry_to_row`
    /// is replayed on every sort/filter.
    static SELF_ICON_CACHE: RefCell<HashMap<(String, bool), Option<Image>>> =
        RefCell::new(HashMap::new());
}

/// Does this file type carry its OWN icon (≠ generic icon for its
/// extension)? Executables and related types generally embed their own
/// resource, which must be resolved by path rather than by extension.
fn has_own_icon(ext: &str) -> bool {
    matches!(ext, "exe" | "scr" | "cpl" | "ico")
}

/// Exact icon of path `path`, cached by path + size. Unlike
/// [`self_icon`], it doesn't replace a failure with the extension's generic icon:
/// a `.lnk` shortcut needs this to keep its own `IconLocation`.
fn cached_path_icon(path: &Path, big: bool) -> Option<Image> {
    let key = (path.display().to_string(), big);
    let cached = SELF_ICON_CACHE.with(|c| c.borrow().get(&key).cloned());
    if let Some(v) = cached {
        return v;
    }
    let img = openwith::icon_rgba_for_path(&key.0, big).map(|(rgba, w, h)| {
        Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
            &rgba, w, h,
        ))
    });
    SELF_ICON_CACHE.with(|c| c.borrow_mut().insert(key, img.clone()));
    img
}

/// Icon specific to file `path`, cached by path + size. Falls back to
/// the extension icon if the shell returns nothing.
fn self_icon(path: &Path, ext: &str, big: bool) -> Image {
    cached_path_icon(path, big).unwrap_or_else(|| ext_app_icon(ext, big))
}

thread_local! {
    /// Cache of `.lnk` shortcut targets by PATH → `(target_is_folder,
    /// target_extension)`, or `None` if unresolved. Avoids a COM call + a `stat`
    /// per `.lnk` on EVERY (re)listing (sort, filter, timezone change…).
    static LNK_TARGET_CACHE: RefCell<HashMap<String, Option<(bool, String)>>> =
        RefCell::new(HashMap::new());
}

/// Icon of a `.lnk` shortcut. For a file target (or an unresolved one), the LINK's
/// PATH is submitted to the Shell first: it's the one that knows the `IconLocation`
/// possibly stored in the shortcut and the target's own icon. Manual
/// resolution now only serves the historical "folder + arrow" rendering
/// and the extension fallback if the Shell renders nothing.
/// `None` = link and target unresolved (→ generic `.lnk` icon) or non-Windows.
fn resolve_lnk_icon(parent_display: &str, name: &str, big: bool) -> Option<(Image, bool)> {
    #[cfg(windows)]
    {
        let path = std::path::PathBuf::from(parent_display).join(name);
        let key = path.display().to_string();
        let cached = LNK_TARGET_CACHE.with(|c| c.borrow().get(&key).cloned());
        let resolved = match cached {
            Some(v) => v,
            None => {
                let v = openwith::resolve_shortcut(&path).map(|t| {
                    let ext = t
                        .extension()
                        .map(|e| e.to_string_lossy().to_ascii_lowercase())
                        .unwrap_or_default();
                    (t.is_dir(), ext)
                });
                LNK_TARGET_CACHE.with(|c| c.borrow_mut().insert(key, v.clone()));
                v
            }
        };
        match resolved {
            // Keeps the explicit historical "folder + arrow" rendering: a
            // non-empty Shell image would take priority over this SVG in Slint.
            Some((true, _)) => Some((Image::default(), true)),
            Some((false, ext)) => Some((
                cached_path_icon(&path, big).unwrap_or_else(|| ext_app_icon(&ext, big)),
                false,
            )),
            None => cached_path_icon(&path, big).map(|icon| (icon, false)),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (parent_display, name, big);
        None
    }
}

/// If `lnk_path` is a Windows `.lnk` pointing at a file Ranger can preview
/// (image / audio / video / PDF), returns the TARGET's thumbnail kind — so an image
/// shortcut previews the image, like Explorer. Reuses the icon path's
/// `LNK_TARGET_CACHE`, so it adds no COM call the icon didn't already make.
/// `None` on non-Windows, for a folder target, or a non-previewable one.
fn lnk_thumbnail_kind(lnk_path: &Path) -> Option<FileKind> {
    #[cfg(windows)]
    {
        let key = lnk_path.display().to_string();
        let cached = LNK_TARGET_CACHE.with(|c| c.borrow().get(&key).cloned());
        let (is_dir, ext) = match cached {
            Some(v) => v?,
            None => {
                let resolved = openwith::resolve_shortcut(lnk_path).map(|t| {
                    let e = t
                        .extension()
                        .map(|x| x.to_string_lossy().to_ascii_lowercase())
                        .unwrap_or_default();
                    (t.is_dir(), e)
                });
                LNK_TARGET_CACHE.with(|c| c.borrow_mut().insert(key, resolved.clone()));
                resolved?
            }
        };
        if is_dir {
            return None;
        }
        thumbnail_kind_for_row(rfs::classify_kind(Some(&ext), false).as_i32(), &ext)
            .map(|(kind, _)| kind)
    }
    #[cfg(not(windows))]
    {
        let _ = lnk_path;
        None
    }
}

/// Resolves a row's OS icon from only the information already present
/// in the model. This route is shared by the initial listing and the
/// List/Previews toggle, which therefore no longer need to re-read the folder.
fn row_app_icon(
    parent_display: &str,
    name: &str,
    ext: &str,
    is_dir: bool,
    big: bool,
) -> (Image, bool) {
    if is_dir {
        return (Image::default(), false);
    }
    if ext == "lnk" {
        return resolve_lnk_icon(parent_display, name, big)
            .unwrap_or_else(|| (ext_app_icon(ext, big), false));
    }
    if has_own_icon(ext) {
        return (
            self_icon(&Path::new(parent_display).join(name), ext, big),
            false,
        );
    }
    (ext_app_icon(ext, big), false)
}

/// Parses the extension filter's free text into lowercase extensions
/// without a dot. FLEXIBLE syntax: comma AND/OR space separators
/// ("jpg, png", "jpg png", ".JPG,.PNG" → `["jpg","png"]`). Empty if nothing.
fn parse_ext_filter(text: &str) -> Vec<String> {
    text.split(|c: char| c == ',' || c.is_whitespace())
        .map(|t| t.trim_start_matches('.').to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Applies the extension filter to `entries` (in place): keeps only the
/// files whose extension is in the filter. FOLDERS remain visible
/// (navigation preserved). No-op if the filter is empty/disabled.
fn apply_ext_filter(entries: &mut Vec<Entry>, on: bool, filter: &str) {
    if !on {
        return;
    }
    let exts = parse_ext_filter(filter);
    if exts.is_empty() {
        return;
    }
    entries.retain(|e| {
        e.is_dir || {
            let ext = ops::ext_of(&e.name);
            !ext.is_empty() && exts.contains(&ext)
        }
    });
}

fn entry_to_row(
    e: &Entry,
    parent_display: &str,
    lang: Lang,
    now_unix: i64,
    big_icon: bool,
    compact_icon_rows: bool,
) -> FileRow {
    let size = if e.is_dir {
        "—".to_string()
    } else {
        e.size_bytes
            .map(|b| rfs::format_size(b, lang))
            .unwrap_or_else(|| "—".to_string())
    };
    let modified = e
        .mtime_unix
        .map(|m| rfs::format_mtime(m, mtime_offset()))
        .unwrap_or_else(|| "—".to_string());
    // "age" column: compact text + warm-to-cold color bucket.
    let (age, age_bucket) = match e.mtime_unix {
        Some(m) => (
            rfs::format_age(m, now_unix, lang),
            rfs::age_bucket(m, now_unix),
        ),
        None => ("—".to_string(), -1),
    };
    // Splits name/extension for coloring (files only: a
    // folder named "x.y" has no "extension" to highlight).
    // Name DISPLAY (stem + colored extension): `split_name` leaves
    // dotfiles whole. TYPING (the "ext" column, icon, sort, filter): `ext_of`
    // treats a dotfile's suffix as an extension (`.gitignore` → gitignore).
    let (name_base, name_ext) = if e.is_dir {
        (e.name.clone(), String::new())
    } else {
        let (stem, ext) = ops::split_name(&e.name);
        if ext.is_empty() {
            (e.name.clone(), String::new())
        } else {
            (stem, format!(".{ext}"))
        }
    };
    let ext_raw = if e.is_dir {
        String::new()
    } else {
        ops::ext_of(&e.name)
    };
    #[cfg(windows)]
    let drop_runnable = !e.is_dir && matches!(ext_raw.as_str(), "exe" | "com" | "cmd" | "bat");
    // The executable bit alone gives false positives (FAT/NTFS/exFAT mounts at
    // 0777, files copied from Windows): an image/video/document… is NOT
    // a program, so no "launch with these files" (see can_be_program).
    #[cfg(not(windows))]
    let drop_runnable = !e.is_dir && e.executable && e.kind.can_be_program();
    let preview_capable = thumbnail_kind_for_row(e.kind.as_i32(), &ext_raw).is_some();
    // Actual OS app icon for this extension — empty for a folder
    // or if unavailable (falls back to the view's type icon). Cached.
    // Resolution suited to the mode: 256 px in previews (enlarged), 32 px in list.
    // For `.lnk` shortcuts: we resolve the target → target app icon
    // (file) OR "folder + arrow" marker (folder) via `link_folder`.
    let use_large_icon = big_icon && (!compact_icon_rows || preview_capable);
    let (app_icon, link_folder) =
        row_app_icon(parent_display, &e.name, &ext_raw, e.is_dir, use_large_icon);
    FileRow {
        name: e.name.clone().into(),
        name_base: name_base.into(),
        name_ext: name_ext.into(),
        ext: ext_raw.into(),
        path: parent_display.to_string().into(),
        size: size.into(),
        modified: modified.into(),
        is_dir: e.is_dir,
        is_symlink: e.is_symlink,
        drop_runnable,
        kind: e.kind.as_i32(),
        selected: false,
        cut: false,
        hidden: e.hidden,
        age: age.into(),
        age_bucket,
        // Image metadata: empty here; filled in place by the
        // imgmeta worker when the resolution/depth column is visible.
        resolution: SharedString::default(),
        depth: SharedString::default(),
        // Thumbnail: empty here; filled in place by the worker in
        // preview mode, or immediately from the cache in update_panels_ui.
        thumbnail: slint::Image::default(),
        app_icon,
        link_folder,
        preview_capable,
        visual_y: 0.0,
        visual_h: COMPACT_ICON_ROW_HEIGHT,
        model_index: 0,
        rendered: false,
    }
}

fn entries_to_rows(
    entries: &[Entry],
    parent_display: &str,
    lang: Lang,
    now_unix: i64,
    big_icon: bool,
    zoom: i32,
    compact_icon_rows: bool,
) -> Vec<FileRow> {
    let mut rows: Vec<FileRow> = entries
        .iter()
        .map(|entry| {
            entry_to_row(
                entry,
                parent_display,
                lang,
                now_unix,
                big_icon,
                compact_icon_rows,
            )
        })
        .collect();
    layout_rows(&mut rows, zoom, compact_icon_rows);
    rows
}

// ---------- Selection helpers (Slint model manipulation) ----------
//
// Convention: all of them return the **number of selected items** after
// the operation, so the caller can push it directly into
// `selected-count` without recomputing it.

fn selection_set_only<M: slint::Model<Data = FileRow>>(model: &M, idx: i32) -> i32 {
    let mut count = 0i32;
    let n = model.row_count();
    for i in 0..n {
        if let Some(mut row) = model.row_data(i) {
            let should = i as i32 == idx;
            if row.selected != should {
                row.selected = should;
                model.set_row_data(i, row);
            }
            if should {
                count += 1;
            }
        }
    }
    count
}

fn selection_toggle<M: slint::Model<Data = FileRow>>(model: &M, idx: i32) -> i32 {
    let n = model.row_count();
    if idx < 0 || (idx as usize) >= n {
        return count_selected(model);
    }
    if let Some(mut row) = model.row_data(idx as usize) {
        row.selected = !row.selected;
        model.set_row_data(idx as usize, row);
    }
    count_selected(model)
}

thread_local! {
    /// State of an additive/subtractive rubber-band, UI thread. `.0` = mode
    /// (0 = replace, 1 = add [Shift], 2 = subtract [Ctrl]); `.1` = snapshot
    /// of the selection AT THE START of the drag (base combined with the band on every update).
    static RB_STATE: RefCell<(i32, Vec<bool>)> = const { RefCell::new((0, Vec::new())) };
}

/// Boolean snapshot of the current selection (for the additive rubber-band).
fn snapshot_selection<M: slint::Model<Data = FileRow>>(model: &M) -> Vec<bool> {
    (0..model.row_count())
        .map(|i| model.row_data(i).map(|r| r.selected).unwrap_or(false))
        .collect()
}

/// Applies a rubber-band `[lo, hi]` combined with a `base` according to `mode`
/// (0 = replace, 1 = add, 2 = subtract). Returns the selected count.
fn selection_apply_band<M: slint::Model<Data = FileRow>>(
    model: &M,
    lo: i32,
    hi: i32,
    mode: i32,
    base: &[bool],
) -> i32 {
    let mut count = 0i32;
    let n = model.row_count();
    for i in 0..n {
        let in_band = (i as i32) >= lo && (i as i32) <= hi;
        let based = base.get(i).copied().unwrap_or(false);
        let want = match mode {
            1 => based || in_band,  // Shift: add the band to the base selection
            2 => based && !in_band, // Ctrl: subtract the band from the base
            _ => in_band,           // replace
        };
        if let Some(mut row) = model.row_data(i)
            && row.selected != want
        {
            row.selected = want;
            model.set_row_data(i, row);
        }
        if want {
            count += 1;
        }
    }
    count
}

fn selection_set_range<M: slint::Model<Data = FileRow>>(model: &M, a: i32, b: i32) -> i32 {
    let lo = a.min(b);
    let hi = a.max(b);
    let mut count = 0i32;
    let n = model.row_count();
    for i in 0..n {
        if let Some(mut row) = model.row_data(i) {
            let in_range = (i as i32) >= lo && (i as i32) <= hi;
            if row.selected != in_range {
                row.selected = in_range;
                model.set_row_data(i, row);
            }
            if in_range {
                count += 1;
            }
        }
    }
    count
}

fn selection_set_all<M: slint::Model<Data = FileRow>>(model: &M, value: bool) -> i32 {
    let n = model.row_count();
    for i in 0..n {
        if let Some(mut row) = model.row_data(i)
            && row.selected != value
        {
            row.selected = value;
            model.set_row_data(i, row);
        }
    }
    if value { n as i32 } else { 0 }
}

fn count_selected<M: slint::Model<Data = FileRow>>(model: &M) -> i32 {
    let mut c = 0i32;
    let n = model.row_count();
    for i in 0..n {
        if let Some(row) = model.row_data(i)
            && row.selected
        {
            c += 1;
        }
    }
    c
}

/// Targets entry `name` in the ACTIVE panel: single selection + keyboard cursor +
/// scroll-into-view. No-op if the name is absent from the current model. Used to
/// "follow" an entry that was just created. (Reusable for other
/// "reveal this entry" cases.)
fn focus_entry_by_name(window: &MainWindow, state: &AppState, name: &str) {
    let model = state.active_rows_model();
    let idx = (0..model.row_count())
        .find(|&i| model.row_data(i).is_some_and(|r| r.name.as_str() == name));
    let Some(idx) = idx.map(|i| i as i32) else {
        return;
    };
    let count = selection_set_only(&model, idx);
    state.with_tabs_mut(|b| {
        let a = b.active;
        let t = &mut b.tabs[a];
        t.cursor = idx;
        t.selection_anchor = idx;
        t.scroll_gen += 1; // scroll-into-view on the Slint side
    });
    update_panels_ui(window, state);
    push_active_footer(window, state, count);
}

/// Reveals an entry on the next UI turn. Navigation publishes both a viewport
/// reset and a new row model; deferring the reveal guarantees that the reset is
/// applied first instead of racing the `scroll-gen` update. The directory guard
/// prevents a late callback from changing a view the user has already left.
fn schedule_focus_entry_by_name(
    window: &MainWindow,
    state: &AppState,
    expected_dir: PathBuf,
    name: String,
) {
    let weak = window.as_weak();
    let state = state.clone();
    defer(move || {
        let Some(window) = weak.upgrade() else { return };
        if ops::paths_equal(&state.current_path(), &expected_dir) {
            focus_entry_by_name(&window, &state, &name);
        }
    });
}

/// Last "displayable" segment of a path: `file_name`, or the SHARE
/// name for a share root `\\HOST\share` (whose `file_name()` is `None`), as
/// it appears in the server's share list.
fn child_leaf(path: &Path) -> Option<String> {
    if let Some(n) = path.file_name() {
        return Some(n.to_string_lossy().into_owned());
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        if let Some(Component::Prefix(p)) = path.components().next()
            && let Prefix::UNC(_, share) | Prefix::VerbatimUNC(_, share) = p.kind()
        {
            let s = share.to_string_lossy();
            if !s.is_empty() {
                return Some(s.into_owned());
            }
        }
    }
    None
}

fn upward_navigation_child_name(target: &Path, left: &Path) -> Option<String> {
    let direct_parent = left
        .parent()
        .is_some_and(|parent| ops::paths_equal(parent, target))
        || rfs::unc_share_parent(left)
            .as_deref()
            .is_some_and(|parent| ops::paths_equal(parent, target));
    direct_parent.then(|| child_leaf(left)).flatten()
}

/// After an UPWARD navigation (back / parent folder), selects
/// child `left` we came from in the TARGET folder, if it is its DIRECT
/// child → cursor + highlight + scroll-into-view. No-op otherwise (going back to a
/// folder with no parent-child relation).
fn select_child_from(window: &MainWindow, state: &AppState, target: &Path, left: &Path) {
    if let Some(name) = upward_navigation_child_name(target, left) {
        // On network, the listing is still on the worker: remember the target
        // and apply it atomically along with the delivered rows.
        let active = *state.active_panel.borrow();
        let deferred = {
            let mut panels = state.panels.borrow_mut();
            if let Some(panel) = panels.get_mut(active) {
                if panel.pending_listing
                    && panel.tabs.tabs[panel.tabs.active].current_path == target
                {
                    panel.pending_select = Some(name.clone());
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if deferred {
            return;
        }
        schedule_focus_entry_by_name(window, state, target.to_path_buf(), name);
    }
}

fn apply_name_filter(entries: &mut Vec<Entry>, filter: &str) {
    if filter.is_empty() {
        return;
    }
    let needle = filter.to_lowercase();
    entries.retain(|entry| name_contains_filter(&entry.name, &needle));
}

/// `needle` is already normalized to lowercase by the listing's caller, so as
/// not to redo this work for every entry of a large folder.
fn name_contains_filter(name: &str, needle: &str) -> bool {
    name.to_lowercase().contains(needle)
}

// Helpers ----------

/// Absolute paths of the active panel's selected rows.
fn selected_paths(state: &AppState) -> Vec<PathBuf> {
    let rows = state.active_rows_model();
    let cur_dir = state.current_path();
    (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .filter(|r| r.selected)
        .map(|r| cur_dir.join(r.name.as_str()))
        .collect()
}

/// Current folder of a given panel (by index). Empty if the index is invalid.
fn panel_dir(state: &AppState, panel: usize) -> PathBuf {
    let panels = state.panels.borrow();
    panels
        .get(panel)
        .map(|p| p.tabs.tabs[p.tabs.active].current_path.clone())
        .unwrap_or_default()
}

/// Path of the folder at row `row` of panel `panel`, if it is one — for
/// dropping a selection INTO a subfolder hovered during a drag.
/// `None` if the index is out of bounds or the row isn't a folder.
fn panel_folder_at_row(state: &AppState, panel: usize, row: usize) -> Option<PathBuf> {
    let panels = state.panels.borrow();
    let p = panels.get(panel)?;
    let r = p.rows_model.row_data(row)?;
    if !r.is_dir {
        return None;
    }
    Some(
        p.tabs.tabs[p.tabs.active]
            .current_path
            .join(r.name.as_str()),
    )
}

/// Path of row `row` (file OR folder) of panel `panel`.
fn panel_path_at_row(state: &AppState, panel: usize, row: usize) -> Option<PathBuf> {
    let panels = state.panels.borrow();
    let p = panels.get(panel)?;
    let r = p.rows_model.row_data(row)?;
    Some(
        p.tabs.tabs[p.tabs.active]
            .current_path
            .join(r.name.as_str()),
    )
}

/// Paths selected in a GIVEN panel (not necessarily the active one) — used
/// by cross-view drag'n'drop (the source may not be the active panel).
fn panel_selected_paths(state: &AppState, panel: usize) -> Vec<PathBuf> {
    let panels = state.panels.borrow();
    let Some(p) = panels.get(panel) else {
        return Vec::new();
    };
    let dir = p.tabs.tabs[p.tabs.active].current_path.clone();
    (0..p.rows_model.row_count())
        .filter_map(|i| p.rows_model.row_data(i))
        .filter(|r| r.selected)
        .map(|r| dir.join(r.name.as_str()))
        .collect()
}

/// Is a row target the source itself, or a descendant into which
/// a source folder can't be dropped? Pure, no I/O or canonicalization.
fn paths_conflict_with_drop_target(
    sources: &[PathBuf],
    target: &Path,
    target_is_dir: bool,
) -> bool {
    sources.iter().any(|source| {
        ops::paths_equal(source, target) || (target_is_dir && target.starts_with(source))
    })
}

/// Lightweight validation called only when the hovered row changes.
fn file_drop_target_invalid(state: &AppState, src_panel: i32, target_panel: i32, row: i32) -> bool {
    if src_panel < 0 || target_panel < 0 {
        return false; // external drag: paths validated on the native drop
    }
    let src = src_panel as usize;
    let target = target_panel as usize;

    // Hot path: within the same view, the target is exactly the source if its
    // row belongs to the selection. No list traversal needed.
    if src == target {
        if row < 0 {
            return false;
        }
        return state
            .panels
            .borrow()
            .get(target)
            .and_then(|panel| panel.rows_model.row_data(row as usize))
            .is_some_and(|entry| entry.selected);
    }

    let (target_path, target_is_dir) = if row >= 0 {
        let row = row as usize;
        let Some(path) = panel_path_at_row(state, target, row) else {
            return false;
        };
        (path, panel_folder_at_row(state, target, row).is_some())
    } else {
        (panel_dir(state, target), true)
    };
    if target_path.as_os_str().is_empty() {
        return false;
    }
    let sources = panel_selected_paths(state, src);
    paths_conflict_with_drop_target(&sources, &target_path, target_is_dir)
}

/// Sets the `cut = true` flag on rows whose name is in `names`.
fn apply_cut_marks<M: Model<Data = FileRow>>(model: &M, names: &std::collections::HashSet<String>) {
    let n = model.row_count();
    for i in 0..n {
        if let Some(mut row) = model.row_data(i) {
            let should = names.contains(row.name.as_str());
            if row.cut != should {
                row.cut = should;
                model.set_row_data(i, row);
            }
        }
    }
}

/// Clears the `cut` flag on all rows (Ctrl+C after Ctrl+X, or navigation).
fn clear_cut_marks<M: Model<Data = FileRow>>(model: &M) {
    let n = model.row_count();
    for i in 0..n {
        if let Some(mut row) = model.row_data(i)
            && row.cut
        {
            row.cut = false;
            model.set_row_data(i, row);
        }
    }
}

// ---------- i18n helpers ----------

fn apply_language(window: &MainWindow, lang: Lang) {
    window.set_strings(i18n::strings_for(lang));

    let lang_labels: Vec<SharedString> = i18n::language_labels();
    let lang_idx = Lang::all().iter().position(|l| *l == lang).unwrap_or(0) as i32;
    window.set_language_labels(ModelRc::new(VecModel::from(lang_labels)));
    window.set_language_index(lang_idx);

    let theme_labels: Vec<SharedString> = i18n::theme_labels(lang);
    window.set_theme_labels(ModelRc::new(VecModel::from(theme_labels)));

    // "Default tab bar position" setting: same
    // labels as the per-view menu.
    let tabbar_labels: Vec<SharedString> = i18n::tabbar_labels(lang);
    window.set_tabbar_default_labels(ModelRc::new(VecModel::from(tabbar_labels)));

    // The per-panel footer is recomputed by update_panels_ui (called after
    // a language change from the on_language_changed callback).
}

// The cross-view insertion index comes from the exact gap claimed on the Slint side
// on the real geometry, then read via `drag-target-gap`.

fn home_dir() -> PathBuf {
    // Cross-platform: `$HOME` (Linux) / `%USERPROFILE%` (Windows).
    dirs::home_dir().unwrap_or_else(std::env::temp_dir)
}

/// Builds `(panels, stretches, active_panel)` from a `WorkspaceState`.
/// Shared by `from_workspace` (creation) and `replace_with_workspace`
/// (in-place loading). Validates the paths (fallback `$HOME`).
fn build_panels(ws: WorkspaceState) -> (Vec<Panel>, LayoutNode, usize) {
    let ws = ws.sanitized();
    // `sanitized()` guarantees a valid tree covering exactly the panels.
    let layout = ws.layout_tree();
    let mut panels = Vec::with_capacity(ws.panels.len());
    for ps in &ws.panels {
        let mut tabs = Vec::with_capacity(ps.tabs.len());
        for ts in &ps.tabs {
            let path = resolve_restored_path(&ts.path);
            let sort = SortState {
                column: ts.sort_column,
                order: ts.sort_order,
            };
            tabs.push(Tab::restored(
                path,
                sort,
                ts.preview,
                ts.show_hidden,
                ts.group_mode,
            ));
        }
        // `tabs` guaranteed non-empty (sanitized removes empty panels).
        let active = ps.active_tab.min(tabs.len() - 1);
        let initial_display = tabs[active].current_path.clone();
        let (rows_model, rendered_rows_model) = new_row_models();
        panels.push(Panel {
            tabs: TabBook { tabs, active },
            rows_model,
            rendered_rows_model,
            rows_revision: Cell::new(0),
            viewport_reset_gen: Cell::new(0),
            viewport_top: Cell::new(0.0),
            viewport_height: Cell::new(DEFAULT_RENDER_VIEWPORT_HEIGHT),
            rendered_first: Cell::new(0),
            rendered_end: Cell::new(0),
            displayed_path: initial_display,
            columns: columns::sanitize(ps.columns.clone()),
            hidden_count: 0,
            unavailable: false,
            tabs_viewport_x: 0.0,
            tab_bar_mode: ps.tab_bar_mode.min(2),
            vbar_user_w: ps.vbar_width.max(0.0),
            pending_initial: false,
            pending_listing: false,
            listing_gen: 0,
            pending_select: None,
        });
    }
    let active_panel = ws.active_panel.min(panels.len() - 1);
    (panels, layout, active_panel)
}

/// Resolves a path restored from the workspace: we KEEP the path as-is
/// (even if it isn't accessible — network drive not started, mount missing).
/// Only an empty path falls back to `$HOME`. The inaccessible tab shows a
/// "not found" banner via `refresh_listing` and refreshes automatically
/// as soon as the folder becomes accessible again.
fn resolve_restored_path(raw: &str) -> PathBuf {
    if raw.is_empty() {
        home_dir()
    } else {
        PathBuf::from(raw)
    }
}

/// Splits an absolute path into breadcrumb segments. Each `Crumb`
/// carries a `label` (displayed name) and the cumulative absolute `path` to
/// navigate to on click. The first segment represents the root.
///
/// - Linux: `/usr/lib` → [("/", "/"), ("usr", "/usr"), ("lib", "/usr/lib")].
/// - Windows: `C:\Users\user` → [("C:\\", "C:\\"), ("Users", "C:\\Users"),
///   ("user", "C:\\Users\\user")]. The **drive prefix** (`C:`) and the
///   **root** (`\`) are merged into ONE `C:\` segment — without this we'd get
///   a "C:" segment (drive-relative, ambiguous) followed by an incorrect "/".
fn breadcrumbs(path: &Path) -> Vec<Crumb> {
    use std::path::{Component, Prefix};
    // UNC server root `\\HOST`: `std::path` recognizes NO prefix there
    // (it requires `server\share`) → components = RootDir + Normal("HOST"), and the
    // generic rendering used to give "/ › HOST" — a "/" that makes no sense on
    // Windows. A single "\\HOST" crumb.
    #[cfg(windows)]
    if let Some(server) = rfs::unc_server_root(path) {
        let nav = format!(r"\\{server}");
        return vec![Crumb {
            label: nav.clone().into(),
            path: nav.into(),
        }];
    }
    let mut out: Vec<Crumb> = Vec::new();
    let mut acc = PathBuf::new();
    // Windows: we defer emitting the prefix (`C:`) until the root, to
    // merge them into "C:\". `pending_prefix` = a prefix seen but not yet
    // emitted.
    let mut pending_prefix = false;
    // Emits the prefix alone (degenerate case: prefix without a root, e.g. "C:foo"
    // drive-relative — unlikely since we only navigate absolute paths,
    // but we stay robust).
    macro_rules! flush_prefix {
        () => {
            if pending_prefix {
                pending_prefix = false;
                let nav = acc.display().to_string();
                out.push(Crumb {
                    label: nav.clone().into(),
                    path: nav.into(),
                });
            }
        };
    }
    // UNC share name (remembered at the prefix): the share-root crumb
    // then displays as "share" alone — the server crumb already precedes it.
    let mut unc_share: Option<String> = None;
    for comp in path.components() {
        match comp {
            Component::Prefix(p) => {
                // UNC path `\\HOST\share\…`: a clickable "\\HOST" crumb
                // leads to the server root, which enumerates the shares.
                if let Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) = p.kind() {
                    let nav = format!(r"\\{}", server.to_string_lossy());
                    out.push(Crumb {
                        label: nav.clone().into(),
                        path: nav.into(),
                    });
                    unc_share = Some(share.to_string_lossy().into_owned());
                }
                acc.push(p.as_os_str());
                pending_prefix = true;
            }
            Component::RootDir => {
                acc.push(Component::RootDir.as_os_str());
                let nav = acc.display().to_string();
                // UNC → "share" label (server already in the preceding crumb);
                // with a drive prefix (Windows) → "C:\"; otherwise (Unix) → "/".
                let label = match unc_share.take() {
                    Some(share) if !share.is_empty() => share,
                    _ if pending_prefix => nav.clone(),
                    _ => "/".to_string(),
                };
                pending_prefix = false;
                out.push(Crumb {
                    label: label.into(),
                    path: nav.into(),
                });
            }
            Component::Normal(seg) => {
                flush_prefix!();
                acc.push(seg);
                out.push(Crumb {
                    label: seg.to_string_lossy().to_string().into(),
                    path: acc.display().to_string().into(),
                });
            }
            // `.` / `..`: added as-is while staying robust (rare case
            // after canonicalization).
            other => {
                flush_prefix!();
                let s = other.as_os_str().to_string_lossy().to_string();
                if !s.is_empty() {
                    acc.push(&s);
                    out.push(Crumb {
                        label: s.clone().into(),
                        path: acc.display().to_string().into(),
                    });
                }
            }
        }
    }
    // Final flush (degenerate case "C:" without a root) — inline to avoid
    // reassigning `pending_prefix` one last time (dead assignment).
    if pending_prefix {
        let nav = acc.display().to_string();
        out.push(Crumb {
            label: nav.clone().into(),
            path: nav.into(),
        });
    }
    if out.is_empty() {
        out.push(Crumb {
            label: "/".into(),
            path: "/".into(),
        });
    }
    out
}

/// Imposed width of a tab, in logical pixels, estimated from its title.
/// The bridge is the bar's geometric source of truth: widths and
/// offsets are provided as plain values to the Slint model (`TabInfo`). The
/// drop-point computation therefore stays independent of the reactive layout and shares
/// the rendering's geometry. The estimate targets an 11 px sans-serif
/// interface font; a wider title is simply elided by the `TabItem`.
fn estimate_tab_width(title: &str) -> f32 {
    // TabItem's fixed chrome: padding-left 10 + spacing 6 + "×" slot 18 +
    // padding-right 4 = 38 px.
    const CHROME: f32 = 38.0;
    let text: f32 = title
        .chars()
        .map(|c| match c {
            'i' | 'l' | 'j' | 't' | 'f' | 'r' | '.' | ',' | '\'' | '!' | ':' | ';' | '|' | ' ' => {
                3.4
            }
            'm' | 'w' | 'M' | 'W' | '@' => 10.0,
            c if c.is_ascii_uppercase() || c.is_ascii_digit() => 7.2,
            // CJK / full-width.
            c if (c as u32) >= 0x2E80 => 11.5,
            _ => 6.0,
        })
        .sum();
    // Design bounds (e.g. TabItem's min/max-width).
    (CHROME + text).clamp(90.0, 180.0)
}

/// (width, offset) of each tab in `tabs-row` (2 px spacing), in logical
/// px — the PLAIN values pushed into `TabInfo`.
fn tab_layout(titles: &[String]) -> Vec<(f32, f32)> {
    const SPACING: f32 = 2.0; // MUST == tabs-row.spacing (slint)
    let mut off = 0.0_f32;
    titles
        .iter()
        .map(|t| {
            let w = estimate_tab_width(t);
            let cur = (w, off);
            off += w + SPACING;
            cur
        })
        .collect()
}

/// Height of a VERTICAL bar tab + spacing.
/// MUST == `TabItem.height` / `vtabs-col.spacing` on the Slint side.
const VTAB_EXTENT: f32 = 26.0;
const VTAB_SPACING: f32 = 2.0;

/// (extent, offset) of the tabs on the panel bar's **main axis**:
/// horizontal → widths estimated from the title (`tab_layout`);
/// vertical → UNIFORM step (fixed height), the offset is in Y. Same `TabInfo`
/// contract in both cases — the gap claim (Slint) and the
/// hit-tests (bridge) share this single source of truth.
fn tab_layout_for(mode: u8, titles: &[String]) -> Vec<(f32, f32)> {
    if mode == 0 {
        return tab_layout(titles);
    }
    (0..titles.len())
        .map(|i| (VTAB_EXTENT, i as f32 * (VTAB_EXTENT + VTAB_SPACING)))
        .collect()
}

/// Width, in logical pixels, of the vertical tab bar.
/// Uses the user-chosen width if positive, otherwise 40% of the
/// panel with a 180 px cap. The result stays between 110 px
/// and 60% of the panel. This computation must match `vbar-w` on the Slint side.
fn vbar_width(panel_w: f32, user_w: f32) -> f32 {
    let base = if user_w > 0.0 {
        user_w
    } else {
        (panel_w * 0.40).min(180.0)
    };
    base.clamp(110.0, (panel_w * 0.60).max(110.0))
}

/// Scroll target of the tab bar (px, `viewport-x` ≤ 0) to PROPERLY reveal
/// a tab — rather than an arbitrary distance jump. The
/// geometry (same widths/offsets as the `TabInfo` model) is recomputed here
/// (the bridge has loops, unlike Slint). `idx >= 0` → reveal
/// tab `idx` (wheel); `idx < 0` → step "tab by tab" in direction
/// `forward` (chevrons). `viewport_x`/`view_w` come from the Flickable on the Slint side.
fn tab_scroll_target(
    state: &AppState,
    panel: usize,
    idx: i32,
    forward: bool,
    viewport_x: f32,
    view_w: f32,
) -> f32 {
    let panels = state.panels.borrow();
    let Some(p) = panels.get(panel) else {
        return viewport_x;
    };
    let titles: Vec<String> = p
        .tabs
        .tabs
        .iter()
        .map(|t| tab_title(&t.current_path))
        .collect();
    // (extent, offset) on the bar's main axis — the rest of the computation is
    // agnostic to orientation (`view_w` = the same axis's visible extent).
    let geo = tab_layout_for(p.tab_bar_mode, &titles);
    let Some(&(lw, lo)) = geo.last() else {
        return viewport_x;
    };
    let content_w = lo + lw;
    if content_w <= view_w + 0.5 {
        return 0.0; // no overflow
    }
    let max_scroll = view_w - content_w; // < 0 (right end)
    let vleft = -viewport_x; // visible left edge (content coords)
    let target = if idx >= 0 {
        // Reveal tab `idx`: overflowing right edge → align right; hidden
        // left edge → align left; otherwise already visible (unchanged).
        match geo.get(idx as usize) {
            Some(&(w, off)) if off + w > vleft + view_w => view_w - (off + w),
            Some(&(_w, off)) if off < vleft => -off,
            _ => viewport_x,
        }
    } else if forward {
        // 1st tab not entirely visible on the right → align its right edge.
        geo.iter()
            .find(|(w, off)| off + w > vleft + view_w + 0.5)
            .map(|(w, off)| view_w - (off + w))
            .unwrap_or(max_scroll)
    } else {
        // Last tab hidden on the left (just before the visible zone) → left edge.
        let mut t = 0.0_f32;
        for &(_w, off) in &geo {
            if off < vleft - 0.5 {
                t = -off;
            } else {
                break;
            }
        }
        t
    };
    target.clamp(max_scroll, 0.0)
}

/// Tab title from a path. Prefers the last segment; special-cased
/// for `$HOME` and the root (`/`).
///
/// $HOME → `~` on Unix (universal convention). On Windows, `~` isn't
/// a convention known to users → we let the folder's real name
/// ("firstname.lastname") show through `file_name` below.
fn tab_title(path: &Path) -> String {
    #[cfg(not(windows))]
    if let Some(home) = dirs::home_dir()
        && path == home
    {
        return "~".to_string();
    }
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        return name.to_string();
    }
    let s = path.display().to_string();
    if s.is_empty() { "/".to_string() } else { s }
}

// Tabs are embedded in each `PanelView` via `tabs: [TabInfo]`, which
// `update_panels_ui` fills in for each panel.

// ---------- Watcher notify ----------

/// Debouncing of watcher-triggered refreshes on the UI thread.
/// A single-shot timer is restarted on every event so that a
/// burst, for example during a copy or a multi-delete, produces
/// only a single listing. This coalescing is especially important over SMB.
const WATCH_DEBOUNCE_MS: u64 = 300;
thread_local! {
    static WATCH_DEBOUNCE: slint::Timer = slint::Timer::default();
}

/// Re-arms the displayed folder's watcher. Its creation and the call to `watch()`
/// run in the background, since opening a network handle can involve an
/// SMB reconnection. The `watcher_gen` generation invalidates in-flight installs:
/// only the most recent navigation can install its watcher, and an eject
/// prevents any handle from reappearing on the removed volume.
fn install_watcher(state: &AppState, window: &MainWindow, path: &Path) {
    let weak = window.as_weak();
    let path_owned = path.to_path_buf();
    let r#gen = state.watcher_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let gen_ref = state.watcher_gen.clone();
    let event_gen_ref = gen_ref.clone();
    let slot = state.watcher.clone();
    let thumb_invalidations = state.thumb_invalidations.clone();

    std::thread::spawn(move || {
        let result: notify::Result<RecommendedWatcher> = RecommendedWatcher::new(
            move |res: notify::Result<Event>| match res {
                Ok(ev) => {
                    if matches!(
                        ev.kind,
                        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(_)
                    ) {
                        thumb_invalidations
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner())
                            .extend(ev.paths);
                        let weak = weak.clone();
                        let event_gen_ref = event_gen_ref.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            WATCH_DEBOUNCE.with(|t| {
                                t.start(
                                    slint::TimerMode::SingleShot,
                                    Duration::from_millis(WATCH_DEBOUNCE_MS),
                                    move || {
                                        let Some(w) = weak.upgrade() else { return };
                                        if event_gen_ref.load(Ordering::SeqCst) != r#gen {
                                            return; // watcher for a folder already left
                                        }
                                        // Suspends refreshes during a tab
                                        // drag, since rebuilding the TabItems would cancel
                                        // their pointer capture. A long operation also
                                        // suspends them, to avoid re-listing on every write;
                                        // `run_heavy` calls `refresh_all` at its end.
                                        if w.get_file_drag_active() {
                                            w.set_file_drag_refresh_pending(true);
                                            return;
                                        }
                                        let busy = w.get_drag_active() || w.get_op_busy();
                                        if !busy {
                                            w.invoke_refresh();
                                        }
                                    },
                                );
                            });
                        });
                    }
                }
                Err(err) => warn!(error = %err, "watcher error"),
            },
            notify::Config::default(),
        );
        match result {
            Ok(mut w) => {
                if let Err(err) = w.watch(&path_owned, RecursiveMode::NonRecursive) {
                    warn!(error = %err, path = %path_owned.display(), "watcher watch() failed");
                } else if let Ok(mut guard) = slot.lock()
                    && gen_ref.load(Ordering::SeqCst) == r#gen
                {
                    // Replaces (and drops) the previous one HERE, on the
                    // background thread — its possible network handle doesn't block the UI.
                    *guard = Some(w);
                }
            }
            Err(err) => warn!(error = %err, "watcher creation failed"),
        }
    });
}

// ---------- Window size persistence ----------

fn should_persist_window_dimensions(maximized: bool, fullscreen: bool, minimized: bool) -> bool {
    !maximized && !fullscreen && !minimized
}

/// Converts the physical client size back to OS-logical units for persistence.
/// Ranger's UI zoom changes Slint's effective scale factor, but it must not
/// change the window size saved in the global configuration.
fn persisted_logical_window_size(
    physical: (u32, u32),
    captured_os_scale: f32,
    effective_scale: f32,
) -> (u32, u32) {
    let scale = if captured_os_scale > 0.0 {
        captured_os_scale
    } else {
        // The fallback only applies if the window closes before UI zoom has
        // captured the native DPI scale during startup.
        effective_scale.max(0.01)
    };
    (
        (physical.0 as f32 / scale).round() as u32,
        (physical.1 as f32 / scale).round() as u32,
    )
}

pub fn persist_window_size(window: &MainWindow, state: &AppState) {
    let native_window = window.window();
    let persist_dimensions = should_persist_window_dimensions(
        native_window.is_maximized(),
        native_window.is_fullscreen(),
        native_window.is_minimized(),
    );
    let logical_size = persist_dimensions.then(|| {
        let size = native_window.size();
        persisted_logical_window_size(
            (size.width, size.height),
            state.ui_base_scale.get(),
            native_window.scale_factor(),
        )
    });

    if !persist_dimensions {
        debug!(
            maximized = native_window.is_maximized(),
            fullscreen = native_window.is_fullscreen(),
            minimized = native_window.is_minimized(),
            "preserving last normal window size"
        );
    }

    // Left panel: open state + width.
    let left_panel = window.get_left_panel();
    let sidebar_width = window.get_sidebar_width().round().max(0.0) as u32;

    let cfg = state.snapshot_config();
    let dimensions_changed = logical_size
        .map(|(w, h)| cfg.window_width != w || cfg.window_height != h)
        .unwrap_or(false);
    if dimensions_changed
        || cfg.left_panel != left_panel
        || (sidebar_width > 0 && cfg.sidebar_width != sidebar_width)
    {
        state.persist_config(|c| {
            if let Some((w, h)) = logical_size {
                c.window_width = w;
                c.window_height = h;
            }
            c.left_panel = left_panel;
            if sidebar_width > 0 {
                c.sidebar_width = sidebar_width;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monochrome_shell_icon_detection() {
        // Monochrome glyph (grey, opaque) → should be re-tinted.
        let mono = vec![40, 40, 40, 255, 200, 200, 200, 255];
        assert!(shell_icon_monochrome(&Some((mono, 2, 1))));
        // Coloured icon (one saturated red pixel) → left intact.
        let color = vec![220, 20, 20, 255, 30, 30, 30, 255];
        assert!(!shell_icon_monochrome(&Some((color, 2, 1))));
        // Fully transparent / empty → no tint (nothing to colour).
        let empty = vec![10, 10, 10, 0, 200, 200, 200, 10];
        assert!(!shell_icon_monochrome(&Some((empty, 2, 1))));
        assert!(!shell_icon_monochrome(&None));
    }

    fn op_handle(cancel: Option<Arc<AtomicBool>>) -> OpHandle {
        OpHandle {
            cancel,
            pending_focus: None,
            targets: Vec::new(),
            thumbnail_invalidations: Vec::new(),
            transient_cleanup: None,
        }
    }

    fn op_handle_writing(targets: &[&str]) -> OpHandle {
        OpHandle {
            cancel: None,
            pending_focus: None,
            targets: targets.iter().map(PathBuf::from).collect(),
            thumbnail_invalidations: Vec::new(),
            transient_cleanup: None,
        }
    }

    #[test]
    fn op_registry_tracks_each_operation_under_its_own_id() {
        let ops = OpRegistry::default();
        assert!(!ops.in_flight());

        let first = ops.register(op_handle(None));
        let second = ops.register(op_handle(None));
        assert_ne!(first, second, "ids must distinguish concurrent operations");
        assert!(ops.in_flight());

        // Finishing one leaves the other running.
        assert!(ops.finish(first).is_some());
        assert!(ops.in_flight());
        assert!(ops.finish(second).is_some());
        assert!(!ops.in_flight());
    }

    #[test]
    fn op_registry_ignores_a_completion_delivered_twice() {
        let ops = OpRegistry::default();
        let id = ops.register(op_handle(None));
        assert!(ops.finish(id).is_some());
        // A second delivery must not release an unrelated operation.
        let other = ops.register(op_handle(None));
        assert!(ops.finish(id).is_none());
        assert!(ops.in_flight());
        assert!(ops.finish(other).is_some());
    }

    #[test]
    fn op_registry_returns_the_focus_target_of_that_operation_only() {
        let ops = OpRegistry::default();
        let with_focus = ops.register(OpHandle {
            cancel: None,
            pending_focus: Some(PathBuf::from("/tmp/copied.txt")),
            targets: Vec::new(),
            thumbnail_invalidations: Vec::new(),
            transient_cleanup: None,
        });
        let without = ops.register(op_handle(None));

        assert_eq!(ops.finish(without).and_then(|h| h.pending_focus), None);
        assert_eq!(
            ops.finish(with_focus).and_then(|h| h.pending_focus),
            Some(PathBuf::from("/tmp/copied.txt"))
        );
    }

    #[test]
    fn a_running_operation_holds_its_destinations_until_it_ends() {
        let ops = OpRegistry::default();
        let copying = ops.register(op_handle_writing(&["/dst/photo.jpg", "/dst/notes.txt"]));

        // Claimed from the start — well before the files exist on disk, which is
        // exactly when a second paste would otherwise pick the same name.
        assert!(ops.is_reserved(Path::new("/dst/photo.jpg")));
        assert!(ops.is_reserved(Path::new("/dst/notes.txt")));
        assert!(!ops.is_reserved(Path::new("/dst/other.jpg")));

        // Released together with the operation, so the names free up again.
        assert!(ops.finish(copying).is_some());
        assert!(!ops.is_reserved(Path::new("/dst/photo.jpg")));
        assert!(!ops.is_reserved(Path::new("/dst/notes.txt")));
    }

    #[test]
    fn concurrent_operations_release_only_their_own_destinations() {
        let ops = OpRegistry::default();
        let first = ops.register(op_handle_writing(&["/dst/a.txt"]));
        let _second = ops.register(op_handle_writing(&["/dst/b.txt"]));

        assert!(ops.finish(first).is_some());
        assert!(!ops.is_reserved(Path::new("/dst/a.txt")));
        // The operation still running keeps its own claim.
        assert!(ops.is_reserved(Path::new("/dst/b.txt")));
    }

    #[test]
    fn a_deletion_claims_nothing_since_it_writes_nowhere() {
        let ops = OpRegistry::default();
        let deleting = ops.register(op_handle(None));
        assert!(ops.in_flight());
        assert!(!ops.is_reserved(Path::new("/dst/anything")));
        assert!(ops.finish(deleting).is_some());
    }

    #[test]
    fn a_free_name_steps_over_the_destination_of_a_running_operation() {
        let ops = OpRegistry::default();
        // Nothing exists on disk here, so only the reservation can push the
        // name forward — a plain `exists()` check would hand back the same path.
        let taken_path = std::env::temp_dir().join("ranger-op-reservation-test.txt");
        ops.register(op_handle_writing(&[taken_path.to_str().unwrap()]));

        let picked = ops::unique_sibling_where(&taken_path, |p| p.exists() || ops.is_reserved(p));
        assert_ne!(picked, taken_path, "must not reuse a claimed destination");
        assert_eq!(picked.parent(), taken_path.parent());
    }

    #[test]
    fn the_extension_field_accepts_the_spellings_users_actually_type() {
        // Plain, dotted and shell-style all reach the same stored form. The
        // shell one used to be kept verbatim and matched nothing, so the entry
        // silently disappeared from the menu.
        assert_eq!(parse_exts("zip"), ["zip"]);
        assert_eq!(parse_exts(".zip"), ["zip"]);
        assert_eq!(parse_exts("*.zip"), ["zip"]);
        assert_eq!(parse_exts("ZIP"), ["zip"]);
        // A lone star is the wildcard and must survive untouched.
        assert_eq!(parse_exts("*"), [openers::CTX_EXT_ALL]);
        // Mixed list, separators and spacing.
        assert_eq!(parse_exts("*.zip, .rar ; 7z"), ["zip", "rar", "7z"]);
        assert!(parse_exts("   ").is_empty());
    }

    #[test]
    fn the_context_entry_filter_only_narrows_files_and_needs_every_one_to_match() {
        let archive_only = openers::Opener {
            id: "x".into(),
            label: "Extract".into(),
            program: "7z".into(),
            assoc: None,
            icon: openers::OpenerIcon::None,
            args: vec![],
            default_exts: vec![],
            used_exts: vec![],
            use_count: 0,
            last_used: 0,
            elevated: false,
            ctx_menu: openers::CTX_FILE,
            ctx_exts: vec!["zip".into(), "7z".into()],
        };
        let zip = PathBuf::from("/tmp/a.zip");
        let txt = PathBuf::from("/tmp/a.txt");

        assert!(ctx_entry_applies(
            &archive_only,
            openers::CTX_FILE,
            std::slice::from_ref(&zip)
        ));
        assert!(!ctx_entry_applies(
            &archive_only,
            openers::CTX_FILE,
            std::slice::from_ref(&txt)
        ));
        // A mixed selection hides it: the command would run on the odd one out
        // and fail there.
        assert!(!ctx_entry_applies(
            &archive_only,
            openers::CTX_FILE,
            &[zip.clone(), txt.clone()]
        ));

        // Folders and the view background have no extension — never filtered.
        assert!(ctx_entry_applies(
            &archive_only,
            openers::CTX_DIR,
            std::slice::from_ref(&txt)
        ));
        assert!(ctx_entry_applies(
            &archive_only,
            openers::CTX_BACKGROUND,
            &[]
        ));

        // An unrestricted command shows up on anything, as before.
        let anything = openers::Opener {
            ctx_exts: vec![],
            ..archive_only.clone()
        };
        assert!(ctx_entry_applies(&anything, openers::CTX_FILE, &[txt]));
    }

    #[test]
    fn extract_recipes_are_limited_to_archives_and_the_others_are_not() {
        for r in RECIPES {
            let extracts = r.label_key.starts_with("ow_recipe_extract");
            if extracts {
                assert_eq!(
                    r.ctx_exts,
                    rfs::ARCHIVE_EXTENSIONS,
                    "{} should only show on archives",
                    r.label_key
                );
            } else {
                assert_eq!(
                    r.ctx_exts,
                    [openers::CTX_EXT_ALL],
                    "{} should show on every file",
                    r.label_key
                );
            }
        }
        // The filter is only consulted for the FILE entry, so a recipe that
        // narrows extensions without claiming that menu would be inert.
        for r in RECIPES
            .iter()
            .filter(|r| r.ctx_exts != [openers::CTX_EXT_ALL])
        {
            assert!(
                r.ctx & openers::CTX_FILE != 0,
                "{} restricts extensions but is not pinned to files",
                r.label_key
            );
        }
    }

    #[test]
    fn every_recipe_has_one_of_the_four_builtin_icon_families() {
        let mut kinds = std::collections::BTreeSet::new();
        for recipe in RECIPES {
            assert_ne!(recipe.icon, openers::OpenerIcon::None);
            kinds.insert(recipe.icon.as_i32());
        }
        assert_eq!(kinds, [1, 2, 3, 4].into_iter().collect());
    }

    #[test]
    fn every_recipe_is_translated_and_expresses_its_intended_mode() {
        // Actions that must cover the whole selection in a single run; every
        // other one runs per selected item.
        const RUN_ONCE: &[&str] = &[
            "ow_recipe_compress_zip",
            "ow_recipe_compress_targz",
            "ow_recipe_share_bluetooth",
        ];

        for r in RECIPES {
            // A recipe whose label falls back to its own key would show the raw
            // key in the settings. Its visible label must remain exactly the
            // translated action in every bundled language: the icon identifies
            // the tool, so adding a textual prefix would be redundant.
            for lang in [Lang::En, Lang::Fr, Lang::Es, Lang::De, Lang::It] {
                let action = i18n::tr(lang, r.label_key);
                assert_ne!(action, r.label_key, "missing translation: {}", r.label_key);
                assert_eq!(r.label(lang), action);
            }
            assert!(r.ctx != 0, "a recipe nobody can reach: {}", r.label_key);
            assert!(
                !r.programs.is_empty(),
                "a recipe with no program can never appear: {}",
                r.label_key
            );

            // Each recipe must land in the mode it was written for; the args
            // alone decide that, so a typo would silently flip it.
            let o = openers::Opener {
                id: String::new(),
                label: String::new(),
                program: r.programs[0].into(),
                assoc: None,
                // Simulates an opener saved before recipe icons were persisted.
                icon: openers::OpenerIcon::None,
                args: split_args(r.args),
                default_exts: Vec::new(),
                used_exts: Vec::new(),
                use_count: 0,
                last_used: 0,
                elevated: false,
                ctx_menu: r.ctx,
                ctx_exts: Vec::new(),
            };
            assert_eq!(effective_opener_icon(&o), r.icon);
            if RUN_ONCE.contains(&r.label_key) {
                assert!(o.expands_list(), "{} must run once", r.label_key);
            } else {
                assert!(
                    !o.expands_list() && o.has_tag(),
                    "{} must run once per selected item",
                    r.label_key
                );
            }
            // No recipe may fall through to the historical mode that silently
            // appends every path at the end — each states what it wants.
            assert!(
                o.expands_list() || o.has_tag(),
                "{} would fall back to appending paths blindly",
                r.label_key
            );
        }

        // One action key is shared by several tools ("Extract here" serves 7z
        // and tar). They must agree on the mode, otherwise the same wording
        // would behave differently depending on which chip was picked.
        for r in RECIPES {
            for other in RECIPES.iter().filter(|o| o.label_key == r.label_key) {
                assert_eq!(
                    split_args(r.args).iter().any(|a| a == openers::LIST_TAG),
                    split_args(other.args)
                        .iter()
                        .any(|a| a == openers::LIST_TAG),
                    "{} runs differently for {} and {}",
                    r.label_key,
                    r.tool,
                    other.tool
                );
            }
        }
    }

    #[test]
    fn the_preview_states_how_many_processes_will_start() {
        let en = Lang::En;
        // No tag: one process receives the whole selection, whatever its size.
        assert_eq!(run_count_text(en, false, 5), "Will run once");
        // A tag makes it one process per item — the rule this line exists to expose.
        assert!(run_count_text(en, true, 3).contains('3'));
        assert_eq!(run_count_text(en, true, 1), "Will run once");
        // Nothing selected (from Settings): state the rule, not a count of zero.
        assert_eq!(
            run_count_text(en, true, 0),
            "Runs once per selected item",
            "an empty selection must not read as 'will run 0 times'"
        );
    }

    #[test]
    fn quoted_arguments_survive_the_split_and_stay_one_argument() {
        // The plain case is unchanged.
        assert_eq!(
            split_args("a {dir}/x.7z {file}"),
            ["a", "{dir}/x.7z", "{file}"]
        );
        // Quotes group a run: without this, an archive name containing a space
        // reached the program as two mangled arguments.
        assert_eq!(
            split_args(r#"a "{dir}/Mon Archive.7z" {file}"#),
            ["a", "{dir}/Mon Archive.7z", "{file}"]
        );
        // Single quotes work too, and may open mid-argument.
        assert_eq!(split_args("-o'My Folder'"), ["-oMy Folder"]);
        assert_eq!(split_args(r#"--out="a b" c"#), ["--out=a b", "c"]);
        // A quote of the other kind inside a quoted run is literal.
        assert_eq!(split_args(r#""it's here""#), ["it's here"]);
        // Unbalanced input is tolerated: the field is unbalanced while typing.
        assert_eq!(split_args(r#"a "unterminated"#), ["a", "unterminated"]);
        // Runs of whitespace collapse, and an empty field yields no argument.
        assert_eq!(split_args("  a   b  "), ["a", "b"]);
        assert!(split_args("   ").is_empty());
        // An explicitly empty argument is preserved.
        assert_eq!(split_args(r#"a "" b"#), ["a", "", "b"]);
    }

    #[test]
    fn the_preview_keeps_argument_boundaries_visible() {
        // A single path containing spaces must not read as two arguments.
        let argv = vec![
            "a".to_string(),
            "/tmp/Mon Archive.7z".to_string(),
            "/tmp/x.txt".to_string(),
        ];
        assert_eq!(display_argv(&argv), r#"a "/tmp/Mon Archive.7z" /tmp/x.txt"#);
        // Nothing is quoted when nothing needs it.
        assert_eq!(display_argv(&["a".into(), "b".into()]), "a b");
    }

    #[test]
    fn a_batch_never_sends_two_items_to_the_same_destination() {
        // `x` and `x - Copy01` both reduce to the base `x`, so picking their
        // names independently hands both `x - Copy02` — one result would
        // overwrite the other. Nothing exists on disk here, so only the
        // batch's own bookkeeping can separate them.
        let dir = std::env::temp_dir().join("ranger-batch-target-test");
        let sources = vec![dir.join("notes.txt"), dir.join("notes - Copy01.txt")];

        let picked = plan_unique_targets(&sources, |_| false);
        assert_eq!(picked.len(), 2);
        assert_ne!(
            picked[0], picked[1],
            "two items of one batch must not share a destination"
        );

        // Names already spoken for are stepped over as well.
        let blocked = plan_unique_targets(&sources[..1], |p| {
            p.file_name().is_some_and(|n| n == "notes.txt")
        });
        assert_ne!(blocked[0], sources[0]);
    }

    #[test]
    fn a_reservation_only_takes_a_name_that_was_otherwise_free() {
        use EntryNameAvailability as A;
        // Nothing on disk, but a running operation will write there.
        assert_eq!(with_reservation(A::Available, true), A::Reserved);
        assert_eq!(with_reservation(A::Available, false), A::Available);
        // A real entry keeps its own kind: it is what the dialogs report on,
        // and masking it would hide the "replace this file" choice.
        assert_eq!(
            with_reservation(A::ExistingNonDirectory, true),
            A::ExistingNonDirectory
        );
        assert_eq!(
            with_reservation(A::ExistingDirectory, true),
            A::ExistingDirectory
        );
        // A syntactically invalid name stays invalid whatever is reserved.
        assert_eq!(with_reservation(A::Invalid, true), A::Invalid);
    }

    #[test]
    fn renaming_onto_a_reserved_name_never_offers_force_replace() {
        use RenameNameStatus as R;
        // No entry exists there to replace, and the running operation would
        // overwrite the renamed item as soon as it reached that path.
        assert_eq!(rename_with_reservation(R::Valid, true), R::Conflict);
        assert_eq!(rename_with_reservation(R::Valid, false), R::Valid);
        // A real file conflict keeps offering the replace route.
        assert_eq!(
            rename_with_reservation(R::ReplaceableFile, false),
            R::ReplaceableFile
        );
        assert_eq!(rename_with_reservation(R::Invalid, true), R::Invalid);
    }

    #[test]
    fn toast_stack_summarises_only_what_it_cannot_show() {
        // Up to the cap every operation gets its own card, so no chip.
        assert_eq!(hidden_toast_count(0), 0);
        assert_eq!(hidden_toast_count(MAX_VISIBLE_TOASTS), 0);
        // Past it, the chip accounts for exactly the ones left out.
        assert_eq!(hidden_toast_count(MAX_VISIBLE_TOASTS + 1), 1);
        assert_eq!(hidden_toast_count(MAX_VISIBLE_TOASTS + 7), 7);
    }

    #[test]
    fn op_registry_cancels_only_the_targeted_operation() {
        let ops = OpRegistry::default();
        let first_flag = Arc::new(AtomicBool::new(false));
        let second_flag = Arc::new(AtomicBool::new(false));
        let first = ops.register(op_handle(Some(first_flag.clone())));
        let _second = ops.register(op_handle(Some(second_flag.clone())));

        ops.cancel(first);
        assert!(first_flag.load(Ordering::Relaxed));
        assert!(!second_flag.load(Ordering::Relaxed));

        // An unknown id, and an operation without a cancel flag, are no-ops.
        ops.cancel(first + 1000);
        let uninterruptible = ops.register(op_handle(None));
        ops.cancel(uninterruptible);
        assert!(!second_flag.load(Ordering::Relaxed));
    }

    #[test]
    #[cfg(windows)]
    fn image_gallery_is_windows_only_and_image_only() {
        assert!(should_try_image_gallery("jpg"));
        assert!(should_try_image_gallery("PNG"));
        assert!(!should_try_image_gallery("txt"));
        assert!(!should_try_image_gallery("mp4"));
    }

    #[test]
    fn contextual_open_and_duplicate_share_adjacent_tab_insertion() {
        let mut book = TabBook::single(PathBuf::from("A"));
        book.open(PathBuf::from("B"));
        book.open(PathBuf::from("C"));
        assert!(book.select(0));

        let opened = book.open_after_active(PathBuf::from("X"));
        assert_eq!(opened, 1);
        assert_eq!(book.active, 1);
        assert_eq!(
            book.tabs
                .iter()
                .map(|tab| tab.current_path.as_path())
                .collect::<Vec<_>>(),
            ["A", "X", "B", "C"].map(Path::new)
        );

        assert!(book.duplicate(2));
        assert_eq!(book.active, 3);
        assert_eq!(
            book.tabs
                .iter()
                .map(|tab| tab.current_path.as_path())
                .collect::<Vec<_>>(),
            ["A", "X", "B", "B", "C"].map(Path::new)
        );
    }

    #[test]
    fn filename_caret_stops_before_a_real_extension() {
        assert_eq!(filename_caret_offset("toti.p3d", false), 4);
        assert_eq!(
            filename_caret_offset("résumé.txt", false),
            "résumé".len() as i32
        );
    }

    #[test]
    fn filename_caret_keeps_folders_dotfiles_and_numeric_suffixes_whole() {
        for (name, is_dir) in [
            ("folder.with.dot", true),
            (".bashrc", false),
            ("data.2024", false),
            ("README", false),
        ] {
            assert_eq!(
                filename_caret_offset(name, is_dir),
                name.len() as i32,
                "{name}"
            );
        }
    }

    fn thumb_test_request(path: &str, panel: usize, row: usize, row_count: usize) -> ThumbRequest {
        ThumbRequest {
            job: ThumbJob {
                path: PathBuf::from(path),
                kind: FileKind::Image,
                svg: false,
            },
            locations: vec![ThumbLocation {
                panel,
                row,
                row_count,
            }],
        }
    }

    #[test]
    fn thumbnail_scroll_reprioritizes_the_next_job() {
        let scheduler = ThumbScheduler::new();
        scheduler.replace_pending(
            (0..20)
                .map(|row| thumb_test_request(&format!("image-{row:02}.png"), 0, row, 20))
                .collect(),
        );
        scheduler.update_viewport(0, 0, 1);

        // The first decode has already started when the user jumps to the bottom.
        let first = scheduler.try_take_next().unwrap();
        assert_eq!(first.path, PathBuf::from("image-00.png"));
        // Several intermediate positions may be published during a
        // fast scroll: only the most recent one should drive the next choice.
        scheduler.update_viewport(0, 7, 8);
        scheduler.update_viewport(0, 12, 13);
        scheduler.update_viewport(0, 18, 19);
        scheduler.complete(&first.path, first.serial);

        // The next one comes from the new visible zone, not the old FIFO.
        let next = scheduler.try_take_next().unwrap();
        assert_eq!(next.path, PathBuf::from("image-18.png"));
        scheduler.complete(&next.path, next.serial);
        let next = scheduler.try_take_next().unwrap();
        assert_eq!(next.path, PathBuf::from("image-19.png"));
        scheduler.complete(&next.path, next.serial);
    }

    #[test]
    fn thumbnail_pool_takes_each_job_exactly_once_under_concurrency() {
        use std::sync::Mutex;
        // Invariant that makes the pool safe: several workers pull in
        // parallel, but `take_next` removes the path from `pending` UNDER LOCK →
        // no path is decoded twice, none is lost. We stress it
        // with lots of jobs and more threads than the real pool.
        const JOBS: usize = 500;
        let scheduler = Arc::new(ThumbScheduler::new());
        scheduler.replace_pending(
            (0..JOBS)
                .map(|row| thumb_test_request(&format!("img-{row:04}.png"), 0, row, JOBS))
                .collect(),
        );

        let taken: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::with_capacity(JOBS)));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let sched = scheduler.clone();
            let taken = taken.clone();
            handles.push(std::thread::spawn(move || {
                // `try_take_next` returns `None` on an empty queue (clean test exit);
                // in production it's `take_next`, blocking, that waits for the next job.
                while let Some(job) = sched.try_take_next() {
                    taken
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(job.path.clone());
                    sched.complete(&job.path, job.serial);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let mut paths = taken.lock().unwrap_or_else(|e| e.into_inner()).clone();
        assert_eq!(paths.len(), JOBS, "each job taken exactly once");
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), JOBS, "no duplicate decode");
    }

    #[test]
    fn thumbnail_requests_are_deduplicated_and_stale_paths_are_removed() {
        let scheduler = ThumbScheduler::new();
        scheduler.replace_pending(vec![
            thumb_test_request("shared.png", 0, 3, 10),
            thumb_test_request("shared.png", 1, 1, 10),
            thumb_test_request("obsolete.png", 0, 4, 10),
        ]);
        scheduler.replace_pending(vec![
            thumb_test_request("shared.png", 0, 3, 10),
            thumb_test_request("shared.png", 1, 1, 10),
        ]);
        scheduler.update_viewport(1, 1, 2);

        let only = scheduler.try_take_next().unwrap();
        assert_eq!(only.path, PathBuf::from("shared.png"));
        // A refresh during decoding must not create a duplicate.
        scheduler.replace_pending(vec![thumb_test_request("shared.png", 1, 1, 10)]);
        scheduler.complete(&only.path, only.serial);
        assert!(scheduler.try_take_next().is_none());
    }

    #[test]
    fn invalidated_thumbnail_generation_cannot_complete_a_new_request() {
        let scheduler = ThumbScheduler::new();
        let path = PathBuf::from("gallery").join("Image02.jpg");
        scheduler.replace_pending(vec![thumb_test_request(
            path.to_string_lossy().as_ref(),
            0,
            0,
            1,
        )]);
        let stale = scheduler.try_take_next().expect("stale generation");

        scheduler.invalidate_paths(std::slice::from_ref(&path));
        scheduler.merge_pending(vec![thumb_test_request(
            path.to_string_lossy().as_ref(),
            0,
            0,
            1,
        )]);
        let current = scheduler.try_take_next().expect("current generation");
        assert_ne!(stale.serial, current.serial);
        assert!(
            scheduler
                .in_flight_locations(&path, stale.serial)
                .is_empty(),
            "an invalidated decode must lose ownership of the path"
        );
        assert_eq!(
            scheduler.in_flight_locations(&path, current.serial).len(),
            1
        );

        // A late callback from the old decode cannot acknowledge/remove the
        // replacement job that now owns the same path.
        scheduler.complete(&path, stale.serial);
        assert_eq!(
            scheduler.in_flight_locations(&path, current.serial).len(),
            1
        );
        scheduler.complete(&path, current.serial);
    }

    #[test]
    fn thumbnail_lru_removes_only_the_reused_path() {
        let image = image_from_thumb(&Thumbnail {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        });
        let reused = PathBuf::from("gallery").join("Image02.jpg");
        let untouched = PathBuf::from("gallery").join("Image03.jpg");
        let mut cache = ThumbLru::new(8);
        cache.put(reused.to_string_lossy().into_owned(), image.clone());
        cache.put(untouched.to_string_lossy().into_owned(), image);

        cache.remove_path(&reused);

        assert!(cache.get(reused.to_string_lossy().as_ref()).is_none());
        assert!(cache.get(untouched.to_string_lossy().as_ref()).is_some());
    }

    #[test]
    fn thumbnail_idempotent_merge_keeps_the_ready_heap_intact() {
        let scheduler = ThumbScheduler::new();
        scheduler.replace_pending(vec![thumb_test_request("stable.png", 0, 4, 10)]);
        {
            let mut queue = scheduler.queue.lock().unwrap_or_else(|e| e.into_inner());
            scheduler.ensure_ready(&mut queue);
            assert_eq!(queue.ready.len(), 1);
            assert!(!scheduler.priorities_dirty.load(Ordering::Acquire));
        }

        // Hot path of the viewport callback: the visible row is already known at the
        // same location. No O(n) rebuild, no worker wake-up needed.
        scheduler.merge_pending(vec![thumb_test_request("stable.png", 0, 4, 10)]);
        let queue = scheduler.queue.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(queue.ready.len(), 1);
        assert!(!scheduler.priorities_dirty.load(Ordering::Acquire));
    }

    #[test]
    fn thumbnail_navigation_keeps_new_queue_after_stale_job_finishes() {
        let scheduler = ThumbScheduler::new();
        scheduler.replace_pending(vec![thumb_test_request("old-folder.png", 0, 0, 1)]);
        let stale = scheduler.try_take_next().unwrap();

        // Navigation during decoding: the new request replaces the whole
        // old queue, but the already-started job can finish without polluting it.
        scheduler.replace_pending(
            (0..6)
                .map(|row| thumb_test_request(&format!("new-{row}.png"), 0, row, 6))
                .collect(),
        );
        scheduler.update_viewport(0, 5, 5);
        scheduler.complete(&stale.path, stale.serial);

        let next = scheduler.try_take_next().unwrap();
        assert_eq!(next.path, PathBuf::from("new-5.png"));
        scheduler.complete(&next.path, next.serial);
        assert_ne!(next.path, stale.path);
    }

    #[test]
    fn visible_thumbnail_holes_are_reconciled_when_render_window_is_unchanged() {
        let gallery = PathBuf::from("gallery");
        let state = AppState::new_at(Config::default(), gallery.clone(), 0);
        let (top, height, expected) = {
            let mut panels = state.panels.borrow_mut();
            let panel = &mut panels[0];
            panel.tabs.tabs[0].preview = true;
            panel.tabs.tabs[0].zoom = THUMB_DEFAULT_ZOOM;

            let mut rows: Vec<FileRow> = (0..100)
                .map(|index| FileRow {
                    name: format!("image-{index:03}.png").into(),
                    ext: "png".into(),
                    kind: FileKind::Image.as_i32(),
                    preview_capable: true,
                    ..Default::default()
                })
                .collect();
            layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
            let top = rows[50].visual_y;
            let height = rows[50].visual_h * 2.0;
            let visible = row_range_for_slice(&rows, top, top + height);
            let expected = (visible.0..visible.1)
                .map(|index| gallery.join(format!("image-{index:03}.png")))
                .collect::<Vec<_>>();
            panel.viewport_top.set(top);
            panel.viewport_height.set(height);
            panel.replace_rows(rows);
            // Reproduces the zoom relayout: `replace_rows` has already pre-marked
            // exactly the window that the callback is going to republish.
            assert_eq!(
                (panel.rendered_first.get(), panel.rendered_end.get()),
                row_range_for_content_span(
                    &*panel.rows_model,
                    (top - height).max(0.0),
                    top + height * 2.0,
                )
            );
            (top, height, expected)
        };

        // Same range twice: the second reconciliation must neither lose nor
        // duplicate the already-pending requests.
        update_panel_render_window(&state, 0, top, height);
        update_panel_render_window(&state, 0, top, height);

        let mut actual = Vec::new();
        while let Some(job) = state.thumb_scheduler.try_take_next() {
            actual.push(job.path.clone());
            state.thumb_scheduler.complete(&job.path, job.serial);
        }
        actual.sort();
        let mut expected = expected;
        expected.sort();
        assert_eq!(actual, expected);
    }

    #[test]
    fn cached_offscreen_thumbnails_are_not_redecoded_by_a_global_refresh() {
        let gallery = PathBuf::from("gallery-cache");
        let state = AppState::new_at(Config::default(), gallery.clone(), 0);
        let cached_image = image_from_thumb(&Thumbnail {
            width: 1,
            height: 1,
            rgba: vec![1, 2, 3, 255],
        });
        {
            let mut panels = state.panels.borrow_mut();
            let panel = &mut panels[0];
            panel.tabs.tabs[0].preview = true;
            panel.tabs.tabs[0].zoom = THUMB_DEFAULT_ZOOM;
            panel.viewport_height.set(76.0);
            let mut rows: Vec<FileRow> = (0..40)
                .map(|index| FileRow {
                    name: format!("cached-{index:02}.png").into(),
                    ext: "png".into(),
                    kind: FileKind::Image.as_i32(),
                    preview_capable: true,
                    ..Default::default()
                })
                .collect();
            layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
            panel.replace_rows(rows);
        }
        for index in 0..40 {
            let key = gallery
                .join(format!("cached-{index:02}.png"))
                .to_string_lossy()
                .into_owned();
            state
                .thumb_cache
                .borrow_mut()
                .put(key, cached_image.clone());
        }

        request_thumbnails(&state);
        assert!(
            state.thumb_scheduler.try_take_next().is_none(),
            "a texture already in the LRU must not be re-decoded off-screen"
        );
    }

    #[test]
    fn only_normal_windows_persist_their_dimensions() {
        assert!(should_persist_window_dimensions(false, false, false));
        assert!(!should_persist_window_dimensions(true, false, false));
        assert!(!should_persist_window_dimensions(false, true, false));
        assert!(!should_persist_window_dimensions(false, false, true));
    }

    #[test]
    fn window_persistence_ignores_ranger_ui_zoom() {
        // 150% OS DPI and 110% Ranger zoom produce an effective Slint scale
        // of 1.65. Persistence must divide by 1.5, not by 1.65.
        assert_eq!(
            persisted_logical_window_size((1650, 1050), 1.5, 1.65),
            (1100, 700)
        );
    }

    #[test]
    fn window_persistence_has_a_pre_zoom_startup_fallback() {
        assert_eq!(
            persisted_logical_window_size((1375, 875), 0.0, 1.25),
            (1100, 700)
        );
    }

    /// Reduces a `Vec<Crumb>` to comparable `(label, path)` pairs.
    fn pairs(path: &Path) -> Vec<(String, String)> {
        breadcrumbs(path)
            .iter()
            .map(|c| (c.label.to_string(), c.path.to_string()))
            .collect()
    }

    #[cfg(unix)]
    #[test]
    fn breadcrumbs_unix_absolute() {
        assert_eq!(
            pairs(Path::new("/usr/lib")),
            vec![
                ("/".into(), "/".into()),
                ("usr".into(), "/usr".into()),
                ("lib".into(), "/usr/lib".into()),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn breadcrumbs_unix_root() {
        assert_eq!(pairs(Path::new("/")), vec![("/".into(), "/".into())]);
    }

    /// Windows: the drive prefix and the root merge into "C:\", and
    /// each segment accumulates a valid navigable path (no incorrect "/").
    #[cfg(windows)]
    #[test]
    fn breadcrumbs_windows_drive() {
        assert_eq!(
            pairs(Path::new(r"C:\Users\user")),
            vec![
                (r"C:\".into(), r"C:\".into()),
                ("Users".into(), r"C:\Users".into()),
                ("user".into(), r"C:\Users\user".into()),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn breadcrumbs_windows_drive_root() {
        assert_eq!(
            pairs(Path::new(r"C:\")),
            vec![(r"C:\".into(), r"C:\".into())]
        );
    }

    #[test]
    fn upward_navigation_reveals_only_the_folder_just_left() {
        let parent = PathBuf::from("Lunarya").join("Vorlune");
        let child = parent.join("Norvak");

        assert_eq!(
            upward_navigation_child_name(&parent, &child).as_deref(),
            Some("Norvak")
        );
        assert_eq!(
            upward_navigation_child_name(&parent, &child.join("deeper")),
            None
        );
        assert_eq!(
            upward_navigation_child_name(&parent, &PathBuf::from("Lunarya").join("Calyss")),
            None
        );
    }

    // Tab geometry -----

    #[test]
    fn tab_width_is_clamped_and_monotonic() {
        // Short title → min bound (90); very long → max bound (180).
        assert_eq!(estimate_tab_width("~"), 90.0);
        assert_eq!(
            estimate_tab_width("a-really-endless-folder-name-that-keeps-going"),
            180.0
        );
        // A longer title is never NARROWER (monotonicity).
        let w1 = estimate_tab_width("Documents");
        let w2 = estimate_tab_width("Documents-2024");
        assert!(w2 >= w1, "{w2} >= {w1}");
        // Intermediate zone: well within the bounds.
        assert!(w1 > 90.0 && w1 < 180.0, "{w1}");
    }

    #[test]
    fn own_icon_only_for_self_iconed_types() {
        // Executables & co: EMBEDDED icon → "by path" route.
        for ext in ["exe", "scr", "cpl", "ico"] {
            assert!(has_own_icon(ext), "{ext} should carry its own icon");
        }
        // Ordinary types: "by extension" route (cached, no I/O).
        for ext in ["txt", "png", "pdf", "docx", "lnk", ""] {
            assert!(!has_own_icon(ext), "{ext} should NOT trigger per-file I/O");
        }
    }

    #[test]
    fn preview_capability_and_compact_height_share_the_same_rules() {
        assert_eq!(
            thumbnail_kind_for_row(FileKind::Image.as_i32(), "svg"),
            Some((FileKind::Image, true))
        );
        for ext in ["af", "afdesign", "afphoto", "afpub"] {
            assert_eq!(
                thumbnail_kind_for_row(FileKind::Image.as_i32(), ext),
                Some((FileKind::Image, false)),
                "{ext}"
            );
        }
        assert_eq!(
            thumbnail_kind_for_row(FileKind::Document.as_i32(), "pdf"),
            Some((FileKind::Document, false))
        );
        assert_eq!(
            thumbnail_kind_for_row(FileKind::Audio.as_i32(), "mp3"),
            Some((FileKind::Audio, false))
        );
        assert_eq!(
            thumbnail_kind_for_row(FileKind::Audio.as_i32(), "FLAC"),
            Some((FileKind::Audio, false))
        );
        assert!(thumbnail_kind_for_row(FileKind::Document.as_i32(), "docx").is_none());

        assert_eq!(effective_row_height(true, THUMB_DEFAULT_ZOOM, true), 76.0);
        assert_eq!(effective_row_height(false, THUMB_DEFAULT_ZOOM, true), 28.0);
        assert_eq!(effective_row_height(false, THUMB_DEFAULT_ZOOM, false), 76.0);
        // The setting never alters the list mode's levels.
        assert_eq!(effective_row_height(false, LIST_DEFAULT_ZOOM, true), 28.0);

        // Zoom scale: the LIST is a single-level floor and the
        // FIRST notch upward enters thumbnail mode. (The purely
        // constant invariants are verified at COMPILE TIME near the `const`s, see `_`.)
        assert_eq!(zoom_to_height(LIST_DEFAULT_ZOOM), 28.0); // list (floor)
        assert_eq!(zoom_to_height(THUMB_ZOOM), 52.0); // 1st thumbnail
        assert_eq!(zoom_to_height(MAX_ZOOM), 220.0); // sharp upper bound
    }

    #[test]
    fn zoom_keeps_the_single_visible_selection_at_the_same_screen_position() {
        let panel = Panel::with_mode(PathBuf::from("gallery"), columns::default_columns(), 0);
        let mut rows: Vec<FileRow> = (0..80)
            .map(|index| FileRow {
                name: format!("image-{index:02}.png").into(),
                preview_capable: true,
                selected: index == 22,
                ..Default::default()
            })
            .collect();
        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, false);
        let viewport = ZoomViewport {
            top: rows[20].visual_y + 10.0,
            height: 420.0,
            // Deliberately points at another row: the single visible
            // selection must take priority.
            pointer_y: 15.0,
        };
        let anchor =
            capture_zoom_anchor(&rows, viewport.top, viewport.height, viewport.pointer_y).unwrap();
        assert_eq!(anchor.row_index, 22);
        panel.viewport_top.set(viewport.top);
        panel.viewport_height.set(viewport.height);
        panel.replace_rows(rows);

        let new_top = zoom_panel_visuals(&panel, true, MAX_ZOOM, false, false, viewport);
        let selected = panel.rows_model.row_data(anchor.row_index).unwrap();
        let selected_anchor_y =
            selected.visual_y + selected.visual_h * anchor.row_fraction - new_top;
        assert!((selected_anchor_y - anchor.viewport_y).abs() < 0.01);
        assert!(selected_anchor_y >= 0.0 && selected_anchor_y <= viewport.height);
        assert!((panel.viewport_top.get() - new_top).abs() < 0.01);
        assert_eq!(
            (panel.rendered_first.get(), panel.rendered_end.get()),
            row_range_for_content_span(
                &*panel.rows_model,
                (new_top - viewport.height).max(0.0),
                new_top + viewport.height * 2.0,
            )
        );
    }

    #[test]
    fn zoom_uses_the_pointer_for_multiple_or_offscreen_selections() {
        let mut rows: Vec<FileRow> = (0..100)
            .map(|index| FileRow {
                preview_capable: index % 2 == 0,
                selected: matches!(index, 2 | 31),
                ..Default::default()
            })
            .collect();
        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
        let top = rows[30].visual_y;
        let height = 360.0;
        let pointer_y = 95.0;
        let old_content_y = top + pointer_y;
        let expected_row = row_index_at_content_y_slice(&rows, old_content_y).unwrap();
        let anchor = capture_zoom_anchor(&rows, top, height, pointer_y).unwrap();
        assert_eq!(anchor.row_index, expected_row);

        layout_rows(&mut rows, MAX_ZOOM, true);
        let new_top = restore_zoom_viewport_top(&rows, Some(anchor), top, height);
        let row = &rows[anchor.row_index];
        let restored_pointer_y = row.visual_y + row.visual_h * anchor.row_fraction - new_top;
        assert!((restored_pointer_y - pointer_y).abs() < 0.01);

        // Same policy with a single OFF-screen selection: no jump to
        // it, the user keeps the zone they're actually looking at.
        for row in &mut rows {
            row.selected = false;
        }
        rows[2].selected = true;
        let anchor = capture_zoom_anchor(&rows, new_top, height, pointer_y).unwrap();
        assert_ne!(anchor.row_index, 2);
        assert_eq!(
            anchor.row_index,
            row_index_at_content_y_slice(&rows, new_top + pointer_y).unwrap()
        );
    }

    #[test]
    fn zoom_falls_back_to_the_view_center_and_clamps_short_content() {
        let mut rows: Vec<FileRow> = (0..10)
            .map(|_| FileRow {
                preview_capable: true,
                ..Default::default()
            })
            .collect();
        layout_rows(&mut rows, LIST_DEFAULT_ZOOM, false);
        let viewport_height = 500.0;
        // The pointer is in the blank space below the 280 px of content; the center
        // (250 px), however, stays on a valid row.
        let anchor = capture_zoom_anchor(&rows, 0.0, viewport_height, 450.0).unwrap();
        assert_eq!(anchor.viewport_y, viewport_height * 0.5);

        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, false);
        let top = restore_zoom_viewport_top(&rows, Some(anchor), 0.0, viewport_height);
        let row = &rows[anchor.row_index];
        let max_top =
            rows.last().unwrap().visual_y + rows.last().unwrap().visual_h - viewport_height;
        assert!((top - max_top).abs() < 0.01);
        let anchored_row_y = row.visual_y + row.visual_h * anchor.row_fraction - top;
        assert!(anchored_row_y >= 0.0 && anchored_row_y <= viewport_height);

        // If the content fits in the view again, no negative position or
        // one above the maximum can leak out to the Slint scrollbar.
        layout_rows(&mut rows, LIST_DEFAULT_ZOOM, false);
        assert_eq!(
            restore_zoom_viewport_top(&rows, Some(anchor), 9_999.0, viewport_height),
            0.0
        );
    }

    #[test]
    fn variable_row_geometry_drives_hit_test_and_rubber_band() {
        let mut rows = vec![
            FileRow {
                preview_capable: true,
                ..Default::default()
            },
            FileRow {
                preview_capable: false,
                ..Default::default()
            },
            FileRow {
                preview_capable: true,
                ..Default::default()
            },
        ];
        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
        let model = VecModel::from(rows);

        assert_eq!(row_index_at_content_y(&model, 75.0), 0);
        assert_eq!(row_index_at_content_y(&model, 76.0), 1);
        assert_eq!(row_index_at_content_y(&model, 103.0), 1);
        assert_eq!(row_index_at_content_y(&model, 104.0), 2);
        assert_eq!(row_index_at_content_y(&model, 180.0), -1);
        assert_eq!(row_band_for_content_range(&model, 70.0, 110.0), (0, 2));
    }

    #[test]
    fn variable_row_binary_search_matches_a_linear_oracle_far_from_the_top() {
        let mut rows: Vec<FileRow> = (0..4_000)
            .map(|index| FileRow {
                preview_capable: index % 3 != 1,
                ..Default::default()
            })
            .collect();
        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
        let total_height = rows.last().map(|row| row.visual_y + row.visual_h).unwrap();
        let model = VecModel::from(rows.clone());

        // Samples every zone of the gallery, including exactly
        // at the heterogeneous boundaries where rounding errors appear.
        let mut samples = vec![-1.0, 0.0, total_height, total_height + 1.0];
        for row in rows.iter().step_by(37) {
            samples.extend([
                row.visual_y,
                row.visual_y + row.visual_h * 0.5,
                row.visual_y + row.visual_h,
            ]);
        }
        for y in samples {
            let expected = rows
                .iter()
                .position(|row| y >= row.visual_y && y < row.visual_y + row.visual_h)
                .map(|index| index as i32)
                .unwrap_or(-1);
            assert_eq!(row_index_at_content_y(&model, y), expected, "y={y}");
        }
    }

    #[test]
    fn render_window_is_exact_bounded_and_tracks_a_far_scroll() {
        let mut rows: Vec<FileRow> = (0..10_000)
            .map(|index| FileRow {
                preview_capable: index % 2 == 0,
                ..Default::default()
            })
            .collect();
        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
        let top = rows[8_500].visual_y;
        let height = 720.0;
        let expected = row_range_for_slice(&rows, top - height, top + height * 2.0);
        let actual = mark_render_window(&mut rows, top, height);

        assert_eq!(actual, expected);
        assert!(actual.0 > 8_400, "the window must follow a distant scroll");
        assert!(
            actual.1 - actual.0 < 80,
            "the overscan must never materialize all 10,000 rows"
        );
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(
                row.rendered,
                index >= actual.0 && index < actual.1,
                "index={index}"
            );
        }

        let changed = render_window_changed_indices(0, 48, actual.0, actual.1);
        assert_eq!(
            changed.len(),
            48 + actual.1 - actual.0,
            "a distant jump must visit only the old and the new window"
        );
        assert_eq!(
            render_window_changed_indices(10, 20, 15, 25),
            vec![10, 11, 12, 13, 14, 20, 21, 22, 23, 24],
            "a normal scroll must not notify the shared area twice"
        );
    }

    #[test]
    fn filtered_render_model_keeps_logical_indexes_and_selection_in_sync() {
        let (rows_model, rendered_model) = new_row_models();
        let mut rows: Vec<FileRow> = (0..120)
            .map(|index| FileRow {
                name: format!("row-{index}").into(),
                preview_capable: index % 2 == 0,
                ..Default::default()
            })
            .collect();
        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
        let top = rows[80].visual_y;
        let (first, end) = mark_render_window(&mut rows, top, 300.0);
        rows_model.set_vec(rows);

        assert_eq!(rendered_model.row_count(), end - first);
        for visible in 0..rendered_model.row_count() {
            let row = rendered_model.row_data(visible).unwrap();
            assert_eq!(row.model_index, (first + visible) as i32);
        }

        let logical = first + (end - first) / 2;
        let mut row = rows_model.row_data(logical).unwrap();
        row.selected = true;
        rows_model.set_row_data(logical, row);
        assert!(
            rendered_model
                .row_data(logical - first)
                .is_some_and(|row| row.selected)
        );
    }

    #[test]
    fn changing_context_resets_the_exact_viewport_before_replacing_rows() {
        let panel = Panel::with_mode(PathBuf::from("old"), columns::default_columns(), 0);
        let mut rows: Vec<FileRow> = (0..2_000)
            .map(|index| FileRow {
                preview_capable: index % 2 == 0,
                ..Default::default()
            })
            .collect();
        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
        panel.viewport_top.set(rows[1_500].visual_y);
        panel.viewport_height.set(600.0);
        panel.replace_rows(rows.clone());
        assert!(panel.rendered_first.get() > 1_400);

        let previous_gen = panel.viewport_reset_gen.get();
        panel.reset_rows_viewport();
        panel.replace_rows(rows);
        assert_eq!(panel.viewport_top.get(), 0.0);
        assert_eq!(panel.rendered_first.get(), 0);
        assert_eq!(panel.viewport_reset_gen.get(), previous_gen.wrapping_add(1));
    }

    #[test]
    fn heterogeneous_rows_do_not_change_multi_selection_semantics() {
        let mut rows: Vec<FileRow> = (0..8)
            .map(|index| FileRow {
                preview_capable: index % 2 == 0,
                selected: matches!(index, 1 | 5 | 7),
                ..Default::default()
            })
            .collect();
        layout_rows(&mut rows, THUMB_DEFAULT_ZOOM, true);
        let model = VecModel::from(rows);
        let base = snapshot_selection(&model);

        assert_eq!(selection_apply_band(&model, 2, 4, 1, &base), 6);
        assert_eq!(
            snapshot_selection(&model),
            vec![false, true, true, true, true, true, false, true]
        );
        assert_eq!(selection_apply_band(&model, 2, 5, 2, &base), 2);
        assert_eq!(
            snapshot_selection(&model),
            vec![false, true, false, false, false, false, false, true]
        );
    }

    #[test]
    fn workspace_names_are_compared_trimmed_and_case_insensitive() {
        assert_eq!(normalized_workspace_name("  Projet Été  "), "projet été");
        assert_eq!(
            normalized_workspace_name("PROJET ÉTÉ"),
            normalized_workspace_name("projet été")
        );
    }

    #[test]
    fn type_ahead_filter_matches_any_name_substring() {
        assert!(name_contains_filter("Manuel outlook", "out"));
        assert!(name_contains_filter("Manuel outlook", "look"));
        assert!(name_contains_filter("OUTLOOK Notes", "look"));
        assert!(!name_contains_filter("Manuel outlook", "word"));
    }

    #[test]
    fn rename_status_only_offers_force_replace_for_two_files() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "ranger-rename-status-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("source.txt");
        std::fs::write(&source, b"source").unwrap();

        assert_eq!(
            rename_name_status(&source, "source.txt"),
            RenameNameStatus::Valid
        );
        assert_eq!(
            rename_name_status(&source, "free.txt"),
            RenameNameStatus::Valid
        );
        assert_eq!(
            entry_name_availability(&dir, "free.txt"),
            EntryNameAvailability::Available
        );
        assert_eq!(
            rename_name_status(&source, "../bad"),
            RenameNameStatus::Invalid
        );
        assert_eq!(
            entry_name_availability(&dir, "../bad"),
            EntryNameAvailability::Invalid
        );

        std::fs::write(dir.join("target.txt"), b"target").unwrap();
        assert_eq!(
            entry_name_availability(&dir, "target.txt"),
            EntryNameAvailability::ExistingNonDirectory
        );
        assert_eq!(
            rename_name_status(&source, "target.txt"),
            RenameNameStatus::ReplaceableFile
        );

        std::fs::create_dir(dir.join("target-dir")).unwrap();
        assert_eq!(
            entry_name_availability(&dir, "target-dir"),
            EntryNameAvailability::ExistingDirectory
        );
        assert_eq!(
            rename_name_status(&source, "target-dir"),
            RenameNameStatus::Conflict
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn image_depth_separates_color_bits_from_alpha() {
        assert_eq!(
            format_img_meta(1920, 1080, 24, true),
            ("1920x1080".to_string(), "24-bit + alpha".to_string())
        );
        assert_eq!(
            format_img_meta(800, 600, 24, false),
            ("800x600".to_string(), "24-bit".to_string())
        );
    }

    #[test]
    fn opener_filter_searches_label_program_arguments_and_extensions() {
        let opener = openers::Opener {
            id: "vscode".into(),
            label: "Visual Studio Code".into(),
            program: r"C:\Apps\Code.exe".into(),
            assoc: Some("Applications\\Code.exe".into()),
            icon: openers::OpenerIcon::None,
            args: vec!["--wait".into(), "{file}".into()],
            default_exts: vec!["rs".into()],
            used_exts: vec!["txt".into()],
            use_count: 0,
            last_used: 0,
            elevated: false,
            ctx_menu: 0,
            ctx_exts: Vec::new(),
        };
        for needle in ["visual", "code.exe", "--wait", "rs", "txt"] {
            assert!(opener_matches_filter(&opener, needle), "{needle}");
        }
        assert!(!opener_matches_filter(&opener, "gimp"));
    }

    #[test]
    fn async_listing_only_accepts_the_latest_matching_navigation() {
        let path = PathBuf::from("network-a");
        let mut panel = Panel::with_mode(path.clone(), columns::default_columns(), 0);
        panel.pending_listing = true;
        panel.listing_gen = 7;

        assert!(async_listing_is_current(&panel, 7, &path));
        assert!(!async_listing_is_current(&panel, 6, &path));
        assert!(!async_listing_is_current(&panel, 7, Path::new("network-b")));

        panel.pending_listing = false;
        assert!(!async_listing_is_current(&panel, 7, &path));
    }

    #[cfg(windows)]
    #[test]
    fn dropped_files_can_launch_windows_programs_and_batch_scripts() {
        for candidate in ["tool.exe", "tool.COM", "tool.cmd", "tool.BaT"] {
            assert!(is_drop_runnable(Path::new(candidate)), "{candidate}");
        }
        for candidate in ["notes.txt", "archive.zip", "folder"] {
            assert!(!is_drop_runnable(Path::new(candidate)), "{candidate}");
        }
    }

    #[test]
    fn file_drop_rejects_the_source_itself_and_folder_descendants() {
        let album = PathBuf::from("gallery").join("album");
        let sources = vec![album.clone()];

        assert!(paths_conflict_with_drop_target(&sources, &album, true));
        assert!(paths_conflict_with_drop_target(
            &sources,
            &album.join("subfolder"),
            true
        ));
        assert!(!paths_conflict_with_drop_target(
            &sources,
            &PathBuf::from("gallery").join("other"),
            true
        ));

        let file = PathBuf::from("gallery").join("notes.txt");
        assert!(paths_conflict_with_drop_target(
            std::slice::from_ref(&file),
            &file,
            false
        ));
    }

    #[test]
    fn tab_layout_accumulates_offsets_with_spacing() {
        let titles = vec!["~".to_string(), "src".to_string(), "target".to_string()];
        let geo = tab_layout(&titles);
        assert_eq!(geo.len(), 3);
        // First tab at offset 0.
        assert_eq!(geo[0].1, 0.0);
        // Each offset = previous offset + previous width + 2 spacing.
        assert_eq!(geo[1].1, geo[0].1 + geo[0].0 + 2.0);
        assert_eq!(geo[2].1, geo[1].1 + geo[1].0 + 2.0);
        // All widths within the design bounds.
        assert!(geo.iter().all(|(w, _)| (90.0..=180.0).contains(w)));
    }

    // Workspace "modified" detection -----

    fn tab(path: &str) -> TabState {
        TabState {
            path: path.into(),
            sort_column: SortColumn::Name,
            sort_order: SortOrder::Asc,
            preview: false,
            show_hidden: false,
            group_mode: GroupMode::FoldersFirst,
        }
    }

    fn panel(tabs: Vec<TabState>) -> PanelState {
        PanelState {
            stretch: 1.0,
            active_tab: 0,
            tabs,
            columns: columns::default_columns(),
            tab_bar_mode: 0,
            vbar_width: 0.0,
        }
    }

    fn ws(panels: Vec<PanelState>) -> WorkspaceState {
        WorkspaceState {
            active_panel: 0,
            panels,
            layout: None,
            workspace_name: None,
            closed_tabs: Vec::new(),
            sidebar_sections: SidebarSectionsState::default(),
        }
    }

    /// Adding an open tab constitutes a workspace modification.
    #[test]
    fn differ_detects_added_tab() {
        let a = ws(vec![panel(vec![tab(r"C:\a")])]);
        let b = ws(vec![panel(vec![tab(r"C:\a"), tab(r"C:\b")])]);
        assert!(workspaces_differ(&a, &b));
    }

    /// A per-tab display setting (hidden files, previews, sort) counts.
    #[test]
    fn differ_detects_per_tab_settings() {
        let a = ws(vec![panel(vec![tab(r"C:\a")])]);
        let mut b = ws(vec![panel(vec![tab(r"C:\a")])]);
        b.panels[0].tabs[0].show_hidden = true;
        assert!(workspaces_differ(&a, &b));

        let mut c = ws(vec![panel(vec![tab(r"C:\a")])]);
        c.panels[0].tabs[0].preview = true;
        assert!(workspaces_differ(&a, &c));
    }

    /// Tab bar position (per-view): counted.
    #[test]
    fn differ_detects_tab_bar_mode() {
        let a = ws(vec![panel(vec![tab(r"C:\a")])]);
        let mut b = ws(vec![panel(vec![tab(r"C:\a")])]);
        b.panels[0].tab_bar_mode = 1;
        assert!(workspaces_differ(&a, &b));
    }

    /// A left-bar section's collapse state goes through the same signature as
    /// the tabs: the asterisk and the Update button can never diverge.
    #[test]
    fn differ_detects_sidebar_section_state() {
        let a = ws(vec![panel(vec![tab(r"C:\a")])]);
        let mut b = a.clone();
        b.sidebar_sections.favorites_collapsed = true;
        assert!(workspaces_differ(&a, &b));

        b.sidebar_sections.favorites_collapsed = false;
        b.sidebar_sections.network_collapsed = true;
        assert!(workspaces_differ(&a, &b));
    }

    #[test]
    fn sidebar_section_state_survives_app_state_capture_and_reset() {
        let mut saved = ws(vec![panel(vec![tab(r"C:\a")])]);
        saved.sidebar_sections = SidebarSectionsState {
            shortcuts_collapsed: true,
            favorites_collapsed: true,
            drives_collapsed: false,
            network_collapsed: true,
        };
        let config = Config {
            sidebar_section_order: [
                SidebarSection::Favorites,
                SidebarSection::Network,
                SidebarSection::Drives,
                SidebarSection::Shortcuts,
            ],
            ..Config::default()
        };
        let state = AppState::from_workspace(config.clone(), saved.clone());
        assert_eq!(
            state.capture_workspace().sidebar_sections,
            saved.sidebar_sections
        );

        state.reset_to_blank();
        assert_eq!(
            state.capture_workspace().sidebar_sections,
            SidebarSectionsState::default()
        );
        // Reset and loading a workspace never touch the
        // global section order preference.
        assert_eq!(
            state.snapshot_config().sidebar_section_order,
            config.sidebar_section_order
        );
    }

    /// Volatile fields (split ratios, column widths, active tab or panel,
    /// and `workspace_name`) don't affect the workspace's persistent state.
    #[test]
    fn differ_ignores_volatile_fields() {
        let base = ws(vec![panel(vec![tab(r"C:\a")]), panel(vec![tab(r"C:\b")])]);
        let mut other = base.clone();
        // Split ratios.
        other.panels[0].stretch = 2.5;
        // Active tab / panel (simple focus).
        other.active_panel = 1;
        other.panels[1].active_tab = 0;
        // Vertical bar width + attached name.
        other.panels[0].vbar_width = 220.0;
        other.workspace_name = Some("toto".into());
        // Columns (resized widths).
        if let Some(c) = other.panels[0].columns.first_mut() {
            c.width += 40.0;
        }
        assert!(!workspaces_differ(&base, &other));
    }

    /// Idempotence: a workspace never differs from itself.
    #[test]
    fn differ_reflexive() {
        let a = ws(vec![panel(vec![tab(r"C:\a"), tab(r"C:\b")])]);
        assert!(!workspaces_differ(&a, &a.clone()));
    }
}
