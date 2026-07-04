//! Crash diagnostics for Probe Shell.
//!
//! Release builds on Windows use `windows_subsystem = "windows"`, so a panic can
//! otherwise look like a silent exit. The panic hook records the panic payload,
//! source location and a backtrace to `log/crash.log` beside the portable exe.

use std::backtrace::Backtrace;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic;
use std::time::{SystemTime, UNIX_EPOCH};

/// Install a process-wide panic hook. Safe to call once during startup.
pub fn install() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        write_panic(info);
        default_hook(info);
    }));
}

/// Path to the crash log. Kept separate from `error.log` so users can quickly
/// find the last hard crash without searching through warning logs.
pub fn path() -> std::path::PathBuf {
    let dir = crate::config::log_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir.join("crash.log")
}

fn write_panic(info: &panic::PanicHookInfo<'_>) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();

    let payload = info
        .payload()
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| info.payload().downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string());

    let location = info
        .location()
        .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
        .unwrap_or_else(|| "<unknown>".to_string());

    let backtrace = Backtrace::force_capture();

    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path()) {
        let _ = writeln!(
            f,
            "\n================ Probe Shell crash ================\ntime_unix: {ts}\nversion: {}\nlocation: {location}\npanic: {payload}\n\nBacktrace:\n{backtrace}\n",
            env!("CARGO_PKG_VERSION")
        );
    }

    tracing::error!(%location, %payload, "panic captured; crash.log written");
}
