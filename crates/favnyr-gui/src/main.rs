//! Main `ranger` binary.
//!
//! Startup sequence:
//!   1. `tracing` logging.
//!   2. Creating the XDG directories.
//!   3. Loading (or creating) `config.toml`.
//!   4. Instantiating the Slint window + installing the bridge.
//!   5. Slint event loop.
//!   6. Persisting the window size on close.

// On Windows, in a **release** build, no black console window on launch (GUI
// app). In debug we keep the console to see logs/panics during dev.
// (No-op on other OSes.)
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result};
use slint::ComponentHandle;
use tracing::{info, warn};

use ranger_core::{Config, WorkspaceState, logging, paths, workspace};

mod actions;
mod bridge;
mod clipboard;
mod i18n;
mod openwith;
mod shellmenu;
#[cfg(windows)]
mod winddrag;
mod winmsg;
#[cfg(windows)]
mod winportable;
#[cfg(windows)]
mod winshare;
#[cfg(windows)]
mod winthumb;
#[cfg(windows)]
mod winutil;

slint::include_modules!();

/// Actual constraints declared by `MainWindow` in the Slint file.
/// Keeping them here lets us clamp a restored size even before the
/// backend creates the native window.
const MIN_WINDOW_WIDTH: u32 = 720;
const MIN_WINDOW_HEIGHT: u32 = 420;

#[derive(Debug, PartialEq, Eq)]
enum StartupRequest {
    Normal,
    /// Internal protocol used only for tab tear-off.
    DetachedTab {
        dir: PathBuf,
        position: Option<(i32, i32)>,
        tab_bar_mode: u8,
    },
    /// Public argument: visible name of a saved workspace.
    Workspace(String),
}

fn parse_startup_request(args: impl IntoIterator<Item = OsString>) -> StartupRequest {
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return StartupRequest::Normal;
    };

    if first == "--detached-tab" {
        let Some(dir) = args
            .next()
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
        else {
            return StartupRequest::Normal;
        };
        let x = args
            .next()
            .and_then(|value| value.to_str().and_then(|value| value.parse().ok()));
        let y = args
            .next()
            .and_then(|value| value.to_str().and_then(|value| value.parse().ok()));
        let position = match (x, y) {
            (Some(x), Some(y)) => Some((x, y)),
            _ => None,
        };
        let tab_bar_mode = if position.is_some() {
            args.next()
                .and_then(|value| value.to_str().and_then(|value| value.parse().ok()))
                .filter(|mode| *mode <= 2)
                .unwrap_or(0)
        } else {
            0
        };
        return StartupRequest::DetachedTab {
            dir,
            position,
            tab_bar_mode,
        };
    }

    // Also accepts an unquoted name made of several words. Workspace names
    // come from Unicode UI fields; a rare non-UTF-8 argument remains
    // displayable anyway thanks to the tolerant conversion.
    let mut words = vec![first.to_string_lossy().into_owned()];
    words.extend(args.map(|arg| arg.to_string_lossy().into_owned()));
    let name = words.join(" ").trim().to_string();
    if name.is_empty() {
        StartupRequest::Normal
    } else {
        StartupRequest::Workspace(name)
    }
}

fn restore_session(config: Config) -> bridge::AppState {
    match WorkspaceState::load(&paths::workspace_path()) {
        Ok(Some(ws)) => {
            info!(panels = ws.panels.len(), "workspace restored");
            bridge::AppState::from_workspace(config, ws)
        }
        Ok(None) => bridge::AppState::new(config),
        Err(err) => {
            warn!(error = %err, "workspace invalid, starting fresh");
            bridge::AppState::new(config)
        }
    }
}

/// Applies the UI's minimum bounds and, when known, the maximum
/// dimensions of the client area that fits on the target monitor.
///
/// The maximum is expressed in LOGICAL pixels, like `Config::window_*`.
fn clamp_restored_size(requested: (u32, u32), maximum: Option<(u32, u32)>) -> (u32, u32) {
    let requested = (
        requested.0.max(MIN_WINDOW_WIDTH),
        requested.1.max(MIN_WINDOW_HEIGHT),
    );
    let Some((max_w, max_h)) = maximum else {
        return requested;
    };

    // On a screen exceptionally smaller than Slint's constraints,
    // the minimum size still takes priority (the UI can't shrink
    // any further anyway).
    (
        requested.0.min(max_w.max(MIN_WINDOW_WIDTH)),
        requested.1.min(max_h.max(MIN_WINDOW_HEIGHT)),
    )
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy)]
struct WindowsWorkArea {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
    scale: f32,
    decoration_width: i32,
    decoration_height: i32,
}

