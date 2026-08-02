//! Local PowerShell/CMD worker backed by a real PTY (ConPTY on Windows).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{Child as PtyChild, CommandBuilder, PtySize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config::Session;
use crate::i18n::t;
use crate::ssh::{SessionCommand, SessionEvent, SessionHandle};

pub fn spawn_local_session(
    runtime: &tokio::runtime::Handle,
    tab_id: String,
    session: Session,
    cols: u32,
    rows: u32,
) -> (SessionHandle, UnboundedReceiver<SessionEvent>) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (events, event_rx) = mpsc::unbounded_channel();
    let task_events = events.clone();
    let join = runtime.spawn(async move {
        if let Err(error) = run(session, command_rx, task_events.clone(), cols, rows).await {
            let _ = task_events.send(SessionEvent::Closed(format!("{error:#}")));
        }
    });
    (
        SessionHandle {
            tab_id,
            commands,
            join,
        },
        event_rx,
    )
}

fn size(cols: u32, rows: u32) -> PtySize {
    PtySize {
        rows: rows.clamp(1, u16::MAX as u32) as u16,
        cols: cols.clamp(1, u16::MAX as u32) as u16,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[derive(Default)]
struct Utf8StreamDecoder {
    pending: Vec<u8>,
}

impl Utf8StreamDecoder {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.decode(bytes, false)
    }

    fn finish(&mut self) -> String {
        self.decode(&[], true)
    }

    fn decode(&mut self, bytes: &[u8], eof: bool) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    out.push_str(text);
                    self.pending.clear();
                    break;
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if valid > 0 {
                        if let Ok(text) = std::str::from_utf8(&self.pending[..valid]) {
                            out.push_str(text);
                        }
                        self.pending.drain(..valid);
                        continue;
                    }
                    if let Some(len) = err.error_len() {
                        out.push_str("\u{fffd}");
                        self.pending.drain(..len);
                        continue;
                    }
                    if eof {
                        out.push_str(&String::from_utf8_lossy(&self.pending));
                        self.pending.clear();
                    }
                    break;
                }
            }
        }
        out
    }
}

fn terminate_pty_child(child: &mut dyn PtyChild) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        if let Some(pid) = child.process_id() {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
    }
    let _ = child.kill();
}

async fn run(
    session: Session,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: UnboundedSender<SessionEvent>,
    cols: u32,
    rows: u32,
) -> Result<()> {
    let (program, args) = local_program(&session.host);
    let pty = portable_pty::native_pty_system();
    let pair = pty.openpty(size(cols, rows)).context("open local PTY")?;
    let mut command = CommandBuilder::new(&program);
    command.args(&args);
    command.env("TERM", "xterm-256color");
    command.env("LANG", "C.UTF-8");
    let child = pair
        .slave
        .spawn_command(command)
        .with_context(|| format!("start {program}"))?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("clone PTY reader")?;
    let writer = Arc::new(Mutex::new(
        pair.master.take_writer().context("take PTY writer")?,
    ));
    let child = Arc::new(Mutex::new(child));
    let read_events = events.clone();
    std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        let mut decoder = Utf8StreamDecoder::default();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    let tail = decoder.finish();
                    if !tail.is_empty() {
                        let _ = read_events.send(SessionEvent::Output(tail));
                    }
                    let _ = read_events.send(SessionEvent::Closed(
                        t("本地终端已退出", "Local terminal exited").into(),
                    ));
                    break;
                }
                Ok(n) => {
                    let text = decoder.push(&buffer[..n]);
                    if !text.is_empty() {
                        if read_events.send(SessionEvent::Output(text)).is_err() {
                            break;
                        }
                    }
                }
                Err(error) => {
                    let _ = read_events.send(SessionEvent::Closed(format!(
                        "{}: {error}",
                        t("本地终端读取失败", "Local terminal read failed")
                    )));
                    break;
                }
            }
        }
    });

    let _ = events.send(SessionEvent::Connected);
    let _ = events.send(SessionEvent::Status(format!(
        "{} {}",
        t("已启动", "Started"),
        session.name
    )));
    while let Some(command) = commands.recv().await {
        match command {
            SessionCommand::RawInput(bytes) => {
                let result = writer
                    .lock()
                    .map_err(|_| anyhow::anyhow!("PTY writer lock poisoned"))
                    .and_then(|mut out| {
                        out.write_all(&bytes)
                            .and_then(|_| out.flush())
                            .map_err(Into::into)
                    });
                if result.is_err() {
                    break;
                }
            }
            SessionCommand::Resize(c, r) => {
                let _ = pair.master.resize(size(c, r));
            }
            SessionCommand::Close => {
                if let Ok(mut c) = child.lock() {
                    terminate_pty_child(c.as_mut());
                }
                break;
            }
            SessionCommand::AddTunnel { .. } | SessionCommand::StopTunnel(_) => {}
        }
    }
    if let Ok(mut c) = child.lock() {
        terminate_pty_child(c.as_mut());
    }
    Ok(())
}

fn local_program(kind: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    match kind {
        "cmd" => ("cmd.exe".into(), vec!["/Q".into(), "/K".into(), "chcp 65001>nul".into()]),
        _ => ("powershell.exe".into(), vec![
            "-NoLogo".into(), "-NoExit".into(), "-Command".into(),
            "$u=[Text.UTF8Encoding]::new($false);[Console]::InputEncoding=$u;[Console]::OutputEncoding=$u;$OutputEncoding=$u".into(),
        ]),
    }
    #[cfg(not(windows))]
    {
        (
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            Vec::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::local_program;
    #[cfg(windows)]
    #[test]
    fn windows_shells_are_utf8() {
        assert!(local_program("powershell")
            .1
            .iter()
            .any(|v| v.contains("OutputEncoding")));
        assert!(local_program("cmd").1.iter().any(|v| v.contains("65001")));
    }
}
