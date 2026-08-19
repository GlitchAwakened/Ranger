//! Cross-instance IPC for Ranger (Windows) — **tab transfer via
//! drag-and-drop between two Ranger windows**. No dependency added:
//! raw FFI to `user32` / `comctl32` (system DLLs), like `places.rs`.
//!
//! Principle: each instance MARKS its window (`SetPropW`) and SUBCLASSES it
//! (`SetWindowSubclass`) to intercept `WM_COPYDATA`. On drop, the SOURCE
//! instance finds the Ranger window under the cursor (`WindowFromPoint`) and
//! sends it the serialized tab (`SendMessageW(WM_COPYDATA)`). The target
//! extracts the payload in its subclassed procedure, then processes it on the
//! Slint UI thread (`invoke_from_event_loop`).
//!
//! `WM_COPYDATA` is SYNCHRONOUS: the subclassed procedure just COPIES the
//! payload and schedules its processing (immediate return → the source
//! doesn't block).

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicIsize, Ordering};

    type Hwnd = isize;

    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[repr(C)]
    struct CopyDataStruct {
        dw_data: usize,
        cb_data: u32,
        lp_data: *const core::ffi::c_void,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn WindowFromPoint(pt: Point) -> Hwnd;
        fn GetAncestor(hwnd: Hwnd, flags: u32) -> Hwnd;
        fn SetPropW(hwnd: Hwnd, s: *const u16, data: isize) -> i32;
        fn GetPropW(hwnd: Hwnd, s: *const u16) -> isize;
        // Sending WITH TIMEOUT: if the target is frozen, we don't wait
        // indefinitely (SendMessageW would block), and we can verify it
        // actually PROCESSED the message (LRESULT) before removing the tab on
        // the source side.
        fn SendMessageTimeoutW(
            hwnd: Hwnd,
            msg: u32,
            wparam: usize,
            lparam: isize,
            flags: u32,
            timeout_ms: u32,
            result: *mut usize,
        ) -> isize;
        fn SetForegroundWindow(hwnd: Hwnd) -> i32;
        fn SetFocus(hwnd: Hwnd) -> Hwnd;
        fn ClientToScreen(hwnd: Hwnd, point: *mut Point) -> i32;
        fn ScreenToClient(hwnd: Hwnd, point: *mut Point) -> i32;
        fn EnumThreadWindows(
            thread_id: u32,
            cb: unsafe extern "system" fn(Hwnd, isize) -> i32,
            lparam: isize,
        ) -> i32;
        fn IsWindowVisible(hwnd: Hwnd) -> i32;
        fn GetWindow(hwnd: Hwnd, cmd: u32) -> Hwnd;
        // Cross-instance hover: ASYNCHRONOUS (fire-and-forget) → adds no
        // latency to the source's drag (unlike SendMessage).
        fn PostMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> i32;
        fn RegisterWindowMessageW(s: *const u16) -> u32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    // Window subclassing (Common Controls v6, system DLL).
    #[link(name = "comctl32")]
    unsafe extern "system" {
        fn SetWindowSubclass(
            hwnd: Hwnd,
            proc: unsafe extern "system" fn(Hwnd, u32, usize, isize, usize, usize) -> isize,
            id: usize,
            ref_data: usize,
        ) -> i32;
        fn DefSubclassProc(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> isize;
    }

    const WM_COPYDATA: u32 = 0x004A;
    const GA_ROOT: u32 = 2;
    const GW_OWNER: u32 = 4;
    const SUBCLASS_ID: usize = 0x52_4E_47_52; // "RNGR"
    /// Protocol tag/version (`dwData` field of the COPYDATASTRUCT): identifies
    /// a "Ranger tab" payload — any other WM_COPYDATA (from another app) is
    /// ignored.
    const PROTO_TAB_V1: usize = 0x524E_4701; // "RNG" + version 1
    const SMTO_BLOCK: u32 = 0x0001;
    const SMTO_ABORTIFHUNG: u32 = 0x0002;
    const SEND_TIMEOUT_MS: u32 = 2000;

    /// Property name marking a Ranger window (detectable by other instances).
    /// NUL-terminated (UTF-16).
    fn prop_name() -> Vec<u16> {
        "RangerTabTargetWindow\0".encode_utf16().collect()
    }

    // HWND of THIS instance's main window (0 = not initialized). Used to
    // exclude itself from detection and to identify itself as the sender.
    static SELF_HWND: AtomicIsize = AtomicIsize::new(0);
    // REGISTERED window message IDs (RegisterWindowMessageW → same values
    // across all Ranger instances for a given string). Resolved at init.
    static WM_HOVER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    static WM_HOVER_END: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    /// Incoming message from another Ranger instance.
    pub enum Incoming {
        /// Tab DROPPED (serialized payload) → insert it.
        Transfer(String),
        /// HOVER in progress at SCREEN point `(x, y)` (physical) → insertion preview.
        Hover(i32, i32),
        /// End of hover (cursor left / drag ended elsewhere) → clear the preview.
        HoverEnd,
    }

    /// Handler for incoming messages (on the UI thread).
    type Handler = Box<dyn Fn(Incoming)>;

    thread_local! {
        /// Handler being called (UI thread). Captured once at init; never
        /// crosses a thread (thread_local of the UI thread).
        static HANDLER: RefCell<Option<Handler>> = const { RefCell::new(None) };
    }

    fn hover_msg_name() -> Vec<u16> {
        "RangerTabHover\0".encode_utf16().collect()
    }
    fn hover_end_msg_name() -> Vec<u16> {
        "RangerTabHoverEnd\0".encode_utf16().collect()
    }

    /// Posts (UI thread) an `Incoming` variant to the handler, deferred.
    fn dispatch(incoming: Incoming) {
        // We're already on the UI thread (WndProc), but we DEFER to avoid
        // re-entering Slint from the message loop (consistent with the
        // transfer, and safe for a synchronous WM_COPYDATA).
        // `invoke_from_event_loop` requires `Send` → `Incoming` only captures
        // `Send` types.
        let _ = slint::invoke_from_event_loop(move || {
            HANDLER.with(|h| {
                if let Some(f) = h.borrow().as_ref() {
                    f(incoming);
                }
            });
        });
    }

    unsafe extern "system" fn find_main_cb(hwnd: Hwnd, lparam: isize) -> i32 {
        unsafe {
            // The first VISIBLE TOP-LEVEL window (no owner) of the thread = the
            // main window. `lparam` points to a `Hwnd` to fill in.
            if IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER) == 0 {
                *(lparam as *mut Hwnd) = hwnd;
                return 0; // stop
            }
            1 // continue
        }
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: Hwnd,
        msg: u32,
        wparam: usize,
        lparam: isize,
        _id: usize,
        _data: usize,
    ) -> isize {
        unsafe {
            // Hover (async PostMessage): SCREEN coords packed into wparam/lparam.
            let hover = WM_HOVER.load(Ordering::Relaxed);
            let hover_end = WM_HOVER_END.load(Ordering::Relaxed);
            if hover != 0 && msg == hover {
                dispatch(Incoming::Hover(wparam as i32, lparam as i32));
                return 0;
            }
            if hover_end != 0 && msg == hover_end {
                dispatch(Incoming::HoverEnd);
                return 0;
            }
            if msg == WM_COPYDATA {
                let cds = &*(lparam as *const CopyDataStruct);
                // Only payloads marked with OUR protocol are processed: a
                // WM_COPYDATA from a third-party application passes through to
                // DefSubclassProc.
                if cds.dw_data != PROTO_TAB_V1 {
                    return DefSubclassProc(hwnd, msg, wparam, lparam);
                }
                if !cds.lp_data.is_null() && cds.cb_data > 0 {
                    let bytes =
                        std::slice::from_raw_parts(cds.lp_data as *const u8, cds.cb_data as usize);
                    let s = String::from_utf8_lossy(bytes).into_owned();
                    // DEFERRED processing on the UI thread (immediate return → the
                    // source isn't left blocked in its synchronous SendMessage).
                    dispatch(Incoming::Transfer(s));
                }
                return 1; // TRUE = message handled
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
    }

    /// Marks + subclasses this instance's window and registers the handler
    /// for incoming messages. Must be called on the UI thread, AFTER the
    /// window exists (HWND created). Idempotent; `false` = window not found
    /// yet → retry.
    pub fn init(handler: impl Fn(Incoming) + 'static) -> bool {
        if SELF_HWND.load(Ordering::SeqCst) != 0 {
            return true; // already initialized
        }
        unsafe {
            let mut hwnd: Hwnd = 0;
            EnumThreadWindows(
                GetCurrentThreadId(),
                find_main_cb,
                &mut hwnd as *mut Hwnd as isize,
            );
            if hwnd == 0 {
                return false; // window not (yet) visible → caller retries
            }
            if SetWindowSubclass(hwnd, subclass_proc, SUBCLASS_ID, 0) == 0 {
                return false; // subclassing refused → IPC inactive, retryable
            }
            SetPropW(hwnd, prop_name().as_ptr(), 1);
            WM_HOVER.store(
                RegisterWindowMessageW(hover_msg_name().as_ptr()),
                Ordering::SeqCst,
            );
            WM_HOVER_END.store(
                RegisterWindowMessageW(hover_end_msg_name().as_ptr()),
                Ordering::SeqCst,
            );
            SELF_HWND.store(hwnd, Ordering::SeqCst);
        }
        HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(handler)));
        true
    }

    /// Signals a HOVER in progress at SCREEN point `(x, y)` (physical) to the
    /// target window (async, non-blocking). Coords packed into wparam/lparam.
    pub fn hover(target: isize, x: i32, y: i32) {
        let msg = WM_HOVER.load(Ordering::SeqCst);
        if msg != 0 {
            unsafe { PostMessageW(target, msg, x as isize as usize, y as isize) };
        }
    }

    /// Signals the END of a hover to the target window (clears its preview).
    pub fn hover_end(target: isize) {
        let msg = WM_HOVER_END.load(Ordering::SeqCst);
        if msg != 0 {
            unsafe { PostMessageW(target, msg, 0, 0) };
        }
    }

    /// Ranger window OTHER than self under the SCREEN point `(x, y)`
    /// (physical). `None` if the point isn't over a third-party Ranger window.
    pub fn target_at(x: i32, y: i32) -> Option<isize> {
        unsafe {
            let top = GetAncestor(WindowFromPoint(Point { x, y }), GA_ROOT);
            if top == 0 || top == SELF_HWND.load(Ordering::SeqCst) {
                return None;
            }
            if GetPropW(top, prop_name().as_ptr()) != 0 {
                Some(top)
            } else {
                None
            }
        }
    }

    /// Main HWND of this instance, available after [`init`]. Shared with the
    /// OLE file drop target to avoid a second, fragile per-thread window
    /// detection.
    pub fn self_hwnd() -> Option<isize> {
        let hwnd = SELF_HWND.load(Ordering::SeqCst);
        (hwnd != 0).then_some(hwnd)
    }

    /// Converts a point from the Ranger window's physical CLIENT coordinate
    /// space to screen coordinates. Unlike `Window::position`, this excludes
    /// exactly the Win32 borders and title bar.
    pub fn client_to_screen(x: i32, y: i32) -> Option<(i32, i32)> {
        let hwnd = SELF_HWND.load(Ordering::SeqCst);
        if hwnd == 0 {
            return None;
        }
        let mut point = Point { x, y };
        (unsafe { ClientToScreen(hwnd, &mut point) } != 0).then_some((point.x, point.y))
    }

    /// Inverse conversion, used to transform OLE and IPC screen coordinates
    /// into CLIENT coordinates before the Slint hit-test.
    pub fn screen_to_client(x: i32, y: i32) -> Option<(i32, i32)> {
        let hwnd = SELF_HWND.load(Ordering::SeqCst);
        if hwnd == 0 {
            return None;
        }
        let mut point = Point { x, y };
        (unsafe { ScreenToClient(hwnd, &mut point) } != 0).then_some((point.x, point.y))
    }

    /// Activates the target window after an OLE drop. Without this, the first
    /// click on the drop menu would sometimes only activate the window,
    /// without triggering the chosen entry.
    pub fn focus_self() {
        let hwnd = SELF_HWND.load(Ordering::SeqCst);
        if hwnd != 0 {
            unsafe {
                SetForegroundWindow(hwnd);
                SetFocus(hwnd);
            }
        }
    }

    /// Sends the payload (UTF-8) to the target window and brings it to the
    /// foreground. `true` ONLY if the target PROCESSED the message (LRESULT of
    /// our subclassed proc) — the caller only removes the source tab in that
    /// case (otherwise it would be lost). Bounded timeout: a frozen target
    /// doesn't block the source.
    pub fn send(target: isize, payload: &str) -> bool {
        let bytes = payload.as_bytes();
        let cds = CopyDataStruct {
            dw_data: PROTO_TAB_V1,
            cb_data: bytes.len() as u32,
            lp_data: bytes.as_ptr() as *const core::ffi::c_void,
        };
        unsafe {
            let self_hwnd = SELF_HWND.load(Ordering::SeqCst) as usize;
            let mut lresult: usize = 0;
            let rc = SendMessageTimeoutW(
                target,
                WM_COPYDATA,
                self_hwnd,
                &cds as *const CopyDataStruct as isize,
                SMTO_BLOCK | SMTO_ABORTIFHUNG,
                SEND_TIMEOUT_MS,
                &mut lresult,
            );
            if rc == 0 || lresult != 1 {
                return false; // target frozen / gone / message not handled
            }
            SetForegroundWindow(target);
        }
        true
    }
}

