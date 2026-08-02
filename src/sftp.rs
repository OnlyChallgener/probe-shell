//! SFTP subsystem worker.
//!
//! Each terminal tab that spawns an SSH shell also spawns a *separate* SSH
//! connection for SFTP. This keeps the shell PTY completely unblocked: large
//! file transfers cannot stall readline or vim.
//!
//! The public API is a simple command channel (`SftpHandle::commands`) that
//! accepts `SftpCommand` messages. Results and status updates are pushed back
//! via the shared `UnboundedSender<SessionEvent>` that already exists for the
//! terminal tab.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use russh::client::{self, Handler};
use russh::keys::key::PrivateKeyWithHashAlg;
use russh::keys::load_secret_key;
use russh::Disconnect;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use futures::stream::{FuturesUnordered, StreamExt};
use ssh_key::{HashAlg, PublicKey};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::config::{AuthMethod, Session};
use crate::i18n::t;
use crate::ssh::{
    format_mtime, format_size, RemoteEntry, RemoteTreeNode, SessionEvent, SftpSearchState,
};

const SFTP_DIR_TIMEOUT: Duration = Duration::from_secs(22);
const SFTP_STAT_TIMEOUT: Duration = Duration::from_secs(8);
const SSH_BROWSER_EXEC_TIMEOUT: Duration = Duration::from_secs(18);
const SSH_UPLOAD_FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);
const SSH_UPLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(150);
const SFTP_MAX_RECURSIVE_NODES: usize = 20_000;

type CancelFlag = Arc<AtomicBool>;
type SftpTaskSet = Arc<Mutex<Vec<JoinHandle<()>>>>;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Commands sent to the SFTP worker task from the UI thread.
#[derive(Debug)]
pub enum SftpCommand {
    /// List the contents of a remote directory.
    ListDir(String),
    /// Refresh button: re-list the directory *and* re-sync the whole expanded
    /// left tree, so external/own changes (deleted/created dirs) show up without
    /// a reconnect (#189). Plain navigation uses `ListDir` to avoid the extra
    /// per-click tree round-trips.
    RefreshDir(String),
    /// Toggle a directory node in the tree (expand if collapsed, collapse if expanded).
    ToggleTreeNode(String),
    /// Search files/folders below a specific directory. This is intentionally
    /// bounded so a router flash filesystem or slow WAN session cannot freeze
    /// the UI by walking the whole system forever.
    Search {
        search_id: String,
        root: String,
        query: String,
    },
    /// Cancel the currently running bounded recursive search. Search runs on a
    /// separate task so directory navigation/refresh can continue while it is active.
    CancelSearch,
    /// Download a remote file to a local directory.
    Download { remote: String, local_dir: String },
    /// Multi-select download (#100): tar the named entries under `remote_dir`
    /// into one archive on the remote, download it, then delete the temp.
    DownloadArchive {
        remote_dir: String,
        names: Vec<String>,
        local_dir: String,
    },
    /// Cancel an in-progress transfer by its id (#100). The partial local file
    /// (and any remote temp archive) are cleaned up.
    CancelTransfer(String),
    /// Upload a local file into a remote directory.
    Upload { local: String, remote_dir: String },
    /// Delete a remote file (falls back to removing an empty directory).
    Delete(String),
    /// Download a file to a temp dir and open it with the OS default app
    /// ("Open/Edit externally", #81). When `edit` is set, watch the temp copy
    /// and re-upload on every change.
    OpenTemp { remote: String, edit: bool },
    /// Rename / move a remote file or directory (#69).
    Rename { from: String, to: String },
    /// Change a remote path's permission bits (POSIX mode, e.g. 0o755) (#69).
    Chmod { path: String, mode: u32 },
    /// Create an empty remote directory (#69).
    MkDir(String),
    /// Create an empty remote file (#69).
    TouchFile(String),
    /// Read a remote file's text for the built-in viewer/editor (#70).
    ReadText { remote: String, edit: bool },
    /// Overwrite a remote file with text from the built-in editor (#70).
    WriteText { remote: String, content: String },
    /// Gracefully shut down the SFTP worker.
    Close,
}

/// Handle retained by the UI to drive a running SFTP worker.
pub struct SftpHandle {
    pub commands: UnboundedSender<SftpCommand>,
    #[allow(dead_code)]
    pub join: JoinHandle<()>,
    shutdown: CancelFlag,
    tasks: SftpTaskSet,
}

