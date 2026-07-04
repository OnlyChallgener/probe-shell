//! Lightweight device profiling for session cards and file-mode defaults.
//!
//! The classifier is intentionally offline-only for the session list: it never
//! opens a socket and never blocks the UI.  It combines stable signals in this
//! order: explicit session kind, user-provided name/note keywords, brand hints,
//! and finally very conservative IP-gateway heuristics.  If confidence is low we
//! fall back to the protocol label (SSH/Telnet/Serial) instead of guessing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    OpenWrt,
    Router,
    RuijieRouter,
    XiaomiRouter,
    TpLinkRouter,
    HuaweiRouter,
    AsusRouter,
    Modem,
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
    /// Short label shown on the left session card.
    pub label: &'static str,
    pub icon: &'static str,
    pub hint_zh: &'static str,
    pub hint_en: &'static str,
    /// Suggested file-browser mode.  This is a hint only; the worker still
    /// falls back automatically when a server rejects the preferred mode.
    pub file_mode: FileModeHint,
    /// 0-100, used for future UI detail pages.  Unknown devices intentionally
    /// remain low-confidence and show the protocol label instead of a guess.
    pub confidence: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileModeHint {
    Auto,
    SftpFirst,
    SshBrowserFirst,
    None,
}

impl DeviceKind {
    fn profile(self, protocol_label: &'static str) -> DeviceProfile {
        match self {
            DeviceKind::OpenWrt => DeviceProfile { kind: self, label: "OpenWrt", icon: "router", hint_zh: "OpenWrt 路由 · 自动选择文件模式", hint_en: "OpenWrt router · auto file mode", file_mode: FileModeHint::Auto, confidence: 95 },
            DeviceKind::RuijieRouter => DeviceProfile { kind: self, label: "锐捷路由", icon: "router", hint_zh: "锐捷路由 · 自动选择文件模式", hint_en: "Ruijie router · auto file mode", file_mode: FileModeHint::Auto, confidence: 92 },
            DeviceKind::XiaomiRouter => DeviceProfile { kind: self, label: "小米路由", icon: "router", hint_zh: "小米路由 · 自动选择文件模式", hint_en: "Xiaomi router · auto file mode", file_mode: FileModeHint::Auto, confidence: 88 },
            DeviceKind::TpLinkRouter => DeviceProfile { kind: self, label: "TP路由", icon: "router", hint_zh: "TP-Link 路由", hint_en: "TP-Link router", file_mode: FileModeHint::Auto, confidence: 86 },
            DeviceKind::HuaweiRouter => DeviceProfile { kind: self, label: "华为设备", icon: "router", hint_zh: "华为网络设备", hint_en: "Huawei network device", file_mode: FileModeHint::Auto, confidence: 84 },
            DeviceKind::AsusRouter => DeviceProfile { kind: self, label: "华硕路由", icon: "router", hint_zh: "华硕路由", hint_en: "ASUS router", file_mode: FileModeHint::Auto, confidence: 84 },
            DeviceKind::Router => DeviceProfile { kind: self, label: "Router", icon: "router", hint_zh: "路由管理 · 自动选择文件模式", hint_en: "router ops · auto file mode", file_mode: FileModeHint::Auto, confidence: 80 },
            DeviceKind::Modem => DeviceProfile { kind: self, label: "光猫", icon: "settings_input_component", hint_zh: "光猫/ONT", hint_en: "modem / ONT", file_mode: FileModeHint::None, confidence: 86 },
            DeviceKind::Nas => DeviceProfile { kind: self, label: "NAS", icon: "storage", hint_zh: "NAS · SFTP优先", hint_en: "NAS · SFTP first", file_mode: FileModeHint::SftpFirst, confidence: 90 },
            DeviceKind::Docker => DeviceProfile { kind: self, label: "Docker", icon: "dns", hint_zh: "Docker 主机", hint_en: "Docker host", file_mode: FileModeHint::SftpFirst, confidence: 88 },
            DeviceKind::Linux => DeviceProfile { kind: self, label: "Linux", icon: "terminal", hint_zh: "Linux 主机", hint_en: "Linux host", file_mode: FileModeHint::SftpFirst, confidence: 82 },
            DeviceKind::Cloud => DeviceProfile { kind: self, label: "云主机", icon: "cloud", hint_zh: "云服务器", hint_en: "cloud server", file_mode: FileModeHint::SftpFirst, confidence: 82 },
            DeviceKind::Windows => DeviceProfile { kind: self, label: "Windows", icon: "desktop_windows", hint_zh: "Windows 主机", hint_en: "Windows host", file_mode: FileModeHint::SftpFirst, confidence: 82 },
            DeviceKind::Serial => DeviceProfile { kind: self, label: "Serial", icon: "settings_ethernet", hint_zh: "串口设备", hint_en: "serial device", file_mode: FileModeHint::None, confidence: 100 },
            DeviceKind::Telnet => DeviceProfile { kind: self, label: "Telnet", icon: "settings_input_component", hint_zh: "Telnet 会话", hint_en: "telnet session", file_mode: FileModeHint::None, confidence: 100 },
            DeviceKind::Unknown => DeviceProfile { kind: self, label: protocol_label, icon: "dns", hint_zh: "", hint_en: "", file_mode: FileModeHint::Auto, confidence: 0 },
        }
    }
}

