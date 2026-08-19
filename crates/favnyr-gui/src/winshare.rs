//! Native Windows **Share sheet** for the context-menu "Share" entry.
//!
//! The Windows shell "Share" verb, invoked via `IContextMenu` from an
//! UNPACKAGED host, fails with `ERROR_INVALID_WINDOW_HANDLE`: the modern share
//! flyout (`ShowShareUIForWindow`) requires the host app to have registered a
//! `DataTransferManager` for its window first — Explorer does, Ranger did not.
//! So Ranger drives the sheet itself: register the manager, provide the selected
//! files as `StorageItems`, then show the UI anchored on the main window.
//!
//! Threading: everything runs on the UI thread (the window's apartment). WinRT
//! interfaces are `!Send`, so the storage items can't be built off-thread and
//! carried in; they're resolved inside the `DataRequested` handler instead. The
//! blocking `IAsyncOperation::join()` there is safe because the storage lookup
//! operations complete on a threadpool thread (no message pump needed).

use std::cell::Cell;
use std::ffi::c_void;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use windows::ApplicationModel::DataTransfer::{DataRequestedEventArgs, DataTransferManager};
use windows::Foundation::TypedEventHandler;
use windows::Storage::{IStorageItem, StorageFile, StorageFolder};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::IDataTransferManagerInterop;
use windows::core::{HSTRING, Interface};
use windows_collections::IIterable;

thread_local! {
    /// The previous share's `DataRequested` registration on the window's manager,
    /// removed before registering the next one.
    static LAST_HANDLER: Cell<Option<i64>> = const { Cell::new(None) };
}

/// Resolves `paths` to WinRT `IStorageItem`s. Called on the UI thread inside the
/// `DataRequested` handler (WinRT interfaces are `!Send`, so they can't be built
/// off-thread and carried in). Blocking `.join()` is safe: `StorageFile` and
/// `StorageFolder` lookups complete on a threadpool thread, so no message pump
/// is needed. Missing / inaccessible entries are skipped.
fn build_items(paths: &[PathBuf]) -> Vec<Option<IStorageItem>> {
    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        let hs = HSTRING::from(path.as_os_str());
        let item: Option<IStorageItem> = if path.is_dir() {
            StorageFolder::GetFolderFromPathAsync(&hs)
                .and_then(|op| op.join())
                .ok()
                .and_then(|f| f.cast().ok())
        } else {
            StorageFile::GetFileFromPathAsync(&hs)
                .and_then(|op| op.join())
                .ok()
                .and_then(|f| f.cast().ok())
        };
        if let Some(item) = item {
            items.push(Some(item));
        }
    }
    items
}

/// Shows the Windows Share sheet for `paths`, anchored on `hwnd` (the main
/// window). Call on the UI thread. `Err` if no item resolved or a WinRT call
/// failed.
pub fn share_files(hwnd: isize, paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Err(anyhow!("share: no path"));
    }
    // The handler is `Send`, WinRT items are not → capture the (Send) paths and
    // resolve them to storage items inside the handler, on the UI thread.
    let owned: Vec<PathBuf> = paths.to_vec();
    let title = HSTRING::from(
        paths
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Ranger".to_string()),
    );

    unsafe {
        let interop: IDataTransferManagerInterop =
            windows::core::factory::<DataTransferManager, IDataTransferManagerInterop>()?;
        let hwnd = HWND(hwnd as *mut c_void);
        let dtm: DataTransferManager = interop.GetForWindow(hwnd)?;

        // `GetForWindow` returns the SAME manager for the window each time, so
        // drop the previous share's handler before adding this one — otherwise
        // stale handlers pile up and could answer with old paths.
        LAST_HANDLER.with(|slot| {
            if let Some(token) = slot.take() {
                let _ = dtm.RemoveDataRequested(token);
            }
        });

        // The sheet pulls the payload through this handler when it opens.
        let handler = TypedEventHandler::<DataTransferManager, DataRequestedEventArgs>::new(
            move |_sender, args| {
                if let Some(args) = args.as_ref() {
                    let items = build_items(&owned);
                    if !items.is_empty() {
                        let data = args.Request()?.Data()?;
                        data.Properties()?.SetTitle(&title)?;
                        let iterable = IIterable::<IStorageItem>::from(items);
                        data.SetStorageItems(&iterable, true)?;
                    }
                }
                Ok(())
            },
        );
        let token = dtm.DataRequested(&handler)?;
        LAST_HANDLER.with(|slot| slot.set(Some(token)));
        interop.ShowShareUIForWindow(hwnd)?;
    }
    Ok(())
}