impl SftpHandle {
    pub fn list_dir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ListDir(path));
    }
    pub fn refresh_dir(&self, path: String) {
        let _ = self.commands.send(SftpCommand::RefreshDir(path));
    }
    pub fn download(&self, remote: String, local_dir: String) {
        crate::operation_log::record_sftp("下载", &remote);
        let _ = self
            .commands
            .send(SftpCommand::Download { remote, local_dir });
    }
    pub fn download_archive(&self, remote_dir: String, names: Vec<String>, local_dir: String) {
        crate::operation_log::record("打包下载", &remote_dir, "请求", &format!("{} 项", names.len()));
        let _ = self.commands.send(SftpCommand::DownloadArchive {
            remote_dir,
            names,
            local_dir,
        });
    }
    pub fn cancel_transfer(&self, id: String) {
        let _ = self.commands.send(SftpCommand::CancelTransfer(id));
    }
    pub fn upload(&self, local: String, remote_dir: String) {
        crate::operation_log::record("上传", &remote_dir, "请求", &local);
        let _ = self
            .commands
            .send(SftpCommand::Upload { local, remote_dir });
    }
    pub fn toggle_tree_node(&self, path: String) {
        let _ = self.commands.send(SftpCommand::ToggleTreeNode(path));
    }
    pub fn search(&self, search_id: String, root: String, query: String) {
        crate::operation_log::record("搜索", &root, "请求", &query);
        let _ = self.commands.send(SftpCommand::Search {
            search_id,
            root,
            query,
        });
    }
    pub fn cancel_search(&self) {
        crate::operation_log::record("停止搜索", "", "请求", "");
        let _ = self.commands.send(SftpCommand::CancelSearch);
    }
    pub fn delete(&self, path: String) {
        crate::operation_log::record_sftp("删除", &path);
        let _ = self.commands.send(SftpCommand::Delete(path));
    }
    pub fn open_temp(&self, remote: String, edit: bool) {
        crate::operation_log::record_sftp(if edit { "外部编辑" } else { "外部查看" }, &remote);
        let _ = self.commands.send(SftpCommand::OpenTemp { remote, edit });
    }
    pub fn rename(&self, from: String, to: String) {
        crate::operation_log::record("重命名", &from, "请求", &format!("-> {to}"));
        let _ = self.commands.send(SftpCommand::Rename { from, to });
    }
    pub fn chmod(&self, path: String, mode: u32) {
        crate::operation_log::record("修改权限", &path, "请求", &format!("{:o}", mode & 0o7777));
        let _ = self.commands.send(SftpCommand::Chmod { path, mode });
    }
    pub fn mkdir(&self, path: String) {
        crate::operation_log::record_sftp("新建文件夹", &path);
        let _ = self.commands.send(SftpCommand::MkDir(path));
    }
    pub fn touch(&self, path: String) {
        crate::operation_log::record_sftp("新建文件", &path);
        let _ = self.commands.send(SftpCommand::TouchFile(path));
    }
    pub fn read_text(&self, remote: String, edit: bool) {
        crate::operation_log::record_sftp(if edit { "内置编辑" } else { "查看" }, &remote);
        let _ = self.commands.send(SftpCommand::ReadText { remote, edit });
    }
    pub fn write_text(&self, remote: String, content: String) {
        crate::operation_log::record_sftp("保存编辑", &remote);
        let _ = self.commands.send(SftpCommand::WriteText { remote, content });
    }
    pub fn close(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        let _ = self.commands.send(SftpCommand::Close);
        let tasks = self.tasks.clone();
        let worker = self.join.abort_handle();
        std::thread::spawn(move || {
            // Give cooperative cancellation a short window to remove partial
            // transfer output, then force-stop anything still blocked in I/O.
            std::thread::sleep(Duration::from_millis(250));
            abort_tracked_tasks(&tasks);
            worker.abort();
        });
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Spawn an SFTP worker on the Tokio runtime.
///
/// The worker opens its own SSH connection to the same server, authenticates,
/// and requests the `sftp` subsystem. Events (directory listings, progress,
/// errors) are sent back via `events`, which is the same sender used by the
/// terminal's shell session.
/// Turn an SFTP-worker failure into a status-bar message.
///
/// SFTP runs on its own SSH connection, fully separate from the shell PTY, so
/// when it can't connect the terminal keeps working — we just surface why in the
/// SFTP panel. The common bastion/jump-host case is "shell is allowed but the
/// `sftp` subsystem is not", which shows up as a failed subsystem request /
/// channel / handshake (or an explicit "permission denied"). For that family we
/// give a plain-language hint instead of the raw russh error (#190).
fn friendly_sftp_error(err: &anyhow::Error) -> String {
    let chain = err
        .chain()
        .map(|e| e.to_string().to_lowercase())
        .collect::<Vec<_>>()
        .join(" | ");
    let permission_like = [
        "subsystem",       // server refused the `sftp` subsystem request
        "sftp channel",    // channel_open_session refused
        "sftp handshake",  // subsystem opened but no SFTP server behind it
        "permission",
        "denied",
        "prohibited",      // "administratively prohibited"
        "not allowed",
    ]
    .iter()
    .any(|k| chain.contains(k));
    if permission_like {
        t(
            "SFTP 不可用,请检查是否有访问权限(服务器可能未开放 SFTP)",
            "SFTP unavailable — check whether you have permission (server may not allow SFTP)",
        )
        .to_string()
    } else {
        format!("{}: {err:#}", t("SFTP 错误", "SFTP error"))
    }
}

pub fn spawn_sftp(
    runtime: &tokio::runtime::Handle,
    session: Session,
    events: UnboundedSender<SessionEvent>,
) -> SftpHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let self_tx = cmd_tx.clone();
    let events_err = events.clone();
    let shutdown: CancelFlag = Arc::new(AtomicBool::new(false));
    let tasks: SftpTaskSet = Arc::new(Mutex::new(Vec::new()));
    let shutdown_worker = shutdown.clone();
    let tasks_worker = tasks.clone();
    let join = runtime.spawn(async move {
        if let Err(err) = run_sftp(session, cmd_rx, self_tx, events, shutdown_worker, tasks_worker).await {
            let _ = events_err.send(SessionEvent::SftpStatus(friendly_sftp_error(&err)));
        }
    });
    SftpHandle {
        commands: cmd_tx,
        join,
        shutdown,
        tasks,
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Tree state helpers
// ---------------------------------------------------------------------------

/// Recursively build the flat node list from tree state (DFS pre-order).
fn build_tree_nodes(
    path: &str,
    depth: u32,
    expanded: &std::collections::HashSet<String>,
    tree_dirs: &std::collections::HashMap<String, Vec<(String, String)>>,
    nodes: &mut Vec<RemoteTreeNode>,
) {
    let name = if path == "/" {
        "/".to_string()
    } else {
        path.rsplit('/').next().unwrap_or(path).to_string()
    };
    let children = tree_dirs.get(path);
    let has_children = children.map(|c| !c.is_empty()).unwrap_or(false);
    let is_expanded = expanded.contains(path);
    nodes.push(RemoteTreeNode {
        path: path.to_string(),
        name,
        depth,
        expanded: is_expanded,
        has_children,
    });
    if is_expanded {
        if let Some(ch) = children {
            for (_, child_path) in ch {
                build_tree_nodes(child_path, depth + 1, expanded, tree_dirs, nodes);
            }
        }
    }
}

/// Rebuild the flat tree node list from the current cache and push it to the UI.
fn emit_tree(
    tree_dirs: &std::collections::HashMap<String, Vec<(String, String)>>,
    tree_expanded: &std::collections::HashSet<String>,
    events: &UnboundedSender<SessionEvent>,
) {
    let mut nodes = Vec::new();
    build_tree_nodes("/", 0, tree_expanded, tree_dirs, &mut nodes);
    let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
}


/// Ensure the left tree is expanded down to `path`. This keeps the tree in sync
/// with the right file list after navigation such as /dev/pts: / and /dev are
/// expanded and /dev/pts becomes visible/highlightable without changing the
/// right-side listing.
async fn ensure_tree_path(
    sftp: &SftpSession,
    path: &str,
    tree_dirs: &mut std::collections::HashMap<String, Vec<(String, String)>>,
    tree_expanded: &mut std::collections::HashSet<String>,
) {
    let target = normalize_tree_path(path);
    if !tree_dirs.contains_key("/") {
        let _ = cache_tree_dir(sftp, "/", tree_dirs).await;
    }
    tree_expanded.insert("/".to_string());
    if target == "/" {
        return;
    }

    let mut current = "/".to_string();
    for segment in target.trim_start_matches('/').split('/') {
        if segment.is_empty() {
            continue;
        }
        let child = format!("{}/{}", current.trim_end_matches('/'), segment);
        if !tree_dirs.contains_key(&current) {
            let _ = cache_tree_dir(sftp, &current, tree_dirs).await;
        }
        let found = tree_dirs
            .get(&current)
            .map(|c| c.iter().any(|(_, p)| p == &child))
            .unwrap_or(false);
        if !found {
            break;
        }
        // The right file list has already read `target`. Fetching that same
        // directory again only to expand the left navigation doubled every
        // folder click on slow routers. Cache ancestors as needed, but defer the
        // target's own child listing until the user explicitly expands it.
        if child != target && !tree_dirs.contains_key(&child) {
            let _ = cache_tree_dir(sftp, &child, tree_dirs).await;
        }
        tree_expanded.insert(child.clone());
        current = child;
    }
}

async fn cache_tree_dir(
    sftp: &SftpSession,
    dir: &str,
    tree_dirs: &mut std::collections::HashMap<String, Vec<(String, String)>>,
) -> bool {
    match list_dirs_only_impl(sftp, dir).await {
        Ok(dirs) => {
            tree_dirs.insert(dir.to_string(), dirs);
            true
        }
        Err(err) => {
            tracing::debug!("sftp tree cache kept for {dir}: {err:#}");
            false
        }
    }
}

fn normalize_tree_path(path: &str) -> String {
    let p = path.trim();
    if p.is_empty() || p == "." {
        return "/".to_string();
    }
    let mut out = p.replace('\\', "/");
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    if !out.starts_with('/') {
        out.insert(0, '/');
    }
    out
}

/// Re-fetch a directory's sub-directories into the tree cache, but only if that
/// directory is already known to the tree (root or previously expanded) — so a
/// mutation under a collapsed/unknown branch doesn't graft unrelated nodes in.
/// This is how create/delete/rename keep the left tree in sync without a
/// reconnect (#189).
async fn sync_tree_dir(
    sftp: &SftpSession,
    dir: &str,
    tree_dirs: &mut std::collections::HashMap<String, Vec<(String, String)>>,
) {
    if tree_dirs.contains_key(dir) {
        let _ = cache_tree_dir(sftp, dir, tree_dirs).await;
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

async fn run_sftp(
    session: Session,
    mut commands: UnboundedReceiver<SftpCommand>,
    self_tx: UnboundedSender<SftpCommand>,
    events: UnboundedSender<SessionEvent>,
    shutdown: CancelFlag,
    tasks: SftpTaskSet,
) -> Result<()> {
    let _ = events.send(SessionEvent::SftpStatus(t("SFTP 连接中...", "SFTP connecting...").into()));

    // Open a dedicated SSH connection for SFTP.
    let config = Arc::new(client::Config {
        // Keep the idle SFTP connection alive (#160). Without a keepalive, an idle
        // SFTP connection (no file ops for a while) gets silently dropped by
        // NAT / firewall / server idle timeouts; afterwards every operation fails
        // ("folder read failed"). Send a keepalive every 30 s so traffic never
        // goes quiet; keepalive_max (default 3) still closes a genuinely dead
        // connection after ~90 s of unanswered keepalives.
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        // Match the shell connection's algorithm set so SFTP reaches the same
        // legacy servers (#172) instead of failing with "No common algorithm".
        preferred: russh::Preferred {
            kex: std::borrow::Cow::Borrowed(crate::ssh::COMPAT_KEX),
            cipher: std::borrow::Cow::Borrowed(crate::ssh::COMPAT_CIPHER),
            ..russh::Preferred::DEFAULT
        },
        ..<_>::default()
    });

    let addr = format!("{}:{}", session.host, session.port);
    // Tunnel through the same proxy as the shell session, if configured.
    let mut handle = match crate::proxy::resolve(&session.proxy) {
        Some(p) => {
            let stream = crate::proxy::connect(&p, &session.host, session.port)
                .await
                .with_context(|| format!("sftp proxy connect {} failed", addr))?;
            client::connect_stream(config, stream, sftp_handler(&session, &events))
                .await
                .with_context(|| format!("sftp connect {} failed", addr))?
        }
        None => client::connect(config, addr.as_str(), sftp_handler(&session, &events))
            .await
            .with_context(|| format!("sftp connect {} failed", addr))?,
    };

    // Resolve missing username/password (shares the shell's prompt; the UI
    // de-dupes by session id so SFTP doesn't prompt a second time) (#110).
    let (user, password) = match crate::ssh::resolve_credentials(&session, &events).await {
        Some(c) => c,
        None => return Err(anyhow!(t("已取消登录", "login cancelled"))),
    };

    // --- Authenticate (same method as the shell session) -------------------
    let authed = match session.auth {
        AuthMethod::Password => handle
            .authenticate_password(&user, password.as_str())
            .await
            .context("sftp password auth failed")?,
        AuthMethod::Key => {
            let raw = session.private_key_path.trim();
            if raw.is_empty() {
                return Err(anyhow!(t("私钥路径为空", "private key path is empty")));
            }
            let normalised = raw.replace('\\', "/");
            let key_path = normalised
                .strip_suffix(".pub")
                .map(str::to_string)
                .unwrap_or(normalised);
            // An encrypted private key needs its passphrase; reuse the session's
            // password field for it (empty = unencrypted), exactly like the shell
            // session does — otherwise a passphrase-protected key authenticates the
            // shell but fails SFTP with "the key is encrypted" (#133).
            let pass = password.as_str();
            let keypair = load_secret_key(
                Path::new(&key_path),
                if pass.is_empty() { None } else { Some(pass) },
            )
            .with_context(|| format!("failed to load key {key_path}"))?;
            // RSA keys need an explicit SHA-2 hash; other key types don't.
            let hash = keypair.algorithm().is_rsa().then_some(HashAlg::Sha256);
            let key_with_hash = PrivateKeyWithHashAlg::new(Arc::new(keypair), hash)
                .context("invalid private key")?;
            handle
                .authenticate_publickey(&user, key_with_hash)
                .await
                .context("sftp publickey auth failed")?
        }
    };

    if !authed {
        return Err(anyhow!(t("SFTP 认证失败", "SFTP authentication failed")));
    }

    // --- Open remote file browser ------------------------------------------
    // Directory browsing is safest when native SFTP is available: it can list
    // directories, identify file/folder metadata, transfer files, and does not
    // depend on extra shell exec channels.  Therefore Auto mode is now:
    //   1) try native SFTP first
    //   2) fall back to SSH-browser only when the SFTP subsystem is unavailable
    //   3) SCP remains a transfer fallback concept, not a directory browser.
    // This avoids the common OpenWrt/Dropbear failure where an extra exec channel
    // is refused while the SSH shell itself is still connected.
    let sftp = match open_sftp_subsystem(&handle).await {
        Ok(sftp) => {
            let _ = events.send(SessionEvent::SftpStatus(t(
                "SFTP 模式",
                "SFTP mode",
            ).into()));
            sftp
        }
        Err(err) => {
            if shell_pwd(&handle).await.is_ok() {
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} · {}",
                    t("SSH 文件浏览模式", "SSH file-browser mode"),
                    t("未检测到标准 SFTP", "native SFTP not detected")
                )));
                return run_ssh_file_browser(handle, commands, events, shutdown, tasks).await;
            }
            let _ = events.send(SessionEvent::SftpStatus(format!(
                "{}: {err:#}",
                t("文件浏览不可用", "Remote file browser unavailable")
            )));
            return Err(err);
        }
    };
    // Share the session + connection so transfers can run on their own task,
    // leaving the command loop free to list/switch directories meanwhile (#116-2).
    let sftp = std::sync::Arc::new(sftp);
    let handle = std::sync::Arc::new(handle);

    // Per-transfer cancel flags, keyed by transfer id. A download task registers
    // its flag here; a CancelTransfer command flips it; the task removes it on
    // exit (#100 cancel download).
    let cancels: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Resolve the home directory and do an initial listing.
    let home = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| "/".to_string());
    let _ = events.send(SessionEvent::SftpStatus(format!("{} {}...", t("SFTP 加载", "SFTP loading"), home)));
    match list_dir_impl(&sftp, &home).await {
        Ok(entries) => {
            let _ = events.send(SessionEvent::SftpEntries {
                path: home.clone(),
                entries,
            });
            let _ = events.send(SessionEvent::SftpStatus(home.clone()));
        }
        Err(e) => {
            let _ = events.send(SessionEvent::SftpError(list_error_msg(&home, &e)));
        }
    }

    // --- Directory tree initialization -------------------------------------
    // tree_dirs: path -> [(child_name, child_full_path)] for directories only
    // tree_expanded: set of paths currently shown as expanded
    let mut tree_dirs: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut tree_expanded: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Fetch root "/" subdirs, then expand path down to home.
    let _ = cache_tree_dir(&sftp, "/", &mut tree_dirs).await;
    tree_expanded.insert("/".to_string());

    // Walk each path segment from "/" toward home, expanding as we go.
    if home != "/" {
        let mut current = "/".to_string();
        for segment in home.trim_start_matches('/').split('/') {
            if segment.is_empty() {
                continue;
            }
            let child = format!("{}/{}", current.trim_end_matches('/'), segment);
            // Only expand if this child appeared in the parent listing.
            let found = tree_dirs
                .get(&current)
                .map(|c| c.iter().any(|(_, p)| p == &child))
                .unwrap_or(false);
            if !found {
                break;
            }
            let _ = cache_tree_dir(&sftp, &child, &mut tree_dirs).await;
            tree_expanded.insert(child.clone());
            current = child;
        }
    }
    {
        let mut nodes = Vec::new();
        build_tree_nodes("/", 0, &tree_expanded, &tree_dirs, &mut nodes);
        let _ = events.send(SessionEvent::SftpTreeUpdate(nodes));
    }

    // --- Command loop -------------------------------------------------------
    // Keep long recursive searches off the command loop. Otherwise a router search
    // makes navigation/refresh feel frozen until the walk finishes.
    let mut search_cancel: Option<Arc<AtomicBool>> = None;
    let mut search_task: Option<tokio::task::AbortHandle> = None;
    let mut active_search: Option<(String, String, String)> = None;
    while let Some(cmd) = commands.recv().await {
        match cmd {
            SftpCommand::Close => {
                shutdown.store(true, Ordering::Relaxed);
                if let Some(cancel) = search_cancel.take() {
                    cancel.store(true, Ordering::Relaxed);
                }
                if let Some(task) = search_task.take() {
                    task.abort();
                }
                if let Ok(c) = cancels.lock() {
                    for flag in c.values() {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
                let _ = events.send(SessionEvent::SftpStatus(t(
                    "文件连接已断开",
                    "File connection disconnected",
                ).into()));
                break;
            }

            SftpCommand::ListDir(path) => {
                let _ = events.send(SessionEvent::SftpStatus(format!("{} {}...", t("加载", "Loading"), path)));
                match list_dir_impl(&sftp, &path).await {
                    Ok(entries) => {
                        let _ = events.send(SessionEvent::SftpEntries {
                            path: path.clone(),
                            entries,
                        });
                        ensure_tree_path(&sftp, &path, &mut tree_dirs, &mut tree_expanded).await;
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                        let _ = events.send(SessionEvent::SftpStatus(path));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpError(list_error_msg(&path, &e)));
                    }
                }
            }

            SftpCommand::RefreshDir(path) => {
                // File panel — same as ListDir.
                let _ = events.send(SessionEvent::SftpStatus(format!("{} {}...", t("加载", "Loading"), path)));
                match list_dir_impl(&sftp, &path).await {
                    Ok(entries) => {
                        let _ = events.send(SessionEvent::SftpEntries {
                            path: path.clone(),
                            entries,
                        });
                        let _ = events.send(SessionEvent::SftpStatus(path.clone()));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpError(list_error_msg(&path, &e)));
                    }
                }
                // Tree — re-fetch every currently-expanded directory so deleted /
                // created folders sync without a reconnect (#189). Stale entries
                // whose parent no longer lists them are simply never walked by
                // build_tree_nodes, so they drop out on the rebuild.
                let expanded: Vec<String> = tree_expanded.iter().cloned().collect();
                for dir in expanded {
                    let _ = cache_tree_dir(&sftp, &dir, &mut tree_dirs).await;
                }
                emit_tree(&tree_dirs, &tree_expanded, &events);
            }

            SftpCommand::ToggleTreeNode(path) => {
                if path == "/" {
                    if tree_expanded.contains("/") {
                        // Root double-click again: collapse all first-level folders.
                        tree_expanded.clear();
                    } else {
                        // Root double-click: refresh and show first-level folders only;
                        // do not touch the right file list.
                        let _ = cache_tree_dir(&sftp, "/", &mut tree_dirs).await;
                        tree_expanded.insert("/".to_string());
                    }
                } else if tree_expanded.contains(&path) {
                    // Collapse this node and all descendants.
                    let prefix = format!("{}/", path.trim_end_matches('/'));
                    tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                } else {
                    // Expand: fetch children if not yet cached.
                    if !tree_dirs.contains_key(&path) {
                        let _ = cache_tree_dir(&sftp, &path, &mut tree_dirs).await;
                    }
                    tree_expanded.insert(path.clone());
                }
                emit_tree(&tree_dirs, &tree_expanded, &events);
            }

            SftpCommand::Search {
                search_id,
                root,
                query,
            } => {
                if let Some(cancel) = search_cancel.take() {
                    cancel.store(true, Ordering::Relaxed);
                }
                if let Some(task) = search_task.take() {
                    task.abort();
                }
                let cancel = Arc::new(AtomicBool::new(false));
                search_cancel = Some(cancel.clone());
                let roots = split_search_roots(&root);
                let result_path = root.clone();
                let query = query.trim().to_string();
                active_search = Some((search_id.clone(), result_path.clone(), query.clone()));
                let sftp = sftp.clone();
                let events = events.clone();
                let shutdown_task = shutdown.clone();
                let task = tokio::spawn(async move {
                    let mut emitter = SearchEmitter::new(
                        &events,
                        search_id,
                        result_path.clone(),
                        query.clone(),
                    );
                    emitter.status(SftpSearchState::Started);
                    for root in roots.iter() {
                        if is_cancelled(&cancel, &shutdown_task) {
                            break;
                        }
                        match search_dir_impl(&sftp, root, &query, 400, 900, cancel.clone(), shutdown_task.clone(), &mut emitter).await {
                            Ok(()) => {}
                            Err(_) => continue,
                        }
                        if emitter.found >= 400 {
                            break;
                        }
                    }
                    emitter.flush();
                    if is_cancelled(&cancel, &shutdown_task) {
                        emitter.status(SftpSearchState::Cancelled);
                        return;
                    }
                    emitter.status(SftpSearchState::Completed);
                });
                search_task = Some(task.abort_handle());
                track_task(&tasks, task);
            }

            SftpCommand::CancelSearch => {
                if let Some(cancel) = search_cancel.take() {
                    cancel.store(true, Ordering::Relaxed);
                    if let Some(task) = search_task.take() {
                        task.abort();
                    }
                    if let Some((search_id, root, query)) = active_search.take() {
                        let _ = events.send(SessionEvent::SftpSearchStatus {
                            search_id,
                            root,
                            query,
                            state: SftpSearchState::Cancelled,
                            found: 0,
                            scanned: 0,
                            elapsed_ms: 0,
                        });
                    }
                } else {
                    let _ = events.send(SessionEvent::SftpStatus(t("当前没有正在运行的搜索", "No active search").into()));
                }
            }

            SftpCommand::Download { remote, local_dir } => {
                // Run on its own task so the command loop stays free to list /
                // switch directories during the transfer (#116-2).
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                // Register a cancel flag up-front under the file id, so a
                // CancelTransfer arriving mid-download can flip it (#100).
                let file_id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                cancels
                    .lock()
                    .unwrap()
                    .insert(file_id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                let shutdown_task = shutdown.clone();
                let task = tokio::spawn(async move {
                // A directory target → recursively mirror the whole tree (#50).
                let is_dir = remote_lstat_is_dir(&sftp, &remote).await.unwrap_or(false);
                if is_dir {
                    let dirname = base_name(&remote);
                    // #100.3: an empty folder downloads nothing — just say so
                    // rather than silently creating an empty local directory.
                    let empty = list_dir_impl(&sftp, &remote)
                        .await
                        .map(|e| e.is_empty())
                        .unwrap_or(false);
                    if empty {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}", t("空文件夹", "Empty folder"), dirname
                        )));
                        return;
                    }
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{} {}/...", t("下载文件夹", "Downloading folder"), dirname
                    )));
                    match download_dir(&sftp, &handle, &remote, &local_dir, &events, &cancel, &shutdown_task).await {
                        Ok(true) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}", t("下载完成", "Downloaded"), dirname
                            )));
                        }
                        Ok(false) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}", t("已取消", "Cancelled"), dirname
                            )));
                        }
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}", t("下载失败", "Download failed")
                            )));
                        }
                    }
                } else {
                    // Sanitize the server-supplied name before it touches the local
                    // filesystem (#26): a malicious server could otherwise craft a
                    // name with traversal, shell-special chars or a Windows reserved
                    // device name to write outside the chosen dir or hit a device.
                    let filename = sanitize_filename(&base_name(&remote));
                    let local_path = format!("{}/{}", local_dir.trim_end_matches('/'), filename);
                    let id = file_id.clone();
                    let _ = events.send(SessionEvent::SftpStatus(format!("{} {}...", t("下载", "Downloading"), filename)));
                    match download_impl(&handle, &remote, &local_path, &filename, &id, &events, &cancel, &shutdown_task).await {
                        Ok(true) => {
                            let _ = events
                                .send(SessionEvent::SftpStatus(format!("{}: {}", t("下载完成", "Downloaded"), filename)));
                        }
                        Ok(false) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!("{}: {}", t("已取消", "Cancelled"), filename)));
                        }
                        Err(e) => {
                            emit_transfer(&events, &id, &filename, false, 0, 0, 2, &e.to_string());
                            let _ = events.send(SessionEvent::SftpStatus(format!("{}: {e}", t("下载失败", "Download failed"))));
                        }
                    }
                }
                cancels_done.lock().unwrap().remove(&file_id);
                });
                track_task(&tasks, task);
            }

            SftpCommand::DownloadArchive {
                remote_dir,
                names,
                local_dir,
            } => {
                // #100: multi-select download. Instead of N concurrent transfers
                // (which raced and dropped files), tar everything into ONE archive
                // on the remote, pull that single file, then delete the temp.
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                // Register a cancel flag up-front so CancelTransfer can flip it (#100).
                let id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                cancels.lock().unwrap().insert(id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                let shutdown_task = shutdown.clone();
                let task = tokio::spawn(async move {
                    let n = names.len();
                    let tmp = format!("/tmp/probe-shell-{}.tar", Uuid::new_v4());
                    // Name the archive after the first item's stem, per the user:
                    // 11.txt → "11等文件.tar". Sanitize since names come from the server.
                    let first = names.first().map(|s| s.as_str()).unwrap_or("download");
                    let stem = first
                        .rsplit_once('.')
                        .map(|(a, _)| a)
                        .filter(|a| !a.is_empty())
                        .unwrap_or(first);
                    let arc_name =
                        sanitize_filename(&format!("{}{}.tar", stem, t("等文件", "-and-more")));
                    let local_path =
                        format!("{}/{}", local_dir.trim_end_matches('/'), arc_name);
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{} {} {}...", t("打包下载", "Archiving"), n, t("项", "items")
                    )));
                    // Show a "preparing" row in the transfer panel right away so a
                    // big selection isn't a silent wait while tar runs (#100). The
                    // download then reuses this same id, so the row turns into the
                    // live progress bar once bytes start flowing.
                    emit_transfer(&events, &id, &arc_name, false, 0, 0, 3, "");
                    // Plain tar (no gzip): the user prefers speed over a smaller file.
                    // Server-supplied names are untrusted → quote every argument.
                    let mut cmd =
                        format!("tar -cf {} -C {}", sh_quote(&tmp), sh_quote(&remote_dir));
                    for nm in &names {
                        cmd.push(' ');
                        cmd.push_str(&sh_quote(nm));
                    }
                    let _ = &sftp; // listing session kept alive; transfer uses `handle`
                    let res: Result<bool> = async {
                        let st = exec_remote(&handle, &cmd).await.context("tar on remote")?;
                        if st != 0 {
                            return Err(anyhow!(t("远端 tar 打包失败", "remote tar failed")));
                        }
                        download_impl(&handle, &tmp, &local_path, &arc_name, &id, &events, &cancel, &shutdown_task)
                            .await
                    }
                    .await;
                    // Best-effort cleanup of the remote temp tar — success, failure
                    // or cancel all reach here, so no junk is left on the server (#100).
                    let _ = exec_remote(&handle, &format!("rm -f {}", sh_quote(&tmp))).await;
                    match res {
                        Ok(true) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}", t("下载完成", "Downloaded"), arc_name
                            )));
                        }
                        Ok(false) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}", t("已取消", "Cancelled"), arc_name
                            )));
                        }
                        Err(e) => {
                            emit_transfer(&events, &id, &arc_name, false, 0, 0, 2, &e.to_string());
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}", t("下载失败", "Download failed")
                            )));
                        }
                    }
                    cancels_done.lock().unwrap().remove(&id);
                });
                track_task(&tasks, task);
            }

            SftpCommand::CancelTransfer(id) => {
                if let Some(flag) = cancels.lock().unwrap().get(&id) {
                    flag.store(true, Ordering::Relaxed);
                }
            }

            SftpCommand::Upload { local, remote_dir } => {
                // Run on its own task so the command loop stays free to list /
                // switch directories during the transfer (#116-2).
                let sftp = sftp.clone();
                let handle = handle.clone();
                let events = events.clone();
                // Register a cancel flag up-front under the file id so a
                // CancelTransfer arriving mid-upload can flip it (#100).
                let up_id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                cancels.lock().unwrap().insert(up_id.clone(), cancel.clone());
                let cancels_done = cancels.clone();
                let shutdown_task = shutdown.clone();
                let task = tokio::spawn(async move {
                // A directory source → recursively upload the whole tree (#50).
                let is_dir = tokio::fs::symlink_metadata(&local)
                    .await
                    .map(|m| !m.file_type().is_symlink() && m.is_dir())
                    .unwrap_or(false);
                if is_dir {
                    let dirname = base_name(&local);
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{} {}/...", t("上传文件夹", "Uploading folder"), dirname
                    )));
                    let res = upload_dir(&handle, &sftp, &local, &remote_dir, &events, &cancel, &shutdown_task).await;
                    if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                        let _ = events.send(SessionEvent::SftpEntries {
                            path: remote_dir.clone(),
                            entries,
                        });
                    }
                    match res {
                        Ok(true) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}", t("上传完成", "Uploaded"), dirname
                            )));
                        }
                        Ok(false) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}", t("已取消", "Cancelled"), dirname
                            )));
                        }
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}", t("上传失败", "Upload failed")
                            )));
                        }
                    }
                } else {
                    let filename = base_name(&local);
                    let remote_path = format!("{}/{}", remote_dir.trim_end_matches('/'), filename);
                    let id = up_id.clone();
                    let _ = events.send(SessionEvent::SftpStatus(format!("{} {}...", t("上传", "Uploading"), filename)));
                    match upload_pipelined(&handle, &local, &remote_path, &filename, &id, &events, &cancel, &shutdown_task).await {
                        Ok(true) => {
                            if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                                let _ = events.send(SessionEvent::SftpEntries {
                                    path: remote_dir.clone(),
                                    entries,
                                });
                            }
                            let _ = events
                                .send(SessionEvent::SftpStatus(format!("{}: {}", t("上传完成", "Uploaded"), filename)));
                        }
                        Ok(false) => {
                            // Refresh the listing so the removed partial file disappears.
                            if let Ok(entries) = list_dir_impl(&sftp, &remote_dir).await {
                                let _ = events.send(SessionEvent::SftpEntries {
                                    path: remote_dir.clone(),
                                    entries,
                                });
                            }
                            let _ = events.send(SessionEvent::SftpStatus(format!("{}: {}", t("已取消", "Cancelled"), filename)));
                        }
                        Err(e) => {
                            emit_transfer(&events, &id, &filename, true, 0, 0, 2, &e.to_string());
                            let _ = events.send(SessionEvent::SftpStatus(format!("{}: {e}", t("上传失败", "Upload failed"))));
                        }
                    }
                }
                cancels_done.lock().unwrap().remove(&up_id);
                });
                track_task(&tasks, task);
            }

            SftpCommand::Delete(path) => {
                let filename = base_name(&path);
                let _ = events.send(SessionEvent::SftpStatus(format!("{} {}...", t("删除", "Deleting"), filename)));
                if is_forbidden_remote_path(&path) {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t("鍒犻櫎澶辫触", "Delete failed"),
                        t("拒绝删除危险路径", "Refusing to delete a dangerous path")
                    )));
                    continue;
                }
                // Directories are removed recursively (a plain remove_dir only
                // works on an empty dir, so an uploaded folder couldn't be
                // deleted); files via remove_file.
                let is_dir = remote_lstat_is_dir(&sftp, &path).await.unwrap_or(false);
                let res: Result<()> = if is_dir {
                    remove_dir_recursive(&sftp, &path).await
                } else {
                    sftp.remove_file(&path)
                        .await
                        .map(|_| ())
                        .map_err(|e| anyhow::anyhow!("{e}"))
                };
                match res {
                    Ok(_) => {
                        let parent = parent_dir(&path);
                        if let Ok(entries) = list_dir_impl(&sftp, &parent).await {
                            let _ = events.send(SessionEvent::SftpEntries {
                                path: parent.clone(),
                                entries,
                            });
                        }
                        // Keep the left directory tree in sync (#189): drop the
                        // deleted folder and any cached descendants, then re-list
                        // the parent's sub-dirs so the deleted node disappears
                        // without needing a reconnect.
                        let prefix = format!("{}/", path.trim_end_matches('/'));
                        tree_dirs.retain(|p, _| p != &path && !p.starts_with(&prefix));
                        tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                        sync_tree_dir(&sftp, &parent, &mut tree_dirs).await;
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                        let _ =
                            events.send(SessionEvent::SftpStatus(format!("{}: {}", t("已删除", "Deleted"), filename)));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!("{}: {e}", t("删除失败", "Delete failed"))));
                    }
                }
            }

            SftpCommand::Rename { from, to } => {
                let refresh = parent_dir(&from);
                match sftp.rename(&from, &to).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已重命名", "Renamed"),
                            base_name(&to)
                        )));
                        // Sync the left tree (#189): drop the old name + cached
                        // descendants, then re-list both the source and the
                        // destination parent (rename can also move across dirs).
                        let prefix = format!("{}/", from.trim_end_matches('/'));
                        tree_dirs.retain(|p, _| p != &from && !p.starts_with(&prefix));
                        tree_expanded.retain(|p| p != &from && !p.starts_with(&prefix));
                        sync_tree_dir(&sftp, &refresh, &mut tree_dirs).await;
                        let to_parent = parent_dir(&to);
                        if to_parent != refresh {
                            sync_tree_dir(&sftp, &to_parent, &mut tree_dirs).await;
                        }
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("重命名失败", "Rename failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    let _ = events.send(SessionEvent::SftpEntries { path: refresh, entries });
                }
            }

            SftpCommand::Chmod { path, mode } => {
                let refresh = parent_dir(&path);
                let attrs = FileAttributes {
                    permissions: Some(mode),
                    ..Default::default()
                };
                match sftp.set_metadata(&path, attrs).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {} → {:o}",
                            t("已修改权限", "Permissions changed"),
                            base_name(&path),
                            mode
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("修改权限失败", "chmod failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    let _ = events.send(SessionEvent::SftpEntries { path: refresh, entries });
                }
            }

            SftpCommand::MkDir(path) => {
                let refresh = parent_dir(&path);
                match sftp.create_dir(&path).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已新建文件夹", "Folder created"),
                            base_name(&path)
                        )));
                        // Show the new folder in the left tree too (#189).
                        sync_tree_dir(&sftp, &refresh, &mut tree_dirs).await;
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("新建文件夹失败", "Create folder failed")
                        )));
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    let _ = events.send(SessionEvent::SftpEntries { path: refresh, entries });
                }
            }

            SftpCommand::TouchFile(path) => {
                let refresh = parent_dir(&path);
                // create() truncates if the file exists, so refuse to clobber.
                let exists = sftp.metadata(&path).await.is_ok();
                if exists {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t("文件已存在", "File already exists"),
                        base_name(&path)
                    )));
                } else {
                    match sftp.create(&path).await {
                        Ok(_) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("已新建文件", "File created"),
                                base_name(&path)
                            )));
                        }
                        Err(e) => {
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {e}",
                                t("新建文件失败", "Create file failed")
                            )));
                        }
                    }
                }
                if let Ok(entries) = list_dir_impl(&sftp, &refresh).await {
                    let _ = events.send(SessionEvent::SftpEntries { path: refresh, entries });
                }
            }

            SftpCommand::OpenTemp { remote, edit } => {
                // Sanitize the remote-controlled name before it becomes a local
                // file path that we later hand to the OS "open" call.
                let filename = sanitize_filename(&base_name(&remote));
                let tmp_dir = std::env::temp_dir().join("probe-shell");
                let _ = tokio::fs::create_dir_all(&tmp_dir).await;
                let local = tmp_dir.join(&filename);
                let local_str = local.to_string_lossy().to_string();
                let _ = events.send(SessionEvent::SftpStatus(format!("{} {}...", t("打开", "Opening"), filename)));
                let xid = Uuid::new_v4().to_string();
                let open_cancel = Arc::new(AtomicBool::new(false));
                match download_impl(&handle, &remote, &local_str, &filename, &xid, &events, &open_cancel, &shutdown).await {
                    Ok(true) => {
                        open_with_os(&local_str);
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            if edit { t("已打开编辑", "Opened for editing") } else { t("已打开", "Opened") },
                            filename
                        )));
                        if edit {
                            let watcher = spawn_edit_watcher(
                                self_tx.clone(),
                                local_str,
                                remote.clone(),
                                filename,
                                events.clone(),
                                shutdown.clone(),
                            );
                            track_task(&tasks, watcher);
                        }
                    }
                    Ok(false) => {
                        let _ = tokio::fs::remove_file(&local_str).await;
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}", t("已取消", "Cancelled"), filename
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!("{}: {e}", t("打开失败", "Open failed"))));
                    }
                }
            }
            SftpCommand::ReadText { remote, edit } => {
                let name = base_name(&remote);
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{} {}...",
                    t("打开", "Opening"),
                    name
                )));
                let (content, error) = match read_text_guarded(&sftp, &remote).await {
                    Ok(text) => (text, String::new()),
                    Err(msg) => (String::new(), msg),
                };
                let _ = events.send(SessionEvent::SftpFileText {
                    path: remote,
                    name,
                    content,
                    edit,
                    error,
                });
            }
            SftpCommand::WriteText { remote, content } => {
                let name = base_name(&remote);
                match write_text_file(&sftp, &remote, &content).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已保存", "Saved"),
                            name
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e:#}",
                            t("保存失败", "Save failed")
                        )));
                    }
                }
            }
        }
    }

    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "")
        .await;
    Ok(())
}