#[cfg(not(windows))]
mod imp {
    /// Incoming message (inert variant outside Windows). The variants are
    /// filtered (`match`) by the handler but never CONSTRUCTED until
    /// cross-instance IPC is implemented on Linux → `allow(dead_code)`.
    #[allow(dead_code)]
    pub enum Incoming {
        Transfer(String),
        Hover(i32, i32),
        HoverEnd,
    }
    /// No-op outside Windows (`true` = don't retry). Cross-instance IPC is
    /// Windows-only for now (Linux: window detection + IPC still to be defined).
    pub fn init(_handler: impl Fn(Incoming) + 'static) -> bool {
        true
    }
    pub fn target_at(_x: i32, _y: i32) -> Option<isize> {
        None
    }
    pub fn send(_target: isize, _payload: &str) -> bool {
        false
    }
    pub fn hover(_target: isize, _x: i32, _y: i32) {}
    pub fn hover_end(_target: isize) {}
    // `self_hwnd` / `client_to_screen` / `screen_to_client` / `focus_self`: no
    // stub outside Windows — these window helpers (HWND, coordinate
    // conversions, focus) are only called under `#[cfg(windows)]` (OLE file
    // drag, screen↔client conversions). On Linux, `Window::position()` serves
    // as the fallback directly in the bridge (`window_logical_to_screen`, etc.).
}

pub use imp::{Incoming, hover, hover_end, init, send, target_at};
#[cfg(windows)]
pub use imp::{client_to_screen, focus_self, screen_to_client, self_hwnd};