#[cfg(windows)]
impl WindowsWorkArea {
    fn width(self) -> i32 {
        self.right - self.left
    }

    fn height(self) -> i32 {
        self.bottom - self.top
    }

    /// Maximum logical client size that also lets the native decorations
    /// (title bar + borders) fit within the work area.
    fn max_logical_client_size(self) -> (u32, u32) {
        let scale = self.scale.max(0.5);
        let client_w = (self.width() - self.decoration_width).max(1);
        let client_h = (self.height() - self.decoration_height).max(1);
        (
            (client_w as f32 / scale).floor().max(1.0) as u32,
            (client_h as f32 / scale).floor().max(1.0) as u32,
        )
    }

    fn logical_to_physical(self, logical: (u32, u32)) -> (u32, u32) {
        let scale = self.scale.max(0.5);
        (
            (logical.0 as f32 * scale).floor().max(1.0) as u32,
            (logical.1 as f32 * scale).floor().max(1.0) as u32,
        )
    }
}

/// Returns the work area and DPI metrics of the primary monitor, or
/// of the monitor containing the drop point of a detached tab.
#[cfg(windows)]
fn windows_work_area(
    detached_pos: Option<(i32, i32)>,
    fallback_scale: f32,
) -> Option<WindowsWorkArea> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
        MonitorFromPoint,
    };
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetSystemMetricsForDpi, MDT_EFFECTIVE_DPI};
    use windows::Win32::UI::WindowsAndMessaging::{
        SM_CXFRAME, SM_CXPADDEDBORDER, SM_CYCAPTION, SM_CYFRAME,
    };

    let (point, flags) = match detached_pos {
        Some((x, y)) => (POINT { x, y }, MONITOR_DEFAULTTONEAREST),
        None => (POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
    };
    let monitor = unsafe { MonitorFromPoint(point, flags) };
    if monitor.0.is_null() {
        return None;
    }

    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut monitor_info) }.as_bool() {
        return None;
    }

    let mut dpi_x = 0_u32;
    let mut dpi_y = 0_u32;
    let dpi = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
        .is_ok()
        && dpi_x > 0
    {
        dpi_x
    } else {
        (fallback_scale.max(0.5) * 96.0).round() as u32
    };
    let scale = dpi as f32 / 96.0;

    // Metrics for the MONITOR's DPI, not just the primary screen's.
    // This matters when detaching between two screens with
    // different scale factors.
    let (caption, frame_x, frame_y, padded_border) = unsafe {
        (
            GetSystemMetricsForDpi(SM_CYCAPTION, dpi),
            GetSystemMetricsForDpi(SM_CXFRAME, dpi),
            GetSystemMetricsForDpi(SM_CYFRAME, dpi),
            GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi),
        )
    };
    let decoration_width = 2 * (frame_x + padded_border).max(0);
    let decoration_height = caption.max(0) + 2 * (frame_y + padded_border).max(0);
    let work = monitor_info.rcWork;
    if work.right <= work.left || work.bottom <= work.top {
        return None;
    }

    Some(WindowsWorkArea {
        left: work.left,
        top: work.top,
        right: work.right,
        bottom: work.bottom,
        scale,
        decoration_width,
        decoration_height,
    })
}

#[cfg(windows)]
fn fit_window_axis(desired: i32, start: i32, end: i32, outer_size: i32) -> i32 {
    let latest_start = end.saturating_sub(outer_size);
    if latest_start < start {
        start
    } else {
        desired.clamp(start, latest_start)
    }
}

/// On Windows, catches Ctrl+C and Ctrl+Break from a development terminal.
/// The first signal closes the Slint loop so the state can be persisted before
/// the controlled termination via `finish`. The second immediately terminates the process.
/// Without an attached console, this handler stays inactive.
#[cfg(windows)]
fn install_console_ctrl_handler() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static QUITTING: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn handler(_ctrl_type: u32) -> i32 {
        if QUITTING.swap(true, Ordering::SeqCst) {
            // A second signal, or a loop that doesn't respond, forces an
            // immediate termination.
            terminate_process_now(130);
        }
        // The first signal requests a clean exit from the Slint loop; the
        // main thread then persists the state before calling `finish`.
        let _ = slint::invoke_from_event_loop(|| {
            let _ = slint::quit_event_loop();
        });
        1 // TRUE: event handled (we don't run the default behavior).
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), 1);
    }
}