/// Try to open the real SFTP subsystem.
async fn open_sftp_subsystem(handle: &client::Handle<SftpClientHandler>) -> Result<SftpSession> {
    let channel = handle
        .channel_open_session()
        .await
        .context("open sftp channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    SftpSession::new(channel.into_stream())
        .await
        .context("sftp handshake")
}

/// Capture stdout/stderr and the exit status from a one-shot SSH exec channel.
async fn exec_capture(
    handle: &client::Handle<SftpClientHandler>,
    cmd: &str,
) -> Result<(u32, Vec<u8>, Vec<u8>)> {
    let mut ch = handle
        .channel_open_session()
        .await
        .context("open exec channel")?;
    ch.exec(true, cmd.as_bytes())
        .await
        .with_context(|| format!("exec remote command: {cmd}"))?;

    let mut status = 0u32;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(msg) = ch.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            russh::ChannelMsg::ExtendedData { data, ext: _ } => stderr.extend_from_slice(&data),
            russh::ChannelMsg::ExitStatus { exit_status } => status = exit_status,
            russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok((status, stdout, stderr))
}

/// SSH-browser exec wrapper. A few embedded SSH servers (especially router
/// builds) temporarily reject opening a new exec channel while the previous
/// command is being torn down. Retrying once after a tiny delay avoids turning
/// a folder click into a dead panel. If the server consistently refuses exec,
/// the user still gets a clear status message instead of an app crash.
async fn exec_capture_browser(
    handle: &client::Handle<SftpClientHandler>,
    cmd: &str,
) -> Result<(u32, Vec<u8>, Vec<u8>)> {
    match tokio::time::timeout(SSH_BROWSER_EXEC_TIMEOUT, exec_capture(handle, cmd)).await {
        Ok(Ok(r)) => Ok(r),
        Ok(Err(e)) => {
            let text = format!("{e:#}").to_lowercase();
            if text.contains("open exec channel")
                || text.contains("administratively prohibited")
                || text.contains("channel open")
            {
                tokio::time::sleep(Duration::from_millis(260)).await;
                match tokio::time::timeout(SSH_BROWSER_EXEC_TIMEOUT, exec_capture(handle, cmd)).await {
                    Ok(Ok(r)) => Ok(r),
                    Ok(Err(e)) => Err(e).with_context(|| "retry after exec channel failure"),
                    Err(_) => Err(anyhow!(
                        "Connection Timeout: exec channel exceeded {}s after retry",
                        SSH_BROWSER_EXEC_TIMEOUT.as_secs()
                    )),
                }
            } else {
                Err(e)
            }
        }
        Err(_) => Err(anyhow!(
            "Connection Timeout: exec channel exceeded {}s",
            SSH_BROWSER_EXEC_TIMEOUT.as_secs()
        )),
    }
}


async fn ensure_shell_tree_path(
    handle: &client::Handle<SftpClientHandler>,
    path: &str,
    tree_dirs: &mut std::collections::HashMap<String, Vec<(String, String)>>,
    tree_expanded: &mut std::collections::HashSet<String>,
) {
    let target = normalize_tree_path(path);
    if !tree_dirs.contains_key("/") {
        let _ = cache_shell_tree_dir(handle, "/", tree_dirs).await;
    }
    tree_expanded.insert("/".to_string());
    if target == "/" { return; }
    let mut current = "/".to_string();
    for segment in target.trim_start_matches('/').split('/') {
        if segment.is_empty() { continue; }
        let child = format!("{}/{}", current.trim_end_matches('/'), segment);
        if !tree_dirs.contains_key(&current) {
            let _ = cache_shell_tree_dir(handle, &current, tree_dirs).await;
        }
        let found = tree_dirs
            .get(&current)
            .map(|c| c.iter().any(|(_, p)| p == &child))
            .unwrap_or(false);
        if !found { break; }
        if child != target && !tree_dirs.contains_key(&child) {
            let _ = cache_shell_tree_dir(handle, &child, tree_dirs).await;
        }
        tree_expanded.insert(child.clone());
        current = child;
    }
}

async fn cache_shell_tree_dir(
    handle: &client::Handle<SftpClientHandler>,
    dir: &str,
    tree_dirs: &mut std::collections::HashMap<String, Vec<(String, String)>>,
) -> bool {
    match shell_list_dirs_only(handle, dir).await {
        Ok(dirs) => {
            tree_dirs.insert(dir.to_string(), dirs);
            true
        }
        Err(err) => {
            tracing::debug!("ssh tree cache kept for {dir}: {err:#}");
            false
        }
    }
}

/// Fallback browser for servers that do not provide the `sftp` subsystem.
///
/// This is intentionally conservative: it uses POSIX shell commands over SSH to
/// browse directories and handle basic file operations. It fixes the common
/// OpenWrt/Dropbear case where interactive SSH works and MobaXterm can browse
/// files, but a strict SFTP client cannot.
async fn run_ssh_file_browser(
    handle: client::Handle<SftpClientHandler>,
    mut commands: UnboundedReceiver<SftpCommand>,
    events: UnboundedSender<SessionEvent>,
    shutdown: CancelFlag,
    tasks: SftpTaskSet,
) -> Result<()> {
    // `russh::client::Handle` itself is not Clone in the version we build
    // against. Wrap it in Arc so background search tasks can share the same
    // browser connection without moving it out of the command loop. This fixes
    // the Windows release build error introduced by multi-scope search while
    // keeping search cancellable and non-blocking for the UI.
    let handle = Arc::new(handle);
    let home = shell_pwd(&handle).await.unwrap_or_else(|_| "/".to_string());
    let _ = events.send(SessionEvent::SftpStatus(t(
        "标准 SFTP 不可用，已切换为 SSH 兼容传输",
        "Standard SFTP unavailable; using SSH-compatible transfer",
    ).into()));

    emit_shell_dir(&handle, &events, &home).await;

    let mut tree_dirs: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut tree_expanded: std::collections::HashSet<String> = std::collections::HashSet::new();
    let _ = cache_shell_tree_dir(&handle, "/", &mut tree_dirs).await;
    tree_expanded.insert("/".to_string());
    ensure_shell_tree_path(&handle, &home, &mut tree_dirs, &mut tree_expanded).await;
    emit_tree(&tree_dirs, &tree_expanded, &events);

    // Same rule as real SFTP mode: recursive search must never block the file
    // browser command loop. This keeps folder expansion and refresh responsive.
    let mut search_cancel: Option<Arc<AtomicBool>> = None;
    let mut search_task: Option<tokio::task::AbortHandle> = None;
    let mut active_search: Option<(String, String, String)> = None;
    // SSH-compatible uploads run on independent exec channels and tasks. The
    // directory command loop therefore remains responsive while bytes flow.
    let transfer_cancels: Arc<Mutex<HashMap<String, CancelFlag>>> =
        Arc::new(Mutex::new(HashMap::new()));
    while let Some(cmd) = commands.recv().await {
        match cmd {
            SftpCommand::Close => {
                shutdown.store(true, Ordering::Relaxed);
                if let Some(cancel) = search_cancel.take() {
                    cancel.store(true, Ordering::Relaxed);
                }
                if let Some(task) = search_task.take() {
                    task.abort();
                }
                if let Ok(cancels) = transfer_cancels.lock() {
                    for cancel in cancels.values() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
                let _ = events.send(SessionEvent::SftpStatus(t(
                    "文件连接已断开",
                    "File connection disconnected",
                ).into()));
                break;
            }
            SftpCommand::ListDir(path) | SftpCommand::RefreshDir(path) => {
                emit_shell_dir(&handle, &events, &path).await;
                ensure_shell_tree_path(&handle, &path, &mut tree_dirs, &mut tree_expanded).await;
                emit_tree(&tree_dirs, &tree_expanded, &events);
            }
            SftpCommand::ToggleTreeNode(path) => {
                if path == "/" {
                    if tree_expanded.contains("/") {
                        tree_expanded.clear();
                    } else {
                        let _ = cache_shell_tree_dir(&handle, "/", &mut tree_dirs).await;
                        tree_expanded.insert("/".to_string());
                    }
                } else if tree_expanded.contains(&path) {
                    let prefix = format!("{}/", path.trim_end_matches('/'));
                    tree_expanded.retain(|p| p != &path && !p.starts_with(&prefix));
                } else {
                    if !tree_dirs.contains_key(&path) {
                        let _ = cache_shell_tree_dir(&handle, &path, &mut tree_dirs).await;
                    }
                    tree_expanded.insert(path.clone());
                }
                emit_tree(&tree_dirs, &tree_expanded, &events);
            }
            SftpCommand::Search {
                search_id,
                root,
                query,
            } => {
                if let Some(cancel) = search_cancel.take() {
                    cancel.store(true, Ordering::Relaxed);
                }
                if let Some(task) = search_task.take() {
                    task.abort();
                }
                let cancel = Arc::new(AtomicBool::new(false));
                search_cancel = Some(cancel.clone());
                let roots = split_search_roots(&root);
                let result_path = root.clone();
                let query = query.trim().to_string();
                active_search = Some((search_id.clone(), result_path.clone(), query.clone()));
                let handle = handle.clone();
                let events = events.clone();
                let shutdown_task = shutdown.clone();
                let task = tokio::spawn(async move {
                    let mut emitter = SearchEmitter::new(
                        &events,
                        search_id,
                        result_path.clone(),
                        query.clone(),
                    );
                    emitter.status(SftpSearchState::Started);
                    for root in roots.iter() {
                        if is_cancelled(&cancel, &shutdown_task) {
                            break;
                        }
                        match shell_search_dir_impl(&handle, root, &query, 400, 900, cancel.clone(), shutdown_task.clone(), &mut emitter).await {
                            Ok(()) => {}
                            Err(_) => continue,
                        }
                        if emitter.found >= 400 {
                            break;
                        }
                    }
                    emitter.flush();
                    if is_cancelled(&cancel, &shutdown_task) {
                        emitter.status(SftpSearchState::Cancelled);
                        return;
                    }
                    emitter.status(SftpSearchState::Completed);
                });
                search_task = Some(task.abort_handle());
                track_task(&tasks, task);
            }
            SftpCommand::CancelSearch => {
                if let Some(cancel) = search_cancel.take() {
                    cancel.store(true, Ordering::Relaxed);
                    if let Some(task) = search_task.take() {
                        task.abort();
                    }
                    if let Some((search_id, root, query)) = active_search.take() {
                        let _ = events.send(SessionEvent::SftpSearchStatus {
                            search_id,
                            root,
                            query,
                            state: SftpSearchState::Cancelled,
                            found: 0,
                            scanned: 0,
                            elapsed_ms: 0,
                        });
                    }
                } else {
                    let _ = events.send(SessionEvent::SftpStatus(t("当前没有正在运行的搜索", "No active search").into()));
                }
            }
            SftpCommand::MkDir(path) => {
                let refresh = parent_dir(&path);
                let cmd = format!("mkdir -p {}", sh_quote(&path));
                emit_shell_action(&handle, &events, &cmd, t("新建文件夹失败", "Create folder failed")).await;
                if tree_dirs.contains_key(&refresh) {
                    if let Ok(dirs) = shell_list_dirs_only(&handle, &refresh).await {
                        tree_dirs.insert(refresh.clone(), dirs);
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                }
                emit_shell_dir(&handle, &events, &refresh).await;
            }
            SftpCommand::TouchFile(path) => {
                let refresh = parent_dir(&path);
                let cmd = format!("test -e {0} || : > {0}", sh_quote(&path));
                emit_shell_action(&handle, &events, &cmd, t("新建文件失败", "Create file failed")).await;
                emit_shell_dir(&handle, &events, &refresh).await;
            }
            SftpCommand::Delete(path) => {
                let refresh = parent_dir(&path);
                if is_forbidden_remote_path(&path) {
                    let _ = events.send(SessionEvent::SftpStatus(format!(
                        "{}: {}",
                        t("鍒犻櫎澶辫触", "Delete failed"),
                        t("拒绝删除危险路径", "Refusing to delete a dangerous path")
                    )));
                    continue;
                }
                let cmd = format!(
                    concat!(
                        "p={0}; ",
                        "[ -n \"$p\" ] && [ \"$p\" != / ] && [ \"$p\" != . ] && [ \"$p\" != .. ] || exit 64; ",
                        "if [ -L \"$p\" ] || [ ! -d \"$p\" ]; then rm -f -- \"$p\"; ",
                        "else excess=$(find \"$p\" -xdev -mindepth 1 -print | sed -n '20001p'); ",
                        "[ -z \"$excess\" ] || exit 65; find \"$p\" -xdev -depth -mindepth 1 ",
                        "\\( -type l -o -type f -o ! -type d \\) -exec rm -f -- {{}} + -o -type d -exec rmdir -- {{}} +; ",
                        "rmdir -- \"$p\"; fi"
                    ),
                    sh_quote(&path)
                );
                emit_shell_action(&handle, &events, &cmd, t("删除失败", "Delete failed")).await;
                if tree_dirs.contains_key(&refresh) {
                    if let Ok(dirs) = shell_list_dirs_only(&handle, &refresh).await {
                        tree_dirs.insert(refresh.clone(), dirs);
                        emit_tree(&tree_dirs, &tree_expanded, &events);
                    }
                }
                emit_shell_dir(&handle, &events, &refresh).await;
            }
            SftpCommand::Rename { from, to } => {
                let refresh = parent_dir(&to);
                let cmd = format!("mv {} {}", sh_quote(&from), sh_quote(&to));
                emit_shell_action(&handle, &events, &cmd, t("重命名失败", "Rename failed")).await;
                emit_shell_dir(&handle, &events, &refresh).await;
            }
            SftpCommand::Chmod { path, mode } => {
                let refresh = parent_dir(&path);
                let cmd = format!("chmod {:o} {}", mode & 0o7777, sh_quote(&path));
                emit_shell_action(&handle, &events, &cmd, t("修改权限失败", "chmod failed")).await;
                emit_shell_dir(&handle, &events, &refresh).await;
            }
            SftpCommand::ReadText { remote, edit } => {
                let name = base_name(&remote);
                let (content, error) = match shell_read_text(&handle, &remote).await {
                    Ok(text) => (text, String::new()),
                    Err(e) => (String::new(), e),
                };
                let _ = events.send(SessionEvent::SftpFileText {
                    path: remote,
                    name,
                    content,
                    edit,
                    error,
                });
            }
            SftpCommand::WriteText { remote, content } => {
                let refresh = parent_dir(&remote);
                match shell_write_text(&handle, &remote, &content).await {
                    Ok(_) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("已保存", "Saved"),
                            base_name(&remote)
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("保存失败", "Save failed")
                        )));
                    }
                }
                emit_shell_dir(&handle, &events, &refresh).await;
            }
            SftpCommand::Download { remote, local_dir } => {
                match shell_download_file(&handle, &remote, &local_dir).await {
                    Ok(filename) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {}",
                            t("下载完成", "Downloaded"),
                            filename
                        )));
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("下载失败", "Download failed")
                        )));
                    }
                }
            }
            SftpCommand::OpenTemp { remote, edit } => {
                let tmp = std::env::temp_dir().join("probe-shell");
                let dir = tmp.to_string_lossy().to_string();
                let _ = tokio::fs::create_dir_all(&dir).await;
                match shell_download_file(&handle, &remote, &dir).await {
                    Ok(filename) => {
                        let local = format!("{}/{}", dir.trim_end_matches('/'), filename);
                        open_with_os(&local);
                        if edit {
                            let _ = events.send(SessionEvent::SftpStatus(t(
                                "SSH 文件浏览模式暂不支持外部编辑后自动回传",
                                "SSH file-browser mode does not auto-upload external edits yet",
                            ).into()));
                        }
                    }
                    Err(e) => {
                        let _ = events.send(SessionEvent::SftpStatus(format!(
                            "{}: {e}",
                            t("打开失败", "Open failed")
                        )));
                    }
                }
            }
            SftpCommand::Upload { local, remote_dir } => {
                let id = Uuid::new_v4().to_string();
                let cancel = Arc::new(AtomicBool::new(false));
                if let Ok(mut cancels) = transfer_cancels.lock() {
                    cancels.insert(id.clone(), cancel.clone());
                }
                let cancels_done = transfer_cancels.clone();
                let handle = handle.clone();
                let events = events.clone();
                let shutdown_task = shutdown.clone();
                let task = tokio::spawn(async move {
                    let name = base_name(&local);
                    let metadata = tokio::fs::symlink_metadata(&local).await;
                    let result = match metadata {
                        Ok(meta) if meta.file_type().is_symlink() => Err(anyhow!(t(
                            "只允许上传普通文件或文件夹，符号链接不会被跟随",
                            "Only regular files or folders can be uploaded; symbolic links are not followed",
                        ))),
                        Ok(meta) if meta.is_file() => {
                            shell_upload_regular_file(
                                &handle,
                                &local,
                                &remote_dir,
                                &name,
                                &id,
                                &events,
                                &cancel,
                                &shutdown_task,
                            )
                            .await
                        }
                        Ok(meta) if meta.is_dir() => {
                            emit_transfer(&events, &id, &name, true, 0, 0, 3, "");
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {}",
                                t("正在准备上传文件夹", "Preparing folder upload"),
                                name
                            )));
                            shell_upload_directory(
                                &handle,
                                &local,
                                &remote_dir,
                                &name,
                                &id,
                                &events,
                                &cancel,
                                &shutdown_task,
                            )
                            .await
                        }
                        Ok(_) => Err(anyhow!(t(
                            "只允许上传普通文件或文件夹",
                            "Only regular files or folders can be uploaded",
                        ))),
                        Err(err) => Err(anyhow!(err).context("read local upload source")),
                    };

                    match result {
                        Ok(ShellUploadResult::Completed(outcome)) => {
                            emit_transfer(
                                &events,
                                &id,
                                &name,
                                true,
                                outcome.transferred,
                                outcome.total,
                                1,
                                "",
                            );
                            // Refresh after success, then restore the useful final
                            // status so directory-loading text does not hide 100%.
                            emit_shell_dir(&handle, &events, &remote_dir).await;
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {} · 100% · {} {}",
                                t("上传完成", "Uploaded"),
                                name,
                                t("平均速率", "Average"),
                                format_transfer_rate(outcome.average_bps)
                            )));
                        }
                        Ok(ShellUploadResult::Cancelled(outcome)) => {
                            emit_transfer(
                                &events,
                                &id,
                                &name,
                                true,
                                outcome.transferred,
                                outcome.total,
                                4,
                                t("已取消", "Cancelled"),
                            );
                            let percent = transfer_percent(outcome.transferred, outcome.total, false);
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {} · {:.1}% · {} {}",
                                t("已取消", "Cancelled"),
                                name,
                                percent,
                                t("平均速率", "Average"),
                                format_transfer_rate(outcome.average_bps)
                            )));
                        }
                        Err(err) => {
                            let msg = err.to_string();
                            emit_transfer(&events, &id, &name, true, 0, 0, 2, &msg);
                            let _ = events.send(SessionEvent::SftpStatus(format!(
                                "{}: {msg}",
                                t("上传失败", "Upload failed")
                            )));
                        }
                    }
                    if let Ok(mut cancels) = cancels_done.lock() {
                        cancels.remove(&id);
                    }
                });
                track_task(&tasks, task);
            }
            SftpCommand::CancelTransfer(id) => {
                if let Ok(cancels) = transfer_cancels.lock() {
                    if let Some(cancel) = cancels.get(&id) {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
            SftpCommand::DownloadArchive { .. } => {
                let _ = events.send(SessionEvent::SftpStatus(t(
                    "SSH 兼容模式暂不支持批量打包下载",
                    "SSH-compatible mode does not support archive downloads yet",
                ).into()));
            }
        }
    }

    let _ = handle
        .disconnect(Disconnect::ByApplication, "bye", "")
        .await;
    Ok(())
}

