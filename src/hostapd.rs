//! `hostapd_cli status` polling (DFS/CAC state).
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct HostapdCfg {
    pub cli: String,
    pub ctrl: Option<String>,
}

const KEYS: [&str; 5] = ["state", "freq", "channel", "cac_time_seconds", "cac_time_left_seconds"];

/// Keep only the DFS-relevant `key=value` lines of `hostapd_cli status`.
pub fn parse_status(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .filter(|(k, _)| KEYS.contains(k))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect()
}

/// `[-p DIR] -i IFACE [-l LINK]`: the common prefix of every hostapd_cli call
/// (control actions and status polls build their argv from it).
pub fn prefix(ctrl: Option<&str>, iface: &str, link: Option<u32>) -> Vec<String> {
    let mut a: Vec<String> = vec![];
    if let Some(c) = ctrl {
        a.extend(["-p".to_string(), c.to_string()]);
    }
    a.extend(["-i".to_string(), iface.to_string()]);
    if let Some(l) = link {
        a.extend(["-l".to_string(), l.to_string()]);
    }
    a
}

/// `hostapd_cli status`, for one link of an MLD interface when `link` is set.
pub fn status_link(cfg: &HostapdCfg, iface: &str, link: Option<u32>) -> Result<BTreeMap<String, String>, String> {
    let mut args = prefix(cfg.ctrl.as_deref(), iface, link);
    args.push("status".to_string());
    crate::util::run(&cfg.cli, &args, Duration::from_secs(3)).map(|t| parse_status(&t))
}

pub fn status(cfg: &HostapdCfg, iface: &str) -> Result<BTreeMap<String, String>, String> {
    status_link(cfg, iface, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_forms() {
        assert_eq!(prefix(None, "wlan0", None).join(" "), "-i wlan0");
        assert_eq!(prefix(Some("/var/run/hostapd"), "wlan0", Some(1)).join(" "), "-p /var/run/hostapd -i wlan0 -l 1");
    }

    #[test]
    fn status_parsing() {
        let m = parse_status("state=DFS\nphy=phy0\nfreq=5500\nchannel=100\ncac_time_seconds=60\ncac_time_left_seconds=41\nnum_sta_non_erp=0\n");
        assert_eq!(m.len(), 5);
        assert_eq!(m["state"], "DFS");
        assert_eq!(m["cac_time_left_seconds"], "41");
        assert!(!m.contains_key("phy"));
    }
}
