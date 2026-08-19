//! Ranger's core: user config, i18n, XDG paths, logging, errors.
//!
//! This crate is deliberately **free of any UI or network dependency**; all
//! the logic shared between the Slint GUI and any future utilities
//! (tests, benchmarks) lives here.

pub mod columns;
pub mod config;
pub mod eject;
pub mod error;
pub mod favorites;
pub mod fs;
pub mod i18n;
pub mod layout;
pub mod logging;
pub mod openers;
pub mod paths;
pub mod places;
pub mod process_lock;
pub mod shortcuts;
pub mod thumbnail;
pub mod workspace;

pub use columns::ColumnSpec;
pub use config::{Config, SidebarSection};
pub use error::{Error, Result};
pub use favorites::{FavNode, FavStore, FlatFav};
pub use fs::{Entry, FileKind, SortColumn, SortOrder};
pub use i18n::{Lang, Theme};
pub use layout::{Layout, LayoutNode, PanelGeom, Rect, Side, SplitDir, SplitterGeom};
pub use openers::{Opener, OpenerIcon, OpenerStore, TagContext};
pub use places::{Place, PlaceKind};
pub use thumbnail::Thumbnail;
pub use workspace::{
    NamedWorkspaceMeta, PanelState, SidebarSectionsState, TabState, WorkspaceState,
};