async fn shell_pwd(handle: &client::Handle<SftpClientHandler>) -> Result<String> {
    let (code, out, err) = exec_capture_browser(handle, "pwd").await?;
    if code != 0 {
        return Err(anyhow!(String::from_utf8_lossy(&err).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

async fn emit_shell_dir(
    handle: &client::Handle<SftpClientHandler>,
    events: &UnboundedSender<SessionEvent>,
    path: &str,
) {
    let _ = events.send(SessionEvent::SftpStatus(format!("{} {}...", t("加载", "Loading"), path)));
    match shell_list_dir(handle, path).await {
        Ok(entries) => {
            let _ = events.send(SessionEvent::SftpEntries {
                path: path.to_string(),
                entries,
            });
            let _ = events.send(SessionEvent::SftpStatus(format!(
                "{} · {}",
                path,
                t("SSH 文件浏览", "SSH browser")
            )));
        }
        Err(e) => {
            let _ = events.send(SessionEvent::SftpError(list_error_msg(path, &e)));
        }
    }
}

async fn emit_shell_action(
    handle: &client::Handle<SftpClientHandler>,
    events: &UnboundedSender<SessionEvent>,
    cmd: &str,
    fail_title: &str,
) {
    match exec_capture_browser(handle, cmd).await {
        Ok((0, _, _)) => {}
        Ok((_, _, err)) => {
            let msg = String::from_utf8_lossy(&err).trim().to_string();
            let _ = events.send(SessionEvent::SftpStatus(format!("{fail_title}: {msg}")));
        }
        Err(e) => {
            let _ = events.send(SessionEvent::SftpStatus(format!("{fail_title}: {e}")));
        }
    }
}

async fn shell_list_dirs_only(
    handle: &client::Handle<SftpClientHandler>,
    path: &str,
) -> Result<Vec<(String, String)>> {
    Ok(shell_list_dir(handle, path)
        .await?
        .into_iter()
        .filter(|e| e.is_dir)
        .map(|e| (e.name, e.full_path))
        .collect())
}

async fn shell_list_dir(
    handle: &client::Handle<SftpClientHandler>,
    path: &str,
) -> Result<Vec<RemoteEntry>> {
    let cmd = format!(
        concat!(
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; ",
            "p={path}; cd \"$p\" 2>/dev/null || exit 2; ",
            "for f in .[!.]* ..?* *; do ",
            "[ -e \"$f\" ] || continue; ",
            "[ \"$f\" = . ] && continue; [ \"$f\" = .. ] && continue; ",
            "if [ -L \"$f\" ]; then ",
            "  if [ ! -e \"$f\" ]; then typ=dead; ",
            "  elif [ -d \"$f\" ]; then typ=ld; else typ=lf; fi; ",
            "elif [ -d \"$f\" ]; then typ=d; else typ=f; fi; ",
            "sz=$(stat -c %s \"$f\" 2>/dev/null || wc -c < \"$f\" 2>/dev/null || echo 0); ",
            "mt=$(stat -c %Y \"$f\" 2>/dev/null || echo 0); ",
            "md=$(stat -c %a \"$f\" 2>/dev/null || echo 0); ",
            "printf '%s\\t%s\\t%s\\t%s\\t%s\\n' \"$typ\" \"$sz\" \"$mt\" \"$md\" \"$f\"; ",
            "done"
        ),
        path = sh_quote(path)
    );
    let (code, out, err) = exec_capture_browser(handle, &cmd).await?;
    if code != 0 {
        let msg = String::from_utf8_lossy(&err).trim().to_string();
        return Err(anyhow!(if msg.is_empty() {
            format!("cannot list {path}")
        } else {
            msg
        }));
    }

    let mut entries = Vec::new();
    for line in String::from_utf8_lossy(&out).lines() {
        let mut parts = line.splitn(5, '\t');
        let typ = parts.next().unwrap_or("");
        let size = parts.next().unwrap_or("0").trim().parse::<u64>().unwrap_or(0);
        let modified = parts.next().unwrap_or("0").trim().parse::<u32>().unwrap_or(0);
        let mode_txt = parts.next().unwrap_or("0").trim();
        let name = parts.next().unwrap_or("").to_string();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let full_path = if path == "/" {
            format!("/{name}")
        } else {
            format!("{}/{}", path.trim_end_matches('/'), name)
        };
        let mode = u32::from_str_radix(mode_txt, 8).unwrap_or(0);
        let (is_dir, kind) = match typ {
            "d" => (true, "dir"),
            "ld" => (true, "symlink-dir"),
            "lf" => (false, "symlink-file"),
            "dead" => (false, "dead-link"),
            _ => (false, "file"),
        };
        entries.push(RemoteEntry {
            name,
            full_path,
            is_dir,
            kind: kind.to_string(),
            size,
            modified,
            mode,
        });
    }

    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });
    Ok(entries)
}

async fn shell_search_dir_impl(
    handle: &client::Handle<SftpClientHandler>,
    root: &str,
    query: &str,
    max_results: usize,
    max_dirs: usize,
    cancel: CancelFlag,
    shutdown: CancelFlag,
    emitter: &mut SearchEmitter<'_>,
) -> Result<()> {
    let q = query.trim().to_lowercase();
    let base = normalise_remote_dir(root);
    let mut stack = vec![base.clone()];
    let mut seen: HashSet<String> = HashSet::new();

    while let Some(dir) = stack.pop() {
        if is_cancelled(&cancel, &shutdown) {
            break;
        }
        let key = normalise_remote_dir(&dir);
        if !seen.insert(key) {
            continue;
        }
        emitter.scan_dir();
        if emitter.scanned > max_dirs || emitter.found >= max_results {
            break;
        }

        let entries = match shell_list_dir(handle, &dir).await {
            Ok(v) => v,
            Err(_) => {
                // A denied branch should not cancel a search. This matters on
                // routers where /proc, /sys, or vendor paths may reject exec/stat.
                continue;
            }
        };

        for mut entry in entries {
            let rel = entry
                .full_path
                .strip_prefix(base.trim_end_matches('/'))
                .unwrap_or(&entry.full_path)
                .trim_start_matches('/')
                .to_string();
            let hay = format!("{} {}", entry.name.to_lowercase(), entry.full_path.to_lowercase());
            if q.is_empty() || hay.contains(&q) {
                if !rel.is_empty() {
                    entry.name = rel;
                }
                emitter.push(entry.clone());
                if emitter.found >= max_results {
                    break;
                }
            }
            if entry.is_dir && entry.kind != "symlink-dir" {
                let name = entry.name.rsplit('/').next().unwrap_or(&entry.name);
                let next = normalise_remote_dir(&entry.full_path);
                if name != "." && name != ".." && !seen.contains(&next) {
                    stack.push(next);
                }
            }
        }
    }

    Ok(())
}

async fn shell_read_text(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
) -> std::result::Result<String, String> {
    use base64::Engine as _;
    let cmd = format!(
        "test -f {0} || exit 3; sz=$(stat -c %s {0} 2>/dev/null || echo 0); [ \"$sz\" -le 2097152 ] || exit 4; base64 {0}",
        sh_quote(remote)
    );
    let (code, out, err) = exec_capture_browser(handle, &cmd)
        .await
        .map_err(|e| e.to_string())?;
    match code {
        0 => {}
        3 => return Err(t("不是普通文件,无法打开", "Not a regular file; cannot open").into()),
        4 => return Err(t("文件过大,无法在内置编辑器中打开(上限 2 MB),请下载查看", "Too large for the built-in editor (2 MB limit); download it instead").into()),
        _ => return Err(String::from_utf8_lossy(&err).trim().to_string()),
    }
    let b64 = String::from_utf8_lossy(&out).replace('\r', "").replace('\n', "");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .map_err(|e| format!("base64 decode: {e}"))?;
    if bytes
        .iter()
        .any(|&b| (b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r') || b == 0x7f)
    {
        return Err(t("包含控制字符(疑似二进制),无法以文本打开,请下载查看", "Contains control characters (likely binary); download it instead").into());
    }
    String::from_utf8(bytes)
        .map_err(|_| t("非 UTF-8 文本,无法打开", "Not UTF-8 text; cannot open").into())
}

async fn shell_write_text(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    content: &str,
) -> Result<()> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    let cmd = format!(
        "printf %s {} | base64 -d > {}",
        sh_quote(&encoded),
        sh_quote(remote)
    );
    let (code, _out, err) = exec_capture_browser(handle, &cmd).await?;
    if code == 0 {
        Ok(())
    } else {
        Err(anyhow!(String::from_utf8_lossy(&err).trim().to_string()))
    }
}

async fn shell_download_file(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    local_dir: &str,
) -> Result<String> {
    use base64::Engine as _;
    let cmd = format!("test -f {0} || exit 3; base64 {0}", sh_quote(remote));
    let (code, out, err) = exec_capture_browser(handle, &cmd).await?;
    if code != 0 {
        let msg = String::from_utf8_lossy(&err).trim().to_string();
        return Err(anyhow!(if code == 3 {
            t("当前 SSH 文件浏览模式只支持下载普通文件", "SSH file-browser mode can only download regular files").to_string()
        } else if msg.is_empty() {
            "download failed".to_string()
        } else {
            msg
        }));
    }
    let b64 = String::from_utf8_lossy(&out).replace('\r', "").replace('\n', "");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .context("base64 decode remote file")?;
    let filename = sanitize_filename(&base_name(remote));
    let local_path = std::path::Path::new(local_dir).join(&filename);
    tokio::fs::write(&local_path, bytes)
        .await
        .with_context(|| format!("write local {}", local_path.display()))?;
    Ok(filename)
}



#[derive(Clone, Copy)]
struct ShellUploadOutcome {
    transferred: u64,
    total: u64,
    average_bps: f64,
}

enum ShellUploadResult {
    Completed(ShellUploadOutcome),
    Cancelled(ShellUploadOutcome),
}

fn format_transfer_rate(bytes_per_second: f64) -> String {
    let value = if bytes_per_second.is_finite() && bytes_per_second > 0.0 {
        bytes_per_second.round().min(u64::MAX as f64) as u64
    } else {
        0
    };
    format!("{}/s", format_size(value))
}

fn transfer_percent(transferred: u64, total: u64, confirmed: bool) -> f64 {
    if confirmed {
        100.0
    } else if total == 0 {
        0.0
    } else {
        ((transferred as f64 / total as f64) * 100.0).clamp(0.0, 99.9)
    }
}

fn emit_shell_upload_progress(
    events: &UnboundedSender<SessionEvent>,
    id: &str,
    name: &str,
    transferred: u64,
    total: u64,
    bytes_per_second: f64,
) {
    emit_transfer(events, id, name, true, transferred, total, 0, "");
    let _ = events.send(SessionEvent::SftpStatus(format!(
        "{}: {} · {:.1}% · {} {}",
        t("上传中", "Uploading"),
        name,
        transfer_percent(transferred, total, false),
        t("实时速率", "Live"),
        format_transfer_rate(bytes_per_second)
    )));
}

struct ShellUploadReader {
    inner: tokio::fs::File,
    cancel: CancelFlag,
    shutdown: CancelFlag,
    events: UnboundedSender<SessionEvent>,
    id: String,
    name: String,
    total: u64,
    transferred: Arc<AtomicU64>,
    last_emitted_bytes: u64,
    last_emit: Instant,
}

impl tokio::io::AsyncRead for ShellUploadReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if is_cancelled(&this.cancel, &this.shutdown) {
            return std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "upload cancelled",
            )));
        }
        let before = buf.filled().len();
        match tokio::io::AsyncRead::poll_read(
            std::pin::Pin::new(&mut this.inner),
            cx,
            buf,
        ) {
            std::task::Poll::Ready(Ok(())) => {
                let read = buf.filled().len().saturating_sub(before) as u64;
                if read > 0 {
                    let done = this.transferred.fetch_add(read, Ordering::Relaxed) + read;
                    let elapsed = this.last_emit.elapsed();
                    if elapsed >= SSH_UPLOAD_PROGRESS_INTERVAL {
                        let delta = done.saturating_sub(this.last_emitted_bytes);
                        let rate = if elapsed.as_secs_f64() > 0.0 {
                            delta as f64 / elapsed.as_secs_f64()
                        } else {
                            0.0
                        };
                        emit_shell_upload_progress(
                            &this.events,
                            &this.id,
                            &this.name,
                            done,
                            this.total,
                            rate,
                        );
                        this.last_emitted_bytes = done;
                        this.last_emit = Instant::now();
                    }
                }
                std::task::Poll::Ready(Ok(()))
            }
            other => other,
        }
    }
}

