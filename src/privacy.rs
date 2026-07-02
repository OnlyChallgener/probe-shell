//! Privacy helpers for screenshots / demos.
//!
//! Keep this module deliberately small and side-effect free: the UI can render
//! either raw or masked strings without touching saved sessions.

/// Mask a username while keeping enough shape to recognise the account.
pub fn mask_user(user: &str) -> String {
    let trimmed = user.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut chars = trimmed.chars();
    let first = chars.next().unwrap_or('*');
    if trimmed.chars().count() <= 2 {
        return format!("{}*", first);
    }
    format!("{}***", first)
}

/// Mask IPv4 / IPv6 / hostnames for screen sharing.
pub fn mask_host(host: &str) -> String {
    let host = host.trim();
    if host.is_empty() {
        return String::new();
    }

    // IPv4: keep the first two octets, hide the rest.
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok()) {
        return format!("{}.{}.*.*", parts[0], parts[1]);
    }

    // IPv6: keep a short prefix only. Do not try to normalise; display safety is
    // more important than exact RFC formatting here.
    if host.contains(':') {
        let prefix = host.split(':').take(2).collect::<Vec<_>>().join(":");
        return if prefix.is_empty() { "****::".to_string() } else { format!("{}::****", prefix) };
    }

    // Hostname / domain: keep the first label's first and last character.
    let first_label = host.split('.').next().unwrap_or(host);
    let count = first_label.chars().count();
    if count <= 2 {
        return "**".to_string();
    }
    let first = first_label.chars().next().unwrap_or('*');
    let last = first_label.chars().last().unwrap_or('*');
    if host.contains('.') {
        format!("{}***{}.***", first, last)
    } else {
        format!("{}***{}", first, last)
    }
}

pub fn display_user(user: &str, privacy: bool) -> String {
    if privacy { mask_user(user) } else { user.to_string() }
}

pub fn display_host(host: &str, privacy: bool) -> String {
    if privacy { mask_host(host) } else { host.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_ipv4() {
        assert_eq!(mask_host("192.168.5.1"), "192.168.*.*");
    }

    #[test]
    fn masks_user() {
        assert_eq!(mask_user("root"), "r***");
    }
}
