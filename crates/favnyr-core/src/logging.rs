//! Initialization of the `tracing` subscriber for the GUI binary.
//!
//! Outputs: **console** (stderr — mostly useful in debug) **and** a **file**
//! `cache/ranger.log`. The file is essential in release builds: the app then
//! runs without a console (`windows_subsystem = "windows"`), so logs would
//! have nowhere else to go — and the "open cache folder" button in settings
//! gives access to it for diagnosing an issue.
//!
//! The file is **truncated on every session** (bounded size, no rotation →
//! no extra dependency added): it always reflects the latest run.
//!
//! Format driven by `RANGER_LOG_FORMAT`: `json` | `pretty` | other (compact).
//! Default filter `info`, overridable via `RUST_LOG`.

use std::sync::Mutex;

use tracing_subscriber::fmt::writer::MakeWriterExt;
use tracing_subscriber::{EnvFilter, fmt};

/// Initializes the global subscriber. Idempotent: a second call is ignored.
pub fn init() {
    let env = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let format = std::env::var("RANGER_LOG_FORMAT").unwrap_or_default();

    // Best-effort log file: create the cache folder then open the file in
    // TRUNCATE mode (one session = one file). `None` if unavailable →
    // fall back to console only.
    let log_file = {
        let path = crate::paths::log_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .ok()
    };

    let res = match log_file {
        Some(file) => {
            // Console + file. `stderr` as an explicit fn-pointer (avoids type
            // inference friction with `MakeWriterExt::and`). ANSI disabled: the
            // file stays readable (no color escape codes).
            let stderr = std::io::stderr as fn() -> std::io::Stderr;
            let writer = stderr.and(Mutex::new(file));
            match format.as_str() {
                "json" => fmt()
                    .with_env_filter(env)
                    .json()
                    .with_writer(writer)
                    .try_init(),
                "pretty" => fmt()
                    .with_env_filter(env)
                    .pretty()
                    .with_ansi(false)
                    .with_writer(writer)
                    .try_init(),
                _ => fmt()
                    .with_env_filter(env)
                    .compact()
                    .with_ansi(false)
                    .with_writer(writer)
                    .try_init(),
            }
        }
        None => match format.as_str() {
            "json" => fmt().with_env_filter(env).json().try_init(),
            "pretty" => fmt().with_env_filter(env).pretty().try_init(),
            _ => fmt().with_env_filter(env).compact().try_init(),
        },
    };

    if let Err(err) = res {
        eprintln!("[ranger] tracing init failed: {err}");
    }
}