fn shell_upload_error(code: u32, stderr: &[u8]) -> anyhow::Error {
    let detail = String::from_utf8_lossy(stderr).trim().to_string();
    let friendly = match code {
        21 => t("远端目标目录不存在或不可访问", "Remote target directory is unavailable"),
        22 => t("无法创建远端临时文件", "Cannot create the remote temporary file"),
        23 => t("远端写入失败，可能空间不足", "Remote write failed; storage may be full"),
        24 => t("无法校验远端文件大小", "Cannot verify the remote file size"),
        25 => t("远端文件大小校验失败", "Remote file size verification failed"),
        26 => t("无法原子替换目标文件", "Cannot atomically replace the target file"),
        27 => t("目标路径是文件夹，不能用文件覆盖", "The target path is a folder"),
        31 => t("远端缺少 tar，无法上传文件夹", "The remote host has no tar command for folder upload"),
        32 => t("无法创建远端临时目录", "Cannot create the remote temporary directory"),
        33 => t("远端解包失败，可能空间不足", "Remote extraction failed; storage may be full"),
        34 => t("文件夹包结构校验失败", "Folder archive structure verification failed"),
        35 => t("目标路径不是文件夹或不可写", "The target path is not a writable folder"),
        36 => t("合并文件夹内容失败", "Failed to merge the uploaded folder"),
        37 => t("远端文件夹收尾失败", "Failed to finalize the remote folder"),
        _ => t("远端上传命令执行失败", "Remote upload command failed"),
    };
    if detail.is_empty() {
        anyhow!("{friendly} (exit {code})")
    } else {
        anyhow!("{friendly} (exit {code}): {detail}")
    }
}

