//! Local audit log for user-visible file operations.
//!
//! The log is intentionally local-only and append-only. It records *what the
//! user asked Probe Shell to do* (create/delete/edit/chmod/download/upload/…)
//! with a timestamp and target path, but never stores file contents or secrets.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const MAX_LOG_SIZE: u64 = 20 * 1024 * 1024;

pub fn path() -> PathBuf {
    crate::config::log_dir().join("operations.log")
}

/// Append one operation line to `log/operations.log`.
pub fn record(action: &str, target: &str, status: &str, detail: &str) {
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(meta) = fs::metadata(&p) {
        if meta.len() > MAX_LOG_SIZE {
            let _ = fs::rename(&p, p.with_extension("log.1"));
        }
    }
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let target = sanitize_field(target);
    let detail = sanitize_field(detail);
    let line = format!("{now}\t{action}\t{status}\t{target}\t{detail}\n");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn record_sftp(action: &str, target: &str) {
    record(action, target, "请求", "SFTP/SSH文件操作");
}

fn sanitize_field(s: &str) -> String {
    s.replace('\r', " ").replace('\n', " ").replace('\t', " ")
}

pub fn ensure_exists() -> PathBuf {
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if !p.exists() {
        let _ = OpenOptions::new().create(true).append(true).open(&p)
            .and_then(|mut f| f.write_all("time\taction\tstatus\ttarget\tdetail\n".as_bytes()));
    }
    p
}

pub fn open_log_file() {
    let p = ensure_exists();
    open_path(&p);
}

#[cfg(windows)]
fn open_path(path: &std::path::Path) {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteW(
            hwnd: isize,
            lp_operation: *const u16,
            lp_file: *const u16,
            lp_parameters: *const u16,
            lp_directory: *const u16,
            n_show_cmd: i32,
        ) -> isize;
    }
    let to_wide = |s: &OsStr| -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    };
    let op: Vec<u16> = OsStr::new("open").encode_wide().chain(std::iter::once(0)).collect();
    let file = to_wide(path.as_os_str());
    unsafe {
        ShellExecuteW(0, op.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), 1);
    }
}

#[cfg(not(windows))]
fn open_path(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(not(target_os = "macos"))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}
