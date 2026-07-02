//! Shared UI/session state types used by `app.rs`.
//!
//! `app.rs` is still the top-level Slint callback hub, but pure data types live
//! here so the main file can be split gradually without touching the UI bridge.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::sftp::SftpHandle;
use crate::ssh::ProcInfo;
use crate::system::SystemSnapshot;

pub type SftpHandles = Arc<Mutex<HashMap<String, SftpHandle>>>;

/// Per-tab flag: once the user explicitly navigates via the SFTP tree or
/// toolbar, stop auto-syncing to the terminal's `cd` path.
///
/// Per-tab last cwd the SFTP panel followed (from OSC 7). Used to ignore the
/// OSC 7 every prompt re-emits at an unchanged directory; manual SFTP
/// navigation REMOVES the entry so the very next OSC 7 — same directory or
/// not — snaps the panel back to the shell's cwd.
pub type SftpLastCwd = Arc<Mutex<HashMap<String, String>>>;

/// Per-tab connection status + latest remote resource sample, used to drive the
/// sidebar for whichever tab is active. `Arc<Mutex>` because SSH event-pump
/// threads update it before bouncing to the UI thread.
#[derive(Clone, Default)]
pub struct TabStatus {
    /// Display host, e.g. `root@192.168.100.2`.
    pub host: String,
    /// Saved-session id, used to reconnect in place.
    pub session_id: String,
    /// 0 = connecting, 1 = connected, 2 = disconnected.
    pub state: u8,
    /// CPU usage in 0.0..1.0.
    pub cpu: f32,
    pub mem_used_kib: u64,
    pub mem_total_kib: u64,
    pub swap_used_kib: u64,
    pub swap_total_kib: u64,
    /// Latest per-interface rates: (name, rx_bps, tx_bps), busiest first.
    pub net: Vec<(String, u64, u64)>,
    /// Which interface drives the top sparkline (empty = auto = busiest).
    pub selected_iface: String,
    /// Ring buffer of the selected interface's total (rx+tx) bytes/sec.
    pub net_hist: Vec<f32>,
    /// Per-filesystem (mount, available_bytes, total_bytes).
    pub disks: Vec<(String, u64, u64)>,
    /// Top remote processes by CPU, for the process monitor popup.
    pub procs: Vec<ProcInfo>,
}

pub type TabStatuses = Arc<Mutex<HashMap<String, TabStatus>>>;

/// Last local-machine sample (shown on the welcome tab).
pub type LocalSnap = Arc<Mutex<SystemSnapshot>>;