async fn shell_stream_upload(
    handle: &client::Handle<SftpClientHandler>,
    local_stream: &str,
    remote_command: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &CancelFlag,
    shutdown: &CancelFlag,
    total: u64,
) -> Result<ShellUploadResult> {
    let local_file = tokio::fs::File::open(local_stream)
        .await
        .with_context(|| format!("open local upload stream {local_stream}"))?;

    // Embedded SSH servers can briefly reject a new exec channel while another
    // one is closing. Retry only before any bytes are sent, so data is never
    // duplicated and directory browsing remains independent.
    let mut channel = match handle.channel_open_session().await {
        Ok(channel) => channel,
        Err(first) => {
            tracing::debug!("SSH upload channel open failed, retrying: {first}");
            tokio::time::sleep(Duration::from_millis(260)).await;
            handle
                .channel_open_session()
                .await
                .context("open SSH upload channel after retry")?
        }
    };
    if let Err(first) = channel.exec(true, remote_command.as_bytes()).await {
        tracing::debug!("SSH upload exec request failed, retrying: {first}");
        let _ = channel.close().await;
        tokio::time::sleep(Duration::from_millis(260)).await;
        channel = handle
            .channel_open_session()
            .await
            .context("open SSH upload retry channel")?;
        channel
            .exec(true, remote_command.as_bytes())
            .await
            .context("start SSH upload command after retry")?;
    }

    emit_shell_upload_progress(events, id, name, 0, total, 0.0);
    let transferred = Arc::new(AtomicU64::new(0));
    let started = Instant::now();
    let reader = ShellUploadReader {
        inner: local_file,
        cancel: cancel.clone(),
        shutdown: shutdown.clone(),
        events: events.clone(),
        id: id.to_string(),
        name: name.to_string(),
        total,
        transferred: transferred.clone(),
        last_emitted_bytes: 0,
        last_emit: Instant::now(),
    };

    let send_result = channel.data(reader).await;
    let sent = transferred.load(Ordering::Relaxed);
    let send_elapsed = started.elapsed();
    let average_bps = if send_elapsed.as_secs_f64() > 0.0 {
        sent as f64 / send_elapsed.as_secs_f64()
    } else {
        0.0
    };
    let cancelled = is_cancelled(cancel, shutdown);

    if !cancelled && send_result.is_ok() {
        emit_transfer(events, id, name, true, sent, total, 0, "");
        let _ = events.send(SessionEvent::SftpStatus(format!(
            "{}: {} · 100% · {} {}",
            t("正在校验", "Verifying"),
            name,
            t("平均速率", "Average"),
            format_transfer_rate(average_bps)
        )));
    }

    let _ = channel.eof().await;
    let wait_timeout = if cancelled {
        Duration::from_secs(5)
    } else {
        SSH_UPLOAD_FINALIZE_TIMEOUT
    };
    let wait_result = tokio::time::timeout(wait_timeout, async {
        let mut status: Option<u32> = None;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(msg) = channel.wait().await {
            match msg {
                russh::ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
                russh::ChannelMsg::ExtendedData { data, ext: _ } => {
                    stderr.extend_from_slice(&data)
                }
                russh::ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                russh::ChannelMsg::Close => break,
                _ => {}
            }
        }
        (status, stdout, stderr)
    })
    .await;
    let _ = channel.close().await;

    let outcome = ShellUploadOutcome {
        transferred: sent,
        total,
        average_bps,
    };
    if cancelled {
        return Ok(ShellUploadResult::Cancelled(outcome));
    }
    if let Err(err) = send_result {
        return Err(anyhow!(err).context("send SSH upload data"));
    }
    let (status, stdout, stderr) = wait_result.map_err(|_| {
        anyhow!(
            "{}",
            t(
                "远端上传收尾超时，临时文件已请求清理",
                "Remote upload finalization timed out; temporary cleanup was requested",
            )
        )
    })?;
    let ok_marker = format!("OK\t{total}");
    let confirmed = String::from_utf8_lossy(&stdout)
        .lines()
        .any(|line| line.trim() == ok_marker);
    let code = status.unwrap_or(if confirmed { 0 } else { u32::MAX });
    if code != 0 {
        return Err(shell_upload_error(code, &stderr));
    }
    if !confirmed {
        return Err(anyhow!(t(
            "远端未返回完整性确认，已按失败处理",
            "The remote host did not confirm integrity; treating the upload as failed",
        )));
    }
    Ok(ShellUploadResult::Completed(outcome))
}

fn checked_shell_upload_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        Err(anyhow!(t(
            "本地文件名无效，无法上传",
            "The local file name is invalid for upload",
        )))
    } else {
        Ok(())
    }
}

fn remote_child_path(parent: &str, name: &str) -> String {
    let parent = normalise_remote_dir(parent);
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
    }
}

async fn shell_upload_regular_file(
    handle: &client::Handle<SftpClientHandler>,
    local: &str,
    remote_dir: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &CancelFlag,
    shutdown: &CancelFlag,
) -> Result<ShellUploadResult> {
    checked_shell_upload_name(name)?;
    let total = tokio::fs::metadata(local)
        .await
        .with_context(|| format!("stat local upload file {local}"))?
        .len();
    let remote_dir = normalise_remote_dir(remote_dir);
    let final_path = remote_child_path(&remote_dir, name);
    let temp_path = remote_child_path(
        &remote_dir,
        &format!(".probe-shell-upload-{}.part", Uuid::new_v4()),
    );
    let command = format!(
        concat!(
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; ",
            "dir={dir}; dst={dst}; tmp={tmp}; expected={expected}; ",
            "[ -d \"$dir\" ] || exit 21; [ ! -d \"$dst\" ] || exit 27; ",
            "trap 'rm -f \"$tmp\"' HUP INT TERM EXIT; umask 077; ",
            ": > \"$tmp\" || exit 22; cat > \"$tmp\" || exit 23; ",
            "got=$(wc -c < \"$tmp\" 2>/dev/null) || exit 24; ",
            "[ \"$got\" = \"$expected\" ] || exit 25; ",
            "mv -f \"$tmp\" \"$dst\" || exit 26; ",
            "trap - HUP INT TERM EXIT; printf 'OK\\t%s\\n' \"$got\""
        ),
        dir = sh_quote(&remote_dir),
        dst = sh_quote(&final_path),
        tmp = sh_quote(&temp_path),
        expected = total,
    );
    shell_stream_upload(
        handle,
        local,
        &command,
        name,
        id,
        events,
        cancel,
        shutdown,
        total,
    )
    .await
}

fn build_folder_tar(
    local_root: &Path,
    archive_path: &Path,
    cancel: &CancelFlag,
) -> Result<u64> {
    let root_name = local_root
        .file_name()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("folder has no file name"))?
        .to_os_string();
    let archive_file = std::fs::File::create(archive_path)
        .with_context(|| format!("create temporary archive {}", archive_path.display()))?;
    let mut builder = tar::Builder::new(archive_file);
    builder.follow_symlinks(false);
    let mut stack = vec![(local_root.to_path_buf(), Path::new(&root_name).to_path_buf())];
    let mut nodes = 0usize;

    while let Some((local_dir, archive_dir)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("upload cancelled"));
        }
        nodes = nodes.saturating_add(1);
        if nodes > SFTP_MAX_RECURSIVE_NODES {
            return Err(anyhow!("folder upload exceeded node limit"));
        }
        builder
            .append_dir(&archive_dir, &local_dir)
            .with_context(|| format!("archive directory {}", local_dir.display()))?;
        for entry in std::fs::read_dir(&local_dir)
            .with_context(|| format!("read local directory {}", local_dir.display()))?
        {
            let entry = entry.context("read local directory entry")?;
            let metadata = std::fs::symlink_metadata(entry.path())
                .with_context(|| format!("stat local entry {}", entry.path().display()))?;
            nodes = nodes.saturating_add(1);
            if nodes > SFTP_MAX_RECURSIVE_NODES {
                return Err(anyhow!("folder upload exceeded node limit"));
            }
            if metadata.file_type().is_symlink() {
                continue;
            }
            let archive_child = archive_dir.join(entry.file_name());
            if metadata.is_dir() {
                stack.push((entry.path(), archive_child));
            } else if metadata.is_file() {
                let mut file = std::fs::File::open(entry.path())
                    .with_context(|| format!("open local file {}", entry.path().display()))?;
                builder
                    .append_file(&archive_child, &mut file)
                    .with_context(|| format!("archive file {}", entry.path().display()))?;
            }
        }
    }
    builder.finish().context("finish temporary folder archive")?;
    let archive_file = builder.into_inner().context("close temporary folder archive")?;
    archive_file.sync_all().context("flush temporary folder archive")?;
    Ok(archive_file.metadata().context("stat temporary folder archive")?.len())
}

async fn shell_upload_directory(
    handle: &client::Handle<SftpClientHandler>,
    local: &str,
    remote_dir: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &CancelFlag,
    shutdown: &CancelFlag,
) -> Result<ShellUploadResult> {
    checked_shell_upload_name(name)?;
    let archive_path = std::env::temp_dir().join(format!(
        "probe-shell-folder-upload-{}.tar",
        Uuid::new_v4()
    ));
    let local_root = Path::new(local).to_path_buf();
    let archive_for_build = archive_path.clone();
    let cancel_for_build = cancel.clone();
    let build_result = tokio::task::spawn_blocking(move || {
        build_folder_tar(&local_root, &archive_for_build, &cancel_for_build)
    })
    .await
    .context("join folder archive task")?;

    let total = match build_result {
        Ok(total) => total,
        Err(err) if is_cancelled(cancel, shutdown) => {
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Ok(ShellUploadResult::Cancelled(ShellUploadOutcome {
                transferred: 0,
                total: 0,
                average_bps: 0.0,
            }));
        }
        Err(err) => {
            let _ = tokio::fs::remove_file(&archive_path).await;
            return Err(err);
        }
    };
    if is_cancelled(cancel, shutdown) {
        let _ = tokio::fs::remove_file(&archive_path).await;
        return Ok(ShellUploadResult::Cancelled(ShellUploadOutcome {
            transferred: 0,
            total,
            average_bps: 0.0,
        }));
    }

    let remote_dir = normalise_remote_dir(remote_dir);
    let final_path = remote_child_path(&remote_dir, name);
    let temp_root = remote_child_path(
        &remote_dir,
        &format!(".probe-shell-folder-{}", Uuid::new_v4()),
    );
    let extracted_root = remote_child_path(&temp_root, name);
    let command = format!(
        concat!(
            "PATH=/usr/bin:/bin:/usr/sbin:/sbin; export PATH; ",
            "dir={dir}; dst={dst}; tmp={tmp}; src={src}; expected={expected}; ",
            "command -v tar >/dev/null 2>&1 || exit 31; ",
            "[ -d \"$dir\" ] || exit 21; ",
            "trap 'rm -rf \"$tmp\"' HUP INT TERM EXIT; umask 077; ",
            "mkdir \"$tmp\" || exit 32; tar -x -f - -C \"$tmp\" || exit 33; ",
            "[ -d \"$src\" ] || exit 34; ",
            "if [ -e \"$dst\" ]; then ",
            "  [ -d \"$dst\" ] || exit 35; ",
            "  cp -R \"$src/.\" \"$dst/\" || exit 36; ",
            "  rm -rf \"$tmp\" || exit 37; ",
            "else ",
            "  mv \"$src\" \"$dst\" || exit 37; ",
            "  rmdir \"$tmp\" 2>/dev/null || rm -rf \"$tmp\"; ",
            "fi; trap - HUP INT TERM EXIT; printf 'OK\\t%s\\n' \"$expected\""
        ),
        dir = sh_quote(&remote_dir),
        dst = sh_quote(&final_path),
        tmp = sh_quote(&temp_root),
        src = sh_quote(&extracted_root),
        expected = total,
    );
    let archive_string = archive_path.to_string_lossy().to_string();
    let result = shell_stream_upload(
        handle,
        &archive_string,
        &command,
        name,
        id,
        events,
        cancel,
        shutdown,
        total,
    )
    .await;
    let _ = tokio::fs::remove_file(&archive_path).await;
    result
}


/// Read a remote file as UTF-8 text for the built-in editor, rejecting files
/// that are too large, binary, or not valid UTF-8 (#70). Returns the text on
/// success or a human-readable error message on failure.
async fn read_text_guarded(sftp: &SftpSession, remote: &str) -> std::result::Result<String, String> {
    use tokio::io::AsyncReadExt;
    const MAX_EDIT_BYTES: u64 = 2 * 1024 * 1024; // 2 MiB
    let size = sftp
        .metadata(remote)
        .await
        .ok()
        .and_then(|m| m.size)
        .unwrap_or(0);
    if size > MAX_EDIT_BYTES {
        return Err(t(
            "文件过大,无法在内置编辑器中打开(上限 2 MB),请下载查看",
            "Too large for the built-in editor (2 MB limit); download it instead",
        )
        .into());
    }
    let mut f = sftp
        .open(remote)
        .await
        .map_err(|e| format!("{}: {e}", t("打开失败", "Open failed")))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)
        .await
        .map_err(|e| format!("{}: {e}", t("读取失败", "Read failed")))?;
    // Control characters (beyond tab/newline/CR) have no glyph — they render as
    // tofu boxes — and round-tripping them through the editor risks corrupting
    // the file (e.g. .viminfo). Treat such files as binary (#70).
    if bytes
        .iter()
        .any(|&b| (b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r') || b == 0x7f)
    {
        return Err(t(
            "包含控制字符(疑似二进制),无法以文本打开,请下载查看",
            "Contains control characters (likely binary); download it instead",
        )
        .into());
    }
    String::from_utf8(bytes)
        .map_err(|_| t("非 UTF-8 文本,无法打开", "Not UTF-8 text; cannot open").into())
}

/// Overwrite a remote file with the given text (CREATE | WRITE | TRUNCATE).
async fn write_text_file(sftp: &SftpSession, remote: &str, content: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut f = sftp
        .create(remote)
        .await
        .with_context(|| format!("create remote {remote}"))?;
    f.write_all(content.as_bytes())
        .await
        .context("write remote file")?;
    f.flush().await.context("flush remote file")?;
    let _ = f.shutdown().await;
    Ok(())
}

/// File name component of a path.  Handles both remote (`/`) and local Windows
/// (`\`) separators, so uploading `C:\…\frp.tar.gz` yields `frp.tar.gz` rather
/// than the whole path (which previously became the remote file name).
fn base_name(path: &str) -> String {
    let sep = |c: char| c == '/' || c == '\\';
    path.trim_end_matches(sep)
        .rsplit(sep)
        .next()
        .unwrap_or(path)
        .to_string()
}

/// Single-quote a string for safe interpolation into a remote `/bin/sh`
/// command. Remote names come from the *server's* listing and are therefore
/// untrusted — without quoting, a crafted name like `; rm -rf ~` would run.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Run a one-shot command on the remote over its own exec channel and return
/// the exit status. Stdout/stderr are drained and discarded.
async fn exec_remote(handle: &client::Handle<SftpClientHandler>, cmd: &str) -> Result<u32> {
    let mut ch = handle
        .channel_open_session()
        .await
        .context("open exec channel")?;
    ch.exec(true, cmd.as_bytes())
        .await
        .context("exec remote command")?;
    let mut status = 0u32;
    while let Some(msg) = ch.wait().await {
        match msg {
            russh::ChannelMsg::ExitStatus { exit_status } => status = exit_status,
            russh::ChannelMsg::Close => break,
            _ => {}
        }
    }
    Ok(status)
}

/// Parent directory of a remote path ("/a/b" → "/a", "/a" → "/").
fn parent_dir(path: &str) -> String {
    let p = path.trim_end_matches('/');
    match p.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => p[..i].to_string(),
    }
}

/// Open a local file with the OS default application.
///
/// Security: we must NOT route the path through a shell.  The previous
/// `cmd /C start "" <path>` let cmd.exe re-parse the path, so a remote file name
/// containing shell metacharacters (`&` `|` `>` `<` `^` …) — e.g. `foo&calc.exe`
/// — could inject and run arbitrary commands when the user opened it.  We call
/// `ShellExecuteW` directly instead: it treats the path as one opaque string, so
/// no shell parsing happens.  (`xdg-open` on Unix already takes a single argv
/// argument and never invokes a shell.)
#[cfg(windows)]
fn open_with_os(path: &str) {
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
    let to_wide = |s: &str| -> Vec<u16> {
        OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    };
    let op = to_wide("open");
    let file = to_wide(path);
    unsafe {
        ShellExecuteW(
            0,
            op.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // SW_SHOWNORMAL
        );
    }
}