fn main() -> Result<()> {
    let t_start = std::time::Instant::now();
    logging::init();
    paths::ensure_dirs().context("creating XDG directories")?;

    let mut config = Config::load_or_default(&paths::config_path()).unwrap_or_else(|err| {
        warn!(error = %err, "config invalid, using defaults");
        Config::default()
    });

    // The internal tear-off protocol now has an explicit marker,
    // which frees up the first public positional argument for a workspace name.
    // The parser performs no disk access.
    let startup = parse_startup_request(std::env::args_os().skip(1));
    let (detached_dir, detached_pos, detached_tab_bar, requested_workspace) = match startup {
        StartupRequest::Normal => (None, None, 0, None),
        StartupRequest::Workspace(name) => (None, None, 0, Some(name)),
        StartupRequest::DetachedTab {
            dir,
            position,
            tab_bar_mode,
        } => (Some(dir), position, tab_bar_mode, None),
    };

    let window = MainWindow::new().context("Slint MainWindow::new failed")?;

    // Restores the persisted size without ever letting the outer window
    // exceed the target monitor's work area. Config values are
    // LOGICAL; the work area and Windows decorations are PHYSICAL.
    {
        let requested = (config.window_width, config.window_height);
        let minimum_restored = clamp_restored_size(requested, None);

        #[cfg(windows)]
        let restored = {
            if let Some(work) = windows_work_area(detached_pos, window.window().scale_factor()) {
                let restored = clamp_restored_size(requested, Some(work.max_logical_client_size()));
                let (physical_w, physical_h) = work.logical_to_physical(restored);
                // Before the native window is created, Slint doesn't
                // necessarily know the final DPI yet. Giving it a LOGICAL size
                // avoids a double conversion when opening on a
                // secondary screen; physical pixels stay reserved for the placement
                // calculation below.
                window.window().set_size(slint::LogicalSize::new(
                    restored.0 as f32,
                    restored.1 as f32,
                ));

                let outer_w = physical_w as i32 + work.decoration_width;
                let outer_h = physical_h as i32 + work.decoration_height;
                let (desired_x, desired_y) = detached_pos
                    .map(|(px, py)| (px - 50, py - 14))
                    .unwrap_or_else(|| {
                        (
                            work.left + (work.width() - outer_w) / 2,
                            work.top + (work.height() - outer_h) / 2,
                        )
                    });
                let x = fit_window_axis(desired_x, work.left, work.right, outer_w);
                let y = fit_window_axis(desired_y, work.top, work.bottom, outer_h);
                window
                    .window()
                    .set_position(slint::PhysicalPosition::new(x, y));
                restored
            } else {
                // Very unlikely fallback (no Win32 monitor available): the
                // cross-platform behavior stays usable.
                window.window().set_size(slint::LogicalSize::new(
                    minimum_restored.0 as f32,
                    minimum_restored.1 as f32,
                ));
                if let Some((px, py)) = detached_pos {
                    window
                        .window()
                        .set_position(slint::PhysicalPosition::new(px - 50, py - 14));
                }
                minimum_restored
            }
        };

        #[cfg(not(windows))]
        let restored = {
            window.window().set_size(slint::LogicalSize::new(
                minimum_restored.0 as f32,
                minimum_restored.1 as f32,
            ));
            // Tab tear-off: physical screen position provided by the backend.
            if let Some((px, py)) = detached_pos {
                window
                    .window()
                    .set_position(slint::PhysicalPosition::new(px - 50, py - 14));
            }
            minimum_restored
        };

        // A normal instance sanitizes any size incompatible with the
        // current monitor. A detached instance stays ephemeral and never modifies this
        // global preference.
        if detached_dir.is_none()
            && (config.window_width != restored.0 || config.window_height != restored.1)
        {
            info!(
                requested_width = requested.0,
                requested_height = requested.1,
                restored_width = restored.0,
                restored_height = restored.1,
                "clamping persisted window size to monitor work area"
            );
            config.window_width = restored.0;
            config.window_height = restored.1;
            if let Err(err) = config.save(&paths::config_path()) {
                warn!(error = %err, "saving clamped window size failed");
            }
        }
    }

    // Detached instance (tear-off): blank state on the requested folder.
    // Otherwise: restores the workspace (panels + tabs) if present, or falls
    // back to the default state (one panel, one tab on $HOME).
    let mut workspace_warning = None;
    let state = if let Some(dir) = &detached_dir {
        info!(dir = %dir.display(), "starting detached instance (tab tear-off)");
        let s = bridge::AppState::new_at(config, dir.clone(), detached_tab_bar);
        // A detached instance is ephemeral: its view changes must
        // never replace the main instance's shared workspace.
        s.set_ephemeral();
        s
    } else if let Some(name) = requested_workspace {
        let dir = paths::workspaces_dir();
        if let Some(meta) = workspace::find_named_workspace(&dir, &name) {
            match workspace::load_named_workspace(&dir, &meta.id) {
                Ok((canonical_name, mut ws)) => {
                    info!(
                        name = canonical_name,
                        "starting named workspace from command line"
                    );
                    // `from_workspace` uses this field for the title and reloads
                    // the saved snapshot used for the "modified" state.
                    ws.workspace_name = Some(canonical_name);
                    bridge::AppState::from_workspace(config, ws)
                }
                Err(err) => {
                    warn!(error = %err, name, "command-line workspace could not be loaded");
                    workspace_warning = Some(name);
                    restore_session(config)
                }
            }
        } else {
            warn!(name, "command-line workspace not found");
            workspace_warning = Some(name);
            restore_session(config)
        }
    } else {
        restore_session(config)
    };
    bridge::install(&window, state.clone());
    if let Some(name) = workspace_warning {
        bridge::show_workspace_not_found_notice(&window, &state, &name);
    }

    // Ctrl+C / Ctrl+Break from the terminal → clean exit (dev). Installed
    // right before the loop (the event loop exists → `invoke_from_event_loop` OK).
    #[cfg(windows)]
    install_console_ctrl_handler();

    info!(
        init_ms = t_start.elapsed().as_millis(),
        "starting Slint event loop"
    );
    window.run().context("Slint event loop failed")?;

    // Persistence on close: window size (config) + panel/tab layout
    // (workspace). SKIPPED for a detached instance (tear-off): it's
    // ephemeral and would otherwise overwrite the main instance's config/workspace
    // (both files are shared).
    // ALSO skipped if the instance was emptied by transferring all its tabs to
    // another instance: otherwise we'd overwrite the shared workspace (empty
    // state), and the tab — already on the other instance — would be "recreated" on
    // the next launch.
    if detached_dir.is_none() && !bridge::suppress_workspace_persist() {
        bridge::persist_window_size(&window, &state);
        state.persist_workspace();
    }

    // On Windows, the GUI stack's CRT/DllMain teardown (winit/FemtoVG and
    // AccessKit/UI-Automation COM) can hang. Since the state is already persisted,
    // the forced termination avoids leaving a residual process. Normal return
    // on other platforms.
    finish()
}

