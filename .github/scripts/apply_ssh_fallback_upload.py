from pathlib import Path

sftp_path = Path("src/sftp.rs")
cargo_path = Path("Cargo.toml")
text = sftp_path.read_text(encoding="utf-8")
cargo = cargo_path.read_text(encoding="utf-8")


def replace_once(source: str, old: str, new: str, label: str) -> str:
    count = source.count(old)
    if count != 1:
        raise RuntimeError(f"{label}: expected exactly one match, found {count}")
    return source.replace(old, new, 1)


def replace_after(source: str, anchor: str, old: str, new: str, label: str) -> str:
    anchor_pos = source.find(anchor)
    if anchor_pos < 0:
        raise RuntimeError(f"{label}: anchor not found")
    pos = source.find(old, anchor_pos)
    if pos < 0:
        raise RuntimeError(f"{label}: target not found after anchor")
    return source[:pos] + new + source[pos + len(old):]


text = replace_once(
    text,
    "use std::sync::atomic::{AtomicBool, Ordering};",
    "use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};",
    "atomic imports",
)

text = replace_once(
    text,
    "const SSH_BROWSER_EXEC_TIMEOUT: Duration = Duration::from_secs(18);\nconst SFTP_MAX_RECURSIVE_NODES: usize = 20_000;",
    "const SSH_BROWSER_EXEC_TIMEOUT: Duration = Duration::from_secs(18);\nconst SSH_UPLOAD_FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);\nconst SSH_UPLOAD_PROGRESS_INTERVAL: Duration = Duration::from_millis(150);\nconst SFTP_MAX_RECURSIVE_NODES: usize = 20_000;",
    "upload constants",
)

text = replace_once(
    text,
    '''    let _ = events.send(SessionEvent::SftpStatus(t(
        "SSH 文件浏览模式：服务器未提供 SFTP 子系统",
        "SSH file-browser mode: server does not provide an SFTP subsystem",
    ).into()));''',
    '''    let _ = events.send(SessionEvent::SftpStatus(t(
        "标准 SFTP 不可用，已切换为 SSH 兼容传输",
        "Standard SFTP unavailable; using SSH-compatible transfer",
    ).into()));''',
    "fallback status",
)

anchor = "// Same rule as real SFTP mode: recursive search must never block the file"
text = replace_after(
    text,
    anchor,
    '''    let mut search_cancel: Option<Arc<AtomicBool>> = None;
    let mut search_task: Option<tokio::task::AbortHandle> = None;
    let mut active_search: Option<(String, String, String)> = None;
    while let Some(cmd) = commands.recv().await {''',
    '''    let mut search_cancel: Option<Arc<AtomicBool>> = None;
    let mut search_task: Option<tokio::task::AbortHandle> = None;
    let mut active_search: Option<(String, String, String)> = None;
    // SSH-compatible uploads run on independent exec channels and tasks. The
    // directory command loop therefore remains responsive while bytes flow.
    let transfer_cancels: Arc<Mutex<HashMap<String, CancelFlag>>> =
        Arc::new(Mutex::new(HashMap::new()));
    while let Some(cmd) = commands.recv().await {''',
    "fallback transfer state",
)

text = replace_after(
    text,
    anchor,
    '''                if let Some(task) = search_task.take() {
                    task.abort();
                }
                let _ = events.send(SessionEvent::SftpStatus(t(
                    "文件连接已断开",''',
    '''                if let Some(task) = search_task.take() {
                    task.abort();
                }
                if let Ok(cancels) = transfer_cancels.lock() {
                    for cancel in cancels.values() {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
                let _ = events.send(SessionEvent::SftpStatus(t(
                    "文件连接已断开",''',
    "fallback close cancellation",
)

text = replace_once(
    text,
    '''            SftpCommand::Upload { .. }
            | SftpCommand::DownloadArchive { .. }
            | SftpCommand::CancelTransfer(_) => {
                let _ = events.send(SessionEvent::SftpStatus(t(
                    "SSH 文件浏览模式暂不支持此传输操作；安装 openssh-sftp-server 后可使用完整 SFTP",
                    "This transfer is not supported in SSH file-browser mode; install openssh-sftp-server for full SFTP",
                ).into()));
            }''',
    '''            SftpCommand::Upload { local, remote_dir } => {
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
            }''',
    "fallback upload commands",
)

helper = r'''

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
'''

marker = '''/// Read a remote file as UTF-8 text for the built-in editor, rejecting files
/// that are too large, binary, or not valid UTF-8 (#70). Returns the text on'''
text = replace_once(text, marker, helper + "\n\n" + marker, "insert SSH upload helpers")

cargo = replace_once(
    cargo,
    'base64 = "0.22"\n',
    'base64 = "0.22"\n# Build safe temporary tar streams for SSH-compatible folder uploads.\ntar = "0.4"\n',
    "tar dependency",
)

sftp_path.write_text(text, encoding="utf-8")
cargo_path.write_text(cargo, encoding="utf-8")