#[cfg(not(windows))]
fn open_with_os(path: &str) {
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// Make a remote-supplied file name safe to use as a *local* file name (for
/// both downloads and temp files): drops path separators (defence-in-depth
/// against traversal), replaces characters invalid on Windows or special to
/// shells with `_`, trims surrounding whitespace and Windows' trailing dots,
/// and neutralises reserved device names (CON, NUL, COM1…).  Normal names
/// (letters, digits, `.`, `-`, `_`, Unicode) pass through; Unix dotfiles keep
/// their leading dot.  Falls back to `file` when nothing usable remains.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | '&' | '^' | '%' | '!'
            | '`' | '$' | '\'' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // Drop leading whitespace and trailing dots/spaces (Windows strips the
    // latter silently). A leading dot is preserved so `.bashrc` survives.
    let trimmed = cleaned.trim_start_matches(' ').trim_end_matches([' ', '.']);
    if trimmed.is_empty() {
        return "file".to_string();
    }
    // Windows reserved device names are reserved case-insensitively and even
    // with an extension ("CON.txt" still opens the console). A download named
    // after one could read/write a device instead of a file, so prefix `_`.
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let reserved = matches!(
        stem.to_ascii_uppercase().as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    );
    if reserved {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Watch a downloaded temp file and re-upload it to the remote whenever it
/// changes on disk (the "edit" flow).  Re-upload is routed back through the
/// worker's own command channel.  Stops when the channel closes or after a
/// generous idle window.
fn spawn_edit_watcher(
    self_tx: UnboundedSender<SftpCommand>,
    local: String,
    remote: String,
    filename: String,
    events: UnboundedSender<SessionEvent>,
    shutdown: CancelFlag,
) -> JoinHandle<()> {
    let remote_dir = parent_dir(&remote);
    tokio::spawn(async move {
        let mtime = |p: &str| std::fs::metadata(p).ok().and_then(|m| m.modified().ok());
        let mut last = mtime(&local);
        // ~40 min of 2s polls; also exits early once the worker is gone.
        for _ in 0..1200 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if shutdown.load(Ordering::Relaxed) || self_tx.is_closed() {
                break;
            }
            let cur = mtime(&local);
            if cur.is_some() && cur != last {
                last = cur;
                let _ = self_tx.send(SftpCommand::Upload {
                    local: local.clone(),
                    remote_dir: remote_dir.clone(),
                });
                let _ = events.send(SessionEvent::SftpStatus(format!(
                    "{}: {}",
                    t("已上传修改", "Re-uploaded changes"),
                    filename
                )));
            }
        }
    })
}
// ---------------------------------------------------------------------------
// SFTP helpers
// ---------------------------------------------------------------------------

/// A friendlier message for a failed directory listing, calling out the common
/// permission-denied case explicitly rather than dumping the raw error (#112).
fn list_error_msg(path: &str, e: &impl std::fmt::Display) -> String {
    let raw = e.to_string();
    let low = raw.to_lowercase();
    let code = if low.contains("permission") || low.contains("denied") {
        "Permission Denied"
    } else if low.contains("timeout") || low.contains("timed out") {
        "Connection Timeout"
    } else if low.contains("no such") || low.contains("not found") {
        "Not Found"
    } else if low.contains("open exec channel") || low.contains("channel open") || low.contains("administratively prohibited") {
        "Exec Channel Refused"
    } else if low.contains("connection") || low.contains("reset") || low.contains("closed") {
        "Connection Closed"
    } else {
        "Open Failed"
    };
    format!("{} {} [{}]: {}", t("无法访问", "Cannot open"), path, code, raw)
}

fn normalise_remote_dir(path: &str) -> String {
    let p = path.trim();
    if p.is_empty() || p == "." {
        "/".to_string()
    } else if p == ".." {
        "..".to_string()
    } else if p == "/" {
        "/".to_string()
    } else {
        let mut out = p.replace('\\', "/");
        while out.ends_with('/') && out.len() > 1 {
            out.pop();
        }
        if out.starts_with('/') { out } else { format!("/{out}") }
    }
}

fn is_forbidden_remote_path(path: &str) -> bool {
    let p = path.trim().replace('\\', "/");
    if p.is_empty() || p.split('/').any(|part| part == "..") {
        return true;
    }
    !p.split('/').any(|part| !part.is_empty() && part != ".")
}

fn mode_is_dir(permissions: Option<u32>) -> bool {
    (permissions.unwrap_or(0) & 0o170_000) == 0o040_000
}

async fn remote_lstat_is_dir(sftp: &SftpSession, path: &str) -> Result<bool> {
    let meta = sftp
        .symlink_metadata(path)
        .await
        .with_context(|| format!("lstat {path}"))?;
    Ok(mode_is_dir(meta.permissions))
}

fn split_search_roots(root: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in root.split(';') {
        let p = normalise_remote_dir(raw);
        if p.is_empty() || p == ".." {
            continue;
        }
        if out.iter().any(|existing| remote_path_same_or_child(&p, existing)) {
            continue;
        }
        out.retain(|existing| !remote_path_same_or_child(existing, &p));
        out.push(p);
    }
    if out.is_empty() {
        out.push("/".to_string());
    }
    out
}

fn remote_path_same_or_child(path: &str, parent: &str) -> bool {
    let path = normalise_remote_dir(path);
    let parent = normalise_remote_dir(parent);
    parent == "/" || path == parent || path.starts_with(&(parent + "/"))
}

fn is_cancelled(cancel: &CancelFlag, shutdown: &CancelFlag) -> bool {
    cancel.load(Ordering::Relaxed) || shutdown.load(Ordering::Relaxed)
}

struct SearchEmitter<'a> {
    events: &'a UnboundedSender<SessionEvent>,
    search_id: String,
    root: String,
    query: String,
    pending: Vec<RemoteEntry>,
    found: usize,
    scanned: usize,
    started: Instant,
    last_batch: Instant,
    last_status: Instant,
}

impl<'a> SearchEmitter<'a> {
    fn new(
        events: &'a UnboundedSender<SessionEvent>,
        search_id: String,
        root: String,
        query: String,
    ) -> Self {
        let now = Instant::now();
        Self {
            events,
            search_id,
            root,
            query,
            pending: Vec::with_capacity(25),
            found: 0,
            scanned: 0,
            started: now,
            last_batch: now,
            last_status: now,
        }
    }

    fn status(&self, state: SftpSearchState) {
        let _ = self.events.send(SessionEvent::SftpSearchStatus {
            search_id: self.search_id.clone(),
            root: self.root.clone(),
            query: self.query.clone(),
            state,
            found: self.found,
            scanned: self.scanned,
            elapsed_ms: self.started.elapsed().as_millis(),
        });
    }

    fn scan_dir(&mut self) {
        self.scanned = self.scanned.saturating_add(1);
        if self.scanned == 1 || self.last_status.elapsed() >= Duration::from_millis(220) {
            self.status(SftpSearchState::Progress);
            self.last_status = Instant::now();
        }
    }

    fn push(&mut self, entry: RemoteEntry) {
        self.found = self.found.saturating_add(1);
        self.pending.push(entry);
        if self.pending.len() >= 25 || self.last_batch.elapsed() >= Duration::from_millis(120) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let entries = std::mem::take(&mut self.pending);
        let _ = self.events.send(SessionEvent::SftpSearchBatch {
            search_id: self.search_id.clone(),
            root: self.root.clone(),
            query: self.query.clone(),
            entries,
        });
        self.last_batch = Instant::now();
    }
}

fn track_task(tasks: &SftpTaskSet, task: JoinHandle<()>) {
    if let Ok(mut set) = tasks.lock() {
        set.retain(|task| !task.is_finished());
        set.push(task);
    } else {
        task.abort();
    }
}

fn abort_tracked_tasks(tasks: &SftpTaskSet) {
    if let Ok(mut set) = tasks.lock() {
        for task in set.iter() {
            task.abort();
        }
        set.clear();
    }
}

async fn search_dir_impl(
    sftp: &SftpSession,
    root: &str,
    query: &str,
    max_results: usize,
    max_dirs: usize,
    cancel: CancelFlag,
    shutdown: CancelFlag,
    emitter: &mut SearchEmitter<'_>,
) -> Result<()> {
    let q = query.trim().to_lowercase();
    let base = normalise_remote_dir(root);
    let mut stack = vec![base.clone()];
    let mut seen: HashSet<String> = HashSet::new();

    while let Some(dir) = stack.pop() {
        if is_cancelled(&cancel, &shutdown) {
            break;
        }
        let key = normalise_remote_dir(&dir);
        if !seen.insert(key) {
            continue;
        }
        emitter.scan_dir();
        if emitter.scanned > max_dirs || emitter.found >= max_results {
            break;
        }

        let entries = match list_dir_impl(sftp, &dir).await {
            Ok(v) => v,
            Err(_) => {
                // Permission denied under one branch should not cancel the entire search.
                continue;
            }
        };

        for mut entry in entries {
            let rel = if entry.full_path == base {
                entry.name.clone()
            } else {
                entry
                    .full_path
                    .strip_prefix(base.trim_end_matches('/'))
                    .unwrap_or(&entry.full_path)
                    .trim_start_matches('/')
                    .to_string()
            };
            let hay = format!("{} {}", entry.name.to_lowercase(), entry.full_path.to_lowercase());
            if q.is_empty() || hay.contains(&q) {
                if !rel.is_empty() {
                    entry.name = rel;
                }
                emitter.push(entry.clone());
                if emitter.found >= max_results {
                    break;
                }
            }
            if entry.is_dir && entry.kind != "symlink-dir" {
                let name = entry.name.rsplit('/').next().unwrap_or(&entry.name);
                let next = normalise_remote_dir(&entry.full_path);
                if name != "." && name != ".." && !seen.contains(&next) {
                    stack.push(next);
                }
            }
        }
    }

    Ok(())
}


/// List only child directories for the lightweight left navigation tree.
///
/// `list_dir_impl` returns both files and directories. The tree cache only
/// needs directories, and it must not include dead links or regular files;
/// otherwise the UI may draw an expandable arrow for a non-directory item.
async fn list_dirs_only_impl(sftp: &SftpSession, path: &str) -> Result<Vec<(String, String)>> {
    let entries = list_dir_impl(sftp, path).await?;
    let mut dirs: Vec<(String, String)> = entries
        .into_iter()
        .filter(|entry| entry.is_dir && entry.kind != "dead-link")
        .map(|entry| (entry.name, entry.full_path))
        .collect();

    dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    Ok(dirs)
}

async fn list_dir_impl(sftp: &SftpSession, path: &str) -> Result<Vec<RemoteEntry>> {
    let mut last_err: Option<anyhow::Error> = None;
    let raw = {
        let mut ok = None;
        for attempt in 0..2 {
            match tokio::time::timeout(SFTP_DIR_TIMEOUT, sftp.read_dir(path)).await {
                Ok(Ok(raw)) => {
                    ok = Some(raw);
                    break;
                }
                Ok(Err(e)) => {
                    let msg = e.to_string().to_lowercase();
                    let err = anyhow!(e).context(format!("read_dir {path} failed"));
                    if msg.contains("permission") || msg.contains("denied") || msg.contains("no such") {
                        return Err(err);
                    }
                    last_err = Some(err);
                }
                Err(_) => {
                    last_err = Some(anyhow!(
                        "Connection Timeout: read_dir {path} exceeded {}s",
                        SFTP_DIR_TIMEOUT.as_secs()
                    ));
                }
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(260)).await;
            }
        }
        ok.ok_or_else(|| last_err.unwrap_or_else(|| anyhow!("read_dir {path} failed")))?
    };

    let mut entries: Vec<RemoteEntry> = Vec::new();
    for e in raw.into_iter().filter(|e| {
        let n = e.file_name();
        n != "." && n != ".."
    }) {
        let name = e.file_name().to_string();
        let full_path = format!("{}/{}", path.trim_end_matches('/'), name);
        let meta = e.metadata();
        let permissions = meta.permissions.unwrap_or(0);
        let file_type = permissions & 0o170_000;
        let size = meta.size.unwrap_or(0);
        let modified = meta.mtime.unwrap_or(0);

        let mut is_dir = file_type == 0o040_000;
        let kind = if is_dir {
            "dir".to_string()
        } else if file_type == 0o120_000 {
            // SFTP directory listings often tell us only "this is a symlink".
            // Do a small, timed stat of the target so the UI can show a folder
            // symlink (small arrow) or a dead link (grey warning) before the user
            // wastes a double-click.
            match tokio::time::timeout(SFTP_STAT_TIMEOUT, sftp.metadata(&full_path)).await {
                Ok(Ok(target)) => {
                    let tperm = target.permissions.unwrap_or(0);
                    if (tperm & 0o170_000) == 0o040_000 {
                        is_dir = true;
                        "symlink-dir".to_string()
                    } else {
                        "symlink-file".to_string()
                    }
                }
                Ok(Err(_)) | Err(_) => "dead-link".to_string(),
            }
        } else {
            "file".to_string()
        };

        if kind == "dead-link" {
            is_dir = false;
        }

        entries.push(RemoteEntry {
            name,
            full_path,
            is_dir,
            kind,
            size,
            modified,
            mode: permissions & 0o7777,
        });
    }

    // Sort: directories first, then files; both groups alphabetically. Dead
    // links stay with files so they don't look expandable.
    entries.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(entries)
}

/// Emit a transfer-progress event.
fn emit_transfer(
    events: &UnboundedSender<SessionEvent>,
    id: &str,
    name: &str,
    is_upload: bool,
    transferred: u64,
    total: u64,
    state: u8,
    msg: &str,
) {
    let _ = events.send(SessionEvent::SftpTransfer {
        id: id.to_string(),
        name: name.to_string(),
        is_upload,
        transferred,
        total,
        state,
        msg: msg.to_string(),
    });
}

/// Download a remote file over a dedicated, *pipelined* raw SFTP channel.
///
/// The high-level reader issues one READ and waits for the reply before the
/// next, so throughput is capped by the round-trip time (slow on any latent
/// link). Here we keep many READ requests in flight at once, each tagged with
/// its absolute offset so out-of-order completion is fine — mirroring
/// `upload_pipelined`.
///
/// Returns `Ok(true)` when the whole file was written, or `Ok(false)` if the
/// transfer was cancelled. In both the cancel and error cases the partial
/// local file is removed so no half-downloaded junk is left behind.
async fn download_impl(
    handle: &client::Handle<SftpClientHandler>,
    remote: &str,
    local: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &CancelFlag,
    shutdown: &CancelFlag,
) -> Result<bool> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    const CHUNK: usize = 32 * 1024;
    const MAX_INFLIGHT: usize = 32; // ~1 MB outstanding hides the RTT

    let channel = handle
        .channel_open_session()
        .await
        .context("open sftp download channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    let raw = Arc::new(RawSftpSession::new(channel.into_stream()));
    raw.init().await.context("sftp download handshake")?;

    let total = raw
        .stat(remote)
        .await
        .ok()
        .and_then(|a| a.attrs.size)
        .unwrap_or(0);
    let fhandle = raw
        .open(remote, OpenFlags::READ, FileAttributes::default())
        .await
        .with_context(|| format!("open remote {remote}"))?
        .handle;
    let mut local_file = tokio::fs::File::create(local)
        .await
        .with_context(|| format!("create local {local}"))?;

    emit_transfer(events, id, name, false, 0, total, 0, "");

    let mut done: u64 = 0;
    let mut last = Instant::now();
    let mut err: Option<anyhow::Error> = None;
    let mut cancelled = false;

    if total > 0 {
        let mut next_off = 0u64;
        let mut inflight = FuturesUnordered::new();
        loop {
            if is_cancelled(cancel, shutdown) {
                cancelled = true;
            }
            // Top up the pipeline with fresh READ requests.
            while !cancelled && err.is_none() && next_off < total && inflight.len() < MAX_INFLIGHT {
                let off = next_off;
                let want = ((total - off) as usize).min(CHUNK);
                next_off += want as u64;
                let raw2 = raw.clone();
                let h = fhandle.clone();
                inflight.push(async move {
                    // Fill the whole chunk, coping with short reads.
                    let mut data = Vec::with_capacity(want);
                    let mut o = off;
                    let end = off + want as u64;
                    while o < end {
                        match raw2.read(h.clone(), o, (end - o) as u32).await {
                            Ok(d) => {
                                if d.data.is_empty() {
                                    break;
                                }
                                o += d.data.len() as u64;
                                data.extend_from_slice(&d.data);
                            }
                            Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
                            Err(e) => return Err(anyhow!("read remote: {e}")),
                        }
                    }
                    Ok::<(u64, Vec<u8>), anyhow::Error>((off, data))
                });
            }
            if inflight.is_empty() {
                break;
            }
            match inflight.next().await {
                Some(Ok((off, data))) => {
                    if !data.is_empty() {
                        if let Err(e) = local_file.seek(std::io::SeekFrom::Start(off)).await {
                            err = Some(anyhow!("seek local: {e}"));
                        } else if let Err(e) = local_file.write_all(&data).await {
                            err = Some(anyhow!("write local: {e}"));
                        } else {
                            done += data.len() as u64;
                        }
                    }
                    if last.elapsed() >= Duration::from_millis(150) {
                        last = Instant::now();
                        emit_transfer(events, id, name, false, done, total, 0, "");
                    }
                }
                Some(Err(e)) => err = Some(e),
                None => {}
            }
            if (cancelled || err.is_some()) && inflight.is_empty() {
                break;
            }
        }
    } else {
        // Unknown / zero size: serial drain until EOF (rare; keeps correctness).
        let mut off = 0u64;
        loop {
            if is_cancelled(cancel, shutdown) {
                cancelled = true;
                break;
            }
            match raw.read(fhandle.clone(), off, CHUNK as u32).await {
                Ok(d) => {
                    if d.data.is_empty() {
                        break;
                    }
                    local_file
                        .write_all(&d.data)
                        .await
                        .context("write local file")?;
                    off += d.data.len() as u64;
                    done += d.data.len() as u64;
                    if last.elapsed() >= Duration::from_millis(150) {
                        last = Instant::now();
                        emit_transfer(events, id, name, false, done, done, 0, "");
                    }
                }
                Err(SftpError::Status(s)) if s.status_code == StatusCode::Eof => break,
                Err(e) => {
                    err = Some(anyhow!("read remote: {e}"));
                    break;
                }
            }
        }
    }

    let _ = raw.close(fhandle).await;

    if let Some(e) = err {
        drop(local_file);
        let _ = tokio::fs::remove_file(local).await;
        return Err(e);
    }
    if cancelled {
        drop(local_file);
        let _ = tokio::fs::remove_file(local).await;
        emit_transfer(events, id, name, false, done, total, 4, t("已取消", "Cancelled"));
        return Ok(false);
    }
    local_file.flush().await.context("flush local file")?;
    emit_transfer(events, id, name, false, done, total.max(done), 1, "");
    Ok(true)
}