/// On Windows, immediately terminates the process without going through the
/// GUI stack's CRT/DllMain teardown. On other systems, returns normally.
#[cfg(windows)]
fn finish() -> Result<()> {
    terminate_process_now(0)
}
#[cfg(not(windows))]
fn finish() -> Result<()> {
    Ok(())
}

/// Forces the current process to end via `TerminateProcess`, without running the
/// CRT shutdown or the `DllMain`s. Only call this after the state has been persisted.
#[cfg(windows)]
fn terminate_process_now(code: u32) -> ! {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> isize;
        fn TerminateProcess(handle: isize, exit_code: u32) -> i32;
    }
    unsafe {
        TerminateProcess(GetCurrentProcess(), code);
    }
    // TerminateProcess is asynchronous: we wait for the process to actually die.
    loop {
        std::thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_size_keeps_a_normal_window_unchanged() {
        assert_eq!(
            clamp_restored_size((1100, 700), Some((1904, 993))),
            (1100, 700)
        );
    }

    #[test]
    fn restored_size_clamps_dimensions_saved_while_maximized() {
        // A size coming from a maximized window is bounded to the
        // normal available client area.
        assert_eq!(
            clamp_restored_size((1920, 1009), Some((1904, 993))),
            (1904, 993)
        );
    }

    #[test]
    fn restored_size_enforces_the_slint_minimum() {
        assert_eq!(
            clamp_restored_size((1, 2), Some((1904, 993))),
            (MIN_WINDOW_WIDTH, MIN_WINDOW_HEIGHT)
        );
    }

    #[test]
    fn positional_arguments_form_a_workspace_name() {
        assert_eq!(
            parse_startup_request([OsString::from("Mon"), OsString::from("Workspace")]),
            StartupRequest::Workspace("Mon Workspace".into())
        );
    }

    #[test]
    fn detached_tab_protocol_cannot_be_confused_with_a_workspace() {
        assert_eq!(
            parse_startup_request([
                OsString::from("--detached-tab"),
                OsString::from("C:\\Photos"),
                OsString::from("120"),
                OsString::from("240"),
                OsString::from("2"),
            ]),
            StartupRequest::DetachedTab {
                dir: PathBuf::from("C:\\Photos"),
                position: Some((120, 240)),
                tab_bar_mode: 2,
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn detached_window_position_stays_inside_negative_monitor_coordinates() {
        assert_eq!(fit_window_axis(-2500, -1920, 0, 1100), -1920);
        assert_eq!(fit_window_axis(-50, -1920, 0, 1100), -1100);
    }
}
