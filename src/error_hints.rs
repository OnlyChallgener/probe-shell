//! Human-readable connection error hints.

pub struct ErrorHint {
    pub zh: &'static str,
    pub en: &'static str,
}

pub fn hint_for(reason: &str) -> Option<ErrorHint> {
    let r = reason.to_ascii_lowercase();
    if r.contains("permission denied") || r.contains("auth") || r.contains("password") {
        Some(ErrorHint { zh: "认证失败：请检查用户名、密码或私钥。", en: "Auth failed: check username, password or private key." })
    } else if r.contains("timed out") || r.contains("timeout") {
        Some(ErrorHint { zh: "连接超时：设备可能离线、端口不通或被防火墙拦截。", en: "Connection timed out: host may be offline, blocked, or unreachable." })
    } else if r.contains("refused") {
        Some(ErrorHint { zh: "连接被拒绝：SSH 服务可能未启动，或端口填写错误。", en: "Connection refused: SSH service may be stopped or the port is wrong." })
    } else if r.contains("no route") || r.contains("unreachable") {
        Some(ErrorHint { zh: "网络不可达：请检查当前网络、网关、VPN 或 IPv6 路由。", en: "Network unreachable: check network, gateway, VPN or IPv6 route." })
    } else if r.contains("host key") || r.contains("known_hosts") {
        Some(ErrorHint { zh: "主机密钥异常：设备可能重装过系统，确认安全后再信任。", en: "Host key issue: verify the device before trusting the new key." })
    } else {
        None
    }
}

pub fn append_hint(reason: &str, lang_en: bool) -> String {
    match hint_for(reason) {
        Some(h) => {
            if lang_en { format!("{reason}\n{}", h.en) } else { format!("{reason}\n{}", h.zh) }
        }
        None => reason.to_string(),
    }
}
