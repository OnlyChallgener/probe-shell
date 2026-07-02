//! Lightweight device profiling for session cards.
//!
//! This is intentionally heuristic and offline-only. It does not open a socket
//! or run remote commands while drawing the session list, so the welcome page
//! remains fast and cannot hang on offline hosts.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    OpenWrt,
    Router,
    Nas,
    Docker,
    Linux,
    Cloud,
    Windows,
    Serial,
    Telnet,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct DeviceProfile {
    pub kind: DeviceKind,
    pub label: &'static str,
    pub icon: &'static str,
    pub hint_zh: &'static str,
    pub hint_en: &'static str,
}

impl DeviceKind {
    fn profile(self) -> DeviceProfile {
        match self {
            DeviceKind::OpenWrt => DeviceProfile { kind: self, label: "OpenWrt", icon: "router", hint_zh: "推荐 logread / ubus", hint_en: "logread / ubus ready" },
            DeviceKind::Router => DeviceProfile { kind: self, label: "Router", icon: "router", hint_zh: "路由管理", hint_en: "router ops" },
            DeviceKind::Nas => DeviceProfile { kind: self, label: "NAS", icon: "storage", hint_zh: "文件 / Docker", hint_en: "files / docker" },
            DeviceKind::Docker => DeviceProfile { kind: self, label: "Docker", icon: "dns", hint_zh: "推荐 docker ps", hint_en: "docker ps ready" },
            DeviceKind::Linux => DeviceProfile { kind: self, label: "Linux", icon: "terminal", hint_zh: "系统状态", hint_en: "system status" },
            DeviceKind::Cloud => DeviceProfile { kind: self, label: "Cloud", icon: "cloud", hint_zh: "云服务器", hint_en: "cloud server" },
            DeviceKind::Windows => DeviceProfile { kind: self, label: "Windows", icon: "desktop_windows", hint_zh: "Windows 主机", hint_en: "windows host" },
            DeviceKind::Serial => DeviceProfile { kind: self, label: "Serial", icon: "settings_ethernet", hint_zh: "串口设备", hint_en: "serial device" },
            DeviceKind::Telnet => DeviceProfile { kind: self, label: "Telnet", icon: "settings_input_component", hint_zh: "旧设备", hint_en: "legacy device" },
            DeviceKind::Unknown => DeviceProfile { kind: self, label: "SSH", icon: "dns", hint_zh: "SSH 会话", hint_en: "ssh session" },
        }
    }
}

pub fn classify(kind: &str, name: &str, host: &str, user: &str, note: &str) -> DeviceProfile {
    let kind = kind.trim().to_ascii_lowercase();
    if kind == "serial" {
        return DeviceKind::Serial.profile();
    }
    if kind == "telnet" {
        return DeviceKind::Telnet.profile();
    }

    let hay = format!("{} {} {} {}", name, host, user, note).to_ascii_lowercase();
    let device = if hay.contains("openwrt") || hay.contains("immortalwrt") || hay.contains("lede") {
        DeviceKind::OpenWrt
    } else if hay.contains("be72") || hay.contains("router") || hay.contains("路由") || hay.contains("gateway") || hay.contains("gw") {
        DeviceKind::Router
    } else if hay.contains("synology") || hay.contains("群晖") || hay.contains("nas") {
        DeviceKind::Nas
    } else if hay.contains("docker") || hay.contains("compose") || hay.contains("container") {
        DeviceKind::Docker
    } else if hay.contains("ubuntu") || hay.contains("debian") || hay.contains("centos") || hay.contains("linux") {
        DeviceKind::Linux
    } else if hay.contains("vps") || hay.contains("ecs") || hay.contains("cloud") || hay.contains("aliyun") || hay.contains("tencent") {
        DeviceKind::Cloud
    } else if hay.contains("windows") || hay.contains("win11") || hay.contains("win10") {
        DeviceKind::Windows
    } else if user == "root" {
        DeviceKind::Linux
    } else {
        DeviceKind::Unknown
    };
    device.profile()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_openwrt() {
        assert_eq!(classify("ssh", "OpenWrt BE72", "192.168.5.1", "root", "").kind, DeviceKind::OpenWrt);
    }
}