pub fn classify(kind: &str, name: &str, host: &str, user: &str, note: &str) -> DeviceProfile {
    let kind_l = kind.trim().to_ascii_lowercase();
    if kind_l == "serial" {
        return DeviceKind::Serial.profile("Serial");
    }
    if kind_l == "telnet" {
        return DeviceKind::Telnet.profile("Telnet");
    }

    let hay_raw = format!("{} {} {} {}", name, host, user, note);
    let hay = hay_raw.to_ascii_lowercase();
    let host_l = host.trim().to_ascii_lowercase();

    let device = if contains_any(&hay, &["openwrt", "immortalwrt", "lede", "istoreos", "istore os", "ubus", "opkg"]) {
        DeviceKind::OpenWrt
    } else if contains_any(&hay, &["ruijie", "锐捷", "be72", "eg310", "rg-"]) {
        DeviceKind::RuijieRouter
    } else if contains_any(&hay, &["xiaomi", "miwifi", "redmi", "小米", "红米"]) || host_l == "192.168.31.1" {
        DeviceKind::XiaomiRouter
    } else if contains_any(&hay, &["tplink", "tp-link", "tp link", "普联"]) || host_l == "192.168.0.1" {
        DeviceKind::TpLinkRouter
    } else if contains_any(&hay, &["huawei", "华为", "honor", "荣耀"]) {
        DeviceKind::HuaweiRouter
    } else if contains_any(&hay, &["asus", "华硕", "merlin", "梅林"]) || host_l == "192.168.50.1" {
        DeviceKind::AsusRouter
    } else if contains_any(&hay, &["router", "路由", "gateway", "网关", "gw", "ikuai", "爱快", "mwan", "wan6"]) {
        DeviceKind::Router
    } else if contains_any(&hay, &["光猫", "猫", "modem", "onu", "ont", "宽带猫"]) {
        DeviceKind::Modem
    } else if contains_any(&hay, &["synology", "群晖", "nas", "飞牛", "fnos", "truenas", "unraid", "qnap", "威联通"]) {
        DeviceKind::Nas
    } else if contains_any(&hay, &["docker", "compose", "container", "portainer"]) {
        DeviceKind::Docker
    } else if contains_any(&hay, &["ubuntu", "debian", "centos", "rocky", "almalinux", "linux", "fedora", "archlinux"]) {
        DeviceKind::Linux
    } else if contains_any(&hay, &["vps", "ecs", "cloud", "aliyun", "tencent", "aws", "azure", "oracle"]) {
        DeviceKind::Cloud
    } else if contains_any(&hay, &["windows", "win11", "win10", "powershell", "pwsh"]) {
        DeviceKind::Windows
    } else if looks_like_gateway_ip(&host_l) && contains_any(&hay, &["root", "admin"]) && contains_any(&hay, &["ssh", "lan", "home"]) {
        // Conservative gateway fallback: only classify when another weak signal
        // says this is a managed network device.  A bare 192.168.x.1 by itself
        // stays Unknown to avoid false labels.
        DeviceKind::Router
    } else {
        DeviceKind::Unknown
    };

    device.profile(if kind_l == "ssh" || kind_l.is_empty() { "SSH" } else { "SSH" })
}

pub fn recommended_file_mode(kind: &str, name: &str, host: &str, user: &str, note: &str) -> FileModeHint {
    classify(kind, name, host, user, note).file_mode
}

fn contains_any(hay: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| hay.contains(n))
}

fn looks_like_gateway_ip(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 || parts[3] != "1" {
        return false;
    }
    let Ok(a) = parts[0].parse::<u8>() else { return false; };
    let Ok(b) = parts[1].parse::<u8>() else { return false; };
    matches!(a, 10) || (a == 192 && b == 168) || (a == 172 && (16..=31).contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_openwrt() {
        assert_eq!(classify("ssh", "OpenWrt BE72", "192.168.5.1", "root", "").kind, DeviceKind::OpenWrt);
    }

    #[test]
    fn unknown_root_is_not_forced_linux() {
        assert_eq!(classify("ssh", "OP", "op.example.com", "root", "").kind, DeviceKind::Unknown);
    }

    #[test]
    fn detects_ruijie_from_brand() {
        assert_eq!(classify("ssh", "BE72", "192.168.5.1", "root", "锐捷主路由").kind, DeviceKind::RuijieRouter);
    }
}