/// Recursively download a remote directory tree under `local_parent` (#50).
///
/// Iterative (work-stack) rather than a boxed async recursion: each remote dir
/// is mirrored to a sanitized local name, then its files are downloaded with
/// the same per-file pipeline used for single downloads. Names are sanitized (#26)
/// so a hostile server can't escape the chosen folder.
async fn download_dir(
    sftp: &SftpSession,
    handle: &client::Handle<SftpClientHandler>,
    remote_root: &str,
    local_parent: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &CancelFlag,
    shutdown: &CancelFlag,
) -> Result<bool> {
    let root_name = sanitize_filename(&base_name(remote_root));
    let root_local = format!("{}/{}", local_parent.trim_end_matches('/'), root_name);
    // (remote_dir, local_dir) pairs still to mirror.
    let mut stack = vec![(remote_root.trim_end_matches('/').to_string(), root_local.clone())];
    let mut visited: HashSet<String> = HashSet::new();
    let mut nodes = 0usize;
    while let Some((rdir, ldir)) = stack.pop() {
        if is_cancelled(cancel, shutdown) {
            let _ = tokio::fs::remove_dir_all(&root_local).await;
            return Ok(false);
        }
        let key = normalise_remote_dir(&rdir);
        if !visited.insert(key) {
            continue;
        }
        nodes += 1;
        if nodes > SFTP_MAX_RECURSIVE_NODES {
            return Err(anyhow!("recursive download exceeded node limit"));
        }
        tokio::fs::create_dir_all(&ldir)
            .await
            .with_context(|| format!("create local dir {ldir}"))?;
        for entry in list_dir_impl(sftp, &rdir).await? {
            if is_cancelled(cancel, shutdown) {
                let _ = tokio::fs::remove_dir_all(&root_local).await;
                return Ok(false);
            }
            nodes += 1;
            if nodes > SFTP_MAX_RECURSIVE_NODES {
                return Err(anyhow!("recursive download exceeded node limit"));
            }
            if entry.is_dir && entry.kind != "symlink-dir" {
                let child_local = format!("{}/{}", ldir, sanitize_filename(&entry.name));
                stack.push((entry.full_path, child_local));
            } else if !entry.is_dir {
                let fname = sanitize_filename(&entry.name);
                let lpath = format!("{}/{}", ldir, fname);
                let id = Uuid::new_v4().to_string();
                if !download_impl(handle, &entry.full_path, &lpath, &fname, &id, events, cancel, shutdown).await? {
                    let _ = tokio::fs::remove_dir_all(&root_local).await;
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

/// Recursively remove a remote directory tree (#50 follow-up).
///
/// A plain `remove_dir` only deletes an *empty* directory, so deleting an
/// uploaded folder failed. We BFS to discover every sub-directory (deleting
/// files as we go), then rmdir them deepest-first.
async fn remove_dir_recursive(sftp: &SftpSession, root: &str) -> Result<()> {
    if is_forbidden_remote_path(root) {
        return Err(anyhow!("refusing to delete dangerous path"));
    }
    let mut all_dirs = vec![root.trim_end_matches('/').to_string()];
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(normalise_remote_dir(root));
    let mut nodes = 1usize;
    let mut i = 0;
    while i < all_dirs.len() {
        let d = all_dirs[i].clone();
        i += 1;
        for entry in list_dir_impl(sftp, &d).await? {
            nodes += 1;
            if nodes > SFTP_MAX_RECURSIVE_NODES {
                return Err(anyhow!("recursive delete exceeded node limit"));
            }
            if entry.is_dir && entry.kind != "symlink-dir" {
                let key = normalise_remote_dir(&entry.full_path);
                if visited.insert(key) {
                    all_dirs.push(entry.full_path);
                }
            } else {
                sftp.remove_file(&entry.full_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("remove file {}: {e}", entry.full_path))?;
            }
        }
    }
    // BFS discovered parents before children, so reversing gives deepest-first.
    for d in all_dirs.iter().rev() {
        sftp.remove_dir(d)
            .await
            .map_err(|e| anyhow::anyhow!("remove dir {d}: {e}"))?;
    }
    Ok(())
}

async fn cleanup_remote_upload(sftp: &SftpSession, files: &[String], dirs: &[String]) {
    for file in files.iter().rev() {
        let _ = sftp.remove_file(file).await;
    }
    for dir in dirs.iter().rev() {
        let _ = sftp.remove_dir(dir).await;
    }
}

/// Recursively upload a local directory tree into `remote_parent` (#50).
///
/// Iterative work-stack: mirror each local dir to the remote (create_dir, whose
/// "already exists" error is ignored), then upload its files with the pipelined
/// path. Symlinks and other special files are skipped.
async fn upload_dir(
    handle: &client::Handle<SftpClientHandler>,
    sftp: &SftpSession,
    local_root: &str,
    remote_parent: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &CancelFlag,
    shutdown: &CancelFlag,
) -> Result<bool> {
    let root_name = base_name(local_root);
    let remote_root = format!("{}/{}", remote_parent.trim_end_matches('/'), root_name);
    let mut stack = vec![(local_root.to_string(), remote_root)];
    let mut visited: HashSet<String> = HashSet::new();
    let mut created_files: Vec<String> = Vec::new();
    let mut created_dirs: Vec<String> = Vec::new();
    let mut nodes = 0usize;
    while let Some((ldir, rdir)) = stack.pop() {
        if is_cancelled(cancel, shutdown) {
            cleanup_remote_upload(sftp, &created_files, &created_dirs).await;
            return Ok(false);
        }
        let key = tokio::fs::canonicalize(&ldir)
            .await
            .unwrap_or_else(|_| Path::new(&ldir).to_path_buf())
            .to_string_lossy()
            .into_owned();
        if !visited.insert(key) {
            continue;
        }
        nodes += 1;
        if nodes > SFTP_MAX_RECURSIVE_NODES {
            return Err(anyhow!("recursive upload exceeded node limit"));
        }
        // Best-effort mkdir; an error usually just means the dir already exists.
        if sftp.create_dir(&rdir).await.is_ok() {
            created_dirs.push(rdir.clone());
        }
        let mut rd = tokio::fs::read_dir(&ldir)
            .await
            .with_context(|| format!("read local dir {ldir}"))?;
        while let Some(entry) = rd.next_entry().await.context("read dir entry")? {
            if is_cancelled(cancel, shutdown) {
                cleanup_remote_upload(sftp, &created_files, &created_dirs).await;
                return Ok(false);
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let lpath = entry.path().to_string_lossy().to_string();
            let rchild = format!("{}/{}", rdir, name);
            let meta = tokio::fs::symlink_metadata(entry.path())
                .await
                .context("file type")?;
            nodes += 1;
            if nodes > SFTP_MAX_RECURSIVE_NODES {
                return Err(anyhow!("recursive upload exceeded node limit"));
            }
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push((lpath, rchild));
            } else if meta.is_file() {
                let id = Uuid::new_v4().to_string();
                if upload_pipelined(handle, &lpath, &rchild, &name, &id, events, cancel, shutdown).await? {
                    created_files.push(rchild);
                } else {
                    cleanup_remote_upload(sftp, &created_files, &created_dirs).await;
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}

/// Pipelined SFTP upload (#16).
///
/// The high-level `SftpSession`/`File` writes one chunk and waits for the
/// server's ack before sending the next, so throughput is capped by the
/// round-trip time (~15x slower than scp on a latent link).  Here we open a
/// dedicated raw SFTP channel and keep many WRITE requests in flight at once
/// (each tagged with its absolute offset, so out-of-order completion is fine),
/// which hides the latency and brings us within a single order of magnitude of
/// native scp.
async fn upload_pipelined(
    handle: &client::Handle<SftpClientHandler>,
    local: &str,
    remote: &str,
    name: &str,
    id: &str,
    events: &UnboundedSender<SessionEvent>,
    cancel: &CancelFlag,
    shutdown: &CancelFlag,
) -> Result<bool> {
    use tokio::io::AsyncReadExt;

    const CHUNK: usize = 32 * 1024; // safe SFTP write size
    const MAX_INFLIGHT: usize = 32; // ~1 MB of outstanding writes hides the RTT

    let total = tokio::fs::metadata(local)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    let mut local_file = tokio::fs::File::open(local)
        .await
        .with_context(|| format!("open local {local}"))?;

    // Dedicated raw SFTP channel for the transfer (keeps the browse session
    // responsive and lets us issue concurrent WRITE requests).
    let channel = handle
        .channel_open_session()
        .await
        .context("open sftp upload channel")?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .context("request sftp subsystem")?;
    let raw = Arc::new(RawSftpSession::new(channel.into_stream()));
    raw.init().await.context("sftp upload handshake")?;

    let fhandle = raw
        .open(
            remote,
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
            FileAttributes::default(),
        )
        .await
        .with_context(|| format!("create remote {remote}"))?
        .handle;

    emit_transfer(events, id, name, true, 0, total, 0, "");

    let mut offset: u64 = 0;
    let mut done: u64 = 0;
    let mut last = Instant::now();
    let mut eof = false;
    let mut err: Option<anyhow::Error> = None;
    let mut cancelled = false;
    let mut inflight = FuturesUnordered::new();

    while !eof || !inflight.is_empty() {
        if is_cancelled(cancel, shutdown) {
            cancelled = true;
            eof = true; // stop reading more; drain what's in flight
        }
        // Top up the pipeline with fresh WRITE requests.
        while !eof && inflight.len() < MAX_INFLIGHT {
            let mut buf = vec![0u8; CHUNK];
            match local_file.read(&mut buf).await {
                Ok(0) => eof = true,
                Ok(n) => {
                    buf.truncate(n);
                    let off = offset;
                    offset += n as u64;
                    let raw2 = raw.clone();
                    let h = fhandle.clone();
                    inflight.push(async move {
                        raw2.write(h, off, buf).await.map(|_| n as u64)
                    });
                }
                Err(e) => {
                    err = Some(anyhow!("read local file: {e}"));
                    eof = true;
                }
            }
        }
        match inflight.next().await {
            Some(Ok(n)) => {
                done += n;
                if last.elapsed() >= Duration::from_millis(150) {
                    last = Instant::now();
                    emit_transfer(events, id, name, true, done, total, 0, "");
                }
            }
            Some(Err(e)) => {
                err = Some(anyhow!("write remote file: {e}"));
                eof = true; // stop reading more
            }
            None => {}
        }
        if err.is_some() {
            break;
        }
    }

    let _ = raw.close(fhandle).await;
    if let Some(e) = err {
        // Drop the partial remote file so a failed upload leaves no junk.
        let _ = raw.remove(remote).await;
        return Err(e);
    }
    if cancelled {
        // Remove the half-written remote file on cancel (#100).
        let _ = raw.remove(remote).await;
        emit_transfer(events, id, name, true, done, total, 4, t("已取消", "Cancelled"));
        return Ok(false);
    }
    emit_transfer(events, id, name, true, done, total.max(done), 1, "");
    Ok(true)
}

// ---------------------------------------------------------------------------
// russh client handler — verifies the host key against known_hosts, reusing the
// shell session's prompt path (#109-5). The UI de-duplicates by host:port, so a
// fresh host confirmed for the shell won't prompt again for SFTP.
// ---------------------------------------------------------------------------

struct SftpClientHandler {
    host: String,
    port: u16,
    events: UnboundedSender<SessionEvent>,
}

fn sftp_handler(session: &Session, events: &UnboundedSender<SessionEvent>) -> SftpClientHandler {
    SftpClientHandler {
        host: session.host.clone(),
        port: session.port,
        events: events.clone(),
    }
}

#[async_trait]
impl Handler for SftpClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(crate::ssh::verify_host_key(&self.host, self.port, server_public_key, &self.events).await)
    }

    async fn data(
        &mut self,
        _channel: russh::ChannelId,
        _data: &[u8],
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

// Keep format helpers and RemoteTreeNode imports live.
const _: fn() = || {
    let _ = format_size(0);
    let _ = format_mtime(0);
    let _: RemoteTreeNode;
};

#[cfg(test)]
mod sanitize_tests {
    use super::sanitize_filename;

    #[test]
    fn plain_names_pass_through() {
        assert_eq!(sanitize_filename("report.txt"), "report.txt");
        assert_eq!(sanitize_filename("my-file_v2.tar.gz"), "my-file_v2.tar.gz");
        assert_eq!(sanitize_filename("数据.csv"), "数据.csv");
        // Unix dotfiles keep their leading dot.
        assert_eq!(sanitize_filename(".bashrc"), ".bashrc");
    }

    #[test]
    fn strips_path_separators_and_traversal() {
        // base_name already strips dirs, but sanitize is defence-in-depth: the
        // result must never keep a separator that could escape the target dir.
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
        let traversal = sanitize_filename("../../etc/passwd");
        assert!(!traversal.contains('/') && !traversal.contains('\\'));
        let win = sanitize_filename("..\\..\\Windows\\System32");
        assert!(!win.contains('/') && !win.contains('\\'));
    }

    #[test]
    fn replaces_shell_and_windows_special_chars() {
        assert_eq!(sanitize_filename("foo&calc.exe"), "foo_calc.exe");
        assert_eq!(sanitize_filename("a|b>c<d:e?f*g"), "a_b_c_d_e_f_g");
        assert_eq!(sanitize_filename("$(whoami)"), "_(whoami)");
        assert_eq!(sanitize_filename("a`b'c"), "a_b_c");
    }

    #[test]
    fn trims_whitespace_and_trailing_dots() {
        assert_eq!(sanitize_filename("   spaced.txt  "), "spaced.txt");
        assert_eq!(sanitize_filename("name..."), "name");
        // control chars become underscores, not trimmed
        assert_eq!(sanitize_filename("a\tb"), "a_b");
    }

    #[test]
    fn neutralises_windows_reserved_device_names() {
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("nul"), "_nul");
        assert_eq!(sanitize_filename("COM1"), "_COM1");
        assert_eq!(sanitize_filename("LPT9.txt"), "_LPT9.txt"); // reserved even with ext
        assert_eq!(sanitize_filename("Aux.log"), "_Aux.log");
        // Not reserved: a name that merely starts with the same letters.
        assert_eq!(sanitize_filename("console.txt"), "console.txt");
        assert_eq!(sanitize_filename("COM10"), "COM10");
    }

    #[test]
    fn empty_or_all_bad_falls_back() {
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename("   "), "file");
        assert_eq!(sanitize_filename("..."), "file");
    }
}
