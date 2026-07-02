//! Starter quick commands for Probe Shell's "one-click ops" direction.
//!
//! The list is intentionally small: fewer commands means less visual noise and
//! less risk of a dangerous operation being clicked by mistake.

use crate::config::QuickCommand;

fn cmd(name: &str, command: &str, group: &str) -> QuickCommand {
    QuickCommand {
        name: name.to_string(),
        command: command.to_string(),
        group: group.to_string(),
        send_enter: true,
    }
}

pub fn starter_quick_groups() -> Vec<String> {
    vec!["System".into(), "OpenWrt".into(), "Docker".into()]
}

pub fn starter_quick_commands() -> Vec<QuickCommand> {
    vec![
        cmd("系统概览", "uname -a && uptime && free -h && df -h", "System"),
        cmd("监听端口", "ss -tulpen | head -80", "System"),
        cmd("进程 Top", "ps aux --sort=-%mem | head -20", "System"),
        cmd("OpenWrt 版本", "ubus call system board", "OpenWrt"),
        cmd("OpenWrt 日志", "logread -f", "OpenWrt"),
        cmd("DHCP 租约", "cat /tmp/dhcp.leases", "OpenWrt"),
        cmd("Docker 容器", "docker ps --format 'table {{.Names}}\\t{{.Status}}\\t{{.Ports}}'", "Docker"),
        cmd("Docker 占用", "docker stats --no-stream", "Docker"),
    ]
}
